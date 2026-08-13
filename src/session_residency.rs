#![allow(
    dead_code,
    reason = "the Runtime-owned residency foundation awaits public command/query routing"
)]

//! The crate-private owner of loaded Session residency.
//!
//! This module deliberately stops at the boundary between durable Session definitions, the
//! Workspace resolver, replay-backed Idle installation with derived readiness, and a loaded
//! [`SessionExecutor`].
//! Conversation Storage retains semantic replay and recording ownership; this registry only keeps
//! their prepared state/recorder alive through publication and routes owner-local Turn commands.
//! It owns no public payload projection or Turn state. `RuntimeInner` retains this registry as one
//! deep resource owner without making any residency permit, gate, executor, or task handle part
//! of the Runtime interface.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration as StdDuration;

use thiserror::Error;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent_session_lifecycle::{
    AgentRevisionRef, AgentStatus, ForkAnchor, ForkSourceKind, SealedSessionAgentUpgradeAttempt,
    SealedSessionDefinitionAttempt, SealedSessionForkAttempt, SealedSessionLifecycleAttempt,
    SealedSessionMetadataAttempt, SessionDefinition, SessionLifecycle, SessionLifecycleDecision,
    SessionLifecycleDecisionError, SessionModelConfig,
};
use crate::compaction::{CompactionSettings, CompactionSettingsSnapshot};
#[cfg(not(test))]
use crate::conversation_storage::load_replayed_conversation;
use crate::conversation_storage::{
    ConversationLoadError, ConversationReplayError, ForkAnchorResolutionError,
};
#[cfg(test)]
use crate::conversation_storage::{
    ReplayPreparationBarrier, load_replayed_conversation_with_barrier_for_test,
};
use crate::durable_state::{
    DurableConversationTargetError, DurableSessionAgentUpgradeError,
    DurableSessionAgentUpgradeOutcome, DurableSessionDefinitionError,
    DurableSessionDefinitionOutcome, DurableSessionForkError, DurableSessionHead,
    DurableSessionLifecycleError, DurableSessionLifecycleOutcome, DurableSessionMetadataError,
    DurableSessionMetadataOutcome, DurableState,
};
use crate::model_gateway::{ModelCatalogView, ModelGateway};
use crate::prompt::{
    PromptError, PromptErrorKind, PromptIntent, PromptResourceView, PromptService,
    SessionPromptSelection,
};
use crate::runtime_interface::{SessionReadinessView, SessionUnavailableView};
use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedTask};
use crate::session_execution::{
    LoadedSessionConversation, PrepareUnloadWaiter, SessionAgentAvailabilityError,
    SessionCancelError, SessionCancelTarget, SessionDefinitionPublicationError,
    SessionDefinitionPublicationOutcome, SessionExecutor, SessionExecutorCloseError,
    SessionExecutorDependencies, SessionExecutorPrepareUnloadError, SessionExecutorSnapshot,
    SessionExecutorSnapshotError, SessionExecutorStartError, SessionExecutorSubscription,
    SessionExecutorTranscriptError, SessionFollowUpError, SessionInteractionError,
    SessionPromptAvailabilityError, SessionQueuedMessageError, SessionSecurityInvalidationError,
    SessionSteerError, SessionSubmitError, SessionWorkspaceDefinitionError,
    model_available_for_definition, prompt_available_for_definition,
};
use crate::session_transcript::SessionTranscriptCapture;
use crate::tools::{ProductionToolConfig, ToolSet, TurnToolResources};
use crate::turn_item_interaction::InteractionResolutionInput;
use crate::wire::{
    AgentId, CommandId, InteractionResolutionKey, ItemId, RequestId, SessionDefinitionRevision,
    SessionId, Timestamp, TurnId,
};
use crate::workspace::{
    CapturedWorkspacePromptSource, CapturedWorkspaceSkillSource, Workspace, WorkspaceResolveError,
    WorkspaceResolver, WorkspaceSnapshotCandidate, WorkspaceSnapshotFinishError,
};

const SESSION_RESIDENCY_REQUEST_QUEUE_CAPACITY: usize = 8;

/// The configured graceful-Unload grace used by every residency Unload and by the registry
/// shutdown broadcast.  The Runtime validates its finite semantics (non-zero, ≤ 5 minutes); test
/// wrappers delegate this default.
const DEFAULT_UNLOAD_GRACE: StdDuration = StdDuration::from_secs(30);

/// The outcome of one admitted Load request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionResidencyLoadOutcome {
    Loaded,
    NoChange,
}

impl SessionResidencyLoadOutcome {
    pub(crate) const fn changed(self) -> bool {
        matches!(self, Self::Loaded)
    }
}

/// Redacted failures from one Session residency Load request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyLoadError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session was not found")]
    SessionNotFound,
    #[error("Session is archived")]
    SessionArchived,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session definition changed while loading")]
    StaleDefinition,
    #[error("workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("workspace candidate was rejected")]
    WorkspaceRejected,
    #[error("recorded conversation state is corrupt")]
    RecordedStateCorrupt,
    #[error("durable state exceeds its selected size limit")]
    DurableStateTooLarge,
    #[error("durable storage is unavailable")]
    StorageUnavailable,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// The outcome of one admitted Unload request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionResidencyUnloadOutcome {
    Unloaded,
    NoChange,
}

impl SessionResidencyUnloadOutcome {
    pub(crate) const fn changed(self) -> bool {
        matches!(self, Self::Unloaded)
    }
}

/// Redacted failures from one Session residency Unload request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyUnloadError {
    #[error("session residency is closing")]
    Closing,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyForkError {
    #[error("session residency is closing")]
    Closing,
    #[error("Fork source Session was not found")]
    SourceNotFound,
    #[error("Fork source Session is deleted")]
    SourceDeleted,
    #[error("Fork anchor is invalid for the selected source path")]
    InvalidAnchor,
    #[error("Fork source conversation exceeds its selected storage limit")]
    SourceConversationTooLarge,
    #[error("Fork source conversation is corrupt")]
    SourceConversationCorrupt,
    #[error("requested Agent is disabled")]
    AgentDisabled,
    #[error("requested Agent is deleted")]
    AgentDeleted,
    #[error("durable state exceeds its selected size limit")]
    DurableStateTooLarge,
    #[error("Fork publication is unavailable")]
    Unavailable,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one loaded Session snapshot request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencySnapshotError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one loaded Session transcript capture request.  The capture is a
/// loaded-only direct route: it clones the installed executor under a short standard mutex and
/// awaits the executor actor's coherent capture, never touching DurableState or the per-Session
/// operation gate.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyTranscriptError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures of one host security Workspace invalidation route.  The host has already
/// published the hard restriction fact; the registry only looks up the loaded executor and
/// awaits its out-of-band recovery API (never the per-Session operation gate).  An executor
/// Closing is only the registry Closing when the registry itself is closing; otherwise it is a
/// normal per-Session Unload / old exact executor race and reports SessionNotLoaded.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencySecurityInvalidationError {
    #[error("runtime is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("runtime dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencySubmitError {
    #[error("session residency is closing")]
    Closing,
    #[error("the Submit command conflicts with an in-flight command")]
    CommandConflict,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("session execution is busy")]
    SessionBusy,
    #[error("Session is not ready to accept Turns: {0:?}")]
    SessionNotReady(SessionUnavailableView),
    #[error("session residency is preparing")]
    Preparing,
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
    #[error("Session authority was revoked before Turn start")]
    Unauthorized,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyInteractionError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("the expected Turn does not match the Interaction owner")]
    ExpectedTurnMismatch,
    #[error("interaction was not found")]
    NotFound,
    #[error("interaction resolution family does not match the pending request")]
    FamilyMismatch,
    #[error("interaction resolution is invalid for the pending request")]
    InvalidResolution,
    #[error("interaction was already resolved by another logical action")]
    AlreadyResolved,
    #[error("interaction resolution conflicts with an existing command")]
    CommandConflict,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionInteractionTarget {
    pub(crate) expected_turn_id: TurnId,
    pub(crate) item_id: ItemId,
    pub(crate) request_id: RequestId,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyCancelError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
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
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyFollowUpError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("session execution has no active Turn")]
    TurnNotRunning,
    #[error("the FollowUp command conflicts with an admitted command")]
    CommandConflict,
    #[error("the FollowUp queue is full")]
    QueueFull,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencySteerError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
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
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyQueuedMessageError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("the queued message is not queued")]
    NotQueued,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencySubscriptionError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("session event publisher is unavailable")]
    PublisherUnavailable,
}

/// Redacted failures from one loaded Workspace definition publication request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyWorkspaceDefinitionError {
    #[error("session residency is closing")]
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
    #[error("durable state exceeds its selected size limit")]
    StateTooLarge,
    #[error("workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("workspace candidate was rejected")]
    WorkspaceRejected,
    #[error("durable storage is unavailable")]
    StorageUnavailable,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one Session lifecycle request routed through residency.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyLifecycleError {
    #[error("session residency is closing")]
    Closing,
    #[error("session execution is busy")]
    SessionBusy,
    #[error("Session was not found")]
    SessionNotFound,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session lifecycle transition is invalid")]
    InvalidLifecycleTransition,
    #[error("durable state exceeds its selected size limit")]
    DurableStateTooLarge,
    #[error("durable storage is unavailable")]
    StorageUnavailable,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one Session Agent revision upgrade request routed through residency.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyAgentUpgradeError {
    #[error("session residency is closing")]
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
    #[error("Session upgrade targets another Agent")]
    AgentMismatch,
    #[error("Agent is disabled")]
    AgentDisabled,
    #[error("Agent is deleted")]
    AgentDeleted,
    #[error("Agent revision is unavailable")]
    RevisionUnavailable,
    #[error("durable state exceeds its selected size limit")]
    DurableStateTooLarge,
    #[error("durable storage is unavailable")]
    StorageUnavailable,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one loaded Session Workspace reload request routed through residency.
/// The reload is a loaded-only operation: it never reads or updates DurableState.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyWorkspaceReloadError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session is not loaded")]
    SessionNotLoaded,
    #[error("session execution is busy")]
    SessionBusy,
    #[error("workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("workspace candidate was rejected")]
    WorkspaceRejected,
    #[error("workspace authority was denied")]
    Unauthorized,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one Session metadata CAS routed through residency.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyMetadataError {
    #[error("session residency is closing")]
    Closing,
    #[error("Session was not found")]
    SessionNotFound,
    #[error("Session metadata compare-and-swap is stale")]
    StaleRevision,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("durable state exceeds its selected size limit")]
    DurableStateTooLarge,
    #[error("durable storage is unavailable")]
    StorageUnavailable,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one per-Session Agent availability fan-out operation.  An executor
/// missing under the gate is an Unload-first NoChange, not an error; an executor that already
/// fatally failed is internal poison.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyAgentAvailabilityError {
    #[error("session residency is closing")]
    Closing,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from one Runtime shared-resource installation fan-out.  The operation is
/// Runtime-scope, not Session-scope: it precomputes availability for every loaded Session and
/// installs the new Prompt/Model roots into every loaded executor and the residency actor's own
/// future Turn resources.  The precompute performs an exact Agent-definition read from
/// DurableState for each loaded Session; it never updates DurableState.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencySharedResourcesError {
    #[error("session residency is closing")]
    Closing,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// The construction failure for the residency actor.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionResidencyStartError {
    #[error("session residency is closing")]
    Closing,
    #[error("session residency dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// A process-local identity retained by one installed loaded Session.
///
/// The value has no SessionId, path, definition, or executor reference.  Only this module compares
/// it, and only by identity, so an old Unload completion can never remove a replacement owner.
#[derive(Clone)]
struct SessionResidencyPermit {
    identity: Arc<SessionResidencyPermitIdentity>,
}

struct SessionResidencyPermitIdentity;

impl SessionResidencyPermit {
    fn new() -> Self {
        Self {
            identity: Arc::new(SessionResidencyPermitIdentity),
        }
    }

    fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl fmt::Debug for SessionResidencyPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionResidencyPermit { .. }")
    }
}

/// The typed async exclusion for one Session residency operation.
///
/// The raw Tokio guard never crosses a module interface and cannot be confused with the
/// longer-lived loaded residency identity retained by `LoadedSession`.
struct SessionResidencyOperationPermit {
    _guard: OwnedMutexGuard<()>,
}

impl SessionResidencyOperationPermit {
    async fn acquire(gate: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self {
            _guard: gate.lock_owned().await,
        }
    }
}

impl fmt::Debug for SessionResidencyOperationPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionResidencyOperationPermit { .. }")
    }
}

/// One loaded Session and its exact owner permit.  This type never crosses the module boundary.
struct LoadedSession {
    executor: SessionExecutor,
    permit: SessionResidencyPermit,
}

/// The process-local residency state shared only with owner-tracked child operations.
///
/// The actor remains the sole creator of LoadedSession values and the sole creator of operation
/// tasks.  Child operations use this short-lock projection because the expensive resolver,
/// executor, and durable awaits must happen outside a `std::sync` guard.
struct ResidencyState {
    loaded: BTreeMap<SessionId, LoadedSession>,
    gates: BTreeMap<SessionId, Arc<tokio::sync::Mutex<()>>>,
}

impl ResidencyState {
    fn new() -> Self {
        Self {
            loaded: BTreeMap::new(),
            gates: BTreeMap::new(),
        }
    }
}

struct ResidencyShared {
    state: Mutex<ResidencyState>,
    #[cfg(test)]
    replay_preparation_barrier: Mutex<Option<Arc<ReplayPreparationBarrier>>>,
}

impl ResidencyShared {
    fn new() -> Self {
        Self {
            state: Mutex::new(ResidencyState::new()),
            #[cfg(test)]
            replay_preparation_barrier: Mutex::new(None),
        }
    }

    fn cancel_admission(&self, closing: &CancellationToken) {
        let _state = lock(&self.state);
        closing.cancel();
    }

    fn gate(&self, session_id: SessionId) -> Arc<tokio::sync::Mutex<()>> {
        let mut state = lock(&self.state);
        Arc::clone(
            state
                .gates
                .entry(session_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    fn has_loaded(&self, session_id: SessionId) -> bool {
        lock(&self.state).loaded.contains_key(&session_id)
    }

    fn executor(&self, session_id: SessionId) -> Option<SessionExecutor> {
        lock(&self.state)
            .loaded
            .get(&session_id)
            .map(|loaded| loaded.executor.clone())
    }

    fn loaded_session_snapshots(&self) -> Vec<Arc<SessionExecutorSnapshot>> {
        let executors = lock(&self.state)
            .loaded
            .values()
            .map(|loaded| loaded.executor.clone())
            .collect::<Vec<_>>();
        executors
            .into_iter()
            .map(|executor| executor.published_snapshot())
            .collect()
    }

    fn install_if_open(
        &self,
        session_id: SessionId,
        executor: SessionExecutor,
        permit: SessionResidencyPermit,
        closing: &CancellationToken,
    ) -> InstallResult {
        let mut state = lock(&self.state);
        if closing.is_cancelled() {
            return InstallResult::Closing;
        }
        if state.loaded.contains_key(&session_id) {
            return InstallResult::AlreadyLoaded;
        }

        state
            .loaded
            .insert(session_id, LoadedSession { executor, permit });

        // Admission cancellation serializes with this short map guard.  Keep the recheck while
        // the guard is held as a defensive exact-install check; the actor's close path cannot
        // observe this transient value until the child has completed.
        if closing.is_cancelled() {
            state.loaded.remove(&session_id);
            InstallResult::Closing
        } else {
            InstallResult::Installed
        }
    }

    fn remove_exact(&self, session_id: SessionId, permit: &SessionResidencyPermit) -> RemoveResult {
        let mut state = lock(&self.state);
        match state.loaded.get(&session_id) {
            None => RemoveResult::Missing,
            Some(loaded) if loaded.permit.same_as(permit) => {
                state.loaded.remove(&session_id);
                RemoveResult::Removed
            }
            Some(_) => RemoveResult::PermitMismatch,
        }
    }

    fn installed_executors(&self) -> Vec<SessionExecutor> {
        lock(&self.state)
            .loaded
            .values()
            .map(|loaded| loaded.executor.clone())
            .collect()
    }

    fn clear(&self) {
        let mut state = lock(&self.state);
        state.loaded.clear();
        state.gates.clear();
    }

    fn remove_gate_if_unused(&self, session_id: SessionId) {
        let mut state = lock(&self.state);
        if state.loaded.contains_key(&session_id) {
            return;
        }
        if state
            .gates
            .get(&session_id)
            .is_some_and(|gate| Arc::strong_count(gate) == 1)
        {
            state.gates.remove(&session_id);
        }
    }

    #[cfg(test)]
    fn loaded_count(&self) -> usize {
        lock(&self.state).loaded.len()
    }

    #[cfg(test)]
    fn gate_count(&self) -> usize {
        lock(&self.state).gates.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallResult {
    Installed,
    AlreadyLoaded,
    Closing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoveResult {
    Removed,
    Missing,
    PermitMismatch,
}

/// A shared poison bit distinguishes an intentional residency close from an unexpected owner
/// failure.  Request Drop implementations use it so a dead actor settles as Internal rather than
/// accidentally redacting an invariant failure as ordinary Closing.
#[derive(Default)]
struct RegistryFailureState {
    fatal: std::sync::atomic::AtomicBool,
}

impl RegistryFailureState {
    fn mark_fatal(&self) {
        self.fatal.store(true, std::sync::atomic::Ordering::Release);
    }

    fn is_fatal(&self) -> bool {
        self.fatal.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OperationId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationKind {
    Load,
    Unload,
    Fork,
    Lifecycle,
    Snapshot,
    WorkspaceDefinition,
    Metadata,
    AgentUpgrade,
    WorkspaceReload,
    AgentAvailability,
    SharedResources,
}

impl OperationKind {
    fn internal_completion(self) -> OperationCompletion {
        self.error_completion(false)
    }

    fn closing_completion(self) -> OperationCompletion {
        self.error_completion(true)
    }

    fn error_completion(self, closing: bool) -> OperationCompletion {
        match self {
            Self::Load => OperationCompletion::Load(Err(if closing {
                SessionResidencyLoadError::Closing
            } else {
                SessionResidencyLoadError::InternalDispatchUnavailable
            })),
            Self::Unload => OperationCompletion::Unload(Err(if closing {
                SessionResidencyUnloadError::Closing
            } else {
                SessionResidencyUnloadError::InternalDispatchUnavailable
            })),
            Self::Fork => OperationCompletion::Fork(Err(if closing {
                SessionResidencyForkError::Closing
            } else {
                SessionResidencyForkError::InternalDispatchUnavailable
            })),
            Self::Lifecycle => OperationCompletion::Lifecycle(Err(if closing {
                SessionResidencyLifecycleError::Closing
            } else {
                SessionResidencyLifecycleError::InternalDispatchUnavailable
            })),
            Self::Snapshot => OperationCompletion::Snapshot(Err(if closing {
                SessionResidencySnapshotError::Closing
            } else {
                SessionResidencySnapshotError::InternalDispatchUnavailable
            })),
            Self::WorkspaceDefinition => {
                OperationCompletion::WorkspaceDefinition(Err(if closing {
                    SessionResidencyWorkspaceDefinitionError::Closing
                } else {
                    SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
                }))
            }
            Self::Metadata => OperationCompletion::Metadata(Err(if closing {
                SessionResidencyMetadataError::Closing
            } else {
                SessionResidencyMetadataError::InternalDispatchUnavailable
            })),
            Self::AgentUpgrade => OperationCompletion::AgentUpgrade(Err(if closing {
                SessionResidencyAgentUpgradeError::Closing
            } else {
                SessionResidencyAgentUpgradeError::InternalDispatchUnavailable
            })),
            Self::WorkspaceReload => OperationCompletion::WorkspaceReload(Err(if closing {
                SessionResidencyWorkspaceReloadError::Closing
            } else {
                SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable
            })),
            Self::AgentAvailability => OperationCompletion::AgentAvailability(Err(if closing {
                SessionResidencyAgentAvailabilityError::Closing
            } else {
                SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable
            })),
            Self::SharedResources => OperationCompletion::SharedResources(Err(if closing {
                SessionResidencySharedResourcesError::Closing
            } else {
                SessionResidencySharedResourcesError::InternalDispatchUnavailable
            })),
        }
    }
}

enum OperationCompletion {
    Load(Result<SessionResidencyLoadOutcome, SessionResidencyLoadError>),
    Unload(Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError>),
    Fork(Result<Arc<DurableSessionHead>, SessionResidencyForkError>),
    Lifecycle(Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError>),
    Snapshot(Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError>),
    WorkspaceDefinition(
        Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError>,
    ),
    Metadata(Result<DurableSessionMetadataOutcome, SessionResidencyMetadataError>),
    AgentUpgrade(Result<DurableSessionAgentUpgradeOutcome, SessionResidencyAgentUpgradeError>),
    WorkspaceReload(
        Result<SessionDefinitionPublicationOutcome, SessionResidencyWorkspaceReloadError>,
    ),
    AgentAvailability(Result<(), SessionResidencyAgentAvailabilityError>),
    SharedResources(Result<ResidencyTurnResources, SessionResidencySharedResourcesError>),
}

impl OperationCompletion {
    fn is_internal(&self) -> bool {
        matches!(
            self,
            Self::Load(Err(SessionResidencyLoadError::InternalDispatchUnavailable))
                | Self::Unload(Err(
                    SessionResidencyUnloadError::InternalDispatchUnavailable
                ))
                | Self::Fork(Err(SessionResidencyForkError::InternalDispatchUnavailable))
                | Self::Lifecycle(Err(
                    SessionResidencyLifecycleError::InternalDispatchUnavailable
                ))
                | Self::Snapshot(Err(
                    SessionResidencySnapshotError::InternalDispatchUnavailable
                ))
                | Self::WorkspaceDefinition(Err(
                    SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
                ))
                | Self::Metadata(Err(
                    SessionResidencyMetadataError::InternalDispatchUnavailable
                ))
                | Self::AgentUpgrade(Err(
                    SessionResidencyAgentUpgradeError::InternalDispatchUnavailable
                ))
                | Self::WorkspaceReload(Err(
                    SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable
                ))
                | Self::AgentAvailability(Err(
                    SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable
                ))
                | Self::SharedResources(Err(
                    SessionResidencySharedResourcesError::InternalDispatchUnavailable
                ))
        )
    }

    fn is_closing(&self) -> bool {
        matches!(
            self,
            Self::Load(Err(SessionResidencyLoadError::Closing))
                | Self::Unload(Err(SessionResidencyUnloadError::Closing))
                | Self::Fork(Err(SessionResidencyForkError::Closing))
                | Self::Lifecycle(Err(SessionResidencyLifecycleError::Closing))
                | Self::Snapshot(Err(SessionResidencySnapshotError::Closing))
                | Self::WorkspaceDefinition(Err(SessionResidencyWorkspaceDefinitionError::Closing))
                | Self::Metadata(Err(SessionResidencyMetadataError::Closing))
                | Self::AgentUpgrade(Err(SessionResidencyAgentUpgradeError::Closing))
                | Self::WorkspaceReload(Err(SessionResidencyWorkspaceReloadError::Closing))
                | Self::AgentAvailability(Err(SessionResidencyAgentAvailabilityError::Closing))
                | Self::SharedResources(Err(SessionResidencySharedResourcesError::Closing))
        )
    }
}

enum OperationSender {
    Load(oneshot::Sender<Result<SessionResidencyLoadOutcome, SessionResidencyLoadError>>),
    Unload(oneshot::Sender<Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError>>),
    Fork(oneshot::Sender<Result<Arc<DurableSessionHead>, SessionResidencyForkError>>),
    Lifecycle(
        oneshot::Sender<Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError>>,
    ),
    Snapshot(oneshot::Sender<Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError>>),
    WorkspaceDefinition(
        oneshot::Sender<
            Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError>,
        >,
    ),
    Metadata(oneshot::Sender<Result<DurableSessionMetadataOutcome, SessionResidencyMetadataError>>),
    AgentUpgrade(
        oneshot::Sender<
            Result<DurableSessionAgentUpgradeOutcome, SessionResidencyAgentUpgradeError>,
        >,
    ),
    WorkspaceReload(
        oneshot::Sender<
            Result<SessionDefinitionPublicationOutcome, SessionResidencyWorkspaceReloadError>,
        >,
    ),
    AgentAvailability(oneshot::Sender<Result<(), SessionResidencyAgentAvailabilityError>>),
    SharedResources(oneshot::Sender<Result<(), SessionResidencySharedResourcesError>>),
}

/// The response cell retained by the actor while the caller-side receiver may be dropped.
struct OperationWaiter {
    sender: Mutex<Option<OperationSender>>,
}

impl OperationWaiter {
    fn new(sender: OperationSender) -> Self {
        Self {
            sender: Mutex::new(Some(sender)),
        }
    }

    /// Settles only after the child has been reaped.  A mismatch between a completion and its
    /// registered response is an internal dispatch failure, never a caller-visible ordinary
    /// error.
    fn settle(&self, completion: OperationCompletion) -> bool {
        let mut sender = lock(&self.sender);
        let Some(sender) = sender.take() else {
            return true;
        };
        match (sender, completion) {
            (OperationSender::Load(sender), OperationCompletion::Load(result)) => {
                let _ = sender.send(result);
                true
            }
            (OperationSender::Unload(sender), OperationCompletion::Unload(result)) => {
                let _ = sender.send(result);
                true
            }
            (OperationSender::Fork(sender), OperationCompletion::Fork(result)) => {
                let _ = sender.send(result);
                true
            }
            (OperationSender::Lifecycle(sender), OperationCompletion::Lifecycle(result)) => {
                let _ = sender.send(result);
                true
            }
            (OperationSender::Snapshot(sender), OperationCompletion::Snapshot(result)) => {
                let _ = sender.send(result);
                true
            }
            (
                OperationSender::WorkspaceDefinition(sender),
                OperationCompletion::WorkspaceDefinition(result),
            ) => {
                let _ = sender.send(result);
                true
            }
            (OperationSender::Metadata(sender), OperationCompletion::Metadata(result)) => {
                let _ = sender.send(result);
                true
            }
            (OperationSender::AgentUpgrade(sender), OperationCompletion::AgentUpgrade(result)) => {
                let _ = sender.send(result);
                true
            }
            (
                OperationSender::WorkspaceReload(sender),
                OperationCompletion::WorkspaceReload(result),
            ) => {
                let _ = sender.send(result);
                true
            }
            (
                OperationSender::AgentAvailability(sender),
                OperationCompletion::AgentAvailability(result),
            ) => {
                let _ = sender.send(result);
                true
            }
            (
                OperationSender::SharedResources(sender),
                OperationCompletion::SharedResources(result),
            ) => {
                // The actor installs the new ResidencyTurnResources into its own future Turn
                // resources before settling; the caller only needs the success verdict.
                let _ = sender.send(result.map(|_| ()));
                true
            }
            _ => false,
        }
    }

    fn settle_internal(&self) {
        let Some(sender) = lock(&self.sender).take() else {
            return;
        };
        match sender {
            OperationSender::Load(sender) => {
                let _ = sender.send(Err(SessionResidencyLoadError::InternalDispatchUnavailable));
            }
            OperationSender::Unload(sender) => {
                let _ = sender.send(Err(
                    SessionResidencyUnloadError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::Fork(sender) => {
                let _ = sender.send(Err(SessionResidencyForkError::InternalDispatchUnavailable));
            }
            OperationSender::Lifecycle(sender) => {
                let _ = sender.send(Err(
                    SessionResidencyLifecycleError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::Snapshot(sender) => {
                let _ = sender.send(Err(
                    SessionResidencySnapshotError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::WorkspaceDefinition(sender) => {
                let _ = sender.send(Err(
                    SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::Metadata(sender) => {
                let _ = sender.send(Err(
                    SessionResidencyMetadataError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::AgentUpgrade(sender) => {
                let _ = sender.send(Err(
                    SessionResidencyAgentUpgradeError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::WorkspaceReload(sender) => {
                let _ = sender.send(Err(
                    SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::AgentAvailability(sender) => {
                let _ = sender.send(Err(
                    SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable,
                ));
            }
            OperationSender::SharedResources(sender) => {
                let _ = sender.send(Err(
                    SessionResidencySharedResourcesError::InternalDispatchUnavailable,
                ));
            }
        }
    }
}

/// Waiters are retained independently of the actor's mutable operation map so an aborted actor
/// can settle every accepted request from its synchronous exit guard.
struct ActiveWaiters {
    waiters: Mutex<BTreeMap<OperationId, Arc<OperationWaiter>>>,
    #[cfg(test)]
    changed: Notify,
}

impl Default for ActiveWaiters {
    fn default() -> Self {
        Self {
            waiters: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            changed: Notify::new(),
        }
    }
}

impl ActiveWaiters {
    fn insert(&self, id: OperationId, waiter: Arc<OperationWaiter>) {
        let previous = lock(&self.waiters).insert(id, waiter);
        debug_assert!(
            previous.is_none(),
            "one residency operation owns one waiter slot"
        );
        #[cfg(test)]
        self.changed.notify_waiters();
    }

    fn remove(&self, id: OperationId) {
        let _ = lock(&self.waiters).remove(&id);
        #[cfg(test)]
        self.changed.notify_waiters();
    }

    fn settle_all_internal(&self) {
        let waiters = lock(&self.waiters).values().cloned().collect::<Vec<_>>();
        for waiter in waiters {
            waiter.settle_internal();
        }
        lock(&self.waiters).clear();
        #[cfg(test)]
        self.changed.notify_waiters();
    }

    #[cfg(test)]
    async fn wait_for_nonempty(&self) {
        self.wait_for_count(1).await;
    }

    #[cfg(test)]
    async fn wait_for_count(&self, minimum: usize) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if lock(&self.waiters).len() >= minimum {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    async fn wait_for_empty(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if lock(&self.waiters).is_empty() {
                return;
            }
            notified.await;
        }
    }
}

struct ActiveOperation {
    session_id: Option<SessionId>,
    waiter: Arc<OperationWaiter>,
    task: Option<TrackedTask>,
}

/// The operation context moved into every admitted child.  No child uses a raw task handle; the
/// actor retains and reaps the `TrackedTask` created for it.
///
/// The two lifecycle tokens are deliberately distinct: `closing` stops residency admission and
/// aborts in-flight registry operations on an ordinary shutdown, while `executor_force_closing`
/// is handed only to loaded SessionExecutors as their lifecycle token and is cancelled only by
/// fatal/poison paths.  An ordinary registry close therefore never cancels the executor token:
/// installed executors keep their active Turns until the two-phase PrepareForUnload broadcast
/// grants them the full grace period.
#[derive(Clone)]
struct OperationContext {
    state: Arc<ResidencyShared>,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    turn_resources: Option<ResidencyTurnResources>,
    unload_grace: StdDuration,
    /// Stops residency admission and aborts in-flight registry operations.  Never cancels loaded
    /// executor lifecycle work.
    closing: CancellationToken,
    /// The fail-fast executor lifecycle token installed on every loaded SessionExecutor.  Only
    /// fatal/poison paths cancel it.
    executor_force_closing: CancellationToken,
    failure: Arc<RegistryFailureState>,
    #[cfg(test)]
    replay_preparation_barrier: Option<Arc<ReplayPreparationBarrier>>,
}

#[derive(Clone)]
struct ResidencyTurnResources {
    prompt_resources: Arc<PromptResourceView>,
    model_gateway: Arc<ModelGateway>,
    model_catalog: Arc<ModelCatalogView>,
    tools: TurnToolResources,
    compaction: CompactionSettingsSnapshot,
}

impl OperationContext {
    fn poison(&self) {
        self.failure.mark_fatal();
        self.state.cancel_admission(&self.closing);
        self.executor_force_closing.cancel();
        self.task_context.request_closing();
        self.durable_state.request_closing();
    }

    fn internal_load(&self) -> SessionResidencyLoadError {
        self.poison();
        SessionResidencyLoadError::InternalDispatchUnavailable
    }

    fn internal_lifecycle(&self) -> SessionResidencyLifecycleError {
        self.poison();
        SessionResidencyLifecycleError::InternalDispatchUnavailable
    }

    fn internal_fork(&self) -> SessionResidencyForkError {
        self.poison();
        SessionResidencyForkError::InternalDispatchUnavailable
    }

    fn internal_snapshot(&self) -> SessionResidencySnapshotError {
        self.poison();
        SessionResidencySnapshotError::InternalDispatchUnavailable
    }

    fn internal_workspace(&self) -> SessionResidencyWorkspaceDefinitionError {
        self.poison();
        SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
    }

    fn internal_metadata(&self) -> SessionResidencyMetadataError {
        self.poison();
        SessionResidencyMetadataError::InternalDispatchUnavailable
    }

    fn internal_agent_upgrade(&self) -> SessionResidencyAgentUpgradeError {
        self.poison();
        SessionResidencyAgentUpgradeError::InternalDispatchUnavailable
    }

    fn internal_workspace_reload(&self) -> SessionResidencyWorkspaceReloadError {
        self.poison();
        SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable
    }

    fn internal_agent_availability(&self) -> SessionResidencyAgentAvailabilityError {
        self.poison();
        SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable
    }

    fn internal_shared_resources(&self) -> SessionResidencySharedResourcesError {
        self.poison();
        SessionResidencySharedResourcesError::InternalDispatchUnavailable
    }

    fn internal_unload(&self) -> SessionResidencyUnloadError {
        self.poison();
        SessionResidencyUnloadError::InternalDispatchUnavailable
    }
}

/// A completion guard closes the shared owners if an admitted child unwinds before it can report
/// its result.  If the shared task owner is already closing, the typed result remains Closing;
/// that is an expected shutdown race rather than an additional poison.
struct ChildCompletionGuard {
    sender: Option<mpsc::UnboundedSender<(OperationId, OperationCompletion)>>,
    operation_id: OperationId,
    kind: OperationKind,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    closing: CancellationToken,
    executor_force_closing: CancellationToken,
    failure: Arc<RegistryFailureState>,
    state: Arc<ResidencyShared>,
    settled: bool,
}

impl ChildCompletionGuard {
    fn new(
        sender: mpsc::UnboundedSender<(OperationId, OperationCompletion)>,
        operation_id: OperationId,
        kind: OperationKind,
        context: &OperationContext,
    ) -> Self {
        Self {
            sender: Some(sender),
            operation_id,
            kind,
            task_context: context.task_context.clone(),
            durable_state: context.durable_state.clone(),
            closing: context.closing.clone(),
            executor_force_closing: context.executor_force_closing.clone(),
            failure: Arc::clone(&context.failure),
            state: Arc::clone(&context.state),
            settled: false,
        }
    }

    fn complete(&mut self, completion: OperationCompletion) {
        let sender = self
            .sender
            .take()
            .expect("one residency child completion guard settles exactly once");
        if sender.send((self.operation_id, completion)).is_err() {
            self.failure.mark_fatal();
            self.state.cancel_admission(&self.closing);
            self.executor_force_closing.cancel();
            self.task_context.request_closing();
            self.durable_state.request_closing();
        }
        self.settled = true;
    }
}

impl Drop for ChildCompletionGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let completion = if self.failure.is_fatal() {
            self.kind.internal_completion()
        } else if self.task_context.is_closing() {
            self.kind.closing_completion()
        } else {
            self.failure.mark_fatal();
            self.state.cancel_admission(&self.closing);
            self.executor_force_closing.cancel();
            self.task_context.request_closing();
            self.durable_state.request_closing();
            self.kind.internal_completion()
        };
        if let Some(sender) = self.sender.take() {
            if sender.send((self.operation_id, completion)).is_err() {
                self.failure.mark_fatal();
                self.state.cancel_admission(&self.closing);
                self.executor_force_closing.cancel();
                self.task_context.request_closing();
                self.durable_state.request_closing();
            }
        }
        self.settled = true;
    }
}

struct LoadRequest {
    session_id: SessionId,
    response:
        Option<oneshot::Sender<Result<SessionResidencyLoadOutcome, SessionResidencyLoadError>>>,
}

impl LoadRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyLoadError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyLoadError::InternalDispatchUnavailable));
        }
    }
}

impl Drop for LoadRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct UnloadRequest {
    session_id: SessionId,
    response:
        Option<oneshot::Sender<Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError>>>,
}

impl UnloadRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyUnloadError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencyUnloadError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for UnloadRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct ForkRequest {
    source_session_id: SessionId,
    anchor: Option<ForkAnchor>,
    child_created_at: Timestamp,
    response: Option<oneshot::Sender<Result<Arc<DurableSessionHead>, SessionResidencyForkError>>>,
}

impl ForkRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyForkError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyForkError::InternalDispatchUnavailable));
        }
    }
}

impl Drop for ForkRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct LifecycleRequest {
    attempt: Option<SealedSessionLifecycleAttempt>,
    response: Option<
        oneshot::Sender<Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError>>,
    >,
}

impl LifecycleRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyLifecycleError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencyLifecycleError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for LifecycleRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct SnapshotRequest {
    session_id: SessionId,
    response: Option<
        oneshot::Sender<Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError>>,
    >,
}

impl SnapshotRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencySnapshotError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencySnapshotError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for SnapshotRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct WorkspaceDefinitionRequest {
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    workspace: Option<Workspace>,
    model: Option<SessionModelConfig>,
    prompts: Option<SessionPromptSelection>,
    owner_timestamp: Timestamp,
    command_id: CommandId,
    response: Option<
        oneshot::Sender<
            Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError>,
        >,
    >,
}

struct MetadataRequest {
    attempt: Option<SealedSessionMetadataAttempt>,
    timestamp: Timestamp,
    command_id: CommandId,
    response: Option<
        oneshot::Sender<Result<DurableSessionMetadataOutcome, SessionResidencyMetadataError>>,
    >,
}

impl MetadataRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyMetadataError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencyMetadataError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for MetadataRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct AgentUpgradeRequest {
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    target: Option<AgentRevisionRef>,
    owner_timestamp: Timestamp,
    command_id: CommandId,
    response: Option<
        oneshot::Sender<
            Result<DurableSessionAgentUpgradeOutcome, SessionResidencyAgentUpgradeError>,
        >,
    >,
}

impl AgentUpgradeRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyAgentUpgradeError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencyAgentUpgradeError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for AgentUpgradeRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct WorkspaceReloadRequest {
    session_id: SessionId,
    owner_timestamp: Timestamp,
    command_id: CommandId,
    response: Option<
        oneshot::Sender<
            Result<SessionDefinitionPublicationOutcome, SessionResidencyWorkspaceReloadError>,
        >,
    >,
}

impl WorkspaceReloadRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyWorkspaceReloadError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for WorkspaceReloadRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct AgentAvailabilityRequest {
    session_id: SessionId,
    agent_id: AgentId,
    available: bool,
    timestamp: Timestamp,
    command_id: CommandId,
    response: Option<oneshot::Sender<Result<(), SessionResidencyAgentAvailabilityError>>>,
}

impl AgentAvailabilityRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyAgentAvailabilityError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for AgentAvailabilityRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

struct SharedResourcesRequest {
    prompt_resources: Arc<PromptResourceView>,
    model_catalog: Arc<ModelCatalogView>,
    timestamp: Timestamp,
    command_id: CommandId,
    response: Option<oneshot::Sender<Result<(), SessionResidencySharedResourcesError>>>,
}

impl SharedResourcesRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencySharedResourcesError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencySharedResourcesError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for SharedResourcesRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

impl WorkspaceDefinitionRequest {
    fn reject_closing(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(SessionResidencyWorkspaceDefinitionError::Closing));
        }
    }

    fn reject_internal(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(
                SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable,
            ));
        }
    }
}

impl Drop for WorkspaceDefinitionRequest {
    fn drop(&mut self) {
        self.reject_internal();
    }
}

enum ResidencyRequest {
    Load(LoadRequest),
    Unload(UnloadRequest),
    Fork(ForkRequest),
    Lifecycle(LifecycleRequest),
    Snapshot(SnapshotRequest),
    WorkspaceDefinition(WorkspaceDefinitionRequest),
    Metadata(MetadataRequest),
    AgentUpgrade(AgentUpgradeRequest),
    WorkspaceReload(WorkspaceReloadRequest),
    AgentAvailability(AgentAvailabilityRequest),
    SharedResources(SharedResourcesRequest),
}

impl ResidencyRequest {
    fn reject_closing(&mut self) {
        match self {
            Self::Load(request) => request.reject_closing(),
            Self::Unload(request) => request.reject_closing(),
            Self::Fork(request) => request.reject_closing(),
            Self::Lifecycle(request) => request.reject_closing(),
            Self::Snapshot(request) => request.reject_closing(),
            Self::WorkspaceDefinition(request) => request.reject_closing(),
            Self::Metadata(request) => request.reject_closing(),
            Self::AgentUpgrade(request) => request.reject_closing(),
            Self::WorkspaceReload(request) => request.reject_closing(),
            Self::AgentAvailability(request) => request.reject_closing(),
            Self::SharedResources(request) => request.reject_closing(),
        }
    }

    fn reject_internal(&mut self) {
        match self {
            Self::Load(request) => request.reject_internal(),
            Self::Unload(request) => request.reject_internal(),
            Self::Fork(request) => request.reject_internal(),
            Self::Lifecycle(request) => request.reject_internal(),
            Self::Snapshot(request) => request.reject_internal(),
            Self::WorkspaceDefinition(request) => request.reject_internal(),
            Self::Metadata(request) => request.reject_internal(),
            Self::AgentUpgrade(request) => request.reject_internal(),
            Self::WorkspaceReload(request) => request.reject_internal(),
            Self::AgentAvailability(request) => request.reject_internal(),
            Self::SharedResources(request) => request.reject_internal(),
        }
    }
}

/// The actor owns the request receiver, all admitted child registrations, and the installed
/// executors' final close sequence.  Different Sessions can progress in different children;
/// same-Session ordering is carried by `ResidencyState::gates`.
struct SessionResidencyActor {
    receiver: mpsc::Receiver<ResidencyRequest>,
    completions: mpsc::UnboundedReceiver<(OperationId, OperationCompletion)>,
    completion_sender: mpsc::UnboundedSender<(OperationId, OperationCompletion)>,
    closing: CancellationToken,
    executor_force_closing: CancellationToken,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    turn_resources: Option<ResidencyTurnResources>,
    unload_grace: StdDuration,
    state: Arc<ResidencyShared>,
    failure: Arc<RegistryFailureState>,
    active_waiters: Arc<ActiveWaiters>,
    active: BTreeMap<OperationId, ActiveOperation>,
    next_operation_id: u64,
}

impl SessionResidencyActor {
    async fn run(mut self) -> bool {
        loop {
            if self.closing.is_cancelled() {
                return self.close_and_drain(!self.failure.is_fatal()).await;
            }

            tokio::select! {
                biased;
                _ = self.closing.cancelled() => {
                    return self.close_and_drain(!self.failure.is_fatal()).await;
                }
                completion = self.completions.recv() => match completion {
                    Some(completion) => {
                        if !self.handle_completion(completion).await {
                            self.poison();
                            self.reap_all_active_as_internal().await;
                            return self.close_and_drain(false).await;
                        }
                    }
                    None => {
                        self.poison();
                        self.reap_all_active_as_internal().await;
                        return self.close_and_drain(false).await;
                    }
                },
                request = self.receiver.recv() => match request {
                    Some(mut request) => {
                        if self.closing.is_cancelled() {
                            self.reject_request(&mut request);
                        } else {
                            self.start_request(request);
                        }
                    }
                    None => {
                        self.poison();
                        self.reap_all_active_as_internal().await;
                        return self.close_and_drain(false).await;
                    }
                },
            }
        }
    }

    fn operation_context(&self) -> OperationContext {
        OperationContext {
            state: Arc::clone(&self.state),
            task_context: self.task_context.clone(),
            durable_state: self.durable_state.clone(),
            resolver: Arc::clone(&self.resolver),
            prompt_service: Arc::clone(&self.prompt_service),
            turn_resources: self.turn_resources.clone(),
            unload_grace: self.unload_grace,
            closing: self.closing.clone(),
            executor_force_closing: self.executor_force_closing.clone(),
            failure: Arc::clone(&self.failure),
            #[cfg(test)]
            replay_preparation_barrier: lock(&self.state.replay_preparation_barrier).clone(),
        }
    }

    fn poison(&self) {
        self.failure.mark_fatal();
        self.state.cancel_admission(&self.closing);
        self.executor_force_closing.cancel();
        self.task_context.request_closing();
        self.durable_state.request_closing();
    }

    fn reject_request(&self, request: &mut ResidencyRequest) {
        if self.failure.is_fatal() {
            request.reject_internal();
        } else {
            request.reject_closing();
        }
    }

    fn start_request(&mut self, request: ResidencyRequest) {
        match request {
            ResidencyRequest::Load(request) => self.start_load(request),
            ResidencyRequest::Unload(request) => self.start_unload(request),
            ResidencyRequest::Fork(request) => self.start_fork(request),
            ResidencyRequest::Lifecycle(request) => self.start_lifecycle(request),
            ResidencyRequest::Snapshot(request) => self.start_snapshot(request),
            ResidencyRequest::WorkspaceDefinition(request) => {
                self.start_workspace_definition(request)
            }
            ResidencyRequest::Metadata(request) => self.start_metadata(request),
            ResidencyRequest::AgentUpgrade(request) => self.start_agent_upgrade(request),
            ResidencyRequest::WorkspaceReload(request) => self.start_workspace_reload(request),
            ResidencyRequest::AgentAvailability(request) => self.start_agent_availability(request),
            ResidencyRequest::SharedResources(request) => self.start_shared_resources(request),
        }
    }

    fn next_operation_id(&mut self) -> Option<OperationId> {
        let id = OperationId(self.next_operation_id);
        self.next_operation_id = self.next_operation_id.checked_add(1)?;
        Some(id)
    }

    fn start_child<F>(
        &mut self,
        session_id: Option<SessionId>,
        kind: OperationKind,
        sender: OperationSender,
        future: F,
    ) where
        F: std::future::Future<Output = OperationCompletion> + Send + 'static,
    {
        let waiter = Arc::new(OperationWaiter::new(sender));
        let Some(operation_id) = self.next_operation_id() else {
            self.poison();
            waiter.settle_internal();
            return;
        };
        self.active_waiters
            .insert(operation_id, Arc::clone(&waiter));
        self.active.insert(
            operation_id,
            ActiveOperation {
                session_id,
                waiter: Arc::clone(&waiter),
                task: None,
            },
        );

        let context = self.operation_context();
        let completion_sender = self.completion_sender.clone();
        let guard = ChildCompletionGuard::new(completion_sender, operation_id, kind, &context);
        let worker = async move {
            let mut guard = guard;
            let completion = future.await;
            guard.complete(completion);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                if let Some(active) = self.active.get_mut(&operation_id) {
                    active.task = Some(task);
                } else {
                    // This is only reachable if actor state was corrupted between installation
                    // and spawn return.  The child guard still reports the completion; poison now
                    // makes all owner-facing fallout deterministic.
                    self.poison();
                }
            }
            Err(RuntimeTaskError::OwnerClosing) => {
                // The moved guard reports typed Closing through the single completion path.  Do
                // not add a second poison for an ordinary owner shutdown race.
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                // The moved guard reports InternalDispatchUnavailable exactly once and closes the
                // shared owners.  The actor handles the completion; it must not settle twice.
            }
        }
    }

    fn start_load(&mut self, mut request: LoadRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let session_id = request.session_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::Load,
            OperationSender::Load(sender),
            async move { OperationCompletion::Load(run_load(context, session_id).await) },
        );
    }

    fn start_unload(&mut self, mut request: UnloadRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let session_id = request.session_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::Unload,
            OperationSender::Unload(sender),
            async move { OperationCompletion::Unload(run_unload(context, session_id).await) },
        );
    }

    fn start_fork(&mut self, mut request: ForkRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let Some(anchor) = request.anchor.take() else {
            self.poison();
            return;
        };
        let source_session_id = request.source_session_id;
        let child_created_at = request.child_created_at;
        let context = self.operation_context();
        self.start_child(
            Some(source_session_id),
            OperationKind::Fork,
            OperationSender::Fork(sender),
            async move {
                OperationCompletion::Fork(
                    run_fork(context, source_session_id, anchor, child_created_at).await,
                )
            },
        );
    }

    fn start_lifecycle(&mut self, mut request: LifecycleRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let Some(attempt) = request.attempt.take() else {
            self.poison();
            return;
        };
        let session_id = attempt.session_id();
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::Lifecycle,
            OperationSender::Lifecycle(sender),
            async move { OperationCompletion::Lifecycle(run_lifecycle(context, attempt).await) },
        );
    }

    fn start_snapshot(&mut self, mut request: SnapshotRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let session_id = request.session_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::Snapshot,
            OperationSender::Snapshot(sender),
            async move { OperationCompletion::Snapshot(run_snapshot(context, session_id).await) },
        );
    }

    fn start_workspace_definition(&mut self, mut request: WorkspaceDefinitionRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let session_id = request.session_id;
        let expected_revision = request.expected_revision;
        let workspace = request.workspace.take();
        let model = request.model.take();
        let prompts = request.prompts.take();
        let owner_timestamp = request.owner_timestamp;
        let command_id = request.command_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::WorkspaceDefinition,
            OperationSender::WorkspaceDefinition(sender),
            async move {
                OperationCompletion::WorkspaceDefinition(
                    run_session_definition(
                        context,
                        session_id,
                        expected_revision,
                        workspace,
                        model,
                        prompts,
                        owner_timestamp,
                        command_id,
                    )
                    .await,
                )
            },
        );
    }

    fn start_metadata(&mut self, mut request: MetadataRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let Some(attempt) = request.attempt.take() else {
            self.poison();
            return;
        };
        let session_id = attempt.session_id();
        let timestamp = request.timestamp;
        let command_id = request.command_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::Metadata,
            OperationSender::Metadata(sender),
            async move {
                OperationCompletion::Metadata(
                    run_metadata(context, attempt, timestamp, command_id).await,
                )
            },
        );
    }

    fn start_agent_upgrade(&mut self, mut request: AgentUpgradeRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let session_id = request.session_id;
        let expected_revision = request.expected_revision;
        let target = request.target;
        let owner_timestamp = request.owner_timestamp;
        let command_id = request.command_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::AgentUpgrade,
            OperationSender::AgentUpgrade(sender),
            async move {
                OperationCompletion::AgentUpgrade(
                    run_agent_upgrade(
                        context,
                        session_id,
                        expected_revision,
                        target,
                        owner_timestamp,
                        command_id,
                    )
                    .await,
                )
            },
        );
    }

    fn start_workspace_reload(&mut self, mut request: WorkspaceReloadRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let session_id = request.session_id;
        let owner_timestamp = request.owner_timestamp;
        let command_id = request.command_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::WorkspaceReload,
            OperationSender::WorkspaceReload(sender),
            async move {
                OperationCompletion::WorkspaceReload(
                    run_workspace_reload(context, session_id, owner_timestamp, command_id).await,
                )
            },
        );
    }

    fn start_agent_availability(&mut self, mut request: AgentAvailabilityRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let session_id = request.session_id;
        let agent_id = request.agent_id;
        let available = request.available;
        let timestamp = request.timestamp;
        let command_id = request.command_id;
        let context = self.operation_context();
        self.start_child(
            Some(session_id),
            OperationKind::AgentAvailability,
            OperationSender::AgentAvailability(sender),
            async move {
                OperationCompletion::AgentAvailability(
                    run_agent_availability(
                        context, session_id, agent_id, available, timestamp, command_id,
                    )
                    .await,
                )
            },
        );
    }

    fn start_shared_resources(&mut self, mut request: SharedResourcesRequest) {
        let Some(sender) = request.response.take() else {
            self.poison();
            return;
        };
        let prompt_resources = Arc::clone(&request.prompt_resources);
        let model_catalog = Arc::clone(&request.model_catalog);
        let timestamp = request.timestamp;
        let command_id = request.command_id;
        let context = self.operation_context();
        // The operation is Runtime-scope: it owns no per-Session gate slot, and its completion
        // installs the new ResidencyTurnResources into the actor before settling the caller.
        self.start_child(
            None,
            OperationKind::SharedResources,
            OperationSender::SharedResources(sender),
            async move {
                OperationCompletion::SharedResources(
                    run_shared_resources(
                        context,
                        prompt_resources,
                        model_catalog,
                        timestamp,
                        command_id,
                    )
                    .await,
                )
            },
        );
    }

    async fn handle_completion(
        &mut self,
        (operation_id, completion): (OperationId, OperationCompletion),
    ) -> bool {
        let Some(active) = self.active.get_mut(&operation_id) else {
            // A completion without an exact active slot is an integrity failure, including a
            // duplicate completion after the slot was retired.
            self.poison();
            return false;
        };
        let task = active.task.take();
        let missing_task_is_valid =
            task.is_none() && (completion.is_closing() || completion.is_internal());
        if task.is_none() && !missing_task_is_valid {
            self.poison();
            return false;
        }
        let worker_result = match task {
            Some(task) => task.wait().await,
            None => Ok(()),
        };
        let Some(active) = self.active.get(&operation_id) else {
            self.poison();
            return false;
        };

        if worker_result.is_err() || completion.is_internal() || self.failure.is_fatal() {
            self.poison();
            active.waiter.settle_internal();
            self.remove_active(operation_id);
            return false;
        }

        if completion.is_closing() {
            self.state.cancel_admission(&self.closing);
        }
        // A successful shared-resource installation installs the new ResidencyTurnResources into
        // the actor before the caller is settled, so every Load admitted after this completion
        // uses the new Prompt/Model roots.  The value stays inside the completion handed to the
        // waiter, which downgrades the success to unit for the caller.
        let completion = match completion {
            OperationCompletion::SharedResources(Ok(resources)) => {
                self.turn_resources = Some(resources.clone());
                OperationCompletion::SharedResources(Ok(resources))
            }
            completion => completion,
        };
        if !active.waiter.settle(completion) {
            self.poison();
            active.waiter.settle_internal();
            self.remove_active(operation_id);
            return false;
        }
        self.remove_active(operation_id);
        true
    }

    fn remove_active(&mut self, operation_id: OperationId) {
        let removed = self
            .active
            .remove(&operation_id)
            .expect("one completion retires one active operation");
        self.active_waiters.remove(operation_id);
        // Only per-Session operations own a gate; the Runtime-scope shared-resource operation
        // carries no SessionId and never clears a per-Session gate.
        if let Some(session_id) = removed.session_id {
            if !self
                .active
                .values()
                .any(|active| active.session_id == Some(session_id))
            {
                self.state.remove_gate_if_unused(session_id);
            }
        }
    }

    async fn reap_all_active_as_internal(&mut self) {
        let ids = self.active.keys().copied().collect::<Vec<_>>();
        for operation_id in ids {
            let task = self
                .active
                .get_mut(&operation_id)
                .and_then(|active| active.task.take());
            if let Some(task) = task {
                let _ = task.wait().await;
            }
            if let Some(active) = self.active.get(&operation_id) {
                active.waiter.settle_internal();
            }
            self.remove_active(operation_id);
        }
    }

    async fn close_and_drain(&mut self, mut normal: bool) -> bool {
        self.receiver.close();
        let mut requests_drained = false;

        loop {
            if !requests_drained {
                loop {
                    match self.receiver.try_recv() {
                        Ok(mut request) => self.reject_request(&mut request),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        // `recv() == None` remains the definitive observation.  In particular,
                        // this loop never treats an Empty result as proof that pre-close reserved
                        // permits have been released.
                        Err(mpsc::error::TryRecvError::Disconnected) => break,
                    }
                }
            }

            if requests_drained && self.active.is_empty() {
                break;
            }

            tokio::select! {
                biased;
                completion = self.completions.recv(), if !self.active.is_empty() => match completion {
                    Some(completion) => {
                        if !self.handle_completion(completion).await {
                            normal = false;
                            self.poison();
                            self.reap_all_active_as_internal().await;
                        }
                    }
                    None => {
                        normal = false;
                        self.poison();
                        self.reap_all_active_as_internal().await;
                    }
                },
                request = self.receiver.recv(), if !requests_drained => match request {
                    Some(mut request) => self.reject_request(&mut request),
                    None => requests_drained = true,
                },
            }
        }

        if !self.close_installed_executors().await {
            normal = false;
            self.poison();
        }
        self.state.clear();
        normal && !self.failure.is_fatal()
    }

    async fn close_installed_executors(&self) -> bool {
        let executors = self.state.installed_executors();
        // Broadcast the graceful-Unload preparation to every installed executor first (the
        // admission gates close synchronously), so all grace periods count down in parallel
        // instead of accumulating N×grace sequentially, then await the shared waiters and close
        // each executor.  No untracked task is spawned.  This normal close never cancels the
        // executor force token: the broadcast itself is the grace trigger, and the ordinary
        // `executor.close()` below only cancels each executor's own internal close token.
        let waiters = executors
            .iter()
            .map(|executor| executor.begin_prepare_for_unload(self.unload_grace))
            .collect::<Vec<Result<PrepareUnloadWaiter, SessionExecutorPrepareUnloadError>>>();
        let mut normal = true;
        for waiter in waiters {
            match waiter {
                Ok(waiter) => {
                    if waiter.wait().await.is_err() {
                        normal = false;
                    }
                }
                Err(_) => normal = false,
            }
        }
        for executor in executors {
            if executor.close().await.is_err() {
                normal = false;
            }
        }
        normal
    }
}

struct ActorExitGuard {
    closing: CancellationToken,
    executor_force_closing: CancellationToken,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    failure: Arc<RegistryFailureState>,
    active_waiters: Arc<ActiveWaiters>,
    shared: Arc<ResidencyShared>,
    armed: bool,
}

impl ActorExitGuard {
    fn new(
        closing: CancellationToken,
        executor_force_closing: CancellationToken,
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        failure: Arc<RegistryFailureState>,
        active_waiters: Arc<ActiveWaiters>,
        shared: Arc<ResidencyShared>,
    ) -> Self {
        Self {
            closing,
            executor_force_closing,
            task_context,
            durable_state,
            failure,
            active_waiters,
            shared,
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
        self.failure.mark_fatal();
        self.shared.cancel_admission(&self.closing);
        self.executor_force_closing.cancel();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        self.active_waiters.settle_all_internal();
    }
}

/// The deep, crate-private loaded Session residency owner.
///
/// `RuntimeInner` owns this value and will route public commands to it. This module does not
/// expose the loaded executor, the per-Session gate, or the residency permit.
pub(crate) struct SessionResidencyRegistry {
    sender: mpsc::Sender<ResidencyRequest>,
    closing: CancellationToken,
    executor_force_closing: CancellationToken,
    task: TrackedTask,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    failure: Arc<RegistryFailureState>,
    shared: Arc<ResidencyShared>,
    active_waiters: Arc<ActiveWaiters>,
}

impl fmt::Debug for SessionResidencyRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionResidencyRegistry { .. }")
    }
}

impl SessionResidencyRegistry {
    pub(crate) fn loaded_session_snapshots(&self) -> Vec<Arc<SessionExecutorSnapshot>> {
        self.shared.loaded_session_snapshots()
    }

    /// Starts a test-only residency actor without Turn resources.
    #[cfg(test)]
    pub(crate) fn start(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
    ) -> Result<Self, SessionResidencyStartError> {
        Self::start_inner(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            None,
            DEFAULT_UNLOAD_GRACE,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one Runtime Turn resource bundle binds the exact captured owners and settings"
    )]
    pub(crate) fn start_with_turn_resources(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        compaction: CompactionSettingsSnapshot,
    ) -> Result<Self, SessionResidencyStartError> {
        Self::start_with_turn_resources_and_tools_and_compaction(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
            ToolSet::empty(),
            compaction,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one test-injected turn resource bundle binds the exact runtime owners"
    )]
    pub(crate) fn start_with_turn_resources_and_tools(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        tool_set: Arc<ToolSet>,
    ) -> Result<Self, SessionResidencyStartError> {
        Self::start_with_turn_resources_and_tools_and_compaction(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
            tool_set,
            CompactionSettings::default()
                .validate()
                .expect("default compaction settings are valid"),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one Turn resource bundle binds the exact runtime owners and settings"
    )]
    pub(crate) fn start_with_turn_resources_and_tools_and_compaction(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        tool_set: Arc<ToolSet>,
        compaction: CompactionSettingsSnapshot,
    ) -> Result<Self, SessionResidencyStartError> {
        Self::start_with_turn_resources_and_tools_and_compaction_and_unload_grace(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
            tool_set,
            compaction,
            DEFAULT_UNLOAD_GRACE,
        )
    }

    /// The production start seam: installs the Runtime-validated graceful-Unload grace on the
    /// residency actor so every Unload and the registry shutdown broadcast use it.
    #[allow(
        clippy::too_many_arguments,
        reason = "one Turn resource bundle plus the configured Unload grace binds the exact runtime owners and settings"
    )]
    pub(crate) fn start_with_turn_resources_and_tools_and_compaction_and_unload_grace(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        tool_set: Arc<ToolSet>,
        compaction: CompactionSettingsSnapshot,
        unload_grace: StdDuration,
    ) -> Result<Self, SessionResidencyStartError> {
        Self::start_inner(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            Some(ResidencyTurnResources {
                prompt_resources,
                model_gateway,
                model_catalog,
                tools: TurnToolResources::Captured(tool_set),
                compaction,
            }),
            unload_grace,
        )
    }

    /// The narrow production start seam: installs the frozen production Tool config (instead
    /// of one test-injected ToolSet) together with the Runtime-validated graceful-Unload
    /// grace.  The config is materialized per admission against the exact captured Workspace
    /// snapshot; nothing is materialized here.
    #[allow(
        clippy::too_many_arguments,
        reason = "one Turn resource bundle plus the frozen production Tool config and the configured Unload grace binds the exact runtime owners and settings"
    )]
    pub(crate) fn start_with_turn_resources_and_production_tools_and_compaction_and_unload_grace(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        tool_config: ProductionToolConfig,
        compaction: CompactionSettingsSnapshot,
        unload_grace: StdDuration,
    ) -> Result<Self, SessionResidencyStartError> {
        Self::start_inner(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            Some(ResidencyTurnResources {
                prompt_resources,
                model_gateway,
                model_catalog,
                tools: TurnToolResources::Production(tool_config),
                compaction,
            }),
            unload_grace,
        )
    }

    fn start_inner(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        turn_resources: Option<ResidencyTurnResources>,
        unload_grace: StdDuration,
    ) -> Result<Self, SessionResidencyStartError> {
        let closing = CancellationToken::new();
        // The fail-fast executor lifecycle token stays distinct from the admission token for the
        // whole registry lifetime: ordinary close cancels only `closing`, while every fatal/
        // poison path cancels both.  Loaded executors receive this token (never `closing`) as
        // their lifecycle token, so an explicit shutdown broadcast grants them the full grace.
        let executor_force_closing = CancellationToken::new();
        let (sender, receiver) = mpsc::channel(SESSION_RESIDENCY_REQUEST_QUEUE_CAPACITY);
        let failure = Arc::new(RegistryFailureState::default());
        let active_waiters = Arc::new(ActiveWaiters::default());
        let shared = Arc::new(ResidencyShared::new());
        let (completion_sender, completion_receiver) = mpsc::unbounded_channel();
        let actor = SessionResidencyActor {
            receiver,
            completions: completion_receiver,
            completion_sender,
            closing: closing.clone(),
            executor_force_closing: executor_force_closing.clone(),
            task_context: task_context.clone(),
            durable_state: durable_state.clone(),
            resolver,
            prompt_service,
            turn_resources,
            unload_grace,
            state: Arc::clone(&shared),
            failure: Arc::clone(&failure),
            active_waiters: Arc::clone(&active_waiters),
            active: BTreeMap::new(),
            next_operation_id: 1,
        };

        let actor_closing = closing.clone();
        let actor_force_closing = executor_force_closing.clone();
        let actor_task_context = task_context.clone();
        let actor_durable_state = durable_state.clone();
        let actor_failure = Arc::clone(&failure);
        let actor_waiters = Arc::clone(&active_waiters);
        let actor_shared = Arc::clone(&shared);
        let mut exit_guard = ActorExitGuard::new(
            actor_closing,
            actor_force_closing,
            actor_task_context,
            actor_durable_state,
            actor_failure,
            actor_waiters,
            actor_shared,
        );
        let task = match task_context.spawn_tracked(async move {
            let normal_exit = actor.run().await;
            if normal_exit {
                exit_guard.disarm();
            }
        }) {
            Ok(task) => task,
            Err(RuntimeTaskError::OwnerClosing) => {
                task_context.request_closing();
                durable_state.request_closing();
                return Err(SessionResidencyStartError::Closing);
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                task_context.request_closing();
                durable_state.request_closing();
                return Err(SessionResidencyStartError::InternalDispatchUnavailable);
            }
        };

        Ok(Self {
            sender,
            closing,
            executor_force_closing,
            task,
            task_context,
            durable_state,
            failure,
            shared,
            active_waiters,
        })
    }

    /// Stops new residency admission.  Accepted child operations remain owner-tracked and are
    /// allowed to settle; `close` performs the asynchronous drain.  This never cancels the
    /// executor force token: loaded executors keep their active Turns until the two-phase
    /// PrepareForUnload broadcast in `close` grants them the full grace period.
    pub(crate) fn request_closing(&self) {
        self.shared.cancel_admission(&self.closing);
    }

    /// Closes admission, drains every accepted request (including requests behind pre-close
    /// reserved permits), reaps every child, and only then closes installed executors.  Waiting on
    /// the actor uses `TrackedTask`, so cancellation restores the exact owner registration.
    pub(crate) async fn close(&self) {
        self.request_closing();
        if self.task.wait().await.is_err() {
            // An unexpected actor failure has already poisoned these owners through its exit
            // guard.  The extra shutdown here ensures child registrations cannot remain in the
            // shared RuntimeTaskContext after this registry close returns.
            self.failure.mark_fatal();
            self.shared.cancel_admission(&self.closing);
            self.executor_force_closing.cancel();
            self.task_context.request_closing();
            self.durable_state.request_closing();
            // The actor's asynchronous close path could not run after an unexpected exit.  Close
            // the installed executor actors synchronously through their own cancellation-safe
            // tracked handles before shutting down the shared task owner; otherwise their sender
            // clones retained in `shared` would keep their actors alive during owner shutdown.
            let executors = self.shared.installed_executors();
            for executor in executors {
                let _ = executor.close().await;
            }
            self.shared.clear();
            self.task_context.shutdown().await;
        }
    }

    /// Loads one current durable Session as a loaded Idle executor; readiness is derived from
    /// the captured availability facts (Ready, or an Unavailable cause that a later
    /// ReloadWorkspace can recover).  A duplicate request is an idempotent NoChange and never
    /// starts a second executor.
    pub(crate) async fn load_ready_idle(
        &self,
        session_id: SessionId,
    ) -> Result<SessionResidencyLoadOutcome, SessionResidencyLoadError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::Load(LoadRequest {
            session_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter.await.unwrap_or_else(|_| self.load_waiter_fallback())
    }

    /// Unloads one Session after its executor has fully closed.  A dropped caller waiter cannot
    /// cancel the admitted child or expose Unloaded early.
    pub(crate) async fn unload(
        &self,
        session_id: SessionId,
    ) -> Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::Unload(UnloadRequest {
            session_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.unload_waiter_fallback())
    }

    /// Forks one Session while holding the same per-Session gate used by Load and Unload. The
    /// loaded membership observed under this gate is the source-kind linearization point.
    pub(crate) async fn fork(
        &self,
        source_session_id: SessionId,
        anchor: ForkAnchor,
        child_created_at: Timestamp,
    ) -> Result<Arc<DurableSessionHead>, SessionResidencyForkError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::Fork(ForkRequest {
            source_session_id,
            anchor: Some(anchor),
            child_created_at,
            response: Some(response),
        });
        self.admit(request).await;
        waiter.await.unwrap_or_else(|_| self.fork_waiter_fallback())
    }

    /// Updates durable lifecycle only while the Session is unloaded.  The per-Session gate is
    /// retained across the full durable completion, so a concurrent Load cannot slip between the
    /// unloaded check and the durable publication.
    pub(crate) async fn update_lifecycle(
        &self,
        attempt: SealedSessionLifecycleAttempt,
    ) -> Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::Lifecycle(LifecycleRequest {
            attempt: Some(attempt),
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.lifecycle_waiter_fallback())
    }

    /// Returns the installed executor's immutable snapshot.  The executor is cloned under a
    /// short standard mutex and the await happens outside that guard.
    pub(crate) async fn snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::Snapshot(SnapshotRequest {
            session_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.snapshot_waiter_fallback())
    }

    /// Returns a coherent immutable transcript capture for the loaded Session.  The registry
    /// clones the installed executor under a short standard mutex and awaits the executor
    /// actor's capture directly; no per-Session operation gate or residency child is involved.
    /// An executor Closing is only the registry Closing when the registry itself is closing;
    /// otherwise it is a normal per-Session Unload / old exact executor race and reports
    /// SessionNotLoaded.
    pub(crate) async fn transcript_capture(
        &self,
        session_id: SessionId,
    ) -> Result<SessionTranscriptCapture, SessionResidencyTranscriptError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencyTranscriptError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencyTranscriptError::SessionNotLoaded)?;
        executor
            .transcript_capture()
            .await
            .map_err(|error| match error {
                SessionExecutorTranscriptError::Closing => {
                    // An executor that stopped admitting while the residency registry itself is
                    // still open is a Session that is unloading (its admission gate closed): map
                    // it to SessionNotLoaded instead of a Runtime shutdown.  A registry that is
                    // itself closing stays Closing.
                    if self.closing.is_cancelled() {
                        SessionResidencyTranscriptError::Closing
                    } else {
                        SessionResidencyTranscriptError::SessionNotLoaded
                    }
                }
                SessionExecutorTranscriptError::InternalDispatchUnavailable => {
                    SessionResidencyTranscriptError::InternalDispatchUnavailable
                }
            })
    }

    pub(crate) async fn submit(
        &self,
        session_id: SessionId,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<TurnId, SessionResidencySubmitError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencySubmitError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencySubmitError::SessionNotLoaded)?;
        executor
            .submit(command_id, intent)
            .await
            .map_err(|error| match error {
                SessionSubmitError::Closing => {
                    // An executor that stopped admitting while the residency registry itself is
                    // still open is a Session that is unloading (its admission gate closed): map
                    // it to SessionNotLoaded instead of a Runtime shutdown.  A registry that is
                    // itself closing stays RuntimeClosing.
                    if self.closing.is_cancelled() {
                        SessionResidencySubmitError::Closing
                    } else {
                        SessionResidencySubmitError::SessionNotLoaded
                    }
                }
                SessionSubmitError::CommandConflict => SessionResidencySubmitError::CommandConflict,
                SessionSubmitError::SessionBusy => SessionResidencySubmitError::SessionBusy,
                SessionSubmitError::SessionNotReady(cause) => {
                    SessionResidencySubmitError::SessionNotReady(cause)
                }
                // A security-invalidation Preparing Session settles the dedicated internal
                // Preparing error; the RuntimeDependencyUnavailable public cause is reserved
                // for the real storage-probe fact.
                SessionSubmitError::Preparing => SessionResidencySubmitError::Preparing,
                // The dedicated internal runtime-dependency failure is handled entirely inside
                // the executor (fact + probe) and is re-settled as the public-facing
                // `SessionNotReady(RuntimeDependencyUnavailable)` cause before it can leave the
                // actor; this exhaustive fallback is only reachable through an internal
                // dispatch bug.
                SessionSubmitError::RuntimeDependencyUnavailable => {
                    SessionResidencySubmitError::SessionNotReady(
                        SessionUnavailableView::RuntimeDependencyUnavailable,
                    )
                }
                SessionSubmitError::DependencyUnavailable => {
                    SessionResidencySubmitError::DependencyUnavailable
                }
                SessionSubmitError::AgentUnavailable => {
                    SessionResidencySubmitError::AgentUnavailable
                }
                SessionSubmitError::Prompt => SessionResidencySubmitError::Prompt,
                SessionSubmitError::InvalidArgument => SessionResidencySubmitError::InvalidArgument,
                SessionSubmitError::ContextOverflow => SessionResidencySubmitError::ContextOverflow,
                SessionSubmitError::Cancelled => SessionResidencySubmitError::Cancelled,
                SessionSubmitError::SecurityRevoked => SessionResidencySubmitError::Unauthorized,
                SessionSubmitError::PrepareForUnload => {
                    SessionResidencySubmitError::SessionNotLoaded
                }
                SessionSubmitError::InternalDispatchUnavailable => {
                    SessionResidencySubmitError::InternalDispatchUnavailable
                }
            })
    }

    pub(crate) async fn resolve_interaction(
        &self,
        session_id: SessionId,
        target: SessionInteractionTarget,
        resolution_key: InteractionResolutionKey,
        resolution: InteractionResolutionInput,
        timestamp: Timestamp,
    ) -> Result<(), SessionResidencyInteractionError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencyInteractionError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencyInteractionError::SessionNotLoaded)?;
        executor
            .resolve_interaction(
                target.expected_turn_id,
                target.item_id,
                target.request_id,
                resolution_key,
                resolution,
                timestamp,
            )
            .await
            .map_err(|error| match error {
                SessionInteractionError::Closing => SessionResidencyInteractionError::Closing,
                SessionInteractionError::ExpectedTurnMismatch => {
                    SessionResidencyInteractionError::ExpectedTurnMismatch
                }
                SessionInteractionError::NotFound => SessionResidencyInteractionError::NotFound,
                SessionInteractionError::FamilyMismatch => {
                    SessionResidencyInteractionError::FamilyMismatch
                }
                SessionInteractionError::InvalidResolution => {
                    SessionResidencyInteractionError::InvalidResolution
                }
                SessionInteractionError::AlreadyResolved => {
                    SessionResidencyInteractionError::AlreadyResolved
                }
                SessionInteractionError::CommandConflict => {
                    SessionResidencyInteractionError::CommandConflict
                }
                SessionInteractionError::InternalDispatchUnavailable => {
                    SessionResidencyInteractionError::InternalDispatchUnavailable
                }
            })
    }

    pub(crate) async fn follow_up(
        &self,
        session_id: SessionId,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), SessionResidencyFollowUpError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencyFollowUpError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencyFollowUpError::SessionNotLoaded)?;
        executor
            .follow_up(command_id, intent)
            .await
            .map_err(|error| match error {
                SessionFollowUpError::Closing => SessionResidencyFollowUpError::Closing,
                SessionFollowUpError::TurnNotRunning => {
                    SessionResidencyFollowUpError::TurnNotRunning
                }
                SessionFollowUpError::CommandConflict => {
                    SessionResidencyFollowUpError::CommandConflict
                }
                SessionFollowUpError::QueueFull => SessionResidencyFollowUpError::QueueFull,
                SessionFollowUpError::InternalDispatchUnavailable => {
                    SessionResidencyFollowUpError::InternalDispatchUnavailable
                }
            })
    }

    pub(crate) async fn steer(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), SessionResidencySteerError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencySteerError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencySteerError::SessionNotLoaded)?;
        executor
            .steer(turn_id, command_id, intent)
            .await
            .map_err(|error| match error {
                SessionSteerError::Closing => SessionResidencySteerError::Closing,
                SessionSteerError::TurnNotRunning => SessionResidencySteerError::TurnNotRunning,
                SessionSteerError::TurnCancelling => SessionResidencySteerError::TurnCancelling,
                SessionSteerError::ExpectedTurnMismatch => {
                    SessionResidencySteerError::ExpectedTurnMismatch
                }
                SessionSteerError::CommandConflict => SessionResidencySteerError::CommandConflict,
                SessionSteerError::QueueFull => SessionResidencySteerError::QueueFull,
                SessionSteerError::InternalDispatchUnavailable => {
                    SessionResidencySteerError::InternalDispatchUnavailable
                }
            })
    }

    pub(crate) async fn cancel_queued_message(
        &self,
        session_id: SessionId,
        command_id: CommandId,
    ) -> Result<(), SessionResidencyQueuedMessageError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencyQueuedMessageError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencyQueuedMessageError::SessionNotLoaded)?;
        executor
            .cancel_queued_message(command_id)
            .await
            .map_err(|error| match error {
                SessionQueuedMessageError::Closing => SessionResidencyQueuedMessageError::Closing,
                SessionQueuedMessageError::NotQueued => {
                    SessionResidencyQueuedMessageError::NotQueued
                }
                SessionQueuedMessageError::InternalDispatchUnavailable => {
                    SessionResidencyQueuedMessageError::InternalDispatchUnavailable
                }
            })
    }

    pub(crate) async fn cancel(
        &self,
        session_id: SessionId,
        target: SessionCancelTarget,
        timestamp: Timestamp,
    ) -> Result<crate::session_execution::SessionCancelAccepted, SessionResidencyCancelError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencyCancelError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencyCancelError::SessionNotLoaded)?;
        executor
            .cancel(target, timestamp)
            .await
            .map_err(|error| match error {
                SessionCancelError::Closing => SessionResidencyCancelError::Closing,
                SessionCancelError::SessionNotLoaded => {
                    SessionResidencyCancelError::SessionNotLoaded
                }
                SessionCancelError::SubmitNotCancellable => {
                    SessionResidencyCancelError::SubmitNotCancellable
                }
                SessionCancelError::ExpectedTurnMismatch => {
                    SessionResidencyCancelError::ExpectedTurnMismatch
                }
                SessionCancelError::TurnNotRunning => SessionResidencyCancelError::TurnNotRunning,
                SessionCancelError::TurnCancelling => SessionResidencyCancelError::TurnCancelling,
                SessionCancelError::TurnTerminal => SessionResidencyCancelError::TurnTerminal,
                SessionCancelError::InternalDispatchUnavailable => {
                    SessionResidencyCancelError::InternalDispatchUnavailable
                }
            })
    }

    /// Routes one host security Workspace invalidation out-of-band: the loaded executor is
    /// cloned directly from the residency loaded map (no per-Session operation gate is waited
    /// on) and its out-of-band security invalidation API is awaited.  The request becomes owned
    /// by the exact executor actor from the send point, so an Unload/close race settles inside
    /// that actor and an old handle can never forward the signal to a future replacement.
    pub(crate) async fn invalidate_workspace_authority(
        &self,
        session_id: SessionId,
        timestamp: Timestamp,
    ) -> Result<(), SessionResidencySecurityInvalidationError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencySecurityInvalidationError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencySecurityInvalidationError::SessionNotLoaded)?;
        let waiter = executor
            .begin_security_invalidation(timestamp)
            .map_err(|error| self.map_security_invalidation_error(error))?;
        waiter
            .wait()
            .await
            .map_err(|error| self.map_security_invalidation_error(error))
    }

    /// Maps one executor-layer security invalidation failure to the residency view.  An
    /// executor Closing is only the residency Closing when the registry closing token is
    /// already cancelled; otherwise it is a normal per-Session Unload / old exact executor race
    /// and reports SessionNotLoaded (the host restriction stays published, and a later
    /// reloaded Session can be re-invalidated).  Internal stays Internal.  The begin and wait
    /// paths share this one mapping so both settle identically.
    fn map_security_invalidation_error(
        &self,
        error: SessionSecurityInvalidationError,
    ) -> SessionResidencySecurityInvalidationError {
        match error {
            SessionSecurityInvalidationError::Closing => {
                if self.closing.is_cancelled() {
                    SessionResidencySecurityInvalidationError::Closing
                } else {
                    SessionResidencySecurityInvalidationError::SessionNotLoaded
                }
            }
            SessionSecurityInvalidationError::InternalDispatchUnavailable => {
                SessionResidencySecurityInvalidationError::InternalDispatchUnavailable
            }
        }
    }

    pub(crate) async fn subscribe(
        &self,
        session_id: SessionId,
    ) -> Result<SessionExecutorSubscription, SessionResidencySubscriptionError> {
        if self.closing.is_cancelled() {
            return Err(SessionResidencySubscriptionError::Closing);
        }
        let executor = self
            .shared
            .executor(session_id)
            .ok_or(SessionResidencySubscriptionError::SessionNotLoaded)?;
        executor.subscribe().await.map_err(|error| match error {
            SessionExecutorSnapshotError::Closing => SessionResidencySubscriptionError::Closing,
            SessionExecutorSnapshotError::InternalDispatchUnavailable => {
                SessionResidencySubscriptionError::PublisherUnavailable
            }
        })
    }

    /// Routes a complete loaded Session definition replacement to the installed executor.  The
    /// executor itself decides whether the candidate changes Workspace semantics and applies the
    /// loaded Idle requirement or the prebuilt Workspace Snapshot installation.
    pub(crate) async fn update_workspace_definition(
        &self,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
    ) -> Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
        self.update_session_definition(
            session_id,
            expected_revision,
            Some(workspace),
            None,
            None,
            owner_timestamp,
            CommandId::generate().expect("test wrapper generates a process-local command id"),
        )
        .await
    }

    /// Performs one ordinary Session definition CAS under the per-Session gate shared with Load,
    /// Unload, Fork, Lifecycle, and Metadata.  The gate covers the durable CAS, the loaded
    /// membership decision, and the required loaded executor publication, so a concurrent
    /// Load/Unload cannot slip between them.
    #[allow(
        clippy::too_many_arguments,
        reason = "one gated definition operation carries its Session identity, CAS token, three replacements, and owner event facts"
    )]
    pub(crate) async fn update_session_definition(
        &self,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        workspace: Option<Workspace>,
        model: Option<SessionModelConfig>,
        prompts: Option<SessionPromptSelection>,
        owner_timestamp: Timestamp,
        command_id: CommandId,
    ) -> Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::WorkspaceDefinition(WorkspaceDefinitionRequest {
            session_id,
            expected_revision,
            workspace,
            model,
            prompts,
            owner_timestamp,
            command_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.workspace_waiter_fallback())
    }

    /// Performs one Session metadata CAS under the per-Session gate shared with Load, Unload,
    /// Fork, and Lifecycle.  The gate covers the durable update, the loaded membership decision,
    /// and the required loaded executor publication, so a concurrent Load/Unload cannot slip
    /// between them.
    pub(crate) async fn update_session_metadata(
        &self,
        attempt: SealedSessionMetadataAttempt,
        timestamp: Timestamp,
        command_id: CommandId,
    ) -> Result<DurableSessionMetadataOutcome, SessionResidencyMetadataError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::Metadata(MetadataRequest {
            attempt: Some(attempt),
            timestamp,
            command_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.metadata_waiter_fallback())
    }

    /// Performs one explicit Session Agent revision upgrade under the per-Session gate shared
    /// with Load, Unload, Fork, Lifecycle, Metadata, and ordinary definition CAS.  The gate
    /// covers the durable Agent → Session-gated update, the loaded membership decision, and the
    /// required loaded executor publication, so a concurrent Load/Unload cannot slip between
    /// them.  Target current resolution and retained membership/status validation happen only
    /// inside DurableState.
    pub(crate) async fn upgrade_session_agent(
        &self,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        target: Option<AgentRevisionRef>,
        owner_timestamp: Timestamp,
        command_id: CommandId,
    ) -> Result<DurableSessionAgentUpgradeOutcome, SessionResidencyAgentUpgradeError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::AgentUpgrade(AgentUpgradeRequest {
            session_id,
            expected_revision,
            target,
            owner_timestamp,
            command_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.agent_upgrade_waiter_fallback())
    }

    /// Performs one loaded Session Workspace reload under the per-Session gate shared with Load,
    /// Unload, Fork, Lifecycle, Metadata, ordinary definition CAS, and Agent upgrade.  The gate
    /// covers the loaded membership decision and the full executor reload completion, so a
    /// concurrent Load/Unload cannot slip between them.  The reload never reads or updates
    /// DurableState; an executor missing under the gate maps directly to SessionNotLoaded.
    pub(crate) async fn reload_workspace(
        &self,
        session_id: SessionId,
        owner_timestamp: Timestamp,
        command_id: CommandId,
    ) -> Result<SessionDefinitionPublicationOutcome, SessionResidencyWorkspaceReloadError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::WorkspaceReload(WorkspaceReloadRequest {
            session_id,
            owner_timestamp,
            command_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.workspace_reload_waiter_fallback())
    }

    /// Applies one Agent availability fact to one loaded Session under the per-Session gate
    /// shared with every other residency operation.  The operation rechecks under the gate that
    /// the executor still exists and still pins the requested AgentId; an Unload that wins the
    /// gate is a NoChange, so no status fan-out is ever lost to a concurrent Unload.
    pub(crate) async fn set_session_agent_availability(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        available: bool,
        timestamp: Timestamp,
        command_id: CommandId,
    ) -> Result<(), SessionResidencyAgentAvailabilityError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::AgentAvailability(AgentAvailabilityRequest {
            session_id,
            agent_id,
            available,
            timestamp,
            command_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.agent_availability_waiter_fallback())
    }

    /// Installs one Runtime shared-resource pair over every loaded Session and into the
    /// residency actor's own future Turn resources.  The operation is Runtime-scope: it owns no
    /// per-Session gate slot, precomputes every availability fact before any executor update,
    /// and fails the whole fan-out atomically (no executor is updated when any precompute or
    /// install step fails).  The caller holds the Runtime shared-resource write gate, so no
    /// external Submit admission can capture a half-switched pair.
    pub(crate) async fn install_shared_resources(
        &self,
        prompt_resources: Arc<PromptResourceView>,
        model_catalog: Arc<ModelCatalogView>,
        timestamp: Timestamp,
        command_id: CommandId,
    ) -> Result<(), SessionResidencySharedResourcesError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::SharedResources(SharedResourcesRequest {
            prompt_resources,
            model_catalog,
            timestamp,
            command_id,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.shared_resources_waiter_fallback())
    }

    /// Returns the SessionIds of every currently loaded Session whose installed definition pins
    /// the requested AgentId.  This is a short-lock immutable projection: the per-Session gate
    /// recheck inside each fan-out operation is the linearization point against Unload/Load.
    pub(crate) fn loaded_session_ids_for_agent(&self, agent_id: AgentId) -> Vec<SessionId> {
        self.shared
            .loaded_session_snapshots()
            .into_iter()
            .filter(|snapshot| snapshot.definition().agent().agent_id() == agent_id)
            .map(|snapshot| snapshot.definition().session_id())
            .collect()
    }

    async fn admit(&self, mut request: ResidencyRequest) {
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                self.reject_admission(&mut request);
                return;
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    self.reject_admission(&mut request);
                    return;
                }
            },
        };
        // The bounded sender permit is the admission linearization point.  There is no await or
        // fallible operation between reservation and transferring request ownership to the actor.
        permit.send(request);
    }

    fn reject_admission(&self, request: &mut ResidencyRequest) {
        if self.failure.is_fatal() {
            request.reject_internal();
        } else {
            request.reject_closing();
        }
    }

    fn load_waiter_fallback(
        &self,
    ) -> Result<SessionResidencyLoadOutcome, SessionResidencyLoadError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyLoadError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyLoadError::Closing
        } else {
            SessionResidencyLoadError::InternalDispatchUnavailable
        })
    }

    fn unload_waiter_fallback(
        &self,
    ) -> Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyUnloadError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyUnloadError::Closing
        } else {
            SessionResidencyUnloadError::InternalDispatchUnavailable
        })
    }

    fn fork_waiter_fallback(&self) -> Result<Arc<DurableSessionHead>, SessionResidencyForkError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyForkError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyForkError::Closing
        } else {
            SessionResidencyForkError::InternalDispatchUnavailable
        })
    }

    fn lifecycle_waiter_fallback(
        &self,
    ) -> Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyLifecycleError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyLifecycleError::Closing
        } else {
            SessionResidencyLifecycleError::InternalDispatchUnavailable
        })
    }

    fn snapshot_waiter_fallback(
        &self,
    ) -> Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError> {
        Err(if self.failure.is_fatal() {
            SessionResidencySnapshotError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencySnapshotError::Closing
        } else {
            SessionResidencySnapshotError::InternalDispatchUnavailable
        })
    }

    fn workspace_waiter_fallback(
        &self,
    ) -> Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyWorkspaceDefinitionError::Closing
        } else {
            SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
        })
    }

    fn metadata_waiter_fallback(
        &self,
    ) -> Result<DurableSessionMetadataOutcome, SessionResidencyMetadataError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyMetadataError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyMetadataError::Closing
        } else {
            SessionResidencyMetadataError::InternalDispatchUnavailable
        })
    }

    fn agent_upgrade_waiter_fallback(
        &self,
    ) -> Result<DurableSessionAgentUpgradeOutcome, SessionResidencyAgentUpgradeError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyAgentUpgradeError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyAgentUpgradeError::Closing
        } else {
            SessionResidencyAgentUpgradeError::InternalDispatchUnavailable
        })
    }

    fn workspace_reload_waiter_fallback(
        &self,
    ) -> Result<SessionDefinitionPublicationOutcome, SessionResidencyWorkspaceReloadError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyWorkspaceReloadError::Closing
        } else {
            SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable
        })
    }

    fn agent_availability_waiter_fallback(
        &self,
    ) -> Result<(), SessionResidencyAgentAvailabilityError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyAgentAvailabilityError::Closing
        } else {
            SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable
        })
    }

    fn shared_resources_waiter_fallback(&self) -> Result<(), SessionResidencySharedResourcesError> {
        Err(if self.failure.is_fatal() {
            SessionResidencySharedResourcesError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencySharedResourcesError::Closing
        } else {
            SessionResidencySharedResourcesError::InternalDispatchUnavailable
        })
    }

    #[cfg(test)]
    pub(crate) fn set_replay_preparation_barrier_for_test(
        &self,
        barrier: Option<Arc<ReplayPreparationBarrier>>,
    ) {
        *lock(&self.shared.replay_preparation_barrier) = barrier;
    }

    #[cfg(test)]
    pub(crate) fn loaded_count_for_test(&self) -> usize {
        self.shared.loaded_count()
    }

    #[cfg(test)]
    pub(crate) fn gate_count_for_test(&self) -> usize {
        self.shared.gate_count()
    }

    #[cfg(test)]
    pub(crate) fn executor_for_test(&self, session_id: SessionId) -> Option<SessionExecutor> {
        self.shared.executor(session_id)
    }

    #[cfg(test)]
    async fn wait_for_active_operation_for_test(&self) {
        self.active_waiters.wait_for_nonempty().await;
    }

    #[cfg(test)]
    async fn wait_for_active_operation_count_for_test(&self, minimum: usize) {
        self.active_waiters.wait_for_count(minimum).await;
    }

    #[cfg(test)]
    async fn wait_for_no_active_operation_for_test(&self) {
        self.active_waiters.wait_for_empty().await;
    }
}

impl Drop for SessionResidencyRegistry {
    fn drop(&mut self) {
        self.request_closing();
    }
}

async fn run_load(
    context: OperationContext,
    session_id: SessionId,
) -> Result<SessionResidencyLoadOutcome, SessionResidencyLoadError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;

    if context.state.has_loaded(session_id) {
        return Ok(SessionResidencyLoadOutcome::NoChange);
    }

    // Assumption for the next DurableState slice: `session_current` is a synchronous coherent
    // Option read and its accessors expose the current head/definition Arc values without a
    // second lookup.
    let current = match context.durable_state.session_current(session_id) {
        Some(current) => current,
        None => return Err(SessionResidencyLoadError::SessionNotFound),
    };
    let head = current.head().clone();
    let definition = current.definition().clone();
    if !valid_current_shape(session_id, &head, &definition) {
        return Err(context.internal_load());
    }
    // Archived/Deleted settles first with its stable typed error, before any Agent/Model/Prompt
    // resource read, so an unrelated resource or storage failure never poisons Load.
    match head.lifecycle() {
        SessionLifecycle::Open => {}
        SessionLifecycle::Archived => return Err(SessionResidencyLoadError::SessionArchived),
        SessionLifecycle::Deleted => return Err(SessionResidencyLoadError::SessionDeleted),
    }
    // The Agent availability fact is captured synchronously right after the durable current
    // capture and stays independent of the Workspace preparation below, so a disabled/deleted
    // Agent still loads its last-good WorkspaceSnapshot and conversation and only projects
    // AgentUnavailable until it is re-enabled.  A missing Agent head or an identity mismatch
    // under a valid loaded Session is an internal invariant.
    let agent_head = context
        .durable_state
        .agent_head(definition.agent().agent_id())
        .ok_or_else(|| context.internal_load())?;
    if agent_head.agent_id() != definition.agent().agent_id() {
        return Err(context.internal_load());
    }
    let agent_available = matches!(agent_head.status(), AgentStatus::Enabled);
    // The Model availability fact is captured synchronously right after the definition capture
    // and stays independent of both the Agent availability fact and the Workspace preparation,
    // so a model that cannot serve a Turn still loads its last-good WorkspaceSnapshot and
    // conversation and only projects ModelUnavailable until a definition publication restores a
    // resolvable model.  Ordinary model selection/reasoning/output incompatibilities degrade
    // only this fact; a resolution failure on the installed Runtime-owned catalog is an
    // internal invariant that aborts Load.
    let model_available = model_available_for_load(&context, &definition)?;
    // The selected-Prompt availability fact is captured independently right after the model
    // fact and stays independent of the Workspace preparation, so a selection that cannot
    // serve a Turn still loads its last-good WorkspaceSnapshot and conversation and only
    // projects PromptUnavailable until a definition publication restores the selection.  It
    // reads the exact retained Agent revision through the same seam a future Turn admission
    // would use; Closing aborts Load as Closing and every other failure on the installed
    // Runtime-owned Prompt view is an internal invariant that aborts Load.  The test-only
    // dependency shape without Turn resources captures no Prompt fact and defaults to
    // available.
    let prompt_available = prompt_available_for_load(&context, &definition).await?;

    // Workspace resolve/capture/revalidation ordinary failures below degrade loaded readiness
    // to Unavailable instead of failing Load: the Session still opens and replays its
    // conversation and installs a loaded executor with its Recorder, and a later ReloadWorkspace
    // (or true Workspace definition update) restores Ready.  Only Closing and internal failures
    // abort Load before replay.
    enum WorkspacePreparation {
        Ready {
            candidate: WorkspaceSnapshotCandidate,
            prompt_sources: Arc<[CapturedWorkspacePromptSource]>,
            skill_sources: Arc<[CapturedWorkspaceSkillSource]>,
            requires_revalidation: bool,
        },
        Unavailable(SessionUnavailableView),
    }
    let preparation = match context
        .resolver
        .resolve(session_id, definition.workspace())
        .await
    {
        Ok(candidate) => {
            if candidate.revision() != definition.workspace().revision() {
                return Err(context.internal_load());
            }
            let skill_context = candidate.skill_capture_context();
            if !skill_context.roots().is_empty() {
                // Skill source discovery remains fail-closed until SkillService owns its
                // candidate path.
                return Err(context.internal_load());
            }
            let prompt_context = candidate.prompt_capture_context();
            let requires_revalidation = candidate.requires_revalidation();
            let capture = context
                .prompt_service
                .capture_workspace_sources(prompt_context);
            tokio::pin!(capture);
            match tokio::select! {
                biased;
                _ = context.closing.cancelled() => return Err(SessionResidencyLoadError::Closing),
                result = &mut capture => result,
            } {
                Ok(prompt_sources) => WorkspacePreparation::Ready {
                    candidate,
                    prompt_sources,
                    skill_sources: Arc::from(Vec::new().into_boxed_slice()),
                    requires_revalidation,
                },
                Err(error) => match map_prompt_load_readiness(&context, error) {
                    Ok(cause) => WorkspacePreparation::Unavailable(cause),
                    Err(load_error) => return Err(load_error),
                },
            }
        }
        Err(WorkspaceResolveError::Closing) => {
            return Err(SessionResidencyLoadError::Closing);
        }
        Err(
            WorkspaceResolveError::RootUnavailable
            | WorkspaceResolveError::RootNotDirectory
            | WorkspaceResolveError::CanonicalizationFailed
            | WorkspaceResolveError::DuplicateRoot
            | WorkspaceResolveError::OverlappingRoots
            | WorkspaceResolveError::CwdOutsideRoots
            | WorkspaceResolveError::CwdRootMismatch
            | WorkspaceResolveError::AuthorityUnavailable
            | WorkspaceResolveError::AuthorityDenied,
        ) => WorkspacePreparation::Unavailable(SessionUnavailableView::WorkspaceUnavailable),
        Err(WorkspaceResolveError::InternalDispatchUnavailable) => {
            return Err(context.internal_load());
        }
    };

    let target = context
        .durable_state
        .open_conversation_target(session_id)
        .await
        .map_err(|error| map_conversation_target_load_error(&context, error))?;
    #[cfg(test)]
    let loaded_conversation = load_replayed_conversation_with_barrier_for_test(
        target,
        context.task_context.clone(),
        context.replay_preparation_barrier.clone(),
    )
    .await
    .map_err(|error| map_conversation_load_error(&context, error))?;
    #[cfg(not(test))]
    let loaded_conversation = load_replayed_conversation(target, context.task_context.clone())
        .await
        .map_err(|error| map_conversation_load_error(&context, error))?;
    let mut readiness = SessionReadinessView::Ready;
    let workspace_snapshot = match preparation {
        WorkspacePreparation::Ready {
            candidate,
            prompt_sources,
            skill_sources,
            requires_revalidation,
        } => {
            // The authority revalidation stays after replay for a successful candidate; its
            // ordinary failures degrade readiness to WorkspaceUnavailable instead of failing
            // Load, while Closing and internal failures keep the existing Load error shape.
            if requires_revalidation {
                let revalidation_result = {
                    let revalidation = context
                        .resolver
                        .revalidate_candidate(&candidate, definition.workspace());
                    tokio::pin!(revalidation);
                    tokio::select! {
                        biased;
                        _ = context.closing.cancelled() => {
                            loaded_conversation.recorder.close().await;
                            return Err(SessionResidencyLoadError::Closing);
                        }
                        result = &mut revalidation => result,
                    }
                };
                match revalidation_result {
                    Ok(true) => {}
                    Ok(false) => {
                        readiness = SessionReadinessView::Unavailable(
                            SessionUnavailableView::WorkspaceUnavailable,
                        )
                    }
                    Err(WorkspaceResolveError::Closing) => {
                        loaded_conversation.recorder.close().await;
                        return Err(SessionResidencyLoadError::Closing);
                    }
                    Err(
                        WorkspaceResolveError::RootUnavailable
                        | WorkspaceResolveError::RootNotDirectory
                        | WorkspaceResolveError::CanonicalizationFailed
                        | WorkspaceResolveError::DuplicateRoot
                        | WorkspaceResolveError::OverlappingRoots
                        | WorkspaceResolveError::CwdOutsideRoots
                        | WorkspaceResolveError::CwdRootMismatch
                        | WorkspaceResolveError::AuthorityUnavailable
                        | WorkspaceResolveError::AuthorityDenied,
                    ) => {
                        readiness = SessionReadinessView::Unavailable(
                            SessionUnavailableView::WorkspaceUnavailable,
                        )
                    }
                    Err(WorkspaceResolveError::InternalDispatchUnavailable) => {
                        loaded_conversation.recorder.close().await;
                        return Err(context.internal_load());
                    }
                }
            }
            match readiness {
                SessionReadinessView::Ready => {
                    let snapshot = match candidate.finish(prompt_sources, skill_sources) {
                        Ok(snapshot) => snapshot,
                        Err(WorkspaceSnapshotFinishError::AuthorizationMismatch) => {
                            loaded_conversation.recorder.close().await;
                            return Err(context.internal_load());
                        }
                    };
                    Some(snapshot)
                }
                SessionReadinessView::Unavailable(_) => None,
                SessionReadinessView::Preparing => unreachable!(),
            }
        }
        WorkspacePreparation::Unavailable(cause) => {
            readiness = SessionReadinessView::Unavailable(cause);
            None
        }
    };

    // The final durable current/lifecycle/definition exact recheck runs for both Ready and
    // Unavailable outcomes; a stale/lifecycle change closes the fresh Recorder and returns the
    // original typed Load error without installing any partial owner.
    let final_recheck: Result<(), SessionResidencyLoadError> = (|| {
        let final_current = context
            .durable_state
            .session_current(session_id)
            .ok_or_else(|| context.internal_load())?;
        let final_head = final_current.head();
        let final_definition = final_current.definition();
        if !valid_current_shape(session_id, final_head, final_definition) {
            return Err(context.internal_load());
        }
        match final_head.lifecycle() {
            SessionLifecycle::Open => {}
            SessionLifecycle::Archived => return Err(SessionResidencyLoadError::SessionArchived),
            SessionLifecycle::Deleted => return Err(SessionResidencyLoadError::SessionDeleted),
        }
        if final_definition.revision() != definition.revision() {
            return Err(SessionResidencyLoadError::StaleDefinition);
        }
        if final_definition.as_ref() != definition.as_ref() {
            return Err(context.internal_load());
        }
        if context.closing.is_cancelled() {
            return Err(SessionResidencyLoadError::Closing);
        }
        Ok(())
    })();
    if let Err(error) = final_recheck {
        loaded_conversation.recorder.close().await;
        return Err(error);
    }
    let recorder = loaded_conversation.recorder;
    let live_state = loaded_conversation.live_state;
    let replay_diagnostics = loaded_conversation.diagnostics;
    let recorder_for_executor = recorder.clone();
    let conversation = LoadedSessionConversation::from_replay(
        live_state,
        recorder_for_executor,
        replay_diagnostics,
    );
    let executor_result = match (context.turn_resources.as_ref(), readiness) {
        (Some(resources), SessionReadinessView::Ready) => {
            SessionExecutor::start_loaded_idle_with_turn_resources_and_lifecycle(
                SessionExecutorDependencies::with_turn_resources_and_tool_resources_and_compaction(
                    context.task_context.clone(),
                    context.durable_state.clone(),
                    Arc::clone(&context.resolver),
                    Arc::clone(&context.prompt_service),
                    Arc::clone(&resources.prompt_resources),
                    Arc::clone(&resources.model_gateway),
                    Arc::clone(&resources.model_catalog),
                    resources.tools.clone(),
                    resources.compaction.clone(),
                ),
                definition,
                agent_available,
                model_available,
                prompt_available,
                None,
                Some(
                    workspace_snapshot.expect("a Ready load always finishes its WorkspaceSnapshot"),
                ),
                conversation,
                context.executor_force_closing.clone(),
            )
        }
        (Some(resources), SessionReadinessView::Unavailable(cause)) => {
            SessionExecutor::start_loaded_idle_with_turn_resources_and_lifecycle(
                SessionExecutorDependencies::with_turn_resources_and_tool_resources_and_compaction(
                    context.task_context.clone(),
                    context.durable_state.clone(),
                    Arc::clone(&context.resolver),
                    Arc::clone(&context.prompt_service),
                    Arc::clone(&resources.prompt_resources),
                    Arc::clone(&resources.model_gateway),
                    Arc::clone(&resources.model_catalog),
                    resources.tools.clone(),
                    resources.compaction.clone(),
                ),
                definition,
                agent_available,
                model_available,
                prompt_available,
                Some(cause),
                None,
                conversation,
                context.executor_force_closing.clone(),
            )
        }
        (Some(_), SessionReadinessView::Preparing) => unreachable!(),
        (None, SessionReadinessView::Ready) => {
            #[cfg(test)]
            {
                SessionExecutor::start_loaded_ready_idle(
                    context.task_context.clone(),
                    context.durable_state.clone(),
                    Arc::clone(&context.resolver),
                    Arc::clone(&context.prompt_service),
                    definition,
                    workspace_snapshot.expect("a Ready load always finishes its WorkspaceSnapshot"),
                    conversation,
                )
            }
            #[cfg(not(test))]
            {
                recorder.close().await;
                return Err(context.internal_load());
            }
        }
        (None, SessionReadinessView::Unavailable(_) | SessionReadinessView::Preparing) => {
            recorder.close().await;
            return Err(context.internal_load());
        }
    };
    let executor = match executor_result {
        Ok(executor) => executor,
        Err(SessionExecutorStartError::Closing) => {
            recorder.close().await;
            return Err(SessionResidencyLoadError::Closing);
        }
        Err(
            SessionExecutorStartError::SessionIdMismatch
            | SessionExecutorStartError::WorkspaceRevisionMismatch,
        ) => {
            recorder.close().await;
            return Err(context.internal_load());
        }
        Err(SessionExecutorStartError::InternalDispatchUnavailable) => {
            recorder.close().await;
            return Err(context.internal_load());
        }
    };

    let permit = SessionResidencyPermit::new();
    match context
        .state
        .install_if_open(session_id, executor.clone(), permit, &context.closing)
    {
        InstallResult::Installed => Ok(SessionResidencyLoadOutcome::Loaded),
        InstallResult::AlreadyLoaded => {
            recorder.close().await;
            if executor.close().await.is_err() {
                Err(context.internal_load())
            } else {
                Ok(SessionResidencyLoadOutcome::NoChange)
            }
        }
        InstallResult::Closing => {
            recorder.close().await;
            if executor.close().await.is_err() {
                Err(context.internal_load())
            } else {
                Err(SessionResidencyLoadError::Closing)
            }
        }
    }
}

fn map_prompt_load_readiness(
    context: &OperationContext,
    error: PromptError,
) -> Result<SessionUnavailableView, SessionResidencyLoadError> {
    match error.kind() {
        PromptErrorKind::SourceDiscovery => Ok(SessionUnavailableView::WorkspaceUnavailable),
        PromptErrorKind::ContentLoad | PromptErrorKind::DuplicateKey => {
            Ok(SessionUnavailableView::PromptUnavailable)
        }
        PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing
        | PromptErrorKind::InvalidIntent
        | PromptErrorKind::InvalidContribution
        | PromptErrorKind::ContextLimitExceeded
        | PromptErrorKind::Internal => Err(context.internal_load()),
    }
}

/// Classifies the captured definition's model availability against the installed Runtime-owned
/// catalog through the exact Turn model resolution seam, as an independent fact alongside the
/// Agent availability fact and the Workspace preparation.  Ordinary model
/// selection/reasoning/output incompatibilities degrade only this fact and Load still returns
/// Loaded; a resolution failure on the installed Runtime-owned catalog is an internal invariant
/// that aborts Load through the existing poison path.  The test-only dependency shape without
/// Turn resources captures no model fact and defaults to available; production always carries
/// the Runtime-owned catalog and rejects a missing dependency shape at executor start.
fn model_available_for_load(
    context: &OperationContext,
    definition: &SessionDefinition,
) -> Result<bool, SessionResidencyLoadError> {
    let Some(resources) = context.turn_resources.as_ref() else {
        return Ok(true);
    };
    match model_available_for_definition(
        &resources.model_gateway,
        Arc::clone(&resources.model_catalog),
        definition,
    ) {
        Ok(available) => Ok(available),
        Err(_) => Err(context.internal_load()),
    }
}

/// Classifies the captured definition's selected Agent+Session Prompt selection against the
/// installed Runtime-owned Prompt resources through the exact `for_turn` selection stage, as an
/// independent fact alongside the Agent and model availability facts and the Workspace
/// preparation.  Ordinary selection failures (missing Prompt, wrong role, duplicate resolved
/// key) degrade only this fact and Load still returns Loaded; a Closing aborts Load as Closing
/// and any other failure on the installed Runtime-owned Prompt view is an internal invariant
/// that aborts Load through the existing poison path.  The test-only dependency shape without
/// Turn resources captures no Prompt fact and defaults to available; production always carries
/// the Runtime-owned Prompt view.
async fn prompt_available_for_load(
    context: &OperationContext,
    definition: &SessionDefinition,
) -> Result<bool, SessionResidencyLoadError> {
    let Some(resources) = context.turn_resources.as_ref() else {
        return Ok(true);
    };
    match prompt_available_for_definition(
        context.durable_state.clone(),
        Arc::clone(&context.prompt_service),
        Arc::clone(&resources.prompt_resources),
        definition,
    )
    .await
    {
        Ok(available) => Ok(available),
        Err(SessionPromptAvailabilityError::Closing) => Err(SessionResidencyLoadError::Closing),
        Err(SessionPromptAvailabilityError::InternalDispatchUnavailable) => {
            Err(context.internal_load())
        }
    }
}

async fn run_unload(
    context: OperationContext,
    session_id: SessionId,
) -> Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;

    let Some((executor, permit)) = loaded_executor_and_permit(&context.state, session_id) else {
        return Ok(SessionResidencyUnloadOutcome::NoChange);
    };

    // Keep the map entry and exact permit installed until the executor has drained.  A
    // concurrent lifecycle request therefore remains Busy until this operation removes residency.
    // The configured grace lets the active admission/Turn finish naturally; the executor itself
    // signals PrepareForUnload at the deadline and settles its pending Interactions truthfully.
    match executor.prepare_for_unload(context.unload_grace).await {
        Ok(()) => {}
        Err(SessionExecutorPrepareUnloadError::Closing) => {
            // The executor actor was already closing before it accepted the prepare request
            // (for example an already-drained executor during registry shutdown).  It has either
            // drained itself or drains on close: join it and remove the exact owner so no
            // partial owner remains installed.
            if executor.close().await.is_err() {
                return Err(context.internal_unload());
            }
            return match context.state.remove_exact(session_id, &permit) {
                RemoveResult::Removed => {
                    if context.closing.is_cancelled() {
                        Err(SessionResidencyUnloadError::Closing)
                    } else {
                        Ok(SessionResidencyUnloadOutcome::Unloaded)
                    }
                }
                RemoveResult::Missing | RemoveResult::PermitMismatch => {
                    Err(context.internal_unload())
                }
            };
        }
        Err(SessionExecutorPrepareUnloadError::Internal) => {
            return Err(context.internal_unload());
        }
    }

    if let Err(SessionExecutorCloseError::InternalDispatchUnavailable) = executor.close().await {
        return Err(context.internal_unload());
    }
    match context.state.remove_exact(session_id, &permit) {
        RemoveResult::Removed => Ok(SessionResidencyUnloadOutcome::Unloaded),
        RemoveResult::Missing | RemoveResult::PermitMismatch => Err(context.internal_unload()),
    }
}

async fn run_fork(
    context: OperationContext,
    source_session_id: SessionId,
    anchor: ForkAnchor,
    child_created_at: Timestamp,
) -> Result<Arc<DurableSessionHead>, SessionResidencyForkError> {
    let gate = context.state.gate(source_session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    let source = if context.state.has_loaded(source_session_id) {
        ForkSourceKind::LiveSnapshot
    } else {
        ForkSourceKind::RecordedHistory
    };
    let attempt =
        SealedSessionForkAttempt::new(source_session_id, source, anchor.clone(), child_created_at);
    let result = match source {
        ForkSourceKind::LiveSnapshot => {
            let executor = context
                .state
                .executor(source_session_id)
                .ok_or_else(|| context.internal_fork())?;
            let snapshot =
                executor
                    .capture_fork_conversation(anchor)
                    .map_err(|error| match error {
                        ForkAnchorResolutionError::InvalidAnchor => {
                            SessionResidencyForkError::InvalidAnchor
                        }
                        ForkAnchorResolutionError::InvalidSource
                        | ForkAnchorResolutionError::TooLarge
                        | ForkAnchorResolutionError::Encode
                        | ForkAnchorResolutionError::Unavailable => context.internal_fork(),
                    })?;
            context
                .durable_state
                .fork_session_from_live_snapshot(attempt, snapshot)
                .await
        }
        ForkSourceKind::RecordedHistory => context.durable_state.fork_session(attempt).await,
    };
    result.map_err(|error| map_durable_fork_error(&context, error))
}

fn map_durable_fork_error(
    context: &OperationContext,
    error: DurableSessionForkError,
) -> SessionResidencyForkError {
    match error {
        DurableSessionForkError::Closing => SessionResidencyForkError::Closing,
        DurableSessionForkError::SourceNotFound => SessionResidencyForkError::SourceNotFound,
        DurableSessionForkError::SourceDeleted => SessionResidencyForkError::SourceDeleted,
        DurableSessionForkError::InvalidAnchor => SessionResidencyForkError::InvalidAnchor,
        DurableSessionForkError::SourceConversationTooLarge => {
            SessionResidencyForkError::SourceConversationTooLarge
        }
        DurableSessionForkError::SourceConversationCorrupt => {
            SessionResidencyForkError::SourceConversationCorrupt
        }
        DurableSessionForkError::AgentDisabled => SessionResidencyForkError::AgentDisabled,
        DurableSessionForkError::AgentDeleted => SessionResidencyForkError::AgentDeleted,
        DurableSessionForkError::DurableStateTooLarge => {
            SessionResidencyForkError::DurableStateTooLarge
        }
        DurableSessionForkError::IdentityUnavailable
        | DurableSessionForkError::CollisionAttemptsExhausted
        | DurableSessionForkError::SourceIdentityConflict
        | DurableSessionForkError::StorageUnavailable => SessionResidencyForkError::Unavailable,
        DurableSessionForkError::InternalDispatchUnavailable => context.internal_fork(),
    }
}

async fn run_lifecycle(
    context: OperationContext,
    attempt: SealedSessionLifecycleAttempt,
) -> Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError> {
    let session_id = attempt.session_id();
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;

    if context.state.has_loaded(session_id) {
        let current = context
            .durable_state
            .session_current(session_id)
            .ok_or_else(|| context.internal_lifecycle())?;
        if !valid_current_shape(session_id, current.head(), current.definition()) {
            return Err(context.internal_lifecycle());
        }
        return match attempt.decide(current.head().lifecycle()) {
            Ok(SessionLifecycleDecision::NoChange) => Ok(DurableSessionLifecycleOutcome::NoChange(
                Arc::clone(current.head()),
            )),
            Ok(SessionLifecycleDecision::Publish(_)) => {
                Err(SessionResidencyLifecycleError::SessionBusy)
            }
            Err(SessionLifecycleDecisionError::SessionDeleted) => {
                Err(SessionResidencyLifecycleError::SessionDeleted)
            }
            Err(SessionLifecycleDecisionError::InvalidTransition) => {
                Err(SessionResidencyLifecycleError::InvalidLifecycleTransition)
            }
        };
    }

    let outcome = match context
        .durable_state
        .update_session_lifecycle(attempt)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return map_durable_lifecycle_error(&context, error),
    };
    if outcome.head().session_id() != session_id {
        return Err(context.internal_lifecycle());
    }
    Ok(outcome)
}

async fn run_snapshot(
    context: OperationContext,
    session_id: SessionId,
) -> Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    let Some(executor) = context.state.executor(session_id) else {
        return Err(SessionResidencySnapshotError::SessionNotLoaded);
    };

    match executor.snapshot().await {
        Ok(snapshot) => Ok(snapshot),
        Err(SessionExecutorSnapshotError::Closing) => Err(SessionResidencySnapshotError::Closing),
        Err(SessionExecutorSnapshotError::InternalDispatchUnavailable) => {
            Err(context.internal_snapshot())
        }
    }
}

/// Performs one ordinary Session definition CAS under the per-Session gate.  The loaded
/// membership observed under this gate is the linearization point for the required loaded
/// publication: a changed durable update publishes the exact changed definition through the
/// installed executor, and any executor failure after a durable Updated is a required post-commit
/// live-publication failure that poisons the shared owners.  Normal registry shutdown drains
/// admitted children before closing executors, so a loaded executor is never observed closing
/// under an admitted definition operation.
#[allow(
    clippy::too_many_arguments,
    reason = "the child operation receives the complete fixed definition CAS and publication context"
)]
async fn run_session_definition(
    context: OperationContext,
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    workspace: Option<Workspace>,
    model: Option<SessionModelConfig>,
    prompts: Option<SessionPromptSelection>,
    owner_timestamp: Timestamp,
    command_id: CommandId,
) -> Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    if let Some(executor) = context.state.executor(session_id) {
        let outcome = match executor
            .update_session_definition_with_cancellation(
                expected_revision,
                workspace,
                model,
                prompts,
                owner_timestamp,
                command_id,
                context.closing.clone(),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return Err(map_executor_workspace_error(&context, error)),
        };
        // The executor performed the durable CAS and validated its exact outcome; the gate
        // guarantees no other mutation can interleave, so the current durable head is the exact
        // committed one.
        let current = context
            .durable_state
            .session_current(session_id)
            .ok_or_else(|| context.internal_workspace())?;
        if !valid_current_shape(session_id, current.head(), current.definition()) {
            return Err(context.internal_workspace());
        }
        return Ok(if outcome.changed() {
            DurableSessionDefinitionOutcome::Updated(
                Arc::clone(current.head()),
                Arc::clone(current.definition()),
            )
        } else {
            DurableSessionDefinitionOutcome::NoChange(
                Arc::clone(current.head()),
                Arc::clone(current.definition()),
            )
        });
    }

    let attempt = SealedSessionDefinitionAttempt::new(
        session_id,
        expected_revision,
        workspace,
        model,
        prompts,
        owner_timestamp,
    );
    let outcome = context
        .durable_state
        .update_session_definition(attempt)
        .await
        .map_err(|error| map_durable_definition_error(&context, error))?;
    map_unloaded_definition_outcome(&context, session_id, outcome)
}

/// Performs one explicit Session Agent revision upgrade under the per-Session gate.  The loaded
/// membership observed under this gate is the linearization point for the required loaded
/// publication: a changed durable upgrade publishes the exact changed definition through the
/// installed executor, and any executor failure after a durable Updated is a required post-commit
/// live-publication failure that poisons the shared owners.  Normal registry shutdown drains
/// admitted children before closing executors, so a loaded executor is never observed closing
/// under an admitted Agent upgrade operation.
async fn run_agent_upgrade(
    context: OperationContext,
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    target: Option<AgentRevisionRef>,
    owner_timestamp: Timestamp,
    command_id: CommandId,
) -> Result<DurableSessionAgentUpgradeOutcome, SessionResidencyAgentUpgradeError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    if let Some(executor) = context.state.executor(session_id) {
        let outcome = match executor
            .upgrade_session_agent_with_cancellation(
                expected_revision,
                target,
                owner_timestamp,
                command_id,
                context.closing.clone(),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return Err(map_executor_agent_upgrade_error(&context, error)),
        };
        // The executor performed the durable CAS and validated its exact outcome; the gate
        // guarantees no other mutation can interleave, so the current durable head is the exact
        // committed one.
        let current = context
            .durable_state
            .session_current(session_id)
            .ok_or_else(|| context.internal_agent_upgrade())?;
        if !valid_current_shape(session_id, current.head(), current.definition()) {
            return Err(context.internal_agent_upgrade());
        }
        let committed = if outcome.changed() {
            DurableSessionAgentUpgradeOutcome::Updated(
                Arc::clone(current.head()),
                Arc::clone(current.definition()),
            )
        } else {
            DurableSessionAgentUpgradeOutcome::NoChange(
                Arc::clone(current.head()),
                Arc::clone(current.definition()),
            )
        };
        if committed.definition().revision() != outcome.definition_revision() {
            return Err(context.internal_agent_upgrade());
        }
        return Ok(committed);
    }

    let attempt = SealedSessionAgentUpgradeAttempt::new(
        session_id,
        expected_revision,
        target,
        owner_timestamp,
    );
    let outcome = context
        .durable_state
        .upgrade_session_agent(attempt)
        .await
        .map_err(|error| map_durable_agent_upgrade_error(&context, error))?;
    let head = outcome.head();
    let definition = outcome.definition();
    if !valid_current_shape(session_id, head, definition) {
        return Err(context.internal_agent_upgrade());
    }
    Ok(outcome)
}

fn loaded_executor_and_permit(
    state: &Arc<ResidencyShared>,
    session_id: SessionId,
) -> Option<(SessionExecutor, SessionResidencyPermit)> {
    let state = lock(&state.state);
    state
        .loaded
        .get(&session_id)
        .map(|loaded| (loaded.executor.clone(), loaded.permit.clone()))
}

fn valid_current_shape(
    session_id: SessionId,
    head: &crate::durable_state::DurableSessionHead,
    definition: &crate::agent_session_lifecycle::SessionDefinition,
) -> bool {
    head.session_id() == session_id
        && definition.session_id() == session_id
        && head.current_definition_revision() == definition.revision()
}

fn map_conversation_target_load_error(
    context: &OperationContext,
    error: DurableConversationTargetError,
) -> SessionResidencyLoadError {
    match error {
        DurableConversationTargetError::Closing => SessionResidencyLoadError::Closing,
        DurableConversationTargetError::SessionNotFound => {
            SessionResidencyLoadError::SessionNotFound
        }
        DurableConversationTargetError::Corrupt => SessionResidencyLoadError::RecordedStateCorrupt,
        DurableConversationTargetError::TooLarge => SessionResidencyLoadError::DurableStateTooLarge,
        DurableConversationTargetError::StorageUnavailable => {
            SessionResidencyLoadError::StorageUnavailable
        }
        DurableConversationTargetError::InternalDispatchUnavailable => context.internal_load(),
    }
}

fn map_conversation_load_error(
    context: &OperationContext,
    error: ConversationLoadError,
) -> SessionResidencyLoadError {
    match error {
        ConversationLoadError::Replay(replay) => match replay {
            ConversationReplayError::HistoryTooLarge => {
                SessionResidencyLoadError::DurableStateTooLarge
            }
            ConversationReplayError::HeaderCorrupt
            | ConversationReplayError::UnsupportedFormatVersion
            | ConversationReplayError::MissingHeader => {
                SessionResidencyLoadError::RecordedStateCorrupt
            }
            ConversationReplayError::InputChanged | ConversationReplayError::InputUnavailable => {
                SessionResidencyLoadError::StorageUnavailable
            }
            ConversationReplayError::LeaseMismatch
            | ConversationReplayError::CounterOverflow
            | ConversationReplayError::InvariantViolation => context.internal_load(),
        },
        ConversationLoadError::TailTruncateFailed => SessionResidencyLoadError::StorageUnavailable,
        ConversationLoadError::LiveStateInvariant => context.internal_load(),
        ConversationLoadError::Runtime(RuntimeTaskError::OwnerClosing) => {
            SessionResidencyLoadError::Closing
        }
        ConversationLoadError::Runtime(
            RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable,
        ) => context.internal_load(),
    }
}

fn map_durable_lifecycle_error(
    context: &OperationContext,
    error: DurableSessionLifecycleError,
) -> Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError> {
    let error = match error {
        DurableSessionLifecycleError::Closing => SessionResidencyLifecycleError::Closing,
        DurableSessionLifecycleError::SessionNotFound => {
            SessionResidencyLifecycleError::SessionNotFound
        }
        DurableSessionLifecycleError::SessionDeleted => {
            SessionResidencyLifecycleError::SessionDeleted
        }
        DurableSessionLifecycleError::InvalidLifecycleTransition => {
            SessionResidencyLifecycleError::InvalidLifecycleTransition
        }
        DurableSessionLifecycleError::DurableStateTooLarge => {
            SessionResidencyLifecycleError::DurableStateTooLarge
        }
        DurableSessionLifecycleError::StorageUnavailable => {
            SessionResidencyLifecycleError::StorageUnavailable
        }
        DurableSessionLifecycleError::InternalDispatchUnavailable => {
            return Err(context.internal_lifecycle());
        }
    };
    Err(error)
}

fn map_executor_workspace_error(
    context: &OperationContext,
    error: SessionWorkspaceDefinitionError,
) -> SessionResidencyWorkspaceDefinitionError {
    match error {
        SessionWorkspaceDefinitionError::Closing => {
            SessionResidencyWorkspaceDefinitionError::Closing
        }
        SessionWorkspaceDefinitionError::SessionBusy => {
            SessionResidencyWorkspaceDefinitionError::SessionBusy
        }
        SessionWorkspaceDefinitionError::SessionNotFound => {
            SessionResidencyWorkspaceDefinitionError::SessionNotFound
        }
        SessionWorkspaceDefinitionError::StaleRevision => {
            SessionResidencyWorkspaceDefinitionError::StaleRevision
        }
        SessionWorkspaceDefinitionError::SessionArchived => {
            SessionResidencyWorkspaceDefinitionError::SessionArchived
        }
        SessionWorkspaceDefinitionError::SessionDeleted => {
            SessionResidencyWorkspaceDefinitionError::SessionDeleted
        }
        SessionWorkspaceDefinitionError::StateTooLarge => {
            SessionResidencyWorkspaceDefinitionError::StateTooLarge
        }
        SessionWorkspaceDefinitionError::WorkspaceUnavailable => {
            SessionResidencyWorkspaceDefinitionError::WorkspaceUnavailable
        }
        SessionWorkspaceDefinitionError::WorkspaceRejected => {
            SessionResidencyWorkspaceDefinitionError::WorkspaceRejected
        }
        SessionWorkspaceDefinitionError::StorageUnavailable => {
            SessionResidencyWorkspaceDefinitionError::StorageUnavailable
        }
        // An ordinary definition publication can never produce Agent-specific failures; an
        // impossible executor error means the actor's validation contract broke and poisons the
        // shared owners.
        SessionWorkspaceDefinitionError::AgentMismatch
        | SessionWorkspaceDefinitionError::AgentDisabled
        | SessionWorkspaceDefinitionError::AgentDeleted
        | SessionWorkspaceDefinitionError::RevisionUnavailable
        | SessionWorkspaceDefinitionError::Unauthorized => context.internal_workspace(),
        SessionWorkspaceDefinitionError::InternalDispatchUnavailable => {
            context.internal_workspace()
        }
    }
}

fn map_executor_agent_upgrade_error(
    context: &OperationContext,
    error: SessionWorkspaceDefinitionError,
) -> SessionResidencyAgentUpgradeError {
    match error {
        SessionWorkspaceDefinitionError::Closing => SessionResidencyAgentUpgradeError::Closing,
        SessionWorkspaceDefinitionError::SessionBusy => {
            SessionResidencyAgentUpgradeError::SessionBusy
        }
        SessionWorkspaceDefinitionError::SessionNotFound => {
            SessionResidencyAgentUpgradeError::SessionNotFound
        }
        SessionWorkspaceDefinitionError::StaleRevision => {
            SessionResidencyAgentUpgradeError::StaleRevision
        }
        SessionWorkspaceDefinitionError::SessionArchived => {
            SessionResidencyAgentUpgradeError::SessionArchived
        }
        SessionWorkspaceDefinitionError::SessionDeleted => {
            SessionResidencyAgentUpgradeError::SessionDeleted
        }
        SessionWorkspaceDefinitionError::AgentMismatch => {
            SessionResidencyAgentUpgradeError::AgentMismatch
        }
        SessionWorkspaceDefinitionError::AgentDisabled => {
            SessionResidencyAgentUpgradeError::AgentDisabled
        }
        SessionWorkspaceDefinitionError::AgentDeleted => {
            SessionResidencyAgentUpgradeError::AgentDeleted
        }
        SessionWorkspaceDefinitionError::RevisionUnavailable => {
            SessionResidencyAgentUpgradeError::RevisionUnavailable
        }
        SessionWorkspaceDefinitionError::StateTooLarge => {
            SessionResidencyAgentUpgradeError::DurableStateTooLarge
        }
        // An Agent upgrade never invokes the Workspace resolver and never captures Prompt or
        // Skill sources, so a Workspace-specific executor error is impossible; an impossible
        // executor error means the actor's validation contract broke and poisons the shared
        // owners.
        SessionWorkspaceDefinitionError::WorkspaceUnavailable
        | SessionWorkspaceDefinitionError::WorkspaceRejected
        | SessionWorkspaceDefinitionError::Unauthorized => context.internal_agent_upgrade(),
        SessionWorkspaceDefinitionError::StorageUnavailable => {
            SessionResidencyAgentUpgradeError::StorageUnavailable
        }
        SessionWorkspaceDefinitionError::InternalDispatchUnavailable => {
            context.internal_agent_upgrade()
        }
    }
}

fn map_durable_agent_upgrade_error(
    context: &OperationContext,
    error: DurableSessionAgentUpgradeError,
) -> SessionResidencyAgentUpgradeError {
    match error {
        DurableSessionAgentUpgradeError::Closing => SessionResidencyAgentUpgradeError::Closing,
        DurableSessionAgentUpgradeError::SessionNotFound => {
            SessionResidencyAgentUpgradeError::SessionNotFound
        }
        DurableSessionAgentUpgradeError::StaleRevision => {
            SessionResidencyAgentUpgradeError::StaleRevision
        }
        DurableSessionAgentUpgradeError::SessionArchived => {
            SessionResidencyAgentUpgradeError::SessionArchived
        }
        DurableSessionAgentUpgradeError::SessionDeleted => {
            SessionResidencyAgentUpgradeError::SessionDeleted
        }
        DurableSessionAgentUpgradeError::AgentMismatch => {
            SessionResidencyAgentUpgradeError::AgentMismatch
        }
        DurableSessionAgentUpgradeError::AgentDisabled => {
            SessionResidencyAgentUpgradeError::AgentDisabled
        }
        DurableSessionAgentUpgradeError::AgentDeleted => {
            SessionResidencyAgentUpgradeError::AgentDeleted
        }
        DurableSessionAgentUpgradeError::RevisionUnavailable => {
            SessionResidencyAgentUpgradeError::RevisionUnavailable
        }
        DurableSessionAgentUpgradeError::DurableStateTooLarge => {
            SessionResidencyAgentUpgradeError::DurableStateTooLarge
        }
        DurableSessionAgentUpgradeError::StorageUnavailable => {
            SessionResidencyAgentUpgradeError::StorageUnavailable
        }
        DurableSessionAgentUpgradeError::InternalDispatchUnavailable => {
            context.internal_agent_upgrade()
        }
    }
}

/// Performs one loaded Session Workspace reload under the per-Session gate shared with Load,
/// Unload, Fork, Lifecycle, Metadata, ordinary definition CAS, and Agent upgrade.  The reload is
/// a loaded-only operation: it never reads or updates DurableState, and it only succeeds while
/// the installed executor is Idle with no active publication.  The gate covers the loaded
/// membership decision and the full executor reload completion, so a concurrent Load/Unload
/// cannot slip between them.  An executor missing under the gate maps directly to
/// SessionNotLoaded.
async fn run_workspace_reload(
    context: OperationContext,
    session_id: SessionId,
    owner_timestamp: Timestamp,
    command_id: CommandId,
) -> Result<SessionDefinitionPublicationOutcome, SessionResidencyWorkspaceReloadError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    let Some(executor) = context.state.executor(session_id) else {
        return Err(SessionResidencyWorkspaceReloadError::SessionNotLoaded);
    };

    executor
        .reload_workspace_with_cancellation(owner_timestamp, command_id, context.closing.clone())
        .await
        .map_err(|error| map_executor_workspace_reload_error(&context, error))
}

/// Applies one Agent availability fact to one loaded Session under the per-Session gate.  An
/// executor missing under the gate is an Unload-first NoChange; the installed definition must
/// still pin the requested AgentId (a mismatch means the Runtime enumeration and the executor
/// disagree and is internal poison).  The operation never reads or writes DurableState.
async fn run_agent_availability(
    context: OperationContext,
    session_id: SessionId,
    agent_id: AgentId,
    available: bool,
    timestamp: Timestamp,
    command_id: CommandId,
) -> Result<(), SessionResidencyAgentAvailabilityError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    let Some(executor) = context.state.executor(session_id) else {
        return Ok(());
    };
    if executor
        .published_snapshot()
        .definition()
        .agent()
        .agent_id()
        != agent_id
    {
        return Err(context.internal_agent_availability());
    }
    executor
        .set_agent_availability_with_cancellation(
            agent_id,
            available,
            timestamp,
            command_id,
            CancellationToken::new(),
        )
        .await
        .map_err(|error| match error {
            SessionAgentAvailabilityError::Closing => {
                SessionResidencyAgentAvailabilityError::Closing
            }
            SessionAgentAvailabilityError::InternalDispatchUnavailable => {
                context.internal_agent_availability()
            }
        })
}

/// The two-phase Runtime shared-resource installation over every loaded Session.
///
/// Phase (a) precomputes, for every loaded Session's exact installed definition, the
/// model availability against the candidate Model catalog and the selected-Prompt availability
/// against the candidate Prompt resources, before any executor is touched.  Ordinary model
/// incompatibilities and ordinary selection failures degrade only the boolean fact; Closing
/// stays Closing (a pre-install ordinary outcome, never a post-install required-publication
/// failure), and every other failure on the installed Runtime-owned catalog/Prompt view is an
/// internal invariant that poisons the shared owners.
///
/// Phase (b) constructs the new ResidencyTurnResources (same model gateway, tool set, and
/// compaction settings; new Prompt/Model roots) and installs it into every loaded executor in
/// sorted SessionId order under each per-Session gate, re-reading the loaded executor and its
/// snapshot under the gate: an executor that disappeared or whose installed definition no
/// longer matches the precomputed exact Arc is a closing/internal invariant under the Runtime
/// global publication, never a silent NoChange.  Once phase (b) has begun, earlier executors
/// may already hold the new roots, so every executor error — including Closing from the shared
/// closing token — is a required live-publication failure of a partially completed fan-out and
/// maps to internal poison, never a plain Closing; only the precompute (phase (a)) keeps the
/// typed Closing outcome, because no executor has been touched yet.  Only after every Session
/// succeeds does the completion return the new ResidencyTurnResources for the actor to install
/// into its own future Turn resources.
async fn run_shared_resources(
    context: OperationContext,
    prompt_resources: Arc<PromptResourceView>,
    model_catalog: Arc<ModelCatalogView>,
    timestamp: Timestamp,
    command_id: CommandId,
) -> Result<ResidencyTurnResources, SessionResidencySharedResourcesError> {
    // Phase (a): precompute every availability fact before any executor update.  The loaded
    // snapshots are already sorted by SessionId (the residency map is a BTreeMap), and the
    // precompute order is irrelevant; only the completion of the whole precompute before any
    // install matters.
    struct PrecomputedSession {
        definition: Arc<SessionDefinition>,
        model_available: bool,
        prompt_available: bool,
    }
    let mut precomputed = Vec::new();
    for snapshot in context.state.loaded_session_snapshots() {
        let definition = Arc::clone(snapshot.definition());
        let Some(resources) = context.turn_resources.as_ref() else {
            // The production Runtime always starts residency with Turn resources; the test-only
            // dependency shape never routes a shared-resource installation.
            return Err(context.internal_shared_resources());
        };
        let model_available = match model_available_for_definition(
            &resources.model_gateway,
            Arc::clone(&model_catalog),
            &definition,
        ) {
            Ok(available) => available,
            Err(_) => return Err(context.internal_shared_resources()),
        };
        let prompt_available = match prompt_available_for_definition(
            context.durable_state.clone(),
            Arc::clone(&context.prompt_service),
            Arc::clone(&prompt_resources),
            &definition,
        )
        .await
        {
            Ok(available) => available,
            Err(SessionPromptAvailabilityError::Closing) => {
                return Err(SessionResidencySharedResourcesError::Closing);
            }
            Err(SessionPromptAvailabilityError::InternalDispatchUnavailable) => {
                return Err(context.internal_shared_resources());
            }
        };
        precomputed.push(PrecomputedSession {
            definition,
            model_available,
            prompt_available,
        });
    }

    // Phase (b): install into every loaded executor under its own gate, then hand the new
    // ResidencyTurnResources back to the actor for its future Loads.
    let Some(current) = context.turn_resources.as_ref() else {
        return Err(context.internal_shared_resources());
    };
    let new_resources = ResidencyTurnResources {
        prompt_resources: Arc::clone(&prompt_resources),
        model_gateway: Arc::clone(&current.model_gateway),
        model_catalog: Arc::clone(&model_catalog),
        // The Tool resource carrier is preserved unchanged: a captured ToolSet stays the
        // exact captured set, and the production config stays frozen (per-admission
        // materialization against the future Workspace snapshot).
        tools: current.tools.clone(),
        compaction: current.compaction.clone(),
    };
    for entry in &precomputed {
        let session_id = entry.definition.session_id();
        let gate = context.state.gate(session_id);
        let _permit = SessionResidencyOperationPermit::acquire(gate).await;
        let Some(executor) = context.state.executor(session_id) else {
            return Err(context.internal_shared_resources());
        };
        if !Arc::ptr_eq(
            executor.published_snapshot().definition(),
            &entry.definition,
        ) {
            return Err(context.internal_shared_resources());
        }
        executor
            .update_shared_resources_with_cancellation(
                Arc::clone(&entry.definition),
                Arc::clone(&prompt_resources),
                Arc::clone(&model_catalog),
                entry.prompt_available,
                entry.model_available,
                timestamp,
                command_id,
                context.closing.clone(),
            )
            .await
            // Once phase (b) has begun, earlier executors may already have installed the new
            // roots; a partially completed fan-out cannot report a plain Closing, so every
            // executor update error — including the closing token's — poisons the shared owners.
            .map_err(|_| context.internal_shared_resources())?;
    }
    Ok(new_resources)
}

fn map_executor_workspace_reload_error(
    context: &OperationContext,
    error: SessionDefinitionPublicationError,
) -> SessionResidencyWorkspaceReloadError {
    match error {
        SessionDefinitionPublicationError::Closing => SessionResidencyWorkspaceReloadError::Closing,
        SessionDefinitionPublicationError::SessionBusy => {
            SessionResidencyWorkspaceReloadError::SessionBusy
        }
        SessionDefinitionPublicationError::WorkspaceUnavailable => {
            SessionResidencyWorkspaceReloadError::WorkspaceUnavailable
        }
        SessionDefinitionPublicationError::WorkspaceRejected => {
            SessionResidencyWorkspaceReloadError::WorkspaceRejected
        }
        SessionDefinitionPublicationError::Unauthorized => {
            SessionResidencyWorkspaceReloadError::Unauthorized
        }
        // A reload never touches DurableState and never performs a durable CAS, so every other
        // executor error is impossible on this seam; an impossible executor error means the
        // actor's validation contract broke and poisons the shared owners.
        SessionDefinitionPublicationError::SessionNotFound
        | SessionDefinitionPublicationError::StaleRevision
        | SessionDefinitionPublicationError::SessionArchived
        | SessionDefinitionPublicationError::SessionDeleted
        | SessionDefinitionPublicationError::AgentMismatch
        | SessionDefinitionPublicationError::AgentDisabled
        | SessionDefinitionPublicationError::AgentDeleted
        | SessionDefinitionPublicationError::RevisionUnavailable
        | SessionDefinitionPublicationError::StateTooLarge
        | SessionDefinitionPublicationError::StorageUnavailable
        | SessionDefinitionPublicationError::InternalDispatchUnavailable => {
            context.internal_workspace_reload()
        }
    }
}

fn map_durable_definition_error(
    context: &OperationContext,
    error: DurableSessionDefinitionError,
) -> SessionResidencyWorkspaceDefinitionError {
    match error {
        DurableSessionDefinitionError::Closing => SessionResidencyWorkspaceDefinitionError::Closing,
        DurableSessionDefinitionError::SessionNotFound => {
            SessionResidencyWorkspaceDefinitionError::SessionNotFound
        }
        DurableSessionDefinitionError::StaleRevision => {
            SessionResidencyWorkspaceDefinitionError::StaleRevision
        }
        DurableSessionDefinitionError::SessionArchived => {
            SessionResidencyWorkspaceDefinitionError::SessionArchived
        }
        DurableSessionDefinitionError::SessionDeleted => {
            SessionResidencyWorkspaceDefinitionError::SessionDeleted
        }
        DurableSessionDefinitionError::DurableStateTooLarge => {
            SessionResidencyWorkspaceDefinitionError::StateTooLarge
        }
        DurableSessionDefinitionError::StorageUnavailable => {
            SessionResidencyWorkspaceDefinitionError::StorageUnavailable
        }
        DurableSessionDefinitionError::InternalDispatchUnavailable => context.internal_workspace(),
    }
}

/// Performs one Session metadata CAS under the per-Session gate.  The loaded membership observed
/// under this gate is the linearization point for the required loaded publication: a changed
/// durable update publishes the exact changed metadata through the installed executor, and any
/// executor failure after a durable Updated is a required post-commit live-publication failure that
/// poisons the shared owners.  Normal registry shutdown drains admitted children before closing
/// executors, so a loaded executor is never observed closing under an admitted metadata operation.
async fn run_metadata(
    context: OperationContext,
    attempt: SealedSessionMetadataAttempt,
    timestamp: Timestamp,
    command_id: CommandId,
) -> Result<DurableSessionMetadataOutcome, SessionResidencyMetadataError> {
    let session_id = attempt.session_id();
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    let loaded_executor = context.state.executor(session_id);
    let outcome = match context.durable_state.update_session_metadata(attempt).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return Err(match error {
                DurableSessionMetadataError::Closing => SessionResidencyMetadataError::Closing,
                DurableSessionMetadataError::SessionNotFound => {
                    SessionResidencyMetadataError::SessionNotFound
                }
                DurableSessionMetadataError::StaleRevision => {
                    SessionResidencyMetadataError::StaleRevision
                }
                DurableSessionMetadataError::SessionDeleted => {
                    SessionResidencyMetadataError::SessionDeleted
                }
                DurableSessionMetadataError::DurableStateTooLarge => {
                    SessionResidencyMetadataError::DurableStateTooLarge
                }
                DurableSessionMetadataError::StorageUnavailable => {
                    SessionResidencyMetadataError::StorageUnavailable
                }
                DurableSessionMetadataError::InternalDispatchUnavailable => {
                    return Err(context.internal_metadata());
                }
            });
        }
    };
    if outcome.head().session_id() != session_id {
        return Err(context.internal_metadata());
    }
    if let (DurableSessionMetadataOutcome::Updated(head), Some(executor)) =
        (&outcome, loaded_executor)
    {
        let published = executor
            .publish_metadata(Arc::new(head.metadata().clone()), timestamp, command_id)
            .await;
        if published.is_err() {
            return Err(context.internal_metadata());
        }
    }
    Ok(outcome)
}

/// Validates the unloaded durable outcome shape and returns the exact committed outcome for the
/// Runtime event/outcome projection.
fn map_unloaded_definition_outcome(
    context: &OperationContext,
    session_id: SessionId,
    outcome: DurableSessionDefinitionOutcome,
) -> Result<DurableSessionDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
    let head = outcome.head();
    let definition = outcome.definition();
    if !valid_current_shape(session_id, head, definition) {
        return Err(context.internal_workspace());
    }
    Ok(outcome)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::future::{Future, poll_fn};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::runtime::Handle;

    use crate::agent_session_lifecycle::{
        SealedSessionDefinitionAttempt, SealedSessionLifecycleAttempt,
    };
    use crate::conversation_storage::{RecordOutcome, RecorderWriteBarrier};
    use crate::durable_state::DurableState;
    use crate::model_gateway::{ModelSelection, ReasoningPreference, ScriptedModelFixture};
    use crate::prompt::{
        PromptBodyIntent, PromptSourceError, TextIntent, WorkspacePromptSource,
        WorkspacePromptSourceAdapter, WorkspacePromptSourceFuture,
    };
    use crate::runtime_task::RuntimeTaskContext;
    use crate::wire::conversation_jsonl::ConversationLineCodec;
    use crate::wire::{CanonicalFileUri, FileUriFamily, SessionId};
    use crate::workspace::{
        RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspacePathTarget,
        WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy, lower_workspace,
    };

    const AGENT_ID: &str = "agt_11111111111111111111111111111111";
    const SESSION_ID: &str = "ses_22222222222222222222222222222222";
    const G1: &str = "00000000000000000001";

    static NEXT_TEST_ROOT: AtomicUsize = AtomicUsize::new(1);

    struct MutableWorkspacePromptSource {
        result: Mutex<Result<Vec<WorkspacePromptSource>, PromptSourceError>>,
        calls: AtomicUsize,
        block_next: AtomicBool,
        entered: AtomicBool,
        released: AtomicBool,
        entered_changed: Notify,
    }

    impl MutableWorkspacePromptSource {
        fn unavailable() -> Self {
            Self {
                result: Mutex::new(Err(PromptSourceError::Unavailable)),
                calls: AtomicUsize::new(0),
                block_next: AtomicBool::new(false),
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
                entered_changed: Notify::new(),
            }
        }

        fn replace(&self, content: &str) {
            let source = WorkspacePromptSource::new(
                "repo".parse().unwrap(),
                "AGENTS.md".parse().unwrap(),
                Arc::from(content),
            );
            *lock(&self.result) = Ok(vec![source]);
        }

        fn fail(&self) {
            *lock(&self.result) = Err(PromptSourceError::Unavailable);
        }

        fn block_next(&self) {
            self.entered.store(false, Ordering::Release);
            self.released.store(false, Ordering::Release);
            self.block_next.store(true, Ordering::Release);
        }

        async fn wait_until_entered(&self) {
            loop {
                let notified = self.entered_changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.entered.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn release_blocked(&self) {
            self.released.store(true, Ordering::Release);
            self.entered_changed.notify_waiters();
        }
    }

    impl WorkspacePromptSourceAdapter for MutableWorkspacePromptSource {
        fn capture<'a>(
            &'a self,
            _context: &'a crate::workspace::WorkspacePromptCaptureContext,
        ) -> WorkspacePromptSourceFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = lock(&self.result).clone();
            Box::pin(async move {
                if self.block_next.swap(false, Ordering::AcqRel) {
                    self.entered.store(true, Ordering::Release);
                    self.entered_changed.notify_waiters();
                    loop {
                        let notified = self.entered_changed.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        if self.released.load(Ordering::Acquire) {
                            break;
                        }
                        notified.await;
                    }
                }
                result
            })
        }
    }

    fn session_candidate(value: u128) -> SessionId {
        format!("ses_{value:032x}")
            .parse()
            .expect("a nonzero test SessionId is canonical")
    }

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
                    "minicore-session-residency-store-{}-{number}",
                    std::process::id()
                ));
                if root.exists() {
                    continue;
                }
                fs::create_dir(&root).expect("the temporary Store root is created");
                set_private_directory_mode(&root);
                let old_workspace = root.with_file_name(format!(
                    "minicore-session-residency-workspace-old-{}-{number}",
                    std::process::id()
                ));
                let new_workspace = root.with_file_name(format!(
                    "minicore-session-residency-workspace-new-{}-{number}",
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
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.old_workspace);
            let _ = fs::remove_dir_all(&self.new_workspace);
        }
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

    fn replayed_user_conversation_fixture() -> Vec<u8> {
        let source = include_str!(
            "../docs/fixtures/wire-v1/conversation/golden/user-sources-and-stamps.jsonl"
        );
        let mut lines = source.lines();
        let header = lines
            .next()
            .expect("the replay fixture has a Header")
            .replace("ses_12121212121212121212121212121212", SESSION_ID)
            .replace("2026-07-31T14:00:00.000Z", "2026-08-03T10:01:00.456Z")
            .replace("agt_23232323232323232323232323232323", AGENT_ID);
        let entry = lines
            .next()
            .expect("the replay fixture has a User entry")
            .replace("ses_12121212121212121212121212121212", SESSION_ID);
        format!("{header}\n{entry}\n").into_bytes()
    }

    fn replayed_user_append_entry_fixture() -> Vec<u8> {
        let source = include_str!(
            "../docs/fixtures/wire-v1/conversation/golden/user-sources-and-stamps.jsonl"
        );
        source
            .lines()
            .nth(2)
            .expect("the replay fixture has a second User entry")
            .replace("ses_12121212121212121212121212121212", SESSION_ID)
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
        let state = DurableState::open(root.to_owned(), context.clone())
            .await
            .expect("the fixture Store opens");
        (context, state)
    }

    async fn open_registry(
        store: &TempStore,
    ) -> (RuntimeTaskContext, DurableState, SessionResidencyRegistry) {
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let registry = SessionResidencyRegistry::start(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .expect("the residency actor starts");
        (context, state, registry)
    }

    /// Clears the fixture Agent/Session prompt selections so the installed Runtime Prompt
    /// resources trivially resolve them; the loaded readiness facts otherwise project
    /// PromptUnavailable for a recovered Session, exactly like the executor test fixture does.
    fn empty_fixture_prompt_selections(store: &TempStore) {
        for (path, from) in [
            (
                store
                    .root
                    .join("agents")
                    .join(AGENT_ID)
                    .join("generations")
                    .join(G1)
                    .join("definition.json"),
                r#""promptIds":["base","safety"]"#,
            ),
            (
                store
                    .root
                    .join("sessions")
                    .join(SESSION_ID)
                    .join("generations")
                    .join(G1)
                    .join("definition.json"),
                r#""promptIds":["base","session-notes"]"#,
            ),
        ] {
            let bytes = fs::read(&path).expect("the fixture definition is readable");
            create_file(&path, &replace_fixture(&bytes, from, r#""promptIds":[]"#));
        }
    }

    /// Starts a residency actor with a scripted model fixture that resolves the fixture
    /// definition's `openai/gpt-5` selection, so an ordinary availability failure degrades only
    /// the loaded readiness instead of failing Load with the missing-dependency shape.
    async fn open_registry_with_turn_resources(
        context: RuntimeTaskContext,
        state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
    ) -> SessionResidencyRegistry {
        let model = ScriptedModelFixture::new(Vec::new());
        let prompt_resources = prompt_service.initialize().await.unwrap();
        SessionResidencyRegistry::start_with_turn_resources(
            context,
            state,
            resolver,
            prompt_service,
            prompt_resources,
            Arc::clone(model.gateway()),
            Arc::clone(model.catalog()),
            CompactionSettings::default()
                .validate()
                .expect("default compaction settings are valid"),
        )
        .expect("the residency actor starts")
    }

    async fn close_fixture(
        context: RuntimeTaskContext,
        state: DurableState,
        registry: SessionResidencyRegistry,
    ) {
        registry.close().await;
        state.close().await;
        assert_eq!(context.registered_task_count_for_test(), 0);
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

    #[tokio::test(flavor = "current_thread")]
    async fn pre_poll_registry_actor_abort_poison_settles_waiters_and_closes_owners() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        context.abort_latest_registered_task();
        assert!(matches!(
            registry.load_ready_idle(session_id).await,
            Err(SessionResidencyLoadError::InternalDispatchUnavailable)
        ));
        registry.close().await;
        state.close().await;
        assert_eq!(context.registered_task_count_for_test(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_and_loaded_workspace_publication_capture_prompt_sources() {
        let store = TempStore::new();
        empty_fixture_prompt_selections(&store);
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new_with_source_grants_for_test(
            context.clone(),
            true,
            false,
        ));
        let source = Arc::new(MutableWorkspacePromptSource::unavailable());
        let adapter: Arc<dyn WorkspacePromptSourceAdapter> = source.clone();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), vec![adapter]).unwrap(),
        );
        let registry = open_registry_with_turn_resources(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        // An unavailable Workspace Prompt source degrades the loaded Session to Idle
        // WorkspaceUnavailable instead of failing Load: no WorkspaceSnapshot is installed and
        // Submit settles SessionNotReady with the exact cause.
        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        assert_eq!(source.call_count(), 1);
        assert_eq!(registry.loaded_count_for_test(), 1);
        let unavailable = registry.snapshot(session_id).await.unwrap();
        assert_eq!(
            unavailable.execution_state(),
            crate::session_execution::SessionExecutionState::Idle
        );
        assert_eq!(
            unavailable.readiness(),
            SessionReadinessView::Unavailable(SessionUnavailableView::WorkspaceUnavailable)
        );
        assert!(
            unavailable.workspace_optional().is_none(),
            "an unavailable Load installs no WorkspaceSnapshot"
        );
        assert!(matches!(
            registry
                .submit(
                    session_id,
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("ping").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Err(SessionResidencySubmitError::SessionNotReady(
                SessionUnavailableView::WorkspaceUnavailable
            ))
        ));

        // Restoring the Prompt source lets the loaded Session recover through ReloadWorkspace:
        // it installs a fresh snapshot and returns to Ready while the durable definition
        // revision stays untouched.
        source.replace("first project prompt");
        let reloaded = registry
            .reload_workspace(
                session_id,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
            )
            .await
            .expect("the loaded Session reloads its Workspace after the Prompt source recovers");
        assert!(reloaded.changed());
        assert_eq!(reloaded.definition_revision().get(), 1);
        assert_eq!(reloaded.workspace_revision().get(), 1);
        let recovered = registry.snapshot(session_id).await.unwrap();
        assert_eq!(recovered.readiness(), SessionReadinessView::Ready);
        let recovered_prompt = recovered.workspace().prompt_context();
        assert_eq!(recovered_prompt.sources().len(), 1);
        assert_eq!(
            recovered_prompt.sources()[0].content(),
            "first project prompt"
        );
        assert_eq!(
            recovered_prompt.sources()[0].relative_location().as_str(),
            "AGENTS.md"
        );

        // A loaded Workspace publication captures its Prompt candidate: a failed capture keeps
        // the last-good snapshot and a recovered one publishes the new prompt sources.
        source.fail();
        assert!(matches!(
            registry
                .update_workspace_definition(
                    session_id,
                    recovered.definition_revision(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionResidencyWorkspaceDefinitionError::WorkspaceUnavailable)
        ));
        let unchanged = registry.snapshot(session_id).await.unwrap();
        let unchanged_prompt = unchanged.workspace().prompt_context();
        assert_eq!(unchanged.definition_revision().get(), 1);
        assert_eq!(
            unchanged_prompt.sources()[0].content(),
            "first project prompt"
        );

        source.replace("second project prompt");
        let outcome = registry
            .update_workspace_definition(
                session_id,
                recovered.definition_revision(),
                changed_workspace(&store.new_workspace),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await
            .expect("the loaded Workspace publication captures its Prompt candidate");
        assert!(outcome.changed());
        let second = registry.snapshot(session_id).await.unwrap();
        let second_prompt = second.workspace().prompt_context();
        assert_eq!(second_prompt.sources().len(), 1);
        assert_eq!(
            second_prompt.sources()[0].content(),
            "second project prompt"
        );
        assert_eq!(source.call_count(), 4);
        assert_eq!(
            state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            2
        );

        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_revalidates_prompt_authority_after_replay_before_install() {
        let store = TempStore::new();
        let (context, state) = open_state(&store.root).await;
        let prompt_grant = Arc::new(AtomicBool::new(true));
        let resolver = Arc::new(WorkspaceResolver::new_with_mutable_prompt_grant_for_test(
            context.clone(),
            Arc::clone(&prompt_grant),
        ));
        let source = Arc::new(MutableWorkspacePromptSource::unavailable());
        source.replace("captured project prompt");
        let adapter: Arc<dyn WorkspacePromptSourceAdapter> = source.clone();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), vec![adapter]).unwrap(),
        );
        let registry = open_registry_with_turn_resources(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .await;
        let barrier = ReplayPreparationBarrier::new();
        barrier.arm_before_recorder();
        registry.set_replay_preparation_barrier_for_test(Some(Arc::clone(&barrier)));
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        tokio::select! {
            _ = barrier.wait_until_before_recorder() => {}
            result = &mut load => {
                panic!("Load settled before replay preparation paused: {result:?}")
            }
        }
        prompt_grant.store(false, Ordering::Release);
        barrier.release_before_recorder();

        // The post-replay authority revalidation failure degrades the loaded Session to Idle
        // WorkspaceUnavailable instead of failing Load: the replayed conversation and its
        // Recorder owner still install with the exact durable definition revision.
        assert_eq!(load.await, Ok(SessionResidencyLoadOutcome::Loaded));
        assert_eq!(source.call_count(), 1);
        assert_eq!(registry.loaded_count_for_test(), 1);
        assert_eq!(registry.gate_count_for_test(), 1);
        let snapshot = registry.snapshot(session_id).await.unwrap();
        assert_eq!(
            snapshot.execution_state(),
            crate::session_execution::SessionExecutionState::Idle
        );
        assert_eq!(
            snapshot.readiness(),
            SessionReadinessView::Unavailable(SessionUnavailableView::WorkspaceUnavailable)
        );
        assert!(snapshot.workspace_optional().is_none());
        let executor = registry
            .executor_for_test(session_id)
            .expect("the revalidated Load installs its executor");
        assert!(
            executor.live_state_for_test().is_some(),
            "the replayed conversation owner is installed"
        );
        assert!(
            executor.recorder_for_test().is_some(),
            "the replay Recorder owner is installed"
        );
        assert_eq!(
            state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            1
        );

        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_residency_loads_ready_after_host_recovery_without_rewriting_durable() {
        let store = TempStore::new();
        empty_fixture_prompt_selections(&store);
        let conversation_path = store
            .root
            .join("sessions")
            .join(SESSION_ID)
            .join("conversation.jsonl");
        let definition_path = store
            .root
            .join("sessions")
            .join(SESSION_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let conversation_before = fs::read(&conversation_path).unwrap();
        let definition_before = fs::read(&definition_path).unwrap();
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        // First residency while host Workspace resources are unavailable: Load installs the
        // Session as Idle WorkspaceUnavailable instead of failing.
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new_with_source_grants_for_test(
            context.clone(),
            true,
            false,
        ));
        let unavailable_source = Arc::new(MutableWorkspacePromptSource::unavailable());
        let adapter: Arc<dyn WorkspacePromptSourceAdapter> = unavailable_source.clone();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), vec![adapter]).unwrap(),
        );
        let registry = open_registry_with_turn_resources(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .await;
        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        assert_eq!(registry.loaded_count_for_test(), 1);
        let unavailable = registry.snapshot(session_id).await.unwrap();
        assert_eq!(
            unavailable.readiness(),
            SessionReadinessView::Unavailable(SessionUnavailableView::WorkspaceUnavailable)
        );
        assert!(unavailable.workspace_optional().is_none());

        // Unload closes the first residency; closing the whole owner set never rewrites the
        // durable definition or conversation.
        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        close_fixture(context, state, registry).await;
        assert_eq!(fs::read(&conversation_path).unwrap(), conversation_before);
        assert_eq!(fs::read(&definition_path).unwrap(), definition_before);

        // The host resources recover before the owner restart: a fresh Runtime, DurableState,
        // resolver, available Prompt source, and residency actor reopen the same Store root and
        // Load replays the exact durable conversation and installs Ready with the captured
        // snapshot at the same durable definition revision.
        let (restarted_context, restarted_state) = open_state(&store.root).await;
        let restarted_resolver = Arc::new(WorkspaceResolver::new_with_source_grants_for_test(
            restarted_context.clone(),
            true,
            false,
        ));
        let restored_source = Arc::new(MutableWorkspacePromptSource::unavailable());
        restored_source.replace("restored project prompt");
        let restarted_adapter: Arc<dyn WorkspacePromptSourceAdapter> = restored_source.clone();
        let restarted_prompt_service = Arc::new(
            PromptService::new(
                Arc::from("required"),
                None,
                Vec::new(),
                vec![restarted_adapter],
            )
            .unwrap(),
        );
        let restarted_registry = open_registry_with_turn_resources(
            restarted_context.clone(),
            restarted_state.clone(),
            restarted_resolver,
            restarted_prompt_service,
        )
        .await;
        assert_eq!(
            restarted_registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        assert_eq!(restarted_registry.loaded_count_for_test(), 1);
        let ready = restarted_registry.snapshot(session_id).await.unwrap();
        assert_eq!(ready.readiness(), SessionReadinessView::Ready);
        assert_eq!(ready.definition_revision().get(), 1);
        assert_eq!(ready.workspace_revision().get(), 1);
        let prompt = ready.workspace().prompt_context();
        assert_eq!(prompt.sources().len(), 1);
        assert_eq!(prompt.sources()[0].content(), "restored project prompt");
        assert_eq!(restored_source.call_count(), 1);

        // The restarted residency replayed the durable conversation through its own owners and
        // left both durable files byte-for-byte untouched at the original definition revision.
        let executor = restarted_registry
            .executor_for_test(session_id)
            .expect("the restarted residency installs its own executor");
        assert!(
            executor.live_state_for_test().is_some(),
            "the restarted Load replays the durable conversation"
        );
        let diagnostics = executor
            .replay_diagnostics_for_test()
            .expect("the restarted Load retains replay diagnostics");
        assert_eq!(
            diagnostics
                .count(crate::conversation_storage::ConversationReplayDiagnosticCode::PartialTail),
            0
        );
        let recorder = executor
            .recorder_for_test()
            .expect("the restarted Load installs its own Recorder");
        assert!(matches!(
            &*recorder.health(),
            crate::conversation_storage::RecordingHealth::Healthy
        ));
        assert_eq!(fs::read(&conversation_path).unwrap(), conversation_before);
        assert_eq!(fs::read(&definition_path).unwrap(), definition_before);
        assert_eq!(
            restarted_state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            1
        );

        assert_eq!(
            restarted_registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        close_fixture(restarted_context, restarted_state, restarted_registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loaded_publication_rejects_changed_prompt_authority_and_keeps_old_snapshot() {
        let store = TempStore::new();
        let (context, state) = open_state(&store.root).await;
        let prompt_grant = Arc::new(AtomicBool::new(true));
        let resolver = Arc::new(WorkspaceResolver::new_with_mutable_prompt_grant_for_test(
            context.clone(),
            Arc::clone(&prompt_grant),
        ));
        let source = Arc::new(MutableWorkspacePromptSource::unavailable());
        source.replace("first project prompt");
        let adapter: Arc<dyn WorkspacePromptSourceAdapter> = source.clone();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), vec![adapter]).unwrap(),
        );
        let registry = SessionResidencyRegistry::start(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .expect("the residency actor starts");
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let first = registry.snapshot(session_id).await.unwrap();

        source.replace("replacement project prompt");
        source.block_next();
        let mut update = Box::pin(registry.update_workspace_definition(
            session_id,
            first.definition_revision(),
            changed_workspace(&store.new_workspace),
            "2026-08-03T10:02:00.000Z".parse().unwrap(),
        ));
        tokio::select! {
            _ = source.wait_until_entered() => {}
            result = &mut update => {
                panic!("Workspace publication settled before Prompt capture paused: {result:?}")
            }
        }
        prompt_grant.store(false, Ordering::Release);
        source.release_blocked();

        assert!(matches!(
            update.await,
            Err(SessionResidencyWorkspaceDefinitionError::WorkspaceUnavailable)
        ));
        let unchanged = registry.snapshot(session_id).await.unwrap();
        assert_eq!(unchanged.definition_revision().get(), 1);
        assert_eq!(
            unchanged.workspace().prompt_context().sources()[0].content(),
            "first project prompt"
        );
        assert_eq!(
            state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            1
        );

        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_closing_cancels_blocked_workspace_prompt_capture() {
        let store = TempStore::new();
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new_with_source_grants_for_test(
            context.clone(),
            true,
            false,
        ));
        let source = Arc::new(MutableWorkspacePromptSource::unavailable());
        source.replace("blocked project prompt");
        source.block_next();
        let adapter: Arc<dyn WorkspacePromptSourceAdapter> = source.clone();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), vec![adapter]).unwrap(),
        );
        let registry = SessionResidencyRegistry::start(
            context.clone(),
            state.clone(),
            Arc::clone(&resolver),
            prompt_service,
        )
        .expect("the residency actor starts");
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        tokio::select! {
            _ = source.wait_until_entered() => {}
            result = &mut load => panic!("Load settled before Prompt capture blocked: {result:?}"),
        }
        registry.close().await;
        assert_eq!(load.await, Err(SessionResidencyLoadError::Closing));
        assert_eq!(registry.loaded_count_for_test(), 0);
        state.close().await;
        assert_eq!(context.registered_task_count_for_test(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executor_closing_cancels_blocked_workspace_prompt_publication() {
        let store = TempStore::new();
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new_with_source_grants_for_test(
            context.clone(),
            true,
            false,
        ));
        let source = Arc::new(MutableWorkspacePromptSource::unavailable());
        source.replace("first project prompt");
        let adapter: Arc<dyn WorkspacePromptSourceAdapter> = source.clone();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), vec![adapter]).unwrap(),
        );
        let registry = SessionResidencyRegistry::start(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .expect("the residency actor starts");
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let first = registry.snapshot(session_id).await.unwrap();

        source.replace("blocked replacement prompt");
        source.block_next();
        let mut update = Box::pin(registry.update_workspace_definition(
            session_id,
            first.definition_revision(),
            changed_workspace(&store.new_workspace),
            "2026-08-03T10:02:00.000Z".parse().unwrap(),
        ));
        tokio::select! {
            _ = source.wait_until_entered() => {}
            result = &mut update => {
                panic!("Workspace publication settled before Prompt capture blocked: {result:?}")
            }
        }
        registry.close().await;
        assert!(matches!(
            update.await,
            Err(SessionResidencyWorkspaceDefinitionError::Closing)
        ));
        assert_eq!(
            state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            1
        );
        state.close().await;
        assert_eq!(context.registered_task_count_for_test(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_load_waiter_still_installs_replayed_residency() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        assert!(poll_once_pending(load.as_mut()).await);
        registry.wait_for_active_operation_for_test().await;
        drop(load);

        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 1);
        assert_eq!(
            registry
                .snapshot(session_id)
                .await
                .expect("the dropped Load still installs a coherent executor")
                .execution_state(),
            crate::session_execution::SessionExecutionState::Idle
        );
        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_load_is_idempotent_and_installs_one_owner() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        assert_eq!(registry.loaded_count_for_test(), 1);
        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::NoChange)
        );
        assert_eq!(registry.loaded_count_for_test(), 1);
        assert_eq!(
            registry
                .snapshot(session_id)
                .await
                .unwrap()
                .execution_state(),
            crate::session_execution::SessionExecutionState::Idle
        );

        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_hydrates_replayed_conversation_and_truncates_partial_tail() {
        let store = TempStore::new();
        let recorded = replayed_user_conversation_fixture();
        let conversation_path = store
            .root
            .join("sessions")
            .join(SESSION_ID)
            .join("conversation.jsonl");
        fs::write(&conversation_path, &recorded).expect("the replay fixture is installed");
        let mut append = fs::OpenOptions::new()
            .append(true)
            .open(&conversation_path)
            .expect("the conversation opens for a partial tail");
        append
            .write_all(b"{\"type\":\"entry\",\"data\":{")
            .expect("the partial tail writes");
        drop(append);

        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );

        assert_eq!(fs::read(&conversation_path).unwrap(), recorded);
        let executor = registry
            .executor_for_test(session_id)
            .expect("the hydrated executor is installed");
        let live_state = executor
            .live_state_for_test()
            .expect("production Load installs replayed live state");
        {
            let live_state = lock(&live_state);
            let views = live_state
                .capture_conversation_views()
                .expect("the replayed state has a valid compaction source");
            assert_eq!(views.conversation().messages().len(), 1);
            assert_eq!(views.compaction_source().units().len(), 1);
            assert_eq!(
                views.compaction_source().units()[0].kind(),
                crate::compaction::CompactionUnitKind::UserMessage
            );
            assert_eq!(views.relations().len(), 1);
            assert_ne!(
                views.conversation().revision(),
                crate::live_conversation::ConversationRevision::default()
            );
            let replayed_entry_id = "ent_a0000000000000000000000000000001"
                .parse()
                .expect("the fixture EntryId is valid");
            assert_eq!(views.selected_head(), Some(&replayed_entry_id));
            assert!(live_state.entry_id_is_reserved_for_test(
                "ent_a0000000000000000000000000000001".parse().unwrap()
            ));
        }

        let diagnostics = executor
            .replay_diagnostics_for_test()
            .expect("production Load retains replay diagnostics");
        assert_eq!(
            diagnostics
                .count(crate::conversation_storage::ConversationReplayDiagnosticCode::PartialTail),
            1
        );
        let recorder = executor
            .recorder_for_test()
            .expect("production Load installs a Recorder");
        assert!(matches!(
            &*recorder.health(),
            crate::conversation_storage::RecordingHealth::Healthy
        ));
        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loaded_lifecycle_is_busy_without_a_durable_change_then_archive_after_unload() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let before = state.session_head(session_id).unwrap();

        assert!(matches!(
            registry
                .update_lifecycle(SealedSessionLifecycleAttempt::unarchive(session_id))
                .await,
            Ok(DurableSessionLifecycleOutcome::NoChange(_))
        ));

        assert!(matches!(
            registry
                .update_lifecycle(SealedSessionLifecycleAttempt::archive(session_id))
                .await,
            Err(SessionResidencyLifecycleError::SessionBusy)
        ));
        let after_busy = state.session_head(session_id).unwrap();
        assert_eq!(before.lifecycle(), after_busy.lifecycle());
        assert_eq!(before.storage_generation(), after_busy.storage_generation());

        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        assert!(matches!(
            registry
                .update_lifecycle(SealedSessionLifecycleAttempt::archive(session_id))
                .await,
            Ok(DurableSessionLifecycleOutcome::Updated(_))
        ));
        assert_eq!(
            state.session_head(session_id).unwrap().lifecycle(),
            SessionLifecycle::Archived
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unloaded_workspace_update_holds_residency_exclusion_and_next_load_uses_it() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let current = state
            .session_current(session_id)
            .expect("the fixture Session is current");
        assert_eq!(current.head().current_definition_revision().get(), 1);
        assert_eq!(current.definition().revision().get(), 1);
        let debug = format!("{current:?}");
        assert!(!debug.contains(SESSION_ID));
        assert!(!debug.contains(store.old_workspace.to_string_lossy().as_ref()));

        let outcome = registry
            .update_workspace_definition(
                session_id,
                current.definition().revision(),
                changed_workspace(&store.new_workspace),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await
            .expect("the unloaded Workspace update publishes");
        assert!(outcome.changed());
        assert_eq!(outcome.definition_revision().get(), 2);
        assert_eq!(outcome.workspace_revision().get(), 2);
        assert_eq!(registry.loaded_count_for_test(), 0);

        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        let snapshot = registry.snapshot(session_id).await.unwrap();
        assert_eq!(snapshot.definition_revision().get(), 2);
        assert_eq!(snapshot.workspace_revision().get(), 2);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_rejects_archived_and_deleted_sessions() {
        for delete in [false, true] {
            let store = TempStore::new();
            let (context, state) = open_state(&store.root).await;
            let session_id: SessionId = SESSION_ID.parse().unwrap();
            state
                .update_session_lifecycle(SealedSessionLifecycleAttempt::archive(session_id))
                .await
                .expect("the fixture archives");
            if delete {
                state
                    .update_session_lifecycle(SealedSessionLifecycleAttempt::delete(session_id))
                    .await
                    .expect("the archived fixture deletes");
            }
            let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
            let prompt_service = Arc::new(
                PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
            );
            let registry = SessionResidencyRegistry::start(
                context.clone(),
                state.clone(),
                resolver,
                prompt_service,
            )
            .expect("the residency actor starts");
            assert_eq!(
                registry.load_ready_idle(session_id).await,
                Err(if delete {
                    SessionResidencyLoadError::SessionDeleted
                } else {
                    SessionResidencyLoadError::SessionArchived
                })
            );
            close_fixture(context, state, registry).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_and_unload_share_one_fifo_source_kind_linearization_gate() {
        for fork_first in [true, false] {
            let store = TempStore::new();
            let (context, state, registry) = open_registry(&store).await;
            let session_id: SessionId = SESSION_ID.parse().unwrap();
            registry.load_ready_idle(session_id).await.unwrap();
            let gate = registry.shared.gate(session_id);
            let gate_guard = SessionResidencyOperationPermit::acquire(gate).await;
            let mut fork = Box::pin(registry.fork(
                session_id,
                ForkAnchor::Genesis,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            ));
            let mut unload = Box::pin(registry.unload(session_id));

            if fork_first {
                assert!(poll_once_pending(fork.as_mut()).await);
                assert!(poll_once_pending(unload.as_mut()).await);
            } else {
                assert!(poll_once_pending(unload.as_mut()).await);
                assert!(poll_once_pending(fork.as_mut()).await);
            }
            registry.wait_for_active_operation_count_for_test(2).await;
            drop(gate_guard);

            let child = fork.await.expect("the queued Fork publishes");
            assert_eq!(
                child
                    .fork_provenance()
                    .expect("the child has Fork provenance")
                    .source(),
                if fork_first {
                    ForkSourceKind::LiveSnapshot
                } else {
                    ForkSourceKind::RecordedHistory
                }
            );
            assert_eq!(unload.await, Ok(SessionResidencyUnloadOutcome::Unloaded));
            assert!(!registry.shared.has_loaded(session_id));
            close_fixture(context, state, registry).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_and_load_share_one_fifo_source_kind_linearization_gate() {
        for load_first in [true, false] {
            let store = TempStore::new();
            let (context, state, registry) = open_registry(&store).await;
            let session_id: SessionId = SESSION_ID.parse().unwrap();
            let gate = registry.shared.gate(session_id);
            let gate_guard = SessionResidencyOperationPermit::acquire(gate).await;
            let mut load = Box::pin(registry.load_ready_idle(session_id));
            let mut fork = Box::pin(registry.fork(
                session_id,
                ForkAnchor::Genesis,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            ));

            if load_first {
                assert!(poll_once_pending(load.as_mut()).await);
                assert!(poll_once_pending(fork.as_mut()).await);
            } else {
                assert!(poll_once_pending(fork.as_mut()).await);
                assert!(poll_once_pending(load.as_mut()).await);
            }
            registry.wait_for_active_operation_count_for_test(2).await;
            drop(gate_guard);

            let child = fork.await.expect("the queued Fork publishes");
            assert_eq!(
                child
                    .fork_provenance()
                    .expect("the child has Fork provenance")
                    .source(),
                if load_first {
                    ForkSourceKind::LiveSnapshot
                } else {
                    ForkSourceKind::RecordedHistory
                }
            );
            assert_eq!(load.await, Ok(SessionResidencyLoadOutcome::Loaded));
            assert!(registry.shared.has_loaded(session_id));
            close_fixture(context, state, registry).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_workspace_publication_keeps_unload_pending() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let executor = registry
            .shared
            .executor(session_id)
            .expect("the loaded executor is installed");
        let hooks = executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let snapshot = registry.snapshot(session_id).await.unwrap();
        let mut update = Box::pin(registry.update_workspace_definition(
            session_id,
            snapshot.definition_revision(),
            changed_workspace(&store.new_workspace),
            "2026-08-03T10:02:00.000Z".parse().unwrap(),
        ));
        tokio::select! {
            _ = hooks.wait_after_candidate_snapshot_finish_before_durable() => {}
            result = &mut update => panic!("publication settled before the deterministic barrier: {result:?}"),
        }

        let mut unload = Box::pin(registry.unload(session_id));
        assert!(poll_once_pending(unload.as_mut()).await);
        registry.wait_for_active_operation_count_for_test(2).await;

        hooks.release_after_candidate_snapshot_finish_before_durable();
        update.await.expect("the blocked publication settles");
        assert_eq!(unload.await, Ok(SessionResidencyUnloadOutcome::Unloaded));
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_workspace_requires_loaded_residency() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let timestamp = "2026-08-03T10:02:00.000Z".parse().unwrap();

        // A never-loaded (or missing) Session maps directly to SessionNotLoaded without reading
        // or updating DurableState.
        assert_eq!(
            registry
                .reload_workspace(session_id, timestamp, CommandId::generate().unwrap())
                .await,
            Err(SessionResidencyWorkspaceReloadError::SessionNotLoaded)
        );
        let missing: SessionId = "ses_ffffffffffffffffffffffffffffffff".parse().unwrap();
        assert_eq!(
            registry
                .reload_workspace(missing, timestamp, CommandId::generate().unwrap())
                .await,
            Err(SessionResidencyWorkspaceReloadError::SessionNotLoaded)
        );

        // The durable definition is untouched by the rejected reloads.
        assert_eq!(
            state
                .session_current(session_id)
                .unwrap()
                .definition()
                .revision()
                .get(),
            1
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loaded_reload_holds_gate_and_linearizes_with_unload() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let gate = registry.shared.gate(session_id);
        let gate_guard = SessionResidencyOperationPermit::acquire(gate).await;
        let mut reload = Box::pin(registry.reload_workspace(
            session_id,
            "2026-08-03T10:02:00.000Z".parse().unwrap(),
            CommandId::generate().unwrap(),
        ));
        let mut unload = Box::pin(registry.unload(session_id));

        // Both operations wait on the same per-Session FIFO gate: the reload completion and the
        // Unload linearize exactly once, in admission order.
        assert!(poll_once_pending(reload.as_mut()).await);
        assert!(poll_once_pending(unload.as_mut()).await);
        registry.wait_for_active_operation_count_for_test(2).await;
        drop(gate_guard);

        let reloaded = reload.await.expect("the queued reload publishes");
        assert!(reloaded.changed());
        assert_eq!(reloaded.definition_revision().get(), 1);
        assert_eq!(reloaded.workspace_revision().get(), 1);
        assert_eq!(unload.await, Ok(SessionResidencyUnloadOutcome::Unloaded));
        assert!(!registry.shared.has_loaded(session_id));
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_unload_waiter_still_removes_residency() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();

        let executor = registry
            .executor_for_test(session_id)
            .expect("the loaded executor is installed");
        let hooks = executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let snapshot = registry.snapshot(session_id).await.unwrap();
        let mut update = Box::pin(registry.update_workspace_definition(
            session_id,
            snapshot.definition_revision(),
            changed_workspace(&store.new_workspace),
            "2026-08-03T10:02:00.000Z".parse().unwrap(),
        ));
        tokio::select! {
            _ = hooks.wait_after_candidate_snapshot_finish_before_durable() => {}
            result = &mut update => panic!("publication settled before the named barrier: {result:?}"),
        }

        let mut unload = Box::pin(registry.unload(session_id));
        assert!(poll_once_pending(unload.as_mut()).await);
        registry.wait_for_active_operation_count_for_test(2).await;
        drop(unload);
        hooks.release_after_candidate_snapshot_finish_before_durable();
        update.await.expect("the blocked publication settles");
        registry.wait_for_no_active_operation_for_test().await;

        assert!(matches!(
            registry.snapshot(session_id).await,
            Err(SessionResidencySnapshotError::SessionNotLoaded)
        ));
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_an_active_publication_and_preclose_reserved_request() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let executor = registry
            .shared
            .executor(session_id)
            .expect("the loaded executor is installed");
        let hooks = executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let snapshot = registry.snapshot(session_id).await.unwrap();
        let mut update = Box::pin(registry.update_workspace_definition(
            session_id,
            snapshot.definition_revision(),
            changed_workspace(&store.new_workspace),
            "2026-08-03T10:02:00.000Z".parse().unwrap(),
        ));
        tokio::select! {
            _ = hooks.wait_after_candidate_snapshot_finish_before_durable() => {}
            result = &mut update => panic!("publication settled before the deterministic barrier: {result:?}"),
        }

        let permit = registry
            .sender
            .clone()
            .reserve_owned()
            .await
            .expect("the bounded residency lane reserves capacity");
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::Snapshot(SnapshotRequest {
            session_id,
            response: Some(response),
        });
        let mut close = Box::pin(registry.close());
        assert!(poll_once_pending(close.as_mut()).await);
        permit.send(request);
        assert!(matches!(
            waiter.await.unwrap(),
            Err(SessionResidencySnapshotError::Closing)
        ));
        hooks.release_after_candidate_snapshot_finish_before_durable();
        close.await;
        update.await.expect("the blocked publication settles");
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        state.close().await;
        assert_eq!(context.registered_task_count_for_test(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn registry_close_waits_for_a_blocked_recorder_append_and_reaps_owner_work() {
        let store = TempStore::new();
        let recorded = replayed_user_conversation_fixture();
        let conversation_path = store
            .root
            .join("sessions")
            .join(SESSION_ID)
            .join("conversation.jsonl");
        fs::write(&conversation_path, &recorded).expect("the replay fixture is installed");

        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let baseline = context.registered_task_count_for_test();

        let recorder = registry
            .executor_for_test(session_id)
            .expect("the loaded executor is installed")
            .recorder_for_test()
            .expect("the loaded executor retains its Recorder");
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));

        // The appended Entry is the fixture's User entry, obtained through the existing
        // production replay codec seam instead of a new production accessor.
        let entry_line = replayed_user_append_entry_fixture();
        let entry = ConversationLineCodec::decode_entry_for_session(&entry_line, session_id)
            .expect("the production codec replays the fixture User entry");

        let mut append = Box::pin(recorder.record(Arc::new(entry)));
        assert!(poll_once_pending(append.as_mut()).await);
        barrier.wait_until_entered().await;
        assert_eq!(
            context.registered_task_count_for_test(),
            baseline + 1,
            "the blocked Recorder append is the only extra registered owner work"
        );

        let mut close = Box::pin(registry.close());
        assert!(
            poll_once_pending(close.as_mut()).await,
            "registry close must drain the blocked Recorder append before it settles"
        );
        assert_eq!(context.registered_task_count_for_test(), baseline + 1);

        barrier.release();
        assert_eq!(append.await, RecordOutcome::Written);
        close.await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        state.close().await;
        assert_eq!(context.registered_task_count_for_test(), 0);

        // The Registry has drained its loaded owner before the DurableState root is closed.
        let (reopen_context, reopened) = open_state(&store.root).await;
        reopened.close().await;
        assert_eq!(reopen_context.registered_task_count_for_test(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_loads_do_not_retain_per_session_gates() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;

        for value in 10..26 {
            assert_eq!(
                registry.load_ready_idle(session_candidate(value)).await,
                Err(SessionResidencyLoadError::SessionNotFound)
            );
            registry.wait_for_no_active_operation_for_test().await;
            assert_eq!(registry.gate_count_for_test(), 0);
        }

        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_maps_strict_conversation_header_failure_without_installing_residency() {
        let store = TempStore::new();
        let conversation_path = store
            .root
            .join("sessions")
            .join(SESSION_ID)
            .join("conversation.jsonl");
        fs::write(&conversation_path, b"not a conversation header\n")
            .expect("the corrupt Header fixture writes");
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Err(SessionResidencyLoadError::RecordedStateCorrupt)
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_workers_are_reaped_after_success_and_error() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();
        let baseline = context.registered_task_count_for_test();

        for path in [
            store.root.join("missing-workspace"),
            store.new_workspace.clone(),
            store.old_workspace.clone(),
            store.new_workspace.clone(),
        ] {
            let snapshot = registry.snapshot(session_id).await.unwrap();
            let result = registry
                .update_workspace_definition(
                    session_id,
                    snapshot.definition_revision(),
                    changed_workspace(&path),
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                )
                .await;
            if path.ends_with("missing-workspace") {
                assert!(matches!(
                    result,
                    Err(SessionResidencyWorkspaceDefinitionError::WorkspaceUnavailable)
                ));
            } else {
                assert!(result.is_ok());
            }
            assert_eq!(context.registered_task_count_for_test(), baseline);
        }

        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_load_waiter_at_replay_barrier_still_installs_residency() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let baseline = context.registered_task_count_for_test();
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let barrier = ReplayPreparationBarrier::new();
        barrier.arm_before_recorder();
        registry.set_replay_preparation_barrier_for_test(Some(Arc::clone(&barrier)));

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        assert!(poll_once_pending(load.as_mut()).await);
        barrier.wait_until_before_recorder().await;
        drop(load);

        // The dropped caller cannot cancel the admitted child; the replay worker completes and
        // still installs the residency.
        barrier.release_before_recorder();
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 1);
        assert_eq!(registry.gate_count_for_test(), 1);
        assert_eq!(
            context.registered_task_count_for_test(),
            baseline + 1,
            "the admitted Load child and its replay worker are reaped; the loaded executor actor remains registered"
        );
        assert_eq!(
            registry
                .snapshot(session_id)
                .await
                .expect("the dropped Load still installs a coherent executor")
                .execution_state(),
            crate::session_execution::SessionExecutionState::Idle
        );
        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_replay_spawn_rejection_maps_to_closing_without_residency() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let barrier = ReplayPreparationBarrier::new();
        barrier.arm_before_spawn();
        registry.set_replay_preparation_barrier_for_test(Some(Arc::clone(&barrier)));

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        assert!(poll_once_pending(load.as_mut()).await);
        barrier.wait_until_before_spawn().await;
        context.request_closing();
        barrier.release_before_spawn();

        assert_eq!(
            load.await,
            Err(SessionResidencyLoadError::Closing),
            "a rejected replay worker spawn maps to the existing Closing contract"
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_replay_worker_panic_maps_to_internal_without_residency() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let barrier = ReplayPreparationBarrier::new();
        barrier.panic_before_recorder();
        registry.set_replay_preparation_barrier_for_test(Some(Arc::clone(&barrier)));

        let result = registry.load_ready_idle(session_id).await;
        assert_eq!(
            result,
            Err(SessionResidencyLoadError::InternalDispatchUnavailable),
            "a panicked replay worker maps to the existing redacted internal contract"
        );
        assert!(
            !format!("{result:?}").contains("ReplayPreparationBarrier"),
            "the replay worker panic payload never crosses the owner boundary"
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_replay_worker_join_failure_maps_to_internal_without_residency() {
        let store = TempStore::new();
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let barrier = ReplayPreparationBarrier::new();
        barrier.arm_before_spawn();
        registry.set_replay_preparation_barrier_for_test(Some(Arc::clone(&barrier)));

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        assert!(poll_once_pending(load.as_mut()).await);
        barrier.wait_until_before_spawn().await;

        // Arm the one-shot join-failure seam while the Load is paused before it spawns the
        // replay blocking worker.  That exact worker consumes the fault: its raw join handle
        // aborts immediately, so `TrackedBlockingJob::wait()` settles WorkerUnavailable without
        // running the replay operation closure.  The parent Load task is never touched.
        context.inject_next_blocking_job_join_failure();
        barrier.release_before_spawn();

        assert_eq!(
            load.await,
            Err(SessionResidencyLoadError::InternalDispatchUnavailable),
            "a replay worker join failure maps to the existing redacted internal contract"
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_installs_ready_idle_with_degraded_recorder_on_length_invariant_failure() {
        let store = TempStore::new();
        let recorded = replayed_user_conversation_fixture();
        let conversation_path = store
            .root
            .join("sessions")
            .join(SESSION_ID)
            .join("conversation.jsonl");
        fs::write(&conversation_path, &recorded).expect("the replay fixture is installed");
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        let barrier = ReplayPreparationBarrier::new();
        barrier.corrupt_length_before_recorder();
        registry.set_replay_preparation_barrier_for_test(Some(Arc::clone(&barrier)));

        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded),
            "Load still installs Ready+Idle when the same target fails its Recorder invariant"
        );
        assert_eq!(registry.loaded_count_for_test(), 1);
        assert_eq!(registry.gate_count_for_test(), 1);
        let snapshot = registry
            .snapshot(session_id)
            .await
            .expect("the degraded-recorder Load installs an Idle executor");
        assert_eq!(
            snapshot.execution_state(),
            crate::session_execution::SessionExecutionState::Idle
        );
        assert_eq!(
            snapshot.recording(),
            crate::runtime_interface::SessionRecordingState::Degraded
        );
        assert_eq!(
            snapshot.diagnostics()[0].code(),
            "session_recording_initialization_failed"
        );

        let executor = registry
            .executor_for_test(session_id)
            .expect("the degraded-recorder Load installs its executor");
        let recorder = executor
            .recorder_for_test()
            .expect("the installed executor retains its Recorder");
        assert!(matches!(
            &*recorder.health(),
            crate::conversation_storage::RecordingHealth::Degraded {
                failed_entry_id: None,
                reason: crate::conversation_storage::SessionRecordingError::TargetInvariant,
            }
        ));

        // No second target/proof was manufactured: the one published conversation file now
        // carries exactly the replayed bytes plus the injected byte.
        let mut expected = recorded;
        expected.extend_from_slice(b"x");
        assert_eq!(fs::read(&conversation_path).unwrap(), expected);

        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_fails_closed_when_definition_publishes_between_candidate_and_final_recheck() {
        let store = TempStore::new();
        let (context, state) = open_state(&store.root).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let current = state
            .session_current(session_id)
            .expect("the fixture Session is current");
        assert_eq!(current.definition().revision().get(), 1);

        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let hooks = resolver.test_hooks();
        hooks.arm_after_candidate_before_final_recheck();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let registry = SessionResidencyRegistry::start(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .expect("the residency actor starts");
        let baseline = context.registered_task_count_for_test();

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        tokio::select! {
            _ = hooks.wait_after_candidate_before_final_recheck() => {}
            result = &mut load => panic!("load settled before the final durable recheck: {result:?}"),
        }

        // The durable definition update publishes while the Load holds its stale candidate and
        // has not yet performed the final durable recheck.  The gate does not protect this
        // direct durable publication, exactly like a concurrent Workspace publication owner.
        let outcome = state
            .update_session_definition(SealedSessionDefinitionAttempt::new(
                session_id,
                current.definition().revision(),
                Some(changed_workspace(&store.new_workspace)),
                None,
                None,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            ))
            .await
            .expect("the durable definition update publishes");
        assert!(outcome.changed());
        assert_eq!(outcome.definition().revision().get(), 2);

        hooks.release_after_candidate_before_final_recheck();
        assert_eq!(
            load.await,
            Err(SessionResidencyLoadError::StaleDefinition),
            "the stale candidate must fail closed without installing residency"
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(registry.gate_count_for_test(), 0);
        assert_eq!(
            context.registered_task_count_for_test(),
            baseline,
            "the ordinary stale Load reaps its child while the owner actors remain available"
        );

        // A current Load resolves the updated definition and installs residency.
        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        let snapshot = registry.snapshot(session_id).await.unwrap();
        assert_eq!(snapshot.definition_revision().get(), 2);
        assert_eq!(snapshot.workspace_revision().get(), 2);
        close_fixture(context, state, registry).await;
    }

    // The exact replacement race (rename of the old root while the candidate holds an
    // opened capability) is Unix-only: Windows intentionally returns a sharing violation
    // because the opened directory lacks FILE_SHARE_DELETE, so the rename never succeeds.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn read_access_load_revalidates_readable_candidate_and_rejects_replaced_root() {
        let store = TempStore::new();
        empty_fixture_prompt_selections(&store);
        let (context, state) = open_state(&store.root).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();

        // The production read_file authority grants a readable filesystem ceiling with no
        // Prompt/Skill source ceilings: the resolved candidate is readable with no sources,
        // so only candidate-driven revalidation closes the async authority/capture window.
        let (resolver, _control) = WorkspaceResolver::new_with_read_access(context.clone());
        let hooks = resolver.test_hooks();
        hooks.arm_after_candidate_before_final_recheck();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let registry = open_registry_with_turn_resources(
            context.clone(),
            state.clone(),
            Arc::new(resolver),
            prompt_service,
        )
        .await;

        let mut load = Box::pin(registry.load_ready_idle(session_id));
        tokio::select! {
            _ = hooks.wait_after_candidate_before_final_recheck() => {}
            result = &mut load => {
                panic!("load settled before the after-candidate revalidation: {result:?}")
            }
        }

        // Replace the resolved root at the same path while the Load holds its stale
        // candidate: the canonical path text is unchanged, but the safe identity differs.
        let displaced = store.root.join("displaced-workspace");
        fs::rename(&store.old_workspace, &displaced).expect("the old root is displaceable");
        fs::create_dir(&store.old_workspace).expect("the replacement root is created");
        fs::create_dir(store.old_workspace.join("src")).expect("the replacement cwd is created");
        set_private_directory_mode(&store.old_workspace);
        set_private_directory_mode(&store.old_workspace.join("src"));

        hooks.release_after_candidate_before_final_recheck();

        // The candidate-driven revalidation runs because the read-only root is readable,
        // observes the replaced identity, and installs no stale Ready snapshot.
        assert_eq!(load.await, Ok(SessionResidencyLoadOutcome::Loaded));
        let unavailable = registry.snapshot(session_id).await.unwrap();
        assert_eq!(
            unavailable.readiness(),
            SessionReadinessView::Unavailable(SessionUnavailableView::WorkspaceUnavailable)
        );
        assert!(
            unavailable.workspace_optional().is_none(),
            "a replaced root must not install the stale Ready snapshot"
        );
        assert_eq!(registry.loaded_count_for_test(), 1);

        let _ = fs::remove_dir_all(&displaced);
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn equivalent_workspace_with_model_change_skips_resolver_but_true_workspace_change_resolves()
     {
        let store = TempStore::new();
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let hooks = resolver.test_hooks();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let registry = SessionResidencyRegistry::start(
            context.clone(),
            state.clone(),
            resolver,
            prompt_service,
        )
        .expect("the residency actor starts");
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        let current = state
            .session_current(session_id)
            .expect("the fixture Session is current");
        let expected_model = SessionModelConfig::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ReasoningPreference::High,
            None,
        );

        // A canonical-equivalent Workspace combined with a Model change is future-only: it must
        // publish without invoking the Workspace resolver at all.
        hooks.arm_after_candidate_before_final_recheck();
        let future_only = registry
            .update_session_definition(
                session_id,
                current.definition().revision(),
                Some(current.definition().workspace().clone()),
                Some(expected_model.clone()),
                None,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
            )
            .await
            .expect("the equivalent-Workspace Model update publishes");
        assert!(future_only.changed());
        assert_eq!(future_only.definition_revision().get(), 2);
        assert_eq!(future_only.workspace_revision().get(), 1);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                hooks.wait_after_candidate_before_final_recheck(),
            )
            .await
            .is_err(),
            "an equivalent Workspace must not invoke the resolver"
        );
        let snapshot = registry.snapshot(session_id).await.unwrap();
        assert_eq!(snapshot.workspace_revision().get(), 1);
        assert_eq!(snapshot.definition_revision().get(), 2);

        // A true Workspace semantic change on the same loaded Session does resolve.
        let workspace = {
            let workspace_update = registry.update_session_definition(
                session_id,
                snapshot.definition_revision(),
                Some(changed_workspace(&store.new_workspace)),
                None,
                None,
                "2026-08-03T10:03:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
            );
            tokio::pin!(workspace_update);
            tokio::select! {
                _ = hooks.wait_after_candidate_before_final_recheck() => {}
                result = workspace_update.as_mut() => {
                    panic!("Workspace publication settled before resolver recheck: {result:?}")
                }
            }
            hooks.release_after_candidate_before_final_recheck();
            workspace_update
                .as_mut()
                .await
                .expect("the true Workspace change publishes")
        };
        assert!(workspace.changed());
        assert_eq!(workspace.definition_revision().get(), 3);
        assert_eq!(workspace.workspace_revision().get(), 2);
        let snapshot = registry.snapshot(session_id).await.unwrap();
        assert_eq!(snapshot.workspace_revision().get(), 2);
        assert_eq!(
            snapshot
                .workspace()
                .prompt_context()
                .primary_root()
                .as_path(),
            fs::canonicalize(&store.new_workspace).unwrap()
        );
        close_fixture(context, state, registry).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cold_reload_after_recorder_append_accepts_the_full_second_user_entry() {
        let store = TempStore::new();
        let recorded = replayed_user_conversation_fixture();
        let conversation_path = store
            .root
            .join("sessions")
            .join(SESSION_ID)
            .join("conversation.jsonl");
        fs::write(&conversation_path, &recorded).expect("the replay fixture is installed");
        let (context, state, registry) = open_registry(&store).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        registry.load_ready_idle(session_id).await.unwrap();

        // Physical append of the fixture's second User entry through the installed Recorder.
        let recorder = registry
            .executor_for_test(session_id)
            .expect("the loaded executor is installed")
            .recorder_for_test()
            .expect("the loaded executor retains its Recorder");
        let entry_line = replayed_user_append_entry_fixture();
        let entry = ConversationLineCodec::decode_entry_for_session(&entry_line, session_id)
            .expect("the production codec replays the fixture User entry");
        assert_eq!(
            recorder.record(Arc::new(entry)).await,
            RecordOutcome::Written
        );

        // Cold reload: unload closes the Recorder and a fresh Load replays the durable bytes.
        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        registry.wait_for_no_active_operation_for_test().await;
        assert_eq!(registry.loaded_count_for_test(), 0);
        assert_eq!(
            registry.load_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );

        let executor = registry
            .executor_for_test(session_id)
            .expect("the cold reload installs its executor");
        let live_state = executor
            .live_state_for_test()
            .expect("the cold reload installs replayed live state");
        let reloaded_recorder = executor
            .recorder_for_test()
            .expect("the cold reload installs its Recorder");
        assert!(matches!(
            &*reloaded_recorder.health(),
            crate::conversation_storage::RecordingHealth::Healthy
        ));
        {
            let live_state = lock(&live_state);
            let views = live_state
                .capture_conversation_views()
                .expect("the reloaded state has a valid compaction source");
            assert_eq!(
                views.conversation().messages().len(),
                2,
                "the complete second User entry is replayed, not only codec-decoded"
            );
            assert_eq!(views.compaction_source().units().len(), 2);
            assert_eq!(views.relations().len(), 2);
            let second_entry_id: crate::wire::EntryId = "ent_a0000000000000000000000000000002"
                .parse()
                .expect("the fixture EntryId is valid");
            assert_eq!(views.selected_head(), Some(&second_entry_id));
            assert!(live_state.entry_id_is_reserved_for_test(second_entry_id));
        }

        // The durable file is the replayed bytes plus exactly one complete appended entry line.
        let mut expected = recorded;
        expected.extend_from_slice(&entry_line);
        expected.push(b'\n');
        assert_eq!(fs::read(&conversation_path).unwrap(), expected);
        assert_eq!(
            registry.unload(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        close_fixture(context, state, registry).await;
    }

    #[test]
    fn residency_errors_and_permits_are_redacted_and_identity_only() {
        let sensitive_id = SESSION_ID;
        let sensitive_path = "/private/session/path";
        for error in [
            format!(
                "{:?}",
                SessionResidencyLoadError::InternalDispatchUnavailable
            ),
            format!(
                "{:?}",
                SessionResidencyUnloadError::InternalDispatchUnavailable
            ),
            format!("{:?}", SessionResidencyLifecycleError::SessionBusy),
            format!("{:?}", SessionResidencySnapshotError::SessionNotLoaded),
            format!(
                "{:?}",
                SessionResidencyWorkspaceDefinitionError::WorkspaceRejected
            ),
        ] {
            assert!(!error.contains(sensitive_id));
            assert!(!error.contains(sensitive_path));
        }
        let first = SessionResidencyPermit::new();
        let clone = first.clone();
        let other = SessionResidencyPermit::new();
        assert!(first.same_as(&clone));
        assert!(!first.same_as(&other));
        assert!(!format!("{first:?}").contains(sensitive_id));
    }
}
