use serde::{Deserialize, Serialize};

use crate::event::SessionEventKind;
use crate::ids::TurnId;
use crate::tools::{LegacyToolCallSummary, LegacyToolResultSummary, LegacyUserQuestion};

use super::snapshot::{SessionSnapshot, TurnOutcome};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum SessionEvent {
    Snapshot(SessionSnapshot),
    TurnStarted {
        turn_id: TurnId,
    },
    TextDelta {
        turn_id: TurnId,
        delta: String,
    },
    ReasoningDelta {
        turn_id: TurnId,
        delta: String,
    },
    ToolStarted {
        turn_id: TurnId,
        call: LegacyToolCallSummary,
    },
    ToolFinished {
        turn_id: TurnId,
        result: LegacyToolResultSummary,
    },
    InputRequested {
        turn_id: TurnId,
        question: LegacyUserQuestion,
    },
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    ResyncRequired,
    Closed,
}

impl SessionEvent {
    pub(crate) const fn kind(&self) -> SessionEventKind {
        match self {
            Self::Snapshot(_) => SessionEventKind::Snapshot,
            Self::TurnStarted { .. } => SessionEventKind::TurnStarted,
            Self::TextDelta { .. } => SessionEventKind::TextDelta,
            Self::ReasoningDelta { .. } => SessionEventKind::ReasoningDelta,
            Self::ToolStarted { .. } => SessionEventKind::ToolStarted,
            Self::ToolFinished { .. } => SessionEventKind::ToolFinished,
            Self::InputRequested { .. } => SessionEventKind::InputRequested,
            Self::TurnFinished { .. } => SessionEventKind::TurnFinished,
            Self::ResyncRequired => SessionEventKind::ResyncRequired,
            Self::Closed => SessionEventKind::Closed,
        }
    }
}

// P6 deletion target: remove with the legacy session event surface.
const _: for<'a> fn(&'a SessionEvent) -> SessionEventKind = SessionEvent::kind;
