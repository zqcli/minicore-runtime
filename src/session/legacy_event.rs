// P4-B/P5 deletion target: remove after the actor uses final SessionEvent.

use serde::{Deserialize, Serialize};

use crate::ids::TurnId;
use crate::tools::{LegacyToolCallSummary, LegacyToolResultSummary, LegacyUserQuestion};

use super::legacy_snapshot::{LegacySessionSnapshot, LegacyTurnOutcome};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LegacySessionEventKind {
    Snapshot,
    TurnStarted,
    TextDelta,
    ReasoningDelta,
    ToolStarted,
    ToolFinished,
    InputRequested,
    TurnFinished,
    ResyncRequired,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum LegacySessionEvent {
    Snapshot(LegacySessionSnapshot),
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
        outcome: LegacyTurnOutcome,
    },
    ResyncRequired,
    Closed,
}

impl LegacySessionEvent {
    pub(crate) const fn kind(&self) -> LegacySessionEventKind {
        match self {
            Self::Snapshot(_) => LegacySessionEventKind::Snapshot,
            Self::TurnStarted { .. } => LegacySessionEventKind::TurnStarted,
            Self::TextDelta { .. } => LegacySessionEventKind::TextDelta,
            Self::ReasoningDelta { .. } => LegacySessionEventKind::ReasoningDelta,
            Self::ToolStarted { .. } => LegacySessionEventKind::ToolStarted,
            Self::ToolFinished { .. } => LegacySessionEventKind::ToolFinished,
            Self::InputRequested { .. } => LegacySessionEventKind::InputRequested,
            Self::TurnFinished { .. } => LegacySessionEventKind::TurnFinished,
            Self::ResyncRequired => LegacySessionEventKind::ResyncRequired,
            Self::Closed => LegacySessionEventKind::Closed,
        }
    }
}

// P6 deletion target: remove with the legacy session event surface.
const _: for<'a> fn(&'a LegacySessionEvent) -> LegacySessionEventKind = LegacySessionEvent::kind;
