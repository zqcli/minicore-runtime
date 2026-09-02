use crate::execution::ConfigRevision;
use crate::ids::LoopId;
use crate::interaction::PendingInteraction;
use crate::model::ModelRef;

/// Point-in-time public state of one agent loop.
///
/// The runner writes the authoritative state to a watch channel; handles only
/// project it. There is no separate private state copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopState {
    pub loop_id: LoopId,
    pub status: LoopStatus,

    pub request_index: u32,
    pub config_revision: ConfigRevision,

    pub model: Option<ModelRef>,
    pub pending_interaction: Option<PendingInteraction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopStatus {
    Starting,
    RunningModel,
    RunningTools,
    WaitingForInput,
    Finishing,
    Finished,
}

impl LoopState {
    pub(crate) const fn new(
        loop_id: LoopId,
        status: LoopStatus,
        request_index: u32,
        config_revision: ConfigRevision,
    ) -> Self {
        Self {
            loop_id,
            status,
            request_index,
            config_revision,
            model: None,
            pending_interaction: None,
        }
    }
}
