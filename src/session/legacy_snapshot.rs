// P4-B/P5 deletion target: remove after final SessionState actor wiring.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::PublicErrorSummary;
use crate::ids::{SessionId, TurnId};
use crate::model::Usage;
use crate::tools::LegacyUserQuestion;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum LegacySnapshotShapeError {
    #[error("idle session snapshot cannot have an active turn")]
    IdleHasActiveTurn,
    #[error("idle session snapshot cannot have a pending question")]
    IdleHasPendingQuestion,
    #[error("running session snapshot must have an active turn")]
    RunningMissingActiveTurn,
    #[error("active turn does not match the session status")]
    ActiveTurnMismatch,
    #[error("running session snapshot cannot have a pending question")]
    RunningHasPendingQuestion,
    #[error("waiting session snapshot must have an active turn")]
    WaitingMissingActiveTurn,
    #[error("waiting session snapshot must have a pending question")]
    WaitingMissingQuestion,
    #[error("waiting question does not match the session interaction")]
    WaitingQuestionMismatch,
    #[error("closing session snapshot cannot have a pending question")]
    ClosingHasPendingQuestion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LegacyTurnSummary {
    pub turn_id: TurnId,
}

impl LegacyTurnSummary {
    pub const fn new(turn_id: TurnId) -> Self {
        Self { turn_id }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub(crate) enum LegacyTurnOutcome {
    Completed,
    Cancelled,
    Failed { error: PublicErrorSummary },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LegacyTurnTerminal {
    pub turn_id: TurnId,
    pub outcome: LegacyTurnOutcome,
}

impl LegacyTurnTerminal {
    pub const fn new(turn_id: TurnId, outcome: LegacyTurnOutcome) -> Self {
        Self { turn_id, outcome }
    }

    pub const fn completed(turn_id: TurnId) -> Self {
        Self::new(turn_id, LegacyTurnOutcome::Completed)
    }

    pub const fn cancelled(turn_id: TurnId) -> Self {
        Self::new(turn_id, LegacyTurnOutcome::Cancelled)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LegacySnapshotHistory {
    pub last_error: Option<PublicErrorSummary>,
    pub last_terminal: Option<LegacyTurnTerminal>,
}

impl LegacySnapshotHistory {
    pub const fn new(
        last_error: Option<PublicErrorSummary>,
        last_terminal: Option<LegacyTurnTerminal>,
    ) -> Self {
        Self {
            last_error,
            last_terminal,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacySessionSnapshot {
    session_id: SessionId,
    status: super::legacy_state::LegacySessionStatus,
    active_turn: Option<LegacyTurnSummary>,
    pending_question: Option<LegacyUserQuestion>,
    usage: Usage,
    last_error: Option<PublicErrorSummary>,
    last_terminal: Option<LegacyTurnTerminal>,
    conversation_seq: u64,
}

#[derive(Deserialize)]
struct LegacySessionSnapshotWire {
    session_id: SessionId,
    status: super::legacy_state::LegacySessionStatus,
    active_turn: Option<LegacyTurnSummary>,
    pending_question: Option<LegacyUserQuestion>,
    usage: Usage,
    last_error: Option<PublicErrorSummary>,
    last_terminal: Option<LegacyTurnTerminal>,
    conversation_seq: u64,
}

impl LegacySessionSnapshot {
    pub fn new(
        session_id: SessionId,
        status: super::legacy_state::LegacySessionStatus,
        active_turn: Option<LegacyTurnSummary>,
        pending_question: Option<LegacyUserQuestion>,
        usage: Usage,
        history: LegacySnapshotHistory,
        conversation_seq: u64,
    ) -> Result<Self, LegacySnapshotShapeError> {
        let snapshot = Self {
            session_id,
            status,
            active_turn,
            pending_question,
            usage,
            last_error: history.last_error,
            last_terminal: history.last_terminal,
            conversation_seq,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), LegacySnapshotShapeError> {
        match self.status {
            super::legacy_state::LegacySessionStatus::Idle => {
                if self.active_turn.is_some() {
                    return Err(LegacySnapshotShapeError::IdleHasActiveTurn);
                }
                if self.pending_question.is_some() {
                    return Err(LegacySnapshotShapeError::IdleHasPendingQuestion);
                }
            }
            super::legacy_state::LegacySessionStatus::Running { turn_id } => {
                let Some(active_turn) = self.active_turn else {
                    return Err(LegacySnapshotShapeError::RunningMissingActiveTurn);
                };
                if active_turn.turn_id != turn_id {
                    return Err(LegacySnapshotShapeError::ActiveTurnMismatch);
                }
                if self.pending_question.is_some() {
                    return Err(LegacySnapshotShapeError::RunningHasPendingQuestion);
                }
            }
            super::legacy_state::LegacySessionStatus::WaitingForInput {
                turn_id,
                interaction_id,
            } => {
                let Some(active_turn) = self.active_turn else {
                    return Err(LegacySnapshotShapeError::WaitingMissingActiveTurn);
                };
                if active_turn.turn_id != turn_id {
                    return Err(LegacySnapshotShapeError::ActiveTurnMismatch);
                }
                let Some(question) = self.pending_question.as_ref() else {
                    return Err(LegacySnapshotShapeError::WaitingMissingQuestion);
                };
                if question.interaction_id() != interaction_id {
                    return Err(LegacySnapshotShapeError::WaitingQuestionMismatch);
                }
            }
            super::legacy_state::LegacySessionStatus::Closing => {
                if self.pending_question.is_some() {
                    return Err(LegacySnapshotShapeError::ClosingHasPendingQuestion);
                }
            }
        }
        Ok(())
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn status(&self) -> super::legacy_state::LegacySessionStatus {
        self.status
    }

    pub const fn active_turn(&self) -> Option<&LegacyTurnSummary> {
        self.active_turn.as_ref()
    }

    pub(crate) const fn pending_question(&self) -> Option<&LegacyUserQuestion> {
        self.pending_question.as_ref()
    }

    pub const fn last_error(&self) -> Option<&PublicErrorSummary> {
        self.last_error.as_ref()
    }

    pub const fn last_terminal(&self) -> Option<&LegacyTurnTerminal> {
        self.last_terminal.as_ref()
    }

    pub const fn conversation_seq(&self) -> u64 {
        self.conversation_seq
    }
}

#[cfg(test)]
impl LegacySessionSnapshot {
    pub(crate) const fn usage(&self) -> &Usage {
        &self.usage
    }
}

impl<'de> Deserialize<'de> for LegacySessionSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = LegacySessionSnapshotWire::deserialize(deserializer)?;
        Self::new(
            value.session_id,
            value.status,
            value.active_turn,
            value.pending_question,
            value.usage,
            LegacySnapshotHistory::new(value.last_error, value.last_terminal),
            value.conversation_seq,
        )
        .map_err(serde::de::Error::custom)
    }
}

const _: () = {
    // P6 deletion target: remove with the legacy session snapshot surface.
    let _: fn(TurnId) -> LegacyTurnTerminal = LegacyTurnTerminal::completed;
    let _: fn(&LegacySessionSnapshot) -> SessionId = LegacySessionSnapshot::session_id;
    let _: for<'a> fn(&'a LegacySessionSnapshot) -> Option<&'a LegacyTurnSummary> =
        LegacySessionSnapshot::active_turn;
    let _: for<'a> fn(&'a LegacySessionSnapshot) -> Option<&'a LegacyUserQuestion> =
        LegacySessionSnapshot::pending_question;
    let _: for<'a> fn(&'a LegacySessionSnapshot) -> Option<&'a PublicErrorSummary> =
        LegacySessionSnapshot::last_error;
    let _: for<'a> fn(&'a LegacySessionSnapshot) -> Option<&'a LegacyTurnTerminal> =
        LegacySessionSnapshot::last_terminal;
    let _: fn(&LegacySessionSnapshot) -> u64 = LegacySessionSnapshot::conversation_seq;
};
