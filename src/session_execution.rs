#![allow(
    dead_code,
    reason = "the loaded Session executor awaits public routing and Turn integration"
)]

//! The crate-private loaded Session execution seam.
//!
//! This module deliberately stops at one already-loaded, Ready+Idle Session. It retains the
//! replay-seeded live state and inline Recorder supplied by residency, but owns neither Runtime
//! residency nor the public Runtime facade. The Runtime-owned residency registry that starts an
//! executor retains its permit (and excludes lifecycle/load changes) for as long as the loaded
//! executor is live; this constructor does not acquire that permit.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent_session_lifecycle::{
    SealedSessionDefinitionAttempt, SessionDefinition, SessionDefinitionDecision,
    SessionDefinitionDecisionError, SessionLifecycle,
};
use crate::conversation_storage::{ConversationReplayDiagnostics, SessionRecorder};
use crate::durable_state::{
    DurableSessionDefinitionError, DurableSessionDefinitionOutcome, DurableState,
};
use crate::live_conversation::LiveSessionState;
use crate::prompt::{PromptError, PromptErrorKind, PromptService};
use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedTask};
use crate::wire::{SessionDefinitionRevision, SessionId, Timestamp, WorkspaceRevision};
use crate::workspace::{
    Workspace, WorkspaceResolveError, WorkspaceResolver, WorkspaceSnapshot,
    WorkspaceSnapshotFinishError,
};

const SESSION_EXECUTOR_REQUEST_QUEUE_CAPACITY: usize = 8;

/// The only execution states represented by a loaded Session executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}

impl SessionExecutionState {
    const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// A small immutable, coherent loaded Session read model.
///
/// It intentionally exposes no live conversation, turn, model, tool, recorder, or event state.
#[derive(Clone)]
pub(crate) struct SessionExecutorSnapshot {
    definition: Arc<SessionDefinition>,
    workspace: Arc<WorkspaceSnapshot>,
    execution_state: SessionExecutionState,
}

impl SessionExecutorSnapshot {
    fn new(
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        execution_state: SessionExecutionState,
    ) -> Self {
        Self {
            definition,
            workspace,
            execution_state,
        }
    }

    pub(crate) fn definition(&self) -> &Arc<SessionDefinition> {
        &self.definition
    }

    pub(crate) fn workspace(&self) -> &Arc<WorkspaceSnapshot> {
        &self.workspace
    }

    pub(crate) fn workspace_revision(&self) -> WorkspaceRevision {
        self.workspace.revision()
    }

    pub(crate) fn definition_revision(&self) -> SessionDefinitionRevision {
        self.definition.revision()
    }

    pub(crate) const fn execution_state(&self) -> SessionExecutionState {
        self.execution_state
    }
}

impl fmt::Debug for SessionExecutorSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionExecutorSnapshot")
            .field("session_definition_revision", &self.definition.revision())
            .field("workspace_revision", &self.workspace.revision())
            .field("execution_state", &self.execution_state)
            .finish()
    }
}

/// The closed result of one Workspace definition CAS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionWorkspaceDefinitionOutcome {
    NoChange {
        definition_revision: SessionDefinitionRevision,
        workspace_revision: WorkspaceRevision,
    },
    Updated {
        definition_revision: SessionDefinitionRevision,
        workspace_revision: WorkspaceRevision,
    },
}

impl SessionWorkspaceDefinitionOutcome {
    pub(crate) const fn definition_revision(self) -> SessionDefinitionRevision {
        match self {
            Self::NoChange {
                definition_revision,
                ..
            }
            | Self::Updated {
                definition_revision,
                ..
            } => definition_revision,
        }
    }

    pub(crate) const fn workspace_revision(self) -> WorkspaceRevision {
        match self {
            Self::NoChange {
                workspace_revision, ..
            }
            | Self::Updated {
                workspace_revision, ..
            } => workspace_revision,
        }
    }

    pub(crate) const fn changed(self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

/// Redacted failures for the loaded Workspace definition interface.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionWorkspaceDefinitionError {
    #[error("session executor is closing")]
    Closing,
    #[error("session execution is busy")]
    SessionBusy,
    #[error("Session was not found")]
    SessionNotFound,
    #[error("Session definition compare-and-swap is stale")]
    StaleRevision,
    #[error("Session is archived")]
    SessionArchived,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session definition revision is exhausted")]
    RevisionExhausted,
    #[error("durable state exceeds its selected size limit")]
    StateTooLarge,
    #[error("workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("workspace candidate was rejected")]
    WorkspaceRejected,
    #[error("durable storage is unavailable")]
    StorageUnavailable,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures for the immutable snapshot request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionExecutorSnapshotError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failure from joining one loaded Session executor during Unload/shutdown.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionExecutorCloseError {
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from executor construction.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionExecutorStartError {
    #[error("loaded Session and Workspace identities do not match")]
    SessionIdMismatch,
    #[error("loaded Session and Workspace revisions do not match")]
    WorkspaceRevisionMismatch,
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// A typed process-local exclusion for one definition publication.
///
/// The identity is intentionally opaque.  Only the actor and the owner-retained completion can
/// compare it, so a completion from a different publication cannot install a snapshot.
#[derive(Clone)]
pub(crate) struct SessionDefinitionPublicationPermit {
    identity: Arc<PublicationPermitIdentity>,
}

struct PublicationPermitIdentity;

impl SessionDefinitionPublicationPermit {
    fn new() -> Self {
        Self {
            identity: Arc::new(PublicationPermitIdentity),
        }
    }

    fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl fmt::Debug for SessionDefinitionPublicationPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionDefinitionPublicationPermit { .. }")
    }
}

#[derive(Clone)]
pub(crate) struct LoadedSessionConversation {
    live_state: Arc<Mutex<LiveSessionState>>,
    recorder: Arc<SessionRecorder>,
    replay_diagnostics: ConversationReplayDiagnostics,
}

impl LoadedSessionConversation {
    pub(crate) fn from_replay(
        live_state: LiveSessionState,
        recorder: SessionRecorder,
        replay_diagnostics: ConversationReplayDiagnostics,
    ) -> Self {
        Self {
            live_state: Arc::new(Mutex::new(live_state)),
            recorder: Arc::new(recorder),
            replay_diagnostics,
        }
    }
}

/// The loaded Session control actor handle.
#[derive(Clone)]
pub(crate) struct SessionExecutor {
    sender: mpsc::Sender<SessionExecutorRequest>,
    closing: CancellationToken,
    task: TrackedTask,
    failure_state: Arc<ActorFailureState>,
    conversation: Option<Arc<LoadedSessionConversation>>,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

impl fmt::Debug for SessionExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionExecutor { .. }")
    }
}

impl SessionExecutor {
    /// Starts one loaded Ready+Idle Session.
    ///
    /// The caller must already own the future Runtime residency permit and must exclude
    /// lifecycle/load changes for the exact loaded Session. This constructor validates the
    /// definition/snapshot binding and adopts an already replay-seeded state/Recorder pair; it
    /// deliberately does not acquire that future permit.
    pub(crate) fn start_loaded_ready_idle(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: LoadedSessionConversation,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_ready_idle_inner(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            definition,
            workspace,
            Some(Arc::new(conversation)),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_loaded_ready_idle_without_conversation(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_ready_idle_inner(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            definition,
            workspace,
            None,
        )
    }

    fn start_loaded_ready_idle_inner(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: Option<Arc<LoadedSessionConversation>>,
    ) -> Result<Self, SessionExecutorStartError> {
        if definition.session_id() != workspace.session_id() {
            return Err(SessionExecutorStartError::SessionIdMismatch);
        }
        if definition.workspace().revision() != workspace.revision() {
            return Err(SessionExecutorStartError::WorkspaceRevisionMismatch);
        }

        let current = Arc::new(SessionExecutorSnapshot::new(
            Arc::clone(&definition),
            workspace,
            SessionExecutionState::Idle,
        ));
        let (sender, receiver) = mpsc::channel(SESSION_EXECUTOR_REQUEST_QUEUE_CAPACITY);
        let (completion_sender, completion_receiver) = mpsc::unbounded_channel();
        let closing = CancellationToken::new();
        #[cfg(test)]
        let hooks = Arc::new(SessionExecutorTestHooksInner::new());
        let failure_state = Arc::new(ActorFailureState::default());
        let actor = SessionExecutorActor {
            receiver,
            completions: completion_receiver,
            completion_sender,
            closing: closing.clone(),
            task_context: task_context.clone(),
            durable_state: durable_state.clone(),
            resolver,
            prompt_service,
            current,
            execution_state: SessionExecutionState::Idle,
            active_publication: None,
            failure_state: Arc::clone(&failure_state),
            conversation: conversation.clone(),
            #[cfg(test)]
            hooks: Arc::clone(&hooks),
        };
        let mut exit_guard = ActorExitGuard::new(
            closing.clone(),
            task_context.clone(),
            durable_state.clone(),
            Arc::clone(&failure_state),
        );
        let task = match task_context.spawn_tracked(async move {
            let normal_exit = actor.run().await;
            if normal_exit {
                exit_guard.disarm();
            }
        }) {
            Ok(task) => task,
            Err(RuntimeTaskError::OwnerClosing) => {
                // The guard has no admitted waiter here, but it still closes the durable owner as
                // required for an actor that could not be installed.
                task_context.request_closing();
                durable_state.request_closing();
                return Err(SessionExecutorStartError::Closing);
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                task_context.request_closing();
                durable_state.request_closing();
                return Err(SessionExecutorStartError::InternalDispatchUnavailable);
            }
        };

        Ok(Self {
            sender,
            closing,
            task,
            failure_state,
            conversation,
            #[cfg(test)]
            hooks,
        })
    }

    /// Requests the actor to reject future requests.  An admitted publication may abandon
    /// cancellable candidate capture, but work that has reached durable publication still drains.
    pub(crate) fn request_closing(&self) {
        self.closing.cancel();
    }

    /// Closes this executor, drains accepted requests, waits the admitted publication, and waits
    /// for the owner-tracked actor settlement.  It never shuts down the shared RuntimeTaskContext.
    pub(crate) async fn close(&self) -> Result<(), SessionExecutorCloseError> {
        self.request_closing();
        let task_result = self.task.wait().await;
        if let Some(conversation) = &self.conversation {
            conversation.recorder.close().await;
        }
        if task_result.is_err() || self.failure_state.is_fatal() {
            Err(SessionExecutorCloseError::InternalDispatchUnavailable)
        } else {
            Ok(())
        }
    }

    /// Returns the last coherent immutable loaded snapshot.  Requests sent while a publication
    /// is in flight observe the old snapshot until the actor installs the new one.
    pub(crate) async fn snapshot(
        &self,
    ) -> Result<Arc<SessionExecutorSnapshot>, SessionExecutorSnapshotError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Snapshot(SnapshotRequest {
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionExecutorSnapshotError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionExecutorSnapshotError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionExecutorSnapshotError::Closing)
            } else {
                Err(SessionExecutorSnapshotError::InternalDispatchUnavailable)
            }
        })
    }

    /// Publishes a complete lowered Workspace replacement through the loaded Session actor.
    pub(crate) async fn update_workspace_definition(
        &self,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError> {
        self.update_workspace_definition_with_cancellation(
            expected_revision,
            workspace,
            owner_timestamp,
            CancellationToken::new(),
        )
        .await
    }

    pub(crate) async fn update_workspace_definition_with_cancellation(
        &self,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
        candidate_cancellation: CancellationToken,
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Update(WorkspaceDefinitionRequest {
            expected_revision,
            workspace,
            owner_timestamp,
            candidate_cancellation,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable));
                }
            },
        };
        // Reserving the bounded sender is the admission point.  No cancellable await occurs
        // between it and handing ownership of the request to the actor.
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionWorkspaceDefinitionError::Closing)
            } else {
                Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn test_hooks(&self) -> SessionExecutorTestHooks {
        SessionExecutorTestHooks {
            inner: Arc::clone(&self.hooks),
        }
    }

    #[cfg(test)]
    pub(crate) fn live_state_for_test(&self) -> Option<Arc<Mutex<LiveSessionState>>> {
        self.conversation
            .as_ref()
            .map(|conversation| Arc::clone(&conversation.live_state))
    }

    #[cfg(test)]
    pub(crate) fn recorder_for_test(&self) -> Option<Arc<SessionRecorder>> {
        self.conversation
            .as_ref()
            .map(|conversation| Arc::clone(&conversation.recorder))
    }

    #[cfg(test)]
    pub(crate) fn replay_diagnostics_for_test(&self) -> Option<ConversationReplayDiagnostics> {
        self.conversation
            .as_ref()
            .map(|conversation| conversation.replay_diagnostics.clone())
    }

    #[cfg(test)]
    pub(crate) async fn starting_admission_probe_for_test(
        &self,
    ) -> Result<(), SessionWorkspaceDefinitionError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::StartingProbe(StartingProbeRequest {
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionWorkspaceDefinitionError::Closing)
            } else {
                Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
            }
        })
    }
}

struct SessionExecutorActor {
    receiver: mpsc::Receiver<SessionExecutorRequest>,
    completions: mpsc::UnboundedReceiver<PublicationCompletion>,
    completion_sender: mpsc::UnboundedSender<PublicationCompletion>,
    closing: CancellationToken,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    current: Arc<SessionExecutorSnapshot>,
    execution_state: SessionExecutionState,
    active_publication: Option<ActivePublication>,
    failure_state: Arc<ActorFailureState>,
    conversation: Option<Arc<LoadedSessionConversation>>,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

struct ActivePublication {
    permit: SessionDefinitionPublicationPermit,
    expected: ExpectedPublication,
    waiter: Arc<PublicationWaiterState>,
    worker_task: Option<TrackedTask>,
}

#[derive(Clone)]
struct WorkspacePublicationContext {
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    executor_closing: CancellationToken,
    candidate_cancellation: CancellationToken,
}

impl WorkspacePublicationContext {
    fn is_cancelled(&self) -> bool {
        self.executor_closing.is_cancelled() || self.candidate_cancellation.is_cancelled()
    }

    async fn cancelled(&self) {
        tokio::select! {
            _ = self.executor_closing.cancelled() => {}
            _ = self.candidate_cancellation.cancelled() => {}
        }
    }
}

#[derive(Clone)]
enum ExpectedPublication {
    NoChange { definition: Arc<SessionDefinition> },
    Publish { definition: Arc<SessionDefinition> },
}

impl ExpectedPublication {
    fn definition(&self) -> &Arc<SessionDefinition> {
        match self {
            Self::NoChange { definition } | Self::Publish { definition } => definition,
        }
    }

    const fn is_publish(&self) -> bool {
        matches!(self, Self::Publish { .. })
    }
}

#[derive(Clone, Copy)]
enum ActorFatality {
    Integrity,
    Internal,
}

impl SessionExecutorActor {
    async fn run(mut self) -> bool {
        loop {
            if self.closing.is_cancelled() {
                return self.close_and_drain().await;
            }
            tokio::select! {
                biased;
                _ = self.closing.cancelled() => {
                    return self.close_and_drain().await;
                }
                completion = self.completions.recv() => match completion {
                    Some(completion) => {
                        if let Err(fatality) = self.handle_completion(completion).await {
                            self.close_for_fatal(fatality);
                            return self.close_and_drain().await;
                        }
                    }
                    None => {
                        self.reap_after_missing_completion().await;
                        return self.close_and_drain().await;
                    }
                },
                request = self.receiver.recv() => match request {
                    Some(mut request) => {
                        if self.closing.is_cancelled() {
                            request.reject_closing();
                            continue;
                        }
                        if let Err(fatality) = self.handle_request(&mut request) {
                            self.close_for_fatal(fatality);
                            return self.close_and_drain().await;
                        }
                    }
                    None => return self.close_and_drain().await,
                },
            }
        }
    }

    async fn close_and_drain(&mut self) -> bool {
        self.receiver.close();
        self.execution_state = SessionExecutionState::Finishing;
        self.install_current_state(SessionExecutionState::Finishing);
        let mut requests_drained = false;
        let mut normal_exit = true;

        loop {
            if !requests_drained {
                loop {
                    match self.receiver.try_recv() {
                        Ok(mut request) => request.reject_closing(),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            requests_drained = true;
                            break;
                        }
                    }
                }
            }

            if self.active_publication.is_none() && requests_drained {
                if let Some(conversation) = &self.conversation {
                    conversation.recorder.close().await;
                }
                return normal_exit;
            }

            tokio::select! {
                biased;
                completion = self.completions.recv(), if self.active_publication.is_some() => match completion {
                    Some(completion) => {
                        if let Err(fatality) = self.handle_completion(completion).await {
                            normal_exit = false;
                            self.close_for_fatal(fatality);
                        }
                    }
                    None => {
                        normal_exit = false;
                        self.reap_after_missing_completion().await;
                    }
                },
                request = self.receiver.recv(), if !requests_drained => match request {
                    Some(mut request) => request.reject_closing(),
                    None => requests_drained = true,
                },
            }
        }
    }

    async fn reap_after_missing_completion(&mut self) {
        let Some(mut active) = self.active_publication.take() else {
            return;
        };
        if let Some(worker_task) = active.worker_task.take() {
            let _ = worker_task.wait().await;
        }
        self.closing.cancel();
        self.failure_state.mark_fatal();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        active.waiter.settle(Err(
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
        ));
        self.finish_active_waiter(&active.waiter);
    }

    fn handle_request(
        &mut self,
        request: &mut SessionExecutorRequest,
    ) -> Result<(), ActorFatality> {
        match request {
            SessionExecutorRequest::Snapshot(request) => {
                request.settle(Ok(Arc::clone(&self.current)));
            }
            #[cfg(test)]
            SessionExecutorRequest::StartingProbe(request) => {
                if self.active_publication.is_some() || !self.execution_state.is_idle() {
                    request.settle(Err(SessionWorkspaceDefinitionError::SessionBusy));
                } else {
                    request.settle(Ok(()));
                }
            }
            SessionExecutorRequest::Update(request) => {
                self.start_publication(request)?;
            }
        }
        Ok(())
    }

    fn start_publication(
        &mut self,
        request: &mut WorkspaceDefinitionRequest,
    ) -> Result<(), ActorFatality> {
        if self.active_publication.is_some() || !self.execution_state.is_idle() {
            request.settle(Err(SessionWorkspaceDefinitionError::SessionBusy));
            return Ok(());
        }
        if self.current.definition().revision() != request.expected_revision {
            request.settle(Err(SessionWorkspaceDefinitionError::StaleRevision));
            return Ok(());
        }
        if self.task_context.is_closing() {
            request.settle(Err(SessionWorkspaceDefinitionError::Closing));
            return Ok(());
        }
        if request.candidate_cancellation.is_cancelled() {
            request.settle(Err(SessionWorkspaceDefinitionError::Closing));
            return Ok(());
        }

        let attempt = SealedSessionDefinitionAttempt::new(
            self.current.definition().session_id(),
            request.expected_revision,
            Some(request.workspace.clone()),
            None,
            None,
            request.owner_timestamp,
        );
        // This is deliberately before publication admission and before any resolver call.  In
        // particular, stale wins over canonical no-op.
        let expected = match attempt.decide(SessionLifecycle::Open, self.current.definition()) {
            Ok(SessionDefinitionDecision::NoChange) => ExpectedPublication::NoChange {
                definition: Arc::clone(self.current.definition()),
            },
            Ok(SessionDefinitionDecision::Publish(definition)) => ExpectedPublication::Publish {
                definition: Arc::new(definition),
            },
            Err(SessionDefinitionDecisionError::StaleRevision) => {
                request.settle(Err(SessionWorkspaceDefinitionError::StaleRevision));
                return Ok(());
            }
            Err(SessionDefinitionDecisionError::SessionArchived) => {
                request.settle(Err(SessionWorkspaceDefinitionError::SessionArchived));
                return Ok(());
            }
            Err(SessionDefinitionDecisionError::SessionDeleted) => {
                request.settle(Err(SessionWorkspaceDefinitionError::SessionDeleted));
                return Ok(());
            }
            Err(SessionDefinitionDecisionError::RevisionExhausted) => {
                request.settle(Err(SessionWorkspaceDefinitionError::RevisionExhausted));
                return Ok(());
            }
        };

        let permit = SessionDefinitionPublicationPermit::new();
        let waiter = Arc::new(PublicationWaiterState::new(
            request
                .response
                .take()
                .expect("an admitted update request owns one waiter"),
        ));
        self.failure_state.install(Arc::clone(&waiter));
        let active = ActivePublication {
            permit: permit.clone(),
            expected: expected.clone(),
            waiter,
            worker_task: None,
        };
        // Install the actor publication state before spawning any asynchronous work.  The second
        // request is therefore Busy even if the owner scheduler immediately runs the worker.
        self.active_publication = Some(active);

        let completion_sender = self.completion_sender.clone();
        let task_context = self.task_context.clone();
        let durable_state = self.durable_state.clone();
        let publication_context = WorkspacePublicationContext {
            durable_state: durable_state.clone(),
            resolver: Arc::clone(&self.resolver),
            prompt_service: Arc::clone(&self.prompt_service),
            executor_closing: self.closing.clone(),
            candidate_cancellation: request.candidate_cancellation.clone(),
        };
        let session_id = self.current.definition().session_id();
        let expected_for_worker = expected.clone();
        #[cfg(test)]
        let hooks_for_worker = Arc::clone(&self.hooks);
        let guard = PublicationCompletionGuard::new(
            completion_sender.clone(),
            permit,
            task_context.clone(),
            durable_state.clone(),
        );
        let worker = async move {
            let mut guard = guard;
            #[cfg(test)]
            let result = run_publication(
                publication_context,
                session_id,
                attempt,
                expected_for_worker,
                hooks_for_worker,
            )
            .await;
            #[cfg(not(test))]
            let result = run_publication(
                publication_context,
                session_id,
                attempt,
                expected_for_worker,
            )
            .await;
            guard.complete(result);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                self.active_publication
                    .as_mut()
                    .expect("the active publication is installed before spawning")
                    .worker_task = Some(task);
            }
            Err(RuntimeTaskError::OwnerClosing) => {
                // A closing owner cannot admit a worker.  The guard's completion is still the
                // single settlement path; its redacted Closing result is mapped by the actor.
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                // The moved RAII guard reports the one InternalDispatchUnavailable completion and
                // closes both owners.  Do not settle here or create a second completion.
            }
        }
        Ok(())
    }

    async fn handle_completion(
        &mut self,
        completion: PublicationCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(mut active) = self.active_publication.take() else {
            self.task_context.request_closing();
            self.durable_state.request_closing();
            return Err(ActorFatality::Internal);
        };

        let PublicationCompletion {
            permit: completion_permit,
            result,
        } = completion;
        let permit_matches = active.permit.same_as(&completion_permit);
        let worker_result = match active.worker_task.take() {
            Some(worker_task) => worker_task.wait().await,
            None => Ok(()),
        };
        if worker_result.is_err() {
            self.active_publication = Some(active);
            self.close_for_fatal(ActorFatality::Internal);
            return Err(ActorFatality::Internal);
        }
        if !permit_matches {
            self.active_publication = Some(active);
            self.close_for_fatal(ActorFatality::Integrity);
            return Err(ActorFatality::Integrity);
        }

        let handling = self.validate_completion(&active.expected, result);
        match handling {
            CompletionHandling::Success(outcome, new_snapshot, new_definition) => {
                if let Some(snapshot) = new_snapshot {
                    let Some(definition) = new_definition else {
                        self.active_publication = Some(active);
                        self.close_for_fatal(ActorFatality::Integrity);
                        return Err(ActorFatality::Integrity);
                    };
                    if self.installation_fault_is_armed() {
                        self.active_publication = Some(active);
                        self.close_for_fatal(ActorFatality::Integrity);
                        return Err(ActorFatality::Integrity);
                    }
                    self.current = Arc::new(SessionExecutorSnapshot::new(
                        definition,
                        snapshot,
                        self.execution_state,
                    ));
                }
                active.waiter.settle(Ok(outcome));
                self.finish_active_waiter(&active.waiter);
                Ok(())
            }
            CompletionHandling::Ordinary(error) => {
                if matches!(
                    error,
                    SessionWorkspaceDefinitionError::InternalDispatchUnavailable
                ) {
                    self.active_publication = Some(active);
                    self.close_for_fatal(ActorFatality::Internal);
                    return Err(ActorFatality::Internal);
                } else if matches!(error, SessionWorkspaceDefinitionError::Closing) {
                    self.closing.cancel();
                }
                active.waiter.settle(Err(error));
                self.finish_active_waiter(&active.waiter);
                Ok(())
            }
            CompletionHandling::Fatal(fatality) => {
                self.active_publication = Some(active);
                self.close_for_fatal(fatality);
                Err(fatality)
            }
        }
    }

    fn validate_completion(
        &self,
        expected: &ExpectedPublication,
        result: PublicationCompletionResult,
    ) -> CompletionHandling {
        match result {
            PublicationCompletionResult::Error(error) => {
                if matches!(
                    error,
                    SessionWorkspaceDefinitionError::InternalDispatchUnavailable
                ) {
                    CompletionHandling::Fatal(ActorFatality::Internal)
                } else {
                    CompletionHandling::Ordinary(error)
                }
            }
            PublicationCompletionResult::Durable { outcome, snapshot } => {
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return CompletionHandling::Ordinary(map_durable_definition_error(error));
                    }
                };
                match (expected, outcome) {
                    (
                        ExpectedPublication::NoChange { definition },
                        DurableSessionDefinitionOutcome::NoChange(head, returned),
                    ) => {
                        if snapshot.is_some()
                            || !valid_durable_definition_shape(
                                head.as_ref(),
                                returned.as_ref(),
                                definition.as_ref(),
                            )
                            || returned.as_ref() != definition.as_ref()
                        {
                            CompletionHandling::Fatal(ActorFatality::Integrity)
                        } else {
                            CompletionHandling::Success(
                                SessionWorkspaceDefinitionOutcome::NoChange {
                                    definition_revision: returned.revision(),
                                    workspace_revision: returned.workspace().revision(),
                                },
                                None,
                                None,
                            )
                        }
                    }
                    (
                        ExpectedPublication::Publish { definition },
                        DurableSessionDefinitionOutcome::Updated(head, returned),
                    ) => {
                        let Some(snapshot) = snapshot else {
                            return CompletionHandling::Fatal(ActorFatality::Integrity);
                        };
                        if !valid_durable_definition_shape(
                            head.as_ref(),
                            returned.as_ref(),
                            definition.as_ref(),
                        ) || returned.as_ref() != definition.as_ref()
                            || snapshot.session_id() != definition.session_id()
                            || snapshot.revision() != definition.workspace().revision()
                        {
                            CompletionHandling::Fatal(ActorFatality::Integrity)
                        } else {
                            CompletionHandling::Success(
                                SessionWorkspaceDefinitionOutcome::Updated {
                                    definition_revision: returned.revision(),
                                    workspace_revision: returned.workspace().revision(),
                                },
                                Some(snapshot),
                                Some(returned),
                            )
                        }
                    }
                    // A durable outcome with the wrong changed/no-op shape is an integrity
                    // failure.  It may already have crossed the Store commit point.
                    _ => CompletionHandling::Fatal(ActorFatality::Integrity),
                }
            }
        }
    }

    fn installation_fault_is_armed(&self) -> bool {
        #[cfg(test)]
        {
            self.hooks
                .fail_next_install_after_commit
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn finish_active_waiter(&self, waiter: &Arc<PublicationWaiterState>) {
        self.failure_state.clear(waiter);
        #[cfg(test)]
        self.hooks.settled.notify();
    }

    fn close_for_fatal(&mut self, _fatality: ActorFatality) {
        self.closing.cancel();
        self.failure_state.mark_fatal();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        if let Some(active) = self.active_publication.take() {
            active.waiter.settle(Err(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            ));
            self.finish_active_waiter(&active.waiter);
        }
    }

    fn install_current_state(&mut self, execution_state: SessionExecutionState) {
        self.current = Arc::new(SessionExecutorSnapshot::new(
            Arc::clone(self.current.definition()),
            Arc::clone(self.current.workspace()),
            execution_state,
        ));
    }
}

fn valid_durable_definition_shape(
    head: &crate::durable_state::DurableSessionHead,
    returned: &SessionDefinition,
    expected: &SessionDefinition,
) -> bool {
    head.session_id() == expected.session_id()
        && returned.session_id() == expected.session_id()
        && head.current_definition_revision() == expected.revision()
        && returned.revision() == expected.revision()
        && returned.workspace().revision() == expected.workspace().revision()
}

struct PublicationCompletion {
    permit: SessionDefinitionPublicationPermit,
    result: PublicationCompletionResult,
}

#[allow(clippy::large_enum_variant)]
enum PublicationCompletionResult {
    Durable {
        outcome: Result<DurableSessionDefinitionOutcome, DurableSessionDefinitionError>,
        snapshot: Option<Arc<WorkspaceSnapshot>>,
    },
    Error(SessionWorkspaceDefinitionError),
}

enum CompletionHandling {
    Success(
        SessionWorkspaceDefinitionOutcome,
        Option<Arc<WorkspaceSnapshot>>,
        Option<Arc<SessionDefinition>>,
    ),
    Ordinary(SessionWorkspaceDefinitionError),
    Fatal(ActorFatality),
}

async fn run_publication(
    context: WorkspacePublicationContext,
    session_id: SessionId,
    attempt: SealedSessionDefinitionAttempt,
    expected: ExpectedPublication,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> PublicationCompletionResult {
    if !expected.is_publish() {
        return PublicationCompletionResult::Durable {
            outcome: context
                .durable_state
                .update_session_definition(attempt)
                .await,
            snapshot: None,
        };
    }

    let candidate = match context
        .resolver
        .resolve(session_id, expected.definition().workspace())
        .await
    {
        Ok(candidate) => candidate,
        Err(error) => return PublicationCompletionResult::Error(map_workspace_error(error)),
    };
    if candidate.revision() != expected.definition().workspace().revision() {
        return PublicationCompletionResult::Error(
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
        );
    }
    let skill_context = candidate.skill_capture_context();
    if !skill_context.roots().is_empty() {
        return PublicationCompletionResult::Error(
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
        );
    }
    let prompt_context = candidate.prompt_capture_context();
    let requires_revalidation = !prompt_context.roots().is_empty();
    let capture = context
        .prompt_service
        .capture_workspace_sources(prompt_context);
    tokio::pin!(capture);
    let prompt_sources = match tokio::select! {
        biased;
        _ = context.cancelled() => return PublicationCompletionResult::Error(
            SessionWorkspaceDefinitionError::Closing,
        ),
        result = &mut capture => result,
    } {
        Ok(sources) => sources,
        Err(error) => return PublicationCompletionResult::Error(map_prompt_error(error)),
    };
    if requires_revalidation {
        let revalidation_result = {
            let revalidation = context
                .resolver
                .revalidate_candidate(&candidate, expected.definition().workspace());
            tokio::pin!(revalidation);
            tokio::select! {
                biased;
                _ = context.cancelled() => return PublicationCompletionResult::Error(
                    SessionWorkspaceDefinitionError::Closing,
                ),
                result = &mut revalidation => result,
            }
        };
        match revalidation_result {
            Ok(true) => {}
            Ok(false) => {
                return PublicationCompletionResult::Error(
                    SessionWorkspaceDefinitionError::WorkspaceUnavailable,
                );
            }
            Err(error) => return PublicationCompletionResult::Error(map_workspace_error(error)),
        }
    }
    let skill_sources = Arc::from(Vec::new().into_boxed_slice());
    if context.is_cancelled() {
        return PublicationCompletionResult::Error(SessionWorkspaceDefinitionError::Closing);
    }
    let snapshot = match candidate.finish(prompt_sources, skill_sources) {
        Ok(snapshot) => snapshot,
        Err(WorkspaceSnapshotFinishError::AuthorizationMismatch) => {
            return PublicationCompletionResult::Error(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            );
        }
    };

    #[cfg(test)]
    hooks.after_candidate_snapshot_finish_before_durable().await;

    let outcome = context
        .durable_state
        .update_session_definition(attempt)
        .await;
    #[cfg(test)]
    if matches!(&outcome, Ok(DurableSessionDefinitionOutcome::Updated(..))) {
        hooks.after_commit_before_install().await;
    }
    PublicationCompletionResult::Durable {
        outcome,
        snapshot: Some(snapshot),
    }
}

fn map_prompt_error(error: PromptError) -> SessionWorkspaceDefinitionError {
    match error.kind() {
        PromptErrorKind::SourceDiscovery => SessionWorkspaceDefinitionError::WorkspaceUnavailable,
        PromptErrorKind::ContentLoad | PromptErrorKind::DuplicateKey => {
            SessionWorkspaceDefinitionError::WorkspaceRejected
        }
        PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing
        | PromptErrorKind::InvalidIntent
        | PromptErrorKind::InvalidContribution
        | PromptErrorKind::ContextLimitExceeded => {
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable
        }
    }
}

fn map_workspace_error(error: WorkspaceResolveError) -> SessionWorkspaceDefinitionError {
    match error {
        WorkspaceResolveError::Closing => SessionWorkspaceDefinitionError::Closing,
        WorkspaceResolveError::RootUnavailable | WorkspaceResolveError::AuthorityUnavailable => {
            SessionWorkspaceDefinitionError::WorkspaceUnavailable
        }
        WorkspaceResolveError::CanonicalizationFailed => {
            SessionWorkspaceDefinitionError::WorkspaceUnavailable
        }
        WorkspaceResolveError::InternalDispatchUnavailable => {
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable
        }
        WorkspaceResolveError::RootNotDirectory
        | WorkspaceResolveError::DuplicateRoot
        | WorkspaceResolveError::OverlappingRoots
        | WorkspaceResolveError::CwdOutsideRoots
        | WorkspaceResolveError::CwdRootMismatch
        | WorkspaceResolveError::AuthorityDenied => {
            SessionWorkspaceDefinitionError::WorkspaceRejected
        }
    }
}

fn map_durable_definition_error(
    error: DurableSessionDefinitionError,
) -> SessionWorkspaceDefinitionError {
    match error {
        DurableSessionDefinitionError::Closing => SessionWorkspaceDefinitionError::Closing,
        DurableSessionDefinitionError::SessionNotFound => {
            SessionWorkspaceDefinitionError::SessionNotFound
        }
        DurableSessionDefinitionError::StaleRevision => {
            SessionWorkspaceDefinitionError::StaleRevision
        }
        DurableSessionDefinitionError::SessionArchived => {
            SessionWorkspaceDefinitionError::SessionArchived
        }
        DurableSessionDefinitionError::SessionDeleted => {
            SessionWorkspaceDefinitionError::SessionDeleted
        }
        DurableSessionDefinitionError::DurableStateTooLarge => {
            SessionWorkspaceDefinitionError::StateTooLarge
        }
        DurableSessionDefinitionError::StorageUnavailable => {
            SessionWorkspaceDefinitionError::StorageUnavailable
        }
        DurableSessionDefinitionError::InternalDispatchUnavailable => {
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable
        }
    }
}

struct WorkspaceDefinitionRequest {
    expected_revision: SessionDefinitionRevision,
    workspace: Workspace,
    owner_timestamp: Timestamp,
    candidate_cancellation: CancellationToken,
    response: Option<
        oneshot::Sender<Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>>,
    >,
}

impl WorkspaceDefinitionRequest {
    fn settle(
        &mut self,
        outcome: Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionWorkspaceDefinitionError::Closing));
    }
}

impl Drop for WorkspaceDefinitionRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct SnapshotRequest {
    response:
        Option<oneshot::Sender<Result<Arc<SessionExecutorSnapshot>, SessionExecutorSnapshotError>>>,
}

impl SnapshotRequest {
    fn settle(
        &mut self,
        outcome: Result<Arc<SessionExecutorSnapshot>, SessionExecutorSnapshotError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionExecutorSnapshotError::Closing));
    }
}

impl Drop for SnapshotRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

#[cfg(test)]
struct StartingProbeRequest {
    response: Option<oneshot::Sender<Result<(), SessionWorkspaceDefinitionError>>>,
}

#[cfg(test)]
impl StartingProbeRequest {
    fn settle(&mut self, outcome: Result<(), SessionWorkspaceDefinitionError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionWorkspaceDefinitionError::Closing));
    }
}

#[cfg(test)]
impl Drop for StartingProbeRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

enum SessionExecutorRequest {
    Update(WorkspaceDefinitionRequest),
    Snapshot(SnapshotRequest),
    #[cfg(test)]
    StartingProbe(StartingProbeRequest),
}

impl SessionExecutorRequest {
    fn reject_closing(&mut self) {
        match self {
            Self::Update(request) => request.reject_closing(),
            Self::Snapshot(request) => request.reject_closing(),
            #[cfg(test)]
            Self::StartingProbe(request) => request.reject_closing(),
        }
    }
}

#[derive(Default)]
struct ActorFailureState {
    active: Mutex<Option<Arc<PublicationWaiterState>>>,
    fatal: std::sync::atomic::AtomicBool,
}

impl ActorFailureState {
    fn mark_fatal(&self) {
        self.fatal.store(true, std::sync::atomic::Ordering::Release);
    }

    fn is_fatal(&self) -> bool {
        self.fatal.load(std::sync::atomic::Ordering::Acquire)
    }

    fn install(&self, waiter: Arc<PublicationWaiterState>) {
        let mut active = lock(&self.active);
        debug_assert!(
            active.is_none(),
            "one SessionExecutor has one active publication"
        );
        *active = Some(waiter);
    }

    fn clear(&self, waiter: &Arc<PublicationWaiterState>) {
        let mut active = lock(&self.active);
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, waiter))
        {
            *active = None;
        }
    }
}

struct PublicationWaiterState {
    sender: Mutex<
        Option<
            oneshot::Sender<
                Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
            >,
        >,
    >,
}

impl PublicationWaiterState {
    fn new(
        sender: oneshot::Sender<
            Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
        >,
    ) -> Self {
        Self {
            sender: Mutex::new(Some(sender)),
        }
    }

    fn settle(
        &self,
        outcome: Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
    ) {
        let sender = lock(&self.sender).take();
        if let Some(sender) = sender {
            let _ = sender.send(outcome);
        }
    }
}

struct ActorExitGuard {
    closing: CancellationToken,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    failure_state: Arc<ActorFailureState>,
    armed: bool,
}

impl ActorExitGuard {
    fn new(
        closing: CancellationToken,
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        failure_state: Arc<ActorFailureState>,
    ) -> Self {
        Self {
            closing,
            task_context,
            durable_state,
            failure_state,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActorExitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.closing.cancel();
        self.failure_state.mark_fatal();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        let waiter = lock(&self.failure_state.active).take();
        if let Some(waiter) = waiter {
            waiter.settle(Err(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            ));
        }
    }
}

struct PublicationCompletionGuard {
    completion_sender: mpsc::UnboundedSender<PublicationCompletion>,
    permit: Option<SessionDefinitionPublicationPermit>,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    settled: bool,
}

impl PublicationCompletionGuard {
    fn new(
        completion_sender: mpsc::UnboundedSender<PublicationCompletion>,
        permit: SessionDefinitionPublicationPermit,
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
    ) -> Self {
        Self {
            completion_sender,
            permit: Some(permit),
            task_context,
            durable_state,
            settled: false,
        }
    }

    fn complete(&mut self, result: PublicationCompletionResult) {
        let permit = self
            .permit
            .take()
            .expect("a publication completion guard sends exactly once");
        let _ = self
            .completion_sender
            .send(PublicationCompletion { permit, result });
        self.settled = true;
    }
}

impl Drop for PublicationCompletionGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let Some(permit) = self.permit.take() else {
            return;
        };
        let result = if self.task_context.is_closing() {
            PublicationCompletionResult::Error(SessionWorkspaceDefinitionError::Closing)
        } else {
            self.task_context.request_closing();
            self.durable_state.request_closing();
            PublicationCompletionResult::Error(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            )
        };
        let _ = self
            .completion_sender
            .send(PublicationCompletion { permit, result });
        self.settled = true;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct SessionExecutorTestHooks {
    inner: Arc<SessionExecutorTestHooksInner>,
}

#[cfg(test)]
impl SessionExecutorTestHooks {
    pub(crate) fn arm_after_candidate_snapshot_finish_before_durable(&self) {
        self.inner.after_snapshot_finish.arm();
    }

    pub(crate) async fn wait_after_candidate_snapshot_finish_before_durable(&self) {
        self.inner.after_snapshot_finish.wait_until_entered().await;
    }

    pub(crate) fn release_after_candidate_snapshot_finish_before_durable(&self) {
        self.inner.after_snapshot_finish.release();
    }

    pub(crate) fn arm_after_commit_before_install(&self) {
        self.inner.after_commit_before_install.arm();
    }

    pub(crate) async fn wait_after_commit_before_install(&self) {
        self.inner
            .after_commit_before_install
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_after_commit_before_install(&self) {
        self.inner.after_commit_before_install.release();
    }

    pub(crate) async fn wait_for_publication_settlement(&self) {
        self.inner.settled.wait().await;
    }

    pub(crate) fn fail_next_snapshot_install_after_commit(&self) {
        self.inner
            .fail_next_install_after_commit
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
struct SessionExecutorTestHooksInner {
    after_snapshot_finish: Arc<NamedAsyncBarrier>,
    after_commit_before_install: Arc<NamedAsyncBarrier>,
    settled: Arc<SettlementNotification>,
    fail_next_install_after_commit: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl SessionExecutorTestHooksInner {
    fn new() -> Self {
        Self {
            after_snapshot_finish: Arc::new(NamedAsyncBarrier::new()),
            after_commit_before_install: Arc::new(NamedAsyncBarrier::new()),
            settled: Arc::new(SettlementNotification::new()),
            fail_next_install_after_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    async fn after_candidate_snapshot_finish_before_durable(&self) {
        self.after_snapshot_finish.wait_if_armed().await;
    }

    async fn after_commit_before_install(&self) {
        self.after_commit_before_install.wait_if_armed().await;
    }
}

#[cfg(test)]
struct NamedAsyncBarrier {
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::atomic::AtomicBool,
    released: std::sync::atomic::AtomicBool,
    changed: tokio::sync::Notify,
}

#[cfg(test)]
impl NamedAsyncBarrier {
    fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            entered: std::sync::atomic::AtomicBool::new(false),
            released: std::sync::atomic::AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn arm(&self) {
        self.entered
            .store(false, std::sync::atomic::Ordering::Release);
        self.released
            .store(false, std::sync::atomic::Ordering::Release);
        self.armed.store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn wait_if_armed(&self) {
        if self
            .armed
            .compare_exchange(
                true,
                false,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        self.entered
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    async fn wait_until_entered(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.entered.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
struct SettlementNotification {
    count: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
}

#[cfg(test)]
impl SettlementNotification {
    fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn notify(&self) {
        self.count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.changed.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let count = self.count.load(std::sync::atomic::Ordering::Acquire);
            if count != 0
                && self
                    .count
                    .compare_exchange(
                        count,
                        count - 1,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_ok()
            {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::future::{Future, poll_fn};
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::runtime::Handle;

    use crate::durable_state::DurableState;
    use crate::runtime_task::RuntimeTaskContext;
    use crate::wire::{CanonicalFileUri, FileUriFamily, SessionId};
    use crate::workspace::{
        RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspacePathTarget,
        WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy, lower_workspace,
    };

    const AGENT_ID: &str = "agt_11111111111111111111111111111111";
    const SESSION_ID: &str = "ses_22222222222222222222222222222222";
    const G1: &str = "00000000000000000001";
    const G2: &str = "00000000000000000002";

    static NEXT_TEST_ROOT: AtomicUsize = AtomicUsize::new(1);

    async fn poll_once_pending<F>(mut future: Pin<&mut F>) -> bool
    where
        F: Future,
    {
        poll_fn(|context| {
            std::task::Poll::Ready(matches!(
                future.as_mut().poll(context),
                std::task::Poll::Pending
            ))
        })
        .await
    }

    struct TempStore {
        root: PathBuf,
        old_workspace: PathBuf,
        new_workspace: PathBuf,
    }

    impl TempStore {
        fn new() -> Self {
            loop {
                let number = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
                let root = std::env::temp_dir().join(format!(
                    "minicore-session-executor-store-{}-{number}",
                    std::process::id()
                ));
                if root.exists() {
                    continue;
                }
                fs::create_dir(&root).expect("the temporary Store root is created");
                set_private_directory_mode(&root);
                let old_workspace = root.with_file_name(format!(
                    "minicore-session-executor-workspace-old-{}-{number}",
                    std::process::id()
                ));
                let new_workspace = root.with_file_name(format!(
                    "minicore-session-executor-workspace-new-{}-{number}",
                    std::process::id()
                ));
                if old_workspace.exists() || new_workspace.exists() {
                    let _ = fs::remove_dir_all(&root);
                    continue;
                }
                fs::create_dir(&old_workspace).expect("the old Workspace root is created");
                fs::create_dir(old_workspace.join("src")).expect("the old cwd is created");
                fs::create_dir(&new_workspace).expect("the new Workspace root is created");
                fs::create_dir(new_workspace.join("src")).expect("the new cwd is created");
                set_private_directory_mode(&old_workspace);
                set_private_directory_mode(&new_workspace);
                set_private_directory_mode(&old_workspace.join("src"));
                set_private_directory_mode(&new_workspace.join("src"));
                create_marked_store(&root);
                create_fixture_agent(&root);
                create_fixture_session(&root, &old_workspace);
                return Self {
                    root,
                    old_workspace,
                    new_workspace,
                };
            }
        }

        fn session_path(&self) -> PathBuf {
            self.root.join("sessions").join(SESSION_ID)
        }

        fn next_generation_path(&self) -> PathBuf {
            self.session_path().join("generations").join(G2)
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.old_workspace);
            let _ = fs::remove_dir_all(&self.new_workspace);
        }
    }

    struct LoadedFixture {
        context: RuntimeTaskContext,
        state: DurableState,
        executor: SessionExecutor,
        definition: Arc<SessionDefinition>,
    }

    fn set_private_directory_mode(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("the fixture directory receives private mode");
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn set_private_file_mode(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .expect("the fixture file receives private mode");
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn create_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("the fixture file is created");
        set_private_file_mode(path);
    }

    fn create_dir(path: &Path) {
        fs::create_dir(path).expect("the fixture directory is created");
        set_private_directory_mode(path);
    }

    fn create_marked_store(root: &Path) {
        create_file(&root.join(".minicore.lock"), b"");
        create_file(&root.join("MINICORE_STORE_V1"), b"");
        create_dir(&root.join("reservations"));
        create_dir(&root.join("reservations").join("agents"));
        create_dir(&root.join("reservations").join("sessions"));
        create_dir(&root.join("agents"));
        create_dir(&root.join("sessions"));
    }

    fn workspace_uri(path: &Path) -> CanonicalFileUri {
        #[cfg(windows)]
        {
            let path = path.to_string_lossy().replace('\\', "/");
            let path = path.strip_prefix('/').unwrap_or(&path);
            return format!("file:///{path}")
                .parse()
                .expect("temporary Windows URI");
        }
        #[cfg(not(windows))]
        {
            CanonicalFileUri::from_decoded_parts(
                FileUriFamily::Posix,
                None,
                path.to_str().expect("temporary path is UTF-8"),
            )
            .expect("temporary POSIX URI")
        }
    }

    fn replace_fixture(input: &[u8], from: &str, to: &str) -> Vec<u8> {
        let input = std::str::from_utf8(input).expect("fixture bytes are UTF-8");
        assert_eq!(
            input.matches(from).count(),
            1,
            "fixture replacement is unique"
        );
        input.replacen(from, to, 1).into_bytes()
    }

    fn session_definition_fixture(workspace: &Path) -> Vec<u8> {
        let input = include_bytes!("../docs/fixtures/durable-store-v1/session-definition.json");
        replace_fixture(
            input,
            "file:///Users/example/project",
            workspace_uri(workspace).as_str(),
        )
    }

    fn create_fixture_agent(root: &Path) {
        let reservation = root.join("reservations").join("agents").join(AGENT_ID);
        create_file(&reservation, b"");
        let entity = root.join("agents").join(AGENT_ID);
        create_dir(&entity);
        create_file(&entity.join("PUBLISHED"), b"");
        let generation = entity.join("generations");
        create_dir(&generation);
        let g1 = generation.join(G1);
        create_dir(&g1);
        create_file(
            &g1.join("head.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/agent-head.json"),
        );
        create_file(
            &g1.join("definition.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/agent-definition.json"),
        );
        create_file(&g1.join("COMMITTED"), b"");
    }

    fn conversation_header_fixture() -> Vec<u8> {
        format!(
            "{{\"type\":\"session_header\",\"data\":{{\"formatVersion\":1,\"sessionId\":\"{SESSION_ID}\",\"createdAt\":\"2026-08-03T10:01:00.456Z\",\"initialAgent\":{{\"agentId\":\"{AGENT_ID}\",\"revision\":\"ar_1\"}},\"initialDefinitionRevision\":\"sdr_1\"}}}}\n"
        )
        .into_bytes()
    }

    fn create_fixture_session(root: &Path, workspace: &Path) {
        let reservation = root.join("reservations").join("sessions").join(SESSION_ID);
        create_file(&reservation, b"");
        let entity = root.join("sessions").join(SESSION_ID);
        create_dir(&entity);
        create_file(&entity.join("PUBLISHED"), b"");
        create_file(
            &entity.join("conversation.jsonl"),
            &conversation_header_fixture(),
        );
        let generation = entity.join("generations");
        create_dir(&generation);
        let g1 = generation.join(G1);
        create_dir(&g1);
        create_file(
            &g1.join("head.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/session-head.json"),
        );
        create_file(
            &g1.join("definition.json"),
            &session_definition_fixture(workspace),
        );
        create_file(&g1.join("COMMITTED"), b"");
    }

    async fn open_state(root: &Path) -> (RuntimeTaskContext, DurableState) {
        let context = RuntimeTaskContext::new(Handle::current())
            .await
            .expect("the test runtime has a time driver");
        let result = DurableState::open(root.to_owned(), context.clone()).await;
        match result {
            Ok(state) => (context, state),
            Err(error) => {
                context.shutdown().await;
                panic!("fixture Store opens: {error:?}");
            }
        }
    }

    async fn loaded_fixture(store: &TempStore) -> LoadedFixture {
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state
            .session_current_definition(session_id)
            .expect("the fixture Session definition is current");
        let candidate = resolver
            .resolve(session_id, definition.workspace())
            .await
            .expect("the fixture Workspace resolves");
        let prompts = Arc::from(Vec::new().into_boxed_slice());
        let skills = Arc::from(Vec::new().into_boxed_slice());
        let workspace_snapshot = candidate.finish(prompts, skills).unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_without_conversation(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
            Arc::clone(&definition),
            workspace_snapshot,
        )
        .expect("the loaded Ready+Idle executor starts");
        LoadedFixture {
            context,
            state,
            executor,
            definition,
        }
    }

    fn changed_workspace(path: &Path) -> Workspace {
        let key: WorkspaceRootKey = "repo".parse().unwrap();
        lower_workspace(
            WorkspaceDefinitionInput::new(
                WorkspaceRootInput::new(
                    key.clone(),
                    workspace_uri(path),
                    RequestedFilesystemAccess::ReadWrite,
                    WorkspaceSourcePolicy::new(true, true),
                ),
                Vec::new(),
                WorkspaceCwdSpec::new(key, "src".parse().unwrap()),
            )
            .unwrap(),
            "wr_99".parse().unwrap(),
            WorkspacePathTarget::current(),
        )
        .unwrap()
    }

    async fn close_loaded(loaded: LoadedFixture) {
        let _ = loaded.executor.close().await;
        loaded.state.close().await;
        // DurableState closes the shared owner; retaining this explicit field makes the fixture's
        // ownership visible and prevents accidental detached context tasks in future test edits.
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_ready_idle_snapshot_and_debug_are_redacted() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let snapshot = loaded.executor.snapshot().await.unwrap();
        assert_eq!(snapshot.execution_state(), SessionExecutionState::Idle);
        assert_eq!(snapshot.definition_revision().get(), 1);
        assert_eq!(snapshot.workspace_revision().get(), 1);
        assert!(Arc::ptr_eq(snapshot.definition(), &loaded.definition));
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains(SESSION_ID));
        assert!(!debug.contains(store.old_workspace.to_string_lossy().as_ref()));
        assert!(!debug.contains("2026-08-03"));
        assert!(
            !format!(
                "{:?}",
                SessionWorkspaceDefinitionError::WorkspaceUnavailable
            )
            .contains(SESSION_ID)
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn changed_workspace_publishes_store_generation_and_reopens() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let current = loaded.executor.snapshot().await.unwrap();
        let result = loaded
            .executor
            .update_workspace_definition(
                current.definition_revision(),
                changed_workspace(&store.new_workspace),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await
            .expect("the changed Workspace publishes");
        assert!(result.changed());
        assert_eq!(result.definition_revision().get(), 2);
        assert_eq!(result.workspace_revision().get(), 2);
        let after = loaded.executor.snapshot().await.unwrap();
        assert_eq!(after.definition_revision().get(), 2);
        assert_eq!(after.workspace_revision().get(), 2);
        assert_eq!(after.workspace().session_id(), SESSION_ID.parse().unwrap());
        assert_eq!(after.workspace().revision().get(), 2);
        assert!(
            store
                .next_generation_path()
                .join("definition.json")
                .is_file()
        );
        assert!(
            loaded
                .state
                .session_current_definition(SESSION_ID.parse().unwrap())
                .unwrap()
                .workspace()
                .primary_root()
                .path()
                == store.new_workspace.as_path()
        );
        let conversation = store.session_path().join("conversation.jsonl");
        assert_eq!(
            fs::read(conversation).unwrap(),
            conversation_header_fixture()
        );
        close_loaded(loaded).await;

        let (context, reopened) = open_state(&store.root).await;
        let definition = reopened
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(definition.revision().get(), 2);
        assert_eq!(definition.workspace().revision().get(), 2);
        assert_eq!(
            definition.workspace().primary_root().path(),
            store.new_workspace
        );
        reopened.close().await;
        let _ = context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publication_barrier_keeps_old_snapshot_and_admission_busy() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let publication_old = Arc::clone(&old);
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    publication_old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        let visible = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&visible, &old));
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::SessionBusy)
        );
        assert_eq!(
            loaded.executor.starting_admission_probe_for_test().await,
            Err(SessionWorkspaceDefinitionError::SessionBusy)
        );
        hooks.release_after_candidate_snapshot_finish_before_durable();
        assert!(publication.await.unwrap().unwrap().changed());
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_update_waiter_does_not_cancel_publication_or_install() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication_old = Arc::clone(&old);
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    publication_old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        publication.abort();
        assert!(publication.await.unwrap_err().is_cancelled());
        hooks.release_after_candidate_snapshot_finish_before_durable();
        hooks.wait_for_publication_settlement().await;
        let after = loaded.executor.snapshot().await.unwrap();
        assert_eq!(after.definition_revision().get(), 2);
        assert_eq!(after.workspace_revision().get(), 2);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_update_waiter_after_commit_does_not_cancel_publication_or_install() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_commit_before_install();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication_old = Arc::clone(&old);
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    publication_old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks.wait_after_commit_before_install().await;
        let visible = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&visible, &old));
        publication.abort();
        assert!(publication.await.unwrap_err().is_cancelled());
        hooks.release_after_commit_before_install();
        hooks.wait_for_publication_settlement().await;
        let after = loaded.executor.snapshot().await.unwrap();
        assert_eq!(after.definition_revision().get(), 2);
        assert_eq!(after.workspace_revision().get(), 2);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_a_blocked_admitted_publication() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        hooks.release_after_candidate_snapshot_finish_before_durable();
        close.await.expect("the executor closes normally");
        assert!(publication.await.unwrap().unwrap().changed());
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_a_preclose_reserved_request_permit() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let permit = loaded
            .executor
            .sender
            .clone()
            .reserve_owned()
            .await
            .expect("the open executor reserves bounded request capacity");
        let (response, waiter) = oneshot::channel();
        let request = SessionExecutorRequest::Snapshot(SnapshotRequest {
            response: Some(response),
        });

        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        // Let the actor run its close transition. The reserved permit keeps the closed receiver
        // from yielding None, so close must remain pending until this permit is consumed.
        tokio::task::yield_now().await;
        assert!(poll_once_pending(close.as_mut()).await);

        let _sender = permit.send(request);
        assert!(matches!(
            waiter.await.unwrap(),
            Err(SessionExecutorSnapshotError::Closing)
        ));
        close.await.expect("the executor closes normally");

        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_finishes_after_a_preclose_reserved_request_permit_is_dropped() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let permit = loaded
            .executor
            .sender
            .clone()
            .reserve_owned()
            .await
            .expect("the open executor reserves bounded request capacity");

        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        tokio::task::yield_now().await;
        assert!(poll_once_pending(close.as_mut()).await);
        drop(permit);
        close.await.expect("the executor closes normally");

        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_post_commit_install_and_publication_settlement() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let durable_task_count = loaded.context.registered_task_count_for_test() - 1;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_commit_before_install();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks.wait_after_commit_before_install().await;
        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        hooks.release_after_commit_before_install();
        close.await.expect("the executor closes normally");
        assert!(publication.await.unwrap().unwrap().changed());
        hooks.wait_for_publication_settlement().await;
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            durable_task_count
        );
        loaded.state.close().await;
        assert_eq!(loaded.context.registered_task_count_for_test(), 0);
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_request_lane_drains_active_publication_without_global_poison() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        publication.abort();
        assert!(publication.await.unwrap_err().is_cancelled());

        let LoadedFixture {
            context,
            state,
            executor,
            ..
        } = loaded;
        drop(executor);
        hooks.release_after_candidate_snapshot_finish_before_durable();
        hooks.wait_for_publication_settlement().await;
        assert!(!context.is_closing());
        assert_eq!(
            state
                .session_current_definition(SESSION_ID.parse().unwrap())
                .unwrap()
                .revision()
                .get(),
            2
        );
        state.close().await;
        let _ = context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_errors_keep_old_definition_and_snapshot() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let before = loaded.executor.snapshot().await.unwrap();
        let missing = store.root.join("missing-workspace");
        let unavailable = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                changed_workspace(&missing),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await;
        assert_eq!(
            unavailable,
            Err(SessionWorkspaceDefinitionError::WorkspaceUnavailable)
        );
        let file_path = store.root.join("not-a-directory");
        create_file(&file_path, b"fixture");
        let rejected = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                changed_workspace(&file_path),
                "2026-08-03T10:03:00.000Z".parse().unwrap(),
            )
            .await;
        assert_eq!(
            rejected,
            Err(SessionWorkspaceDefinitionError::WorkspaceRejected)
        );
        let after = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&after, &before));
        assert_eq!(loaded.definition.revision().get(), 1);
        assert_eq!(
            loaded
                .state
                .session_head(SESSION_ID.parse().unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            1
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canonical_noop_is_zero_resolver_io_and_stale_wins_before_noop() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let before = loaded.executor.snapshot().await.unwrap();
        fs::remove_dir_all(&store.old_workspace).unwrap();
        let no_change = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                loaded.definition.workspace().clone(),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await
            .unwrap();
        assert!(!no_change.changed());
        let after = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&after, &before));
        assert!(!store.next_generation_path().exists());
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    "sdr_2".parse().unwrap(),
                    loaded.definition.workspace().clone(),
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::StaleRevision)
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aborting_publication_worker_before_durable_settlement_closes_admission() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        loaded.context.abort_latest_registered_task();
        assert_eq!(
            publication.await.unwrap(),
            Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
        );
        assert!(matches!(
            loaded.executor.snapshot().await,
            Err(SessionExecutorSnapshotError::Closing)
        ));
        assert_eq!(
            loaded.executor.close().await,
            Err(SessionExecutorCloseError::InternalDispatchUnavailable)
        );
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unexpected_actor_exit_closes_owners_and_settles_future_requests() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        loaded.context.abort_latest_registered_task();
        let mut result = Box::pin(loaded.executor.snapshot());
        assert!(poll_once_pending(result.as_mut()).await);
        assert!(matches!(
            result.await,
            Err(SessionExecutorSnapshotError::Closing)
        ));
        assert_eq!(
            loaded.executor.close().await,
            Err(SessionExecutorCloseError::InternalDispatchUnavailable)
        );
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_commit_snapshot_install_failure_closes_admission_but_durable_reopens_new() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        loaded
            .executor
            .test_hooks()
            .fail_next_snapshot_install_after_commit();
        let before = loaded.executor.snapshot().await.unwrap();
        let result = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                changed_workspace(&store.new_workspace),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await;
        assert_eq!(
            result,
            Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
        );
        assert!(matches!(
            loaded.executor.snapshot().await,
            Err(SessionExecutorSnapshotError::Closing)
        ));
        let durable = loaded
            .state
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(durable.revision().get(), 2);
        assert_eq!(durable.workspace().revision().get(), 2);
        close_loaded(loaded).await;
        let (context, reopened) = open_state(&store.root).await;
        let durable = reopened
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(durable.revision().get(), 2);
        assert_eq!(
            durable.workspace().primary_root().path(),
            store.new_workspace
        );
        reopened.close().await;
        let _ = context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publication_workers_are_reaped_after_success_and_ordinary_error() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let known_durable_tasks = loaded.context.registered_task_count_for_test() - 1;

        let before_error = loaded.executor.snapshot().await.unwrap();
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    before_error.definition_revision(),
                    changed_workspace(&store.root.join("missing-workspace")),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::WorkspaceUnavailable)
        );
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks + 1
        );

        let before_success = loaded.executor.snapshot().await.unwrap();
        assert!(
            loaded
                .executor
                .update_workspace_definition(
                    before_success.definition_revision(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                )
                .await
                .unwrap()
                .changed()
        );
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks + 1
        );

        let before_second_success = loaded.executor.snapshot().await.unwrap();
        assert!(
            loaded
                .executor
                .update_workspace_definition(
                    before_second_success.definition_revision(),
                    changed_workspace(&store.old_workspace),
                    "2026-08-03T10:04:00.000Z".parse().unwrap(),
                )
                .await
                .unwrap()
                .changed()
        );
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks + 1
        );

        loaded
            .executor
            .close()
            .await
            .expect("the executor closes normally");
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks
        );
        loaded.state.close().await;
        assert_eq!(loaded.context.registered_task_count_for_test(), 0);
        let _ = loaded.context;
    }
}
