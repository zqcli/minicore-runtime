#![allow(
    dead_code,
    reason = "the Runtime-owned residency foundation awaits public command/query routing"
)]

//! The crate-private owner of loaded Session residency.
//!
//! This module deliberately stops at the boundary between durable Session definitions, the
//! Workspace resolver, replay-backed Ready+Idle installation, and a loaded [`SessionExecutor`].
//! Conversation Storage retains semantic replay and recording ownership; this registry only keeps
//! their prepared state/recorder alive through publication. It owns no public Runtime routing or
//! Turn state. `RuntimeInner` retains this registry as one deep resource owner without making any
//! residency permit, gate, executor, or task handle part of the Runtime interface.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent_session_lifecycle::{
    SealedSessionDefinitionAttempt, SealedSessionLifecycleAttempt, SessionLifecycle,
    SessionLifecycleDecision, SessionLifecycleDecisionError,
};
use crate::conversation_storage::{
    ConversationLoadError, ConversationReplayError, load_replayed_conversation,
};
use crate::durable_state::{
    DurableConversationTargetError, DurableSessionDefinitionError, DurableSessionDefinitionOutcome,
    DurableSessionLifecycleError, DurableSessionLifecycleOutcome, DurableState,
};
use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedTask};
use crate::session_execution::{
    LoadedSessionConversation, SessionExecutor, SessionExecutorCloseError, SessionExecutorSnapshot,
    SessionExecutorSnapshotError, SessionExecutorStartError, SessionWorkspaceDefinitionError,
    SessionWorkspaceDefinitionOutcome,
};
use crate::wire::{SessionDefinitionRevision, SessionId, Timestamp};
use crate::workspace::{
    Workspace, WorkspaceResolveError, WorkspaceResolver, WorkspaceSnapshotFinishError,
};

const SESSION_RESIDENCY_REQUEST_QUEUE_CAPACITY: usize = 8;

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
}

impl ResidencyShared {
    fn new() -> Self {
        Self {
            state: Mutex::new(ResidencyState::new()),
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
    Lifecycle,
    Snapshot,
    WorkspaceDefinition,
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
        }
    }
}

enum OperationCompletion {
    Load(Result<SessionResidencyLoadOutcome, SessionResidencyLoadError>),
    Unload(Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError>),
    Lifecycle(Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError>),
    Snapshot(Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError>),
    WorkspaceDefinition(
        Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError>,
    ),
}

impl OperationCompletion {
    fn is_internal(&self) -> bool {
        matches!(
            self,
            Self::Load(Err(SessionResidencyLoadError::InternalDispatchUnavailable))
                | Self::Unload(Err(
                    SessionResidencyUnloadError::InternalDispatchUnavailable
                ))
                | Self::Lifecycle(Err(
                    SessionResidencyLifecycleError::InternalDispatchUnavailable
                ))
                | Self::Snapshot(Err(
                    SessionResidencySnapshotError::InternalDispatchUnavailable
                ))
                | Self::WorkspaceDefinition(Err(
                    SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
                ))
        )
    }

    fn is_closing(&self) -> bool {
        matches!(
            self,
            Self::Load(Err(SessionResidencyLoadError::Closing))
                | Self::Unload(Err(SessionResidencyUnloadError::Closing))
                | Self::Lifecycle(Err(SessionResidencyLifecycleError::Closing))
                | Self::Snapshot(Err(SessionResidencySnapshotError::Closing))
                | Self::WorkspaceDefinition(Err(SessionResidencyWorkspaceDefinitionError::Closing))
        )
    }
}

enum OperationSender {
    Load(oneshot::Sender<Result<SessionResidencyLoadOutcome, SessionResidencyLoadError>>),
    Unload(oneshot::Sender<Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError>>),
    Lifecycle(
        oneshot::Sender<Result<DurableSessionLifecycleOutcome, SessionResidencyLifecycleError>>,
    ),
    Snapshot(oneshot::Sender<Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError>>),
    WorkspaceDefinition(
        oneshot::Sender<
            Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError>,
        >,
    ),
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
    session_id: SessionId,
    waiter: Arc<OperationWaiter>,
    task: Option<TrackedTask>,
}

/// The operation context moved into every admitted child.  No child uses a raw task handle; the
/// actor retains and reaps the `TrackedTask` created for it.
#[derive(Clone)]
struct OperationContext {
    state: Arc<ResidencyShared>,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    closing: CancellationToken,
    failure: Arc<RegistryFailureState>,
}

impl OperationContext {
    fn poison(&self) {
        self.failure.mark_fatal();
        self.state.cancel_admission(&self.closing);
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

    fn internal_snapshot(&self) -> SessionResidencySnapshotError {
        self.poison();
        SessionResidencySnapshotError::InternalDispatchUnavailable
    }

    fn internal_workspace(&self) -> SessionResidencyWorkspaceDefinitionError {
        self.poison();
        SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
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
            self.task_context.request_closing();
            self.durable_state.request_closing();
            self.kind.internal_completion()
        };
        if let Some(sender) = self.sender.take() {
            if sender.send((self.operation_id, completion)).is_err() {
                self.failure.mark_fatal();
                self.state.cancel_admission(&self.closing);
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
    owner_timestamp: Timestamp,
    response: Option<
        oneshot::Sender<
            Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError>,
        >,
    >,
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
    Lifecycle(LifecycleRequest),
    Snapshot(SnapshotRequest),
    WorkspaceDefinition(WorkspaceDefinitionRequest),
}

impl ResidencyRequest {
    fn reject_closing(&mut self) {
        match self {
            Self::Load(request) => request.reject_closing(),
            Self::Unload(request) => request.reject_closing(),
            Self::Lifecycle(request) => request.reject_closing(),
            Self::Snapshot(request) => request.reject_closing(),
            Self::WorkspaceDefinition(request) => request.reject_closing(),
        }
    }

    fn reject_internal(&mut self) {
        match self {
            Self::Load(request) => request.reject_internal(),
            Self::Unload(request) => request.reject_internal(),
            Self::Lifecycle(request) => request.reject_internal(),
            Self::Snapshot(request) => request.reject_internal(),
            Self::WorkspaceDefinition(request) => request.reject_internal(),
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
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
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
            closing: self.closing.clone(),
            failure: Arc::clone(&self.failure),
        }
    }

    fn poison(&self) {
        self.failure.mark_fatal();
        self.state.cancel_admission(&self.closing);
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
            ResidencyRequest::Lifecycle(request) => self.start_lifecycle(request),
            ResidencyRequest::Snapshot(request) => self.start_snapshot(request),
            ResidencyRequest::WorkspaceDefinition(request) => {
                self.start_workspace_definition(request)
            }
        }
    }

    fn next_operation_id(&mut self) -> Option<OperationId> {
        let id = OperationId(self.next_operation_id);
        self.next_operation_id = self.next_operation_id.checked_add(1)?;
        Some(id)
    }

    fn start_child<F>(
        &mut self,
        session_id: SessionId,
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
            session_id,
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
            session_id,
            OperationKind::Unload,
            OperationSender::Unload(sender),
            async move { OperationCompletion::Unload(run_unload(context, session_id).await) },
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
            session_id,
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
            session_id,
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
        let Some(workspace) = request.workspace.take() else {
            self.poison();
            return;
        };
        let owner_timestamp = request.owner_timestamp;
        let context = self.operation_context();
        self.start_child(
            session_id,
            OperationKind::WorkspaceDefinition,
            OperationSender::WorkspaceDefinition(sender),
            async move {
                OperationCompletion::WorkspaceDefinition(
                    run_workspace_definition(
                        context,
                        session_id,
                        expected_revision,
                        workspace,
                        owner_timestamp,
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
        if !self
            .active
            .values()
            .any(|active| active.session_id == removed.session_id)
        {
            self.state.remove_gate_if_unused(removed.session_id);
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
        let mut normal = true;
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
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        failure: Arc<RegistryFailureState>,
        active_waiters: Arc<ActiveWaiters>,
        shared: Arc<ResidencyShared>,
    ) -> Self {
        Self {
            closing,
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
    /// Starts the residency actor.  The durable state and resolver are retained by the actor and
    /// by any admitted owner-tracked child work; no raw Tokio task handle escapes this method.
    pub(crate) fn start(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
    ) -> Result<Self, SessionResidencyStartError> {
        let closing = CancellationToken::new();
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
            task_context: task_context.clone(),
            durable_state: durable_state.clone(),
            resolver,
            state: Arc::clone(&shared),
            failure: Arc::clone(&failure),
            active_waiters: Arc::clone(&active_waiters),
            active: BTreeMap::new(),
            next_operation_id: 1,
        };

        let actor_closing = closing.clone();
        let actor_task_context = task_context.clone();
        let actor_durable_state = durable_state.clone();
        let actor_failure = Arc::clone(&failure);
        let actor_waiters = Arc::clone(&active_waiters);
        let actor_shared = Arc::clone(&shared);
        let mut exit_guard = ActorExitGuard::new(
            actor_closing,
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
            task,
            task_context,
            durable_state,
            failure,
            shared,
            active_waiters,
        })
    }

    /// Stops new residency admission.  Accepted child operations remain owner-tracked and are
    /// allowed to settle; `close` performs the asynchronous drain.
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

    /// Loads one current durable Session as a Ready+Idle executor.  A duplicate request is an
    /// idempotent NoChange and never starts a second executor.
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

    /// Routes a loaded Workspace definition replacement to the installed executor.
    pub(crate) async fn update_workspace_definition(
        &self,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
        let (response, waiter) = oneshot::channel();
        let request = ResidencyRequest::WorkspaceDefinition(WorkspaceDefinitionRequest {
            session_id,
            expected_revision,
            workspace: Some(workspace),
            owner_timestamp,
            response: Some(response),
        });
        self.admit(request).await;
        waiter
            .await
            .unwrap_or_else(|_| self.workspace_waiter_fallback())
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
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
        Err(if self.failure.is_fatal() {
            SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
        } else if self.closing.is_cancelled() {
            SessionResidencyWorkspaceDefinitionError::Closing
        } else {
            SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable
        })
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
    match head.lifecycle() {
        SessionLifecycle::Open => {}
        SessionLifecycle::Archived => return Err(SessionResidencyLoadError::SessionArchived),
        SessionLifecycle::Deleted => return Err(SessionResidencyLoadError::SessionDeleted),
    }

    let candidate = match context
        .resolver
        .resolve(session_id, definition.workspace())
        .await
    {
        Ok(candidate) => candidate,
        Err(WorkspaceResolveError::Closing) => {
            return Err(SessionResidencyLoadError::Closing);
        }
        Err(
            WorkspaceResolveError::RootUnavailable
            | WorkspaceResolveError::AuthorityUnavailable
            | WorkspaceResolveError::CanonicalizationFailed,
        ) => {
            return Err(SessionResidencyLoadError::WorkspaceUnavailable);
        }
        Err(
            WorkspaceResolveError::RootNotDirectory
            | WorkspaceResolveError::DuplicateRoot
            | WorkspaceResolveError::OverlappingRoots
            | WorkspaceResolveError::CwdOutsideRoots
            | WorkspaceResolveError::CwdRootMismatch
            | WorkspaceResolveError::AuthorityDenied,
        ) => {
            return Err(SessionResidencyLoadError::WorkspaceRejected);
        }
        Err(WorkspaceResolveError::InternalDispatchUnavailable) => {
            return Err(context.internal_load());
        }
    };

    if candidate.revision() != definition.workspace().revision() {
        return Err(context.internal_load());
    }
    let prompt_context = candidate.prompt_capture_context();
    let skill_context = candidate.skill_capture_context();
    if !prompt_context.roots().is_empty() || !skill_context.roots().is_empty() {
        // Source discovery is intentionally fail-closed until the Prompt/Skill source adapters
        // exist.  Never silently discard an authorized source root in production.
        return Err(context.internal_load());
    }
    let prompt_sources = Arc::from(Vec::new().into_boxed_slice());
    let skill_sources = Arc::from(Vec::new().into_boxed_slice());
    let workspace_snapshot = match candidate.finish(prompt_sources, skill_sources) {
        Ok(snapshot) => snapshot,
        Err(WorkspaceSnapshotFinishError::AuthorizationMismatch) => {
            return Err(context.internal_load());
        }
    };

    let final_current = match context.durable_state.session_current(session_id) {
        Some(current) => current,
        None => return Err(context.internal_load()),
    };
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

    let target = context
        .durable_state
        .open_conversation_target(session_id)
        .await
        .map_err(|error| map_conversation_target_load_error(&context, error))?;
    let loaded_conversation = load_replayed_conversation(target, context.task_context.clone())
        .await
        .map_err(|error| map_conversation_load_error(&context, error))?;
    let recorder = loaded_conversation.recorder;
    let live_state = loaded_conversation.live_state;
    let replay_diagnostics = loaded_conversation.diagnostics;
    let recorder_for_executor = recorder.clone();
    let conversation = LoadedSessionConversation::from_replay(
        live_state,
        recorder_for_executor,
        replay_diagnostics,
    );
    let executor = match SessionExecutor::start_loaded_ready_idle(
        context.task_context.clone(),
        context.durable_state.clone(),
        Arc::clone(&context.resolver),
        definition,
        workspace_snapshot,
        conversation,
    ) {
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

async fn run_unload(
    context: OperationContext,
    session_id: SessionId,
) -> Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;

    let Some((executor, permit)) = loaded_executor_and_permit(&context.state, session_id) else {
        return Ok(SessionResidencyUnloadOutcome::NoChange);
    };

    // Keep the map entry and exact permit installed until the executor's actor has drained.  A
    // concurrent lifecycle request therefore remains Busy until this operation removes residency.
    if let Err(SessionExecutorCloseError::InternalDispatchUnavailable) = executor.close().await {
        context.poison();
        return Err(SessionResidencyUnloadError::InternalDispatchUnavailable);
    }
    match context.state.remove_exact(session_id, &permit) {
        RemoveResult::Removed => Ok(SessionResidencyUnloadOutcome::Unloaded),
        RemoveResult::Missing | RemoveResult::PermitMismatch => {
            context.poison();
            Err(SessionResidencyUnloadError::InternalDispatchUnavailable)
        }
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

async fn run_workspace_definition(
    context: OperationContext,
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    workspace: Workspace,
    owner_timestamp: Timestamp,
) -> Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
    let gate = context.state.gate(session_id);
    let _permit = SessionResidencyOperationPermit::acquire(gate).await;
    if let Some(executor) = context.state.executor(session_id) {
        return match executor
            .update_workspace_definition(expected_revision, workspace, owner_timestamp)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error) => map_executor_workspace_error(&context, error),
        };
    }

    let attempt = SealedSessionDefinitionAttempt::new(
        session_id,
        expected_revision,
        Some(workspace),
        None,
        None,
        owner_timestamp,
    );
    let outcome = context
        .durable_state
        .update_session_definition(attempt)
        .await
        .map_err(|error| map_durable_definition_error(&context, error))?;
    map_unloaded_definition_outcome(&context, session_id, outcome)
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
) -> Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
    let error = match error {
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
        SessionWorkspaceDefinitionError::RevisionExhausted => {
            SessionResidencyWorkspaceDefinitionError::RevisionExhausted
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
        SessionWorkspaceDefinitionError::InternalDispatchUnavailable => {
            return Err(context.internal_workspace());
        }
    };
    Err(error)
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

fn map_unloaded_definition_outcome(
    context: &OperationContext,
    session_id: SessionId,
    outcome: DurableSessionDefinitionOutcome,
) -> Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
    let changed = outcome.changed();
    let head = outcome.head();
    let definition = outcome.definition();
    if !valid_current_shape(session_id, head, definition) {
        return Err(context.internal_workspace());
    }
    let definition_revision = definition.revision();
    let workspace_revision = definition.workspace().revision();
    Ok(if changed {
        SessionWorkspaceDefinitionOutcome::Updated {
            definition_revision,
            workspace_revision,
        }
    } else {
        SessionWorkspaceDefinitionOutcome::NoChange {
            definition_revision,
            workspace_revision,
        }
    })
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::runtime::Handle;

    use crate::agent_session_lifecycle::SealedSessionLifecycleAttempt;
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

    static NEXT_TEST_ROOT: AtomicUsize = AtomicUsize::new(1);

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
        let registry = SessionResidencyRegistry::start(context.clone(), state.clone(), resolver)
            .expect("the residency actor starts");
        (context, state, registry)
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
            let registry =
                SessionResidencyRegistry::start(context.clone(), state.clone(), resolver)
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
                assert_eq!(
                    result,
                    Err(SessionResidencyWorkspaceDefinitionError::WorkspaceUnavailable)
                );
            } else {
                assert!(result.is_ok());
            }
            assert_eq!(context.registered_task_count_for_test(), baseline);
        }

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
