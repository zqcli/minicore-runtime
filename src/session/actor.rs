use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{RunnerEvent, RunnerOutcome, RunnerProgress, SuspensionError, TurnRunnerExit};
use crate::config::{KernelConfig, SessionSpec};
use crate::conversation::{ConversationCommitError, ConversationLog, ConversationSeq};
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::{InteractionId, SessionId, SessionInstanceId, TurnId};

use super::command::SessionCommand;
use super::event_stream::{InternalEventSink, SessionEventStream};
use super::handle::SessionHandle;
use super::state::{SessionHealth, SessionState, SessionStatus};
use super::turn_handle::{TurnCompletion, TurnHandle, TurnOutcome};
use crate::bindings::SessionBindings;
use crate::interaction::{InteractionAnswer, PendingInteraction};

mod commands;
mod lifecycle;
mod run;
mod runner;
mod settlement;
mod supervisor;

pub(super) use lifecycle::RunnerLifecycle;
pub(in crate::session) use supervisor::run_session_actor;

pub(super) enum SessionActorExit {
    Closed,
    CloseFailed(ConversationCommitError),
    DurabilityFailed {
        close_error: Option<Box<ConversationCommitError>>,
    },
    OpenFailed,
    Panicked,
    PanicCloseFailed(ConversationCommitError),
}

pub(super) struct ActorBuildFailure {
    pub(super) log: ConversationLog,
}

pub(super) struct ActorReady {
    pub(super) handle: SessionHandle,
    pub(super) events: SessionEventStream,
    pub(super) runner_lifecycle: RunnerLifecycle,
}

pub(crate) struct SessionActor {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    kernel: KernelConfig,
    spec: SessionSpec,
    bindings: SessionBindings,
    conversation: ConversationLog,
    commands: mpsc::Receiver<SessionCommand>,
    state_tx: watch::Sender<SessionState>,
    events: InternalEventSink,
    root_cancel: CancellationToken,
    runner_lifecycle: RunnerLifecycle,
    active: Option<ActiveTurn>,
    health: SessionHealth,
    closing: bool,
    closing_durability_failure: Option<DiagnosticSummary>,
    last_resolved_interaction: Option<InteractionId>,
}

struct ActiveTurn {
    turn_id: TurnId,
    cancellation: CancellationToken,
    completion: TurnCompletion,
    critical: mpsc::Receiver<RunnerEvent>,
    progress: mpsc::Receiver<RunnerProgress>,
    runner: Option<JoinHandle<TurnRunnerExit>>,
    critical_open: bool,
    progress_open: bool,
    outcome: Option<RunnerOutcome>,
    pending: Option<PendingInteractionState>,
    commit_failure: Option<ActiveCommitFailure>,
}

struct ActiveCommitFailure {
    diagnostic: DiagnosticSummary,
    unknown: bool,
}

struct PendingInteractionState {
    public: PendingInteraction,
    resume: oneshot::Sender<Result<InteractionAnswer, SuspensionError>>,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(pending) = self.pending.take() {
            let _ = pending.resume.send(Err(SuspensionError::RuntimeClosed));
        }
        if let Some(runner) = self.runner.take() {
            runner.abort();
        }
        if let Some(failure) = self.commit_failure.take() {
            if failure.unknown {
                self.completion.durability_unknown(failure.diagnostic);
            } else {
                self.completion.durability_unavailable(failure.diagnostic);
            }
        } else {
            self.completion.runtime_terminated(SessionActor::diagnostic(
                DiagnosticCode::RuntimeTerminated,
                DiagnosticCategory::Internal,
                "session actor dropped an active turn",
                false,
            ));
        }
    }
}

impl SessionActor {
    pub(super) fn new(
        conversation: ConversationLog,
        kernel: KernelConfig,
        bindings: SessionBindings,
        spec: SessionSpec,
        session_id: SessionId,
        instance_id: SessionInstanceId,
        root_cancel: CancellationToken,
    ) -> Result<(Self, ActorReady), Box<ActorBuildFailure>> {
        let health = SessionHealth::Healthy;
        let state = initial_state(
            session_id,
            instance_id,
            conversation.head(),
            conversation.last_terminal().map(|entry| TurnOutcome {
                turn_id: entry.turn_id,
                terminal: entry.terminal,
                usage: entry.usage,
            }),
            health.clone(),
        );
        if state.validate().is_err() {
            return Err(Box::new(ActorBuildFailure { log: conversation }));
        }
        let (state_tx, state_rx) = watch::channel(state);
        let (events, event_stream) =
            match InternalEventSink::channel(session_id, instance_id, kernel.event_capacity) {
                Ok(channels) => channels,
                Err(_) => return Err(Box::new(ActorBuildFailure { log: conversation })),
            };
        let (command_tx, commands) = mpsc::channel(kernel.command_capacity);
        let handle = SessionHandle::new(session_id, instance_id, command_tx, state_rx);
        let runner_lifecycle = RunnerLifecycle::new();
        Ok((
            Self {
                session_id,
                instance_id,
                kernel,
                spec,
                bindings,
                conversation,
                commands,
                state_tx,
                events,
                root_cancel,
                runner_lifecycle: runner_lifecycle.clone(),
                active: None,
                health,
                closing: false,
                closing_durability_failure: None,
                last_resolved_interaction: None,
            },
            ActorReady {
                handle,
                events: event_stream,
                runner_lifecycle,
            },
        ))
    }

    fn state(&self) -> SessionState {
        self.state_tx.borrow().clone()
    }

    fn publish_state(&self, state: SessionState) {
        debug_assert!(state.validate().is_ok());
        self.state_tx.send_replace(state);
    }

    fn active_turn_id(&self) -> Option<TurnId> {
        self.active.as_ref().map(|active| active.turn_id)
    }

    fn diagnostic(
        code: DiagnosticCode,
        category: DiagnosticCategory,
        message: &'static str,
        retryable: bool,
    ) -> DiagnosticSummary {
        DiagnosticSummary::bounded_static(code, category, message, retryable)
    }
}

fn initial_state(
    session_id: SessionId,
    instance_id: SessionInstanceId,
    conversation_seq: ConversationSeq,
    last_terminal: Option<TurnOutcome>,
    health: SessionHealth,
) -> SessionState {
    SessionState {
        session_id,
        instance_id,
        status: SessionStatus::Idle,
        health,
        active_turn: None,
        pending_interaction: None,
        conversation_seq,
        last_terminal,
    }
}

#[cfg(test)]
pub(crate) mod tests;
