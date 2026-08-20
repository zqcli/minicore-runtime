use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::PublicErrorSummary;
use crate::ids::{SessionId, TurnId};
use crate::model::Usage;
use crate::tools::UserQuestion;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SnapshotShapeError {
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
pub struct TurnSummary {
    pub turn_id: TurnId,
}

impl TurnSummary {
    pub const fn new(turn_id: TurnId) -> Self {
        Self { turn_id }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed { error: PublicErrorSummary },
}

pub type TerminalOutcome = TurnOutcome;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TurnTerminal {
    pub turn_id: TurnId,
    pub outcome: TurnOutcome,
}

impl TurnTerminal {
    pub const fn new(turn_id: TurnId, outcome: TurnOutcome) -> Self {
        Self { turn_id, outcome }
    }

    pub const fn completed(turn_id: TurnId) -> Self {
        Self::new(turn_id, TurnOutcome::Completed)
    }

    pub const fn cancelled(turn_id: TurnId) -> Self {
        Self::new(turn_id, TurnOutcome::Cancelled)
    }
}

pub type TurnTerminalSummary = TurnTerminal;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotHistory {
    pub last_error: Option<PublicErrorSummary>,
    pub last_terminal: Option<TurnTerminal>,
}

impl SnapshotHistory {
    pub const fn new(
        last_error: Option<PublicErrorSummary>,
        last_terminal: Option<TurnTerminal>,
    ) -> Self {
        Self {
            last_error,
            last_terminal,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    session_id: SessionId,
    status: super::state::SessionStatus,
    active_turn: Option<TurnSummary>,
    pending_question: Option<UserQuestion>,
    usage: Usage,
    last_error: Option<PublicErrorSummary>,
    last_terminal: Option<TurnTerminal>,
    conversation_seq: u64,
}

#[derive(Deserialize)]
struct SessionSnapshotWire {
    session_id: SessionId,
    status: super::state::SessionStatus,
    active_turn: Option<TurnSummary>,
    pending_question: Option<UserQuestion>,
    usage: Usage,
    last_error: Option<PublicErrorSummary>,
    last_terminal: Option<TurnTerminal>,
    conversation_seq: u64,
}

impl SessionSnapshot {
    pub fn new(
        session_id: SessionId,
        status: super::state::SessionStatus,
        active_turn: Option<TurnSummary>,
        pending_question: Option<UserQuestion>,
        usage: Usage,
        history: SnapshotHistory,
        conversation_seq: u64,
    ) -> Result<Self, SnapshotShapeError> {
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

    pub fn validate(&self) -> Result<(), SnapshotShapeError> {
        match self.status {
            super::state::SessionStatus::Idle => {
                if self.active_turn.is_some() {
                    return Err(SnapshotShapeError::IdleHasActiveTurn);
                }
                if self.pending_question.is_some() {
                    return Err(SnapshotShapeError::IdleHasPendingQuestion);
                }
            }
            super::state::SessionStatus::Running { turn_id } => {
                let Some(active_turn) = self.active_turn else {
                    return Err(SnapshotShapeError::RunningMissingActiveTurn);
                };
                if active_turn.turn_id != turn_id {
                    return Err(SnapshotShapeError::ActiveTurnMismatch);
                }
                if self.pending_question.is_some() {
                    return Err(SnapshotShapeError::RunningHasPendingQuestion);
                }
            }
            super::state::SessionStatus::WaitingForInput {
                turn_id,
                interaction_id,
            } => {
                let Some(active_turn) = self.active_turn else {
                    return Err(SnapshotShapeError::WaitingMissingActiveTurn);
                };
                if active_turn.turn_id != turn_id {
                    return Err(SnapshotShapeError::ActiveTurnMismatch);
                }
                let Some(question) = self.pending_question.as_ref() else {
                    return Err(SnapshotShapeError::WaitingMissingQuestion);
                };
                if question.interaction_id() != interaction_id {
                    return Err(SnapshotShapeError::WaitingQuestionMismatch);
                }
            }
            super::state::SessionStatus::Closing => {
                if self.pending_question.is_some() {
                    return Err(SnapshotShapeError::ClosingHasPendingQuestion);
                }
            }
        }
        Ok(())
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn status(&self) -> super::state::SessionStatus {
        self.status
    }

    pub const fn active_turn(&self) -> Option<&TurnSummary> {
        self.active_turn.as_ref()
    }

    pub const fn pending_question(&self) -> Option<&UserQuestion> {
        self.pending_question.as_ref()
    }

    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    pub const fn last_error(&self) -> Option<&PublicErrorSummary> {
        self.last_error.as_ref()
    }

    pub const fn last_terminal(&self) -> Option<&TurnTerminal> {
        self.last_terminal.as_ref()
    }

    pub const fn conversation_seq(&self) -> u64 {
        self.conversation_seq
    }
}

impl<'de> Deserialize<'de> for SessionSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = SessionSnapshotWire::deserialize(deserializer)?;
        Self::new(
            value.session_id,
            value.status,
            value.active_turn,
            value.pending_question,
            value.usage,
            SnapshotHistory::new(value.last_error, value.last_terminal),
            value.conversation_seq,
        )
        .map_err(serde::de::Error::custom)
    }
}
