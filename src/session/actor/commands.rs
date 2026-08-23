use std::time::{Duration, Instant};

use crate::agent::{
    TurnRunnerControl, TurnRunnerIdentity, TurnRunnerKernel, TurnRunnerRequest, run_turn,
};
use crate::config::UserInput;
use crate::conversation::{
    ConversationCommitError, ConversationCommitErrorKind, TurnExecutionRecord, UnsequencedEntry,
    UserInputRecord, UserMessageDraft,
};
use crate::error::SessionError;
use crate::tools::ApprovalDecision;

use super::super::event::{InteractionResolutionSummary, SessionEvent};
use super::*;

const DEFAULT_TURN_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

impl SessionActor {
    pub(super) async fn handle_command(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Submit {
                input,
                options,
                reply,
            } => {
                if reply.is_closed() {
                    return;
                }
                self.handle_submit(input, options, reply).await;
            }
            SessionCommand::Answer {
                interaction_id,
                answer,
                reply,
            } => {
                let result = self.handle_answer(interaction_id, answer);
                let _ = reply.send(result);
            }
            SessionCommand::Transcript {
                after,
                limit,
                reply,
            } => {
                let result = self.handle_transcript(after, limit).await;
                let _ = reply.send(result);
            }
        }
    }

    async fn handle_submit(
        &mut self,
        input: UserInput,
        options: crate::config::TurnOptions,
        reply: oneshot::Sender<Result<TurnHandle, SessionError>>,
    ) {
        if self.closing {
            let _ = reply.send(Err(SessionError::Closed));
            return;
        }
        if let Some(active_turn) = self.active_turn_id() {
            let _ = reply.send(Err(SessionError::Busy { active_turn }));
            return;
        }
        if let SessionHealth::Degraded { diagnostic } = &self.health {
            let _ = reply.send(Err(SessionError::Degraded(diagnostic.clone())));
            return;
        }
        let effective_rounds = match self.validate_submit(&input, &options) {
            Ok(value) => value,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let turn_id = match TurnId::new() {
            Ok(turn_id) => turn_id,
            Err(_) => {
                let _ = reply.send(Err(SessionError::InvalidInput(invalid_input())));
                return;
            }
        };
        let cancellation = CancellationToken::new();
        let UserInput::Text(text) = input;
        let input = match UserInputRecord::new(text) {
            Ok(input) => input,
            Err(_) => {
                let _ = reply.send(Err(SessionError::InvalidInput(invalid_input())));
                return;
            }
        };
        let execution = match TurnExecutionRecord::new(
            self.spec.model.clone(),
            self.spec.reasoning,
            effective_rounds,
        ) {
            Ok(execution) => execution,
            Err(_) => {
                let _ = reply.send(Err(SessionError::InvalidInput(invalid_input())));
                return;
            }
        };
        if let Err(error) = self
            .conversation
            .append_validated(vec![UnsequencedEntry::UserMessage(UserMessageDraft {
                turn_id,
                input,
                execution,
            })])
            .await
        {
            let _ = reply.send(Err(self.submit_commit_error(error)));
            return;
        }

        let (handle, completion) = TurnHandle::new(
            self.session_id,
            self.instance_id,
            turn_id,
            cancellation.clone(),
        );
        let mut state = self.state();
        state.status = SessionStatus::Running;
        state.active_turn = Some(turn_id);
        state.pending_interaction = None;
        state.conversation_seq = self.conversation.head();
        self.publish_state(state);
        if reply.send(Ok(handle)).is_err() {
            cancellation.cancel();
        }
        let _ = self.events.try_emit(SessionEvent::TurnStarted { turn_id });

        let (critical_tx, critical) = mpsc::channel(self.kernel.runner_capacity);
        let (progress_tx, progress) = mpsc::channel(self.kernel.runner_capacity);
        let deadline = options.deadline.unwrap_or_else(|| {
            Instant::now()
                .checked_add(DEFAULT_TURN_DEADLINE)
                .unwrap_or_else(Instant::now)
        });
        let request = TurnRunnerKernel::from_kernel(&self.kernel).and_then(|kernel| {
            TurnRunnerRequest::new(
                TurnRunnerIdentity {
                    session_id: self.session_id,
                    instance_id: self.instance_id,
                    turn_id,
                },
                self.spec.clone(),
                effective_rounds,
                self.bindings.clone(),
                self.conversation.view(),
                kernel,
                TurnRunnerControl {
                    cancellation: cancellation.clone(),
                    deadline,
                    critical_tx,
                    progress_tx,
                },
            )
        });
        let mut active = ActiveTurn {
            turn_id,
            cancellation,
            completion,
            critical,
            progress,
            runner: None,
            critical_open: true,
            progress_open: true,
            outcome: None,
            pending: None,
            commit_failure: None,
        };
        match request {
            Ok(request) => {
                let guard = self.runner_lifecycle.start();
                let generation = guard.generation();
                let runner = tokio::spawn(async move {
                    let _guard = guard;
                    run_turn(request).await
                });
                self.runner_lifecycle
                    .install_abort(generation, runner.abort_handle());
                active.runner = Some(runner);
            }
            Err(_) => {
                active.outcome = Some(RunnerOutcome::Failed {
                    diagnostic: Self::diagnostic(
                        DiagnosticCode::Internal,
                        DiagnosticCategory::Internal,
                        "turn runner request construction failed",
                        false,
                    ),
                    usage: crate::model::Usage::default(),
                });
            }
        }
        self.active = Some(active);
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.runner.is_none())
        {
            self.settle_active(None).await;
        }
    }

    fn validate_submit(
        &self,
        input: &UserInput,
        options: &crate::config::TurnOptions,
    ) -> Result<u16, SessionError> {
        if input.validate(&self.kernel.limits).is_err()
            || options.validate(&self.kernel.limits).is_err()
            || self
                .bindings
                .validate(&self.spec, &self.kernel.limits)
                .is_err()
            || options
                .deadline
                .is_some_and(|deadline| deadline <= Instant::now())
        {
            return Err(SessionError::InvalidInput(invalid_input()));
        }
        let rounds = options.max_tool_rounds.unwrap_or(self.spec.max_tool_rounds);
        if rounds > self.spec.max_tool_rounds {
            return Err(SessionError::InvalidInput(invalid_input()));
        }
        Ok(rounds)
    }

    pub(super) fn handle_answer(
        &mut self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), SessionError> {
        if self.closing {
            return Err(SessionError::Closed);
        }
        if self.last_resolved_interaction == Some(interaction_id) {
            return Err(SessionError::InteractionAlreadyResolved);
        }
        let (turn_id, pending) = {
            let active = self
                .active
                .as_mut()
                .ok_or(SessionError::InteractionNotFound)?;
            let pending = active
                .pending
                .as_ref()
                .ok_or(SessionError::InteractionNotFound)?;
            if pending.public.interaction_id != interaction_id {
                return Err(SessionError::InteractionNotFound);
            }
            if !answer_matches(&answer, &pending.public.kind) {
                return Err(SessionError::InteractionKindMismatch);
            }
            pending
                .public
                .validate_answer(&answer)
                .map_err(|_| SessionError::InvalidInput(invalid_input()))?;
            let pending = active
                .pending
                .take()
                .ok_or(SessionError::InteractionNotFound)?;
            (active.turn_id, pending)
        };
        let resolution = resolution(&answer);
        let mut state = self.state();
        state.status = SessionStatus::Running;
        state.pending_interaction = None;
        self.publish_state(state);
        self.last_resolved_interaction = Some(interaction_id);
        if pending.resume.send(Ok(answer)).is_err() {
            let usage = self.conversation.confirmed_turn_usage(turn_id);
            if let Some(active) = self.active.as_mut() {
                if active.outcome.is_none() {
                    active.outcome = Some(RunnerOutcome::Failed {
                        diagnostic: Self::diagnostic(
                            DiagnosticCode::Internal,
                            DiagnosticCategory::Internal,
                            "interaction answer receiver closed",
                            false,
                        ),
                        usage,
                    });
                }
                active.cancellation.cancel();
            }
            return Err(SessionError::Closed);
        }
        let _ = self.events.try_emit(SessionEvent::InteractionResolved {
            interaction_id,
            resolution,
        });
        Ok(())
    }

    async fn handle_transcript(
        &mut self,
        after: Option<crate::conversation::ConversationSeq>,
        limit: usize,
    ) -> Result<crate::conversation::TranscriptPage, SessionError> {
        if self.closing {
            return Err(SessionError::Closed);
        }
        match self.conversation.transcript(after, limit).await {
            Ok(page) => Ok(page),
            Err(error) => {
                let diagnostic = transcript_error(error.clone());
                if error.kind() == ConversationCommitErrorKind::DurabilityUnknown {
                    self.mark_degraded(diagnostic.clone());
                }
                Err(SessionError::TranscriptUnavailable(diagnostic))
            }
        }
    }

    fn submit_commit_error(&mut self, error: ConversationCommitError) -> SessionError {
        match error.kind() {
            ConversationCommitErrorKind::Closed => SessionError::Closed,
            ConversationCommitErrorKind::Validation
            | ConversationCommitErrorKind::InvalidConfiguration
            | ConversationCommitErrorKind::InvalidManifest => {
                SessionError::InvalidInput(invalid_input())
            }
            _ => {
                let diagnostic = commit_diagnostic(&error);
                self.mark_degraded(diagnostic.clone());
                SessionError::Degraded(diagnostic)
            }
        }
    }
}

fn answer_matches(answer: &InteractionAnswer, kind: &crate::interaction::InteractionKind) -> bool {
    matches!(
        (answer, kind),
        (
            InteractionAnswer::Approval(_),
            crate::interaction::InteractionKind::Approval(_)
        ) | (
            InteractionAnswer::ToolInput(_),
            crate::interaction::InteractionKind::ToolInput(_)
        )
    )
}

fn resolution(answer: &InteractionAnswer) -> InteractionResolutionSummary {
    match answer {
        InteractionAnswer::Approval(ApprovalDecision::AllowOnce) => {
            InteractionResolutionSummary::Approved
        }
        InteractionAnswer::Approval(ApprovalDecision::Deny) => InteractionResolutionSummary::Denied,
        InteractionAnswer::ToolInput(_) => InteractionResolutionSummary::InputProvided,
    }
}

fn invalid_input() -> DiagnosticSummary {
    SessionActor::diagnostic(
        DiagnosticCode::InvalidConfiguration,
        DiagnosticCategory::Configuration,
        "session command input is invalid",
        false,
    )
}

fn transcript_error(error: ConversationCommitError) -> DiagnosticSummary {
    let retryable = matches!(
        error.kind(),
        ConversationCommitErrorKind::Log(crate::error::SessionLogErrorKind::Unavailable)
    );
    SessionActor::diagnostic(
        DiagnosticCode::Internal,
        DiagnosticCategory::Storage,
        "session transcript is unavailable",
        retryable,
    )
}

pub(super) fn commit_diagnostic(error: &ConversationCommitError) -> DiagnosticSummary {
    let code = match error.kind() {
        ConversationCommitErrorKind::DurabilityUnknown => DiagnosticCode::LogUnknownOutcome,
        ConversationCommitErrorKind::Log(crate::error::SessionLogErrorKind::Conflict) => {
            DiagnosticCode::LogConflict
        }
        _ => DiagnosticCode::Internal,
    };
    SessionActor::diagnostic(
        code,
        DiagnosticCategory::Storage,
        "session conversation commit failed",
        false,
    )
}
