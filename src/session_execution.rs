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

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent_session_lifecycle::{
    SealedSessionDefinitionAttempt, SessionDefinition, SessionDefinitionDecision,
    SessionDefinitionDecisionError, SessionLifecycle,
};
use crate::conversation_storage::{
    ConversationReplayDiagnostics, SessionRecorder, StoredAssistantContent, StoredAssistantMessage,
    StoredToolMessage, StoredToolOutcome, StoredUserMessage,
};
use crate::durable_state::{
    AgentAdmissionError, DurableAgentDefinitionReadError, DurableSessionDefinitionError,
    DurableSessionDefinitionOutcome, DurableState,
};
use crate::live_conversation::{
    InteractionRequestCandidate, InteractionResolutionApplyOutcome, InteractionResolutionCandidate,
    LiveSessionState,
};
use crate::model_gateway::{
    FinalizedAssistantContent, ModelCallError, ModelCallErrorReason, ModelCallPurpose,
    ModelCallRequest, ModelCallResult, ModelCatalogView, ModelGateway, ModelProgressPublisher,
    ModelRequestValidationErrorKind, ProviderRequestDeliveryState,
};
use crate::prompt::{
    PromptError, PromptErrorKind, PromptIntent, PromptResourceView, PromptService,
};
use crate::runtime_task::{Clock, RuntimeTaskContext, RuntimeTaskError, SystemClock, TrackedTask};
use crate::session_ingress::{
    FollowUpQueue, FollowUpQueueError, QueuedSteer, SteerQueue, SteerQueueError,
};
#[cfg(test)]
use crate::tools::ToolExecutionResult;
use crate::tools::{ToolCall, ToolExecutionOutcome, ToolExecutionRequest, ToolSet};
use crate::turn_execution_context::{
    TurnContextCapture, TurnContextCaptureError, TurnExecutionContext,
};
use crate::turn_item_interaction::{
    AssistantDisposition, InteractionRequest, ResolvedInteraction, UserMessageSource,
};
use crate::wire::{
    CommandId, IdGenerationError, InteractionResolutionKey, ItemId, RequestId,
    SessionDefinitionRevision, SessionId, Timestamp, TurnId, WorkspaceRevision,
};
use crate::workspace::{
    Workspace, WorkspaceResolveError, WorkspaceResolver, WorkspaceSnapshot,
    WorkspaceSnapshotFinishError,
};

const SESSION_EXECUTOR_REQUEST_QUEUE_CAPACITY: usize = 8;
const SESSION_EVENT_CAPACITY: usize = 32;
const AGENT_RUN_MAX_LOGICAL_RETRIES: usize = 3;
const AGENT_RUN_RETRY_BACKOFFS: [std::time::Duration; AGENT_RUN_MAX_LOGICAL_RETRIES] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(4),
    std::time::Duration::from_secs(8),
];

/// The only execution states represented by a loaded Session executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionTurnFailure {
    Prompt,
    Model,
    ContextOverflow,
    AgentUnavailable,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionTurnTerminal {
    Completed,
    Failed(SessionTurnFailure),
}

#[derive(Clone)]
pub(crate) struct SessionExecutorEvent {
    timestamp: Timestamp,
    command_id: CommandId,
    turn_id: TurnId,
    terminal: SessionTurnTerminal,
    snapshot: Arc<SessionExecutorSnapshot>,
}

impl SessionExecutorEvent {
    pub(crate) const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub(crate) const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub(crate) const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn terminal(&self) -> SessionTurnTerminal {
        self.terminal
    }

    pub(crate) const fn snapshot(&self) -> &Arc<SessionExecutorSnapshot> {
        &self.snapshot
    }
}

pub(crate) struct SessionExecutorSubscription {
    snapshot: Arc<SessionExecutorSnapshot>,
    receiver: broadcast::Receiver<Arc<SessionExecutorEvent>>,
}

impl SessionExecutorSubscription {
    pub(crate) const fn snapshot(&self) -> &Arc<SessionExecutorSnapshot> {
        &self.snapshot
    }

    pub(crate) async fn recv(&mut self) -> Option<Arc<SessionExecutorEvent>> {
        self.receiver.recv().await.ok()
    }
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
    current_turn: Option<TurnId>,
    last_terminal: Option<(TurnId, SessionTurnTerminal)>,
    pending_interactions: Arc<[crate::live_conversation::PendingInteractionFact]>,
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
            current_turn: None,
            last_terminal: None,
            pending_interactions: Arc::from([]),
        }
    }

    fn with_execution(
        &self,
        execution_state: SessionExecutionState,
        current_turn: Option<TurnId>,
        last_terminal: Option<(TurnId, SessionTurnTerminal)>,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            workspace: Arc::clone(&self.workspace),
            execution_state,
            current_turn,
            last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
        }
    }

    fn with_pending_interactions(
        &self,
        pending_interactions: Arc<[crate::live_conversation::PendingInteractionFact]>,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            workspace: Arc::clone(&self.workspace),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            last_terminal: self.last_terminal,
            pending_interactions,
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

    pub(crate) const fn current_turn(&self) -> Option<TurnId> {
        self.current_turn
    }

    pub(crate) const fn last_terminal(&self) -> Option<(TurnId, SessionTurnTerminal)> {
        self.last_terminal
    }

    pub(crate) fn pending_interactions(
        &self,
    ) -> &[crate::live_conversation::PendingInteractionFact] {
        &self.pending_interactions
    }
}

impl fmt::Debug for SessionExecutorSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionExecutorSnapshot")
            .field("session_definition_revision", &self.definition.revision())
            .field("workspace_revision", &self.workspace.revision())
            .field("execution_state", &self.execution_state)
            .field("current_turn", &self.current_turn)
            .field("last_terminal", &self.last_terminal)
            .field("pending_interactions", &self.pending_interactions.len())
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

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionInteractionError {
    #[error("session executor is closing")]
    Closing,
    #[error("interaction is not pending")]
    NotFound,
    #[error("interaction resolution is invalid for the pending request")]
    InvalidResolution,
    #[error("session interaction dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionCancelError {
    #[error("session executor is closing")]
    Closing,
    #[error("the Session is not loaded")]
    SessionNotLoaded,
    #[error("the Submit is no longer cancellable")]
    SubmitNotCancellable,
    #[error("the Turn target does not match the active Turn")]
    ExpectedTurnMismatch,
    #[error("the Turn is not running")]
    TurnNotRunning,
    #[error("the Turn is already cancelling")]
    TurnCancelling,
    #[error("the Turn is already terminal")]
    TurnTerminal,
    #[error("session cancellation dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionFollowUpError {
    #[error("session executor is closing")]
    Closing,
    #[error("session execution has no active Turn")]
    TurnNotRunning,
    #[error("the FollowUp command conflicts with an admitted command")]
    CommandConflict,
    #[error("the FollowUp queue is full")]
    QueueFull,
    #[error("session follow-up dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionSteerError {
    #[error("session executor is closing")]
    Closing,
    #[error("session execution has no active Turn")]
    TurnNotRunning,
    #[error("the Turn is already cancelling")]
    TurnCancelling,
    #[error("the Steer target does not match the active Turn")]
    ExpectedTurnMismatch,
    #[error("the Steer command conflicts with an admitted command")]
    CommandConflict,
    #[error("the Steer queue is full")]
    QueueFull,
    #[error("session steer dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionQueuedMessageError {
    #[error("session executor is closing")]
    Closing,
    #[error("the queued message is not queued")]
    NotQueued,
    #[error("session queued-message dispatch is unavailable")]
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

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionSubmitError {
    #[error("session executor is closing")]
    Closing,
    #[error("session execution is busy")]
    SessionBusy,
    #[error("loaded session execution dependencies are unavailable")]
    DependencyUnavailable,
    #[error("agent is unavailable for execution")]
    AgentUnavailable,
    #[error("turn prompt capture failed")]
    Prompt,
    #[error("turn input is invalid")]
    InvalidArgument,
    #[error("turn input exceeds the model context limit")]
    ContextOverflow,
    #[error("the Submit was cancelled before Turn start")]
    Cancelled,
    #[error("session turn dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionCancelTarget {
    Submit(CommandId),
    Turn(TurnId),
}

#[derive(Clone)]
struct TurnResources {
    prompt_resources: Arc<PromptResourceView>,
    model_gateway: Arc<ModelGateway>,
    model_catalog: Arc<ModelCatalogView>,
    tool_set: Arc<ToolSet>,
}

pub(crate) struct SessionExecutorDependencies {
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    turn_resources: Option<TurnResources>,
}

impl SessionExecutorDependencies {
    pub(crate) fn with_turn_resources(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
    ) -> Self {
        Self::with_turn_resources_and_tools(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
            ToolSet::empty(),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one test-injected turn resource bundle binds the exact runtime owners"
    )]
    pub(crate) fn with_turn_resources_and_tools(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        tool_set: Arc<ToolSet>,
    ) -> Self {
        Self {
            task_context,
            durable_state,
            resolver,
            prompt_service,
            turn_resources: Some(TurnResources {
                prompt_resources,
                model_gateway,
                model_catalog,
                tool_set,
            }),
        }
    }

    #[cfg(test)]
    fn without_turn_resources(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
    ) -> Self {
        Self {
            task_context,
            durable_state,
            resolver,
            prompt_service,
            turn_resources: None,
        }
    }
}

struct TurnAdmissionGate {
    open: Mutex<bool>,
}

impl TurnAdmissionGate {
    fn new() -> Self {
        Self {
            open: Mutex::new(true),
        }
    }

    fn close(&self) {
        *lock(&self.open) = false;
    }

    fn try_enter(&self) -> Option<TurnAdmissionPermit<'_>> {
        let guard = lock(&self.open);
        if !*guard {
            return None;
        }
        Some(TurnAdmissionPermit { _guard: guard })
    }
}

struct TurnAdmissionPermit<'a> {
    _guard: MutexGuard<'a, bool>,
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
    active_control_generation: ActiveControlGeneration,
}

/// Process-local identity for the control owner of one active Turn.  The worker keeps the exact
/// identity it was admitted with and compares it against the actor-owned current identity before
/// every logical retry.
struct ControlGeneration(u8);

type ActiveControlGeneration = Arc<Mutex<Option<(TurnId, Arc<ControlGeneration>)>>>;

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
            active_control_generation: Arc::new(Mutex::new(None)),
        }
    }

    fn install_control_generation(&self, turn_id: TurnId, generation: Arc<ControlGeneration>) {
        *lock(&self.active_control_generation) = Some((turn_id, generation));
    }

    fn clear_control_generation(&self, turn_id: TurnId, generation: &Arc<ControlGeneration>) {
        let mut current = lock(&self.active_control_generation);
        if current
            .as_ref()
            .is_some_and(|(current_turn, current_generation)| {
                *current_turn == turn_id && Arc::ptr_eq(current_generation, generation)
            })
        {
            *current = None;
        }
    }

    fn has_control_generation(&self, turn_id: TurnId, generation: &Arc<ControlGeneration>) -> bool {
        lock(&self.active_control_generation).as_ref().is_some_and(
            |(current_turn, current_generation)| {
                *current_turn == turn_id && Arc::ptr_eq(current_generation, generation)
            },
        )
    }

    #[cfg(test)]
    fn invalidate_control_generation_for_test(&self, turn_id: TurnId) {
        let mut current = lock(&self.active_control_generation);
        if current
            .as_ref()
            .is_some_and(|(current_turn, _)| *current_turn == turn_id)
        {
            *current = None;
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
    turn_admission_gate: Arc<TurnAdmissionGate>,
    published_snapshot: Arc<Mutex<Arc<SessionExecutorSnapshot>>>,
    conversation: Option<Arc<LoadedSessionConversation>>,
    events: broadcast::Sender<Arc<SessionExecutorEvent>>,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

impl fmt::Debug for SessionExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionExecutor { .. }")
    }
}

impl SessionExecutor {
    /// Starts a test-only loaded Ready+Idle Session without Turn resources.
    #[cfg(test)]
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
            SessionExecutorDependencies::without_turn_resources(
                task_context,
                durable_state,
                resolver,
                prompt_service,
            ),
            definition,
            workspace,
            Some(Arc::new(conversation)),
            CancellationToken::new(),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_loaded_ready_idle_with_turn_resources(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: LoadedSessionConversation,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_ready_idle_with_turn_resources_and_lifecycle(
            dependencies,
            definition,
            workspace,
            conversation,
            CancellationToken::new(),
        )
    }

    pub(crate) fn start_loaded_ready_idle_with_turn_resources_and_lifecycle(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: LoadedSessionConversation,
        lifecycle_closing: CancellationToken,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_ready_idle_inner(
            dependencies,
            definition,
            workspace,
            Some(Arc::new(conversation)),
            lifecycle_closing,
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
            SessionExecutorDependencies::without_turn_resources(
                task_context,
                durable_state,
                resolver,
                prompt_service,
            ),
            definition,
            workspace,
            None,
            CancellationToken::new(),
        )
    }

    fn start_loaded_ready_idle_inner(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: Option<Arc<LoadedSessionConversation>>,
        lifecycle_closing: CancellationToken,
    ) -> Result<Self, SessionExecutorStartError> {
        let SessionExecutorDependencies {
            task_context,
            durable_state,
            resolver,
            prompt_service,
            turn_resources,
        } = dependencies;
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
        let published_snapshot = Arc::new(Mutex::new(Arc::clone(&current)));
        let (sender, receiver) = mpsc::channel(SESSION_EXECUTOR_REQUEST_QUEUE_CAPACITY);
        let (completion_sender, completion_receiver) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(SESSION_EVENT_CAPACITY);
        let closing = CancellationToken::new();
        let turn_admission_gate = Arc::new(TurnAdmissionGate::new());
        #[cfg(test)]
        let hooks = Arc::new(SessionExecutorTestHooksInner::new());
        let failure_state = Arc::new(ActorFailureState::default());
        let actor = SessionExecutorActor {
            receiver,
            completions: completion_receiver,
            completion_sender,
            closing: closing.clone(),
            lifecycle_closing,
            task_context: task_context.clone(),
            durable_state: durable_state.clone(),
            resolver,
            prompt_service,
            current,
            published_snapshot: Arc::clone(&published_snapshot),
            execution_state: SessionExecutionState::Idle,
            active_publication: None,
            failure_state: Arc::clone(&failure_state),
            conversation: conversation.clone(),
            turn_resources,
            active_admission: None,
            active_turn: None,
            pending_interactions: BTreeMap::new(),
            follow_up: FollowUpQueue::new(),
            steer: SteerQueue::new(),
            turn_admission_gate: Arc::clone(&turn_admission_gate),
            events: events.clone(),
            #[cfg(test)]
            hooks: Arc::clone(&hooks),
        };
        let mut exit_guard = ActorExitGuard::new(
            closing.clone(),
            task_context.clone(),
            durable_state.clone(),
            Arc::clone(&failure_state),
            Arc::clone(&turn_admission_gate),
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
            turn_admission_gate,
            published_snapshot,
            conversation,
            events,
            #[cfg(test)]
            hooks,
        })
    }

    /// Requests the actor to reject future requests.  An admitted publication may abandon
    /// cancellable candidate capture, but work that has reached durable publication still drains.
    pub(crate) fn request_closing(&self) {
        self.turn_admission_gate.close();
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

    pub(crate) fn published_snapshot(&self) -> Arc<SessionExecutorSnapshot> {
        Arc::clone(&lock(&self.published_snapshot))
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

    pub(crate) async fn submit(
        &self,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<TurnId, SessionSubmitError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Submit(SubmitRequest {
            command_id,
            intent: Some(intent),
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionSubmitError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionSubmitError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionSubmitError::Closing)
            } else {
                Err(SessionSubmitError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn resolve_interaction(
        &self,
        request_id: RequestId,
        resolution: ResolvedInteraction,
        timestamp: Timestamp,
    ) -> Result<(), SessionInteractionError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::ResolveInteraction(ResolveInteractionRequest {
            request_id,
            resolution: Some(resolution),
            timestamp,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionInteractionError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionInteractionError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionInteractionError::Closing)
            } else {
                Err(SessionInteractionError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn cancel(
        &self,
        target: SessionCancelTarget,
        timestamp: Timestamp,
    ) -> Result<(), SessionCancelError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Cancel(CancelRequest {
            target,
            timestamp,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionCancelError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionCancelError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionCancelError::Closing)
            } else {
                Err(SessionCancelError::InternalDispatchUnavailable)
            }
        })
    }

    /// Queues a FollowUp behind the active Turn.  The public command and snapshot projection are
    /// intentionally deferred to the owning M9 slice; this seam only preserves bounded FIFO
    /// admission and terminal handoff inside the Session actor.
    pub(crate) async fn follow_up(
        &self,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), SessionFollowUpError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::FollowUp(FollowUpRequest {
            command_id,
            intent: Some(intent),
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionFollowUpError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionFollowUpError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionFollowUpError::Closing)
            } else {
                Err(SessionFollowUpError::InternalDispatchUnavailable)
            }
        })
    }

    /// Queues a Steer for the active Turn.  The public command and snapshot projection remain
    /// outside this crate-private seam; consumption is performed by the active Turn worker at a
    /// complete assistant/tool safe point.
    pub(crate) async fn steer(
        &self,
        turn_id: TurnId,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), SessionSteerError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Steer(SteerRequest {
            turn_id,
            command_id,
            intent: Some(intent),
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionSteerError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionSteerError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionSteerError::Closing)
            } else {
                Err(SessionSteerError::InternalDispatchUnavailable)
            }
        })
    }

    /// Removes one admitted Steer or FollowUp by CommandId.  The public command and snapshot
    /// projection remain outside this crate-private seam.
    pub(crate) async fn cancel_queued_message(
        &self,
        command_id: CommandId,
    ) -> Result<(), SessionQueuedMessageError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::CancelQueuedMessage(CancelQueuedMessageRequest {
            command_id,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionQueuedMessageError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionQueuedMessageError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionQueuedMessageError::Closing)
            } else {
                Err(SessionQueuedMessageError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn subscribe(
        &self,
    ) -> Result<SessionExecutorSubscription, SessionExecutorSnapshotError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Subscribe(SubscribeRequest {
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
        waiter.await.unwrap_or(Err(
            SessionExecutorSnapshotError::InternalDispatchUnavailable,
        ))
    }

    #[cfg(test)]
    pub(crate) fn test_hooks(&self) -> SessionExecutorTestHooks {
        SessionExecutorTestHooks {
            inner: Arc::clone(&self.hooks),
        }
    }

    #[cfg(test)]
    pub(crate) async fn wait_until_closing_for_test(&self) {
        self.closing.cancelled().await;
    }

    #[cfg(test)]
    pub(crate) fn live_state_for_test(&self) -> Option<Arc<Mutex<LiveSessionState>>> {
        self.conversation
            .as_ref()
            .map(|conversation| Arc::clone(&conversation.live_state))
    }

    #[cfg(test)]
    pub(crate) fn invalidate_control_generation_for_test(&self, turn_id: TurnId) {
        if let Some(conversation) = &self.conversation {
            conversation.invalidate_control_generation_for_test(turn_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn retry_basis_matches_for_test(
        &self,
        turn_id: TurnId,
        source_revision: crate::live_conversation::ConversationRevision,
    ) -> Option<bool> {
        let conversation = self.conversation.as_ref()?;
        let generation = lock(&conversation.active_control_generation)
            .as_ref()
            .and_then(|(current_turn, generation)| {
                (*current_turn == turn_id).then(|| Arc::clone(generation))
            })?;
        Some(retry_basis_is_current(
            conversation,
            turn_id,
            &generation,
            source_revision,
        ))
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
    completions: mpsc::UnboundedReceiver<ExecutorCompletion>,
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    closing: CancellationToken,
    lifecycle_closing: CancellationToken,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    current: Arc<SessionExecutorSnapshot>,
    published_snapshot: Arc<Mutex<Arc<SessionExecutorSnapshot>>>,
    execution_state: SessionExecutionState,
    active_publication: Option<ActivePublication>,
    failure_state: Arc<ActorFailureState>,
    conversation: Option<Arc<LoadedSessionConversation>>,
    turn_resources: Option<TurnResources>,
    active_admission: Option<ActiveAdmission>,
    active_turn: Option<ActiveTurn>,
    pending_interactions: BTreeMap<RequestId, ActiveInteraction>,
    follow_up: FollowUpQueue,
    steer: SteerQueue,
    turn_admission_gate: Arc<TurnAdmissionGate>,
    events: broadcast::Sender<Arc<SessionExecutorEvent>>,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

struct ActivePublication {
    permit: SessionDefinitionPublicationPermit,
    expected: ExpectedPublication,
    waiter: Arc<PublicationWaiterState>,
    worker_task: Option<TrackedTask>,
}

struct ActiveAdmission {
    command_id: CommandId,
    turn_id: TurnId,
    waiter: Option<oneshot::Sender<Result<TurnId, SessionSubmitError>>>,
    cancellation: CancellationToken,
    task: Option<TrackedTask>,
}

struct ActiveTurn {
    command_id: CommandId,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    cancellation: CancellationToken,
    task: Option<TrackedTask>,
    steer_admission_open: bool,
}

struct ActiveInteraction {
    turn_id: TurnId,
    item_id: ItemId,
    resolution_sender: oneshot::Sender<ResolvedInteraction>,
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
                        if let Err(fatality) = self.handle_request(&mut request).await {
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
        self.pending_interactions.clear();
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

            if self.active_publication.is_none()
                && self.active_admission.is_none()
                && self.active_turn.is_none()
                && requests_drained
            {
                if let Some(conversation) = &self.conversation {
                    conversation.recorder.close().await;
                }
                return normal_exit;
            }

            tokio::select! {
                biased;
                completion = self.completions.recv(), if self.active_publication.is_some() || self.active_admission.is_some() || self.active_turn.is_some() => match completion {
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
        self.turn_admission_gate.close();
        self.closing.cancel();
        self.failure_state.mark_fatal();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        active.waiter.settle(Err(
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
        ));
        self.finish_active_waiter(&active.waiter);
    }

    async fn handle_request(
        &mut self,
        request: &mut SessionExecutorRequest,
    ) -> Result<(), ActorFatality> {
        match request {
            SessionExecutorRequest::Snapshot(request) => {
                #[cfg(test)]
                self.hooks.before_snapshot_response().await;
                request.settle(Ok(Arc::clone(&self.current)));
            }
            SessionExecutorRequest::Subscribe(request) => {
                request.settle(Ok(SessionExecutorSubscription {
                    snapshot: Arc::clone(&self.current),
                    receiver: self.events.subscribe(),
                }));
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
            SessionExecutorRequest::Submit(request) => {
                self.start_admission(request)?;
            }
            SessionExecutorRequest::FollowUp(request) => {
                self.enqueue_follow_up(request)?;
            }
            SessionExecutorRequest::Steer(request) => {
                self.enqueue_steer(request)?;
            }
            SessionExecutorRequest::CancelQueuedMessage(request) => {
                self.cancel_queued_message_request(request)?;
            }
            SessionExecutorRequest::ResolveInteraction(request) => {
                self.resolve_interaction_request(request).await?;
            }
            SessionExecutorRequest::Cancel(request) => {
                self.cancel_request(request).await?;
            }
        }
        Ok(())
    }

    fn enqueue_follow_up(&mut self, request: &mut FollowUpRequest) -> Result<(), ActorFatality> {
        let Some(active_turn) = self.active_turn.as_ref() else {
            request.settle(Err(SessionFollowUpError::TurnNotRunning));
            return Ok(());
        };
        if active_turn.command_id == request.command_id
            || self
                .active_admission
                .as_ref()
                .is_some_and(|admission| admission.command_id == request.command_id)
            || self.steer.contains(request.command_id)
        {
            request.settle(Err(SessionFollowUpError::CommandConflict));
            return Ok(());
        }
        let Some(intent) = request.intent.take() else {
            return Err(ActorFatality::Integrity);
        };
        match self.follow_up.try_push(request.command_id, intent) {
            Ok(()) => request.settle(Ok(())),
            Err(FollowUpQueueError::Full) => {
                request.settle(Err(SessionFollowUpError::QueueFull));
            }
            Err(FollowUpQueueError::DuplicateCommandId) => {
                request.settle(Err(SessionFollowUpError::CommandConflict));
            }
        }
        Ok(())
    }

    fn cancel_queued_message_request(
        &mut self,
        request: &mut CancelQueuedMessageRequest,
    ) -> Result<(), ActorFatality> {
        if self.steer.remove(request.command_id).is_some()
            || self.follow_up.remove(request.command_id).is_some()
        {
            request.settle(Ok(()));
        } else {
            request.settle(Err(SessionQueuedMessageError::NotQueued));
        }
        Ok(())
    }

    fn enqueue_steer(&mut self, request: &mut SteerRequest) -> Result<(), ActorFatality> {
        let Some(active_turn) = self.active_turn.as_ref() else {
            request.settle(Err(SessionSteerError::TurnNotRunning));
            return Ok(());
        };
        if active_turn.turn_id != request.turn_id {
            request.settle(Err(SessionSteerError::ExpectedTurnMismatch));
            return Ok(());
        }
        if active_turn.cancellation.is_cancelled() {
            request.settle(Err(SessionSteerError::TurnCancelling));
            return Ok(());
        }
        if !active_turn.steer_admission_open {
            request.settle(Err(SessionSteerError::TurnNotRunning));
            return Ok(());
        }
        if active_turn.command_id == request.command_id
            || self
                .active_admission
                .as_ref()
                .is_some_and(|admission| admission.command_id == request.command_id)
            || self.follow_up.contains(request.command_id)
        {
            request.settle(Err(SessionSteerError::CommandConflict));
            return Ok(());
        }
        let Some(intent) = request.intent.take() else {
            return Err(ActorFatality::Integrity);
        };
        match self
            .steer
            .try_push(request.turn_id, request.command_id, intent)
        {
            Ok(()) => request.settle(Ok(())),
            Err(SteerQueueError::Full) => request.settle(Err(SessionSteerError::QueueFull)),
            Err(SteerQueueError::DuplicateCommandId) => {
                request.settle(Err(SessionSteerError::CommandConflict));
            }
        }
        Ok(())
    }

    fn start_admission(&mut self, request: &mut SubmitRequest) -> Result<(), ActorFatality> {
        if self.active_publication.is_some()
            || self.active_admission.is_some()
            || self.active_turn.is_some()
            || !self.execution_state.is_idle()
        {
            request.settle(Err(SessionSubmitError::SessionBusy));
            return Ok(());
        }
        let (Some(conversation), Some(resources)) =
            (self.conversation.clone(), self.turn_resources.clone())
        else {
            request.settle(Err(SessionSubmitError::DependencyUnavailable));
            return Ok(());
        };
        let Some(intent) = request.intent.take() else {
            return Err(ActorFatality::Integrity);
        };
        let turn_id = TurnId::generate().map_err(|_| ActorFatality::Internal)?;
        let command_id = request.command_id;
        let waiter = request.response.take();
        self.execution_state = SessionExecutionState::Starting;
        let current = Arc::new(self.current.with_execution(
            SessionExecutionState::Starting,
            None,
            self.current.last_terminal(),
        ));
        self.publish_current(current);
        self.active_admission = Some(ActiveAdmission {
            command_id,
            turn_id,
            waiter,
            cancellation: CancellationToken::new(),
            task: None,
        });

        let cancellation = self
            .active_admission
            .as_ref()
            .expect("admission is installed before spawning")
            .cancellation
            .clone();

        let completion_sender = self.completion_sender.clone();
        let durable_state = self.durable_state.clone();
        let definition = Arc::clone(self.current.definition());
        let workspace = Arc::clone(self.current.workspace());
        let prompt_service = Arc::clone(&self.prompt_service);
        let closing = self.closing.clone();
        let turn_admission_gate = Arc::clone(&self.turn_admission_gate);
        #[cfg(test)]
        let hooks = Arc::clone(&self.hooks);
        let guard = AdmissionCompletionGuard::new(completion_sender, turn_id);
        let worker = async move {
            let mut guard = guard;
            let result = run_admission(AdmissionWork {
                closing,
                durable_state,
                definition,
                workspace,
                prompt_service,
                resources,
                conversation,
                turn_admission_gate,
                cancellation,
                turn_id,
                intent,
                #[cfg(test)]
                hooks,
            })
            .await;
            guard.complete(result);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                self.active_admission
                    .as_mut()
                    .expect("admission is installed before spawn")
                    .task = Some(task);
            }
            Err(RuntimeTaskError::OwnerClosing) => {}
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                self.task_context.request_closing();
                self.durable_state.request_closing();
            }
        }
        Ok(())
    }

    async fn handle_admission_completion(
        &mut self,
        completion: AdmissionCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(mut active) = self.active_admission.take() else {
            return Err(ActorFatality::Internal);
        };
        let task_result = match active.task.take() {
            Some(task) => task.wait().await,
            None => Ok(()),
        };
        if task_result.is_err() || active.turn_id != completion.turn_id {
            if let Some(waiter) = active.waiter.take() {
                let _ = waiter.send(Err(SessionSubmitError::InternalDispatchUnavailable));
            }
            return Err(ActorFatality::Internal);
        }

        let context = match completion.result {
            Ok(context) => context,
            Err(error) => {
                self.execution_state = SessionExecutionState::Idle;
                let current = Arc::new(self.current.with_execution(
                    SessionExecutionState::Idle,
                    None,
                    self.current.last_terminal(),
                ));
                self.publish_current(current);
                if let Some(waiter) = active.waiter.take() {
                    let _ = waiter.send(Err(error));
                }
                return Ok(());
            }
        };

        let Some(conversation) = self.conversation.as_ref().cloned() else {
            return Err(ActorFatality::Integrity);
        };
        let Some(resources) = self.turn_resources.as_ref().cloned() else {
            return Err(ActorFatality::Integrity);
        };
        let cancellation = active.cancellation.clone();
        let control_generation = Arc::new(ControlGeneration(0));
        conversation.install_control_generation(active.turn_id, Arc::clone(&control_generation));
        self.execution_state = SessionExecutionState::Running;
        let current = Arc::new(self.current.with_execution(
            SessionExecutionState::Running,
            Some(active.turn_id),
            self.current.last_terminal(),
        ));
        self.publish_current(current);
        self.active_turn = Some(ActiveTurn {
            command_id: active.command_id,
            turn_id: active.turn_id,
            control_generation: Arc::clone(&control_generation),
            cancellation: cancellation.clone(),
            task: None,
            steer_admission_open: true,
        });

        let turn_id = active.turn_id;
        if cancellation.is_cancelled() {
            let _ = self
                .completion_sender
                .send(ExecutorCompletion::Turn(TurnCompletion {
                    turn_id,
                    terminal: SessionTurnTerminal::Failed(SessionTurnFailure::Model),
                }));
        } else {
            let completion_sender = self.completion_sender.clone();
            let executor_closing = self.closing.clone();
            let lifecycle_closing = self.lifecycle_closing.clone();
            let guard = TurnCompletionGuard::new(completion_sender, turn_id);
            let interaction_completion_sender = self.completion_sender.clone();
            let steer_completion_sender = self.completion_sender.clone();
            #[cfg(test)]
            let hooks = Arc::clone(&self.hooks);
            let worker = async move {
                let mut guard = guard;
                let terminal = run_active_turn(
                    context,
                    resources.model_gateway,
                    conversation,
                    turn_id,
                    control_generation,
                    cancellation,
                    executor_closing,
                    lifecycle_closing,
                    interaction_completion_sender,
                    steer_completion_sender,
                    #[cfg(test)]
                    hooks,
                )
                .await;
                guard.complete(terminal);
            };
            match self.task_context.spawn_tracked(worker) {
                Ok(task) => {
                    self.active_turn
                        .as_mut()
                        .expect("active Turn is installed before spawn")
                        .task = Some(task);
                }
                Err(RuntimeTaskError::OwnerClosing) => {}
                Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                    self.task_context.request_closing();
                    self.durable_state.request_closing();
                }
            }
        }
        if let Some(waiter) = active.waiter.take() {
            let _ = waiter.send(Ok(active.turn_id));
        }
        Ok(())
    }

    async fn handle_interaction_requested(
        &mut self,
        completion: InteractionRequestedCompletion,
    ) -> Result<(), ActorFatality> {
        if self.closing.is_cancelled() {
            return Ok(());
        }
        let Some(active_turn) = self.active_turn.as_ref() else {
            return Err(ActorFatality::Internal);
        };
        if active_turn.turn_id != completion.turn_id {
            return Err(ActorFatality::Internal);
        }
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let candidate = InteractionRequestCandidate::new(
            completion.request_id,
            completion.item_id,
            completion.request,
        );
        let fact = lock(&conversation.live_state)
            .apply_interaction_request(candidate, completion.turn_id, completion.timestamp)
            .map_err(|_| ActorFatality::Integrity)?;
        let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        self.pending_interactions.insert(
            completion.request_id,
            ActiveInteraction {
                turn_id: completion.turn_id,
                item_id: completion.item_id,
                resolution_sender: completion.resolution_sender,
            },
        );
        self.publish_pending_interactions()?;
        if self
            .active_turn
            .as_ref()
            .is_some_and(|active_turn| active_turn.cancellation.is_cancelled())
        {
            self.cancel_pending_interaction(completion.request_id, completion.timestamp)
                .await?;
        }
        Ok(())
    }

    async fn resolve_interaction_request(
        &mut self,
        request: &mut ResolveInteractionRequest,
    ) -> Result<(), ActorFatality> {
        let Some(resolution) = request.resolution.take() else {
            return Err(ActorFatality::Integrity);
        };
        if !self.pending_interactions.contains_key(&request.request_id) {
            request.settle(Err(SessionInteractionError::NotFound));
            return Ok(());
        }
        let resolution_for_worker = resolution.clone_for_owner();
        let key = InteractionResolutionKey::generate().map_err(|_| ActorFatality::Internal)?;
        let candidate = InteractionResolutionCandidate::host(request.request_id, key, resolution)
            .map_err(|_| ActorFatality::Integrity)?;
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let apply = lock(&conversation.live_state)
            .apply_interaction_resolution(candidate, request.timestamp)
            .map_err(|_| SessionInteractionError::InvalidResolution);
        let fact = match apply {
            Ok(InteractionResolutionApplyOutcome::Applied(fact)) => fact,
            Ok(InteractionResolutionApplyOutcome::Idempotent { .. }) => {
                request.settle(Err(SessionInteractionError::NotFound));
                return Ok(());
            }
            Err(error) => {
                request.settle(Err(error));
                return Ok(());
            }
        };
        let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        let active = self
            .pending_interactions
            .remove(&request.request_id)
            .ok_or(ActorFatality::Internal)?;
        let _ = active.item_id;
        self.publish_pending_interactions()?;
        let _ = active.resolution_sender.send(resolution_for_worker);
        request.settle(Ok(()));
        Ok(())
    }

    async fn cancel_request(&mut self, request: &mut CancelRequest) -> Result<(), ActorFatality> {
        match request.target {
            SessionCancelTarget::Submit(command_id) => {
                let Some(active) = self.active_admission.as_ref() else {
                    request.settle(Err(SessionCancelError::SubmitNotCancellable));
                    return Ok(());
                };
                if active.command_id != command_id {
                    request.settle(Err(SessionCancelError::SubmitNotCancellable));
                    return Ok(());
                }
                if active.cancellation.is_cancelled() {
                    request.settle(Err(SessionCancelError::TurnCancelling));
                    return Ok(());
                }
                active.cancellation.cancel();
                request.settle(Ok(()));
            }
            SessionCancelTarget::Turn(turn_id) => {
                if let Some(active) = self.active_admission.as_ref() {
                    if active.turn_id != turn_id {
                        request.settle(Err(SessionCancelError::ExpectedTurnMismatch));
                        return Ok(());
                    }
                    if active.cancellation.is_cancelled() {
                        request.settle(Err(SessionCancelError::TurnCancelling));
                        return Ok(());
                    }
                    active.cancellation.cancel();
                    request.settle(Ok(()));
                    return Ok(());
                }

                let Some(active) = self.active_turn.as_ref() else {
                    let error = if self
                        .current
                        .last_terminal()
                        .is_some_and(|(terminal_turn, _)| terminal_turn == turn_id)
                    {
                        SessionCancelError::TurnTerminal
                    } else {
                        SessionCancelError::TurnNotRunning
                    };
                    request.settle(Err(error));
                    return Ok(());
                };
                if active.turn_id != turn_id {
                    request.settle(Err(SessionCancelError::ExpectedTurnMismatch));
                    return Ok(());
                }
                if active.cancellation.is_cancelled() {
                    request.settle(Err(SessionCancelError::TurnCancelling));
                    return Ok(());
                }
                active.cancellation.cancel();
                self.steer.clear_for_turn(turn_id);
                let pending = self
                    .pending_interactions
                    .iter()
                    .filter_map(|(request_id, interaction)| {
                        (interaction.turn_id == turn_id).then_some(*request_id)
                    })
                    .collect::<Vec<_>>();
                request.settle(Ok(()));
                for request_id in pending {
                    self.cancel_pending_interaction(request_id, request.timestamp)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn cancel_pending_interaction(
        &mut self,
        request_id: RequestId,
        timestamp: Timestamp,
    ) -> Result<(), ActorFatality> {
        let Some(active) = self.pending_interactions.get(&request_id) else {
            return Ok(());
        };
        let turn_id = active.turn_id;
        let candidate = InteractionResolutionCandidate::owner_cancellation(
            request_id,
            crate::turn_item_interaction::InteractionCancelReason::TurnCancelled,
        )
        .map_err(|_| ActorFatality::Integrity)?;
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let fact = match lock(&conversation.live_state)
            .apply_interaction_resolution(candidate, timestamp)
            .map_err(|_| ActorFatality::Integrity)?
        {
            InteractionResolutionApplyOutcome::Applied(fact) => fact,
            InteractionResolutionApplyOutcome::Idempotent { .. } => return Ok(()),
        };
        let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        let active = self
            .pending_interactions
            .remove(&request_id)
            .ok_or(ActorFatality::Internal)?;
        self.publish_pending_interactions()?;
        let _ = active.resolution_sender.send(
            ResolvedInteraction::cancelled_by_owner(
                crate::turn_item_interaction::InteractionCancelReason::TurnCancelled,
            )
            .ok_or(ActorFatality::Integrity)?,
        );
        debug_assert_eq!(active.turn_id, turn_id);
        Ok(())
    }

    fn publish_pending_interactions(&mut self) -> Result<(), ActorFatality> {
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let pending = lock(&conversation.live_state).pending_interaction_facts();
        self.publish_current(Arc::new(self.current.with_pending_interactions(pending)));
        Ok(())
    }

    async fn handle_turn_completion(
        &mut self,
        completion: TurnCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(mut active) = self.active_turn.take() else {
            return Err(ActorFatality::Internal);
        };
        let task_result = match active.task.take() {
            Some(task) => task.wait().await,
            None => Ok(()),
        };
        if active.turn_id != completion.turn_id {
            return Err(ActorFatality::Internal);
        }
        let Some(conversation) = self.conversation.as_ref().cloned() else {
            return Err(ActorFatality::Integrity);
        };
        conversation.clear_control_generation(active.turn_id, &active.control_generation);
        let pending_before = self.pending_interactions.len();
        self.pending_interactions
            .retain(|_, interaction| interaction.turn_id != active.turn_id);
        if self.pending_interactions.len() != pending_before {
            self.publish_pending_interactions()?;
        }
        self.steer.clear_for_turn(active.turn_id);
        let live_turn = lock(&conversation.live_state).current_turn();
        if live_turn == Some(active.turn_id) {
            lock(&conversation.live_state)
                .fail_current_turn(active.turn_id)
                .map_err(|_| ActorFatality::Integrity)?;
        } else if live_turn.is_some() {
            return Err(ActorFatality::Integrity);
        }
        if task_result.is_err() {
            self.task_context.request_closing();
            self.durable_state.request_closing();
        }
        self.execution_state = SessionExecutionState::Idle;
        let current = Arc::new(self.current.with_execution(
            SessionExecutionState::Idle,
            None,
            Some((active.turn_id, completion.terminal)),
        ));
        self.publish_current(current);
        let _ = self.events.send(Arc::new(SessionExecutorEvent {
            timestamp: SystemClock.now(),
            command_id: active.command_id,
            turn_id: active.turn_id,
            terminal: completion.terminal,
            snapshot: Arc::clone(&self.current),
        }));
        if task_result.is_ok() {
            if let Some(queued) = self.follow_up.pop_front() {
                let (command_id, intent) = queued.into_parts();
                let mut request = SubmitRequest {
                    command_id,
                    intent: Some(intent),
                    response: None,
                };
                self.start_admission(&mut request)?;
            }
        }
        if task_result.is_err() {
            Err(ActorFatality::Internal)
        } else {
            Ok(())
        }
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
        completion: ExecutorCompletion,
    ) -> Result<(), ActorFatality> {
        match completion {
            ExecutorCompletion::Publication(completion) => {
                self.handle_publication_completion(completion).await
            }
            ExecutorCompletion::Admission(completion) => {
                self.handle_admission_completion(completion).await
            }
            ExecutorCompletion::InteractionRequested(completion) => {
                self.handle_interaction_requested(completion).await
            }
            ExecutorCompletion::SteerSafePoint(completion) => {
                self.handle_steer_safe_point(completion).await
            }
            ExecutorCompletion::Turn(completion) => self.handle_turn_completion(completion).await,
        }
    }

    async fn handle_steer_safe_point(
        &mut self,
        completion: SteerSafePointCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(active_turn) = self.active_turn.as_ref() else {
            return Err(ActorFatality::Internal);
        };
        if active_turn.turn_id != completion.turn_id {
            return Err(ActorFatality::Internal);
        }
        let steer = self.steer.pop_front_for_turn(completion.turn_id);
        if steer.is_none() && completion.close_if_empty {
            if let Some(active_turn) = self.active_turn.as_mut() {
                active_turn.steer_admission_open = false;
            }
        }
        #[cfg(test)]
        self.hooks.after_steer_arbitration().await;
        let _ = completion.response.send(steer);
        Ok(())
    }

    async fn handle_publication_completion(
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
                    let current = Arc::new(
                        SessionExecutorSnapshot::new(definition, snapshot, self.execution_state)
                            .with_execution(
                                self.execution_state,
                                self.current.current_turn(),
                                self.current.last_terminal(),
                            ),
                    );
                    self.publish_current(current);
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
                    self.turn_admission_gate.close();
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
        self.turn_admission_gate.close();
        self.closing.cancel();
        self.pending_interactions.clear();
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
        let current = Arc::new(self.current.with_execution(
            execution_state,
            self.current.current_turn(),
            self.current.last_terminal(),
        ));
        self.publish_current(current);
    }

    fn publish_current(&mut self, current: Arc<SessionExecutorSnapshot>) {
        self.current = Arc::clone(&current);
        *lock(&self.published_snapshot) = current;
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

enum ExecutorCompletion {
    Publication(PublicationCompletion),
    Admission(AdmissionCompletion),
    InteractionRequested(InteractionRequestedCompletion),
    SteerSafePoint(SteerSafePointCompletion),
    Turn(TurnCompletion),
}

struct InteractionRequestedCompletion {
    turn_id: TurnId,
    timestamp: Timestamp,
    item_id: ItemId,
    tool_call_id: crate::tools::ToolCallId,
    request_id: RequestId,
    request: InteractionRequest,
    resolution_sender: oneshot::Sender<ResolvedInteraction>,
}

struct SteerSafePointCompletion {
    turn_id: TurnId,
    response: oneshot::Sender<Option<QueuedSteer>>,
    close_if_empty: bool,
}

struct PublicationCompletion {
    permit: SessionDefinitionPublicationPermit,
    result: PublicationCompletionResult,
}

struct AdmissionCompletion {
    turn_id: TurnId,
    result: Result<Arc<TurnExecutionContext>, SessionSubmitError>,
}

struct TurnCompletion {
    turn_id: TurnId,
    terminal: SessionTurnTerminal,
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

struct AdmissionWork {
    closing: CancellationToken,
    cancellation: CancellationToken,
    durable_state: DurableState,
    definition: Arc<SessionDefinition>,
    workspace: Arc<WorkspaceSnapshot>,
    prompt_service: Arc<PromptService>,
    resources: TurnResources,
    conversation: Arc<LoadedSessionConversation>,
    turn_admission_gate: Arc<TurnAdmissionGate>,
    turn_id: TurnId,
    intent: PromptIntent,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

async fn run_admission(
    work: AdmissionWork,
) -> Result<Arc<TurnExecutionContext>, SessionSubmitError> {
    let AdmissionWork {
        closing,
        cancellation,
        durable_state,
        definition,
        workspace,
        prompt_service,
        resources,
        conversation,
        turn_admission_gate,
        turn_id,
        intent,
        #[cfg(test)]
        hooks,
    } = work;
    if closing.is_cancelled() {
        return Err(SessionSubmitError::Closing);
    }
    if cancellation.is_cancelled() {
        return Err(SessionSubmitError::Cancelled);
    }
    let agent_read = durable_state.read_agent_definition(definition.agent());
    tokio::pin!(agent_read);
    let agent = tokio::select! {
        biased;
        _ = closing.cancelled() => return Err(SessionSubmitError::Closing),
        _ = cancellation.cancelled() => return Err(SessionSubmitError::Cancelled),
        result = &mut agent_read => result,
    }
    .map_err(map_agent_definition_read_error)?;
    let context = TurnExecutionContext::capture(TurnContextCapture {
        turn_id,
        session: definition,
        agent,
        workspace,
        prompt_service,
        prompt_resources: resources.prompt_resources,
        model_gateway: resources.model_gateway,
        model_catalog: resources.model_catalog,
        tool_set: resources.tool_set,
    })
    .map_err(map_turn_context_capture_error)?;
    let message = tokio::select! {
        biased;
        _ = closing.cancelled() => return Err(SessionSubmitError::Closing),
        _ = cancellation.cancelled() => return Err(SessionSubmitError::Cancelled),
        result = context.resolve_user_message(intent) => result.map_err(map_submit_prompt_error)?,
    };
    if lock(&conversation.live_state).session_id() != context.session_id() {
        return Err(SessionSubmitError::InternalDispatchUnavailable);
    }
    if closing.is_cancelled() {
        return Err(SessionSubmitError::Closing);
    }
    if cancellation.is_cancelled() {
        return Err(SessionSubmitError::Cancelled);
    }
    let item_id = ItemId::generate().map_err(map_id_generation_error)?;
    let admission = tokio::select! {
        biased;
        _ = closing.cancelled() => return Err(SessionSubmitError::Closing),
        _ = cancellation.cancelled() => return Err(SessionSubmitError::Cancelled),
        result = durable_state.acquire_agent_admission(context.agent()) => {
            result.map_err(map_agent_admission_error)?
        }
    };
    #[cfg(test)]
    hooks.after_agent_admission_before_input().await;
    let fact = {
        let _admission = admission;
        let _turn_admission = turn_admission_gate
            .try_enter()
            .ok_or(SessionSubmitError::Closing)?;
        if closing.is_cancelled() {
            return Err(SessionSubmitError::Closing);
        }
        if cancellation.is_cancelled() {
            return Err(SessionSubmitError::Cancelled);
        }
        let mut live_state = lock(&conversation.live_state);
        live_state
            .apply_user_message(
                StoredUserMessage::reconstruct(item_id, UserMessageSource::Input, message),
                turn_id,
                SystemClock.now(),
            )
            .map_err(|_| SessionSubmitError::InternalDispatchUnavailable)?
    };
    let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
    Ok(context)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one ActiveTurn binds its immutable context, model, channels, and cancellation basis"
)]
async fn run_active_turn(
    context: Arc<TurnExecutionContext>,
    model_gateway: Arc<ModelGateway>,
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    cancellation: CancellationToken,
    executor_closing: CancellationToken,
    closing: CancellationToken,
    interaction_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    steer_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> SessionTurnTerminal {
    if context.turn_id() != turn_id {
        return SessionTurnTerminal::Failed(SessionTurnFailure::Internal);
    }
    let result = run_active_turn_inner(
        Arc::clone(&context),
        model_gateway,
        Arc::clone(&conversation),
        turn_id,
        control_generation,
        cancellation,
        executor_closing,
        closing,
        interaction_completion_sender,
        steer_completion_sender,
        #[cfg(test)]
        hooks,
    )
    .await;
    match result {
        Ok(entry) => {
            let _ = conversation.recorder.record(entry).await;
            SessionTurnTerminal::Completed
        }
        Err(failure) => {
            let settled = {
                let mut live_state = lock(&conversation.live_state);
                if live_state.current_turn() == Some(turn_id) {
                    live_state.fail_current_turn(turn_id)
                } else {
                    Ok(())
                }
            };
            if settled.is_err() {
                SessionTurnTerminal::Failed(SessionTurnFailure::Internal)
            } else {
                SessionTurnTerminal::Failed(failure)
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one ActiveTurn binds its immutable context, model, channels, and cancellation basis"
)]
async fn run_active_turn_inner(
    context: Arc<TurnExecutionContext>,
    model_gateway: Arc<ModelGateway>,
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    cancellation: CancellationToken,
    executor_closing: CancellationToken,
    closing: CancellationToken,
    interaction_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    steer_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> Result<Arc<crate::conversation_storage::StoredSessionEntry>, SessionTurnFailure> {
    loop {
        if cancellation.is_cancelled() {
            return Err(SessionTurnFailure::Model);
        }
        let captured = lock(&conversation.live_state)
            .capture_conversation_views()
            .map_err(|_| SessionTurnFailure::Internal)?;
        let source_revision = captured.conversation().revision();
        let assembled = context
            .assemble_agent_run(captured.conversation())
            .map_err(map_turn_prompt_error)?;
        let request = ModelCallRequest::new(
            Arc::clone(context.model()),
            ModelCallPurpose::AgentRun,
            assembled,
            source_revision,
            None,
        )
        .map(Arc::new)
        .map_err(map_model_request_error)?;
        #[cfg(test)]
        hooks.before_agent_run_attempt().await;
        let (result, logical_retry_count) = call_agent_run_with_logical_retry(
            &model_gateway,
            Arc::clone(&request),
            &conversation,
            turn_id,
            Arc::clone(&control_generation),
            cancellation.clone(),
            executor_closing.clone(),
            closing.clone(),
        )
        .await
        .map_err(map_model_call_failure)?;
        if cancellation.is_cancelled() {
            return Err(SessionTurnFailure::Model);
        }
        let response = result.response();
        let mut content = Vec::with_capacity(response.content().len());
        let mut calls = Vec::new();
        for block in response.content() {
            let item_id = ItemId::generate().map_err(|_| SessionTurnFailure::Internal)?;
            match block {
                FinalizedAssistantContent::Reasoning(reasoning) => {
                    content.push(StoredAssistantContent::Reasoning {
                        item_id,
                        content: reasoning.clone(),
                    });
                }
                FinalizedAssistantContent::Text { text } => {
                    content.push(StoredAssistantContent::Text {
                        item_id,
                        text: Arc::clone(text),
                    });
                }
                FinalizedAssistantContent::ToolCall {
                    tool_call_id,
                    name,
                    arguments,
                } => {
                    let call_index =
                        u32::try_from(calls.len()).map_err(|_| SessionTurnFailure::Internal)?;
                    content.push(StoredAssistantContent::ToolCall {
                        item_id,
                        tool_call_id: tool_call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    calls.push((
                        item_id,
                        ToolCall::new(
                            tool_call_id.clone(),
                            name.clone(),
                            arguments.clone(),
                            call_index,
                        ),
                    ));
                }
            }
        }
        let is_tool_round = !calls.is_empty();
        let disposition = if is_tool_round {
            AssistantDisposition::Intermediate
        } else {
            AssistantDisposition::Final
        };
        let body = StoredAssistantMessage::reconstruct(
            disposition,
            content,
            response.model().clone(),
            response.response_id().cloned(),
            response.finish_reason(),
            response.effective_max_output_tokens(),
            response.usage().cloned(),
            logical_retry_count,
            response.metadata().clone(),
        )
        .map_err(|_| SessionTurnFailure::Internal)?;
        let steer = if !is_tool_round {
            arbitrate_one_steer(
                Arc::clone(&conversation),
                turn_id,
                cancellation.clone(),
                &steer_completion_sender,
                true,
                #[cfg(test)]
                Arc::clone(&hooks),
            )
            .await?
        } else {
            SteerArbitration::none()
        };
        if let Some(queued) = steer.queued {
            let intermediate = StoredAssistantMessage::reconstruct(
                AssistantDisposition::Intermediate,
                body.content().to_vec(),
                body.model().clone(),
                body.response_id().cloned(),
                body.finish_reason(),
                body.effective_max_output_tokens(),
                body.usage().cloned(),
                body.logical_retry_count(),
                body.metadata().clone(),
            )
            .map_err(|_| SessionTurnFailure::Internal)?;
            let assistant_fact = lock(&conversation.live_state)
                .apply_assistant_message(intermediate, turn_id, SystemClock.now())
                .map_err(|_| SessionTurnFailure::Internal)?;
            let _ = conversation
                .recorder
                .record(Arc::clone(assistant_fact.entry()))
                .await;
            if let Some(steer) = resolve_one_steer(
                Arc::clone(&context),
                Arc::clone(&conversation),
                turn_id,
                queued,
                assistant_fact.revision(),
                cancellation.clone(),
            )
            .await?
            {
                let steer_fact = lock(&conversation.live_state)
                    .apply_user_message(steer, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?;
                let _ = conversation
                    .recorder
                    .record(Arc::clone(steer_fact.entry()))
                    .await;
            }
            continue;
        }
        let fact = {
            let mut live_state = lock(&conversation.live_state);
            if is_tool_round {
                live_state
                    .apply_assistant_message(body, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?
            } else {
                live_state
                    .complete_with_assistant_message(body, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?
            }
        };
        let entry = Arc::clone(fact.entry());
        if !is_tool_round {
            return Ok(entry);
        }
        let _ = conversation.recorder.record(entry).await;

        let requests = calls
            .into_iter()
            .map(|(item_id, call)| ToolExecutionRequest::new(item_id, call))
            .collect::<Vec<_>>();
        let tool_results = context.tool_set().execute_round(requests).await;
        let mut abandoned = false;
        for outcome in tool_results {
            let outcome = match outcome {
                ToolExecutionOutcome::Interaction {
                    item_id,
                    tool_call_id,
                    request_id,
                    request,
                    resolution_sender,
                    resolution_receiver,
                    allowed,
                    denied,
                } => {
                    let completion = InteractionRequestedCompletion {
                        turn_id,
                        timestamp: SystemClock.now(),
                        item_id,
                        tool_call_id: tool_call_id.clone(),
                        request_id,
                        request,
                        resolution_sender,
                    };
                    if interaction_completion_sender
                        .send(ExecutorCompletion::InteractionRequested(completion))
                        .is_err()
                    {
                        ToolExecutionOutcome::Abandoned {
                            item_id,
                            tool_call_id,
                            reason: crate::tools::ToolAbandonReason::RuntimeFailure,
                        }
                    } else {
                        let resolution = resolution_receiver.await;
                        match resolution {
                            Ok(resolution) => ToolSet::settle_interaction(
                                item_id,
                                tool_call_id,
                                *allowed,
                                *denied,
                                resolution,
                            ),
                            Err(_) => ToolExecutionOutcome::Abandoned {
                                item_id,
                                tool_call_id,
                                reason: crate::tools::ToolAbandonReason::RuntimeFailure,
                            },
                        }
                    }
                }
                outcome => outcome,
            };
            let (item_id, tool_call_id, stored) = stored_tool_outcome(outcome)?;
            abandoned |= matches!(&stored, StoredToolOutcome::Abandoned { .. });
            let fact = lock(&conversation.live_state)
                .apply_tool_message(
                    StoredToolMessage::reconstruct(item_id, tool_call_id, stored),
                    turn_id,
                    SystemClock.now(),
                )
                .map_err(|_| SessionTurnFailure::Internal)?;
            let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        }
        if abandoned {
            lock(&conversation.live_state)
                .abandon_current_tool_exchange(turn_id)
                .map_err(|_| SessionTurnFailure::Internal)?;
            return Err(SessionTurnFailure::Model);
        }
        if cancellation.is_cancelled() {
            return Err(SessionTurnFailure::Model);
        }
        let steer = arbitrate_one_steer(
            Arc::clone(&conversation),
            turn_id,
            cancellation.clone(),
            &steer_completion_sender,
            false,
            #[cfg(test)]
            Arc::clone(&hooks),
        )
        .await?;
        if let Some(queued) = steer.queued {
            if let Some(steer) = resolve_one_steer(
                Arc::clone(&context),
                Arc::clone(&conversation),
                turn_id,
                queued,
                steer.basis_revision,
                cancellation.clone(),
            )
            .await?
            {
                let fact = lock(&conversation.live_state)
                    .apply_user_message(steer, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?;
                let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "retry policy receives the immutable call plus its owner-local cancellation basis"
)]
async fn call_agent_run_with_logical_retry(
    model_gateway: &ModelGateway,
    request: Arc<ModelCallRequest>,
    conversation: &LoadedSessionConversation,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    cancellation: CancellationToken,
    executor_closing: CancellationToken,
    closing: CancellationToken,
) -> Result<(ModelCallResult, u8), ModelCallError> {
    let mut logical_retries = 0_u8;
    loop {
        if cancellation.is_cancelled() || closing.is_cancelled() {
            return Err(ModelCallError::cancelled());
        }
        if !retry_basis_is_current(
            conversation,
            turn_id,
            &control_generation,
            request.source_revision(),
        ) {
            return Err(ModelCallError::cancelled());
        }
        let result = model_gateway
            .generate_model_turn(
                Arc::clone(&request),
                ModelProgressPublisher::discard(),
                cancellation.clone(),
            )
            .await;
        let error = match result {
            Ok(result) => {
                if cancellation.is_cancelled()
                    || closing.is_cancelled()
                    || !retry_basis_is_current(
                        conversation,
                        turn_id,
                        &control_generation,
                        request.source_revision(),
                    )
                {
                    return Err(ModelCallError::cancelled());
                }
                return Ok((result, logical_retries));
            }
            Err(error) => error,
        };
        let Some(delay) = agent_run_retry_delay(&error, usize::from(logical_retries)) else {
            return Err(error);
        };
        if !retry_basis_is_current(
            conversation,
            turn_id,
            &control_generation,
            request.source_revision(),
        ) {
            return Err(error);
        }
        logical_retries += 1;
        tokio::select! {
            biased;
            _ = executor_closing.cancelled() => {
                return Err(error);
            }
            _ = closing.cancelled() => {
                return Err(error);
            }
            _ = cancellation.cancelled() => {
                return Err(error);
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn retry_basis_is_current(
    conversation: &LoadedSessionConversation,
    turn_id: TurnId,
    control_generation: &Arc<ControlGeneration>,
    source_revision: crate::live_conversation::ConversationRevision,
) -> bool {
    if !conversation.has_control_generation(turn_id, control_generation) {
        return false;
    }
    let live_state = lock(&conversation.live_state);
    if live_state.current_turn() != Some(turn_id) {
        return false;
    }
    live_state
        .capture_conversation_views()
        .ok()
        .is_some_and(|views| views.conversation().revision() == source_revision)
}

fn agent_run_retry_delay(
    error: &ModelCallError,
    logical_retries: usize,
) -> Option<std::time::Duration> {
    let backoff = AGENT_RUN_RETRY_BACKOFFS.get(logical_retries).copied()?;
    if !matches!(
        error.delivery(),
        ProviderRequestDeliveryState::NotSent
            | ProviderRequestDeliveryState::RejectedBeforeExecution
    ) {
        return None;
    }
    match error.reason() {
        ModelCallErrorReason::Timeout
        | ModelCallErrorReason::TransportUnavailable
        | ModelCallErrorReason::ProviderUnavailable => Some(backoff),
        ModelCallErrorReason::RateLimited => error
            .retry_after()
            .filter(|hint| *hint <= std::time::Duration::from_secs(60))
            .map(|hint| backoff.max(hint)),
        _ => None,
    }
}

struct SteerArbitration {
    basis_revision: crate::live_conversation::ConversationRevision,
    queued: Option<QueuedSteer>,
}

impl SteerArbitration {
    fn none() -> Self {
        Self {
            basis_revision: crate::live_conversation::ConversationRevision::default(),
            queued: None,
        }
    }
}

async fn arbitrate_one_steer(
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    cancellation: CancellationToken,
    steer_completion_sender: &mpsc::UnboundedSender<ExecutorCompletion>,
    close_if_empty: bool,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> Result<SteerArbitration, SessionTurnFailure> {
    #[cfg(test)]
    hooks.before_steer_safe_point().await;
    let basis_revision = lock(&conversation.live_state)
        .capture_conversation_views()
        .map_err(|_| SessionTurnFailure::Internal)?
        .conversation()
        .revision();
    let (response, waiter) = oneshot::channel();
    steer_completion_sender
        .send(ExecutorCompletion::SteerSafePoint(
            SteerSafePointCompletion {
                turn_id,
                response,
                close_if_empty,
            },
        ))
        .map_err(|_| SessionTurnFailure::Internal)?;
    let Some(queued) = (tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(SessionTurnFailure::Model),
        result = waiter => result.map_err(|_| SessionTurnFailure::Internal)?,
    }) else {
        return Ok(SteerArbitration {
            basis_revision,
            queued: None,
        });
    };
    if queued.turn_id() != turn_id {
        return Err(SessionTurnFailure::Internal);
    }
    Ok(SteerArbitration {
        basis_revision,
        queued: Some(queued),
    })
}

async fn resolve_one_steer(
    context: Arc<TurnExecutionContext>,
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    queued: QueuedSteer,
    basis_revision: crate::live_conversation::ConversationRevision,
    cancellation: CancellationToken,
) -> Result<Option<StoredUserMessage>, SessionTurnFailure> {
    let (_command_id, queued_turn_id, intent) = queued.into_parts();
    if queued_turn_id != turn_id {
        return Err(SessionTurnFailure::Internal);
    }
    let message = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(SessionTurnFailure::Model),
        result = context.resolve_user_message(intent) => result.map_err(map_turn_prompt_error)?,
    };
    if cancellation.is_cancelled() {
        return Err(SessionTurnFailure::Model);
    }
    let current_revision = lock(&conversation.live_state)
        .capture_conversation_views()
        .map_err(|_| SessionTurnFailure::Internal)?
        .conversation()
        .revision();
    if current_revision != basis_revision {
        return Ok(None);
    }
    let item_id = ItemId::generate().map_err(|_| SessionTurnFailure::Internal)?;
    Ok(Some(StoredUserMessage::reconstruct(
        item_id,
        UserMessageSource::Steer,
        message,
    )))
}

fn map_model_call_failure(error: crate::model_gateway::ModelCallError) -> SessionTurnFailure {
    match error.reason() {
        ModelCallErrorReason::ContextOverflow => SessionTurnFailure::ContextOverflow,
        ModelCallErrorReason::Cancelled
        | ModelCallErrorReason::ModelUnavailable
        | ModelCallErrorReason::AuthMissing
        | ModelCallErrorReason::AuthRejected
        | ModelCallErrorReason::RateLimited
        | ModelCallErrorReason::QuotaExceeded
        | ModelCallErrorReason::UnsupportedCapability
        | ModelCallErrorReason::InvalidRequest
        | ModelCallErrorReason::SafetyBlocked
        | ModelCallErrorReason::Timeout
        | ModelCallErrorReason::TransportUnavailable
        | ModelCallErrorReason::ProviderUnavailable
        | ModelCallErrorReason::ProviderRejected
        | ModelCallErrorReason::RequestOutcomeUnknown
        | ModelCallErrorReason::StreamInterrupted
        | ModelCallErrorReason::UnexpectedToolCall
        | ModelCallErrorReason::InvalidStructuredOutput
        | ModelCallErrorReason::InvalidProviderResponse
        | ModelCallErrorReason::IncompleteResponse => SessionTurnFailure::Model,
    }
}

fn stored_tool_outcome(
    outcome: ToolExecutionOutcome,
) -> Result<(ItemId, crate::tools::ToolCallId, StoredToolOutcome), SessionTurnFailure> {
    match outcome {
        ToolExecutionOutcome::Completed {
            item_id,
            tool_call_id,
            source,
            disposition,
            content,
        } => StoredToolOutcome::completed(source, disposition, content)
            .map(|stored| (item_id, tool_call_id, stored))
            .map_err(|_| SessionTurnFailure::Internal),
        ToolExecutionOutcome::Abandoned {
            item_id,
            tool_call_id,
            reason,
        } => Ok((
            item_id,
            tool_call_id,
            StoredToolOutcome::Abandoned { reason },
        )),
        ToolExecutionOutcome::Interaction { .. } => Err(SessionTurnFailure::Internal),
    }
}

fn map_agent_definition_read_error(error: DurableAgentDefinitionReadError) -> SessionSubmitError {
    match error {
        DurableAgentDefinitionReadError::Closing => SessionSubmitError::Closing,
        DurableAgentDefinitionReadError::AgentNotFound
        | DurableAgentDefinitionReadError::RevisionUnavailable => {
            SessionSubmitError::AgentUnavailable
        }
        DurableAgentDefinitionReadError::StorageUnavailable => {
            SessionSubmitError::DependencyUnavailable
        }
        DurableAgentDefinitionReadError::InternalDispatchUnavailable => {
            SessionSubmitError::InternalDispatchUnavailable
        }
    }
}

fn map_turn_context_capture_error(error: TurnContextCaptureError) -> SessionSubmitError {
    match error {
        TurnContextCaptureError::InvalidBinding => SessionSubmitError::InternalDispatchUnavailable,
        TurnContextCaptureError::Model(_) => SessionSubmitError::DependencyUnavailable,
        TurnContextCaptureError::Prompt => SessionSubmitError::Prompt,
    }
}

fn map_submit_prompt_error(error: PromptError) -> SessionSubmitError {
    match error.kind() {
        PromptErrorKind::ContextLimitExceeded => SessionSubmitError::ContextOverflow,
        PromptErrorKind::SourceDiscovery
        | PromptErrorKind::ContentLoad
        | PromptErrorKind::DuplicateKey
        | PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing => SessionSubmitError::Prompt,
        PromptErrorKind::InvalidIntent | PromptErrorKind::InvalidContribution => {
            SessionSubmitError::InvalidArgument
        }
    }
}

fn map_agent_admission_error(error: AgentAdmissionError) -> SessionSubmitError {
    match error {
        AgentAdmissionError::Closing => SessionSubmitError::Closing,
        AgentAdmissionError::AgentUnavailable => SessionSubmitError::AgentUnavailable,
    }
}

fn map_id_generation_error(_error: IdGenerationError) -> SessionSubmitError {
    SessionSubmitError::InternalDispatchUnavailable
}

fn map_turn_prompt_error(error: PromptError) -> SessionTurnFailure {
    match error.kind() {
        PromptErrorKind::ContextLimitExceeded => SessionTurnFailure::ContextOverflow,
        PromptErrorKind::SourceDiscovery
        | PromptErrorKind::ContentLoad
        | PromptErrorKind::DuplicateKey
        | PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing
        | PromptErrorKind::InvalidIntent
        | PromptErrorKind::InvalidContribution => SessionTurnFailure::Prompt,
    }
}

fn map_model_request_error(
    error: crate::model_gateway::ModelRequestValidationError,
) -> SessionTurnFailure {
    match error.kind() {
        ModelRequestValidationErrorKind::AssemblyMismatch
        | ModelRequestValidationErrorKind::InvalidOutputLimit
        | ModelRequestValidationErrorKind::UnsupportedInput => SessionTurnFailure::Internal,
    }
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

struct SubmitRequest {
    command_id: CommandId,
    intent: Option<PromptIntent>,
    response: Option<oneshot::Sender<Result<TurnId, SessionSubmitError>>>,
}

struct FollowUpRequest {
    command_id: CommandId,
    intent: Option<PromptIntent>,
    response: Option<oneshot::Sender<Result<(), SessionFollowUpError>>>,
}

struct SteerRequest {
    turn_id: TurnId,
    command_id: CommandId,
    intent: Option<PromptIntent>,
    response: Option<oneshot::Sender<Result<(), SessionSteerError>>>,
}

struct CancelQueuedMessageRequest {
    command_id: CommandId,
    response: Option<oneshot::Sender<Result<(), SessionQueuedMessageError>>>,
}

impl FollowUpRequest {
    fn settle(&mut self, outcome: Result<(), SessionFollowUpError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionFollowUpError::Closing));
    }
}

impl Drop for FollowUpRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl SteerRequest {
    fn settle(&mut self, outcome: Result<(), SessionSteerError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionSteerError::Closing));
    }
}

impl Drop for SteerRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl CancelQueuedMessageRequest {
    fn settle(&mut self, outcome: Result<(), SessionQueuedMessageError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionQueuedMessageError::Closing));
    }
}

impl Drop for CancelQueuedMessageRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct ResolveInteractionRequest {
    request_id: RequestId,
    resolution: Option<ResolvedInteraction>,
    timestamp: Timestamp,
    response: Option<oneshot::Sender<Result<(), SessionInteractionError>>>,
}

struct CancelRequest {
    target: SessionCancelTarget,
    timestamp: Timestamp,
    response: Option<oneshot::Sender<Result<(), SessionCancelError>>>,
}

impl CancelRequest {
    fn settle(&mut self, outcome: Result<(), SessionCancelError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionCancelError::Closing));
    }
}

impl Drop for CancelRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl ResolveInteractionRequest {
    fn settle(&mut self, outcome: Result<(), SessionInteractionError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionInteractionError::Closing));
    }
}

impl Drop for ResolveInteractionRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct SubscribeRequest {
    response:
        Option<oneshot::Sender<Result<SessionExecutorSubscription, SessionExecutorSnapshotError>>>,
}

impl SubscribeRequest {
    fn settle(
        &mut self,
        outcome: Result<SessionExecutorSubscription, SessionExecutorSnapshotError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionExecutorSnapshotError::Closing));
    }
}

impl Drop for SubscribeRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl SubmitRequest {
    fn settle(&mut self, outcome: Result<TurnId, SessionSubmitError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionSubmitError::Closing));
    }
}

impl Drop for SubmitRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
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
    Submit(SubmitRequest),
    FollowUp(FollowUpRequest),
    Steer(SteerRequest),
    CancelQueuedMessage(CancelQueuedMessageRequest),
    ResolveInteraction(ResolveInteractionRequest),
    Cancel(CancelRequest),
    Subscribe(SubscribeRequest),
    #[cfg(test)]
    StartingProbe(StartingProbeRequest),
}

impl SessionExecutorRequest {
    fn reject_closing(&mut self) {
        match self {
            Self::Update(request) => request.reject_closing(),
            Self::Snapshot(request) => request.reject_closing(),
            Self::Submit(request) => request.reject_closing(),
            Self::FollowUp(request) => request.reject_closing(),
            Self::Steer(request) => request.reject_closing(),
            Self::CancelQueuedMessage(request) => request.reject_closing(),
            Self::ResolveInteraction(request) => request.reject_closing(),
            Self::Cancel(request) => request.reject_closing(),
            Self::Subscribe(request) => request.reject_closing(),
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
    turn_admission_gate: Arc<TurnAdmissionGate>,
    armed: bool,
}

impl ActorExitGuard {
    fn new(
        closing: CancellationToken,
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        failure_state: Arc<ActorFailureState>,
        turn_admission_gate: Arc<TurnAdmissionGate>,
    ) -> Self {
        Self {
            closing,
            task_context,
            durable_state,
            failure_state,
            turn_admission_gate,
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
        self.turn_admission_gate.close();
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
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    permit: Option<SessionDefinitionPublicationPermit>,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    settled: bool,
}

struct AdmissionCompletionGuard {
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    turn_id: TurnId,
    completed: bool,
}

impl AdmissionCompletionGuard {
    fn new(completion_sender: mpsc::UnboundedSender<ExecutorCompletion>, turn_id: TurnId) -> Self {
        Self {
            completion_sender,
            turn_id,
            completed: false,
        }
    }

    fn complete(&mut self, result: Result<Arc<TurnExecutionContext>, SessionSubmitError>) {
        self.completed = true;
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Admission(AdmissionCompletion {
                turn_id: self.turn_id,
                result,
            }));
    }
}

impl Drop for AdmissionCompletionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Admission(AdmissionCompletion {
                turn_id: self.turn_id,
                result: Err(SessionSubmitError::InternalDispatchUnavailable),
            }));
    }
}

struct TurnCompletionGuard {
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    turn_id: TurnId,
    completed: bool,
}

impl TurnCompletionGuard {
    fn new(completion_sender: mpsc::UnboundedSender<ExecutorCompletion>, turn_id: TurnId) -> Self {
        Self {
            completion_sender,
            turn_id,
            completed: false,
        }
    }

    fn complete(&mut self, terminal: SessionTurnTerminal) {
        self.completed = true;
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Turn(TurnCompletion {
                turn_id: self.turn_id,
                terminal,
            }));
    }
}

impl Drop for TurnCompletionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Turn(TurnCompletion {
                turn_id: self.turn_id,
                terminal: SessionTurnTerminal::Failed(SessionTurnFailure::Internal),
            }));
    }
}

impl PublicationCompletionGuard {
    fn new(
        completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
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
        let _ =
            self.completion_sender
                .send(ExecutorCompletion::Publication(PublicationCompletion {
                    permit,
                    result,
                }));
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
        let _ =
            self.completion_sender
                .send(ExecutorCompletion::Publication(PublicationCompletion {
                    permit,
                    result,
                }));
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
    pub(crate) fn arm_before_snapshot_response(&self) {
        self.inner.before_snapshot_response.arm();
    }

    pub(crate) async fn wait_before_snapshot_response(&self) {
        self.inner
            .before_snapshot_response
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_before_snapshot_response(&self) {
        self.inner.before_snapshot_response.release();
    }

    pub(crate) fn arm_after_agent_admission_before_input(&self) {
        self.inner.after_agent_admission_before_input.arm();
    }

    pub(crate) async fn wait_after_agent_admission_before_input(&self) {
        self.inner
            .after_agent_admission_before_input
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_after_agent_admission_before_input(&self) {
        self.inner.after_agent_admission_before_input.release();
    }

    pub(crate) fn arm_before_agent_run_attempt(&self) {
        self.inner.before_agent_run_attempt.arm();
    }

    pub(crate) async fn wait_before_agent_run_attempt(&self) {
        self.inner
            .before_agent_run_attempt
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_before_agent_run_attempt(&self) {
        self.inner.before_agent_run_attempt.release();
    }

    pub(crate) fn arm_before_steer_safe_point(&self) {
        self.inner.before_steer_safe_point.arm();
    }

    pub(crate) async fn wait_before_steer_safe_point(&self) {
        self.inner
            .before_steer_safe_point
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_before_steer_safe_point(&self) {
        self.inner.before_steer_safe_point.release();
    }

    pub(crate) fn arm_after_steer_arbitration(&self) {
        self.inner.after_steer_arbitration.arm();
    }

    pub(crate) async fn wait_after_steer_arbitration(&self) {
        self.inner
            .after_steer_arbitration
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_after_steer_arbitration(&self) {
        self.inner.after_steer_arbitration.release();
    }

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
    before_snapshot_response: Arc<NamedAsyncBarrier>,
    after_agent_admission_before_input: Arc<NamedAsyncBarrier>,
    before_agent_run_attempt: Arc<NamedAsyncBarrier>,
    before_steer_safe_point: Arc<NamedAsyncBarrier>,
    after_steer_arbitration: Arc<NamedAsyncBarrier>,
    after_snapshot_finish: Arc<NamedAsyncBarrier>,
    after_commit_before_install: Arc<NamedAsyncBarrier>,
    settled: Arc<SettlementNotification>,
    fail_next_install_after_commit: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl SessionExecutorTestHooksInner {
    fn new() -> Self {
        Self {
            before_snapshot_response: Arc::new(NamedAsyncBarrier::new()),
            after_agent_admission_before_input: Arc::new(NamedAsyncBarrier::new()),
            before_agent_run_attempt: Arc::new(NamedAsyncBarrier::new()),
            before_steer_safe_point: Arc::new(NamedAsyncBarrier::new()),
            after_steer_arbitration: Arc::new(NamedAsyncBarrier::new()),
            after_snapshot_finish: Arc::new(NamedAsyncBarrier::new()),
            after_commit_before_install: Arc::new(NamedAsyncBarrier::new()),
            settled: Arc::new(SettlementNotification::new()),
            fail_next_install_after_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    async fn before_snapshot_response(&self) {
        self.before_snapshot_response.wait_if_armed().await;
    }

    async fn after_agent_admission_before_input(&self) {
        self.after_agent_admission_before_input
            .wait_if_armed()
            .await;
    }

    async fn before_agent_run_attempt(&self) {
        self.before_agent_run_attempt.wait_if_armed().await;
    }

    async fn before_steer_safe_point(&self) {
        self.before_steer_safe_point.wait_if_armed().await;
    }

    async fn after_steer_arbitration(&self) {
        self.after_steer_arbitration.wait_if_armed().await;
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

    use crate::conversation_storage::load_replayed_conversation_with_barrier_for_test;
    use crate::durable_state::DurableState;
    use crate::model_gateway::ScriptedModelFixture;
    use crate::prompt::{PromptBodyIntent, TextIntent};
    use crate::runtime_task::RuntimeTaskContext;
    use crate::wire::{CanonicalFileUri, FileUriFamily, RequestId, SessionId};
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
        lifecycle_closing: CancellationToken,
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
            lifecycle_closing: CancellationToken::new(),
        }
    }

    async fn scripted_text_fixture(
        store: &TempStore,
        model: &ScriptedModelFixture,
    ) -> LoadedFixture {
        for (path, from, to) in [
            (
                store
                    .root
                    .join("agents")
                    .join(AGENT_ID)
                    .join("generations")
                    .join(G1)
                    .join("definition.json"),
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
            (
                store
                    .session_path()
                    .join("generations")
                    .join(G1)
                    .join("definition.json"),
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        ] {
            let bytes = fs::read(&path).expect("the fixture definition is readable");
            create_file(&path, &replace_fixture(&bytes, from, to));
        }

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state
            .session_current_definition(session_id)
            .expect("the fixture Session definition is current");
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .expect("the fixture Workspace resolves")
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let lifecycle_closing = CancellationToken::new();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources_and_lifecycle(
            SessionExecutorDependencies::with_turn_resources(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
            ),
            Arc::clone(&definition),
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
            lifecycle_closing.clone(),
        )
        .unwrap();
        LoadedFixture {
            context,
            state,
            executor,
            definition,
            lifecycle_closing,
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

    async fn wait_for_terminal(executor: &SessionExecutor, turn_id: TurnId) -> SessionTurnTerminal {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = executor.snapshot().await.unwrap();
                if let Some((completed_turn, terminal)) = snapshot.last_terminal() {
                    assert_eq!(completed_turn, turn_id);
                    return terminal;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the scripted Turn reaches terminal state")
    }

    async fn wait_for_request_count(model: &ScriptedModelFixture, expected: usize) {
        for _ in 0..100_000 {
            if model.request_count() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the scripted provider did not reach the expected attempt count");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_scripted_turn_records_and_replays_user_and_final_assistant() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::new(vec!["scripted answer"]);
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let candidate = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap();
        let workspace = candidate.finish(Arc::from([]), Arc::from([])).unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources(
                context.clone(),
                state.clone(),
                Arc::clone(&resolver),
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
            ),
            Arc::clone(&definition),
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();
        let mut subscription = executor.subscribe().await.unwrap();
        assert_eq!(
            subscription.snapshot().execution_state(),
            SessionExecutionState::Idle
        );
        let command_id = CommandId::generate().unwrap();
        let turn_id = executor
            .submit(
                command_id,
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("hello runtime").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.command_id(), command_id);
        assert_eq!(event.turn_id(), turn_id);
        assert_eq!(event.terminal(), SessionTurnTerminal::Completed);
        assert_eq!(event.snapshot().current_turn(), None);
        assert_eq!(
            event.snapshot().execution_state(),
            SessionExecutionState::Idle
        );
        assert_eq!(model.request_count(), 1);
        let live_state = executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            assert_eq!(live.current_turn(), None);
            assert_eq!(
                live.capture_conversation_views()
                    .unwrap()
                    .conversation()
                    .messages()
                    .len(),
                2
            );
        }
        executor.close().await.unwrap();

        let replayed = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(replayed.live_state.current_turn(), None);
        assert_eq!(
            replayed
                .live_state
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            2
        );

        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_service.initialize().await.unwrap(),
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                replayed.live_state,
                replayed.recorder,
                replayed.diagnostics,
            ),
        )
        .unwrap();
        let failed_turn = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("second request").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            wait_for_terminal(&executor, failed_turn).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 2);
        let live_state = executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            assert_eq!(live.current_turn(), None);
            assert_eq!(
                live.capture_conversation_views()
                    .unwrap()
                    .conversation()
                    .messages()
                    .len(),
                3
            );
        }
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn agent_run_retries_delivery_safe_transient_with_same_request_arc() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["retry succeeded"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("retry me").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        let revision_before_backoff = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert!(Arc::ptr_eq(&requests[0], &requests[1]));
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the retry result is recorded");
        assert!(recording.contains(r#""logicalRetryCount":1"#));
        let revision_after_retry = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_ne!(revision_before_backoff, revision_after_retry);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn agent_run_does_not_retry_unknown_or_stream_interrupted_failures() {
        for reason in [
            ModelCallErrorReason::RequestOutcomeUnknown,
            ModelCallErrorReason::StreamInterrupted,
        ] {
            let store = TempStore::new();
            let model = ScriptedModelFixture::with_failure_reasons_then_responses(
                vec![reason],
                vec!["must not run"],
            );
            let loaded = scripted_text_fixture(&store, &model).await;
            let turn_id = loaded
                .executor
                .submit(
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("do not retry").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                wait_for_terminal(&loaded.executor, turn_id).await,
                SessionTurnTerminal::Failed(SessionTurnFailure::Model)
            );
            assert_eq!(model.request_count(), 1);
            close_loaded(loaded).await;
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn agent_run_retry_exhaustion_stops_after_four_gateway_attempts() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![
                ModelCallErrorReason::Timeout,
                ModelCallErrorReason::TransportUnavailable,
                ModelCallErrorReason::ProviderUnavailable,
                ModelCallErrorReason::Timeout,
            ],
            Vec::new(),
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("exhaust retries").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        wait_for_request_count(&model, 2).await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        wait_for_request_count(&model, 3).await;
        tokio::time::advance(std::time::Duration::from_secs(8)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 4);
        let requests = model.requests();
        assert_eq!(requests.len(), 4);
        assert!(
            requests
                .windows(2)
                .all(|pair| Arc::ptr_eq(&pair[0], &pair[1]))
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_during_agent_run_retry_backoff_sends_no_extra_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("cancel retry").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        wait_for_request_count(&model, 1).await;
        assert_eq!(
            loaded
                .executor
                .cancel(
                    SessionCancelTarget::Turn(turn_id),
                    "2026-08-08T10:03:00.000Z".parse().unwrap(),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_retry_basis_stops_before_the_next_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("stale retry").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        let revision = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_eq!(
            loaded
                .executor
                .retry_basis_matches_for_test(turn_id, revision),
            Some(true)
        );
        assert_eq!(
            loaded
                .executor
                .retry_basis_matches_for_test(turn_id, revision.checked_next().unwrap()),
            Some(false)
        );
        loaded
            .executor
            .invalidate_control_generation_for_test(turn_id);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn executor_lifecycle_close_interrupts_retry_backoff() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let _turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("close retry").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        loaded.lifecycle_closing.cancel();
        assert!(loaded.executor.close().await.is_ok());
        assert_eq!(model.request_count(), 1);
        assert_eq!(loaded.executor.published_snapshot().current_turn(), None);
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executor_close_after_turn_admission_still_runs_first_model_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["admitted attempt"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("close after admission").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        hooks.wait_before_agent_run_attempt().await;
        assert_eq!(model.request_count(), 0);
        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        assert_eq!(model.request_count(), 0);
        hooks.release_before_agent_run_attempt();
        assert!(close.await.is_ok());
        assert_eq!(model.request_count(), 1);
        assert_eq!(
            loaded.executor.published_snapshot().last_terminal(),
            Some((turn_id, SessionTurnTerminal::Completed))
        );
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn steer_queued_during_agent_run_retry_backoff_is_consumed_after_success() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["retry candidate", "after steer"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("initial").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        wait_for_request_count(&model, 1).await;
        let revision_before_steer = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_eq!(
            loaded
                .executor
                .steer(
                    turn_id,
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("queued steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        let revision_after_steer = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_eq!(revision_before_steer, revision_after_steer);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 3);
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("queued steer"));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn text_candidate_steer_wins_and_records_continue_before_steer() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["candidate", "answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_steer_safe_point();

        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("hello").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        hooks.wait_before_steer_safe_point().await;

        let steer_executor = loaded.executor.clone();
        let steer = tokio::spawn(async move {
            steer_executor
                .steer(
                    turn_id,
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("focus").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
        });
        assert_eq!(steer.await.unwrap(), Ok(()));
        hooks.release_before_steer_safe_point();

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);
        let live_state = loaded.executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            let captured = live.capture_conversation_views().unwrap();
            let messages = captured.conversation().messages();
            assert_eq!(messages.len(), 4);
            assert!(matches!(
                messages[1].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { .. }
            ));
            assert!(matches!(
                messages[2].as_ref(),
                crate::prompt::ModelMessageRef::User { .. }
            ));
            assert!(matches!(
                messages[3].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { .. }
            ));
        }

        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the scripted conversation recording is readable");
        let intermediate = recording
            .find(r#""disposition":"intermediate""#)
            .expect("the candidate is recorded as Intermediate");
        let steer = recording
            .find(r#""source":"steer""#)
            .expect("the Steer is recorded");
        let final_assistant = recording
            .rfind(r#""disposition":"final""#)
            .expect("the final assistant is recorded");
        assert!(intermediate < steer && steer < final_assistant);

        loaded.executor.close().await.unwrap();
        loaded.state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn text_candidate_final_reservation_wins_and_closes_steer_admission() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["candidate"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_steer_arbitration();

        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("hello").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        hooks.wait_after_steer_arbitration().await;

        let live_state = loaded.executor.live_state_for_test().unwrap();
        assert_eq!(
            lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            1,
            "final reservation does not mutate the live conversation"
        );

        let steer_executor = loaded.executor.clone();
        let mut steer = Box::pin(
            steer_executor.steer(
                turn_id,
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("late focus").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            ),
        );
        assert!(poll_once_pending(steer.as_mut()).await);
        hooks.release_after_steer_arbitration();
        assert_eq!(steer.await, Err(SessionSteerError::TurnNotRunning));

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);
        {
            let live = lock(&live_state);
            let captured = live.capture_conversation_views().unwrap();
            let messages = captured.conversation().messages();
            assert_eq!(messages.len(), 2);
        }
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the scripted conversation recording is readable");
        assert_eq!(recording.matches(r#""disposition":"final""#).count(), 1);
        assert!(!recording.contains(r#""source":"steer""#));

        loaded.executor.close().await.unwrap();
        loaded.state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_scripted_turn_consumes_steer_after_a_tool_round_before_the_final_model_call()
    {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::with_tool_round(
            "call_echo",
            "echo",
            "{\"value\":1}",
            "tool complete",
        );
        let tool_started = Arc::new(tokio::sync::Notify::new());
        let release_tool = Arc::new(tokio::sync::Notify::new());
        let tool_started_for_executor = Arc::clone(&tool_started);
        let release_tool_for_executor = Arc::clone(&release_tool);
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Parallel,
                )
                .unwrap(),
            ],
            move |call| {
                let tool_started = Arc::clone(&tool_started_for_executor);
                let release_tool = Arc::clone(&release_tool_for_executor);
                Box::pin(async move {
                    tool_started.notify_one();
                    release_tool.notified().await;
                    ToolExecutionResult::completed_text(call.call().arguments().canonical_json())
                        .unwrap()
                })
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        tool_started.notified().await;
        let consumed_steer_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    consumed_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("steer while tool runs").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        let cross_lane_steer_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    cross_lane_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cross-lane steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .follow_up(
                    cross_lane_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("duplicate follow-up").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Err(SessionFollowUpError::CommandConflict)
        );
        assert_eq!(
            executor
                .cancel_queued_message(cross_lane_steer_command_id)
                .await,
            Ok(())
        );
        let cross_lane_follow_up_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .follow_up(
                    cross_lane_follow_up_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cross-lane follow-up").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    cross_lane_follow_up_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("duplicate steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Err(SessionSteerError::CommandConflict)
        );
        assert_eq!(
            executor
                .cancel_queued_message(cross_lane_follow_up_command_id)
                .await,
            Ok(())
        );
        let cancelled_steer_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    cancelled_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancelled steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        let cancelled_follow_up_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .follow_up(
                    cancelled_follow_up_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancelled follow-up").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .cancel_queued_message(cancelled_steer_command_id)
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .cancel_queued_message(cancelled_follow_up_command_id)
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .cancel_queued_message(cancelled_steer_command_id)
                .await,
            Err(SessionQueuedMessageError::NotQueued)
        );
        release_tool.notify_one();
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);

        let live_state = executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            let captured = live.capture_conversation_views().unwrap();
            let messages = captured.conversation().messages();
            assert_eq!(messages.len(), 5);
            assert!(matches!(
                messages[1].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { content }
                    if matches!(content[0].as_ref(), crate::prompt::ModelAssistantContentRef::ToolCall { .. })
            ));
            assert!(matches!(
                messages[2].as_ref(),
                crate::prompt::ModelMessageRef::Tool { .. }
            ));
            assert!(matches!(
                messages[3].as_ref(),
                crate::prompt::ModelMessageRef::User { .. }
            ));
            assert!(matches!(
                messages[4].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { content }
                    if matches!(content[0].as_ref(), crate::prompt::ModelAssistantContentRef::Text("tool complete"))
            ));
        }
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_tool_interaction_is_snapshot_resolved_and_recorded_before_next_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::with_tool_round(
            "call_approval",
            "echo",
            "{\"value\":1}",
            "denied tool round complete",
        );
        let request_id: RequestId = "req_33333333333333333333333333333333".parse().unwrap();
        let interaction_request =
            InteractionRequest::tool_approval(crate::tools::live_approval_request_fixture());
        let allowed = ToolExecutionResult::completed_text("tool ran").unwrap();
        let denied = ToolExecutionResult::PreExecution {
            disposition: crate::tools::ToolResultDisposition::Denied,
            content: crate::tools::ToolResultContent::from_text_parts(vec![
                "approval denied".to_owned(),
            ])
            .unwrap(),
        };
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            {
                let interaction_request = interaction_request.clone();
                let allowed = allowed.clone();
                let denied = denied.clone();
                move |_| {
                    let interaction_request = interaction_request.clone();
                    let allowed = allowed.clone();
                    let denied = denied.clone();
                    Box::pin(async move {
                        ToolExecutionResult::Interaction {
                            request_id,
                            request: interaction_request,
                            allowed: Box::new(allowed),
                            denied: Box::new(denied),
                        }
                    })
                }
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = executor.snapshot().await.unwrap();
                if snapshot.pending_interactions().len() == 1 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Interaction request is projected");
        assert_eq!(pending.current_turn(), Some(turn_id));
        assert_eq!(pending.pending_interactions()[0].request_id(), &request_id);

        let resolution = interaction_request
            .resolve_host(
                crate::turn_item_interaction::InteractionHostResolutionInput::ToolApproval(
                    crate::tools::ToolApprovalDecisionInput::Deny,
                ),
            )
            .unwrap();
        executor
            .resolve_interaction(
                request_id,
                resolution,
                "2026-08-08T10:00:00.000Z".parse().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);
        assert!(
            executor
                .snapshot()
                .await
                .unwrap()
                .pending_interactions()
                .is_empty()
        );
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("interaction_requested"));
        assert!(recording.contains("interaction_resolved"));
        assert!(recording.contains("pre_execution"));
        assert!(recording.contains("approval denied"));
        assert!(recording.contains("denied"));
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_pending_tool_interaction_settles_cancelled_without_a_followup_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::with_tool_round(
            "call_cancel",
            "echo",
            "{\"value\":1}",
            "must not run",
        );
        let request_id: RequestId = "req_44444444444444444444444444444444".parse().unwrap();
        let interaction_request =
            InteractionRequest::tool_approval(crate::tools::live_approval_request_fixture());
        let allowed = ToolExecutionResult::completed_text("tool ran").unwrap();
        let denied = ToolExecutionResult::PreExecution {
            disposition: crate::tools::ToolResultDisposition::Denied,
            content: crate::tools::ToolResultContent::from_text_parts(vec![
                "approval denied".to_owned(),
            ])
            .unwrap(),
        };
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            {
                let interaction_request = interaction_request.clone();
                let allowed = allowed.clone();
                let denied = denied.clone();
                move |_| {
                    let interaction_request = interaction_request.clone();
                    let allowed = allowed.clone();
                    let denied = denied.clone();
                    Box::pin(async move {
                        ToolExecutionResult::Interaction {
                            request_id,
                            request: interaction_request,
                            allowed: Box::new(allowed),
                            denied: Box::new(denied),
                        }
                    })
                }
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = executor.snapshot().await.unwrap();
                if snapshot.pending_interactions().len() == 1 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Interaction request is projected");
        assert_eq!(pending.current_turn(), Some(turn_id));
        assert_eq!(pending.pending_interactions()[0].request_id(), &request_id);

        let timestamp: Timestamp = "2026-08-08T10:01:00.000Z".parse().unwrap();
        let mismatched_turn = TurnId::generate().unwrap();
        assert_eq!(
            executor
                .cancel(SessionCancelTarget::Turn(mismatched_turn), timestamp)
                .await,
            Err(SessionCancelError::ExpectedTurnMismatch)
        );
        assert_eq!(
            executor
                .cancel(SessionCancelTarget::Turn(turn_id), timestamp)
                .await,
            Ok(())
        );
        assert!(
            executor
                .snapshot()
                .await
                .unwrap()
                .pending_interactions()
                .is_empty()
        );
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        assert_eq!(
            executor
                .cancel(SessionCancelTarget::Turn(turn_id), timestamp)
                .await,
            Err(SessionCancelError::TurnTerminal)
        );
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("interaction_requested"));
        assert!(recording.contains("interaction_resolved"));
        assert!(recording.contains("turn_cancelled"));
        assert!(recording.contains("cancelled"));
        assert!(recording.contains("pre_execution"));
        assert!(!recording.contains("must not run"));
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_submit_before_input_prevents_turn_start_and_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::new(vec!["must not run"]);
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let hooks = executor.test_hooks();
        hooks.arm_after_agent_admission_before_input();
        let command_id = CommandId::generate().unwrap();
        let submit_executor = executor.clone();
        let submit = tokio::spawn(async move {
            submit_executor
                .submit(
                    command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancel before input").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
        });
        hooks.wait_after_agent_admission_before_input().await;
        let timestamp: Timestamp = "2026-08-08T10:02:00.000Z".parse().unwrap();
        assert_eq!(
            executor
                .cancel(SessionCancelTarget::Submit(command_id), timestamp)
                .await,
            Ok(())
        );
        hooks.release_after_agent_admission_before_input();
        assert_eq!(submit.await.unwrap(), Err(SessionSubmitError::Cancelled));
        let snapshot = executor.snapshot().await.unwrap();
        assert_eq!(snapshot.execution_state(), SessionExecutionState::Idle);
        assert_eq!(snapshot.current_turn(), None);
        assert_eq!(model.request_count(), 0);
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abandoned_tool_settlement_ends_the_turn_without_a_followup_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model =
            ScriptedModelFixture::with_tool_round("call_abandoned", "echo", "{}", "must not run");
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            |_| {
                Box::pin(async {
                    ToolExecutionResult::Abandoned {
                        reason: crate::tools::ToolAbandonReason::OutcomeUnknown,
                    }
                })
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        let live_state = executor.live_state_for_test().unwrap();
        assert_eq!(lock(&live_state).current_turn(), None);
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("abandoned"));
        assert!(!recording.contains("must not run"));
        executor.close().await.unwrap();
        state.close().await;
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
    async fn follow_up_is_rejected_without_an_active_turn() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let result = loaded
            .executor
            .follow_up(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("queued later").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(result, Err(SessionFollowUpError::TurnNotRunning));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn steer_is_rejected_without_an_active_turn() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let turn_id: TurnId = "trn_11111111111111111111111111111111".parse().unwrap();
        let result = loaded
            .executor
            .steer(
                turn_id,
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("queued steer").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(result, Err(SessionSteerError::TurnNotRunning));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_message_cancel_reports_not_queued_and_closing() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let command_id: CommandId = "cmd_33333333333333333333333333333333".parse().unwrap();
        assert_eq!(
            loaded.executor.cancel_queued_message(command_id).await,
            Err(SessionQueuedMessageError::NotQueued)
        );

        loaded.executor.request_closing();
        assert_eq!(
            loaded.executor.cancel_queued_message(command_id).await,
            Err(SessionQueuedMessageError::Closing)
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
