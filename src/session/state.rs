use thiserror::Error;

use crate::conversation::ConversationSeq;
use crate::error::DiagnosticSummary;
use crate::ids::{SessionId, SessionInstanceId, TurnId};

use super::turn_handle::TurnOutcome;
use crate::interaction::PendingInteraction;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStatus {
    Idle,
    Running,
    WaitingForInput,
    Closing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionHealth {
    Healthy,
    Degraded { diagnostic: DiagnosticSummary },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub status: SessionStatus,
    pub health: SessionHealth,
    pub active_turn: Option<TurnId>,
    pub pending_interaction: Option<PendingInteraction>,
    pub conversation_seq: ConversationSeq,
    pub last_terminal: Option<TurnOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SessionStateError {
    #[error("idle session state has an active turn")]
    IdleHasActiveTurn,
    #[error("idle session state has a pending interaction")]
    IdleHasPendingInteraction,
    #[error("running session state has no active turn")]
    RunningMissingActiveTurn,
    #[error("running session state has a pending interaction")]
    RunningHasPendingInteraction,
    #[error("waiting session state has no active turn")]
    WaitingMissingActiveTurn,
    #[error("waiting session state has no pending interaction")]
    WaitingMissingInteraction,
    #[error("waiting interaction does not match the active turn")]
    WaitingTurnMismatch,
    #[error("closing session state has a pending interaction")]
    ClosingHasPendingInteraction,
    #[error("active turn is already recorded as terminal")]
    ActiveTurnAlreadyTerminal,
}

impl SessionState {
    pub fn validate(&self) -> Result<(), SessionStateError> {
        match self.status {
            SessionStatus::Idle => {
                if self.active_turn.is_some() {
                    return Err(SessionStateError::IdleHasActiveTurn);
                }
                if self.pending_interaction.is_some() {
                    return Err(SessionStateError::IdleHasPendingInteraction);
                }
            }
            SessionStatus::Running => {
                if self.active_turn.is_none() {
                    return Err(SessionStateError::RunningMissingActiveTurn);
                }
                if self.pending_interaction.is_some() {
                    return Err(SessionStateError::RunningHasPendingInteraction);
                }
            }
            SessionStatus::WaitingForInput => {
                let Some(active_turn) = self.active_turn else {
                    return Err(SessionStateError::WaitingMissingActiveTurn);
                };
                let Some(interaction) = self.pending_interaction.as_ref() else {
                    return Err(SessionStateError::WaitingMissingInteraction);
                };
                if interaction.turn_id != active_turn {
                    return Err(SessionStateError::WaitingTurnMismatch);
                }
            }
            SessionStatus::Closing => {
                if self.pending_interaction.is_some() {
                    return Err(SessionStateError::ClosingHasPendingInteraction);
                }
            }
        }
        if self.active_turn.is_some_and(|active| {
            self.last_terminal
                .as_ref()
                .is_some_and(|terminal| terminal.turn_id == active)
        }) {
            return Err(SessionStateError::ActiveTurnAlreadyTerminal);
        }
        Ok(())
    }
}
