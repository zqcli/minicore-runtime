use std::fmt;

use thiserror::Error;
use tokio::sync::oneshot;

use crate::conversation::{
    AssistantMessageDraft, ConversationSeq, ConversationView, SummaryDraft, ToolResultDraft,
};
use crate::error::DiagnosticSummary;
use crate::ids::{ToolCallId, TurnId};
use crate::interaction::{InteractionAnswer, InteractionKind};
use crate::model::{ModelDriverProgress, Usage};
use crate::tools::{ToolName, ToolProgress as ToolProgressValue, ToolResultOutcome};

pub(crate) struct TurnSuspension {
    pub(crate) turn_id: TurnId,
    pub(crate) tool_call_id: ToolCallId,
    pub(crate) tool_name: ToolName,
    pub(crate) kind: InteractionKind,
    pub(crate) resume: oneshot::Sender<Result<InteractionAnswer, SuspensionError>>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitAck {
    pub(crate) head: ConversationSeq,
    pub(crate) conversation: ConversationView,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum RunnerCommitError {
    #[error("runner commit was stale")]
    Stale,
    #[error("runner commit was rejected because the session is degraded")]
    Degraded,
    #[error("runner commit durability is unavailable")]
    DurabilityUnavailable,
    #[error("runner commit durability is unknown")]
    DurabilityUnknown,
    #[error("runner commit runtime is closed")]
    RuntimeClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunnerOutcome {
    Completed {
        usage: Usage,
    },
    Failed {
        diagnostic: DiagnosticSummary,
        usage: Usage,
    },
    Cancelled {
        usage: Usage,
    },
    BudgetExceeded {
        usage: Usage,
    },
}

impl RunnerOutcome {
    pub(crate) const fn usage(&self) -> Usage {
        match self {
            Self::Completed { usage }
            | Self::Failed { usage, .. }
            | Self::Cancelled { usage }
            | Self::BudgetExceeded { usage } => *usage,
        }
    }

    pub(crate) const fn diagnostic(&self) -> Option<&DiagnosticSummary> {
        match self {
            Self::Failed { diagnostic, .. } => Some(diagnostic),
            _ => None,
        }
    }
}

pub(crate) enum RunnerEvent {
    CommitAssistant {
        draft: AssistantMessageDraft,
        reply: oneshot::Sender<Result<CommitAck, RunnerCommitError>>,
    },
    CommitToolResult {
        draft: ToolResultDraft,
        reply: oneshot::Sender<Result<CommitAck, RunnerCommitError>>,
    },
    CommitSummary {
        snapshot_head: ConversationSeq,
        draft: SummaryDraft,
        reply: oneshot::Sender<Result<CommitAck, RunnerCommitError>>,
    },
    Suspend {
        suspension: TurnSuspension,
    },
    Finish {
        outcome: RunnerOutcome,
    },
}

impl fmt::Debug for RunnerEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommitAssistant { draft, .. } => formatter
                .debug_struct("CommitAssistant")
                .field("draft", draft)
                .finish_non_exhaustive(),
            Self::CommitToolResult { draft, .. } => formatter
                .debug_struct("CommitToolResult")
                .field("draft", draft)
                .finish_non_exhaustive(),
            Self::CommitSummary {
                snapshot_head,
                draft,
                ..
            } => formatter
                .debug_struct("CommitSummary")
                .field("snapshot_head", snapshot_head)
                .field("through", &draft.through)
                .field("summary_bytes", &draft.summary.byte_len())
                .finish_non_exhaustive(),
            Self::Suspend { suspension } => formatter
                .debug_struct("Suspend")
                .field("suspension", suspension)
                .finish(),
            Self::Finish { outcome } => formatter
                .debug_struct("Finish")
                .field("outcome", outcome)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunnerProgress {
    ModelStarted {
        model_round: u16,
    },
    ModelProgress {
        model_round: u16,
        progress: ModelDriverProgress,
    },
    ModelFinished {
        model_round: u16,
        usage: Usage,
    },
    ToolStarted {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
    },
    ToolProgress {
        tool_call_id: ToolCallId,
        progress: ToolProgressValue,
    },
    ToolFinished {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        outcome: ToolResultOutcome,
        content_bytes: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TurnRunnerExit {
    Finished { outcome: RunnerOutcome },
    ProtocolClosed { outcome: RunnerOutcome },
    Panicked,
}
