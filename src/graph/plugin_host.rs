use basedrop::Shared;
use clack_host::events::event_types::{ParamModEvent, ParamValueEvent};
use clack_host::events::io::EventBuffer;
use clack_host::events::spaces::CoreEventSpace;
use clack_host::events::{Event, EventFlags, EventHeader};
use clack_host::utils::Cookie;
use fnv::FnvHashMap;
use meadowlark_core_types::SampleRate;
use smallvec::SmallVec;
use std::error::Error;
use std::fmt::Debug;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use super::shared_pool::PluginInstanceID;
use crate::engine::plugin_scanner::PluginFormat;
use crate::graph::shared_pool::{SharedBuffer, SharedPluginHostAudioThread};
use crate::plugin::events::ProcEvent;
use crate::plugin::ext::audio_ports::PluginAudioPortsExt;
use crate::plugin::ext::note_ports::PluginNotePortsExt;
use crate::plugin::ext::params::{ParamInfo, ParamInfoFlags};
use crate::plugin::host_request::RequestFlags;
use crate::plugin::process_info::ProcBuffers;
use crate::plugin::{PluginAudioThread, PluginMainThread, PluginPreset, PluginSaveState};
use crate::transport::TempoMap;
use crate::utils::reducing_queue::{
    ReducFnvConsumer, ReducFnvProducer, ReducFnvValue, ReducingFnvQueue,
};
use crate::{HostRequest, ParamID, ProcInfo, ProcessStatus, ScannedPluginKey};

#[derive(Clone, Copy)]
struct MainToAudioParamValue {
    value: f64,
    _cookie: Cookie,
}

impl ReducFnvValue for MainToAudioParamValue {}

#[derive(Debug, Clone, Copy)]
pub struct ParamGestureInfo {
    pub is_begin: bool,
}

#[derive(Clone, Copy)]
struct AudioToMainParamValue {
    value: Option<f64>,
    gesture: Option<ParamGestureInfo>,
}

impl ReducFnvValue for AudioToMainParamValue {
    fn update(&mut self, new_value: &Self) {
        if new_value.value.is_some() {
            self.value = new_value.value;
        }

        if new_value.gesture.is_some() {
            self.gesture = new_value.gesture;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParamModifiedInfo {
    pub param_id: ParamID,
    pub new_value: Option<f64>,
    pub is_gesturing: bool,
}

#[derive(Debug)]
pub struct PluginHandle {
    pub params: PluginParamsExt,
    pub internal: Option<Box<dyn std::any::Any + Send + 'static>>,
    pub(crate) audio_ports: PluginAudioPortsExt,
    pub(crate) note_ports: PluginNotePortsExt,
    pub(crate) has_automation_out_port: bool,
}

impl PluginHandle {
    pub fn audio_ports(&self) -> &PluginAudioPortsExt {
        &self.audio_ports
    }

    pub fn note_ports(&self) -> &PluginNotePortsExt {
        &self.note_ports
    }

    /// This will only return `true` for internal plugins which send parameter
    /// automation events to other plugins.
    ///
    /// Note, plugins always have an "automation in port".
    pub fn has_automation_out_port(&self) -> bool {
        self.has_automation_out_port
    }
}

pub struct PluginParamsExt {
    /// (parameter info, initial value)
    pub params: FnvHashMap<ParamID, ParamInfo>,

    ui_to_audio_param_value_tx: Option<ReducFnvProducer<ParamID, MainToAudioParamValue>>,
    ui_to_audio_param_mod_tx: Option<ReducFnvProducer<ParamID, MainToAudioParamValue>>,
}

impl Debug for PluginParamsExt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("PluginParamsExt");
        f.field("params", &self.params);
        f.finish()
    }
}

impl PluginParamsExt {
    pub fn set_value(&mut self, param_id: ParamID, value: f64) {
        if let Some(ui_to_audio_param_value_tx) = &mut self.ui_to_audio_param_value_tx {
            if let Some(param_info) = self.params.get(&param_id) {
                if param_info.flags.contains(ParamInfoFlags::IS_READONLY) {
                    log::warn!("Ignored request to set parameter value: parameter with id {:?} is read only", &param_id);
                } else {
                    ui_to_audio_param_value_tx
                        .set(param_id, MainToAudioParamValue { value, _cookie: param_info.cookie });
                    ui_to_audio_param_value_tx.producer_done();
                }
            } else {
                log::warn!(
                    "Ignored request to set parameter value: plugin has no parameter with id {:?}",
                    &param_id
                );
            }
        } else {
            log::warn!("Ignored request to set parameter value: plugin has no parameters");
        }
    }

    pub fn set_mod_amount(&mut self, param_id: ParamID, amount: f64) {
        if let Some(ui_to_audio_param_mod_tx) = &mut self.ui_to_audio_param_mod_tx {
            if let Some(param_info) = self.params.get(&param_id) {
                ui_to_audio_param_mod_tx.set(
                    param_id,
                    MainToAudioParamValue { value: amount, _cookie: param_info.cookie },
                );
                ui_to_audio_param_mod_tx.producer_done();
            } else {
                log::warn!(
                    "Ignored request to set parameter mod amount: plugin has no parameter with id {:?}",
                    &param_id
                );
            }
        } else {
            log::warn!("Ignored request to set parameter mod amount: plugin has no parameters");
        }
    }
}

struct ParamQueuesMainThread {
    audio_to_main_param_value_rx: ReducFnvConsumer<ParamID, AudioToMainParamValue>,
}

struct ParamQueuesAudioThread {
    audio_to_main_param_value_tx: ReducFnvProducer<ParamID, AudioToMainParamValue>,

    ui_to_audio_param_value_rx: ReducFnvConsumer<ParamID, MainToAudioParamValue>,
    ui_to_audio_param_mod_rx: ReducFnvConsumer<ParamID, MainToAudioParamValue>,
}

pub(crate) struct PluginInstanceHost {
    pub id: PluginInstanceID,

    pub audio_ports: Option<PluginAudioPortsExt>,
    pub note_ports: Option<PluginNotePortsExt>,

    pub num_audio_in_channels: usize,
    pub num_audio_out_channels: usize,

    main_thread: Option<Box<dyn PluginMainThread>>,
    pub audio_thread: Option<SharedPluginHostAudioThread>,

    state: Arc<SharedPluginState>,

    save_state: PluginSaveState,
    plugin_version: Option<Shared<String>>,

    param_queues: Option<ParamQueuesMainThread>,
    gesturing_params: FnvHashMap<ParamID, bool>,

    host_request: HostRequest,
    remove_requested: bool,
}

impl PluginInstanceHost {
    pub fn new(
        id: PluginInstanceID,
        save_state: PluginSaveState,
        mut main_thread: Option<Box<dyn PluginMainThread>>,
        host_request: HostRequest,
        plugin_version: Option<Shared<String>>,
    ) -> Self {
        let state = Arc::new(SharedPluginState::new());

        if let Some(preset) = &save_state.preset {
            if let Some(main_thread) = &mut main_thread {
                match main_thread.load_state(preset) {
                    Ok(()) => {
                        log::trace!("Plugin {:?} successfully loaded preset", &id);
                    }
                    Err(e) => {
                        log::error!("Plugin {:?} failed to load preset: {}", &id, e);
                    }
                }
            }
        }

        if main_thread.is_none() {
            state.set(PluginState::InactiveWithError);
        }

        let (num_audio_in_channels, num_audio_out_channels) =
            if let Some(backup_audio_ports) = &save_state.backup_audio_ports {
                (backup_audio_ports.total_in_channels(), backup_audio_ports.total_out_channels())
            } else {
                (0, 0)
            };

        Self {
            id,
            main_thread,
            audio_thread: None,
            audio_ports: None,
            note_ports: None,
            num_audio_in_channels,
            num_audio_out_channels,
            state: Arc::new(SharedPluginState::new()),
            save_state,
            plugin_version,
            param_queues: None,
            gesturing_params: FnvHashMap::default(),
            host_request,
            remove_requested: false,
        }
    }

    pub fn new_graph_in(
        id: PluginInstanceID,
        host_request: HostRequest,
        num_audio_out_channels: usize,
    ) -> Self {
        let state = Arc::new(SharedPluginState::new());

        state.set(PluginState::Inactive);

        // We don't actually use this save state. This is just here to be
        // consistent with the rest of the plugins.
        let save_state = PluginSaveState {
            key: ScannedPluginKey {
                rdn: "app.meadowlark.dropseed-graph-in".into(),
                format: PluginFormat::Internal,
            },
            activation_requested: false,
            backup_audio_ports: None,
            backup_note_ports: None,
            preset: None,
        };

        Self {
            id,
            main_thread: None,
            audio_thread: None,
            audio_ports: None,
            note_ports: None,
            num_audio_in_channels: 0,
            num_audio_out_channels,
            state: Arc::new(SharedPluginState::new()),
            save_state,
            plugin_version: None,
            param_queues: None,
            gesturing_params: FnvHashMap::default(),
            host_request,
            remove_requested: false,
        }
    }

    pub fn new_graph_out(
        id: PluginInstanceID,
        host_request: HostRequest,
        num_audio_in_channels: usize,
    ) -> Self {
        let state = Arc::new(SharedPluginState::new());

        state.set(PluginState::Inactive);

        // We don't actually use this save state. This is just here to be
        // consistent with the rest of the plugins.
        let save_state = PluginSaveState {
            key: ScannedPluginKey {
                rdn: "app.meadowlark.dropseed-graph-out".into(),
                format: PluginFormat::Internal,
            },
            activation_requested: false,
            backup_audio_ports: None,
            backup_note_ports: None,
            preset: None,
        };

        Self {
            id,
            main_thread: None,
            audio_thread: None,
            audio_ports: None,
            note_ports: None,
            num_audio_in_channels,
            num_audio_out_channels: 0,
            state: Arc::new(SharedPluginState::new()),
            save_state,
            plugin_version: None,
            param_queues: None,
            gesturing_params: FnvHashMap::default(),
            host_request,
            remove_requested: false,
        }
    }

    pub fn collect_save_state(&mut self) -> PluginSaveState {
        if self.host_request.state_marked_dirty_and_reset_dirty() {
            if let Some(main_thread) = &mut self.main_thread {
                let preset = match main_thread.collect_save_state() {
                    Ok(preset) => preset.map(|bytes| PluginPreset {
                        version: self.plugin_version.as_ref().map(|v| String::clone(&*v)),
                        bytes,
                    }),
                    Err(e) => {
                        log::error!(
                            "Failed to collect save state from plugin {:?}: {}",
                            &self.id,
                            e
                        );

                        None
                    }
                };

                self.save_state.preset = preset;
            }
        }

        self.save_state.clone()
    }

    pub fn can_activate(&self) -> Result<(), ActivatePluginError> {
        if self.main_thread.is_none() {
            return Err(ActivatePluginError::NotLoaded);
        }
        if self.state.get().is_active() {
            return Err(ActivatePluginError::AlreadyActive);
        }
        if self.host_request.load_requested().contains(RequestFlags::RESTART) {
            return Err(ActivatePluginError::RestartScheduled);
        }
        Ok(())
    }

    pub fn activate(
        &mut self,
        sample_rate: SampleRate,
        min_frames: u32,
        max_frames: u32,
        coll_handle: &basedrop::Handle,
    ) -> Result<(PluginHandle, FnvHashMap<ParamID, f64>), ActivatePluginError> {
        self.can_activate()?;

        let plugin_main_thread = self.main_thread.as_mut().unwrap();

        self.save_state.activation_requested = true;

        let audio_ports = match plugin_main_thread.audio_ports_ext() {
            Ok(audio_ports) => {
                self.num_audio_in_channels = audio_ports.total_in_channels();
                self.num_audio_out_channels = audio_ports.total_out_channels();

                self.save_state.backup_audio_ports = Some(audio_ports.clone());

                audio_ports
            }
            Err(e) => {
                self.state.set(PluginState::InactiveWithError);
                self.audio_ports = None;

                return Err(ActivatePluginError::PluginFailedToGetAudioPortsExt(e));
            }
        };

        let note_ports = match plugin_main_thread.note_ports_ext() {
            Ok(note_ports) => {
                self.save_state.backup_note_ports = Some(note_ports.clone());

                note_ports
            }
            Err(e) => {
                self.state.set(PluginState::InactiveWithError);
                self.note_ports = None;

                return Err(ActivatePluginError::PluginFailedToGetNotePortsExt(e));
            }
        };

        self.audio_ports = Some(audio_ports.clone());
        self.note_ports = Some(note_ports.clone());

        let num_params = plugin_main_thread.num_params() as usize;
        let mut params: FnvHashMap<ParamID, ParamInfo> = FnvHashMap::default();
        let mut param_values: FnvHashMap<ParamID, f64> = FnvHashMap::default();

        for i in 0..num_params {
            match plugin_main_thread.param_info(i) {
                Ok(info) => match plugin_main_thread.param_value(info.stable_id) {
                    Ok(value) => {
                        let id = info.stable_id;

                        let _ = params.insert(id, info);
                        let _ = param_values.insert(id, value);
                    }
                    Err(_) => {
                        self.state.set(PluginState::InactiveWithError);

                        return Err(ActivatePluginError::PluginFailedToGetParamValue(
                            info.stable_id,
                        ));
                    }
                },
                Err(_) => {
                    self.state.set(PluginState::InactiveWithError);

                    return Err(ActivatePluginError::PluginFailedToGetParamInfo(i));
                }
            }
        }

        match plugin_main_thread.activate(sample_rate, min_frames, max_frames, coll_handle) {
            Ok(info) => {
                self.host_request.reset_deactivate();
                self.host_request.request_process();

                self.state.set(PluginState::ActiveAndSleeping);

                let mut params_ext = PluginParamsExt {
                    params,
                    ui_to_audio_param_value_tx: None,
                    ui_to_audio_param_mod_tx: None,
                };

                let (param_queues_main_thread, param_queues_audio_thread) = if num_params > 0 {
                    let (ui_to_audio_param_value_tx, ui_to_audio_param_value_rx) =
                        ReducingFnvQueue::new(num_params, coll_handle);
                    let (ui_to_audio_param_mod_tx, ui_to_audio_param_mod_rx) =
                        ReducingFnvQueue::new(num_params, coll_handle);
                    let (audio_to_main_param_value_tx, audio_to_main_param_value_rx) =
                        ReducingFnvQueue::new(num_params, coll_handle);

                    params_ext.ui_to_audio_param_value_tx = Some(ui_to_audio_param_value_tx);
                    params_ext.ui_to_audio_param_mod_tx = Some(ui_to_audio_param_mod_tx);

                    (
                        Some(ParamQueuesMainThread { audio_to_main_param_value_rx }),
                        Some(ParamQueuesAudioThread {
                            audio_to_main_param_value_tx,
                            ui_to_audio_param_value_rx,
                            ui_to_audio_param_mod_rx,
                        }),
                    )
                } else {
                    (None, None)
                };

                self.param_queues = param_queues_main_thread;

                let mut is_adjusting_parameter = FnvHashMap::default();
                is_adjusting_parameter.reserve(num_params * 2);

                let has_automation_out_port = plugin_main_thread.has_automation_out_port();

                self.audio_thread = Some(SharedPluginHostAudioThread::new(
                    PluginInstanceHostAudioThread {
                        id: self.id.clone(),
                        plugin: info.audio_thread,
                        state: Arc::clone(&self.state),
                        param_queues: param_queues_audio_thread,
                        in_events: EventBuffer::with_capacity(num_params * 3),
                        out_events: EventBuffer::with_capacity(num_params * 3),
                        is_adjusting_parameter,
                        host_request: self.host_request.clone(),
                    },
                    coll_handle,
                ));

                Ok((
                    PluginHandle {
                        audio_ports,
                        internal: info.internal_handle,
                        note_ports,
                        params: params_ext,
                        has_automation_out_port,
                    },
                    param_values,
                ))
            }
            Err(e) => {
                self.state.set(PluginState::InactiveWithError);

                Err(ActivatePluginError::PluginSpecific(e))
            }
        }
    }

    pub fn schedule_deactivate(&mut self) {
        self.save_state.activation_requested = false;

        if !self.state.get().is_active() {
            return;
        }

        // Allow the plugin's audio thread to be dropped when the new
        // schedule is sent.
        self.audio_thread = None;

        // Wait for the audio thread part to go to sleep before
        // deactivating.
        self.host_request.request_deactivate();
    }

    pub fn schedule_remove(&mut self) {
        self.remove_requested = true;

        self.schedule_deactivate();
    }

    pub fn audio_ports_ext(&self) -> Option<&PluginAudioPortsExt> {
        if self.audio_ports.is_some() {
            self.audio_ports.as_ref()
        } else {
            self.save_state.backup_audio_ports.as_ref()
        }
    }

    pub fn note_ports_ext(&self) -> Option<&PluginNotePortsExt> {
        if self.note_ports.is_some() {
            self.note_ports.as_ref()
        } else {
            self.save_state.backup_note_ports.as_ref()
        }
    }

    /// Whether or not this plugin has an automation out port (seperate from audio
    /// and note out ports).
    ///
    /// Only return `true` for internal plugins which output parameter automation
    /// events for other plugins.
    pub fn has_automation_out_port(&self) -> bool {
        if let Some(main_thread) = &self.main_thread {
            main_thread.has_automation_out_port()
        } else {
            false
        }
    }

    pub fn on_idle(
        &mut self,
        sample_rate: SampleRate,
        min_frames: u32,
        max_frames: u32,
        coll_handle: &basedrop::Handle,
    ) -> (OnIdleResult, SmallVec<[ParamModifiedInfo; 4]>) {
        let mut modified_params: SmallVec<[ParamModifiedInfo; 4]> = SmallVec::new();

        if self.main_thread.is_none() {
            if self.remove_requested {
                return (OnIdleResult::PluginReadyToRemove, modified_params);
            } else {
                return (OnIdleResult::Ok, modified_params);
            }
        }

        let plugin_main_thread = self.main_thread.as_mut().unwrap();

        let request_flags = self.host_request.load_requests_and_reset_callback();
        let state = self.state.get();

        /*
        if self.remove_requested && !state.is_active() {
            return (OnIdleResult::PluginReadyToRemove, modified_params);
        }
        */

        if request_flags.contains(RequestFlags::CALLBACK) {
            plugin_main_thread.on_main_thread();
        }

        if request_flags.contains(RequestFlags::DEACTIVATE) {
            if state == PluginState::DroppedAndReadyToDeactivate {
                // Safe to deactive now.

                plugin_main_thread.deactivate();

                self.state.set(PluginState::Inactive);
                self.host_request.reset_deactivate();

                self.param_queues = None;

                if !self.remove_requested {
                    let mut res = OnIdleResult::PluginDeactivated;

                    if self.host_request.reset_restart() {
                        match self.activate(sample_rate, min_frames, max_frames, coll_handle) {
                            Ok((ui_handle, param_values)) => {
                                res = OnIdleResult::PluginActivated(ui_handle, param_values)
                            }
                            Err(e) => res = OnIdleResult::PluginFailedToActivate(e),
                        }
                    }

                    return (res, modified_params);
                } else {
                    return (OnIdleResult::PluginReadyToRemove, modified_params);
                }
            }
        } else if request_flags.contains(RequestFlags::RESTART) && !self.remove_requested {
            // Wait for the audio thread part to go to sleep before
            // deactivating.
            self.host_request.request_deactivate();
        }

        if let Some(params_queue) = &mut self.param_queues {
            params_queue.audio_to_main_param_value_rx.consume(|param_id, new_value| {
                let is_gesturing = if let Some(gesture) = new_value.gesture {
                    let _ = self.gesturing_params.insert(*param_id, gesture.is_begin);
                    gesture.is_begin
                } else {
                    *self.gesturing_params.get(param_id).unwrap_or(&false)
                };

                modified_params.push(ParamModifiedInfo {
                    param_id: *param_id,
                    new_value: new_value.value,
                    is_gesturing,
                })
            });
        }

        (OnIdleResult::Ok, modified_params)
    }

    pub fn update_tempo_map(&mut self, new_tempo_map: &Shared<TempoMap>) {
        if let Some(main_thread) = &mut self.main_thread {
            main_thread.update_tempo_map(new_tempo_map);
        }
    }
}

pub(crate) enum OnIdleResult {
    Ok,
    PluginDeactivated,
    PluginActivated(PluginHandle, FnvHashMap<ParamID, f64>),
    PluginReadyToRemove,
    PluginFailedToActivate(ActivatePluginError),
}

pub(crate) struct PluginInstanceHostAudioThread {
    pub id: PluginInstanceID,

    plugin: Box<dyn PluginAudioThread>,

    state: Arc<SharedPluginState>,

    param_queues: Option<ParamQueuesAudioThread>,
    in_events: EventBuffer,
    out_events: EventBuffer,

    is_adjusting_parameter: FnvHashMap<ParamID, bool>,

    host_request: HostRequest,
}

impl PluginInstanceHostAudioThread {
    pub fn process(
        &mut self,
        proc_info: &ProcInfo,
        buffers: &mut ProcBuffers,
        event_in_buffers: &Option<SmallVec<[SharedBuffer<ProcEvent>; 2]>>,
        event_out_buffer: &Option<SharedBuffer<ProcEvent>>,
        note_in_buffers: &[Option<SmallVec<[SharedBuffer<ProcEvent>; 2]>>],
        note_out_buffers: &[Option<SharedBuffer<ProcEvent>>],
    ) {
        let clear_outputs = |proc_info: &ProcInfo, buffers: &mut ProcBuffers| {
            buffers.clear_all_outputs(proc_info.frames);
            for b in buffers.audio_out.iter_mut() {
                b.sync_constant_mask_to_buffers();
            }
        };

        // Always clear event and note output buffers.
        if let Some(out_buf) = event_out_buffer {
            let mut b = out_buf.borrow_mut();
            b.clear();
        }

        for out_buf in note_out_buffers.iter().flatten() {
            let mut b = out_buf.borrow_mut();
            b.clear();
        }

        let mut state = self.state.get();

        if !state.is_active() {
            // Can't process a plugin that is not active.
            clear_outputs(proc_info, buffers);
            self.in_events.clear();
            return;
        }

        let request_flags = self.host_request.load_requested();

        // Do we want to deactivate the plugin?
        if request_flags.contains(RequestFlags::DEACTIVATE) {
            if state.is_processing() {
                self.plugin.stop_processing();
            }

            self.state.set(PluginState::WaitingToDrop);
            clear_outputs(proc_info, buffers);
            self.in_events.clear();
            return;
        }

        if state == PluginState::ActiveWithError {
            // We can't process a plugin which failed to start processing.
            clear_outputs(proc_info, buffers);
            self.in_events.clear();
            return;
        }

        self.out_events.clear();

        let mut has_param_in_event = false;
        let mut has_note_in_event = false;

        if let Some(params_queue) = &mut self.param_queues {
            params_queue.ui_to_audio_param_value_rx.consume(|param_id, value| {
                has_param_in_event = true;

                let event = ParamValueEvent::new(
                    // TODO: Finer values for `time` instead of just setting it to the first frame?
                    EventHeader::new_core(0, EventFlags::empty()),
                    Cookie::empty(),
                    // TODO: Note ID
                    -1,                // note_id
                    param_id.as_u32(), // param_id
                    // TODO: Port index
                    -1, // port_index
                    // TODO: Channel
                    -1, // channel
                    // TODO: Key
                    -1,          // key
                    value.value, // value
                );

                self.in_events.push(event.as_unknown())
            });

            params_queue.ui_to_audio_param_mod_rx.consume(|param_id, value| {
                has_param_in_event = true;

                let event = ParamModEvent::new(
                    // TODO: Finer values for `time` instead of just setting it to the first frame?
                    EventHeader::new_core(0, EventFlags::empty()),
                    Cookie::empty(),
                    // TODO: Note ID
                    -1,                // note_id
                    param_id.as_u32(), // param_id
                    // TODO: Port index
                    -1, // port_index
                    // TODO: Channel
                    -1, // channel
                    // TODO: Key
                    -1,          // key
                    value.value, // value
                );

                self.in_events.push(event.as_unknown())
            });
        }

        for (port_i, in_buffers) in note_in_buffers.iter().enumerate() {
            if let Some(in_buffers) = in_buffers {
                for in_buf in in_buffers.iter() {
                    let mut b = in_buf.borrow_mut();

                    for mut event in b.drain(..) {
                        let mut do_add = true;
                        match &mut event {
                            ProcEvent::NoteOn(e) => e.0.set_port_index(port_i as i16),
                            ProcEvent::NoteOff(e) => e.0.set_port_index(port_i as i16),
                            ProcEvent::NoteChoke(e) => e.0.set_port_index(port_i as i16),
                            ProcEvent::NoteEnd(e) => e.0.set_port_index(port_i as i16),
                            ProcEvent::NoteExpression(e) => e.set_port_index(port_i as i16),
                            ProcEvent::Midi(e) => e.set_port_index(port_i as u16),
                            ProcEvent::Midi2(e) => e.set_port_index(port_i as u16),
                            _ => do_add = false,
                        }

                        if do_add {
                            has_note_in_event = true;
                            self.in_events.push(event.as_unknown());
                        }
                    }
                }
            }
        }

        if let Some(event_in_buffers) = event_in_buffers {
            for in_buf in event_in_buffers.iter() {
                let mut b = in_buf.borrow_mut();

                for event in b.drain(..) {
                    let do_add = match &event {
                        ProcEvent::ParamValue(_, Some(target_plugin))
                        | ProcEvent::ParamMod(_, Some(target_plugin))
                            if *target_plugin == self.id.node_ref =>
                        {
                            has_param_in_event = true;
                            true
                        }
                        ProcEvent::Transport(_) => true,
                        _ => false,
                    };

                    if do_add {
                        self.in_events.push(event.as_unknown());
                    }
                }
            }
        }

        if let Some(transport_in_event) = proc_info.transport.event() {
            self.in_events.push(transport_in_event.as_unknown());
        }

        if state == PluginState::ActiveAndWaitingForQuiet && !has_note_in_event {
            // Sync constant masks for more efficient silence checking.
            for buf in buffers.audio_in.iter_mut() {
                buf.sync_constant_mask_from_buffers();
            }

            if buffers.audio_inputs_silent(proc_info.frames) {
                self.plugin.stop_processing();

                self.state.set(PluginState::ActiveAndSleeping);
                clear_outputs(proc_info, buffers);

                if has_param_in_event {
                    self.plugin.param_flush(&self.in_events, &mut self.out_events);
                }

                self.in_events.clear();
                return;
            }
        }

        if state.is_sleeping() {
            if !request_flags.contains(RequestFlags::PROCESS) && !has_note_in_event {
                // The plugin is sleeping, there is no request to wake it up, and there
                // are no events to process.
                clear_outputs(proc_info, buffers);

                if has_param_in_event {
                    self.plugin.param_flush(&self.in_events, &mut self.out_events);
                }

                self.in_events.clear();
                return;
            }

            self.host_request.reset_process();

            if self.plugin.start_processing().is_err() {
                // The plugin failed to start processing.
                self.state.set(PluginState::ActiveWithError);
                clear_outputs(proc_info, buffers);

                if has_param_in_event {
                    self.plugin.param_flush(&self.in_events, &mut self.out_events);
                }

                self.in_events.clear();
                return;
            }

            self.state.set(PluginState::ActiveAndProcessing);
            state = PluginState::ActiveAndProcessing;
        }

        // Sync constant masks for the plugin.
        if state != PluginState::ActiveAndWaitingForQuiet {
            for buf in buffers.audio_in.iter_mut() {
                buf.sync_constant_mask_from_buffers();
            }
        }
        for buf in buffers.audio_out.iter_mut() {
            buf.set_constant_mask(0);
        }

        let status = self.plugin.process(proc_info, buffers, &self.in_events, &mut self.out_events);

        self.in_events.clear();

        if let Some(params_queue) = &mut self.param_queues {
            // TODO: Change this to not take a closure so we don't
            // have to duplicate this code.
            params_queue.audio_to_main_param_value_tx.produce(|mut queue| {
                for out_event in &self.out_events {
                        let mut push_to_event_out = false;
                        let mut push_to_note_out_i = None;
                        match out_event.as_core_event() {
                            Some(CoreEventSpace::ParamGestureBegin(event)) => {
                                // TODO: Use event.time for more accurate recording of automation.

                                let param_id = ParamID::new(event.param_id());
                                let is_adjusting =
                                    self.is_adjusting_parameter.entry(param_id).or_insert(false);

                                if *is_adjusting {
                                    log::warn!(
                                        "The plugin sent BEGIN_ADJUST twice. The event was ignored."
                                    );
                                    continue;
                                }

                                *is_adjusting = true;

                                let value = AudioToMainParamValue {
                                    value: None,
                                    gesture: Some(ParamGestureInfo { is_begin: true }),
                                };

                                queue.set_or_update(param_id, value);
                            }
                            Some(CoreEventSpace::ParamGestureEnd(event)) => {
                                let param_id = ParamID::new(event.param_id());
                                let is_adjusting =
                                    self.is_adjusting_parameter.entry(param_id).or_insert(false);

                                if !*is_adjusting {
                                    log::warn!(
                                        "The plugin sent END_ADJUST without a preceding BEGIN_ADJUST. The event was ignored."
                                    );
                                    continue;
                                }

                                *is_adjusting = false;

                                let value = AudioToMainParamValue {
                                    value: None,
                                    gesture: Some(ParamGestureInfo { is_begin: false }),
                                };

                                queue.set_or_update(param_id, value);
                            }
                            Some(CoreEventSpace::ParamValue(event)) => {
                                // TODO: Use event.time for more accurate recording of automation.
                                let param_id = ParamID::new(event.param_id());

                                queue.set_or_update(param_id, AudioToMainParamValue {
                                    value: Some(event.value()),
                                    gesture: None,
                                });
                            }
                            Some(CoreEventSpace::ParamMod(_)) | Some(CoreEventSpace::Transport(_)) => {
                                // This will only be `Some` if the plugin returned `true` in `has_event_out_port()`.
                                if event_out_buffer.is_some() {
                                    push_to_event_out = true;
                                }
                            }
                            Some(CoreEventSpace::NoteOn(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                                    push_to_note_out_i = Some(event.0.port_index() as usize);
                                }
                            }
                            Some(CoreEventSpace::NoteOff(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                                    push_to_note_out_i = Some(event.0.port_index() as usize);
                                }
                            }
                            Some(CoreEventSpace::NoteEnd(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                                    push_to_note_out_i = Some(event.0.port_index() as usize);
                                }
                            }
                            Some(CoreEventSpace::NoteChoke(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                                    push_to_note_out_i = Some(event.0.port_index() as usize);
                                }
                            }
                            Some(CoreEventSpace::NoteExpression(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                                    push_to_note_out_i = Some(event.port_index() as usize);
                                }
                            }
                            Some(CoreEventSpace::Midi(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                                    push_to_note_out_i = Some(event.port_index() as usize);
                                }
                            }
                            /*
                            Some(CoreEventSpace::MidiSysex(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                                    push_to_note_out_i = Some(event.port_index() as usize);
                                }
                            }
                            */
                            Some(CoreEventSpace::Midi2(event)) => {
                                if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                                    push_to_note_out_i = Some(event.port_index() as usize);
                                }
                            }
                            _ => {}
                        }

                        if push_to_event_out {
                            let out_buf = event_out_buffer.as_ref().unwrap();

                            let mut b = out_buf.borrow_mut();

                            b.push(ProcEvent::from_unknown(out_event).unwrap());
                        } else if let Some(buf_i) = push_to_note_out_i {
                            let out_buf = note_out_buffers[buf_i].as_ref().unwrap();

                            let mut b = out_buf.borrow_mut();

                            b.push(ProcEvent::from_unknown(out_event).unwrap());
                        }
                }
            });
        } else {
            for out_event in &self.out_events {
                let mut push_to_event_out = false;
                let mut push_to_note_out_i = None;
                match out_event.as_core_event() {
                    Some(CoreEventSpace::ParamMod(_)) | Some(CoreEventSpace::Transport(_)) => {
                        // This will only be `Some` if the plugin returned `true` in `has_event_out_port()`.
                        if event_out_buffer.is_some() {
                            push_to_event_out = true;
                        }
                    }
                    Some(CoreEventSpace::NoteOn(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                            push_to_note_out_i = Some(event.0.port_index() as usize);
                        }
                    }
                    Some(CoreEventSpace::NoteOff(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                            push_to_note_out_i = Some(event.0.port_index() as usize);
                        }
                    }
                    Some(CoreEventSpace::NoteEnd(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                            push_to_note_out_i = Some(event.0.port_index() as usize);
                        }
                    }
                    Some(CoreEventSpace::NoteChoke(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.0.port_index() as usize) {
                            push_to_note_out_i = Some(event.0.port_index() as usize);
                        }
                    }
                    Some(CoreEventSpace::NoteExpression(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                            push_to_note_out_i = Some(event.port_index() as usize);
                        }
                    }
                    Some(CoreEventSpace::Midi(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                            push_to_note_out_i = Some(event.port_index() as usize);
                        }
                    }
                    /*
                    Some(CoreEventSpace::MidiSysex(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                            push_to_note_out_i = Some(event.port_index() as usize);
                        }
                    }
                    */
                    Some(CoreEventSpace::Midi2(event)) => {
                        if let Some(Some(_)) = note_out_buffers.get(event.port_index() as usize) {
                            push_to_note_out_i = Some(event.port_index() as usize);
                        }
                    }
                    _ => {}
                }

                if push_to_event_out {
                    let out_buf = event_out_buffer.as_ref().unwrap();

                    let mut b = out_buf.borrow_mut();

                    b.push(ProcEvent::from_unknown(out_event).unwrap());
                } else if let Some(buf_i) = push_to_note_out_i {
                    let out_buf = note_out_buffers[buf_i].as_ref().unwrap();

                    let mut b = out_buf.borrow_mut();

                    b.push(ProcEvent::from_unknown(out_event).unwrap());
                }
            }
        }

        self.out_events.clear();

        match status {
            ProcessStatus::Continue => {
                if state != PluginState::ActiveAndProcessing {
                    self.state.set(PluginState::ActiveAndProcessing);
                }
            }
            ProcessStatus::ContinueIfNotQuiet => {
                if state != PluginState::ActiveAndWaitingForQuiet {
                    self.state.set(PluginState::ActiveAndWaitingForQuiet);
                }
            }
            ProcessStatus::Tail => {
                if state != PluginState::ActiveAndProcessing {
                    self.state.set(PluginState::ActiveAndProcessing);
                }

                if buffers.audio_outputs_silent(proc_info.frames) {
                    self.plugin.stop_processing();

                    self.state.set(PluginState::ActiveAndSleeping);
                }
            }
            ProcessStatus::Sleep => {
                self.plugin.stop_processing();

                self.state.set(PluginState::ActiveAndSleeping);
            }
            ProcessStatus::Error => {
                // Discard all output buffers.
                clear_outputs(proc_info, buffers);
                return;
            }
        }

        for buf in buffers.audio_out.iter_mut() {
            buf.sync_constant_mask_to_buffers();
        }
    }
}

impl Drop for PluginInstanceHostAudioThread {
    fn drop(&mut self) {
        if self.state.get().is_processing() {
            self.plugin.stop_processing();
        }

        self.state.set(PluginState::DroppedAndReadyToDeactivate);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum PluginState {
    /// The plugin is inactive, only the main thread uses it
    Inactive = 0,

    /// Activation failed
    InactiveWithError = 1,

    /// The plugin is active and sleeping, the audio engine can call start_processing()
    ActiveAndSleeping = 2,

    /// The plugin is processing
    ActiveAndProcessing = 3,

    /// The plugin is processing, but will be put to sleep the next time all input buffers
    /// are silent.
    ActiveAndWaitingForQuiet = 4,

    /// The plugin did process but is in error
    ActiveWithError = 5,

    /// The plugin audio thread is waiting to be dropped.
    WaitingToDrop = 6,

    /// The plugin is not used anymore by the audio engine and can be deactivated on the main
    /// thread
    DroppedAndReadyToDeactivate = 7,
}

impl PluginState {
    pub fn is_active(&self) -> bool {
        !matches!(
            self,
            PluginState::Inactive
                | PluginState::InactiveWithError
                | PluginState::WaitingToDrop
                | PluginState::DroppedAndReadyToDeactivate
        )
    }

    pub fn is_processing(&self) -> bool {
        matches!(self, PluginState::ActiveAndProcessing | PluginState::ActiveAndWaitingForQuiet)
    }

    pub fn is_sleeping(&self) -> bool {
        *self == PluginState::ActiveAndSleeping
    }
}

impl From<u32> for PluginState {
    fn from(s: u32) -> Self {
        match s {
            0 => PluginState::Inactive,
            1 => PluginState::InactiveWithError,
            2 => PluginState::ActiveAndSleeping,
            3 => PluginState::ActiveAndProcessing,
            4 => PluginState::ActiveAndWaitingForQuiet,
            5 => PluginState::ActiveWithError,
            6 => PluginState::WaitingToDrop,
            7 => PluginState::DroppedAndReadyToDeactivate,
            _ => PluginState::InactiveWithError,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SharedPluginState(AtomicU32);

impl SharedPluginState {
    pub fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    #[inline]
    pub fn get(&self) -> PluginState {
        // TODO: Are we able to use relaxed ordering here?
        let s = self.0.load(Ordering::SeqCst);

        s.into()
    }

    #[inline]
    pub fn set(&self, state: PluginState) {
        // TODO: Are we able to use relaxed ordering here?
        self.0.store(state as u32, Ordering::SeqCst);
    }
}

#[derive(Debug)]
pub enum ActivatePluginError {
    NotLoaded,
    AlreadyActive,
    RestartScheduled,
    PluginFailedToGetAudioPortsExt(String),
    PluginFailedToGetNotePortsExt(String),
    PluginFailedToGetParamInfo(usize),
    PluginFailedToGetParamValue(ParamID),
    PluginSpecific(String),
}

impl Error for ActivatePluginError {}

impl std::fmt::Display for ActivatePluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivatePluginError::NotLoaded => write!(f, "plugin failed to load from disk"),
            ActivatePluginError::AlreadyActive => write!(f, "plugin is already active"),
            ActivatePluginError::RestartScheduled => {
                write!(f, "a restart is scheduled for this plugin")
            }
            ActivatePluginError::PluginFailedToGetAudioPortsExt(e) => {
                write!(f, "plugin returned error while getting audio ports extension: {:?}", e)
            }
            ActivatePluginError::PluginFailedToGetNotePortsExt(e) => {
                write!(f, "plugin returned error while getting note ports extension: {:?}", e)
            }
            ActivatePluginError::PluginFailedToGetParamInfo(index) => {
                write!(f, "plugin returned error while getting parameter info at index: {}", index)
            }
            ActivatePluginError::PluginFailedToGetParamValue(param_id) => {
                write!(
                    f,
                    "plugin returned error while getting parameter value with ID: {:?}",
                    param_id
                )
            }
            ActivatePluginError::PluginSpecific(e) => {
                write!(f, "plugin returned error while activating: {:?}", e)
            }
        }
    }
}

impl From<String> for ActivatePluginError {
    fn from(e: String) -> Self {
        ActivatePluginError::PluginSpecific(e)
    }
}
