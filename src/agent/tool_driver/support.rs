use tokio::sync::mpsc;

use crate::ids::ToolCallId;
use crate::tools::{ToolProgress, ToolProgressEmitter, ToolProgressSink};

use super::{ToolDriverProgress, ToolDriverResult};

impl ToolDriverProgress {
    pub(crate) const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub(crate) const fn progress(&self) -> &ToolProgress {
        &self.progress
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
            .try_send(ToolDriverProgress {
                tool_call_id: self.tool_call_id.clone(),
                progress,
            })
            .is_ok()
    }
}
