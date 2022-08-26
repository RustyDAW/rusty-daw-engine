use audio_graph::{AudioGraphHelper, ScheduleEntry};

use crate::processor_schedule::tasks::Task;

mod delay_comp_task;
mod graph_in_out_task;
mod plugin_task;
mod sum_task;

pub(super) mod verifier;

use verifier::Verifier;

use super::error::GraphCompilerError;
use super::shared_pools::GraphSharedPools;
use super::{PluginInstanceID, PortType, ProcessorSchedule};

pub(super) fn compile_graph(
    shared_pool: &mut GraphSharedPools,
    graph_helper: &mut AudioGraphHelper,
    graph_in_id: &PluginInstanceID,
    graph_out_id: &PluginInstanceID,
    num_graph_in_audio_ports: usize,
    num_graph_out_audio_ports: usize,
    verifier: &mut Verifier,
    coll_handle: &basedrop::Handle,
) -> Result<ProcessorSchedule, GraphCompilerError> {
    let num_plugins = shared_pool.plugin_hosts.pool.len();
    let mut tasks: Vec<Task> = Vec::with_capacity(num_plugins * 2);

    // The `audio_graph` crate compiles a schedule for us in its purest
    // "abstract" form (as a list of Node IDs with their corresponding
    // list of assigned buffer IDs).
    let abstract_schedule = graph_helper.compile()?;

    // We now take that "abstract" schedule and do a one-to-one translation
    // into a schedule with our desired tasks (a list of pointers to
    // processors with their corresponding list of assigned buffer pointers):

    // This flag is used later to see if any of these previously created
    // delay compensation nodes are no longer being used.
    for node in shared_pool.delay_comp_nodes.audio.values_mut() {
        node.active = false;
    }
    for node in shared_pool.delay_comp_nodes.note.values_mut() {
        node.active = false;
    }
    for node in shared_pool.delay_comp_nodes.param_event.values_mut() {
        node.active = false;
    }

    // Allocate/truncate the list of shared buffers based on how many exist
    // in the new abstract schedule.
    shared_pool.buffers.set_num_buffers(
        abstract_schedule.num_buffers[PortType::AUDIO_IDX],
        abstract_schedule.num_buffers[PortType::NOTE_IDX],
        abstract_schedule.num_buffers[PortType::PARAM_AUTOMATION_IDX],
    );

    for schedule_entry in abstract_schedule.schedule.iter() {
        match schedule_entry {
            ScheduleEntry::Node(scheduled_node) => {
                let task = if scheduled_node.id.0 == graph_in_id._node_id() {
                    // The `graph in` node is a special node that handles inputting all
                    // data from the user's system (microphone inputs, midi controller
                    // inputs, etc.) into the graph.
                    graph_in_out_task::construct_graph_in_task(
                        scheduled_node,
                        shared_pool,
                        graph_in_id,
                        num_graph_in_audio_ports,
                    )?
                } else if scheduled_node.id.0 == graph_out_id._node_id() {
                    // The `graph out` node is a special node that handles outputting
                    // the resulting data processed by the graph back to the user's
                    // system (speakers out, midi out, etc.).
                    graph_in_out_task::construct_graph_out_task(
                        scheduled_node,
                        shared_pool,
                        graph_out_id,
                        num_graph_out_audio_ports,
                    )?
                } else {
                    // Construct a task for a plugin.
                    plugin_task::construct_plugin_task(scheduled_node, shared_pool)?
                };

                tasks.push(task);
            }
            ScheduleEntry::Delay(inserted_delay) => {
                let delay = inserted_delay.delay.round() as i64;
                if delay == 0 {
                    // Not technically an error, but this shouldn't happen in the
                    // first place.
                    log::warn!(
                        "Abstract schedule inserted a delay node with 0 latency {:?}",
                        inserted_delay
                    );
                }

                // Construct a delay compensation task.
                tasks.push(delay_comp_task::construct_delay_comp_task(
                    inserted_delay,
                    delay,
                    shared_pool,
                    coll_handle,
                )?);
            }
            ScheduleEntry::Sum(inserted_sum) => {
                // Construct a summation task (a task that adds multiple input buffers
                // into a single output buffer).
                tasks.push(sum_task::construct_sum_task(inserted_sum, shared_pool)?);
            }
        }
    }

    // Remove all delay compensation nodes that are no longer being used.
    //
    // TODO: Use `drain_filter()` once it becomes stable.
    shared_pool.delay_comp_nodes.audio =
        shared_pool.delay_comp_nodes.audio.drain().filter(|(_, node)| node.active).collect();
    shared_pool.delay_comp_nodes.note =
        shared_pool.delay_comp_nodes.note.drain().filter(|(_, node)| node.active).collect();
    shared_pool.delay_comp_nodes.param_event =
        shared_pool.delay_comp_nodes.param_event.drain().filter(|(_, node)| node.active).collect();

    // Construct the new schedule object.
    let new_schedule = ProcessorSchedule::new(
        tasks,
        shared_pool.transports.transport.clone(),
        shared_pool.buffers.audio_buffer_pool.buffer_size(),
    );

    // Verify that the schedule is sound (no race conditions).
    //
    // This is probably expensive, but I would like to keep this check here until we are very
    // confident in the stability and soundness of this audio graph compiler.
    //
    // We are using reference-counted pointers (`basedrop::Shared`) for everything, so we shouldn't
    // ever run into a situation where the schedule assigns a pointer to a buffer or a node that
    // doesn't exist in memory.
    //
    // However, it is still very possible to have race condition bugs in the schedule, such as
    // the same buffer being assigned multiple times within the same task, or the same buffer
    // appearing multiple times between parallel tasks (once we have multithreaded scheduling).
    if let Err(e) = verifier.verify_schedule_for_race_conditions(&new_schedule) {
        return Err(GraphCompilerError::VerifierError(e, abstract_schedule, new_schedule));
    }

    Ok(new_schedule)
}
