use audio_graph::InsertedDelay;

use crate::processor_schedule::tasks::{
    AudioDelayCompNode, AudioDelayCompTask, NoteDelayCompNode, NoteDelayCompTask,
    ParamEventDelayCompNode, ParamEventDelayCompTask, Task,
};

use super::super::error::GraphCompilerError;
use super::super::shared_pools::{
    DelayCompKey, GraphSharedPools, SharedAudioDelayCompNode, SharedNoteDelayCompNode,
    SharedParamEventDelayCompNode,
};
use super::super::PortType;

pub(super) fn construct_delay_comp_task(
    inserted_delay: &InsertedDelay,
    delay: i64,
    shared_pool: &mut GraphSharedPools,
    coll_handle: &basedrop::Handle,
) -> Result<Task, GraphCompilerError> {
    if delay < 0 {
        return Err(GraphCompilerError::UnexpectedError(format!(
            "Abstract schedule inserted a delay with negative latency {:?}",
            inserted_delay
        )));
    }
    let delay = delay as u32;

    let delay_comp_key = DelayCompKey { edge: inserted_delay.edge, delay };

    let task = match inserted_delay.input_buffer.type_index {
        PortType::AUDIO_TYPE_IDX => {
            let audio_in = shared_pool
                .buffers
                .audio_buffer_pool
                .initialized_buffer_at_index(inserted_delay.input_buffer.buffer_index.0);
            let audio_out = shared_pool
                .buffers
                .audio_buffer_pool
                .initialized_buffer_at_index(inserted_delay.output_buffer.buffer_index.0);

            let shared_node =
                shared_pool.delay_comp_nodes.audio.entry(delay_comp_key).or_insert_with(|| {
                    SharedAudioDelayCompNode::new(AudioDelayCompNode::new(delay), coll_handle)
                });
            shared_node.active = true;

            Task::AudioDelayComp(AudioDelayCompTask {
                shared_node: shared_node.clone(),
                audio_in,
                audio_out,
            })
        }
        PortType::NOTE_TYPE_IDX => {
            let note_in = shared_pool
                .buffers
                .note_buffer_pool
                .buffer_at_index(inserted_delay.input_buffer.buffer_index.0);
            let note_out = shared_pool
                .buffers
                .note_buffer_pool
                .buffer_at_index(inserted_delay.output_buffer.buffer_index.0);

            let shared_node =
                shared_pool.delay_comp_nodes.note.entry(delay_comp_key).or_insert_with(|| {
                    SharedNoteDelayCompNode::new(NoteDelayCompNode::new(delay), coll_handle)
                });
            shared_node.active = true;

            Task::NoteDelayComp(NoteDelayCompTask {
                shared_node: shared_node.clone(),
                note_in,
                note_out,
            })
        }
        PortType::PARAM_AUTOMATION_TYPE_IDX => {
            let event_in = shared_pool
                .buffers
                .param_event_buffer_pool
                .buffer_at_index(inserted_delay.input_buffer.buffer_index.0);
            let event_out = shared_pool
                .buffers
                .param_event_buffer_pool
                .buffer_at_index(inserted_delay.output_buffer.buffer_index.0);

            let shared_node =
                shared_pool.delay_comp_nodes.param_event.entry(delay_comp_key).or_insert_with(
                    || {
                        SharedParamEventDelayCompNode::new(
                            ParamEventDelayCompNode::new(delay),
                            coll_handle,
                        )
                    },
                );
            shared_node.active = true;

            Task::ParamEventDelayComp(ParamEventDelayCompTask {
                shared_node: shared_node.clone(),
                event_in,
                event_out,
            })
        }
        _ => {
            return Err(GraphCompilerError::UnexpectedError(format!(
                "Abstract schedule inserted a delay with unkown type index {:?}",
                inserted_delay
            )));
        }
    };

    Ok(task)
}
