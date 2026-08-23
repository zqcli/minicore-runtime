use std::fmt;

use thiserror::Error;
use tokio::sync::oneshot;

use crate::ids::{ToolCallId, TurnId};
use crate::session::{InteractionAnswer, InteractionKind};
use crate::tools::ToolName;

pub(crate) struct TurnSuspension {
    pub(crate) turn_id: TurnId,
    pub(crate) tool_call_id: ToolCallId,
    pub(crate) tool_name: ToolName,
    pub(crate) kind: InteractionKind,
    pub(crate) resume: oneshot::Sender<Result<InteractionAnswer, SuspensionError>>,
}

// P4-C actor wiring will replace this compile anchor when it consumes suspensions directly.
pub(crate) fn take_resume_for_actor(
    suspension: TurnSuspension,
) -> oneshot::Sender<Result<InteractionAnswer, SuspensionError>> {
    suspension.resume
}

impl fmt::Debug for TurnSuspension {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnSuspension")
            .field("turn_id", &self.turn_id)
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SuspensionError {
    #[error("turn suspension was cancelled")]
    Cancelled,
    #[error("turn suspension deadline was exceeded")]
    DeadlineExceeded,
    #[error("turn suspension belongs to a stale turn")]
    StaleTurn,
    #[error("turn suspension is invalid for the current state")]
    InvalidState,
    #[error("turn suspension runtime is closed")]
    RuntimeClosed,
}

impl SuspensionError {
    pub(crate) const fn stale_turn() -> Self {
        Self::StaleTurn
    }
}
