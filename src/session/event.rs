use serde::{Deserialize, Serialize};

use crate::event::SessionEventKind;
use crate::ids::TurnId;
use crate::tools::{ToolCallSummary, ToolResultSummary, UserQuestion};

use super::snapshot::{SessionSnapshot, TurnOutcome};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
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
        call: ToolCallSummary,
    },
    ToolFinished {
        turn_id: TurnId,
        result: ToolResultSummary,
    },
    InputRequested {
        turn_id: TurnId,
        question: UserQuestion,
    },
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    ResyncRequired,
    Closed,
}

impl SessionEvent {
    pub const fn kind(&self) -> SessionEventKind {
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
