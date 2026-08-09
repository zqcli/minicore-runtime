use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::agent_session_lifecycle::{SealedSessionCreateAttempt, SealedSessionLifecycleAttempt};
use crate::durable_state::{DurableOpenError, DurableSessionCreateError, DurableState};
use crate::model_gateway::{ModelCatalogView, ModelGateway};
use crate::prompt::{PromptResourceView, PromptService};
use crate::runtime_interface::{
    CommandCompletion, CommandError, CommandErrorCode, CommandOutcome, CommandOutput,
    CommandRequest, CommandResponse, EventFrame, LoadedSessionSummary, PublicCancelTarget,
    PublicSubject, QueryError, QueryResponse, QueryResult, QueuedFollowUpView, QueuedSteerView,
    RetryAdvice, RuntimeCapabilities, RuntimeCommand, RuntimeDispatchError, RuntimeQuery,
    RuntimeQueryResult, RuntimeReadQuery, RuntimeSnapshot, RuntimeStatusView, RuntimeView,
    SessionCommand, SessionDefinitionSummary, SessionExecutionView, SessionMetadataView,
    SessionQueueView, SessionReadinessView, SessionRecordingView, SessionSnapshot, SnapshotError,
    SnapshotErrorCode, SnapshotRequest, SnapshotResponse, StateEvent, SubmitAdmissionStateView,
    SubmitAdmissionView, SubscriptionError, SubscriptionErrorCode, SubscriptionRequest,
    SubscriptionScope, TurnCommand, TurnFailureView, TurnInterruptionView,
};
use crate::runtime_task::{Clock, RuntimeTaskContext, SystemClock};
use crate::session_execution::{
    SessionCancelTarget, SessionExecutionState, SessionExecutorSnapshot,
    SessionExecutorSubscription, SessionTurnFailure, SessionTurnInterruption, SessionTurnTerminal,
    SessionWorkspaceDefinitionOutcome,
};
use crate::session_residency::{
    SessionResidencyCancelError, SessionResidencyFollowUpError, SessionResidencyLifecycleError,
    SessionResidencyLoadError, SessionResidencyLoadOutcome, SessionResidencyQueuedMessageError,
    SessionResidencyRegistry, SessionResidencySnapshotError, SessionResidencyStartError,
    SessionResidencySteerError, SessionResidencySubmitError, SessionResidencySubscriptionError,
    SessionResidencyUnloadError, SessionResidencyUnloadOutcome,
    SessionResidencyWorkspaceDefinitionError,
};
use crate::wire::{
    ProtocolLimits, SessionDefinitionRevision, SessionId, Timestamp, WorkspaceRevision,
};
use crate::workspace::{
    Workspace, WorkspaceDefinitionSummaryView, WorkspacePathTarget, WorkspaceResolver,
    WorkspaceRootSummaryView, lower_workspace,
};

const DEFAULT_RUNTIME_REQUIRED_POLICY: &str = "Respond helpfully to the user's request.";

/// Host configuration for a MiniCore runtime instance.
#[non_exhaustive]
pub struct MiniCoreRuntimeConfig {
    durable_root: PathBuf,
}

impl MiniCoreRuntimeConfig {
    pub fn new(durable_root: PathBuf) -> Self {
        Self { durable_root }
    }
}

impl fmt::Debug for MiniCoreRuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MiniCoreRuntimeConfig { .. }")
    }
}

/// A closed, redacted failure result from runtime initialization.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum RuntimeInitializationError {
    RuntimeDependencyUnavailable,
    StoreInUse,
    UnsupportedStoreFormat,
    DurableStateCorrupt,
    DurableStateTooLarge,
    StorageUnavailable,
}

impl fmt::Debug for RuntimeInitializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RuntimeDependencyUnavailable => "RuntimeDependencyUnavailable",
            Self::StoreInUse => "StoreInUse",
            Self::UnsupportedStoreFormat => "UnsupportedStoreFormat",
            Self::DurableStateCorrupt => "DurableStateCorrupt",
            Self::DurableStateTooLarge => "DurableStateTooLarge",
            Self::StorageUnavailable => "StorageUnavailable",
        })
    }
}

impl fmt::Display for RuntimeInitializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RuntimeDependencyUnavailable => "runtime dependency unavailable",
            Self::StoreInUse => "durable store is already in use",
            Self::UnsupportedStoreFormat => "durable store format is unsupported",
            Self::DurableStateCorrupt => "durable state is corrupt",
            Self::DurableStateTooLarge => "durable state is too large",
            Self::StorageUnavailable => "durable storage unavailable",
        })
    }
}

impl Error for RuntimeInitializationError {}

/// The host lifecycle facade for the currently supported Store V1 runtime foundation.
pub struct MiniCoreRuntime {
    inner: Arc<RuntimeInner>,
}

impl MiniCoreRuntime {
    pub async fn open(
        config: MiniCoreRuntimeConfig,
        handle: Handle,
    ) -> Result<Self, RuntimeInitializationError> {
        Self::open_with_model_resources(config, handle, None).await
    }

    #[cfg(test)]
    async fn open_with_model_fixture(
        config: MiniCoreRuntimeConfig,
        handle: Handle,
        fixture: &crate::model_gateway::ScriptedModelFixture,
    ) -> Result<Self, RuntimeInitializationError> {
        Self::open_with_model_resources(
            config,
            handle,
            Some((Arc::clone(fixture.gateway()), Arc::clone(fixture.catalog()))),
        )
        .await
    }

    async fn open_with_model_resources(
        config: MiniCoreRuntimeConfig,
        handle: Handle,
        model_resources: Option<(Arc<ModelGateway>, Arc<ModelCatalogView>)>,
    ) -> Result<Self, RuntimeInitializationError> {
        let task_context = RuntimeTaskContext::new(handle)
            .await
            .map_err(|_| RuntimeInitializationError::RuntimeDependencyUnavailable)?;
        let prompt_service = match PromptService::new(
            Arc::from(DEFAULT_RUNTIME_REQUIRED_POLICY),
            None,
            Vec::new(),
            Vec::new(),
        ) {
            Ok(service) => Arc::new(service),
            Err(_) => {
                task_context.shutdown().await;
                return Err(RuntimeInitializationError::RuntimeDependencyUnavailable);
            }
        };
        let prompt_resources = match prompt_service.initialize().await {
            Ok(resources) => resources,
            Err(_) => {
                task_context.shutdown().await;
                return Err(RuntimeInitializationError::RuntimeDependencyUnavailable);
            }
        };
        let (model_gateway, model_catalog) = match model_resources {
            Some(resources) => resources,
            None => {
                let model_gateway = Arc::new(ModelGateway::new(Vec::new()));
                let model_catalog = match model_gateway.initialize().await {
                    Ok(catalog) => catalog,
                    Err(_) => {
                        task_context.shutdown().await;
                        return Err(RuntimeInitializationError::RuntimeDependencyUnavailable);
                    }
                };
                (model_gateway, model_catalog)
            }
        };
        let durable_state =
            match DurableState::open(config.durable_root, task_context.clone()).await {
                Ok(durable_state) => durable_state,
                Err(error) => {
                    task_context.shutdown().await;
                    return Err(error.into());
                }
            };

        let resolver = Arc::new(WorkspaceResolver::new(task_context.clone()));
        let session_residency = match SessionResidencyRegistry::start_with_turn_resources(
            task_context.clone(),
            durable_state.clone(),
            resolver,
            Arc::clone(&prompt_service),
            Arc::clone(&prompt_resources),
            Arc::clone(&model_gateway),
            Arc::clone(&model_catalog),
        ) {
            Ok(session_residency) => Arc::new(session_residency),
            Err(error) => {
                durable_state.close().await;
                return Err(match error {
                    SessionResidencyStartError::Closing
                    | SessionResidencyStartError::InternalDispatchUnavailable => {
                        RuntimeInitializationError::RuntimeDependencyUnavailable
                    }
                });
            }
        };

        let inner = Arc::new(RuntimeInner::new(
            task_context,
            durable_state,
            session_residency,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
        ));
        inner.retain_until_shutdown();
        Ok(Self { inner })
    }

    /// Closes admission, joins accepted work, and releases the Store V1 root lease.
    ///
    /// Hosts must await this before tearing down the injected Tokio runtime.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    pub async fn dispatch(
        &self,
        request: CommandRequest,
    ) -> Result<CommandResponse, RuntimeDispatchError> {
        self.inner.dispatch(request).await
    }

    pub async fn query(&self, query: RuntimeQuery) -> Result<QueryResponse, QueryError> {
        self.inner.query(query)
    }

    pub async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<SnapshotResponse, SnapshotError> {
        self.inner.snapshot(request).await
    }

    pub async fn subscribe(
        &self,
        request: SubscriptionRequest,
    ) -> Result<EventStream, SubscriptionError> {
        self.inner.subscribe(request).await
    }
}

pub struct EventStream {
    runtime: Arc<RuntimeInner>,
    initial: Option<EventFrame>,
    subscription: SessionExecutorSubscription,
}

impl EventStream {
    pub async fn recv(&mut self) -> Option<EventFrame> {
        if let Some(initial) = self.initial.take() {
            return Some(initial);
        }
        let event = self.subscription.recv().await?;
        let snapshot = self
            .runtime
            .public_session_snapshot(Arc::clone(event.snapshot()))
            .ok()?;
        let state = match event.terminal() {
            SessionTurnTerminal::Completed => StateEvent::turn_completed(
                event.timestamp(),
                Some(event.command_id()),
                snapshot,
                event.turn_id(),
                event.timestamp(),
            ),
            SessionTurnTerminal::Failed(failure) => StateEvent::turn_failed(
                event.timestamp(),
                Some(event.command_id()),
                snapshot,
                event.turn_id(),
                event.timestamp(),
                public_turn_failure(failure),
            ),
            SessionTurnTerminal::Interrupted(interruption) => StateEvent::turn_interrupted(
                event.timestamp(),
                Some(event.command_id()),
                snapshot,
                event.turn_id(),
                event.timestamp(),
                public_turn_interruption(interruption),
            ),
        };
        Some(EventFrame::State(state))
    }
}

impl fmt::Debug for EventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EventStream { .. }")
    }
}

impl fmt::Debug for MiniCoreRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MiniCoreRuntime { .. }")
    }
}

impl Drop for MiniCoreRuntime {
    fn drop(&mut self) {
        self.inner.request_closing();
    }
}

impl From<DurableOpenError> for RuntimeInitializationError {
    fn from(error: DurableOpenError) -> Self {
        match error {
            DurableOpenError::StoreInUse => Self::StoreInUse,
            DurableOpenError::UnsupportedStoreFormat => Self::UnsupportedStoreFormat,
            DurableOpenError::DurableStateCorrupt => Self::DurableStateCorrupt,
            DurableOpenError::DurableStateTooLarge => Self::DurableStateTooLarge,
            DurableOpenError::StorageUnavailable => Self::StorageUnavailable,
        }
    }
}

struct RuntimeInner {
    task_context: RuntimeTaskContext,
    #[allow(
        dead_code,
        reason = "the immediately adjacent Turn capture slice consumes the Runtime owner"
    )]
    prompt_service: Arc<PromptService>,
    #[allow(
        dead_code,
        reason = "the immediately adjacent shared-resource capture slice consumes this root"
    )]
    prompt_resources: Arc<PromptResourceView>,
    #[allow(
        dead_code,
        reason = "the immediately adjacent Turn capture slice consumes the Runtime owner"
    )]
    model_gateway: Arc<ModelGateway>,
    #[allow(
        dead_code,
        reason = "the immediately adjacent shared-resource capture slice consumes this root"
    )]
    model_catalog: Arc<ModelCatalogView>,
    retained_until_shutdown: Mutex<Option<Arc<RuntimeInner>>>,
    session_residency: Mutex<Option<Arc<SessionResidencyRegistry>>>,
    durable_state: Mutex<Option<DurableState>>,
    in_flight_commands: Mutex<BTreeMap<crate::wire::CommandId, Arc<RuntimeCommandInFlight>>>,
    lifecycle: Mutex<RuntimeLifecycle>,
    lifecycle_changed: Notify,
}

impl RuntimeInner {
    fn new(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        session_residency: Arc<SessionResidencyRegistry>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
    ) -> Self {
        Self {
            task_context,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
            retained_until_shutdown: Mutex::new(None),
            session_residency: Mutex::new(Some(session_residency)),
            durable_state: Mutex::new(Some(durable_state)),
            in_flight_commands: Mutex::new(BTreeMap::new()),
            lifecycle: Mutex::new(RuntimeLifecycle::Open),
            lifecycle_changed: Notify::new(),
        }
    }

    // A dropped facade must only request Closing; it cannot release the lease before an
    // awaited shutdown has drained the owner. Explicit shutdown breaks this retention.
    fn retain_until_shutdown(self: &Arc<Self>) {
        *lock(&self.retained_until_shutdown) = Some(Arc::clone(self));
    }

    #[cfg(test)]
    fn prompt_resources(&self) -> (&Arc<PromptService>, &Arc<PromptResourceView>) {
        (&self.prompt_service, &self.prompt_resources)
    }

    #[cfg(test)]
    fn model_resources(&self) -> (&Arc<ModelGateway>, &Arc<ModelCatalogView>) {
        (&self.model_gateway, &self.model_catalog)
    }

    async fn dispatch(
        self: &Arc<Self>,
        request: CommandRequest,
    ) -> Result<CommandResponse, RuntimeDispatchError> {
        let command_id = request.command_id();
        let command = request.command().clone();
        let (entry, leader) = {
            let mut in_flight = lock(&self.in_flight_commands);
            match in_flight.get(&command_id) {
                Some(existing) if existing.command() == &command => (Arc::clone(existing), false),
                Some(_) => {
                    return Ok(rejected_command(
                        command_id,
                        CommandErrorCode::CommandConflict,
                        "the command conflicts with an in-flight command",
                        RetryAdvice::DoNotRetry,
                        None,
                    ));
                }
                None => {
                    let entry = Arc::new(RuntimeCommandInFlight::new(command));
                    in_flight.insert(command_id, Arc::clone(&entry));
                    (entry, true)
                }
            }
        };

        if leader {
            let guard =
                RuntimeCommandOwnerGuard::new(Arc::clone(self), command_id, Arc::clone(&entry));
            let owner = RuntimeCommandOwner::new(request);
            match self.task_context.spawn_tracked(owner.run(guard)) {
                Ok(task) => self.task_context.reap_tracked(task),
                Err(_) => {
                    // The rejected future drops its pre-installed guard, settling the shared
                    // completion even if admission closes before the first poll.
                }
            }
        }
        entry.wait().await
    }

    async fn dispatch_once(
        &self,
        request: CommandRequest,
    ) -> Result<CommandResponse, RuntimeDispatchError> {
        let command_id = request.command_id();
        match *lock(&self.lifecycle) {
            RuntimeLifecycle::Open => {}
            RuntimeLifecycle::Closing { .. } => {
                return Ok(rejected_command(
                    command_id,
                    CommandErrorCode::RuntimeClosing,
                    "runtime is closing",
                    retry_with_backoff(),
                    Some(PublicSubject::Runtime),
                ));
            }
            RuntimeLifecycle::Closed => return Err(RuntimeDispatchError::RuntimeClosed),
        }
        let completion = match request.command().clone() {
            RuntimeCommand::Session(SessionCommand::Create {
                agent_id,
                definition,
                metadata,
            }) => {
                let workspace = match lower_workspace(
                    definition.workspace().clone(),
                    WorkspaceRevision::new(NonZeroU64::new(1).expect("one is non-zero")),
                    WorkspacePathTarget::current(),
                ) {
                    Ok(workspace) => workspace,
                    Err(_) => {
                        return Ok(rejected_command(
                            command_id,
                            CommandErrorCode::InvalidArgument,
                            "workspace input is invalid for this host",
                            RetryAdvice::DoNotRetry,
                            Some(PublicSubject::Agent(agent_id)),
                        ));
                    }
                };
                let attempt = match SealedSessionCreateAttempt::new(
                    agent_id,
                    workspace,
                    definition.model().clone(),
                    definition.prompts().clone(),
                    metadata.name(),
                    metadata.description(),
                    SystemClock.now(),
                ) {
                    Ok(attempt) => attempt,
                    Err(_) => {
                        return Ok(rejected_command(
                            command_id,
                            CommandErrorCode::InvalidArgument,
                            "session definition is invalid",
                            RetryAdvice::DoNotRetry,
                            Some(PublicSubject::Agent(agent_id)),
                        ));
                    }
                };
                let Some(durable_state) = lock(&self.durable_state).as_ref().cloned() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                match durable_state.create_session(attempt).await {
                    Ok(head) => completed_output(head.session_id().to_string()),
                    Err(error) => map_session_create_error(command_id, agent_id, error)?,
                }
            }
            RuntimeCommand::Session(SessionCommand::Load { session_id }) => {
                match self.load_session_ready_idle(session_id).await {
                    Ok(_) => completed_output("session loaded"),
                    Err(error) => map_load_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Session(SessionCommand::Unload { session_id }) => {
                match self.unload_session(session_id).await {
                    Ok(_) => completed_output("session unloaded"),
                    Err(error) => map_unload_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Turn(TurnCommand::Submit { session_id, intent }) => {
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                match residency.submit(session_id, command_id, intent).await {
                    Ok(turn_id) => CommandCompletion::Completed {
                        outcome: CommandOutcome::TurnStarted { turn_id },
                        output: None,
                    },
                    Err(error) => map_submit_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Turn(TurnCommand::Steer {
                session_id,
                expected_turn_id,
                intent,
            }) => {
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                match residency
                    .steer(session_id, expected_turn_id, command_id, intent)
                    .await
                {
                    Ok(()) => completed_outcome(CommandOutcome::SteerQueued {
                        turn_id: expected_turn_id,
                    }),
                    Err(error) => map_steer_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Turn(TurnCommand::FollowUp { session_id, intent }) => {
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                match residency.follow_up(session_id, command_id, intent).await {
                    Ok(()) => completed_outcome(CommandOutcome::FollowUpQueued),
                    Err(error) => map_follow_up_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Turn(TurnCommand::CancelQueuedMessage {
                session_id,
                target_command_id,
            }) => {
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                match residency
                    .cancel_queued_message(session_id, target_command_id)
                    .await
                {
                    Ok(()) => completed_outcome(CommandOutcome::QueuedMessageCancelled),
                    Err(error) => map_cancel_queued_message_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Turn(TurnCommand::Cancel { session_id, target }) => {
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                let target = match target {
                    PublicCancelTarget::Submit(command_id) => {
                        SessionCancelTarget::Submit(command_id)
                    }
                    PublicCancelTarget::Turn(turn_id) => SessionCancelTarget::Turn(turn_id),
                };
                match residency
                    .cancel(session_id, target, SystemClock.now())
                    .await
                {
                    Ok(accepted) => completed_outcome(CommandOutcome::CancelAccepted {
                        target: match accepted.target() {
                            SessionCancelTarget::Submit(command_id) => {
                                PublicCancelTarget::Submit(command_id)
                            }
                            SessionCancelTarget::Turn(turn_id) => PublicCancelTarget::Turn(turn_id),
                        },
                        cancel_epoch: accepted.cancel_epoch(),
                    }),
                    Err(error) => map_cancel_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Runtime(_) => rejected_completion(
                CommandErrorCode::ReloadValidationFailed,
                "shared resource reload is not available in this runtime slice",
                RetryAdvice::UserActionRequired,
                Some(PublicSubject::Runtime),
            ),
        };
        CommandResponse::new(command_id, completion)
            .map_err(|_| RuntimeDispatchError::InternalDispatchUnavailable)
    }

    fn query(&self, query: RuntimeQuery) -> Result<QueryResponse, QueryError> {
        if !matches!(*lock(&self.lifecycle), RuntimeLifecycle::Open) {
            return Err(QueryError::new(
                crate::runtime_interface::QueryErrorCode::RuntimeClosing,
                "runtime is closing",
                retry_with_backoff(),
                Some(PublicSubject::Runtime),
            ));
        }
        match query {
            RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities) => {
                Ok(QueryResponse::new(QueryResult::Runtime(
                    RuntimeQueryResult::Capabilities(implemented_runtime_capabilities()),
                )))
            }
        }
    }

    async fn snapshot(&self, request: SnapshotRequest) -> Result<SnapshotResponse, SnapshotError> {
        match request {
            SnapshotRequest::Session { session_id } => {
                let snapshot = self
                    .loaded_session_snapshot(session_id)
                    .await
                    .map_err(|error| map_snapshot_error(session_id, error))?;
                self.public_session_snapshot(snapshot)
                    .map(|snapshot| SnapshotResponse::Session(Box::new(snapshot)))
            }
            SnapshotRequest::Runtime => {
                let Some(residency) = self.residency() else {
                    return Err(runtime_snapshot_closing());
                };
                let mut loaded = Vec::new();
                for snapshot in residency.loaded_session_snapshots() {
                    let session_id = snapshot.definition().session_id();
                    loaded.push(
                        LoadedSessionSummary::new(
                            session_id,
                            SessionReadinessView::Ready,
                            public_execution_state(snapshot.execution_state()),
                            SessionRecordingView::new(snapshot.recording()),
                        )
                        .map_err(|_| unavailable_snapshot(session_id))?,
                    );
                }
                let status = if matches!(*lock(&self.lifecycle), RuntimeLifecycle::Open) {
                    RuntimeStatusView::Running
                } else {
                    RuntimeStatusView::Closing
                };
                RuntimeSnapshot::new(RuntimeView::new(status), loaded, Vec::new())
                    .map(SnapshotResponse::Runtime)
                    .map_err(|_| runtime_snapshot_closing())
            }
        }
    }

    async fn subscribe(
        self: &Arc<Self>,
        request: SubscriptionRequest,
    ) -> Result<EventStream, SubscriptionError> {
        let SubscriptionScope::Session { session_id } = request.scope() else {
            return Err(SubscriptionError::new(
                SubscriptionErrorCode::UnsupportedScope,
                "runtime event scope is not available in this runtime slice",
                RetryAdvice::DoNotRetry,
                Some(PublicSubject::Runtime),
            ));
        };
        let Some(residency) = self.residency() else {
            return Err(subscription_closing());
        };
        let subscription = residency
            .subscribe(session_id)
            .await
            .map_err(|error| map_subscription_error(session_id, error))?;
        let initial = self
            .public_session_snapshot(Arc::clone(subscription.snapshot()))
            .map_err(|_| {
                SubscriptionError::new(
                    SubscriptionErrorCode::PublisherUnavailable,
                    "session event publisher is unavailable",
                    RetryAdvice::DoNotRetry,
                    Some(PublicSubject::Session(session_id)),
                )
            })?;
        Ok(EventStream {
            runtime: Arc::clone(self),
            initial: Some(EventFrame::Snapshot(SnapshotResponse::Session(Box::new(
                initial,
            )))),
            subscription,
        })
    }

    fn public_session_snapshot(
        &self,
        snapshot: Arc<SessionExecutorSnapshot>,
    ) -> Result<SessionSnapshot, SnapshotError> {
        let session_id = snapshot.definition().session_id();
        let Some(durable_state) = lock(&self.durable_state).as_ref().cloned() else {
            return Err(runtime_snapshot_closing());
        };
        let current = durable_state
            .session_current(session_id)
            .ok_or_else(|| not_found_snapshot(session_id))?;
        if current.definition().as_ref() != snapshot.definition().as_ref() {
            return Err(unavailable_snapshot(session_id));
        }
        let metadata = current.head().metadata();
        let metadata = SessionMetadataView::new(
            metadata.revision(),
            metadata.name(),
            metadata.description(),
            metadata.updated_at(),
        )
        .map_err(|_| unavailable_snapshot(session_id))?;
        let definition = snapshot.definition();
        let definition = SessionDefinitionSummary::new(
            session_id,
            definition.revision(),
            definition.agent(),
            workspace_summary(definition.workspace())
                .map_err(|_| unavailable_snapshot(session_id))?,
            definition.model().clone(),
            definition.prompts().clone(),
            definition.created_at(),
        );
        let execution = public_execution_state(snapshot.execution_state());
        let submit_admissions = snapshot
            .active_submit_command_id()
            .map(|command_id| {
                SubmitAdmissionView::new(
                    command_id,
                    if execution == SessionExecutionView::Starting {
                        SubmitAdmissionStateView::Starting
                    } else {
                        SubmitAdmissionStateView::Queued
                    },
                )
            })
            .into_iter()
            .collect();
        let steers = snapshot
            .current_turn()
            .map(|turn_id| {
                snapshot
                    .steer_command_ids()
                    .iter()
                    .copied()
                    .map(|command_id| QueuedSteerView::new(command_id, turn_id))
                    .collect()
            })
            .unwrap_or_default();
        let follow_ups = snapshot
            .follow_up_command_ids()
            .iter()
            .copied()
            .map(QueuedFollowUpView::new)
            .collect();
        let queues = SessionQueueView::new(
            submit_admissions,
            steers,
            follow_ups,
            matches!(
                execution,
                SessionExecutionView::Idle | SessionExecutionView::Running
            ),
        )
        .map_err(|_| unavailable_snapshot(session_id))?;
        SessionSnapshot::new_loaded_ready_with_observation(
            session_id,
            metadata,
            definition,
            execution,
            snapshot.current_turn_view(),
            snapshot.active_items().to_vec(),
            snapshot.public_pending_interactions().to_vec(),
            queues,
            SessionRecordingView::new(snapshot.recording()),
            snapshot.usage().cloned(),
            snapshot.diagnostics().to_vec(),
            ProtocolLimits::v1_0(),
        )
        .map_err(|_| unavailable_snapshot(session_id))
    }

    fn request_closing(&self) {
        let mut lifecycle = lock(&self.lifecycle);
        let changed = *lifecycle == RuntimeLifecycle::Open;
        if *lifecycle == RuntimeLifecycle::Open {
            *lifecycle = RuntimeLifecycle::Closing {
                shutdown_active: false,
            };
        }
        drop(lifecycle);
        self.request_session_residency_closing();
        self.request_durable_actor_closing();
        self.task_context.request_closing();
        if changed {
            self.lifecycle_changed.notify_waiters();
        }
    }

    async fn shutdown(self: &Arc<Self>) {
        loop {
            // Register before inspecting leadership so a cancelled leader cannot clear its
            // claim between our inspection and this wait.
            let notified = self.lifecycle_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            match self.begin_shutdown() {
                RuntimeShutdownAttempt::Leader(mut leadership) => {
                    // Keep each original owner in its mutex while awaiting. A cancelled leader
                    // therefore retains the loaded executors, DurableState, and root lease for
                    // the next shutdown leader to take over.
                    let session_residency = lock(&self.session_residency).as_ref().cloned();
                    if let Some(session_residency) = session_residency {
                        session_residency.close().await;
                        let removed = lock(&self.session_residency).take();
                        drop(removed);
                    }
                    let durable_state = lock(&self.durable_state).as_ref().cloned();
                    if let Some(durable_state) = durable_state {
                        durable_state.close().await;
                        let removed = lock(&self.durable_state).take();
                        drop(removed);
                    } else {
                        self.task_context.shutdown().await;
                    }
                    self.complete_shutdown();
                    leadership.complete();
                    return;
                }
                RuntimeShutdownAttempt::Closed => return,
                RuntimeShutdownAttempt::Waiting => notified.await,
            }
        }
    }

    fn request_durable_actor_closing(&self) {
        if let Some(durable_state) = lock(&self.durable_state).as_ref() {
            durable_state.request_closing();
        }
    }

    fn request_session_residency_closing(&self) {
        if let Some(session_residency) = lock(&self.session_residency).as_ref() {
            session_residency.request_closing();
        }
    }

    fn residency(&self) -> Option<Arc<SessionResidencyRegistry>> {
        lock(&self.session_residency).as_ref().cloned()
    }

    #[allow(
        dead_code,
        reason = "the pending public Session load route consumes this Runtime-owned seam"
    )]
    async fn load_session_ready_idle(
        &self,
        session_id: SessionId,
    ) -> Result<SessionResidencyLoadOutcome, SessionResidencyLoadError> {
        match self.residency() {
            Some(residency) => residency.load_ready_idle(session_id).await,
            None => Err(SessionResidencyLoadError::Closing),
        }
    }

    #[allow(
        dead_code,
        reason = "the pending public Session unload route consumes this Runtime-owned seam"
    )]
    async fn unload_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionResidencyUnloadOutcome, SessionResidencyUnloadError> {
        match self.residency() {
            Some(residency) => residency.unload(session_id).await,
            None => Err(SessionResidencyUnloadError::Closing),
        }
    }

    #[allow(
        dead_code,
        reason = "the pending public Session lifecycle routes consume this owner seam"
    )]
    async fn update_session_lifecycle(
        &self,
        attempt: SealedSessionLifecycleAttempt,
    ) -> Result<crate::durable_state::DurableSessionLifecycleOutcome, SessionResidencyLifecycleError>
    {
        match self.residency() {
            Some(residency) => residency.update_lifecycle(attempt).await,
            None => Err(SessionResidencyLifecycleError::Closing),
        }
    }

    #[allow(
        dead_code,
        reason = "the pending public Session snapshot route consumes this owner seam"
    )]
    async fn loaded_session_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<SessionExecutorSnapshot>, SessionResidencySnapshotError> {
        match self.residency() {
            Some(residency) => residency.snapshot(session_id).await,
            None => Err(SessionResidencySnapshotError::Closing),
        }
    }

    #[allow(
        dead_code,
        reason = "the pending Session definition command consumes this owner seam"
    )]
    async fn update_session_workspace_definition(
        &self,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionResidencyWorkspaceDefinitionError> {
        match self.residency() {
            Some(residency) => {
                residency
                    .update_workspace_definition(
                        session_id,
                        expected_revision,
                        workspace,
                        owner_timestamp,
                    )
                    .await
            }
            None => Err(SessionResidencyWorkspaceDefinitionError::Closing),
        }
    }

    fn finish_in_flight(
        &self,
        command_id: crate::wire::CommandId,
        entry: &Arc<RuntimeCommandInFlight>,
    ) {
        let mut in_flight = lock(&self.in_flight_commands);
        if in_flight
            .get(&command_id)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            in_flight.remove(&command_id);
        }
    }

    fn begin_shutdown(self: &Arc<Self>) -> RuntimeShutdownAttempt {
        let mut lifecycle = lock(&self.lifecycle);
        match *lifecycle {
            RuntimeLifecycle::Open => {
                *lifecycle = RuntimeLifecycle::Closing {
                    shutdown_active: true,
                };
                RuntimeShutdownAttempt::Leader(RuntimeShutdownLeadership::new(Arc::clone(self)))
            }
            RuntimeLifecycle::Closing {
                shutdown_active: false,
            } => {
                *lifecycle = RuntimeLifecycle::Closing {
                    shutdown_active: true,
                };
                RuntimeShutdownAttempt::Leader(RuntimeShutdownLeadership::new(Arc::clone(self)))
            }
            RuntimeLifecycle::Closing {
                shutdown_active: true,
            } => RuntimeShutdownAttempt::Waiting,
            RuntimeLifecycle::Closed => RuntimeShutdownAttempt::Closed,
        }
    }

    fn complete_shutdown(&self) {
        let mut lifecycle = lock(&self.lifecycle);
        *lifecycle = RuntimeLifecycle::Closed;
        drop(lifecycle);
        self.lifecycle_changed.notify_waiters();
        let retained = lock(&self.retained_until_shutdown).take();
        drop(retained);
    }
}

struct RuntimeCommandInFlight {
    command: RuntimeCommand,
    result: Mutex<Option<Result<CommandResponse, RuntimeDispatchError>>>,
    changed: Notify,
}

impl RuntimeCommandInFlight {
    fn new(command: RuntimeCommand) -> Self {
        Self {
            command,
            result: Mutex::new(None),
            changed: Notify::new(),
        }
    }

    fn command(&self) -> &RuntimeCommand {
        &self.command
    }

    fn complete(&self, result: Result<CommandResponse, RuntimeDispatchError>) {
        let mut stored = lock(&self.result);
        if stored.is_none() {
            *stored = Some(result);
            drop(stored);
            self.changed.notify_waiters();
        }
    }

    async fn wait(&self) -> Result<CommandResponse, RuntimeDispatchError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = lock(&self.result).clone() {
                return result;
            }
            changed.await;
        }
    }
}

struct RuntimeCommandOwner {
    request: CommandRequest,
}

impl RuntimeCommandOwner {
    fn new(request: CommandRequest) -> Self {
        Self { request }
    }

    async fn run(self, mut guard: RuntimeCommandOwnerGuard) {
        let RuntimeCommandOwner { request } = self;
        let result = guard.inner.dispatch_once(request).await;
        guard.complete(result);
    }
}

struct RuntimeCommandOwnerGuard {
    inner: Arc<RuntimeInner>,
    command_id: crate::wire::CommandId,
    entry: Arc<RuntimeCommandInFlight>,
    completed: bool,
}

impl RuntimeCommandOwnerGuard {
    fn new(
        inner: Arc<RuntimeInner>,
        command_id: crate::wire::CommandId,
        entry: Arc<RuntimeCommandInFlight>,
    ) -> Self {
        Self {
            inner,
            command_id,
            entry,
            completed: false,
        }
    }

    fn complete(&mut self, result: Result<CommandResponse, RuntimeDispatchError>) {
        self.entry.complete(result);
        self.inner.finish_in_flight(self.command_id, &self.entry);
        self.completed = true;
    }
}

impl Drop for RuntimeCommandOwnerGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.inner.request_closing();
            self.complete(Err(RuntimeDispatchError::InternalDispatchUnavailable));
        }
    }
}

fn completed_output(text: impl AsRef<str>) -> CommandCompletion {
    CommandCompletion::Completed {
        outcome: CommandOutcome::CommandOutput,
        output: Some(
            CommandOutput::new(text).expect("Runtime command output is bounded safe text"),
        ),
    }
}

fn completed_outcome(outcome: CommandOutcome) -> CommandCompletion {
    debug_assert!(!matches!(outcome, CommandOutcome::CommandOutput));
    CommandCompletion::Completed {
        outcome,
        output: None,
    }
}

fn implemented_runtime_capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities::for_v1(vec![
        crate::runtime_interface::RuntimeCapability::StateEvents,
        crate::runtime_interface::RuntimeCapability::RuntimeSnapshot,
        crate::runtime_interface::RuntimeCapability::SessionSnapshot,
    ])
    .expect("the M7 Runtime capability set is a canonical V1 subset")
}

const fn retry_with_backoff() -> RetryAdvice {
    RetryAdvice::RetryWithBackoff { retry_after: None }
}

fn rejected_completion(
    code: CommandErrorCode,
    message: &'static str,
    retry: RetryAdvice,
    subject: Option<PublicSubject>,
) -> CommandCompletion {
    CommandCompletion::Rejected(
        CommandError::new(code, message, retry, subject)
            .expect("Runtime command errors use a valid closed machine contract"),
    )
}

fn rejected_command(
    command_id: crate::wire::CommandId,
    code: CommandErrorCode,
    message: &'static str,
    retry: RetryAdvice,
    subject: Option<PublicSubject>,
) -> CommandResponse {
    CommandResponse::new(
        command_id,
        rejected_completion(code, message, retry, subject),
    )
    .expect("a rejected command has no output")
}

fn map_session_create_error(
    _command_id: crate::wire::CommandId,
    agent_id: crate::wire::AgentId,
    error: DurableSessionCreateError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let completion = match error {
        DurableSessionCreateError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        DurableSessionCreateError::AgentNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Agent was not found",
            RetryAdvice::RefreshAndRetry,
            Some(PublicSubject::Agent(agent_id)),
        ),
        DurableSessionCreateError::AgentDisabled => rejected_completion(
            CommandErrorCode::AgentDisabled,
            "Agent is disabled",
            RetryAdvice::UserActionRequired,
            Some(PublicSubject::Agent(agent_id)),
        ),
        DurableSessionCreateError::AgentDeleted => rejected_completion(
            CommandErrorCode::AgentDeleted,
            "Agent is deleted",
            RetryAdvice::DoNotRetry,
            Some(PublicSubject::Agent(agent_id)),
        ),
        DurableSessionCreateError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            Some(PublicSubject::Runtime),
        ),
        DurableSessionCreateError::IdentityUnavailable
        | DurableSessionCreateError::CollisionAttemptsExhausted
        | DurableSessionCreateError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "session creation is unavailable",
            retry_with_backoff(),
            Some(PublicSubject::Agent(agent_id)),
        ),
        DurableSessionCreateError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_load_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyLoadError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyLoadError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyLoadError::SessionNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Session was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyLoadError::SessionArchived => rejected_completion(
            CommandErrorCode::SessionArchived,
            "Session is archived",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyLoadError::SessionDeleted => rejected_completion(
            CommandErrorCode::SessionDeleted,
            "Session is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyLoadError::StaleDefinition => rejected_completion(
            CommandErrorCode::StaleRevision,
            "Session definition changed while loading",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyLoadError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyLoadError::WorkspaceUnavailable
        | SessionResidencyLoadError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Session could not be loaded",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyLoadError::RecordedStateCorrupt => rejected_completion(
            CommandErrorCode::DurableStateCorrupt,
            "recorded Session state is corrupt",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyLoadError::WorkspaceRejected => rejected_completion(
            CommandErrorCode::Unavailable,
            "Session Workspace was rejected",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyLoadError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_unload_error(
    _command_id: crate::wire::CommandId,
    _session_id: SessionId,
    error: SessionResidencyUnloadError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    match error {
        SessionResidencyUnloadError::Closing => Ok(rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        )),
        SessionResidencyUnloadError::InternalDispatchUnavailable => {
            Err(RuntimeDispatchError::InternalDispatchUnavailable)
        }
    }
}

fn map_submit_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencySubmitError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencySubmitError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencySubmitError::CommandConflict => rejected_completion(
            CommandErrorCode::CommandConflict,
            "the command conflicts with an in-flight command",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencySubmitError::SessionNotLoaded => rejected_completion(
            CommandErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencySubmitError::SessionBusy => rejected_completion(
            CommandErrorCode::SessionBusy,
            "Session is busy",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencySubmitError::AgentUnavailable
        | SessionResidencySubmitError::DependencyUnavailable
        | SessionResidencySubmitError::Prompt => rejected_completion(
            CommandErrorCode::Unavailable,
            "Turn dependencies are unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencySubmitError::InvalidArgument => rejected_completion(
            CommandErrorCode::InvalidArgument,
            "Turn input is invalid",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencySubmitError::ContextOverflow => rejected_completion(
            CommandErrorCode::InvalidArgument,
            "Turn input exceeds the model context limit",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencySubmitError::Cancelled => {
            completed_outcome(CommandOutcome::SubmitCancelled)
        }
        SessionResidencySubmitError::Unauthorized => rejected_completion(
            CommandErrorCode::Unauthorized,
            "Session authority was revoked",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencySubmitError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_follow_up_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyFollowUpError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyFollowUpError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyFollowUpError::SessionNotLoaded => rejected_completion(
            CommandErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyFollowUpError::TurnNotRunning => rejected_completion(
            CommandErrorCode::TurnNotRunning,
            "the Session has no active Turn",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyFollowUpError::CommandConflict => rejected_completion(
            CommandErrorCode::CommandConflict,
            "the FollowUp command conflicts with an admitted command",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyFollowUpError::QueueFull => rejected_completion(
            CommandErrorCode::IngressLaneFull {
                lane: crate::runtime_interface::PublicIngressLane::FollowUp,
            },
            "the FollowUp queue is full",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyFollowUpError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_steer_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencySteerError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencySteerError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencySteerError::SessionNotLoaded => rejected_completion(
            CommandErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencySteerError::TurnNotRunning => rejected_completion(
            CommandErrorCode::TurnNotRunning,
            "the Turn is not running",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencySteerError::TurnCancelling => rejected_completion(
            CommandErrorCode::TurnCancelling,
            "the Turn is already cancelling",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencySteerError::ExpectedTurnMismatch => rejected_completion(
            CommandErrorCode::ExpectedTurnMismatch,
            "the Steer target does not match the active Turn",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencySteerError::CommandConflict => rejected_completion(
            CommandErrorCode::CommandConflict,
            "the Steer command conflicts with an admitted command",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencySteerError::QueueFull => rejected_completion(
            CommandErrorCode::IngressLaneFull {
                lane: crate::runtime_interface::PublicIngressLane::Steer,
            },
            "the Steer queue is full",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencySteerError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_cancel_queued_message_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyQueuedMessageError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyQueuedMessageError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyQueuedMessageError::SessionNotLoaded => rejected_completion(
            CommandErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyQueuedMessageError::NotQueued => rejected_completion(
            CommandErrorCode::QueuedMessageNotQueued,
            "the queued message is not queued",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyQueuedMessageError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_cancel_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyCancelError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyCancelError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyCancelError::SessionNotLoaded => rejected_completion(
            CommandErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyCancelError::SubmitNotCancellable => rejected_completion(
            CommandErrorCode::SubmitNotCancellable,
            "the Submit is no longer cancellable",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyCancelError::ExpectedTurnMismatch => rejected_completion(
            CommandErrorCode::ExpectedTurnMismatch,
            "the Turn target does not match the active Turn",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyCancelError::TurnNotRunning => rejected_completion(
            CommandErrorCode::TurnNotRunning,
            "the Turn is not running",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyCancelError::TurnCancelling => rejected_completion(
            CommandErrorCode::TurnCancelling,
            "the Turn is already cancelling",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyCancelError::TurnTerminal => rejected_completion(
            CommandErrorCode::TurnTerminal,
            "the Turn is already terminal",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyCancelError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_snapshot_error(
    session_id: SessionId,
    error: SessionResidencySnapshotError,
) -> SnapshotError {
    match error {
        SessionResidencySnapshotError::Closing => runtime_snapshot_closing(),
        SessionResidencySnapshotError::SessionNotLoaded => SnapshotError::new(
            SnapshotErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            Some(PublicSubject::Session(session_id)),
        ),
        SessionResidencySnapshotError::InternalDispatchUnavailable => {
            unavailable_snapshot(session_id)
        }
    }
}

fn map_subscription_error(
    session_id: SessionId,
    error: SessionResidencySubscriptionError,
) -> SubscriptionError {
    match error {
        SessionResidencySubscriptionError::Closing => subscription_closing(),
        SessionResidencySubscriptionError::SessionNotLoaded => SubscriptionError::new(
            SubscriptionErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            Some(PublicSubject::Session(session_id)),
        ),
        SessionResidencySubscriptionError::PublisherUnavailable => SubscriptionError::new(
            SubscriptionErrorCode::PublisherUnavailable,
            "session event publisher is unavailable",
            RetryAdvice::DoNotRetry,
            Some(PublicSubject::Session(session_id)),
        ),
    }
}

fn subscription_closing() -> SubscriptionError {
    SubscriptionError::new(
        SubscriptionErrorCode::RuntimeClosing,
        "runtime is closing",
        retry_with_backoff(),
        Some(PublicSubject::Runtime),
    )
}

fn public_turn_failure(failure: SessionTurnFailure) -> TurnFailureView {
    match failure {
        SessionTurnFailure::Prompt => TurnFailureView::Prompt,
        SessionTurnFailure::Model => TurnFailureView::Model,
        SessionTurnFailure::ContextOverflow => TurnFailureView::ContextOverflow,
        SessionTurnFailure::AgentUnavailable => TurnFailureView::DependencyUnavailable,
        SessionTurnFailure::Internal => TurnFailureView::InvariantFailure,
        SessionTurnFailure::EmergencyControl(_) => TurnFailureView::InvariantFailure,
    }
}

fn public_turn_interruption(interruption: SessionTurnInterruption) -> TurnInterruptionView {
    match interruption {
        SessionTurnInterruption::UserCancelled => TurnInterruptionView::UserCancelled,
        SessionTurnInterruption::SecurityRevoked => TurnInterruptionView::SecurityRevoked,
    }
}

fn runtime_snapshot_closing() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::RuntimeClosing,
        "runtime is closing",
        retry_with_backoff(),
        Some(PublicSubject::Runtime),
    )
}

fn not_found_snapshot(session_id: SessionId) -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::NotFound,
        "Session was not found",
        RetryAdvice::RefreshAndRetry,
        Some(PublicSubject::Session(session_id)),
    )
}

fn unavailable_snapshot(session_id: SessionId) -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::Unavailable,
        "Session snapshot is unavailable",
        RetryAdvice::DoNotRetry,
        Some(PublicSubject::Session(session_id)),
    )
}

fn public_execution_state(state: SessionExecutionState) -> SessionExecutionView {
    match state {
        SessionExecutionState::Idle => SessionExecutionView::Idle,
        SessionExecutionState::Starting => SessionExecutionView::Starting,
        SessionExecutionState::Running => SessionExecutionView::Running,
        SessionExecutionState::Finishing => SessionExecutionView::Finishing,
    }
}

fn workspace_summary(workspace: &Workspace) -> Result<WorkspaceDefinitionSummaryView, ()> {
    let primary = workspace.primary_root();
    let primary = WorkspaceRootSummaryView::new(
        primary.key().clone(),
        primary.requested_access(),
        primary.sources(),
    );
    let additional = workspace
        .additional_roots()
        .iter()
        .map(|root| {
            WorkspaceRootSummaryView::new(
                root.key().clone(),
                root.requested_access(),
                root.sources(),
            )
        })
        .collect();
    WorkspaceDefinitionSummaryView::new(primary, additional, workspace.cwd().clone())
        .map_err(|_| ())
}

enum RuntimeShutdownAttempt {
    Leader(RuntimeShutdownLeadership),
    Waiting,
    Closed,
}

/// Holds the runtime shutdown claim until close completes or its caller cancels the future.
struct RuntimeShutdownLeadership {
    inner: Arc<RuntimeInner>,
    completed: bool,
}

impl RuntimeShutdownLeadership {
    fn new(inner: Arc<RuntimeInner>) -> Self {
        Self {
            inner,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RuntimeShutdownLeadership {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut lifecycle = lock(&self.inner.lifecycle);
        let was_active = matches!(
            *lifecycle,
            RuntimeLifecycle::Closing {
                shutdown_active: true
            }
        );
        if was_active {
            *lifecycle = RuntimeLifecycle::Closing {
                shutdown_active: false,
            };
        }
        drop(lifecycle);
        if was_active {
            self.inner.lifecycle_changed.notify_waiters();
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RuntimeLifecycle {
    Open,
    Closing { shutdown_active: bool },
    Closed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::future::{Future, poll_fn};
    use std::num::NonZeroU32;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::runtime::Handle;
    use tokio::sync::Notify;

    use super::{
        MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError, RuntimeLifecycle,
    };
    use crate::agent_session_lifecycle::{
        SealedAgentCreateAttempt, SealedSessionCreateAttempt, SealedSessionLifecycleAttempt,
        SessionModelConfig,
    };
    use crate::conversation_storage::{RecordOutcome, RecorderWriteBarrier, SessionHeader};
    use crate::model_gateway::{
        ModelCallErrorReason, ModelSelection, ReasoningPreference, ScriptedModelFixture,
    };
    use crate::prompt::{
        AgentPromptSelection, PromptBodyIntent, PromptIntent, SessionPromptSelection, TextIntent,
    };
    use crate::runtime_interface::{
        CommandCompletion, CommandErrorCode, CommandOutcome, CommandRequest, CommandResponse,
        EventFrame, EventRoute, ItemContentView, NewSessionDefinition, NewSessionMetadata,
        PublicCancelTarget, QueryResult, RetryAdvice, RuntimeCapability, RuntimeCommand,
        RuntimeQuery, RuntimeQueryResult, RuntimeReadQuery, SessionCommand, SessionEventDetail,
        SessionExecutionView, SessionRecordingState, SessionStateEventKind, SnapshotRequest,
        SnapshotResponse, SubscriptionRequest, SubscriptionScope, TurnCommand, TurnFailureView,
        TurnTerminalView,
    };
    use crate::runtime_task::RuntimeTaskError;
    use crate::session_execution::SessionExecutionState;
    use crate::session_residency::{
        SessionResidencyLifecycleError, SessionResidencyLoadError, SessionResidencyLoadOutcome,
        SessionResidencyUnloadOutcome,
    };
    use crate::wire::conversation_jsonl::ConversationLineCodec;
    use crate::wire::{AgentId, CanonicalFileUri, CommandId, FileUriFamily, SessionId, TurnId};
    use crate::workspace::{
        RequestedFilesystemAccess, Workspace, WorkspaceCwdSpec, WorkspaceDefinitionInput,
        WorkspacePathTarget, WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy,
        lower_workspace,
    };

    static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            loop {
                let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
                assert_ne!(suffix, 0, "test root suffix must be nonzero");
                let path = std::env::temp_dir().join(format!(
                    "minicore-runtime-lifecycle-{}-{suffix}",
                    std::process::id()
                ));
                if !path.exists() {
                    return Self { path };
                }
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            if self.path.exists() {
                fs::remove_dir_all(&self.path)
                    .expect("the temporary runtime root is removed deterministically");
            }
        }
    }

    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            loop {
                let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
                assert_ne!(suffix, 0, "test Workspace suffix must be nonzero");
                let path = std::env::temp_dir().join(format!(
                    "minicore-runtime-workspace-{}-{suffix}",
                    std::process::id()
                ));
                if path.exists() {
                    continue;
                }
                fs::create_dir(&path).expect("the temporary Workspace root is created");
                fs::create_dir(path.join("src")).expect("the temporary Workspace cwd is created");
                return Self { path };
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            if self.path.exists() {
                fs::remove_dir_all(&self.path)
                    .expect("the temporary Workspace root is removed deterministically");
            }
        }
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

    fn workspace_input(path: &Path) -> WorkspaceDefinitionInput {
        let key: WorkspaceRootKey = "repo".parse().unwrap();
        WorkspaceDefinitionInput::new(
            WorkspaceRootInput::new(
                key.clone(),
                workspace_uri(path),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            WorkspaceCwdSpec::new(key, "src".parse().unwrap()),
        )
        .unwrap()
    }

    fn workspace_with_revision(path: &Path, revision: &str) -> Workspace {
        lower_workspace(
            workspace_input(path),
            revision.parse().unwrap(),
            WorkspacePathTarget::current(),
        )
        .unwrap()
    }

    fn workspace(path: &Path) -> Workspace {
        workspace_with_revision(path, "wr_1")
    }

    fn changed_workspace(path: &Path) -> Workspace {
        workspace_with_revision(path, "wr_99")
    }

    async fn create_runtime_agent(runtime: &MiniCoreRuntime) -> AgentId {
        let durable_state = super::lock(&runtime.inner.durable_state)
            .as_ref()
            .cloned()
            .expect("the open Runtime retains DurableState");
        let created_at = "2026-08-03T10:01:00.456Z".parse().unwrap();
        durable_state
            .create_agent(
                SealedAgentCreateAttempt::new(
                    AgentPromptSelection::new(Vec::new()).unwrap(),
                    "Runtime Test Agent",
                    None::<&str>,
                    created_at,
                )
                .unwrap(),
            )
            .await
            .expect("the Runtime test Agent is published")
            .agent_id()
    }

    async fn create_runtime_session(runtime: &MiniCoreRuntime, workspace_root: &Path) -> SessionId {
        let durable_state = super::lock(&runtime.inner.durable_state)
            .as_ref()
            .cloned()
            .expect("the open Runtime retains DurableState");
        let agent_id = create_runtime_agent(runtime).await;
        let created_at = "2026-08-03T10:01:00.456Z".parse().unwrap();
        durable_state
            .create_session(
                SealedSessionCreateAttempt::new(
                    agent_id,
                    workspace(workspace_root),
                    SessionModelConfig::new(
                        ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
                        ReasoningPreference::Auto,
                        Some(NonZeroU32::new(4096).unwrap()),
                    ),
                    SessionPromptSelection::new(Vec::new()).unwrap(),
                    None::<&str>,
                    None::<&str>,
                    created_at,
                )
                .unwrap(),
            )
            .await
            .expect("the Runtime test Session is published")
            .session_id()
    }

    fn command_output(response: &CommandResponse) -> &str {
        match response.completion() {
            CommandCompletion::Completed {
                outcome: CommandOutcome::CommandOutput,
                output: Some(output),
            } => output.text(),
            completion => panic!("expected command output, got {completion:?}"),
        }
    }

    fn started_turn(response: &CommandResponse) -> TurnId {
        match response.completion() {
            CommandCompletion::Completed {
                outcome: CommandOutcome::TurnStarted { turn_id },
                output: None,
            } => *turn_id,
            completion => panic!("expected started Turn, got {completion:?}"),
        }
    }

    async fn create_and_load_public_session(
        runtime: &MiniCoreRuntime,
        workspace_root: &Path,
    ) -> SessionId {
        let agent_id = create_runtime_agent(runtime).await;
        let create = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Create {
                    agent_id,
                    definition: Box::new(NewSessionDefinition::new(
                        workspace_input(workspace_root),
                        SessionModelConfig::new(
                            ModelSelection::new(
                                "openai".parse().unwrap(),
                                "gpt-5".parse().unwrap(),
                            ),
                            ReasoningPreference::Auto,
                            Some(NonZeroU32::new(4096).unwrap()),
                        ),
                        SessionPromptSelection::new(Vec::new()).unwrap(),
                    )),
                    metadata: NewSessionMetadata::new(None::<&str>, None::<&str>).unwrap(),
                }),
            ))
            .await
            .expect("public Create dispatches");
        let session_id = command_output(&create).parse().unwrap();

        let load = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("public Load dispatches");
        assert_eq!(command_output(&load), "session loaded");
        session_id
    }

    fn replayed_user_entry(session_id: SessionId, line_index: usize) -> Vec<u8> {
        let source = include_str!(
            "../docs/fixtures/wire-v1/conversation/golden/user-sources-and-stamps.jsonl"
        );
        let entry = source
            .lines()
            .nth(line_index)
            .expect("the replay fixture has a User entry")
            .replace(
                "ses_12121212121212121212121212121212",
                &session_id.to_string(),
            );
        entry.into_bytes()
    }

    fn replayed_user_conversation(session_id: SessionId, header: SessionHeader) -> Vec<u8> {
        let entry = replayed_user_entry(session_id, 1);
        let mut bytes = ConversationLineCodec::encode_header(&header)
            .expect("the runtime replay Header encodes");
        bytes.push(b'\n');
        bytes.extend_from_slice(&entry);
        bytes.push(b'\n');
        bytes
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

    #[tokio::test(flavor = "current_thread")]
    async fn public_facade_runs_and_replays_one_scripted_turn() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["scripted public answer"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let capabilities = runtime
            .query(RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities))
            .await
            .expect("the public capability query succeeds");
        let QueryResult::Runtime(RuntimeQueryResult::Capabilities(capabilities)) =
            capabilities.data();
        assert_eq!(
            capabilities.values(),
            [
                RuntimeCapability::StateEvents,
                RuntimeCapability::RuntimeSnapshot,
                RuntimeCapability::SessionSnapshot,
            ]
        );
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;

        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("public Session subscription opens");
        let initial = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the snapshot-first subscription responds")
            .expect("the subscription remains open");
        let EventFrame::Snapshot(SnapshotResponse::Session(initial)) = initial else {
            panic!("the subscription must start with the Session snapshot");
        };
        assert_eq!(initial.session_id(), session_id);
        assert_eq!(initial.execution(), SessionExecutionView::Idle);

        let submit_command_id = CommandId::generate().unwrap();
        let submit = runtime
            .dispatch(CommandRequest::new(
                submit_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("hello public runtime").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("public Submit dispatches");
        let turn_id = started_turn(&submit);

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the scripted Turn reaches a terminal event")
            .expect("the subscription remains open");
        let EventFrame::State(terminal) = terminal else {
            panic!("the second frame must be the terminal StateEvent");
        };
        assert_eq!(terminal.command_id(), Some(submit_command_id));
        assert_eq!(
            terminal.route(),
            EventRoute::Turn {
                session_id,
                turn_id,
            }
        );
        assert_eq!(
            terminal.msg().session_kind(),
            Some(SessionStateEventKind::TurnCompleted)
        );
        assert!(matches!(
            terminal.msg().session_detail(),
            Some(SessionEventDetail::TurnTerminal {
                turn_id: completed_turn,
                terminal: TurnTerminalView::Completed { .. },
            }) if completed_turn == turn_id
        ));
        assert_eq!(
            terminal
                .msg()
                .session_snapshot()
                .expect("terminal events carry a Session snapshot")
                .execution(),
            SessionExecutionView::Idle
        );
        assert_eq!(
            terminal
                .msg()
                .session_snapshot()
                .unwrap()
                .usage()
                .unwrap()
                .model_calls(),
            1
        );
        assert_eq!(model.request_count(), 1);

        let snapshot = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("the completed Session snapshot is available");
        let SnapshotResponse::Session(snapshot) = snapshot else {
            panic!("the Session snapshot request returns a Session snapshot");
        };
        assert_eq!(snapshot.execution(), SessionExecutionView::Idle);
        assert_eq!(snapshot.usage().unwrap().model_calls(), 1);

        let unload = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Unload { session_id }),
            ))
            .await
            .expect("public Unload dispatches");
        assert_eq!(command_output(&unload), "session unloaded");
        let reload = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("public reload dispatches");
        assert_eq!(command_output(&reload), "session loaded");

        let residency = runtime
            .inner
            .residency()
            .expect("residency remains installed");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the reloaded executor is installed");
        assert_eq!(executor.snapshot().await.unwrap().last_terminal(), None);
        assert_eq!(
            executor
                .snapshot()
                .await
                .unwrap()
                .usage()
                .unwrap()
                .model_calls(),
            1
        );
        let live_state = executor
            .live_state_for_test()
            .expect("the reloaded executor retains replayed conversation state");
        assert_eq!(
            super::lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            2
        );
        assert_eq!(model.request_count(), 1);
        drop(live_state);
        drop(executor);
        drop(residency);

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_queue_commands_route_to_typed_session_errors() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["unused"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let intent = PromptIntent::new(
            PromptBodyIntent::Text(TextIntent::new("queued").unwrap()),
            Vec::new(),
        )
        .unwrap();
        let turn_id: TurnId = "trn_33333333333333333333333333333333".parse().unwrap();

        let follow_up = runtime
            .dispatch(CommandRequest::new(
                "cmd_11111111111111111111111111111111".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::FollowUp {
                    session_id,
                    intent: intent.clone(),
                }),
            ))
            .await
            .expect("FollowUp dispatch returns a typed completion");
        assert!(matches!(
            follow_up.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::TurnNotRunning
        ));

        let steer = runtime
            .dispatch(CommandRequest::new(
                "cmd_22222222222222222222222222222222".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Steer {
                    session_id,
                    expected_turn_id: turn_id,
                    intent,
                }),
            ))
            .await
            .expect("Steer dispatch returns a typed completion");
        assert!(matches!(
            steer.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::TurnNotRunning
        ));

        let cancel_queued = runtime
            .dispatch(CommandRequest::new(
                "cmd_44444444444444444444444444444444".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::CancelQueuedMessage {
                    session_id,
                    target_command_id: "cmd_55555555555555555555555555555555".parse().unwrap(),
                }),
            ))
            .await
            .expect("CancelQueuedMessage dispatch returns a typed completion");
        assert!(matches!(
            cancel_queued.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::QueuedMessageNotQueued
        ));

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_queue_commands_accept_active_turn_and_cancel_one_fifo_entry() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["after retry", "after steer"],
        );
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("public Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));

        let submit = runtime
            .dispatch(CommandRequest::new(
                "cmd_11111111111111111111111111111111".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("begin queued route").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("public Submit dispatches");
        let turn_id = started_turn(&submit);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while model.request_count() < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first retry attempt is delivered");

        let intent = PromptIntent::new(
            PromptBodyIntent::Text(TextIntent::new("queued follow-up").unwrap()),
            Vec::new(),
        )
        .unwrap();
        let follow_up_command_id: CommandId =
            "cmd_22222222222222222222222222222222".parse().unwrap();
        let follow_up = runtime
            .dispatch(CommandRequest::new(
                follow_up_command_id,
                RuntimeCommand::Turn(TurnCommand::FollowUp {
                    session_id,
                    intent: intent.clone(),
                }),
            ))
            .await
            .expect("public FollowUp dispatches");
        assert!(matches!(
            follow_up.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::FollowUpQueued,
                output: None,
            }
        ));

        let steer = runtime
            .dispatch(CommandRequest::new(
                "cmd_33333333333333333333333333333333".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Steer {
                    session_id,
                    expected_turn_id: turn_id,
                    intent,
                }),
            ))
            .await
            .expect("public Steer dispatches");
        assert!(matches!(
            steer.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SteerQueued { turn_id: queued_turn_id },
                output: None,
            } if *queued_turn_id == turn_id
        ));

        let queued_snapshot = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("public Session snapshot projects active queues");
        let SnapshotResponse::Session(queued_snapshot) = queued_snapshot else {
            panic!("the public Session snapshot returns a Session view");
        };
        assert_eq!(queued_snapshot.execution(), SessionExecutionView::Running);
        assert_eq!(queued_snapshot.current_turn().unwrap().turn_id(), turn_id);
        assert_eq!(queued_snapshot.active_items().len(), 1);
        assert!(matches!(
            queued_snapshot.active_items()[0].content(),
            ItemContentView::UserMessage { .. }
        ));
        assert!(queued_snapshot.pending_interactions().is_empty());
        assert_eq!(queued_snapshot.queues().submit_admissions(), &[]);
        assert_eq!(
            queued_snapshot.queues().steers()[0].command_id(),
            "cmd_33333333333333333333333333333333".parse().unwrap()
        );
        assert_eq!(
            queued_snapshot.queues().follow_ups()[0].command_id(),
            follow_up_command_id
        );

        let cancelled = runtime
            .dispatch(CommandRequest::new(
                "cmd_44444444444444444444444444444444".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::CancelQueuedMessage {
                    session_id,
                    target_command_id: follow_up_command_id,
                }),
            ))
            .await
            .expect("public CancelQueuedMessage dispatches");
        assert!(matches!(
            cancelled.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::QueuedMessageCancelled,
                output: None,
            }
        ));

        let after_cancel = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("public Session snapshot reflects queue cancellation");
        let SnapshotResponse::Session(after_cancel) = after_cancel else {
            panic!("the public Session snapshot returns a Session view");
        };
        assert!(after_cancel.queues().follow_ups().is_empty());
        assert_eq!(after_cancel.queues().steers().len(), 1);

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(8), events.recv())
            .await
            .expect("the queued Turn reaches a terminal event")
            .expect("the subscription remains open");
        assert!(matches!(
            terminal,
            EventFrame::State(event)
                if event.msg().session_kind() == Some(SessionStateEventKind::TurnCompleted)
        ));
        assert_eq!(model.request_count(), 3);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn context_overflow_fails_after_submit_without_provider_attempt() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::with_context_window_tokens(vec!["must not run"], 1);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the constrained scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("public Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));

        let submit_command_id = CommandId::generate().unwrap();
        let submit = runtime
            .dispatch(CommandRequest::new(
                submit_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("overflow this model").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the live User input admits the Turn");
        let turn_id = started_turn(&submit);

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the overflow Turn reaches terminal state")
            .expect("the subscription remains open");
        let EventFrame::State(terminal) = terminal else {
            panic!("the overflow result is a StateEvent");
        };
        assert_eq!(terminal.command_id(), Some(submit_command_id));
        assert!(matches!(
            terminal.msg().session_detail(),
            Some(SessionEventDetail::TurnTerminal {
                turn_id: failed_turn,
                terminal: TurnTerminalView::Failed {
                    reason: TurnFailureView::ContextOverflow,
                    ..
                },
            }) if failed_turn == turn_id
        ));
        assert_eq!(model.request_count(), 0);

        let residency = runtime
            .inner
            .residency()
            .expect("residency remains installed");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the loaded executor is installed");
        let live_state = executor.live_state_for_test().unwrap();
        {
            let live = super::lock(&live_state);
            assert_eq!(live.current_turn(), None);
            assert_eq!(
                live.capture_conversation_views()
                    .unwrap()
                    .conversation()
                    .messages()
                    .len(),
                1
            );
        }
        drop(live_state);
        drop(executor);
        drop(residency);

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_failure_keeps_one_model_attempt_and_replays_only_recorded_prefix() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["live but unrecorded answer"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let residency = runtime
            .inner
            .residency()
            .expect("residency remains installed");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the loaded executor is installed");
        let recorder = executor
            .recorder_for_test()
            .expect("the loaded executor retains its Recorder");
        let barrier = RecorderWriteBarrier::new();
        barrier.fail_before_write();
        recorder.set_write_barrier_for_test(barrier);
        drop(recorder);
        drop(executor);
        drop(residency);

        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("public Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));
        let submit = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("continue live").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("recording failure does not reject the admitted Turn");
        let turn_id = started_turn(&submit);
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the Turn reaches terminal state")
            .expect("the subscription remains open");
        assert!(matches!(
            terminal,
            EventFrame::State(ref event)
                if matches!(
                    event.msg().session_detail(),
                    Some(SessionEventDetail::TurnTerminal {
                        turn_id: completed_turn,
                        terminal: TurnTerminalView::Completed { .. },
                    }) if completed_turn == turn_id
                )
        ));
        let EventFrame::State(terminal) = terminal else {
            unreachable!();
        };
        let terminal_snapshot = terminal.msg().session_snapshot().unwrap();
        assert_eq!(
            terminal_snapshot.recording().state(),
            SessionRecordingState::Degraded
        );
        assert_eq!(
            terminal_snapshot.diagnostics()[0].code(),
            "session_recording_append_failed"
        );
        assert_eq!(terminal_snapshot.usage().unwrap().model_calls(), 1);
        assert_eq!(model.request_count(), 1);

        let SnapshotResponse::Runtime(runtime_snapshot) = runtime
            .snapshot(SnapshotRequest::Runtime)
            .await
            .expect("the Runtime snapshot remains available")
        else {
            unreachable!();
        };
        assert_eq!(
            runtime_snapshot.loaded_sessions()[0].recording().state(),
            SessionRecordingState::Degraded
        );

        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let live_state = executor.live_state_for_test().unwrap();
        assert_eq!(
            super::lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            2
        );
        drop(live_state);
        drop(executor);
        drop(residency);

        let unload = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Unload { session_id }),
            ))
            .await
            .expect("public Unload dispatches");
        assert_eq!(command_output(&unload), "session unloaded");
        let reload = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("public reload dispatches");
        assert_eq!(command_output(&reload), "session loaded");

        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let live_state = executor.live_state_for_test().unwrap();
        assert_eq!(
            super::lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            0
        );
        assert_eq!(model.request_count(), 1);
        drop(live_state);
        drop(executor);
        drop(residency);

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unload_waits_for_admitted_turn_and_concurrent_submit_is_busy() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["answer before unload"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let recorder = executor.recorder_for_test().unwrap();
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        drop(recorder);
        drop(executor);
        drop(residency);

        let first_command_id = CommandId::generate().unwrap();
        let mut first_submit = Box::pin(
            runtime.dispatch(CommandRequest::new(
                first_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("admitted input").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            )),
        );
        assert!(poll_once_pending(first_submit.as_mut()).await);
        barrier.wait_until_entered().await;
        assert_eq!(model.request_count(), 0);

        let busy = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("must be busy").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the concurrent Submit receives a domain completion");
        assert!(matches!(
            busy.completion(),
            CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::SessionBusy
        ));

        let mut unload = Box::pin(runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Unload { session_id }),
        )));
        assert!(poll_once_pending(unload.as_mut()).await);
        assert_eq!(model.request_count(), 0);

        barrier.release();
        let (first_submit, unload) = tokio::join!(first_submit, unload);
        let first_submit = first_submit.expect("the admitted Submit settles");
        let _turn_id = started_turn(&first_submit);
        let unload = unload.expect("Unload settles after the admitted Turn");
        assert_eq!(command_output(&unload), "session unloaded");
        assert_eq!(model.request_count(), 1);

        let reload = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("public reload dispatches");
        assert_eq!(command_output(&reload), "session loaded");
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let live_state = executor.live_state_for_test().unwrap();
        assert_eq!(
            super::lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            2
        );
        drop(live_state);
        drop(executor);
        drop(residency);

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unload_wins_before_input_apply_without_starting_or_recording_a_turn() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["must not run"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let hooks = executor.test_hooks();
        hooks.arm_after_agent_admission_before_input();
        drop(residency);

        let mut submit = Box::pin(
            runtime.dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("must lose to unload").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            )),
        );
        assert!(poll_once_pending(submit.as_mut()).await);
        hooks.wait_after_agent_admission_before_input().await;

        let mut unload = Box::pin(runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Unload { session_id }),
        )));
        assert!(poll_once_pending(unload.as_mut()).await);
        executor.wait_until_closing_for_test().await;
        hooks.release_after_agent_admission_before_input();

        let (submit, unload) = tokio::join!(submit, unload);
        let submit = submit.expect("the losing Submit receives a domain completion");
        assert!(matches!(
            submit.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::RuntimeClosing
                    && error.retry()
                        == (RetryAdvice::RetryWithBackoff { retry_after: None })
        ));
        let unload = unload.expect("Unload settles after cancelling admission");
        assert_eq!(command_output(&unload), "session unloaded");
        assert_eq!(model.request_count(), 0);
        drop(executor);

        let reload = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("public reload dispatches");
        assert_eq!(command_output(&reload), "session loaded");
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let live_state = executor.live_state_for_test().unwrap();
        assert_eq!(
            super::lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            0
        );
        drop(live_state);
        drop(executor);
        drop(residency);

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn user_cancel_before_input_completes_submit_cancelled() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["must not run"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let hooks = executor.test_hooks();
        hooks.arm_after_agent_admission_before_input();
        drop(residency);

        let submit_command_id: CommandId = "cmd_77777777777777777777777777777777".parse().unwrap();
        let mut submit = Box::pin(
            runtime.dispatch(CommandRequest::new(
                submit_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancel before input").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            )),
        );
        assert!(poll_once_pending(submit.as_mut()).await);
        hooks.wait_after_agent_admission_before_input().await;

        let cancel = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Cancel {
                    session_id,
                    target: PublicCancelTarget::Submit(submit_command_id),
                }),
            ))
            .await
            .expect("Cancel dispatches while Submit is Starting");
        assert!(matches!(
            cancel.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::CancelAccepted {
                    target: PublicCancelTarget::Submit(cancelled_command),
                    ..
                },
                output: None,
            } if *cancelled_command == submit_command_id
        ));
        hooks.release_after_agent_admission_before_input();

        let submit = submit.await.expect("Submit settles after cancellation");
        assert!(matches!(
            submit.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SubmitCancelled,
                output: None,
            }
        ));
        assert_eq!(model.request_count(), 0);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_duplicate_in_flight_submit_joins_and_conflict_is_typed() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["one shared model attempt"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let hooks = executor.test_hooks();
        hooks.arm_after_agent_admission_before_input();
        drop(executor);
        drop(residency);

        let command_id: CommandId = "cmd_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap();
        let intent = PromptIntent::new(
            PromptBodyIntent::Text(TextIntent::new("shared public submit").unwrap()),
            Vec::new(),
        )
        .unwrap();
        let mut first = Box::pin(runtime.dispatch(CommandRequest::new(
            command_id,
            RuntimeCommand::Turn(TurnCommand::Submit {
                session_id,
                intent: intent.clone(),
            }),
        )));
        assert!(poll_once_pending(first.as_mut()).await);
        hooks.wait_after_agent_admission_before_input().await;

        let mut duplicate = Box::pin(runtime.dispatch(CommandRequest::new(
            command_id,
            RuntimeCommand::Turn(TurnCommand::Submit {
                session_id,
                intent: intent.clone(),
            }),
        )));
        assert!(poll_once_pending(duplicate.as_mut()).await);
        let conflict = runtime
            .dispatch(CommandRequest::new(
                command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("different public submit").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("conflicting duplicate dispatches");
        assert!(matches!(
            conflict.completion(),
            CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::CommandConflict
        ));
        let cross_command_conflict = runtime
            .dispatch(CommandRequest::new(
                command_id,
                RuntimeCommand::Turn(TurnCommand::Cancel {
                    session_id,
                    target: PublicCancelTarget::Submit(command_id),
                }),
            ))
            .await
            .expect("a cross-command duplicate dispatches");
        assert!(matches!(
            cross_command_conflict.completion(),
            CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::CommandConflict
        ));

        hooks.release_after_agent_admission_before_input();
        let first = first.await.expect("first Submit settles");
        let duplicate = duplicate.await.expect("duplicate Submit settles");
        let first_turn = started_turn(&first);
        assert_eq!(started_turn(&duplicate), first_turn);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_cancel_returns_idempotent_epoch_and_finishing_snapshot() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["must not run"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let hooks = executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        drop(executor);
        drop(residency);

        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("public Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));
        let submit = runtime
            .dispatch(CommandRequest::new(
                "cmd_88888888888888888888888888888888".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancel running").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("public Submit dispatches");
        let turn_id = started_turn(&submit);
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        hooks.wait_before_agent_run_attempt().await;
        drop(executor);
        drop(residency);

        let cancel = runtime
            .dispatch(CommandRequest::new(
                "cmd_99999999999999999999999999999999".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Cancel {
                    session_id,
                    target: PublicCancelTarget::Turn(turn_id),
                }),
            ))
            .await
            .expect("public Cancel dispatches");
        let cancel_epoch = match cancel.completion() {
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::CancelAccepted {
                        target: PublicCancelTarget::Turn(cancelled_turn),
                        cancel_epoch,
                    },
                output: None,
            } if *cancelled_turn == turn_id => *cancel_epoch,
            completion => panic!("unexpected Cancel completion: {completion:?}"),
        };

        let SnapshotResponse::Session(snapshot) = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("Finishing snapshot is available")
        else {
            panic!("the public Session snapshot returns a Session view");
        };
        assert_eq!(snapshot.execution(), SessionExecutionView::Finishing);

        let duplicate = runtime
            .dispatch(CommandRequest::new(
                "cmd_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Cancel {
                    session_id,
                    target: PublicCancelTarget::Turn(turn_id),
                }),
            ))
            .await
            .expect("duplicate Cancel dispatches");
        assert!(matches!(
            duplicate.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::CancelAccepted {
                    target: PublicCancelTarget::Turn(cancelled_turn),
                    cancel_epoch: duplicate_epoch,
                },
                output: None,
            } if *cancelled_turn == turn_id && *duplicate_epoch == cancel_epoch
        ));

        hooks.release_before_agent_run_attempt();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the cancelled Turn reaches terminal state")
            .expect("the subscription remains open");
        assert!(matches!(
            terminal,
            EventFrame::State(event)
                if event.msg().session_kind() == Some(SessionStateEventKind::TurnInterrupted)
        ));
        assert_eq!(model.request_count(), 0);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_snapshot_does_not_wait_for_a_loaded_session_actor() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the Runtime opens");
        let session_id = create_runtime_session(&runtime, workspace.path()).await;
        runtime
            .inner
            .load_session_ready_idle(session_id)
            .await
            .expect("the Session loads");
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(session_id).unwrap();
        let hooks = executor.test_hooks();
        hooks.arm_before_snapshot_response();

        let mut blocked_session_snapshot = Box::pin(executor.snapshot());
        assert!(poll_once_pending(blocked_session_snapshot.as_mut()).await);
        hooks.wait_before_snapshot_response().await;

        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            runtime.snapshot(SnapshotRequest::Runtime),
        )
        .await
        .expect("Runtime Snapshot does not wait for a Session actor")
        .expect("Runtime Snapshot remains available");
        let SnapshotResponse::Runtime(snapshot) = snapshot else {
            panic!("the Runtime snapshot request returns a Runtime snapshot");
        };
        assert_eq!(snapshot.loaded_sessions().len(), 1);
        assert_eq!(snapshot.loaded_sessions()[0].session_id(), session_id);
        assert_eq!(
            snapshot.loaded_sessions()[0].execution(),
            SessionExecutionView::Idle
        );

        hooks.release_before_snapshot_response();
        blocked_session_snapshot
            .await
            .expect("the blocked Session snapshot settles after release");
        drop(executor);
        drop(residency);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_owns_load_unload_and_lifecycle_residency() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
        let (prompt_service, prompt_resources) = runtime.inner.prompt_resources();
        assert_eq!(prompt_resources.definition_count(), 0);
        assert_eq!(
            prompt_service
                .build_reload_candidate()
                .await
                .expect("the empty shared Prompt candidate rebuilds")
                .definition_count(),
            0
        );
        let (model_gateway, model_catalog) = runtime.inner.model_resources();
        assert_eq!(model_catalog.definition_count(), 0);
        assert_eq!(
            model_gateway
                .build_reload_candidate()
                .await
                .expect("the empty Model catalog candidate rebuilds")
                .definition_count(),
            0
        );
        let session_id = create_runtime_session(&runtime, workspace.path()).await;

        assert_eq!(
            runtime.inner.load_session_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        assert_eq!(
            runtime
                .inner
                .loaded_session_snapshot(session_id)
                .await
                .unwrap()
                .execution_state(),
            SessionExecutionState::Idle
        );
        assert!(matches!(
            runtime
                .inner
                .update_session_lifecycle(SealedSessionLifecycleAttempt::unarchive(session_id))
                .await,
            Ok(crate::durable_state::DurableSessionLifecycleOutcome::NoChange(_))
        ));
        assert!(matches!(
            runtime
                .inner
                .update_session_lifecycle(SealedSessionLifecycleAttempt::archive(session_id))
                .await,
            Err(SessionResidencyLifecycleError::SessionBusy)
        ));
        assert_eq!(
            runtime.inner.unload_session(session_id).await,
            Ok(SessionResidencyUnloadOutcome::Unloaded)
        );
        assert!(matches!(
            runtime
                .inner
                .update_session_lifecycle(SealedSessionLifecycleAttempt::archive(session_id))
                .await,
            Ok(crate::durable_state::DurableSessionLifecycleOutcome::Updated(_))
        ));
        assert_eq!(
            runtime.inner.load_session_ready_idle(session_id).await,
            Err(SessionResidencyLoadError::SessionArchived)
        );
        assert!(matches!(
            runtime
                .inner
                .update_session_lifecycle(SealedSessionLifecycleAttempt::unarchive(session_id))
                .await,
            Ok(crate::durable_state::DurableSessionLifecycleOutcome::Updated(_))
        ));
        assert_eq!(
            runtime.inner.load_session_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );

        runtime.shutdown().await;
        assert!(super::lock(&runtime.inner.session_residency).is_none());
        assert!(super::lock(&runtime.inner.durable_state).is_none());

        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("shutdown releases the root after unloading the Session");
        reopened.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_drains_loaded_recorder_before_releasing_root_lease() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
        let session_id = create_runtime_session(&runtime, workspace.path()).await;

        let durable_state = super::lock(&runtime.inner.durable_state)
            .as_ref()
            .cloned()
            .expect("the open Runtime retains DurableState");
        let current = durable_state
            .session_current(session_id)
            .expect("the created Session is catalogued");
        let header = SessionHeader::reconstruct(
            1,
            session_id,
            current.head().created_at(),
            current.definition().agent(),
            current.definition().revision(),
        );
        drop(durable_state);

        let conversation_path = root
            .path()
            .join("sessions")
            .join(session_id.to_string())
            .join("conversation.jsonl");
        let recorded = replayed_user_conversation(session_id, header);
        fs::write(&conversation_path, &recorded).expect("the replay fixture is installed");

        assert_eq!(
            runtime.inner.load_session_ready_idle(session_id).await,
            Ok(SessionResidencyLoadOutcome::Loaded)
        );
        let residency = runtime.inner.residency().expect("residency is installed");
        let recorder = residency
            .executor_for_test(session_id)
            .expect("the loaded executor is installed")
            .recorder_for_test()
            .expect("the loaded executor retains its Recorder");
        let barrier = RecorderWriteBarrier::new();
        barrier.hold_after_write();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));

        let entry_line = replayed_user_entry(session_id, 2);
        let entry = ConversationLineCodec::decode_entry_for_session(&entry_line, session_id)
            .expect("the production codec decodes the replay entry");
        let mut append = Box::pin(recorder.record(Arc::new(entry)));
        assert!(poll_once_pending(append.as_mut()).await);
        barrier.release();
        barrier.wait_until_written().await;

        // Runtime shutdown must drain residency, including the Recorder's exact tracked job,
        // before it closes DurableState and releases the Store root lease.
        let mut shutdown = Box::pin(runtime.shutdown());
        assert!(poll_once_pending(shutdown.as_mut()).await);
        assert!(matches!(
            MiniCoreRuntime::open(
                MiniCoreRuntimeConfig::new(root.path().to_owned()),
                Handle::current(),
            )
            .await,
            Err(RuntimeInitializationError::StoreInUse)
        ));

        barrier.release_after_write();
        assert_eq!(append.await, RecordOutcome::Written);
        shutdown.await;
        assert!(super::lock(&runtime.inner.session_residency).is_none());
        assert!(super::lock(&runtime.inner.durable_state).is_none());

        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the drained shutdown releases the root lease");
        reopened.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_shutdown_retains_a_blocked_residency_owner_for_the_next_leader() {
        let root = TempRoot::new();
        let old_workspace = TempWorkspace::new();
        let new_workspace = TempWorkspace::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
        let session_id = create_runtime_session(&runtime, old_workspace.path()).await;
        runtime
            .inner
            .load_session_ready_idle(session_id)
            .await
            .expect("the Session loads");
        let residency = runtime.inner.residency().expect("residency is installed");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the loaded executor is installed");
        let hooks = executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let current = runtime
            .inner
            .loaded_session_snapshot(session_id)
            .await
            .unwrap();
        let mut update = Box::pin(runtime.inner.update_session_workspace_definition(
            session_id,
            current.definition_revision(),
            changed_workspace(new_workspace.path()),
            "2026-08-03T10:02:00.000Z".parse().unwrap(),
        ));
        tokio::select! {
            _ = hooks.wait_after_candidate_snapshot_finish_before_durable() => {}
            result = &mut update => panic!("publication settled before the named barrier: {result:?}"),
        }

        let mut first = Box::pin(runtime.shutdown());
        assert!(poll_once_pending(first.as_mut()).await);
        drop(first);
        assert!(matches!(
            *super::lock(&runtime.inner.lifecycle),
            RuntimeLifecycle::Closing {
                shutdown_active: false
            }
        ));
        assert!(super::lock(&runtime.inner.session_residency).is_some());
        assert!(super::lock(&runtime.inner.durable_state).is_some());

        let mut second = Box::pin(runtime.shutdown());
        assert!(poll_once_pending(second.as_mut()).await);
        hooks.release_after_candidate_snapshot_finish_before_durable();
        second.await;
        assert!(
            update
                .await
                .expect("the admitted publication settles")
                .changed()
        );
        assert!(super::lock(&runtime.inner.session_residency).is_none());
        assert!(super::lock(&runtime.inner.durable_state).is_none());

        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the later shutdown leader releases the root");
        let durable_state = super::lock(&reopened.inner.durable_state)
            .as_ref()
            .cloned()
            .expect("the reopened Runtime retains DurableState");
        assert_eq!(
            durable_state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            2
        );
        reopened.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_drop_requests_closing_but_a_remaining_facade_can_settle_and_release() {
        let root = TempRoot::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
        let remaining_facade = MiniCoreRuntime {
            inner: std::sync::Arc::clone(&runtime.inner),
        };

        drop(runtime);

        assert_eq!(
            remaining_facade
                .inner
                .task_context
                .spawn_tracked(async {})
                .expect_err("facade drop closes task admission"),
            RuntimeTaskError::OwnerClosing
        );
        assert!(matches!(
            MiniCoreRuntime::open(
                MiniCoreRuntimeConfig::new(root.path().to_owned()),
                Handle::current(),
            )
            .await,
            Err(RuntimeInitializationError::StoreInUse)
        ));

        remaining_facade.shutdown().await;

        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("an explicit shutdown after facade drop releases the lease");
        reopened.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_shutdown_keeps_the_lease_and_lets_a_later_leader_finish_and_reopen() {
        let root = TempRoot::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered_by_task = Arc::clone(&entered);
        let release_by_task = Arc::clone(&release);
        let task = runtime
            .inner
            .task_context
            .spawn_tracked(async move {
                entered_by_task.notify_one();
                release_by_task.notified().await;
            })
            .expect("the open runtime admits owner-retained work");
        entered.notified().await;

        let mut first = Box::pin(runtime.shutdown());
        assert!(poll_once_pending(first.as_mut()).await);
        drop(first);

        assert!(matches!(
            *super::lock(&runtime.inner.lifecycle),
            RuntimeLifecycle::Closing {
                shutdown_active: false
            }
        ));
        assert!(super::lock(&runtime.inner.durable_state).is_some());
        assert!(matches!(
            MiniCoreRuntime::open(
                MiniCoreRuntimeConfig::new(root.path().to_owned()),
                Handle::current(),
            )
            .await,
            Err(RuntimeInitializationError::StoreInUse)
        ));

        let mut second = Box::pin(runtime.shutdown());
        assert!(poll_once_pending(second.as_mut()).await);
        release.notify_one();
        second.await;
        assert_eq!(task.wait().await, Ok(()));

        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the second shutdown leader releases the retained lease");
        reopened.shutdown().await;
    }
}
