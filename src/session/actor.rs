use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    RunnerEvent, RunnerOutcome, RunnerProgress, SessionEnvironment, SuspensionError, TurnRunnerExit,
};
use crate::conversation::{ConversationCommitError, ConversationLog, ConversationSeq};
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::{InteractionId, SessionId, SessionInstanceId, TurnId};

use super::command::SessionCommand;
use super::event_stream::{InternalEventSink, SessionEventStream};
use super::handle::SessionHandle;
use super::state::{SessionHealth, SessionState, SessionStatus};
use super::turn_handle::{TurnCompletion, TurnHandle, TurnOutcome};
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
    environment: Arc<SessionEnvironment>,
    conversation: ConversationLog,
    commands: mpsc::Receiver<SessionCommand>,
    state_tx: watch::Sender<SessionState>,
    events: InternalEventSink,
    root_cancel: CancellationToken,
    runner_lifecycle: RunnerLifecycle,
    core: ActorCoreState,
}

struct ActorCoreState {
    active: Option<ActiveTurn>,
    health: SessionHealth,
    closing: bool,
    closing_durability_failure: Option<DiagnosticSummary>,
    last_terminal: Option<TurnOutcome>,
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
        environment: Arc<SessionEnvironment>,
        session_id: SessionId,
        instance_id: SessionInstanceId,
        root_cancel: CancellationToken,
    ) -> Result<(Self, ActorReady), Box<ActorBuildFailure>> {
        let core = ActorCoreState {
            active: None,
            health: SessionHealth::Healthy,
            closing: false,
            closing_durability_failure: None,
            last_terminal: conversation.last_terminal().map(|entry| TurnOutcome {
                turn_id: entry.turn_id,
                terminal: entry.terminal,
                usage: entry.usage,
            }),
            last_resolved_interaction: None,
        };
        let state = derive_state(session_id, instance_id, conversation.head(), &core);
        if state.validate().is_err() {
            return Err(Box::new(ActorBuildFailure { log: conversation }));
        }
        let (state_tx, state_rx) = watch::channel(state);
        let (_, _, channels) = environment.session_inputs();
        let (events, event_stream) =
            match InternalEventSink::channel(session_id, instance_id, channels.event) {
                Ok(channels) => channels,
                Err(_) => return Err(Box::new(ActorBuildFailure { log: conversation })),
            };
        let (command_tx, commands) = mpsc::channel(channels.command);
        let handle = SessionHandle::new(session_id, instance_id, command_tx, state_rx);
        let runner_lifecycle = RunnerLifecycle::new();
        Ok((
            Self {
                session_id,
                instance_id,
                environment,
                conversation,
                commands,
                state_tx,
                events,
                root_cancel,
                runner_lifecycle: runner_lifecycle.clone(),
                core,
            },
            ActorReady {
                handle,
                events: event_stream,
                runner_lifecycle,
            },
        ))
    }

    fn derived_state(&self) -> SessionState {
        derive_state(
            self.session_id,
            self.instance_id,
            self.conversation.head(),
            &self.core,
        )
    }

    fn publish_state(&self) {
        let state = self.derived_state();
        debug_assert!(state.validate().is_ok());
        self.state_tx.send_replace(state);
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> SessionState {
        self.derived_state()
    }

    fn install_active(&mut self, active: ActiveTurn) {
        self.core.active = Some(active);
        self.publish_state();
    }

    fn record_terminal(&mut self, outcome: TurnOutcome) {
        self.core.last_terminal = Some(outcome);
        self.publish_state();
    }

    fn active_turn_id(&self) -> Option<TurnId> {
        self.core.active.as_ref().map(|active| active.turn_id)
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

fn derive_state(
    session_id: SessionId,
    instance_id: SessionInstanceId,
    conversation_seq: ConversationSeq,
    core: &ActorCoreState,
) -> SessionState {
    let active_turn = core.active.as_ref().map(|active| active.turn_id);
    let pending_interaction = if core.closing {
        None
    } else {
        core.active
            .as_ref()
            .and_then(|active| active.pending.as_ref())
            .map(|pending| pending.public.clone())
    };
    let status = if core.closing {
        SessionStatus::Closing
    } else if pending_interaction.is_some() {
        SessionStatus::WaitingForInput
    } else if active_turn.is_some() {
        SessionStatus::Running
    } else {
        SessionStatus::Idle
    };
    SessionState {
        session_id,
        instance_id,
        status,
        health: core.health.clone(),
        active_turn,
        pending_interaction,
        conversation_seq,
        last_terminal: core.last_terminal.clone(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
