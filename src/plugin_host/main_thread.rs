use audio_graph::{AudioGraphHelper, EdgeID, PortID};
use basedrop::Shared;
use dropseed_plugin_api::ext::audio_ports::PluginAudioPortsExt;
use dropseed_plugin_api::ext::gui::{EmbeddedGuiInfo, GuiResizeHints, GuiSize};
use dropseed_plugin_api::ext::note_ports::PluginNotePortsExt;
use dropseed_plugin_api::ext::params::{ParamID, ParamInfo, ParamInfoFlags};
use dropseed_plugin_api::transport::TempoMap;
use dropseed_plugin_api::{
    DSPluginSaveState, HostRequestChannelReceiver, HostRequestFlags, PluginInstanceID,
    PluginMainThread,
};
use fnv::FnvHashMap;
use meadowlark_core_types::time::{SampleRate, Seconds};
use raw_window_handle::RawWindowHandle;
use smallvec::SmallVec;
use std::error::Error;

use crate::engine::{OnIdleEvent, PluginActivatedStatus};
use crate::graph::{DSEdgeID, PortChannelID};
use crate::utils::thread_id::SharedThreadIDs;

use super::channel::{
    MainToProcParamValue, PlugHostChannelMainThread, PluginActiveState, SharedPluginHostProcessor,
};
use super::error::{ActivatePluginError, SetParamValueError};
use super::PluginHostProcessorWrapper;

mod sync_ports;

// The amount of time to smooth/declick the audio outputs when
// bypassing/unbypassing the plugin.
static BYPASS_DECLICK_SECS: Seconds = Seconds(3.0 / 1000.0);

pub struct PluginHostMainThread {
    id: PluginInstanceID,

    plug_main_thread: Box<dyn PluginMainThread>,

    port_ids: PluginHostPortIDs,
    next_port_id: u32,
    free_port_ids: Vec<PortID>,

    channel: PlugHostChannelMainThread,

    save_state: DSPluginSaveState,

    params: FnvHashMap<ParamID, ParamInfo>,
    gesturing_params: FnvHashMap<ParamID, bool>,
    latency: i64,
    is_loaded: bool,

    num_audio_in_channels: usize,
    num_audio_out_channels: usize,

    host_request_rx: HostRequestChannelReceiver,
    remove_requested: bool,
    save_state_dirty: bool,
    restarting: bool,
}

impl PluginHostMainThread {
    pub(crate) fn new(
        id: PluginInstanceID,
        mut save_state: DSPluginSaveState,
        mut plug_main_thread: Box<dyn PluginMainThread>,
        host_request_rx: HostRequestChannelReceiver,
        plugin_loaded: bool,
        coll_handle: &basedrop::Handle,
    ) -> Self {
        if let Some(save_state) = save_state.raw_state.clone() {
            match plug_main_thread.load_save_state(save_state) {
                Ok(()) => {
                    log::trace!("Plugin {:?} successfully loaded save state", &id);
                }
                Err(e) => {
                    log::error!("Plugin {:?} failed to load save state: {}", &id, e);
                }
            }
        }

        // Collect the total number of audio channels to make it easier
        // for the audio graph compiler.
        let (num_audio_in_channels, num_audio_out_channels) =
            if let Some(audio_ports_ext) = &save_state.backup_audio_ports_ext {
                (audio_ports_ext.total_in_channels(), audio_ports_ext.total_out_channels())
            } else {
                (0, 0)
            };

        if save_state.backup_audio_ports_ext.is_none() {
            // Start with an empty config (no audio or note ports) if
            // the previous backup config did not exist in the save
            // state.
            save_state.backup_audio_ports_ext = Some(PluginAudioPortsExt::empty());
            save_state.backup_note_ports_ext = Some(PluginNotePortsExt::empty());
        }

        let bypassed = save_state.bypassed;

        Self {
            id,
            plug_main_thread,
            port_ids: PluginHostPortIDs::new(),
            next_port_id: 0,
            free_port_ids: Vec::new(),
            channel: PlugHostChannelMainThread::new(bypassed, coll_handle),
            save_state,
            params: FnvHashMap::default(),
            gesturing_params: FnvHashMap::default(),
            latency: 0,
            is_loaded: plugin_loaded,
            num_audio_in_channels,
            num_audio_out_channels,
            host_request_rx,
            remove_requested: false,
            save_state_dirty: false,
            restarting: false,
        }
    }

    /// Returns `true` if this plugin has successfully been loaded from disk,
    /// `false` if not.
    ///
    /// This can be `false` in the case where one user opens another user's
    /// project on a different machine, and that machine does not have this
    /// plugin installed.
    pub fn is_loaded(&self) -> bool {
        self.is_loaded
    }

    /// Returns `true` if this plugin is currently being bypassed.
    pub fn is_bypassed(&self) -> bool {
        self.save_state.bypassed
    }

    /// Bypass/unbypass this plugin.
    pub fn set_bypassed(&mut self, bypassed: bool) {
        if self.save_state.bypassed != bypassed {
            // The user has manually bypassed/unpassed this plugin, so make
            // sure it stays bypassed/unbypassed the next time it is loaded
            // from a save state.
            self.save_state.bypassed = bypassed;
            self.save_state_dirty = true;

            self.channel.shared_state.set_bypassed(bypassed);
        }
    }

    /// Tell the plugin to load the given save state.
    ///
    /// This will return `Err(e)` if the plugin failed to load the given
    /// save state.
    pub fn load_save_state(&mut self, state: Vec<u8>) -> Result<(), String> {
        self.save_state_dirty = true;
        self.plug_main_thread.load_save_state(state)
    }

    /// This will return `true` if the plugin's save state has changed
    /// since the last time its save state was collected.
    pub fn is_save_state_dirty(&self) -> bool {
        self.save_state_dirty
    }

    /// Collect the save state of this plugin.
    pub fn collect_save_state(&mut self) -> DSPluginSaveState {
        if self.save_state_dirty {
            self.save_state_dirty = false;

            let raw_state = match self.plug_main_thread.collect_save_state() {
                Ok(raw_state) => raw_state,
                Err(e) => {
                    log::error!("Failed to collect save state from plugin {:?}: {}", &self.id, e);

                    None
                }
            };

            self.save_state.raw_state = raw_state;
        }

        self.save_state.clone()
    }

    /// Set the value of the given parameter.
    ///
    /// If successful, this returns the actual (clamped) value that the
    /// plugin accepted.
    pub fn set_param_value(
        &mut self,
        param_id: ParamID,
        value: f64,
    ) -> Result<f64, SetParamValueError> {
        if let Some(param_info) = self.params.get(&param_id) {
            if param_info.flags.contains(ParamInfoFlags::IS_READONLY) {
                Err(SetParamValueError::ParamIsReadOnly(param_id))
            } else {
                let value = value.clamp(param_info.min_value, param_info.max_value);

                if let Some(param_queues) = &mut self.channel.param_queues {
                    param_queues
                        .to_proc_param_value_tx
                        .set(param_id, MainToProcParamValue { value });
                    param_queues.to_proc_param_value_tx.producer_done();
                } else {
                    // TODO: Flush parameters on main thread.
                }

                self.save_state_dirty = true;

                Ok(value)
            }
        } else {
            Err(SetParamValueError::ParamDoesNotExist(param_id))
        }
    }

    /// Set the modulation amount on the given parameter.
    ///
    /// If successful, this returns the actual (clamped) modulation
    /// amount that the plugin accepted.
    pub fn set_param_mod_amount(
        &mut self,
        param_id: ParamID,
        mod_amount: f64,
    ) -> Result<f64, SetParamValueError> {
        if let Some(param_info) = self.params.get(&param_id) {
            if param_info.flags.contains(ParamInfoFlags::IS_MODULATABLE) {
                Err(SetParamValueError::ParamIsNotModulatable(param_id))
            } else {
                // TODO: Clamp value?

                if let Some(param_queues) = &mut self.channel.param_queues {
                    param_queues
                        .to_proc_param_mod_tx
                        .set(param_id, MainToProcParamValue { value: mod_amount });
                    param_queues.to_proc_param_mod_tx.producer_done();
                } else {
                    // TODO: Flush parameters on main thread.
                }

                Ok(mod_amount)
            }
        } else {
            Err(SetParamValueError::ParamDoesNotExist(param_id))
        }
    }

    /// Get the display text for the given parameter with the given
    /// value.
    pub fn param_value_to_text(
        &self,
        param_id: ParamID,
        value: f64,
        text_buffer: &mut String,
    ) -> Result<(), String> {
        self.plug_main_thread.param_value_to_text(param_id, value, text_buffer)
    }

    /// Conver the given text input to a value for this parameter.
    pub fn param_text_to_value(&self, param_id: ParamID, text_input: &str) -> Option<f64> {
        self.plug_main_thread.param_text_to_value(param_id, text_input)
    }

    /// If `floating` is `true`, then this returns whether or not this plugin instance supports
    /// creating a custom GUI in a floating window that the plugin manages itself.
    ///
    /// If `floating` is `false`, then this returns whether or not this plugin instance supports
    /// embedding a custom GUI into a window managed by the host.
    fn supports_gui(&self, floating: bool) -> bool {
        self.plug_main_thread.supports_gui(floating)
    }

    /// Create a new floating GUI in a window managed by the plugin itself.
    fn create_new_floating_gui(
        &mut self,
        suggested_title: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        self.plug_main_thread.create_new_floating_gui(suggested_title)
    }

    /// Create a new embedded GUI in a window managed by the host.
    ///
    /// * `scale` - The absolute GUI scaling factor. This overrides any OS info.
    ///     * This should not be used if the windowing API relies upon logical pixels
    /// (such as Cocoa on MacOS).
    ///     * If this plugin prefers to work out the scaling factor itself by querying
    /// the OS directly, then ignore this value.
    /// * `size`
    ///     * If the plugin's GUI is resizable, and the size is known from previous a
    /// previous session, then put the size from that previous session here.
    ///     * If the plugin's GUI is not resizable, then this will be ignored.
    /// * `parent_window` - The `RawWindowHandle` of the window that the GUI should be
    /// embedded into.
    fn create_new_embedded_gui(
        &mut self,
        scale: Option<f64>,
        size: Option<GuiSize>,
        parent_window: RawWindowHandle,
    ) -> Result<EmbeddedGuiInfo, Box<dyn Error>> {
        self.plug_main_thread.create_new_embedded_gui(scale, size, parent_window)
    }

    /// Destroy the currently active GUI.
    fn destroy_gui(&mut self) {
        self.plug_main_thread.destroy_gui();
    }

    fn gui_resize_hints(&self) -> Option<GuiResizeHints> {
        self.plug_main_thread.gui_resize_hints()
    }

    /// If the plugin gui is resizable, then the plugin will calculate the closest
    /// usable size which fits in the given size. Only for embedded GUIs.
    ///
    /// This method does not change the size of the current GUI.
    fn adjust_gui_size(&mut self, size: GuiSize) -> GuiSize {
        self.plug_main_thread.adjust_gui_size(size)
    }

    /// Set the size of the plugin's GUI. Only for embedded GUIs.
    ///
    /// On success, this returns the actual size the plugin resized to.
    fn set_gui_size(&mut self, size: GuiSize) -> Result<GuiSize, Box<dyn Error>> {
        let res = self.plug_main_thread.set_gui_size(size);

        if let Ok(new_size) = &res {
            self.save_state.gui_size = Some(*new_size);
            self.save_state_dirty = true;
        }

        res
    }

    /// Set the absolute GUI scaling factor. This overrides any OS info.
    ///
    /// This should not be used if the windowing API relies upon logical pixels
    /// (such as Cocoa on MacOS).
    ///
    /// If this plugin prefers to work out the scaling factor itself by querying
    /// the OS directly, then ignore this value.
    ///
    /// Returns `true` if the plugin applied the scaling.
    /// Returns `false` if the plugin could not apply the scaling, or if the
    /// plugin ignored the request.
    fn set_gui_scale(&mut self, scale: f64) -> bool {
        self.plug_main_thread.set_gui_scale(scale)
    }

    /// Show the currently active GUI.
    fn show_gui(&mut self) -> Result<(), Box<dyn Error>> {
        self.plug_main_thread.show_gui()
    }

    /// Hide the currently active GUI.
    ///
    /// Note that hiding the GUI is not the same as destroying the GUI.
    /// Hiding only hides the window content, it does not free the GUI's
    /// resources.  Yet it may be a good idea to stop painting timers
    /// when a plugin GUI is hidden.
    fn hide_gui(&mut self) -> Result<(), Box<dyn Error>> {
        self.plug_main_thread.hide_gui()
    }

    /// Returns `Ok(())` if the plugin can be activated right now.
    pub fn can_activate(&self) -> Result<(), ActivatePluginError> {
        // TODO: without this check it seems something is attempting to activate the plugin twice
        if self.channel.shared_state.get_active_state() == PluginActiveState::Active {
            return Err(ActivatePluginError::AlreadyActive);
        }
        Ok(())
    }

    /// Get the audio port configuration on this plugin.
    ///
    /// This will return `None` if this plugin is unloaded and there
    /// exists no backup of the audio ports extension.
    pub fn audio_ports_ext(&self) -> Option<&PluginAudioPortsExt> {
        self.save_state.backup_audio_ports_ext.as_ref()
    }

    /// Get the note port configuration on this plugin.
    ///
    /// This will return `None` if this plugin is unloaded and there
    /// exists no backup of the note ports extension.
    pub fn note_ports_ext(&self) -> Option<&PluginNotePortsExt> {
        self.save_state.backup_note_ports_ext.as_ref()
    }

    /// The total number of audio input channels on this plugin.
    pub fn num_audio_in_channels(&self) -> usize {
        self.num_audio_in_channels
    }

    /// The total number of audio output channels on this plugin.
    pub fn num_audio_out_channels(&self) -> usize {
        self.num_audio_out_channels
    }

    /// The unique ID for this plugin instance.
    pub fn id(&self) -> &PluginInstanceID {
        &self.id
    }

    /// Schedule this plugin to be deactivated.
    ///
    /// This plugin will not be fully deactivated until the plugin host's
    /// processor is dropped in the process thread (which in turn sets the
    /// `PluginActiveState::DroppedAndReadyToDeactivate` flag).
    ///
    /// This returns the plugin host's processor, which is then sent to the
    /// new schedule to be dropped. This is necessary because otherwise it
    /// is possible that the new schedule can be sent before the old
    /// processor has a chance to drop in the process thread, causing it to
    /// be later dropped in the garbage collector thread (not what we want).
    pub(crate) fn schedule_deactivate(
        &mut self,
        coll_handle: &basedrop::Handle,
    ) -> Option<Shared<PluginHostProcessorWrapper>> {
        if self.channel.shared_state.get_active_state() != PluginActiveState::Active {
            return None;
        }

        let plug_proc_to_drop =
            Some(self.channel.drop_processor_pointer_on_main_thread(coll_handle));

        // Set a flag to alert the process thread to drop this plugin host's
        // processor.
        self.channel.shared_state.set_active_state(PluginActiveState::WaitingToDrop);

        plug_proc_to_drop
    }

    /// Schedule this plugin to be removed.
    ///
    /// This plugin will not be fully removed/dropped until the plugin host's
    /// processor is dropped in the process thread (which in turn sets the
    /// `PluginActiveState::DroppedAndReadyToDeactivate` flag).
    ///
    /// This returns the plugin host's processor, which is then sent to the
    /// new schedule to be dropped (because removing a plugin always
    /// requires the graph to recompile). This is necessary because otherwise
    /// it is possible that the new schedule can be sent before the old
    /// processor has a chance to drop in the process thread, causing it to
    /// be later dropped in the garbage collector thread (not what we want).
    pub(crate) fn schedule_remove(
        &mut self,
        coll_handle: &basedrop::Handle,
    ) -> Option<Shared<PluginHostProcessorWrapper>> {
        self.remove_requested = true;

        self.plug_main_thread.destroy_gui();

        self.schedule_deactivate(coll_handle)
    }

    /// Inform the plugin that the project's tempo map has been updated.
    pub(crate) fn update_tempo_map(&mut self, new_tempo_map: &Shared<TempoMap>) {
        self.plug_main_thread.update_tempo_map(new_tempo_map);
    }

    /// Returns the plugin host's processor (wrapped in a thread-safe shared
    /// container).
    pub(crate) fn shared_processor(&self) -> &SharedPluginHostProcessor {
        self.channel.shared_processor()
    }

    /// The abstract graph's port IDs for each of the corresponding
    /// ports/channels in this plugin.
    pub(crate) fn port_ids(&self) -> &PluginHostPortIDs {
        &self.port_ids
    }

    // TODO: let the user manually activate an inactive plugin
    pub(crate) fn activate(
        &mut self,
        sample_rate: SampleRate,
        min_frames: u32,
        max_frames: u32,
        graph_helper: &mut AudioGraphHelper,
        edge_id_to_ds_edge_id: &mut FnvHashMap<EdgeID, DSEdgeID>,
        thread_ids: SharedThreadIDs,
        schedule_version: u64,
        coll_handle: &basedrop::Handle,
    ) -> Result<PluginActivatedStatus, ActivatePluginError> {
        // Return an error if this plugin cannot be activated right now.
        self.can_activate()?;

        let set_inactive_with_error = |self_: &mut Self| {
            self_.channel.shared_state.set_active_state(PluginActiveState::InactiveWithError);
            self_.channel.param_queues = None;
        };

        // Retrieve the (new) audio ports and note ports configuration of this plugin.
        let audio_ports = match self.plug_main_thread.audio_ports_ext() {
            Ok(audio_ports) => audio_ports,
            Err(e) => {
                set_inactive_with_error(self);
                return Err(ActivatePluginError::PluginFailedToGetAudioPortsExt(e));
            }
        };
        let note_ports = match self.plug_main_thread.note_ports_ext() {
            Ok(note_ports) => note_ports,
            Err(e) => {
                set_inactive_with_error(self);
                return Err(ActivatePluginError::PluginFailedToGetNotePortsExt(e));
            }
        };

        // Retrieve the (new) parameters on this plugin.
        let num_params = self.plug_main_thread.num_params() as usize;
        let mut new_params: FnvHashMap<ParamID, ParamInfo> = FnvHashMap::default();
        let mut param_values: Vec<(ParamInfo, f64)> = Vec::with_capacity(num_params);
        for i in 0..num_params {
            match self.plug_main_thread.param_info(i) {
                Ok(info) => match self.plug_main_thread.param_value(info.stable_id) {
                    Ok(value) => {
                        let id = info.stable_id;

                        new_params.insert(id, info.clone());
                        param_values.push((info, value));
                    }
                    Err(_) => {
                        set_inactive_with_error(self);
                        return Err(ActivatePluginError::PluginFailedToGetParamValue(
                            info.stable_id,
                        ));
                    }
                },
                Err(_) => {
                    set_inactive_with_error(self);
                    return Err(ActivatePluginError::PluginFailedToGetParamInfo(i));
                }
            }
        }

        // Retrieve the (new) latency of this plugin.
        let latency = self.plug_main_thread.latency();

        // Add/remove ports from the abstract graph according to the plugin's new
        // audio ports and note ports extensions. This also updates the new latency
        // for the node in the abstract graph.
        //
        // On success, returns:
        // - a list of all edges that were removed as a result of the plugin
        // removing some of its ports
        // - `true` if the audio graph needs to be recompiled as a result of the
        // plugin adding/removing ports.
        let (removed_edges, mut needs_recompile) = match sync_ports::sync_ports_in_graph(
            self,
            graph_helper,
            edge_id_to_ds_edge_id,
            &audio_ports,
            &note_ports,
            latency,
            coll_handle,
        ) {
            Ok((removed_edges, needs_recompile)) => (removed_edges, needs_recompile),
            Err(e) => {
                set_inactive_with_error(self);
                return Err(e);
            }
        };

        // Attempt to activate the plugin.
        match self.plug_main_thread.activate(sample_rate, min_frames, max_frames, coll_handle) {
            Ok(info) => {
                let new_latency = if self.latency != latency {
                    self.latency = latency;
                    needs_recompile = true;
                    Some(latency)
                } else {
                    None
                };

                self.params = new_params;

                let audio_ports_changed =
                    if let Some(old_audio_ports) = &self.save_state.backup_audio_ports_ext {
                        &audio_ports != old_audio_ports
                    } else {
                        true
                    };
                let note_ports_changed =
                    if let Some(old_note_ports) = &self.save_state.backup_note_ports_ext {
                        &note_ports != old_note_ports
                    } else {
                        true
                    };

                let new_audio_ports_ext = if audio_ports_changed {
                    self.num_audio_in_channels = audio_ports.total_in_channels();
                    self.num_audio_out_channels = audio_ports.total_out_channels();

                    self.save_state.backup_audio_ports_ext = Some(audio_ports.clone());
                    Some(audio_ports)
                } else {
                    None
                };
                let new_note_ports_ext = if note_ports_changed {
                    self.save_state.backup_note_ports_ext = Some(note_ports.clone());
                    Some(note_ports)
                } else {
                    None
                };

                // If the plugin restarting requires the graph to recompile first (because
                // the port configuration or latency configuration has changed), tell the
                // new processor to wait for the new schedule before processing.
                let sched_version =
                    if needs_recompile { schedule_version + 1 } else { schedule_version };

                // The number of frames to smooth/declick the audio outputs when
                // bypassing/unbypassing the plugin.
                let bypass_declick_frames =
                    BYPASS_DECLICK_SECS.to_nearest_frame_round(sample_rate).0 as usize;

                // Send the new processor to the process thread.
                self.channel.shared_state.set_active_state(PluginActiveState::Active);
                self.channel.new_processor(
                    info.processor,
                    self.id.unique_id(),
                    num_params,
                    thread_ids,
                    sched_version,
                    bypass_declick_frames,
                    coll_handle,
                );

                // Make sure that the new configurations are saved in the save state of
                // this plugin.
                self.save_state.active = true;
                self.save_state_dirty = true;

                Ok(PluginActivatedStatus {
                    new_parameters: param_values,
                    new_audio_ports_ext,
                    new_note_ports_ext,
                    internal_handle: info.internal_handle,
                    new_latency,
                    removed_edges,
                    caused_recompile: needs_recompile,
                })
            }
            Err(e) => {
                set_inactive_with_error(self);
                Err(ActivatePluginError::PluginSpecific(e))
            }
        }
    }

    /// Poll parameter updates and requests from the plugin and the plugin host's
    /// processor.
    ///
    /// Returns the status of this plugin, along with a list of any parameters
    /// that were modified inside the plugin's custom GUI.
    pub(crate) fn on_idle(
        &mut self,
        sample_rate: SampleRate,
        min_frames: u32,
        max_frames: u32,
        coll_handle: &basedrop::Handle,
        graph_helper: &mut AudioGraphHelper,
        events_out: &mut SmallVec<[OnIdleEvent; 32]>,
        edge_id_to_ds_edge_id: &mut FnvHashMap<EdgeID, DSEdgeID>,
        thread_ids: &SharedThreadIDs,
        schedule_version: u64,
    ) -> (OnIdleResult, SmallVec<[ParamModifiedInfo; 4]>, Option<Shared<PluginHostProcessorWrapper>>)
    {
        let mut modified_params: SmallVec<[ParamModifiedInfo; 4]> = SmallVec::new();
        let mut processor_to_drop = None;

        // Get the latest request flags and activation state.
        let request_flags = self.host_request_rx.fetch_requests();
        let mut active_state = self.channel.shared_state.get_active_state();

        if request_flags.contains(HostRequestFlags::MARK_DIRTY) {
            log::trace!("Plugin {:?} manually marked its state as dirty", &self.id);

            // The plugin has manually changed its save state, so mark the state
            // as dirty so it can be collected later.
            self.save_state_dirty = true;
        }

        if request_flags.contains(HostRequestFlags::CALLBACK) {
            log::trace!("Plugin {:?} requested the host call `on_main_thread()`", &self.id);

            self.plug_main_thread.on_main_thread();
        }

        if request_flags.contains(HostRequestFlags::RESCAN_PARAMS) {
            log::debug!("Plugin {:?} requested the host rescan its parameters", &self.id);

            // The plugin has requested the host to rescan its list of parameters.

            // TODO
        }

        // Poll for parameter updates from the plugin host's processor.
        if let Some(params_queue) = &mut self.channel.param_queues {
            params_queue.from_proc_param_value_rx.consume(|param_id, new_value| {
                let is_gesturing = if let Some(gesture) = new_value.gesture {
                    let _ = self.gesturing_params.insert(*param_id, gesture.is_begin);

                    if !gesture.is_begin {
                        // Only mark the state dirty once the user has finished adjusting
                        // the parameter.
                        self.save_state_dirty = true;
                    }

                    gesture.is_begin
                } else {
                    self.save_state_dirty = true;

                    *self.gesturing_params.get(param_id).unwrap_or(&false)
                };

                modified_params.push(ParamModifiedInfo {
                    param_id: *param_id,
                    new_value: new_value.value,
                    is_gesturing,
                })
            });
        }

        if request_flags.intersects(HostRequestFlags::RESTART | HostRequestFlags::RESCAN_PORTS) {
            if request_flags.contains(HostRequestFlags::RESTART) {
                log::debug!("Plugin {:?} requested the host to restart the plugin", &self.id);
            }
            if request_flags.contains(HostRequestFlags::RESCAN_PORTS) {
                log::debug!(
                    "Plugin {:?} requested the host to rescan its audio and/or note ports",
                    &self.id
                );
            }

            // The plugin has requested the host to restart the plugin (or rescan its
            // audio and/or note ports).
            //
            // We just do a full restart and rescan for all "rescan port" requests for
            // simplicity. I don't expect plugins to change the state of their ports
            // often anyway.

            self.restarting = true;
            processor_to_drop = self.schedule_deactivate(coll_handle);

            if active_state == PluginActiveState::Active {
                active_state = PluginActiveState::WaitingToDrop;
            }
        }

        if request_flags.intersects(HostRequestFlags::GUI_CLOSED | HostRequestFlags::GUI_DESTROYED)
        {
            log::trace!("Plugin {:?} has closed its custom GUI", &self.id);

            let was_destroyed = request_flags.contains(HostRequestFlags::GUI_DESTROYED);

            events_out
                .push(OnIdleEvent::PluginGuiClosed { plugin_id: self.id.clone(), was_destroyed });
        }

        if request_flags.contains(HostRequestFlags::GUI_SHOW) {
            log::trace!("Plugin {:?} requested the host to show its GUI", &self.id);

            events_out.push(OnIdleEvent::PluginRequestedToShowGui { plugin_id: self.id.clone() });
        }

        if request_flags.contains(HostRequestFlags::GUI_HIDE) {
            log::trace!("Plugin {:?} requested the host to hide its GUI", &self.id);

            events_out.push(OnIdleEvent::PluginRequestedToHideGui { plugin_id: self.id.clone() });
        }

        if request_flags.contains(HostRequestFlags::GUI_RESIZE) {
            log::trace!("Plugin {:?} requested the host to resize its GUI", &self.id);

            if let Some(size) = self.host_request_rx.fetch_gui_size_request() {
                events_out.push(OnIdleEvent::PluginRequestedToResizeGui {
                    plugin_id: self.id.clone(),
                    size,
                });
            }
        }

        if request_flags.contains(HostRequestFlags::GUI_HINTS_CHANGED) {
            let resize_hints = self.plug_main_thread.gui_resize_hints();

            log::trace!(
                "Plugin {:?} has changed its gui resize hints to {:?}",
                &self.id,
                &resize_hints
            );

            events_out.push(OnIdleEvent::PluginChangedGuiResizeHints {
                plugin_id: self.id.clone(),
                resize_hints,
            });
        }

        if active_state == PluginActiveState::DroppedAndReadyToDeactivate {
            // The plugin host's processor has successfully been dropped after
            // scheduling this plugin to be deactivated, so it is safe to fully
            // deactivate this plugin now.

            self.plug_main_thread.deactivate();
            self.channel.shared_state.set_active_state(PluginActiveState::Inactive);

            if !self.remove_requested {
                let mut res = OnIdleResult::PluginDeactivated;

                if self.restarting || request_flags.contains(HostRequestFlags::PROCESS) {
                    // The plugin has requested to be reactivated after being deactivated.

                    match self.activate(
                        sample_rate,
                        min_frames,
                        max_frames,
                        graph_helper,
                        edge_id_to_ds_edge_id,
                        thread_ids.clone(),
                        schedule_version,
                        coll_handle,
                    ) {
                        Ok(r) => {
                            self.save_state_dirty = true;
                            res = OnIdleResult::PluginActivated(r);
                        }
                        Err(e) => res = OnIdleResult::PluginFailedToActivate(e),
                    }
                } else {
                    // The user has manually deactivated this plugin, so make sure
                    // it stays deactivated the next time it is loaded from a save
                    // state.
                    self.save_state.active = false;
                    self.save_state_dirty = true;
                }

                return (res, modified_params, processor_to_drop);
            } else {
                // Plugin is ready to be fully removed/dropped.
                return (OnIdleResult::PluginReadyToRemove, modified_params, processor_to_drop);
            }
        } else if request_flags.contains(HostRequestFlags::PROCESS)
            && !self.remove_requested
            && !self.restarting
        {
            log::trace!("Plugin {:?} requested the host to start processing the plugin", &self.id);

            // The plugin has requested the host to start processing this plugin.

            if active_state == PluginActiveState::Active {
                self.channel.shared_state.set_process_requested();
            } else if active_state == PluginActiveState::Inactive
                || active_state == PluginActiveState::InactiveWithError
            {
                let res = match self.activate(
                    sample_rate,
                    min_frames,
                    max_frames,
                    graph_helper,
                    edge_id_to_ds_edge_id,
                    thread_ids.clone(),
                    schedule_version,
                    coll_handle,
                ) {
                    Ok(r) => {
                        self.save_state_dirty = true;

                        OnIdleResult::PluginActivated(r)
                    }
                    Err(e) => OnIdleResult::PluginFailedToActivate(e),
                };

                return (res, modified_params, processor_to_drop);
            }
        }

        (OnIdleResult::Ok, modified_params, processor_to_drop)
    }
}

/// The abstract graph's port IDs for each of the corresponding
/// ports/channels in this plugin.
pub(crate) struct PluginHostPortIDs {
    /// Maps Dropseed's id for this port/channel to the abstract graph's
    /// port ID.
    pub channel_id_to_port_id: FnvHashMap<PortChannelID, PortID>,
    /// Maps the abstract graph's port ID to Dropseed's id for this
    /// port/channel.
    pub port_id_to_channel_id: FnvHashMap<PortID, PortChannelID>,

    /// The abstract graph's port IDs for each channel in the main audio
    /// input port.
    pub main_audio_in_port_ids: Vec<PortID>,
    /// The abstract graph's port IDs for each channel in the main audio
    /// output port.
    pub main_audio_out_port_ids: Vec<PortID>,

    /// The abstract graph's port ID for the main note input port.
    pub main_note_in_port_id: Option<PortID>,
    /// The abstract graph's port ID for the main note output port.
    pub main_note_out_port_id: Option<PortID>,
    /// The abstract graph's port ID for the main automation input port.
    pub automation_in_port_id: Option<PortID>,
    /// The abstract graph's port ID for the main automation output port.
    pub automation_out_port_id: Option<PortID>,
}

impl PluginHostPortIDs {
    pub fn new() -> Self {
        Self {
            channel_id_to_port_id: FnvHashMap::default(),
            port_id_to_channel_id: FnvHashMap::default(),
            main_audio_in_port_ids: Vec::new(),
            main_audio_out_port_ids: Vec::new(),
            main_note_in_port_id: None,
            main_note_out_port_id: None,
            automation_in_port_id: None,
            automation_out_port_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParamModifiedInfo {
    pub param_id: ParamID,
    pub new_value: Option<f64>,
    pub is_gesturing: bool,
}

pub(crate) enum OnIdleResult {
    Ok,
    PluginDeactivated,
    PluginActivated(PluginActivatedStatus),
    PluginReadyToRemove,
    PluginFailedToActivate(ActivatePluginError),
}
