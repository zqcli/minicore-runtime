use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::SessionEnvironment;
use crate::conversation::ConversationView;
use crate::ids::{SessionId, SessionInstanceId, TurnId};

use super::runner_protocol::{RunnerEvent, RunnerProgress};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnRunnerIdentity {
    pub(crate) session_id: SessionId,
    pub(crate) instance_id: SessionInstanceId,
    pub(crate) turn_id: TurnId,
}

pub(crate) struct TurnRunnerControl {
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Instant,
    pub(crate) critical_tx: mpsc::Sender<RunnerEvent>,
    pub(crate) progress_tx: mpsc::Sender<RunnerProgress>,
}

pub(crate) struct TurnRunnerRequest {
    pub(crate) session_id: SessionId,
    pub(crate) instance_id: SessionInstanceId,
    pub(crate) turn_id: TurnId,
    pub(crate) environment: Arc<SessionEnvironment>,
    pub(crate) effective_max_tool_rounds: u16,
    pub(crate) conversation: ConversationView,
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Instant,
    pub(crate) critical_tx: mpsc::Sender<RunnerEvent>,
    pub(crate) progress_tx: mpsc::Sender<RunnerProgress>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum TurnRunnerRequestError {
    #[error("turn runner configuration is invalid")]
    Configuration,
    #[error("turn runner conversation is invalid")]
    Conversation,
}

impl TurnRunnerRequest {
    pub(crate) fn new(
        identity: TurnRunnerIdentity,
        environment: Arc<SessionEnvironment>,
        effective_max_tool_rounds: u16,
        conversation: ConversationView,
        control: TurnRunnerControl,
    ) -> Result<Self, TurnRunnerRequestError> {
        if !(1..=environment.spec.max_tool_rounds).contains(&effective_max_tool_rounds)
            || effective_max_tool_rounds > environment.limits.max_tool_rounds
        {
            return Err(TurnRunnerRequestError::Configuration);
        }
        let active = conversation
            .validated_active_turn(&environment.spec, &environment.limits)
            .map_err(|_| TurnRunnerRequestError::Conversation)?;
        if active.turn_id != Some(identity.turn_id)
            || active
                .execution
                .as_ref()
                .is_none_or(|execution| execution.max_tool_rounds != effective_max_tool_rounds)
        {
            return Err(TurnRunnerRequestError::Conversation);
        }
        Ok(Self {
            session_id: identity.session_id,
            instance_id: identity.instance_id,
            turn_id: identity.turn_id,
            environment,
            effective_max_tool_rounds,
            conversation,
            cancellation: control.cancellation,
            deadline: control.deadline,
            critical_tx: control.critical_tx,
            progress_tx: control.progress_tx,
        })
    }
}

pub(super) struct TurnRunnerContext {
    pub(super) session_id: SessionId,
    pub(super) instance_id: SessionInstanceId,
    pub(super) turn_id: TurnId,
    pub(super) effective_max_tool_rounds: u16,
    pub(super) conversation: ConversationView,
    pub(super) cancellation: CancellationToken,
    pub(super) deadline: Instant,
    pub(super) critical_tx: mpsc::Sender<RunnerEvent>,
    pub(super) progress_tx: mpsc::Sender<RunnerProgress>,
    pub(super) environment: Arc<SessionEnvironment>,
}

impl TurnRunnerContext {
    pub(super) fn from_request(request: TurnRunnerRequest) -> Self {
        Self {
            session_id: request.session_id,
            instance_id: request.instance_id,
            turn_id: request.turn_id,
            effective_max_tool_rounds: request.effective_max_tool_rounds,
            conversation: request.conversation,
            cancellation: request.cancellation,
            deadline: request.deadline,
            critical_tx: request.critical_tx,
            progress_tx: request.progress_tx,
            environment: request.environment,
        }
    }

    pub(super) fn validate_conversation(
        &self,
        conversation: &ConversationView,
    ) -> Result<(), TurnRunnerRequestError> {
        let active = conversation
            .validated_active_turn(&self.environment.spec, &self.environment.limits)
            .map_err(|_| TurnRunnerRequestError::Conversation)?;
        if active.turn_id != Some(self.turn_id)
            || active
                .execution
                .as_ref()
                .is_none_or(|execution| execution.max_tool_rounds != self.effective_max_tool_rounds)
        {
            return Err(TurnRunnerRequestError::Conversation);
        }
        Ok(())
    }
}

#[cfg(test)]
impl TurnRunnerContext {
    pub(super) fn set_compaction_trigger_tokens(&mut self, trigger_tokens: u64) {
        let environment =
            Arc::get_mut(&mut self.environment).expect("test turn environment must have one owner");
        if let Some(compaction) = environment.compaction.as_mut() {
            compaction.trigger_tokens = trigger_tokens;
        }
    }
}

impl fmt::Debug for TurnRunnerRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnRunnerRequest")
            .field("session_id", &self.session_id)
            .field("instance_id", &self.instance_id)
            .field("turn_id", &self.turn_id)
            .field("model", &self.environment.spec.model)
            .field("effective_max_tool_rounds", &self.effective_max_tool_rounds)
            .field("conversation", &self.conversation)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}
