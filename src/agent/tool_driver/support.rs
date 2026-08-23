use tokio::sync::mpsc;

use crate::ids::ToolCallId;
use crate::tools::{
    ToolDecision, ToolExecutionOutcome, ToolProgress, ToolProgressEmitter, ToolProgressSink,
};

use super::{ToolDriverProgress, ToolDriverResult};

pub(super) enum PolicyResolution {
    Decision(ToolDecision),
    Denied,
    Cancelled,
    DeadlineExceeded,
}

pub(super) enum ExecutionResolution {
    Outcome(ToolExecutionOutcome),
    Failed,
    Cancelled,
    DeadlineExceeded,
}

impl ToolDriverProgress {
    pub(crate) const fn tool_call_id(&self) -> &ToolCallId {
        match self {
            Self::Started { tool_call_id, .. } | Self::Update { tool_call_id, .. } => tool_call_id,
        }
    }

    pub(crate) const fn tool_name(&self) -> Option<&crate::tools::ToolName> {
        match self {
            Self::Started { tool_name, .. } => Some(tool_name),
            Self::Update { .. } => None,
        }
    }

    pub(crate) const fn progress(&self) -> Option<&ToolProgress> {
        match self {
            Self::Update { progress, .. } => Some(progress),
            Self::Started { .. } => None,
        }
    }
}

impl ToolDriverResult {
    pub(crate) const fn output(&self) -> &crate::tools::ToolOutput {
        &self.output
    }

    pub(crate) const fn outcome(&self) -> crate::tools::ToolResultOutcome {
        self.outcome
    }
}

pub(super) fn progress_sink(
    tool_call_id: ToolCallId,
    sender: mpsc::Sender<ToolDriverProgress>,
) -> ToolProgressSink {
    ToolProgressSink::from_emitter(DriverProgressEmitter {
        tool_call_id,
        sender,
    })
}

struct DriverProgressEmitter {
    tool_call_id: ToolCallId,
    sender: mpsc::Sender<ToolDriverProgress>,
}

impl ToolProgressEmitter for DriverProgressEmitter {
    fn emit(&self, progress: ToolProgress) -> bool {
        self.sender
            .try_send(ToolDriverProgress::Update {
                tool_call_id: self.tool_call_id.clone(),
                progress,
            })
            .is_ok()
    }
}
