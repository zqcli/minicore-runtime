use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration as StdDuration, Instant};

use tokio::runtime::Handle;
use tokio::sync::{
    Notify, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore, broadcast,
};

use crate::agent_session_lifecycle::{
    AgentRevisionRef, AgentStatus, SealedAgentCreateAttempt, SealedAgentDefinitionAttempt,
    SealedAgentMetadataAttempt, SealedAgentStatusAttempt, SealedSessionCreateAttempt,
    SealedSessionLifecycleAttempt, SealedSessionMetadataAttempt, SessionLifecycle,
};
use crate::compaction::CompactionSettings;
use crate::durable_state::{
    DurableAgentCreateError, DurableAgentDefinitionError, DurableAgentDefinitionOutcome,
    DurableAgentHead, DurableAgentMetadataError, DurableAgentMetadataOutcome,
    DurableAgentStatusError, DurableAgentStatusOutcome, DurableOpenError,
    DurableSessionCreateError, DurableSessionHead, DurableState,
};
use crate::model_gateway::{
    ModelCatalogView, ModelGateway, ModelProviderConfig, ModelResolutionErrorKind,
    ModelSourceAdapter, ProviderSourceBuildError,
};
use crate::prompt::{PromptResourceView, PromptService};
use crate::runtime_interface::{
    AgentCommand, AgentMetadataView, AgentQuery, AgentQueryResult, AgentSummary, CommandCompletion,
    CommandError, CommandErrorCode, CommandOutcome, CommandOutput, CommandRequest, CommandResponse,
    EventFrame, InteractionCommand, LoadedSessionSummary, Page, PublicCancelTarget, PublicSubject,
    QueryError, QueryErrorCode, QueryResponse, QueryResult, QueuedFollowUpView, QueuedSteerView,
    RetryAdvice, RuntimeCapabilities, RuntimeCommand, RuntimeDispatchError,
    RuntimeLifecycleCommand, RuntimeQuery, RuntimeQueryResult, RuntimeReadQuery, RuntimeSnapshot,
    RuntimeStateEventKind, RuntimeStatusView, RuntimeView, SessionCommand,
    SessionDefinitionSummary, SessionExecutionView, SessionForkProvenanceView,
    SessionLifecycleView, SessionMetadataView, SessionQuery, SessionQueryResult, SessionQueueView,
    SessionReadinessView, SessionRecordingView, SessionSnapshot, SessionSummary,
    SessionUnavailableView, SessionWorkspaceInvalidationError, SnapshotError, SnapshotErrorCode,
    SnapshotRequest, SnapshotResponse, StateEvent, SubmitAdmissionStateView, SubmitAdmissionView,
    SubscriptionError, SubscriptionErrorCode, SubscriptionRequest, SubscriptionScope, TurnCommand,
    TurnFailureView, TurnInterruptionView,
};
use crate::runtime_task::{Clock, RuntimeTaskContext, SystemClock};
use crate::session_execution::{
    SessionCancelTarget, SessionExecutionState, SessionExecutorEvent, SessionExecutorSnapshot,
    SessionExecutorSubscription, SessionTurnFailure, SessionTurnInterruption, SessionTurnTerminal,
};
use crate::session_residency::{
    SessionResidencyAgentAvailabilityError, SessionResidencyAgentUpgradeError,
    SessionResidencyCancelError, SessionResidencyFollowUpError, SessionResidencyForkError,
    SessionResidencyInteractionError, SessionResidencyLifecycleError, SessionResidencyLoadError,
    SessionResidencyLoadOutcome, SessionResidencyMetadataError, SessionResidencyQueuedMessageError,
    SessionResidencyRegistry, SessionResidencySecurityInvalidationError,
    SessionResidencySharedResourcesError, SessionResidencySnapshotError,
    SessionResidencyStartError, SessionResidencySteerError, SessionResidencySubmitError,
    SessionResidencySubscriptionError, SessionResidencyUnloadError, SessionResidencyUnloadOutcome,
    SessionResidencyWorkspaceDefinitionError, SessionResidencyWorkspaceReloadError,
};
use crate::wire::{
    PageCursor, ProtocolLimits, SessionDefinitionRevision, SessionId, SessionMetadataRevision,
    Timestamp, WorkspaceRevision,
};
use crate::workspace::{
    Workspace, WorkspaceDefinitionSummaryView, WorkspacePathTarget, WorkspaceReadAccessControl,
    WorkspaceResolver, WorkspaceRootSummaryView, lower_workspace,
};

const DEFAULT_RUNTIME_REQUIRED_POLICY: &str = "Respond helpfully to the user's request.";
const PAGE_CURSOR_CAPACITY: usize = 4_096;
const PAGE_CURSOR_TTL: StdDuration = StdDuration::from_secs(15 * 60);
const PAGE_CURSOR_GENERATION_ATTEMPTS: usize = 32;
const RUNTIME_EVENT_CAPACITY: usize = 32;

/// The default graceful-Unload grace: active Turns settle naturally within this window before
/// the executor fails-closed.
const DEFAULT_UNLOAD_GRACE: StdDuration = StdDuration::from_secs(30);
/// The validated upper bound for `unload_grace`.
const MAX_UNLOAD_GRACE: StdDuration = StdDuration::from_secs(5 * 60);

/// Host configuration for a MiniCore runtime instance.
#[non_exhaustive]
pub struct MiniCoreRuntimeConfig {
    durable_root: PathBuf,
    compaction: CompactionSettings,
    unload_grace: StdDuration,
    model_providers: Vec<ModelProviderConfig>,
    ask_user_tool: bool,
    read_file_tool: bool,
}

impl MiniCoreRuntimeConfig {
    pub fn new(durable_root: PathBuf) -> Self {
        Self {
            durable_root,
            compaction: CompactionSettings::default(),
            unload_grace: DEFAULT_UNLOAD_GRACE,
            model_providers: Vec::new(),
            ask_user_tool: false,
            read_file_tool: false,
        }
    }

    pub fn with_compaction_settings(mut self, compaction: CompactionSettings) -> Self {
        self.compaction = compaction;
        self
    }

    /// Adds one validated host-only provider installation. `open` materializes
    /// each installation's single direct adapter/client and static model source
    /// into the Runtime-owned Model catalog; with no installations the catalog
    /// stays empty exactly as before. A client/adapter build failure fails `open`
    /// with `RuntimeInitializationError::RuntimeDependencyUnavailable`; duplicate
    /// stable model selections across installations (or invalid definitions) fail
    /// `open` with `RuntimeInitializationError::InvalidConfiguration`.
    pub fn with_model_provider(mut self, provider: ModelProviderConfig) -> Self {
        self.model_providers.push(provider);
        self
    }

    /// Sets the graceful-Unload grace period.  `open` validates its finite semantics: it must be
    /// non-zero and at most 5 minutes, otherwise initialization fails with
    /// `RuntimeInitializationError::InvalidConfiguration`.
    pub fn with_unload_grace(mut self, unload_grace: StdDuration) -> Self {
        self.unload_grace = unload_grace;
        self
    }

    /// Opts the Runtime into the closed production `ask_user` builtin ToolSet.
    ///
    /// The default Runtime ToolSet stays empty; this opt-in is idempotent and `open` freezes
    /// it into the single production Tool config passed through the residency start.
    pub fn with_ask_user_tool(mut self) -> Self {
        self.ask_user_tool = true;
        self
    }

    /// Opts the Runtime into the closed production `read_file` builtin ToolSet and the
    /// read-only Workspace resolution authority.
    ///
    /// The default Runtime ToolSet stays empty; this opt-in is idempotent and independent of
    /// `with_ask_user_tool`.  `open` freezes it into the single production Tool config and
    /// selects the read-only Workspace resolver, so every declared root is resolved with an
    /// authority ceiling of exactly `ReadOnly` filesystem access — never `ReadWrite` — and
    /// Prompt/Skill source ceilings stay false: the opt-in is both the Tool installation and
    /// the read-only authority ceiling.  The read_file ToolSet is materialized per admission
    /// against the exact captured Workspace snapshot, so no single static ToolSet is selected
    /// here.
    pub fn with_read_file_tool(mut self) -> Self {
        self.read_file_tool = true;
        self
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
    InvalidConfiguration,
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
            Self::InvalidConfiguration => "InvalidConfiguration",
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
            Self::InvalidConfiguration => "runtime configuration is invalid",
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
        let compaction = config
            .compaction
            .validate()
            .map_err(|_| RuntimeInitializationError::InvalidConfiguration)?;
        // The graceful-Unload grace must be finite and bounded: non-zero and at most 5 minutes.
        // `std::time::Duration` itself is finite, so the finite check is the zero/upper-bound
        // validation.
        if config.unload_grace.is_zero() || config.unload_grace > MAX_UNLOAD_GRACE {
            return Err(RuntimeInitializationError::InvalidConfiguration);
        }
        let unload_grace = config.unload_grace;
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
                // The private test fixture path never reads provider config; the host
                // installation path materializes one static source per validated
                // provider config at `open` (with none, the catalog stays empty
                // exactly as before). Each installation builds exactly one direct
                // adapter/client here; shared-resource reload reuses the installed
                // sources and never rebuilds a client. A client/adapter build
                // failure is a runtime dependency failure; only invalid/duplicate
                // definitions are configuration errors.
                let mut sources: Vec<Arc<dyn ModelSourceAdapter>> = Vec::new();
                for provider in &config.model_providers {
                    let source = match provider.build_source() {
                        Ok(source) => source,
                        Err(error) => {
                            task_context.shutdown().await;
                            return Err(match error {
                                ProviderSourceBuildError::ClientBuild => {
                                    RuntimeInitializationError::RuntimeDependencyUnavailable
                                }
                                ProviderSourceBuildError::InvalidDefinition => {
                                    RuntimeInitializationError::InvalidConfiguration
                                }
                            });
                        }
                    };
                    sources.push(source);
                }
                let model_gateway = Arc::new(ModelGateway::new(sources));
                let model_catalog = match model_gateway.initialize().await {
                    Ok(catalog) => catalog,
                    Err(error) => {
                        task_context.shutdown().await;
                        return Err(match error.kind() {
                            // Invalid definitions and duplicate stable selections
                            // across installations are host configuration errors.
                            ModelResolutionErrorKind::InvalidDefinition => {
                                RuntimeInitializationError::InvalidConfiguration
                            }
                            _ => RuntimeInitializationError::RuntimeDependencyUnavailable,
                        });
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

        // The read_file opt-in selects the read-only Workspace resolver and its revocation
        // control: every declared root resolves with an authority ceiling of exactly
        // `ReadOnly` filesystem access (never `ReadWrite`), Prompt/Skill source ceilings stay
        // false, and the Runtime owner keeps the control so the host Workspace authority
        // invalidation seam can revoke read access per Session.  The default resolver keeps
        // the existing restricted authority unchanged and has no control.
        let (resolver, read_access_control) = if config.read_file_tool {
            let (resolver, control) = WorkspaceResolver::new_with_read_access(task_context.clone());
            (Arc::new(resolver), Some(control))
        } else {
            (Arc::new(WorkspaceResolver::new(task_context.clone())), None)
        };
        // The production Tool config is frozen exactly once at `open`: the two closed host
        // opt-ins (ask_user, read_file) are captured in one immutable config and passed
        // through the residency start.  Nothing is materialized here — each admission
        // materializes its ToolSet against the exact captured Workspace snapshot, so no
        // single static ToolSet is selected when read_file is enabled and the default
        // Runtime ToolSet stays empty.
        let tool_config =
            crate::tools::ProductionToolConfig::new(config.ask_user_tool, config.read_file_tool);
        let session_residency = match SessionResidencyRegistry::start_with_turn_resources_and_production_tools_and_compaction_and_unload_grace(
            task_context.clone(),
            durable_state.clone(),
            resolver,
            Arc::clone(&prompt_service),
            Arc::clone(&prompt_resources),
            Arc::clone(&model_gateway),
            Arc::clone(&model_catalog),
            tool_config,
            compaction,
            unload_grace,
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
            SharedResourceRoots::new(prompt_resources, model_catalog),
            model_gateway,
            unload_grace,
            read_access_control,
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

    /// Host-only security Workspace authority invalidation (not a wire command).  Except for
    /// the Runtime-owned read_file authority, the host has already published the current hard
    /// restriction fact; the Runtime routes out-of-band to the loaded Session executor —
    /// without the runtime publication semaphore and without waiting on any ordinary work
    /// lane — samples one `SystemClock` timestamp, signals the active admission/Turn
    /// first-wins with `SecurityRevoked` (or enters Preparing directly when Idle), and
    /// re-resolves the installed Workspace with the exact current definition.  For a Runtime
    /// opened with the read_file opt-in, this method itself first publishes the permanent
    /// read revocation for the Session through the owner-held control, so the read authority
    /// is already restricted when the recovery re-resolves — the restriction stays current
    /// even if the residency reports SessionNotLoaded/Closing/internal.  The await resolves
    /// only after the recovery final state is installed (Ready or the new Workspace/Prompt
    /// Unavailable cause).  The durable definition/revision/metadata/conversation are never
    /// changed.  No `CommandId` is generated.
    pub async fn invalidate_session_workspace_authority(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionWorkspaceInvalidationError> {
        self.inner
            .invalidate_session_workspace_authority(session_id)
            .await
    }
}

pub struct EventStream {
    runtime: Arc<RuntimeInner>,
    initial: Option<EventFrame>,
    subscription: EventSubscription,
}

enum EventSubscription {
    Runtime(broadcast::Receiver<Arc<StateEvent>>),
    Session(SessionExecutorSubscription),
}

impl EventStream {
    pub async fn recv(&mut self) -> Option<EventFrame> {
        if let Some(initial) = self.initial.take() {
            return Some(initial);
        }
        match &mut self.subscription {
            EventSubscription::Runtime(receiver) => receiver
                .recv()
                .await
                .ok()
                .map(|event| EventFrame::State(event.as_ref().clone())),
            EventSubscription::Session(subscription) => {
                let event = subscription.recv().await?;
                let snapshot = self
                    .runtime
                    .public_session_snapshot(Arc::clone(event.snapshot()))
                    .ok()?;
                let state = match event.as_ref() {
                    SessionExecutorEvent::ExecutionChanged { timestamp, .. } => {
                        StateEvent::session_execution_changed(*timestamp, None, snapshot)
                    }
                    SessionExecutorEvent::DefinitionUpdated {
                        timestamp,
                        command_id,
                        ..
                    } => StateEvent::session_definition_changed(
                        *timestamp,
                        Some(*command_id),
                        snapshot,
                    ),
                    SessionExecutorEvent::MetadataUpdated {
                        timestamp,
                        command_id,
                        ..
                    } => StateEvent::session_metadata_changed(
                        *timestamp,
                        Some(*command_id),
                        snapshot,
                    ),
                    SessionExecutorEvent::WorkspaceReloaded {
                        timestamp,
                        command_id,
                        ..
                    } => StateEvent::session_workspace_reloaded(
                        *timestamp,
                        Some(*command_id),
                        snapshot,
                    ),
                    SessionExecutorEvent::ReadinessChanged {
                        timestamp,
                        command_id,
                        ..
                    } => StateEvent::session_readiness_changed(*timestamp, *command_id, snapshot),
                    SessionExecutorEvent::TurnTerminal {
                        timestamp,
                        command_id,
                        turn_id,
                        terminal,
                        ..
                    } => match terminal {
                        SessionTurnTerminal::Completed => StateEvent::turn_completed(
                            *timestamp,
                            Some(*command_id),
                            snapshot,
                            *turn_id,
                            *timestamp,
                        ),
                        SessionTurnTerminal::Failed(failure) => StateEvent::turn_failed(
                            *timestamp,
                            Some(*command_id),
                            snapshot,
                            *turn_id,
                            *timestamp,
                            public_turn_failure(*failure),
                        ),
                        SessionTurnTerminal::Interrupted(interruption) => {
                            StateEvent::turn_interrupted(
                                *timestamp,
                                Some(*command_id),
                                snapshot,
                                *turn_id,
                                *timestamp,
                                public_turn_interruption(*interruption),
                            )
                        }
                    },
                };
                Some(EventFrame::State(state))
            }
        }
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

#[derive(Clone)]
struct AgentPageCursorEntry {
    snapshot: Arc<[AgentSummary]>,
    offset: usize,
    include_deleted: bool,
    expires_at: Instant,
}

#[derive(Clone)]
struct SessionPageCursorEntry {
    snapshot: Arc<[SessionSummary]>,
    offset: usize,
    include_archived: bool,
    expires_at: Instant,
}

#[derive(Clone)]
enum PageCursorEntry {
    Agents(AgentPageCursorEntry),
    Sessions(SessionPageCursorEntry),
}

impl PageCursorEntry {
    const fn expires_at(&self) -> Instant {
        match self {
            Self::Agents(entry) => entry.expires_at,
            Self::Sessions(entry) => entry.expires_at,
        }
    }
}

#[derive(Default)]
struct PageCursorStore {
    entries: BTreeMap<PageCursor, PageCursorEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageCursorStoreError {
    Stale,
    Unavailable,
}

impl PageCursorStore {
    fn first_agents(
        &mut self,
        snapshot: Vec<AgentSummary>,
        include_deleted: bool,
        limit: usize,
    ) -> Result<Page<AgentSummary>, PageCursorStoreError> {
        let snapshot: Arc<[AgentSummary]> = snapshot.into();
        self.agent_page(snapshot, 0, include_deleted, limit)
    }

    fn next_agents(
        &mut self,
        cursor: PageCursor,
        include_deleted: bool,
        limit: usize,
    ) -> Result<Page<AgentSummary>, PageCursorStoreError> {
        self.remove_expired();
        let Some(PageCursorEntry::Agents(entry)) = self.entries.get(&cursor).cloned() else {
            return Err(PageCursorStoreError::Stale);
        };
        if entry.include_deleted != include_deleted {
            return Err(PageCursorStoreError::Stale);
        }
        let page = self.agent_page(entry.snapshot, entry.offset, include_deleted, limit)?;
        self.entries.remove(&cursor);
        Ok(page)
    }

    fn agent_page(
        &mut self,
        snapshot: Arc<[AgentSummary]>,
        offset: usize,
        include_deleted: bool,
        limit: usize,
    ) -> Result<Page<AgentSummary>, PageCursorStoreError> {
        let end = offset.saturating_add(limit).min(snapshot.len());
        if offset > end {
            return Err(PageCursorStoreError::Stale);
        }
        let items = snapshot[offset..end].to_vec();
        let next_cursor = if end < snapshot.len() {
            Some(
                self.insert_cursor(PageCursorEntry::Agents(AgentPageCursorEntry {
                    snapshot,
                    offset: end,
                    include_deleted,
                    expires_at: Instant::now() + PAGE_CURSOR_TTL,
                }))?,
            )
        } else {
            None
        };
        Ok(Page::new(items, next_cursor))
    }

    fn first_sessions(
        &mut self,
        snapshot: Vec<SessionSummary>,
        include_archived: bool,
        limit: usize,
    ) -> Result<Page<SessionSummary>, PageCursorStoreError> {
        let snapshot: Arc<[SessionSummary]> = snapshot.into();
        self.session_page(snapshot, 0, include_archived, limit)
    }

    fn next_sessions(
        &mut self,
        cursor: PageCursor,
        include_archived: bool,
        limit: usize,
    ) -> Result<Page<SessionSummary>, PageCursorStoreError> {
        self.remove_expired();
        let Some(PageCursorEntry::Sessions(entry)) = self.entries.get(&cursor).cloned() else {
            return Err(PageCursorStoreError::Stale);
        };
        if entry.include_archived != include_archived {
            return Err(PageCursorStoreError::Stale);
        }
        let page = self.session_page(entry.snapshot, entry.offset, include_archived, limit)?;
        self.entries.remove(&cursor);
        Ok(page)
    }

    fn session_page(
        &mut self,
        snapshot: Arc<[SessionSummary]>,
        offset: usize,
        include_archived: bool,
        limit: usize,
    ) -> Result<Page<SessionSummary>, PageCursorStoreError> {
        let end = offset.saturating_add(limit).min(snapshot.len());
        if offset > end {
            return Err(PageCursorStoreError::Stale);
        }
        let items = snapshot[offset..end].to_vec();
        let next_cursor = if end < snapshot.len() {
            Some(
                self.insert_cursor(PageCursorEntry::Sessions(SessionPageCursorEntry {
                    snapshot,
                    offset: end,
                    include_archived,
                    expires_at: Instant::now() + PAGE_CURSOR_TTL,
                }))?,
            )
        } else {
            None
        };
        Ok(Page::new(items, next_cursor))
    }

    fn insert_cursor(
        &mut self,
        entry: PageCursorEntry,
    ) -> Result<PageCursor, PageCursorStoreError> {
        self.remove_expired();
        let mut candidate = None;
        for _ in 0..PAGE_CURSOR_GENERATION_ATTEMPTS {
            let cursor = PageCursor::generate().map_err(|_| PageCursorStoreError::Unavailable)?;
            if !self.entries.contains_key(&cursor) {
                candidate = Some(cursor);
                break;
            }
        }
        let cursor = candidate.ok_or(PageCursorStoreError::Unavailable)?;
        if self.entries.len() >= PAGE_CURSOR_CAPACITY {
            if let Some(evicted) = self.entries.keys().next().copied() {
                self.entries.remove(&evicted);
            }
        }
        self.entries.insert(cursor, entry);
        Ok(cursor)
    }

    fn remove_expired(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, entry| entry.expires_at() > now);
    }
}

/// The Runtime-owned shared resource roots, replaced atomically as one pair after a successful
/// ReloadSharedResources fan-out.  The owning PromptService/ModelGateway stay immutable; only
/// the materialized PromptResourceView/ModelCatalogView roots rotate.
struct SharedResourceRoots {
    #[allow(
        dead_code,
        reason = "the immediately adjacent shared-resource capture slice consumes this root"
    )]
    prompt_resources: Arc<PromptResourceView>,
    #[allow(
        dead_code,
        reason = "the immediately adjacent shared-resource capture slice consumes this root"
    )]
    model_catalog: Arc<ModelCatalogView>,
}

impl SharedResourceRoots {
    fn new(
        prompt_resources: Arc<PromptResourceView>,
        model_catalog: Arc<ModelCatalogView>,
    ) -> Self {
        Self {
            prompt_resources,
            model_catalog,
        }
    }

    fn install(
        &mut self,
        prompt_resources: Arc<PromptResourceView>,
        model_catalog: Arc<ModelCatalogView>,
    ) {
        self.prompt_resources = prompt_resources;
        self.model_catalog = model_catalog;
    }
}

// The shared-resource gate read/write sides are held across the Submit admission capture and
// the reload fan-out + root install.  The crate lint configuration bans raw Tokio guards across
// await points, so the guards are sealed in these permit newtypes exactly like the
// durable-state agent gates; the underlying Arc<RwLock<()>> multi-reader/write linearization
// is unchanged.
struct SharedResourceReadPermit {
    _guard: OwnedRwLockReadGuard<()>,
}

struct SharedResourceWritePermit {
    _guard: OwnedRwLockWriteGuard<()>,
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
        reason = "the immediately adjacent Turn capture slice consumes the Runtime owner"
    )]
    model_gateway: Arc<ModelGateway>,
    #[allow(
        dead_code,
        reason = "the configured graceful-Unload grace is installed on the residency registry at open; retained for the public Unload route and shutdown"
    )]
    unload_grace: StdDuration,
    // The current Runtime-owned shared resource roots.  A successful ReloadSharedResources
    // replaces this pair once under this mutex after the residency fan-out; external Submit
    // admissions hold the shared-resource read gate across their Turn context capture, so they
    // can never observe a half-switched pair.
    shared_resources: Mutex<SharedResourceRoots>,
    // Serializes a ReloadSharedResources fan-out + root install against external Submit
    // admissions.  Reload holds the write side; every external TurnCommand::Submit holds the
    // read side only until residency.submit returns (the Turn context admission is complete).
    // This is a multi-reader gate, so Submits across different Sessions never serialize against
    // each other.
    shared_resource_gate: Arc<RwLock<()>>,
    // The Runtime-owned read_file Workspace read-authority revocation control, installed only
    // by the read_file opt-in at `open`.  The default/no-read Runtime stores None and keeps
    // its existing invalidation behavior; the read_file Runtime revokes through this exact
    // control before every host Workspace authority invalidation so the restriction is
    // published before the recovery re-resolves.  It is a separate owner clone of the same
    // process-local set the resolver authority checks, so no authority state is duplicated.
    read_access_control: Option<WorkspaceReadAccessControl>,
    retained_until_shutdown: Mutex<Option<Arc<RuntimeInner>>>,
    session_residency: Mutex<Option<Arc<SessionResidencyRegistry>>>,
    durable_state: Mutex<Option<DurableState>>,
    // Runtime mutations and new Runtime subscriptions serialize mutation + publication against
    // receiver registration + initial snapshot without holding a live-state lock across await.
    runtime_publication: Arc<Semaphore>,
    runtime_events: Mutex<Option<broadcast::Sender<Arc<StateEvent>>>>,
    page_cursors: Mutex<PageCursorStore>,
    in_flight_commands: Mutex<BTreeMap<crate::wire::CommandId, Arc<RuntimeCommandInFlight>>>,
    lifecycle: Mutex<RuntimeLifecycle>,
    lifecycle_changed: Notify,
}

impl RuntimeInner {
    #[allow(
        clippy::too_many_arguments,
        reason = "one Runtime owner constructor binds the exact validated services, residency, lifecycle settings, and optional read-authority control"
    )]
    fn new(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        session_residency: Arc<SessionResidencyRegistry>,
        prompt_service: Arc<PromptService>,
        shared_resources: SharedResourceRoots,
        model_gateway: Arc<ModelGateway>,
        unload_grace: StdDuration,
        read_access_control: Option<WorkspaceReadAccessControl>,
    ) -> Self {
        let (runtime_events, _) = broadcast::channel(RUNTIME_EVENT_CAPACITY);
        Self {
            task_context,
            prompt_service,
            model_gateway,
            unload_grace,
            shared_resources: Mutex::new(shared_resources),
            shared_resource_gate: Arc::new(RwLock::new(())),
            read_access_control,
            retained_until_shutdown: Mutex::new(None),
            session_residency: Mutex::new(Some(session_residency)),
            durable_state: Mutex::new(Some(durable_state)),
            runtime_publication: Arc::new(Semaphore::new(1)),
            runtime_events: Mutex::new(Some(runtime_events)),
            page_cursors: Mutex::new(PageCursorStore::default()),
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
    fn prompt_resources(&self) -> (Arc<PromptService>, Arc<PromptResourceView>) {
        let roots = lock(&self.shared_resources);
        (
            Arc::clone(&self.prompt_service),
            Arc::clone(&roots.prompt_resources),
        )
    }

    #[cfg(test)]
    fn model_resources(&self) -> (Arc<ModelGateway>, Arc<ModelCatalogView>) {
        let roots = lock(&self.shared_resources);
        (
            Arc::clone(&self.model_gateway),
            Arc::clone(&roots.model_catalog),
        )
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
            RuntimeCommand::Agent(AgentCommand::Create {
                definition,
                metadata,
            }) => {
                let attempt = match SealedAgentCreateAttempt::new(
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
                            "Agent definition or metadata is invalid",
                            RetryAdvice::DoNotRetry,
                            Some(PublicSubject::Runtime),
                        ));
                    }
                };
                let Some(durable_state) = lock(&self.durable_state).as_ref().cloned() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                let _publication = Arc::clone(&self.runtime_publication)
                    .acquire_owned()
                    .await
                    .expect("Runtime publication semaphore remains open");
                match durable_state.create_agent(attempt).await {
                    Ok(head) => {
                        self.publish_durable_agent_event(
                            RuntimeStateEventKind::AgentCreated,
                            command_id,
                            head.as_ref(),
                        );
                        completed_outcome(CommandOutcome::AgentCreated {
                            agent_id: head.agent_id(),
                            definition_revision: head.current_definition_revision(),
                            metadata_revision: head.metadata().revision(),
                        })
                    }
                    Err(error) => map_agent_create_error(command_id, error)?,
                }
            }
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision,
                patch,
            }) => {
                self.dispatch_agent_definition(command_id, agent_id, expected_revision, patch)
                    .await?
            }
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision,
                patch,
            }) => {
                self.dispatch_agent_metadata(command_id, agent_id, expected_revision, patch)
                    .await?
            }
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status,
                status,
            }) => {
                let attempt = SealedAgentStatusAttempt::set_usable(
                    agent_id,
                    expected_status,
                    status.as_agent_status(),
                )
                .expect("AgentUsableStatus cannot target Deleted");
                self.dispatch_agent_status(command_id, agent_id, attempt, false)
                    .await?
            }
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id,
                expected_status,
            }) => {
                self.dispatch_agent_status(
                    command_id,
                    agent_id,
                    SealedAgentStatusAttempt::delete(agent_id, expected_status),
                    true,
                )
                .await?
            }
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
                let _publication = Arc::clone(&self.runtime_publication)
                    .acquire_owned()
                    .await
                    .expect("Runtime publication semaphore remains open");
                match durable_state.create_session(attempt).await {
                    Ok(head) => {
                        self.publish_durable_session_event(
                            RuntimeStateEventKind::SessionCreated,
                            command_id,
                            head.as_ref(),
                        );
                        completed_output(head.session_id().to_string())
                    }
                    Err(error) => map_session_create_error(command_id, agent_id, error)?,
                }
            }
            RuntimeCommand::Session(SessionCommand::Load { session_id }) => {
                let _publication = Arc::clone(&self.runtime_publication)
                    .acquire_owned()
                    .await
                    .expect("Runtime publication semaphore remains open");
                match self.load_session_ready_idle(session_id).await {
                    Ok(outcome) => {
                        if outcome.changed() {
                            self.publish_session_membership(
                                RuntimeStateEventKind::SessionLoaded,
                                command_id,
                                session_id,
                            );
                        }
                        completed_output("session loaded")
                    }
                    Err(error) => map_load_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Session(SessionCommand::Unload { session_id }) => {
                let _publication = Arc::clone(&self.runtime_publication)
                    .acquire_owned()
                    .await
                    .expect("Runtime publication semaphore remains open");
                match self.unload_session(session_id).await {
                    Ok(outcome) => {
                        if outcome.changed() {
                            self.publish_session_membership(
                                RuntimeStateEventKind::SessionUnloaded,
                                command_id,
                                session_id,
                            );
                        }
                        completed_output("session unloaded")
                    }
                    Err(error) => map_unload_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Session(SessionCommand::Archive { session_id }) => {
                self.dispatch_session_lifecycle(
                    command_id,
                    session_id,
                    SealedSessionLifecycleAttempt::archive(session_id),
                    RuntimeStateEventKind::SessionArchived,
                    CommandOutcome::SessionArchived,
                    false,
                )
                .await?
            }
            RuntimeCommand::Session(SessionCommand::Unarchive { session_id }) => {
                self.dispatch_session_lifecycle(
                    command_id,
                    session_id,
                    SealedSessionLifecycleAttempt::unarchive(session_id),
                    RuntimeStateEventKind::SessionUnarchived,
                    CommandOutcome::SessionUnarchived,
                    false,
                )
                .await?
            }
            RuntimeCommand::Session(SessionCommand::Delete { session_id }) => {
                self.dispatch_session_lifecycle(
                    command_id,
                    session_id,
                    SealedSessionLifecycleAttempt::delete(session_id),
                    RuntimeStateEventKind::SessionDeleted,
                    CommandOutcome::SessionDeleted,
                    true,
                )
                .await?
            }
            RuntimeCommand::Session(SessionCommand::Fork {
                source_session_id,
                anchor,
            }) => {
                let _publication = Arc::clone(&self.runtime_publication)
                    .acquire_owned()
                    .await
                    .expect("Runtime publication semaphore remains open");
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                match residency
                    .fork(source_session_id, anchor, SystemClock.now())
                    .await
                {
                    Ok(head) => {
                        let Some(provenance) = head.fork_provenance() else {
                            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
                        };
                        self.publish_durable_session_event(
                            RuntimeStateEventKind::SessionForked,
                            command_id,
                            head.as_ref(),
                        );
                        completed_outcome(CommandOutcome::SessionForked {
                            session_id: head.session_id(),
                            source: provenance.source(),
                        })
                    }
                    Err(error) => map_fork_error(command_id, source_session_id, error)?,
                }
            }
            RuntimeCommand::Session(SessionCommand::UpdateDefinition {
                session_id,
                expected_revision,
                patch,
            }) => {
                self.dispatch_session_definition(command_id, session_id, expected_revision, patch)
                    .await?
            }
            RuntimeCommand::Session(SessionCommand::UpgradeAgentRevision {
                session_id,
                expected_revision,
                target,
            }) => {
                self.dispatch_session_agent_upgrade(
                    command_id,
                    session_id,
                    expected_revision,
                    target,
                )
                .await?
            }
            RuntimeCommand::Session(SessionCommand::ReloadWorkspace { session_id }) => {
                self.dispatch_session_workspace_reload(command_id, session_id)
                    .await?
            }
            RuntimeCommand::Session(SessionCommand::UpdateMetadata {
                session_id,
                expected_revision,
                patch,
            }) => {
                self.dispatch_session_metadata(command_id, session_id, expected_revision, patch)
                    .await?
            }
            RuntimeCommand::Interaction(InteractionCommand::Resolve {
                session_id,
                expected_turn_id,
                item_id,
                request_id,
                resolution,
                resolution_key,
            }) => {
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                match residency
                    .resolve_interaction(
                        session_id,
                        crate::session_residency::SessionInteractionTarget {
                            expected_turn_id,
                            item_id,
                            request_id,
                        },
                        resolution_key,
                        resolution,
                        SystemClock.now(),
                    )
                    .await
                {
                    Ok(()) => completed_outcome(CommandOutcome::InteractionResolved),
                    Err(error) => map_interaction_error(command_id, session_id, error)?,
                }
            }
            RuntimeCommand::Turn(TurnCommand::Submit { session_id, intent }) => {
                let Some(residency) = self.residency() else {
                    return Err(RuntimeDispatchError::RuntimeClosed);
                };
                // External Submit admission captures the current shared resource pair into its
                // Turn context: hold the shared-resource read gate until residency.submit
                // returns (the admission is installed and its Turn context captured), so a
                // concurrent ReloadSharedResources cannot install a half-switched pair under
                // the admission.  The multi-reader gate never serializes Submits across
                // different Sessions against each other.
                let _shared_read = SharedResourceReadPermit {
                    _guard: Arc::clone(&self.shared_resource_gate).read_owned().await,
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
            RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources) => {
                self.dispatch_shared_resources_reload(command_id).await?
            }
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
            RuntimeQuery::Agent(AgentQuery::ListAgents {
                page,
                include_deleted,
            }) => {
                let limit = query_page_limit(page.limit())?;
                let result = match page.cursor() {
                    Some(cursor) => {
                        lock(&self.page_cursors).next_agents(cursor, include_deleted, limit)
                    }
                    None => {
                        let Some(durable_state) = lock(&self.durable_state).as_ref().cloned()
                        else {
                            return Err(runtime_closing_query());
                        };
                        let mut agents = Vec::new();
                        for head in durable_state.agent_catalog_heads() {
                            if !include_deleted && head.status() == AgentStatus::Deleted {
                                continue;
                            }
                            let metadata = head.metadata();
                            let metadata = AgentMetadataView::new(
                                metadata.revision(),
                                metadata.name(),
                                metadata.description(),
                                metadata.updated_at(),
                            )
                            .map_err(|_| unavailable_query(Some(PublicSubject::Runtime)))?;
                            agents.push(AgentSummary::new(
                                head.agent_id(),
                                head.current_definition_revision(),
                                metadata,
                                head.status(),
                                head.created_at(),
                            ));
                        }
                        lock(&self.page_cursors).first_agents(agents, include_deleted, limit)
                    }
                }
                .map_err(map_page_cursor_error)?;
                Ok(QueryResponse::new(QueryResult::Agent(
                    AgentQueryResult::Agents(result),
                )))
            }
            RuntimeQuery::Session(SessionQuery::ListSessions {
                page,
                include_archived,
            }) => {
                let limit = query_page_limit(page.limit())?;
                let result = match page.cursor() {
                    Some(cursor) => {
                        lock(&self.page_cursors).next_sessions(cursor, include_archived, limit)
                    }
                    None => {
                        let Some(durable_state) = lock(&self.durable_state).as_ref().cloned()
                        else {
                            return Err(runtime_closing_query());
                        };
                        let mut sessions = Vec::new();
                        for head in durable_state.session_catalog_heads() {
                            let lifecycle = match head.lifecycle() {
                                SessionLifecycle::Open => SessionLifecycleView::Open,
                                SessionLifecycle::Archived if include_archived => {
                                    SessionLifecycleView::Archived
                                }
                                SessionLifecycle::Archived | SessionLifecycle::Deleted => continue,
                            };
                            let summary = public_session_summary(head.as_ref())
                                .map_err(|_| unavailable_query(Some(PublicSubject::Runtime)))?;
                            debug_assert_eq!(summary.lifecycle(), lifecycle);
                            if summary.lifecycle() != lifecycle {
                                return Err(unavailable_query(Some(PublicSubject::Runtime)));
                            }
                            sessions.push(summary);
                        }
                        sessions.sort_by(|left, right| {
                            right
                                .created_at()
                                .cmp(&left.created_at())
                                .then_with(|| left.session_id().cmp(&right.session_id()))
                        });
                        lock(&self.page_cursors).first_sessions(sessions, include_archived, limit)
                    }
                }
                .map_err(map_page_cursor_error)?;
                Ok(QueryResponse::new(QueryResult::Session(
                    SessionQueryResult::Sessions(result),
                )))
            }
            RuntimeQuery::Session(SessionQuery::GetSessionForkProvenance { session_id }) => {
                let Some(durable_state) = lock(&self.durable_state).as_ref().cloned() else {
                    return Err(runtime_closing_query());
                };
                let head = durable_state
                    .session_head(session_id)
                    .ok_or_else(|| not_found_query(PublicSubject::Session(session_id)))?;
                let provenance = head.fork_provenance().map(|provenance| {
                    SessionForkProvenanceView::new(
                        provenance.source_session_id(),
                        provenance.source(),
                        provenance.anchor().clone(),
                    )
                });
                Ok(QueryResponse::new(QueryResult::Session(
                    SessionQueryResult::ForkProvenance(provenance),
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
            SnapshotRequest::Runtime => self
                .public_runtime_snapshot()
                .map(SnapshotResponse::Runtime),
        }
    }

    async fn subscribe(
        self: &Arc<Self>,
        request: SubscriptionRequest,
    ) -> Result<EventStream, SubscriptionError> {
        if request.scope() == SubscriptionScope::Runtime {
            let _publication = Arc::clone(&self.runtime_publication)
                .acquire_owned()
                .await
                .expect("Runtime publication semaphore remains open");
            if !matches!(*lock(&self.lifecycle), RuntimeLifecycle::Open) {
                return Err(subscription_closing());
            }
            let events = lock(&self.runtime_events);
            let Some(sender) = events.as_ref() else {
                return Err(subscription_closing());
            };
            let receiver = sender.subscribe();
            let snapshot = self.public_runtime_snapshot().map_err(|_| {
                SubscriptionError::new(
                    SubscriptionErrorCode::PublisherUnavailable,
                    "runtime event publisher is unavailable",
                    RetryAdvice::DoNotRetry,
                    Some(PublicSubject::Runtime),
                )
            })?;
            return Ok(EventStream {
                runtime: Arc::clone(self),
                initial: Some(EventFrame::Snapshot(SnapshotResponse::Runtime(snapshot))),
                subscription: EventSubscription::Runtime(receiver),
            });
        }

        let SubscriptionScope::Session { session_id } = request.scope() else {
            unreachable!("Runtime scope returned above");
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
            subscription: EventSubscription::Session(subscription),
        })
    }

    fn public_runtime_snapshot(&self) -> Result<RuntimeSnapshot, SnapshotError> {
        let Some(residency) = self.residency() else {
            return Err(runtime_snapshot_closing());
        };
        let mut loaded = Vec::new();
        for snapshot in residency.loaded_session_snapshots() {
            let session_id = snapshot.definition().session_id();
            loaded.push(
                LoadedSessionSummary::new(
                    session_id,
                    snapshot.readiness(),
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
            .map_err(|_| runtime_snapshot_closing())
    }

    fn publish_durable_agent_event(
        &self,
        kind: RuntimeStateEventKind,
        command_id: crate::wire::CommandId,
        head: &DurableAgentHead,
    ) {
        debug_assert!(matches!(
            kind,
            RuntimeStateEventKind::AgentCreated
                | RuntimeStateEventKind::AgentDefinitionUpdated
                | RuntimeStateEventKind::AgentMetadataUpdated
                | RuntimeStateEventKind::AgentStatusChanged
        ));
        let Ok(agent) = public_agent_summary(head) else {
            return;
        };
        let events = lock(&self.runtime_events);
        let Some(sender) = events.as_ref() else {
            return;
        };
        let Ok(snapshot) = self.public_runtime_snapshot() else {
            return;
        };
        let event = match kind {
            RuntimeStateEventKind::AgentCreated => {
                StateEvent::agent_created(head.created_at(), Some(command_id), snapshot, agent)
            }
            RuntimeStateEventKind::AgentDefinitionUpdated => StateEvent::agent_definition_updated(
                SystemClock.now(),
                Some(command_id),
                snapshot,
                agent,
            ),
            RuntimeStateEventKind::AgentMetadataUpdated => StateEvent::agent_metadata_updated(
                SystemClock.now(),
                Some(command_id),
                snapshot,
                agent,
            ),
            RuntimeStateEventKind::AgentStatusChanged => StateEvent::agent_status_changed(
                SystemClock.now(),
                Some(command_id),
                snapshot,
                agent,
            ),
            RuntimeStateEventKind::SessionCreated
            | RuntimeStateEventKind::SessionLoaded
            | RuntimeStateEventKind::SessionUnloaded
            | RuntimeStateEventKind::SessionArchived
            | RuntimeStateEventKind::SessionUnarchived
            | RuntimeStateEventKind::SessionDeleted
            | RuntimeStateEventKind::SessionForked
            | RuntimeStateEventKind::SessionMetadataUpdated
            | RuntimeStateEventKind::SessionDefinitionUpdated
            | RuntimeStateEventKind::CommandCatalogInvalidated
            | RuntimeStateEventKind::SharedResourcesReloaded => return,
        };
        let _ = sender.send(Arc::new(event));
    }

    fn publish_durable_agent_status_changed(
        &self,
        command_id: crate::wire::CommandId,
        timestamp: Timestamp,
        head: &DurableAgentHead,
    ) {
        let Ok(agent) = public_agent_summary(head) else {
            return;
        };
        let events = lock(&self.runtime_events);
        let Some(sender) = events.as_ref() else {
            return;
        };
        let Ok(snapshot) = self.public_runtime_snapshot() else {
            return;
        };
        let event = StateEvent::agent_status_changed(timestamp, Some(command_id), snapshot, agent);
        let _ = sender.send(Arc::new(event));
    }

    fn publish_session_metadata_updated(
        &self,
        command_id: crate::wire::CommandId,
        timestamp: Timestamp,
        head: &DurableSessionHead,
    ) {
        let Ok(session) = public_session_summary(head) else {
            return;
        };
        let events = lock(&self.runtime_events);
        let Some(sender) = events.as_ref() else {
            return;
        };
        let Ok(snapshot) = self.public_runtime_snapshot() else {
            return;
        };
        let event =
            StateEvent::session_metadata_updated(timestamp, Some(command_id), snapshot, session);
        let _ = sender.send(Arc::new(event));
    }

    fn publish_session_definition_updated(
        &self,
        command_id: crate::wire::CommandId,
        timestamp: Timestamp,
        head: &DurableSessionHead,
    ) {
        let Ok(session) = public_session_summary(head) else {
            return;
        };
        let events = lock(&self.runtime_events);
        let Some(sender) = events.as_ref() else {
            return;
        };
        let Ok(snapshot) = self.public_runtime_snapshot() else {
            return;
        };
        let event =
            StateEvent::session_definition_updated(timestamp, Some(command_id), snapshot, session);
        let _ = sender.send(Arc::new(event));
    }

    fn publish_durable_session_event(
        &self,
        kind: RuntimeStateEventKind,
        command_id: crate::wire::CommandId,
        head: &DurableSessionHead,
    ) {
        debug_assert!(matches!(
            kind,
            RuntimeStateEventKind::SessionCreated
                | RuntimeStateEventKind::SessionArchived
                | RuntimeStateEventKind::SessionUnarchived
                | RuntimeStateEventKind::SessionDeleted
                | RuntimeStateEventKind::SessionForked
        ));
        let Ok(session) = public_session_summary(head) else {
            return;
        };
        let events = lock(&self.runtime_events);
        let Some(sender) = events.as_ref() else {
            return;
        };
        let Ok(snapshot) = self.public_runtime_snapshot() else {
            return;
        };
        let event = match kind {
            RuntimeStateEventKind::SessionCreated => {
                StateEvent::session_created(head.created_at(), Some(command_id), snapshot, session)
            }
            RuntimeStateEventKind::SessionForked => {
                StateEvent::session_forked(head.created_at(), Some(command_id), snapshot, session)
            }
            RuntimeStateEventKind::SessionArchived => {
                StateEvent::session_archived(SystemClock.now(), Some(command_id), snapshot, session)
            }
            RuntimeStateEventKind::SessionUnarchived => StateEvent::session_unarchived(
                SystemClock.now(),
                Some(command_id),
                snapshot,
                session,
            ),
            RuntimeStateEventKind::SessionDeleted => {
                StateEvent::session_deleted(SystemClock.now(), Some(command_id), snapshot, session)
            }
            RuntimeStateEventKind::SessionLoaded
            | RuntimeStateEventKind::SessionUnloaded
            | RuntimeStateEventKind::SessionMetadataUpdated
            | RuntimeStateEventKind::SessionDefinitionUpdated
            | RuntimeStateEventKind::AgentCreated
            | RuntimeStateEventKind::AgentDefinitionUpdated
            | RuntimeStateEventKind::AgentMetadataUpdated
            | RuntimeStateEventKind::AgentStatusChanged
            | RuntimeStateEventKind::CommandCatalogInvalidated
            | RuntimeStateEventKind::SharedResourcesReloaded => return,
        };
        let _ = sender.send(Arc::new(event));
    }

    fn publish_session_membership(
        &self,
        kind: RuntimeStateEventKind,
        command_id: crate::wire::CommandId,
        session_id: SessionId,
    ) {
        let events = lock(&self.runtime_events);
        let Some(sender) = events.as_ref() else {
            return;
        };
        let Ok(snapshot) = self.public_runtime_snapshot() else {
            return;
        };
        let timestamp = SystemClock.now();
        let event = match kind {
            RuntimeStateEventKind::SessionLoaded => {
                StateEvent::session_loaded(timestamp, Some(command_id), snapshot, session_id)
            }
            RuntimeStateEventKind::SessionUnloaded => {
                StateEvent::session_unloaded(timestamp, Some(command_id), snapshot, session_id)
            }
            RuntimeStateEventKind::SessionCreated
            | RuntimeStateEventKind::SessionMetadataUpdated
            | RuntimeStateEventKind::SessionDefinitionUpdated
            | RuntimeStateEventKind::AgentCreated
            | RuntimeStateEventKind::AgentDefinitionUpdated
            | RuntimeStateEventKind::AgentMetadataUpdated
            | RuntimeStateEventKind::AgentStatusChanged
            | RuntimeStateEventKind::SessionArchived
            | RuntimeStateEventKind::SessionUnarchived
            | RuntimeStateEventKind::SessionDeleted
            | RuntimeStateEventKind::SessionForked
            | RuntimeStateEventKind::CommandCatalogInvalidated
            | RuntimeStateEventKind::SharedResourcesReloaded => return,
        };
        let _ = sender.send(Arc::new(event));
    }

    fn public_session_snapshot(
        &self,
        snapshot: Arc<SessionExecutorSnapshot>,
    ) -> Result<SessionSnapshot, SnapshotError> {
        let session_id = snapshot.definition().session_id();
        // The whole read model is projected from the executor's immutable observation state,
        // including the definition.  The recv()/snapshot projection never compares against a
        // temporary durable read here, so every event and snapshot carries the exact definition
        // (and metadata) of its moment even across consecutive updates, and older queued events
        // are never dropped or revision-crossed.
        let metadata = snapshot.metadata();
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
        let readiness = snapshot.readiness();
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
            ) && matches!(readiness, SessionReadinessView::Ready),
        )
        .map_err(|_| unavailable_snapshot(session_id))?;
        SessionSnapshot::new_loaded_with_readiness_with_observation(
            session_id,
            metadata,
            definition,
            readiness,
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
        self.close_runtime_event_publisher();
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
                    self.close_runtime_event_publisher();
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

    /// The host-only security invalidation route: no runtime publication semaphore, no ordinary
    /// work-lane wait.  A missing loaded executor is `SessionNotLoaded`; a registry/runtime
    /// closing is `RuntimeClosing`; an impossible actor failure is `Internal`.  For the
    /// Runtime-owned read_file authority, the permanent per-Session read revocation is
    /// published here first (after the Open lifecycle and residency existence checks, before
    /// the timestamp sample and the residency invalidation), so the hard restriction is
    /// current before the recovery re-resolves and stays published even when the residency
    /// returns SessionNotLoaded/Closing/internal.  The default/no-read Runtime has no control
    /// and keeps its existing behavior.  The single `SystemClock` timestamp is sampled here
    /// and carried through the executor recovery events.
    async fn invalidate_session_workspace_authority(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionWorkspaceInvalidationError> {
        if !matches!(*lock(&self.lifecycle), RuntimeLifecycle::Open) {
            return Err(SessionWorkspaceInvalidationError::RuntimeClosing);
        }
        let Some(residency) = self.residency() else {
            return Err(SessionWorkspaceInvalidationError::RuntimeClosing);
        };
        // The Runtime-owned read_file authority publishes its permanent read revocation for
        // this Session before the host restriction is routed: the revoke is the hard
        // restriction for that authority, stays current even if the residency below returns
        // SessionNotLoaded/Closing/internal, and is observed by the recovery's re-resolve.
        if let Some(control) = self.read_access_control.as_ref() {
            control.revoke(session_id);
        }
        let timestamp = SystemClock.now();
        match residency
            .invalidate_workspace_authority(session_id, timestamp)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => match error {
                SessionResidencySecurityInvalidationError::Closing => {
                    Err(SessionWorkspaceInvalidationError::RuntimeClosing)
                }
                SessionResidencySecurityInvalidationError::SessionNotLoaded => {
                    Err(SessionWorkspaceInvalidationError::SessionNotLoaded)
                }
                SessionResidencySecurityInvalidationError::InternalDispatchUnavailable => {
                    Err(SessionWorkspaceInvalidationError::InternalDispatchUnavailable)
                }
            },
        }
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

    async fn dispatch_agent_status(
        &self,
        command_id: crate::wire::CommandId,
        agent_id: crate::wire::AgentId,
        attempt: SealedAgentStatusAttempt,
        deleted_outcome: bool,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        let Some(durable_state) = lock(&self.durable_state).as_ref().cloned() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        match durable_state.set_agent_status(attempt).await {
            Ok(outcome @ DurableAgentStatusOutcome::Updated(_)) => {
                let head = outcome.head();
                // DurableState has already released the per-Agent status gate: sample the single
                // owner timestamp for this status change, then fan the availability fact out to
                // every loaded Session that pins this Agent before publishing the Runtime event,
                // so the AgentStatusChanged RuntimeSnapshot already reflects the new readiness.
                let timestamp = SystemClock.now();
                if let Some(residency) = self.residency() {
                    let session_ids = residency.loaded_session_ids_for_agent(agent_id);
                    let available = matches!(head.status(), AgentStatus::Enabled);
                    for session_id in session_ids {
                        match residency
                            .set_session_agent_availability(
                                session_id, agent_id, available, timestamp, command_id,
                            )
                            .await
                        {
                            Ok(()) => {}
                            Err(
                                SessionResidencyAgentAvailabilityError::Closing
                                | SessionResidencyAgentAvailabilityError::InternalDispatchUnavailable,
                            ) => {
                                // The durable Agent status has already changed, so this command
                                // must not settle as an ordinary rejection (RuntimeClosing or
                                // otherwise): the post-durable required live Session publication
                                // is incomplete, and durable Agent status and live Session
                                // readiness would diverge indefinitely.  Fail the outer dispatch
                                // exactly like the InternalDispatchUnavailable invariant.
                                return Err(RuntimeDispatchError::InternalDispatchUnavailable);
                            }
                        }
                    }
                }
                self.publish_durable_agent_status_changed(command_id, timestamp, head.as_ref());
                let command_outcome = if deleted_outcome {
                    CommandOutcome::AgentDeleted
                } else {
                    CommandOutcome::AgentStatusChanged {
                        status: head.status(),
                    }
                };
                Ok(completed_outcome(command_outcome))
            }
            Ok(DurableAgentStatusOutcome::NoChange(_)) => {
                Ok(completed_outcome(CommandOutcome::NoChange))
            }
            Err(DurableAgentStatusError::AgentDeleted) if deleted_outcome => {
                Ok(completed_outcome(CommandOutcome::AgentDeleted))
            }
            Err(error) => map_agent_status_error(command_id, agent_id, error),
        }
    }

    async fn dispatch_agent_definition(
        &self,
        command_id: crate::wire::CommandId,
        agent_id: crate::wire::AgentId,
        expected_revision: crate::wire::AgentRevision,
        patch: crate::agent_session_lifecycle::AgentDefinitionPatch,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        let Some(durable_state) = lock(&self.durable_state).as_ref().cloned() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        let Some(current_definition) = durable_state.agent_current_definition(agent_id) else {
            return Ok(rejected_completion(
                CommandErrorCode::NotFound,
                "Agent was not found",
                RetryAdvice::RefreshAndRetry,
                Some(PublicSubject::Agent(agent_id)),
            ));
        };
        let prompts = patch
            .prompts()
            .cloned()
            .unwrap_or_else(|| current_definition.prompts().clone());
        let attempt = SealedAgentDefinitionAttempt::new(
            agent_id,
            expected_revision,
            prompts,
            SystemClock.now(),
        );
        match durable_state.update_agent_definition(attempt).await {
            Ok(DurableAgentDefinitionOutcome::Updated(head, definition)) => {
                self.publish_durable_agent_event(
                    RuntimeStateEventKind::AgentDefinitionUpdated,
                    command_id,
                    head.as_ref(),
                );
                Ok(completed_outcome(CommandOutcome::AgentDefinitionUpdated {
                    definition_revision: definition.revision(),
                }))
            }
            Ok(DurableAgentDefinitionOutcome::NoChange(_, _)) => {
                Ok(completed_outcome(CommandOutcome::NoChange))
            }
            Err(error) => map_agent_definition_error(agent_id, error),
        }
    }

    async fn dispatch_agent_metadata(
        &self,
        command_id: crate::wire::CommandId,
        agent_id: crate::wire::AgentId,
        expected_revision: crate::wire::AgentMetadataRevision,
        patch: crate::agent_session_lifecycle::AgentMetadataPatch,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        let Some(durable_state) = lock(&self.durable_state).as_ref().cloned() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        let attempt = SealedAgentMetadataAttempt::new(
            agent_id,
            expected_revision,
            patch.name().map(str::to_owned),
            patch.description().clone(),
            SystemClock.now(),
        )
        .expect("public Agent metadata patches are already validated");
        match durable_state.update_agent_metadata(attempt).await {
            Ok(DurableAgentMetadataOutcome::Updated(head)) => {
                self.publish_durable_agent_event(
                    RuntimeStateEventKind::AgentMetadataUpdated,
                    command_id,
                    head.as_ref(),
                );
                Ok(completed_outcome(CommandOutcome::AgentMetadataUpdated {
                    metadata_revision: head.metadata().revision(),
                }))
            }
            Ok(DurableAgentMetadataOutcome::NoChange(_)) => {
                Ok(completed_outcome(CommandOutcome::NoChange))
            }
            Err(error) => map_agent_metadata_error(agent_id, error),
        }
    }

    async fn dispatch_session_lifecycle(
        &self,
        command_id: crate::wire::CommandId,
        session_id: SessionId,
        attempt: SealedSessionLifecycleAttempt,
        event_kind: RuntimeStateEventKind,
        changed_outcome: CommandOutcome,
        deleted_is_success: bool,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        match self.update_session_lifecycle(attempt).await {
            Ok(crate::durable_state::DurableSessionLifecycleOutcome::Updated(head)) => {
                self.publish_durable_session_event(event_kind, command_id, head.as_ref());
                Ok(completed_outcome(changed_outcome))
            }
            Ok(crate::durable_state::DurableSessionLifecycleOutcome::NoChange(_)) => {
                let outcome = if deleted_is_success {
                    CommandOutcome::SessionDeleted
                } else {
                    CommandOutcome::NoChange
                };
                Ok(completed_outcome(outcome))
            }
            Err(SessionResidencyLifecycleError::SessionDeleted) if deleted_is_success => {
                Ok(completed_outcome(CommandOutcome::SessionDeleted))
            }
            Err(error) => map_session_lifecycle_error(command_id, session_id, error),
        }
    }

    async fn dispatch_session_metadata(
        &self,
        command_id: crate::wire::CommandId,
        session_id: SessionId,
        expected_revision: SessionMetadataRevision,
        patch: crate::agent_session_lifecycle::SessionMetadataPatch,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        // One owner timestamp is sampled inside the publication semaphore and feeds both the
        // sealed attempt and every metadata event of this command.
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        let Some(residency) = self.residency() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let timestamp = SystemClock.now();
        let (name, description) = patch.into_sealed_parts();
        let attempt = SealedSessionMetadataAttempt::new(
            session_id,
            expected_revision,
            name,
            description,
            timestamp,
        );
        match residency
            .update_session_metadata(attempt, timestamp, command_id)
            .await
        {
            Ok(outcome) if outcome.changed() => {
                self.publish_session_metadata_updated(
                    command_id,
                    timestamp,
                    outcome.head().as_ref(),
                );
                Ok(completed_outcome(CommandOutcome::SessionMetadataUpdated {
                    metadata_revision: outcome.head().metadata().revision(),
                }))
            }
            Ok(_) => Ok(completed_outcome(CommandOutcome::NoChange)),
            Err(error) => map_session_metadata_error(command_id, session_id, error),
        }
    }

    async fn dispatch_session_definition(
        &self,
        command_id: crate::wire::CommandId,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        patch: crate::runtime_interface::SessionDefinitionPatch,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        // One owner timestamp is sampled inside the publication semaphore and feeds both the
        // sealed attempt and every definition event of this command.
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        let Some(residency) = self.residency() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let timestamp = SystemClock.now();
        // The Workspace candidate is host-lowered before any Session gate or durable CAS; an
        // invalid lowering means the typed command cannot form on this host.
        let workspace = match patch.workspace().cloned() {
            Some(input) => match lower_workspace(
                input,
                WorkspaceRevision::new(NonZeroU64::new(1).expect("one is non-zero")),
                WorkspacePathTarget::current(),
            ) {
                Ok(workspace) => Some(workspace),
                Err(_) => {
                    return Ok(rejected_completion(
                        CommandErrorCode::InvalidArgument,
                        "workspace input is invalid for this host",
                        RetryAdvice::DoNotRetry,
                        Some(PublicSubject::Session(session_id)),
                    ));
                }
            },
            None => None,
        };
        match residency
            .update_session_definition(
                session_id,
                expected_revision,
                workspace,
                patch.model().cloned(),
                patch.prompts().cloned(),
                timestamp,
                command_id,
            )
            .await
        {
            Ok(outcome) if outcome.changed() => {
                self.publish_session_definition_updated(
                    command_id,
                    timestamp,
                    outcome.head().as_ref(),
                );
                Ok(completed_outcome(
                    CommandOutcome::SessionDefinitionUpdated {
                        definition_revision: outcome.definition_revision(),
                    },
                ))
            }
            Ok(_) => Ok(completed_outcome(CommandOutcome::NoChange)),
            Err(error) => map_session_definition_error(command_id, session_id, error),
        }
    }

    async fn dispatch_session_agent_upgrade(
        &self,
        command_id: crate::wire::CommandId,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        target: Option<AgentRevisionRef>,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        // One owner timestamp is sampled inside the publication semaphore and feeds both the
        // sealed attempt and every definition event of this command.  Target current resolution
        // happens only inside DurableState under its Agent → Session publication gates.
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        let Some(residency) = self.residency() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let timestamp = SystemClock.now();
        match residency
            .upgrade_session_agent(session_id, expected_revision, target, timestamp, command_id)
            .await
        {
            Ok(outcome) if outcome.changed() => {
                self.publish_session_definition_updated(
                    command_id,
                    timestamp,
                    outcome.head().as_ref(),
                );
                Ok(completed_outcome(
                    CommandOutcome::SessionDefinitionUpdated {
                        definition_revision: outcome.definition().revision(),
                    },
                ))
            }
            Ok(_) => Ok(completed_outcome(CommandOutcome::NoChange)),
            Err(error) => map_session_agent_upgrade_error(command_id, session_id, error),
        }
    }

    async fn dispatch_session_workspace_reload(
        &self,
        command_id: crate::wire::CommandId,
        session_id: SessionId,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        // The reload is Session-scope only: it emits a SessionExecutorEvent, never reads or
        // updates DurableState, and publishes no Runtime-scope event.  The per-Session residency
        // gate plus the executor publication slot already serialize it, so no Runtime
        // publication semaphore is held here.
        let Some(residency) = self.residency() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let timestamp = SystemClock.now();
        match residency
            .reload_workspace(session_id, timestamp, command_id)
            .await
        {
            Ok(_) => Ok(completed_outcome(CommandOutcome::WorkspaceReloaded)),
            Err(error) => map_session_workspace_reload_error(command_id, session_id, error),
        }
    }

    /// Dispatches one Runtime shared-resource reload inside the Runtime publication semaphore.
    /// Both candidates build before anything is installed: any Prompt/Model candidate failure
    /// leaves the old roots, every loaded executor, and every future `turn_resources` completely
    /// unchanged and settles as a typed `ReloadValidationFailed` rejection with no events.  After
    /// both candidates succeed, the shared-resource write gate covers the residency fan-out and
    /// the single atomic Runtime root pair replacement, so no external Submit admission can
    /// capture a half-switched pair; every residency failure at or after this point may already
    /// have entered a required live publication, so it fails the outer dispatch as
    /// `InternalDispatchUnavailable` instead of an ordinary rejection.  Only then is the
    /// Runtime-scope `SharedResourcesReloaded` event published and the typed outcome returned;
    /// per-Session readiness changes are published by the executor fan-out itself.
    async fn dispatch_shared_resources_reload(
        &self,
        command_id: crate::wire::CommandId,
    ) -> Result<CommandCompletion, RuntimeDispatchError> {
        let _publication = Arc::clone(&self.runtime_publication)
            .acquire_owned()
            .await
            .expect("Runtime publication semaphore remains open");
        // The two candidate builds are independent; join them so a slow Prompt discovery and a
        // slow Model discovery overlap.
        let (prompt_candidate, model_candidate) = tokio::join!(
            self.prompt_service.build_reload_candidate(),
            self.model_gateway.build_reload_candidate(),
        );
        let (prompt_resources, model_catalog) = match (prompt_candidate, model_candidate) {
            (Ok(prompt_resources), Ok(model_catalog)) => (prompt_resources, model_catalog),
            _ => {
                // Any ordinary candidate failure (source discovery, content load, duplicate
                // key, invalid definition) is a validation outcome: the old roots stay fully
                // installed and no event is published.
                return Ok(rejected_completion(
                    CommandErrorCode::ReloadValidationFailed,
                    "shared resource reload validation failed",
                    RetryAdvice::UserActionRequired,
                    Some(PublicSubject::Runtime),
                ));
            }
        };
        let Some(residency) = self.residency() else {
            return Err(RuntimeDispatchError::RuntimeClosed);
        };
        let timestamp = SystemClock.now();
        // The write gate covers the residency fan-out and the Runtime root install below.
        let _shared_write = SharedResourceWritePermit {
            _guard: Arc::clone(&self.shared_resource_gate).write_owned().await,
        };
        residency
            .install_shared_resources(
                Arc::clone(&prompt_resources),
                Arc::clone(&model_catalog),
                timestamp,
                command_id,
            )
            .await
            .map_err(|error| match error {
                SessionResidencySharedResourcesError::Closing
                | SessionResidencySharedResourcesError::InternalDispatchUnavailable => {
                    // The candidate succeeded; a Closing/Internal residency result at or after
                    // the install boundary may already have entered a required live
                    // publication, so this can never settle as an ordinary reload rejection.
                    RuntimeDispatchError::InternalDispatchUnavailable
                }
            })?;
        // One atomic root pair replacement after the whole fan-out succeeded.  The write gate is
        // still held, so the pair is never observed half-switched by an external Submit.
        lock(&self.shared_resources).install(prompt_resources, model_catalog);
        self.publish_shared_resources_reloaded(command_id, timestamp);
        Ok(completed_outcome(CommandOutcome::SharedResourcesReloaded))
    }

    fn publish_shared_resources_reloaded(
        &self,
        command_id: crate::wire::CommandId,
        timestamp: Timestamp,
    ) {
        let events = lock(&self.runtime_events);
        let Some(sender) = events.as_ref() else {
            return;
        };
        let Ok(snapshot) = self.public_runtime_snapshot() else {
            return;
        };
        let event = StateEvent::shared_resources_reloaded(timestamp, Some(command_id), snapshot);
        let _ = sender.send(Arc::new(event));
    }

    #[cfg(test)]
    async fn update_session_workspace_definition(
        &self,
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
    ) -> Result<
        crate::durable_state::DurableSessionDefinitionOutcome,
        SessionResidencyWorkspaceDefinitionError,
    > {
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

    fn close_runtime_event_publisher(&self) {
        lock(&self.runtime_events).take();
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
        // The shared task context can close while this dispatch is in flight. A fatal dispatch
        // error (for example root lease identity loss) must publish Closing before that first
        // error becomes observable. A completed publication with a remembered close requirement
        // has the opposite ordering contract: settle the successful waiter first, then publish
        // Closing. Preserve both sides of that Completed-before-Closing boundary.
        let owner_closing = guard.inner.task_context.is_closing();
        let fatal = matches!(
            &result,
            Err(RuntimeDispatchError::InternalDispatchUnavailable)
        );
        if owner_closing && fatal {
            guard.inner.request_closing();
        }
        guard.complete(result);
        if owner_closing && !fatal {
            guard.inner.request_closing();
        }
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
        crate::runtime_interface::RuntimeCapability::PagedQueries,
        crate::runtime_interface::RuntimeCapability::InteractionResolution,
        crate::runtime_interface::RuntimeCapability::SessionFork,
    ])
    .expect("the implemented Runtime capability set is a canonical V1 subset")
}

const fn retry_with_backoff() -> RetryAdvice {
    RetryAdvice::RetryWithBackoff { retry_after: None }
}

fn query_page_limit(limit: NonZeroU32) -> Result<usize, QueryError> {
    if limit.get() > u32::from(ProtocolLimits::v1_0().paging.max_page_size) {
        return Err(QueryError::new(
            QueryErrorCode::InvalidArgument,
            "page limit exceeds the selected protocol limit",
            RetryAdvice::DoNotRetry,
            Some(PublicSubject::Runtime),
        ));
    }
    usize::try_from(limit.get()).map_err(|_| {
        QueryError::new(
            QueryErrorCode::InvalidArgument,
            "page limit is not representable",
            RetryAdvice::DoNotRetry,
            Some(PublicSubject::Runtime),
        )
    })
}

fn public_session_summary(
    head: &DurableSessionHead,
) -> Result<SessionSummary, crate::runtime_interface::ObservationValueError> {
    let metadata = head.metadata();
    let metadata = SessionMetadataView::new(
        metadata.revision(),
        metadata.name(),
        metadata.description(),
        metadata.updated_at(),
    )?;
    let lifecycle = match head.lifecycle() {
        SessionLifecycle::Open => SessionLifecycleView::Open,
        SessionLifecycle::Archived => SessionLifecycleView::Archived,
        SessionLifecycle::Deleted => SessionLifecycleView::Deleted,
    };
    Ok(SessionSummary::new(
        head.session_id(),
        head.current_definition_revision(),
        metadata,
        lifecycle,
        head.fork_provenance().is_some(),
        head.created_at(),
    ))
}

fn public_agent_summary(
    head: &DurableAgentHead,
) -> Result<AgentSummary, crate::runtime_interface::ObservationValueError> {
    let metadata = head.metadata();
    Ok(AgentSummary::new(
        head.agent_id(),
        head.current_definition_revision(),
        AgentMetadataView::new(
            metadata.revision(),
            metadata.name(),
            metadata.description(),
            metadata.updated_at(),
        )?,
        head.status(),
        head.created_at(),
    ))
}

fn map_page_cursor_error(error: PageCursorStoreError) -> QueryError {
    match error {
        PageCursorStoreError::Stale => QueryError::new(
            QueryErrorCode::StaleCursor,
            "cursor is stale",
            RetryAdvice::RefreshAndRetry,
            None,
        ),
        PageCursorStoreError::Unavailable => unavailable_query(Some(PublicSubject::Runtime)),
    }
}

fn unavailable_query(subject: Option<PublicSubject>) -> QueryError {
    QueryError::new(
        QueryErrorCode::Unavailable,
        "query is unavailable",
        retry_with_backoff(),
        subject,
    )
}

fn not_found_query(subject: PublicSubject) -> QueryError {
    QueryError::new(
        QueryErrorCode::NotFound,
        "query subject was not found",
        RetryAdvice::RefreshAndRetry,
        Some(subject),
    )
}

fn runtime_closing_query() -> QueryError {
    QueryError::new(
        QueryErrorCode::RuntimeClosing,
        "runtime is closing",
        retry_with_backoff(),
        Some(PublicSubject::Runtime),
    )
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

fn map_agent_create_error(
    _command_id: crate::wire::CommandId,
    error: DurableAgentCreateError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let completion = match error {
        DurableAgentCreateError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        DurableAgentCreateError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            Some(PublicSubject::Runtime),
        ),
        DurableAgentCreateError::IdentityUnavailable
        | DurableAgentCreateError::CollisionAttemptsExhausted
        | DurableAgentCreateError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Agent creation is unavailable",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        DurableAgentCreateError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_agent_status_error(
    _command_id: crate::wire::CommandId,
    agent_id: crate::wire::AgentId,
    error: DurableAgentStatusError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Agent(agent_id));
    let completion = match error {
        DurableAgentStatusError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        DurableAgentStatusError::AgentNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Agent was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        DurableAgentStatusError::StaleStatus => rejected_completion(
            CommandErrorCode::StaleRevision,
            "Agent status compare-and-swap is stale",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        DurableAgentStatusError::AgentDeleted => rejected_completion(
            CommandErrorCode::AgentDeleted,
            "Agent is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        DurableAgentStatusError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        DurableAgentStatusError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Agent status update is unavailable",
            retry_with_backoff(),
            subject,
        ),
        DurableAgentStatusError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_agent_definition_error(
    agent_id: crate::wire::AgentId,
    error: DurableAgentDefinitionError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Agent(agent_id));
    let completion = match error {
        DurableAgentDefinitionError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        DurableAgentDefinitionError::AgentNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Agent was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        DurableAgentDefinitionError::StaleRevision => rejected_completion(
            CommandErrorCode::StaleRevision,
            "Agent definition compare-and-swap is stale",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        DurableAgentDefinitionError::AgentDeleted => rejected_completion(
            CommandErrorCode::AgentDeleted,
            "Agent is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        DurableAgentDefinitionError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        DurableAgentDefinitionError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Agent definition update is unavailable",
            retry_with_backoff(),
            subject,
        ),
        DurableAgentDefinitionError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_agent_metadata_error(
    agent_id: crate::wire::AgentId,
    error: DurableAgentMetadataError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Agent(agent_id));
    let completion = match error {
        DurableAgentMetadataError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        DurableAgentMetadataError::AgentNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Agent was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        DurableAgentMetadataError::StaleRevision => rejected_completion(
            CommandErrorCode::StaleRevision,
            "Agent metadata compare-and-swap is stale",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        DurableAgentMetadataError::AgentDeleted => rejected_completion(
            CommandErrorCode::AgentDeleted,
            "Agent is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        DurableAgentMetadataError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        DurableAgentMetadataError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Agent metadata update is unavailable",
            retry_with_backoff(),
            subject,
        ),
        DurableAgentMetadataError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
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

fn map_session_lifecycle_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyLifecycleError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyLifecycleError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyLifecycleError::SessionBusy => rejected_completion(
            CommandErrorCode::SessionBusy,
            "Session is busy",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyLifecycleError::SessionNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Session was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyLifecycleError::SessionDeleted => rejected_completion(
            CommandErrorCode::SessionDeleted,
            "Session is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyLifecycleError::InvalidLifecycleTransition => rejected_completion(
            CommandErrorCode::InvalidArgument,
            "Session lifecycle transition is invalid",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyLifecycleError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyLifecycleError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Session lifecycle update is unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyLifecycleError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_session_metadata_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyMetadataError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyMetadataError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyMetadataError::SessionNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Session was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyMetadataError::StaleRevision => rejected_completion(
            CommandErrorCode::StaleRevision,
            "Session metadata compare-and-swap is stale",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyMetadataError::SessionDeleted => rejected_completion(
            CommandErrorCode::SessionDeleted,
            "Session is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyMetadataError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyMetadataError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Session metadata update is unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyMetadataError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_session_definition_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyWorkspaceDefinitionError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyWorkspaceDefinitionError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyWorkspaceDefinitionError::SessionBusy => rejected_completion(
            CommandErrorCode::SessionBusy,
            "the loaded Session must be Idle to change its Workspace",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::SessionNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Session was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::StaleRevision => rejected_completion(
            CommandErrorCode::StaleRevision,
            "Session definition compare-and-swap is stale",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::SessionArchived => rejected_completion(
            CommandErrorCode::SessionArchived,
            "Session is archived",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::SessionDeleted => rejected_completion(
            CommandErrorCode::SessionDeleted,
            "Session is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::StateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::WorkspaceUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "the Workspace candidate is unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::WorkspaceRejected => rejected_completion(
            CommandErrorCode::Unavailable,
            "the Workspace candidate was rejected",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Session definition update is unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyWorkspaceDefinitionError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_session_agent_upgrade_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyAgentUpgradeError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyAgentUpgradeError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyAgentUpgradeError::SessionBusy => rejected_completion(
            CommandErrorCode::SessionBusy,
            "Session is busy",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyAgentUpgradeError::SessionNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Session was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyAgentUpgradeError::StaleRevision => rejected_completion(
            CommandErrorCode::StaleRevision,
            "Session definition compare-and-swap is stale",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyAgentUpgradeError::SessionArchived => rejected_completion(
            CommandErrorCode::SessionArchived,
            "Session is archived",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyAgentUpgradeError::SessionDeleted => rejected_completion(
            CommandErrorCode::SessionDeleted,
            "Session is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyAgentUpgradeError::AgentMismatch => rejected_completion(
            CommandErrorCode::InvalidArgument,
            "Session Agent upgrade targets another Agent",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyAgentUpgradeError::AgentDisabled => rejected_completion(
            CommandErrorCode::AgentDisabled,
            "Agent is disabled",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyAgentUpgradeError::AgentDeleted => rejected_completion(
            CommandErrorCode::AgentDeleted,
            "Agent is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyAgentUpgradeError::RevisionUnavailable => rejected_completion(
            CommandErrorCode::NotFound,
            "Agent revision is unavailable",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyAgentUpgradeError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "durable state exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyAgentUpgradeError::StorageUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Session Agent upgrade is unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyAgentUpgradeError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_session_workspace_reload_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyWorkspaceReloadError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyWorkspaceReloadError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyWorkspaceReloadError::SessionNotLoaded => rejected_completion(
            CommandErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyWorkspaceReloadError::SessionBusy => rejected_completion(
            CommandErrorCode::SessionBusy,
            "the loaded Session must be Idle to reload its Workspace",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyWorkspaceReloadError::WorkspaceUnavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "the Workspace is unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyWorkspaceReloadError::WorkspaceRejected => rejected_completion(
            CommandErrorCode::ReloadValidationFailed,
            "Workspace reload validation failed",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyWorkspaceReloadError::Unauthorized => rejected_completion(
            CommandErrorCode::Unauthorized,
            "Workspace authority was denied",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyWorkspaceReloadError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
}

fn map_fork_error(
    _command_id: crate::wire::CommandId,
    source_session_id: SessionId,
    error: SessionResidencyForkError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(source_session_id));
    let completion = match error {
        SessionResidencyForkError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyForkError::SourceNotFound => rejected_completion(
            CommandErrorCode::NotFound,
            "Fork source Session was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyForkError::SourceDeleted => rejected_completion(
            CommandErrorCode::SessionDeleted,
            "Fork source Session is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyForkError::InvalidAnchor => rejected_completion(
            CommandErrorCode::InvalidForkAnchor,
            "Fork anchor is invalid for the selected source path",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyForkError::AgentDisabled => rejected_completion(
            CommandErrorCode::AgentDisabled,
            "the source Session Agent is disabled",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyForkError::AgentDeleted => rejected_completion(
            CommandErrorCode::AgentDeleted,
            "the source Session Agent is deleted",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyForkError::SourceConversationTooLarge
        | SessionResidencyForkError::DurableStateTooLarge => rejected_completion(
            CommandErrorCode::DurableStateTooLarge,
            "Fork source exceeds its selected size limit",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyForkError::SourceConversationCorrupt => rejected_completion(
            CommandErrorCode::DurableStateCorrupt,
            "Fork source conversation is corrupt",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyForkError::Unavailable => rejected_completion(
            CommandErrorCode::Unavailable,
            "Session Fork is unavailable",
            retry_with_backoff(),
            subject,
        ),
        SessionResidencyForkError::InternalDispatchUnavailable => {
            return Err(RuntimeDispatchError::InternalDispatchUnavailable);
        }
    };
    Ok(completion)
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
        SessionResidencySubmitError::SessionNotReady(cause) => match cause {
            SessionUnavailableView::WorkspaceUnavailable
            | SessionUnavailableView::PromptUnavailable => rejected_completion(
                CommandErrorCode::SessionNotReady,
                "the Session is not ready to accept Turns",
                RetryAdvice::UserActionRequired,
                subject,
            ),
            SessionUnavailableView::AgentUnavailable | SessionUnavailableView::ModelUnavailable => {
                rejected_completion(
                    CommandErrorCode::SessionNotReady,
                    "the Session is not ready to accept Turns",
                    RetryAdvice::UserActionRequired,
                    subject,
                )
            }
            SessionUnavailableView::RuntimeDependencyUnavailable => rejected_completion(
                CommandErrorCode::SessionNotReady,
                "the Session is not ready to accept Turns",
                retry_with_backoff(),
                subject,
            ),
            SessionUnavailableView::DurableStateCorrupt => rejected_completion(
                CommandErrorCode::DurableStateCorrupt,
                "durable Session state is corrupt",
                RetryAdvice::UserActionRequired,
                subject,
            ),
            SessionUnavailableView::DurableStateTooLarge => rejected_completion(
                CommandErrorCode::DurableStateTooLarge,
                "durable Session state exceeds its selected size limit",
                RetryAdvice::UserActionRequired,
                subject,
            ),
        },
        SessionResidencySubmitError::AgentUnavailable
        | SessionResidencySubmitError::DependencyUnavailable
        | SessionResidencySubmitError::Prompt => rejected_completion(
            CommandErrorCode::Unavailable,
            "Turn dependencies are unavailable",
            retry_with_backoff(),
            subject,
        ),
        // A security-invalidation Preparing Session settles the same public shape as the
        // transient RuntimeDependencyUnavailable cause: SessionNotReady + RetryWithBackoff.
        SessionResidencySubmitError::Preparing => rejected_completion(
            CommandErrorCode::SessionNotReady,
            "the Session is not ready to accept Turns",
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

fn map_interaction_error(
    _command_id: crate::wire::CommandId,
    session_id: SessionId,
    error: SessionResidencyInteractionError,
) -> Result<CommandCompletion, RuntimeDispatchError> {
    let subject = Some(PublicSubject::Session(session_id));
    let completion = match error {
        SessionResidencyInteractionError::Closing => rejected_completion(
            CommandErrorCode::RuntimeClosing,
            "runtime is closing",
            retry_with_backoff(),
            Some(PublicSubject::Runtime),
        ),
        SessionResidencyInteractionError::SessionNotLoaded => rejected_completion(
            CommandErrorCode::SessionNotLoaded,
            "Session is not loaded",
            RetryAdvice::UserActionRequired,
            subject,
        ),
        SessionResidencyInteractionError::ExpectedTurnMismatch => rejected_completion(
            CommandErrorCode::ExpectedTurnMismatch,
            "the Interaction target does not match the active Turn",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyInteractionError::NotFound => rejected_completion(
            CommandErrorCode::InteractionNotFound,
            "Interaction was not found",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyInteractionError::FamilyMismatch => rejected_completion(
            CommandErrorCode::InteractionFamilyMismatch,
            "Interaction resolution family does not match the request",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyInteractionError::InvalidResolution => rejected_completion(
            CommandErrorCode::InvalidArgument,
            "Interaction resolution is invalid",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyInteractionError::AlreadyResolved => rejected_completion(
            CommandErrorCode::InteractionAlreadyResolved,
            "Interaction was already resolved",
            RetryAdvice::RefreshAndRetry,
            subject,
        ),
        SessionResidencyInteractionError::CommandConflict => rejected_completion(
            CommandErrorCode::CommandConflict,
            "Interaction resolution conflicts with an existing command",
            RetryAdvice::DoNotRetry,
            subject,
        ),
        SessionResidencyInteractionError::InternalDispatchUnavailable => {
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
        SessionTurnInterruption::PrepareForUnload => TurnInterruptionView::PrepareForUnload,
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
    use std::num::{NonZeroU32, NonZeroU64};
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::runtime::Handle;
    use tokio::sync::Notify;

    use super::{
        MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError, RuntimeLifecycle,
        map_interaction_error,
    };
    use crate::agent_session_lifecycle::{
        AgentStatus, AgentUsableStatus, ForkAnchor, ForkSourceKind, OptionalTextPatch,
        SealedAgentCreateAttempt, SealedSessionCreateAttempt, SealedSessionLifecycleAttempt,
        SessionMetadataPatch, SessionModelConfig,
    };
    use crate::conversation_storage::{RecordOutcome, RecorderWriteBarrier, SessionHeader};
    use crate::model_gateway::{
        CredentialSource, CredentialSourceFuture, EffectiveModelLimits, ModelCallErrorReason,
        ModelCapabilities, ModelCatalogView, ModelDefinition, ModelDefinitionVersion, ModelGateway,
        ModelGenerationDefaults, ModelProgressPublisher, ModelProviderConfig,
        ModelProviderDescriptor, ModelReasoningSummary, ModelSelection, ModelServiceClass,
        ModelSourceAdapter, ModelSourceFuture, ProviderAdapter, ProviderAttemptFuture,
        ProviderAttemptRequest, ProviderEndpointPolicy, ReasoningCapabilities, ReasoningPreference,
        ScriptedModelFixture, TokenEstimateRate, fixed_credential_source,
    };
    use crate::prompt::{
        AgentPromptSelection, ModelMessageRef, PromptBodyIntent, PromptIntent,
        SessionPromptSelection, TextIntent,
    };
    use crate::runtime_interface::{
        AgentCommand, CommandCompletion, CommandErrorCode, CommandOutcome, CommandRequest,
        CommandResponse, EventFrame, EventRoute, InteractionCommand, ItemContentView,
        NewSessionDefinition, NewSessionMetadata, PublicCancelTarget, QueryResult, RetryAdvice,
        RuntimeCapability, RuntimeCommand, RuntimeDispatchError, RuntimeEventDetail,
        RuntimeLifecycleCommand, RuntimeQuery, RuntimeQueryResult, RuntimeReadQuery,
        RuntimeStateEventKind, SessionCommand, SessionEventDetail, SessionExecutionView,
        SessionLifecycleView, SessionReadinessView, SessionRecordingState, SessionStateEventKind,
        SessionUnavailableView, SessionWorkspaceInvalidationError, SnapshotRequest,
        SnapshotResponse, StateEventMsg, SubscriptionRequest, SubscriptionScope, TurnCommand,
        TurnExecutionPhaseView, TurnFailureView, TurnTerminalView,
    };
    use crate::runtime_task::RuntimeTaskError;
    use crate::session_execution::SessionExecutionState;
    use crate::session_residency::{
        SessionResidencyInteractionError, SessionResidencyLifecycleError,
        SessionResidencyLoadError, SessionResidencyLoadOutcome, SessionResidencyUnloadOutcome,
    };
    use crate::tools::{ToolApprovalDecisionInput, UserQuestionAnswer, UserQuestionFieldAnswer};
    use crate::turn_item_interaction::InteractionResolutionInput;
    use crate::wire::conversation_jsonl::ConversationLineCodec;
    use crate::wire::{
        AgentId, CanonicalFileUri, CommandId, FileUriFamily, InteractionResolutionKey, ItemId,
        RequestId, SessionId, SessionMetadataRevision, TurnId,
    };
    use crate::workspace::{
        RequestedFilesystemAccess, Workspace, WorkspaceAccessError, WorkspaceCwdSpec,
        WorkspaceDefinitionInput, WorkspacePathTarget, WorkspaceRootInput, WorkspaceRootKey,
        WorkspaceSourcePolicy, lower_workspace,
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

    async fn create_and_load_public_session_with_agent(
        runtime: &MiniCoreRuntime,
        workspace_root: &Path,
    ) -> (SessionId, AgentId) {
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
        (session_id, agent_id)
    }

    async fn create_and_load_public_session(
        runtime: &MiniCoreRuntime,
        workspace_root: &Path,
    ) -> SessionId {
        create_and_load_public_session_with_agent(runtime, workspace_root)
            .await
            .0
    }

    /// A test-only `ModelSourceAdapter` that always discovers one fixed "openai/gpt-5"
    /// definition.  Its provider is a stub that is never executed (the shared-resource reload
    /// happy path runs no Turn), so a Runtime opened with these resources can reload shared
    /// resources while every loaded Session keeps its model fact available and no readiness
    /// event is emitted.
    struct FixedModelSource {
        definition: ModelDefinition,
    }

    impl ModelSourceAdapter for FixedModelSource {
        fn discover(&self) -> ModelSourceFuture<'_> {
            let definition = self.definition.clone();
            Box::pin(async move { Ok(vec![definition]) })
        }
    }

    struct ReloadOnlyStubProvider;

    impl ProviderAdapter for ReloadOnlyStubProvider {
        fn execute(
            &self,
            _request: ProviderAttemptRequest,
            _progress: ModelProgressPublisher,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> ProviderAttemptFuture<'_> {
            Box::pin(async move {
                unreachable!("the shared-resource reload stub provider is never executed")
            })
        }
    }

    async fn fixed_model_resources() -> (Arc<ModelGateway>, Arc<ModelCatalogView>) {
        let definition = ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            "gpt-5".parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192)),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1)
                .expect("the fixed token estimate rate validates"),
            ModelGenerationDefaults::new(
                NonZeroU32::new(4_096).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            Arc::new(ReloadOnlyStubProvider),
            fixed_credential_source("test-credential"),
        )
        .unwrap_or_else(|_| panic!("the fixed model definition validates"));
        let gateway = Arc::new(ModelGateway::new(vec![Arc::new(FixedModelSource {
            definition,
        })]));
        let catalog = gateway
            .initialize()
            .await
            .unwrap_or_else(|_| panic!("the fixed model catalog initializes"));
        (gateway, catalog)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_subscription_publishes_changed_session_membership_once() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(Vec::<&str>::new());
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the Runtime opens");
        let agent_id = create_runtime_agent(&runtime).await;
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
            .await
            .expect("the Runtime subscription opens");
        let Some(EventFrame::Snapshot(SnapshotResponse::Runtime(initial))) = events.recv().await
        else {
            panic!("the Runtime subscription starts with its snapshot");
        };
        assert!(initial.loaded_sessions().is_empty());

        let create_id = CommandId::generate().unwrap();
        let create = runtime
            .dispatch(CommandRequest::new(
                create_id,
                RuntimeCommand::Session(SessionCommand::Create {
                    agent_id,
                    definition: Box::new(NewSessionDefinition::new(
                        workspace_input(workspace.path()),
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
            .expect("Create dispatches");
        let session_id = command_output(&create).parse().unwrap();
        let created = events.recv().await.expect("Create publishes");
        let EventFrame::State(created) = created else {
            panic!("Create publishes a StateEvent");
        };
        let StateEventMsg::Runtime {
            kind,
            snapshot,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
        } = created.msg()
        else {
            panic!("SessionCreated carries one safe Session summary");
        };
        assert_eq!(*kind, RuntimeStateEventKind::SessionCreated);
        assert_eq!(created.command_id(), Some(create_id));
        assert_eq!(session.session_id(), session_id);
        assert!(!session.forked());
        assert!(snapshot.loaded_sessions().is_empty());

        let load_id = CommandId::generate().unwrap();
        runtime
            .dispatch(CommandRequest::new(
                load_id,
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("Load dispatches");
        let loaded = events.recv().await.expect("Load publishes");
        let EventFrame::State(loaded) = loaded else {
            panic!("Load publishes a StateEvent");
        };
        let StateEventMsg::Runtime {
            kind,
            snapshot,
            detail: None,
        } = loaded.msg()
        else {
            panic!("SessionLoaded has no durable catalog detail");
        };
        assert_eq!(*kind, RuntimeStateEventKind::SessionLoaded);
        assert_eq!(loaded.command_id(), Some(load_id));
        assert_eq!(snapshot.loaded_sessions()[0].session_id(), session_id);

        runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("idempotent Load dispatches");

        let unload_id = CommandId::generate().unwrap();
        runtime
            .dispatch(CommandRequest::new(
                unload_id,
                RuntimeCommand::Session(SessionCommand::Unload { session_id }),
            ))
            .await
            .expect("Unload dispatches");
        let unloaded = events.recv().await.expect("Unload publishes");
        let EventFrame::State(unloaded) = unloaded else {
            panic!("Unload publishes a StateEvent");
        };
        let StateEventMsg::Runtime {
            kind,
            snapshot,
            detail: None,
        } = unloaded.msg()
        else {
            panic!("SessionUnloaded has no durable catalog detail");
        };
        assert_eq!(*kind, RuntimeStateEventKind::SessionUnloaded);
        assert_eq!(unloaded.command_id(), Some(unload_id));
        assert!(snapshot.loaded_sessions().is_empty());

        runtime.shutdown().await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("Runtime closing settles the event stream"),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_session_lifecycle_commands_publish_only_real_changes() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the Runtime opens");
        let agent_id = create_runtime_agent(&runtime).await;
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
            .await
            .expect("the Runtime subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
        ));

        let create = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Create {
                    agent_id,
                    definition: Box::new(NewSessionDefinition::new(
                        workspace_input(workspace.path()),
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
            .expect("Create dispatches");
        let session_id = command_output(&create).parse().unwrap();
        assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

        let archive_id = CommandId::generate().unwrap();
        let archive = runtime
            .dispatch(CommandRequest::new(
                archive_id,
                RuntimeCommand::Session(SessionCommand::Archive { session_id }),
            ))
            .await
            .expect("Archive dispatches");
        assert!(matches!(
            archive.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionArchived,
                output: None,
            }
        ));
        let Some(EventFrame::State(archived)) = events.recv().await else {
            panic!("Archive publishes one StateEvent");
        };
        let StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionArchived,
            snapshot,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
        } = archived.msg()
        else {
            panic!("Archive publishes the changed Session summary");
        };
        assert_eq!(archived.command_id(), Some(archive_id));
        assert!(snapshot.loaded_sessions().is_empty());
        assert_eq!(session.session_id(), session_id);
        assert_eq!(session.lifecycle(), SessionLifecycleView::Archived);

        let repeated_archive = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Archive { session_id }),
            ))
            .await
            .expect("repeated Archive dispatches");
        assert!(matches!(
            repeated_archive.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::NoChange,
                output: None,
            }
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "NoChange does not publish a second event"
        );

        let unarchive_id = CommandId::generate().unwrap();
        let unarchive = runtime
            .dispatch(CommandRequest::new(
                unarchive_id,
                RuntimeCommand::Session(SessionCommand::Unarchive { session_id }),
            ))
            .await
            .expect("Unarchive dispatches");
        assert!(matches!(
            unarchive.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionUnarchived,
                output: None,
            }
        ));
        let Some(EventFrame::State(unarchived)) = events.recv().await else {
            panic!("Unarchive publishes one StateEvent");
        };
        let StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionUnarchived,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } = unarchived.msg()
        else {
            panic!("Unarchive publishes the changed Session summary");
        };
        assert_eq!(unarchived.command_id(), Some(unarchive_id));
        assert_eq!(session.lifecycle(), SessionLifecycleView::Open);

        let repeated_unarchive = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Unarchive { session_id }),
            ))
            .await
            .expect("repeated Unarchive dispatches");
        assert!(matches!(
            repeated_unarchive.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::NoChange,
                output: None,
            }
        ));

        runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("Load dispatches");
        assert!(matches!(events.recv().await, Some(EventFrame::State(_))));
        let busy_archive = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Archive { session_id }),
            ))
            .await
            .expect("loaded Archive is a typed rejection");
        assert!(matches!(
            busy_archive.completion(),
            CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::SessionBusy
        ));
        runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Unload { session_id }),
            ))
            .await
            .expect("Unload dispatches");
        assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

        let open_delete = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Delete { session_id }),
            ))
            .await
            .expect("Open Delete is a typed rejection");
        assert!(matches!(
            open_delete.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::InvalidArgument
        ));

        runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Archive { session_id }),
            ))
            .await
            .expect("Archive before Delete dispatches");
        assert!(matches!(events.recv().await, Some(EventFrame::State(_))));
        let delete_id = CommandId::generate().unwrap();
        let delete = runtime
            .dispatch(CommandRequest::new(
                delete_id,
                RuntimeCommand::Session(SessionCommand::Delete { session_id }),
            ))
            .await
            .expect("Delete dispatches");
        assert!(matches!(
            delete.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionDeleted,
                output: None,
            }
        ));
        let Some(EventFrame::State(deleted)) = events.recv().await else {
            panic!("Delete publishes one StateEvent");
        };
        let StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionDeleted,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } = deleted.msg()
        else {
            panic!("Delete publishes the changed Session summary");
        };
        assert_eq!(deleted.command_id(), Some(delete_id));
        assert_eq!(session.lifecycle(), SessionLifecycleView::Deleted);

        let repeated_delete = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Delete { session_id }),
            ))
            .await
            .expect("repeated Delete dispatches");
        assert!(matches!(
            repeated_delete.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionDeleted,
                output: None,
            }
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "already Deleted does not publish a second event"
        );

        runtime.shutdown().await;
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
            capabilities.data()
        else {
            panic!("the capability query returns Runtime capabilities");
        };
        assert_eq!(
            capabilities.values(),
            [
                RuntimeCapability::StateEvents,
                RuntimeCapability::RuntimeSnapshot,
                RuntimeCapability::SessionSnapshot,
                RuntimeCapability::PagedQueries,
                RuntimeCapability::InteractionResolution,
                RuntimeCapability::SessionFork,
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
    async fn public_fork_selects_live_or_recorded_source_and_reopens_each_child() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["fork source answer"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let source_session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session {
                    session_id: source_session_id,
                },
                false,
            ))
            .await
            .expect("the source subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));
        let residency = runtime.inner.residency().unwrap();
        let executor = residency.executor_for_test(source_session_id).unwrap();
        let hooks = executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        drop(executor);
        drop(residency);

        let submit = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id: source_session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("fork this turn").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the source Turn starts");
        let source_turn_id = started_turn(&submit);
        hooks.wait_before_agent_run_attempt().await;

        let SnapshotResponse::Session(source_snapshot) = runtime
            .snapshot(SnapshotRequest::Session {
                session_id: source_session_id,
            })
            .await
            .expect("the running source snapshot is available")
        else {
            panic!("the source snapshot is a Session snapshot");
        };
        let user_item_id = source_snapshot
            .active_items()
            .iter()
            .find(|item| matches!(item.content(), ItemContentView::UserMessage { .. }))
            .expect("the source snapshot exposes its User message")
            .item_id();
        assert_eq!(
            source_turn_id,
            source_snapshot.current_turn().unwrap().turn_id()
        );

        let live_fork = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Fork {
                    source_session_id,
                    anchor: ForkAnchor::AfterUserMessage {
                        item_id: user_item_id,
                    },
                }),
            ))
            .await
            .expect("the loaded source Fork dispatches");
        let live_child = match live_fork.completion() {
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::SessionForked {
                        session_id,
                        source: ForkSourceKind::LiveSnapshot,
                    },
                output: None,
            } => *session_id,
            completion => panic!("unexpected live Fork completion: {completion:?}"),
        };

        let invalid = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Fork {
                    source_session_id,
                    anchor: ForkAnchor::BeforeFinalAgentMessage {
                        item_id: user_item_id,
                    },
                }),
            ))
            .await
            .expect("the invalid Fork anchor dispatches");
        assert!(matches!(
            invalid.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::InvalidForkAnchor
        ));

        hooks.release_before_agent_run_attempt();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the source Turn reaches terminal state")
            .expect("the source subscription remains open");
        assert!(matches!(
            terminal,
            EventFrame::State(event)
                if event.msg().session_kind() == Some(SessionStateEventKind::TurnCompleted)
        ));

        runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Unload {
                    session_id: source_session_id,
                }),
            ))
            .await
            .expect("the source unloads before the recorded Fork");
        let recorded_fork = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Fork {
                    source_session_id,
                    anchor: ForkAnchor::AfterUserMessage {
                        item_id: user_item_id,
                    },
                }),
            ))
            .await
            .expect("the unloaded source Fork dispatches");
        let recorded_child = match recorded_fork.completion() {
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::SessionForked {
                        session_id,
                        source: ForkSourceKind::RecordedHistory,
                    },
                output: None,
            } => *session_id,
            completion => panic!("unexpected recorded Fork completion: {completion:?}"),
        };
        assert_ne!(live_child, recorded_child);

        runtime.shutdown().await;
        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the Fork children reopen");
        for child in [live_child, recorded_child] {
            let loaded = reopened
                .dispatch(CommandRequest::new(
                    CommandId::generate().unwrap(),
                    RuntimeCommand::Session(SessionCommand::Load { session_id: child }),
                ))
                .await
                .expect("a reopened Fork child loads");
            assert_eq!(command_output(&loaded), "session loaded");
            let SnapshotResponse::Session(snapshot) = reopened
                .snapshot(SnapshotRequest::Session { session_id: child })
                .await
                .expect("a reopened Fork child has a public snapshot")
            else {
                panic!("the child snapshot is a Session snapshot");
            };
            assert_eq!(snapshot.execution(), SessionExecutionView::Idle);
            assert_eq!(snapshot.current_turn(), None);
        }
        reopened.shutdown().await;
    }

    #[test]
    fn interaction_error_mapping_is_typed_and_deterministic() {
        let command_id: CommandId = "cmd_11111111111111111111111111111111".parse().unwrap();
        let session_id: SessionId = "ses_22222222222222222222222222222222".parse().unwrap();
        let cases = [
            (
                SessionResidencyInteractionError::SessionNotLoaded,
                CommandErrorCode::SessionNotLoaded,
                RetryAdvice::UserActionRequired,
            ),
            (
                SessionResidencyInteractionError::ExpectedTurnMismatch,
                CommandErrorCode::ExpectedTurnMismatch,
                RetryAdvice::RefreshAndRetry,
            ),
            (
                SessionResidencyInteractionError::NotFound,
                CommandErrorCode::InteractionNotFound,
                RetryAdvice::RefreshAndRetry,
            ),
            (
                SessionResidencyInteractionError::FamilyMismatch,
                CommandErrorCode::InteractionFamilyMismatch,
                RetryAdvice::DoNotRetry,
            ),
            (
                SessionResidencyInteractionError::InvalidResolution,
                CommandErrorCode::InvalidArgument,
                RetryAdvice::DoNotRetry,
            ),
            (
                SessionResidencyInteractionError::AlreadyResolved,
                CommandErrorCode::InteractionAlreadyResolved,
                RetryAdvice::RefreshAndRetry,
            ),
            (
                SessionResidencyInteractionError::CommandConflict,
                CommandErrorCode::CommandConflict,
                RetryAdvice::DoNotRetry,
            ),
        ];

        for (source, code, retry) in cases {
            let completion = map_interaction_error(command_id, session_id, source).unwrap();
            assert!(matches!(
                completion,
                CommandCompletion::Rejected(error)
                    if error.code() == code
                        && error.retry() == retry
                        && error.subject()
                            == Some(&crate::runtime_interface::PublicSubject::Session(session_id))
            ));
        }

        let closing = map_interaction_error(
            command_id,
            session_id,
            SessionResidencyInteractionError::Closing,
        )
        .unwrap();
        assert!(matches!(
            closing,
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::RuntimeClosing
                    && error.subject()
                        == Some(&crate::runtime_interface::PublicSubject::Runtime)
        ));
        assert_eq!(
            map_interaction_error(
                command_id,
                session_id,
                SessionResidencyInteractionError::InternalDispatchUnavailable,
            ),
            Err(RuntimeDispatchError::InternalDispatchUnavailable)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_interaction_command_routes_to_typed_session_errors() {
        let root = TempRoot::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the Runtime opens");
        let session_id: SessionId = "ses_22222222222222222222222222222222".parse().unwrap();
        let response = runtime
            .dispatch(CommandRequest::new(
                "cmd_55555555555555555555555555555555".parse().unwrap(),
                RuntimeCommand::Interaction(InteractionCommand::Resolve {
                    session_id,
                    expected_turn_id: "trn_33333333333333333333333333333333".parse().unwrap(),
                    item_id: "itm_88888888888888888888888888888888"
                        .parse::<ItemId>()
                        .unwrap(),
                    request_id: "req_66666666666666666666666666666666"
                        .parse::<RequestId>()
                        .unwrap(),
                    resolution: InteractionResolutionInput::ToolApproval(
                        ToolApprovalDecisionInput::Deny,
                    ),
                    resolution_key: "irk_77777777777777777777777777777777"
                        .parse::<InteractionResolutionKey>()
                        .unwrap(),
                }),
            ))
            .await
            .expect("the Interaction command returns a typed completion");
        assert!(matches!(
            response.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::SessionNotLoaded
                    && error.subject()
                        == Some(&crate::runtime_interface::PublicSubject::Session(session_id))
        ));
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
    async fn recorder_failure_live_fork_keeps_unrecorded_tail_while_reload_uses_recorded_prefix() {
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
        let final_agent_item_id = {
            let live = super::lock(&live_state);
            let views = live.capture_conversation_views().unwrap();
            assert_eq!(views.conversation().messages().len(), 2);
            views
                .relations()
                .iter()
                .find(|relation| {
                    relation.family()
                        == crate::turn_item_interaction::ItemContentFamily::AgentMessage
                })
                .expect("the completed live Turn retains its final Agent message relation")
                .item_id()
        };
        drop(live_state);
        drop(executor);
        drop(residency);

        let forked = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Fork {
                    source_session_id: session_id,
                    anchor: ForkAnchor::AfterFinalAgentMessage {
                        item_id: final_agent_item_id,
                    },
                }),
            ))
            .await
            .expect("a degraded loaded Session still Forks from its live tail");
        let live_child = match forked.completion() {
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::SessionForked {
                        session_id,
                        source: ForkSourceKind::LiveSnapshot,
                    },
                output: None,
            } => *session_id,
            completion => panic!("unexpected degraded live Fork completion: {completion:?}"),
        };
        runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load {
                    session_id: live_child,
                }),
            ))
            .await
            .expect("the degraded live Fork child loads");
        let residency = runtime.inner.residency().unwrap();
        let child_executor = residency.executor_for_test(live_child).unwrap();
        let child_live_state = child_executor.live_state_for_test().unwrap();
        assert_eq!(
            super::lock(&child_live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            2,
            "LiveSnapshot preserves both unrecorded source messages"
        );
        drop(child_live_state);
        drop(child_executor);
        drop(residency);
        runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Unload {
                    session_id: live_child,
                }),
            ))
            .await
            .expect("the verified live Fork child unloads");

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
        // A short but non-zero graceful-Unload grace: the in-flight admission is allowed to
        // settle within 20ms, and only then does the graceful preparation fail it closed.
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned())
                .with_unload_grace(std::time::Duration::from_millis(20)),
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
        // Do not wait for the executor closing token: the graceful preparation only closes the
        // executor after the in-flight admission settles, so that wait would deadlock against
        // the held admission hook.  Instead sleep well past the 20ms grace (the preparation
        // deadline fires and the admission gate stays closed) and then release the hook so the
        // losing admission can observe the closed gate and settle.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        hooks.release_after_agent_admission_before_input();

        let (submit, unload) = tokio::join!(submit, unload);
        let submit = submit.expect("the losing Submit receives a domain completion");
        // The graceful Unload won the Session, so the losing Submit is a per-Session
        // SessionNotLoaded + UserActionRequired rejection, never a RuntimeClosing backoff.
        assert!(matches!(
            submit.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::SessionNotLoaded
                    && error.retry() == RetryAdvice::UserActionRequired
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
    async fn public_agent_disable_and_enable_cycles_session_readiness_and_submit() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["scripted post-enable answer"]);
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the scripted Runtime opens");
        let (session_id, agent_id) =
            create_and_load_public_session_with_agent(&runtime, workspace.path()).await;

        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("the Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(snapshot)))
                if snapshot.readiness() == SessionReadinessView::Ready
        ));

        // Disable the Agent: the loaded Idle Session publishes exactly one ReadinessChanged
        // event into the Unavailable(AgentUnavailable) projection.
        let disable_id = CommandId::generate().unwrap();
        let disabled = runtime
            .dispatch(CommandRequest::new(
                disable_id,
                RuntimeCommand::Agent(AgentCommand::SetStatus {
                    agent_id,
                    expected_status: AgentStatus::Enabled,
                    status: AgentUsableStatus::Disabled,
                }),
            ))
            .await
            .expect("Agent Disable dispatches");
        assert!(matches!(
            disabled.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::AgentStatusChanged {
                    status: AgentStatus::Disabled,
                },
                output: None,
            }
        ));
        let Some(EventFrame::State(disabled_event)) = events.recv().await else {
            panic!("Disable publishes one Session StateEvent");
        };
        assert_eq!(disabled_event.command_id(), Some(disable_id));
        assert_eq!(
            disabled_event.msg().session_kind(),
            Some(SessionStateEventKind::SessionReadinessChanged)
        );
        let disabled_snapshot = disabled_event
            .msg()
            .session_snapshot()
            .expect("readiness events carry a Session snapshot");
        assert_eq!(
            disabled_snapshot.readiness(),
            SessionReadinessView::Unavailable(SessionUnavailableView::AgentUnavailable)
        );
        assert_eq!(disabled_snapshot.execution(), SessionExecutionView::Idle);
        assert!(disabled_snapshot.queues().submit_admissions().is_empty());
        assert!(!disabled_snapshot.queues().accepting_input());

        // While Disabled, a public Submit is rejected SessionNotReady + UserActionRequired and
        // never forms a Turn or reaches the model.
        let rejected = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("must wait for enable").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the disabled Session Submit settles");
        assert!(matches!(
            rejected.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::SessionNotReady
                    && error.retry() == RetryAdvice::UserActionRequired
        ));
        assert_eq!(model.request_count(), 0);

        // Enable restores Ready with one matching ReadinessChanged event.
        let enable_id = CommandId::generate().unwrap();
        let enabled = runtime
            .dispatch(CommandRequest::new(
                enable_id,
                RuntimeCommand::Agent(AgentCommand::SetStatus {
                    agent_id,
                    expected_status: AgentStatus::Disabled,
                    status: AgentUsableStatus::Enabled,
                }),
            ))
            .await
            .expect("Agent Enable dispatches");
        assert!(matches!(
            enabled.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::AgentStatusChanged {
                    status: AgentStatus::Enabled,
                },
                output: None,
            }
        ));
        let Some(EventFrame::State(enabled_event)) = events.recv().await else {
            panic!("Enable publishes one Session StateEvent");
        };
        assert_eq!(enabled_event.command_id(), Some(enable_id));
        assert_eq!(
            enabled_event.msg().session_kind(),
            Some(SessionStateEventKind::SessionReadinessChanged)
        );
        let enabled_snapshot = enabled_event
            .msg()
            .session_snapshot()
            .expect("readiness events carry a Session snapshot");
        assert_eq!(enabled_snapshot.readiness(), SessionReadinessView::Ready);
        assert_eq!(enabled_snapshot.execution(), SessionExecutionView::Idle);
        assert!(enabled_snapshot.queues().accepting_input());

        // The Disable/Enable pair publishes exactly one readiness event each: no second event
        // settles after the two.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "Disable/Enable publishes exactly one readiness event each"
        );

        // The restored Session accepts a fresh Submit and runs the scripted model.
        let submit_command_id = CommandId::generate().unwrap();
        let submit = runtime
            .dispatch(CommandRequest::new(
                submit_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("hello after enable").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the restored Session accepts Submit");
        let turn_id = started_turn(&submit);
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(EventFrame::State(event))
                        if event.msg().session_kind()
                            == Some(SessionStateEventKind::TurnCompleted) =>
                    {
                        return event;
                    }
                    Some(EventFrame::State(_)) => continue,
                    other => panic!("unexpected frame while draining to TurnCompleted: {other:?}"),
                }
            }
        })
        .await
        .expect("the restored Session Turn reaches terminal state");
        assert_eq!(terminal.command_id(), Some(submit_command_id));
        assert!(matches!(
            terminal.msg().session_detail(),
            Some(SessionEventDetail::TurnTerminal {
                turn_id: completed_turn,
                terminal: TurnTerminalView::Completed { .. },
            }) if completed_turn == turn_id
        ));
        assert_eq!(model.request_count(), 1);

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_workspace_authority_invalidation_recovers_an_idle_session() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        // A model fixture keeps the model catalog non-empty, so the Session pinned to
        // "openai/gpt-5" has genuine Ready readiness both initially and after the host-only
        // recovery (an empty catalog would leave the final readiness ModelUnavailable).
        let model = ScriptedModelFixture::new(Vec::<&str>::new());
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            &model,
        )
        .await
        .expect("the Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;

        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("the Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(snapshot)))
                if snapshot.readiness() == SessionReadinessView::Ready
        ));

        // The host-only seam (no wire command) recovers the Idle Session; it resolves only
        // after the final readiness state is installed.
        runtime
            .invalidate_session_workspace_authority(session_id)
            .await
            .expect("the host-only invalidation seam recovers the Idle Session");

        // Even when the recovery completes before the await returns, the buffered Session
        // stream must deliver the Preparing start event and the final Ready event in order,
        // both with no CommandId, and never a WorkspaceReloaded event.
        let mut readiness_events = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while readiness_events.len() < 2 {
                let Some(EventFrame::State(event)) = events.recv().await else {
                    panic!("the Session stream stays open through the recovery");
                };
                assert_ne!(
                    event.msg().session_kind(),
                    Some(SessionStateEventKind::SessionWorkspaceReloaded),
                    "the host-only invalidation never publishes WorkspaceReloaded"
                );
                if event.msg().session_kind()
                    == Some(SessionStateEventKind::SessionReadinessChanged)
                {
                    readiness_events.push(event);
                }
            }
        })
        .await
        .expect("the recovery publishes both readiness events");

        let (preparing, ready) = (&readiness_events[0], &readiness_events[1]);
        assert_eq!(preparing.command_id(), None);
        assert_eq!(ready.command_id(), None);
        let preparing_snapshot = preparing
            .msg()
            .session_snapshot()
            .expect("readiness events carry a Session snapshot");
        assert_eq!(
            preparing_snapshot.readiness(),
            SessionReadinessView::Preparing
        );
        assert_eq!(preparing_snapshot.execution(), SessionExecutionView::Idle);
        assert!(preparing_snapshot.queues().submit_admissions().is_empty());
        assert!(preparing_snapshot.queues().steers().is_empty());
        assert!(preparing_snapshot.queues().follow_ups().is_empty());
        assert!(!preparing_snapshot.queues().accepting_input());
        let ready_snapshot = ready
            .msg()
            .session_snapshot()
            .expect("readiness events carry a Session snapshot");
        assert_eq!(ready_snapshot.readiness(), SessionReadinessView::Ready);
        assert_eq!(ready_snapshot.execution(), SessionExecutionView::Idle);
        assert!(ready_snapshot.queues().accepting_input());

        // The seam already installed the final Ready snapshot before it returned.
        let SnapshotResponse::Session(snapshot) = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("the recovered Session snapshot is available")
        else {
            panic!("the recovered snapshot is a Session snapshot");
        };
        assert_eq!(snapshot.readiness(), SessionReadinessView::Ready);
        assert_eq!(snapshot.execution(), SessionExecutionView::Idle);

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_reload_shared_resources_publishes_one_runtime_event_and_no_session_noise() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        // A sourced model gateway: its reload candidate always contains the exact "openai/gpt-5"
        // definition the Session pins, so the happy-path reload leaves the loaded Session
        // genuinely unchanged (still Ready) and publishes no fake readiness event.
        let (gateway, catalog) = fixed_model_resources().await;
        let runtime = MiniCoreRuntime::open_with_model_resources(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
            Some((gateway, catalog)),
        )
        .await
        .expect("the sourced Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;

        let mut runtime_events = runtime
            .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
            .await
            .expect("the Runtime subscription opens");
        assert!(matches!(
            runtime_events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
        ));
        let mut session_events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("the Session subscription opens");
        assert!(matches!(
            session_events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));

        let reload_id = CommandId::generate().unwrap();
        let reload = runtime
            .dispatch(CommandRequest::new(
                reload_id,
                RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources),
            ))
            .await
            .expect("ReloadSharedResources dispatches");
        assert!(matches!(
            reload.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SharedResourcesReloaded,
                output: None,
            }
        ));

        let Some(EventFrame::State(reloaded)) = runtime_events.recv().await else {
            panic!("ReloadSharedResources publishes one Runtime StateEvent");
        };
        assert_eq!(reloaded.command_id(), Some(reload_id));
        let StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SharedResourcesReloaded,
            ..
        } = reloaded.msg()
        else {
            panic!("the Runtime event kind is SharedResourcesReloaded");
        };

        // The unchanged loaded Session stays Ready and publishes no fake readiness event, and
        // the Runtime stream settles with exactly the one SharedResourcesReloaded event.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), session_events.recv())
                .await
                .is_err(),
            "an unchanged loaded Session publishes no readiness event on shared-resource reload"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), runtime_events.recv())
                .await
                .is_err(),
            "ReloadSharedResources publishes exactly one Runtime event"
        );

        let SnapshotResponse::Session(snapshot) = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("the loaded Session snapshot remains available")
        else {
            panic!("the snapshot is a Session snapshot");
        };
        assert_eq!(snapshot.readiness(), SessionReadinessView::Ready);
        assert_eq!(snapshot.execution(), SessionExecutionView::Idle);

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
        let execution_changed =
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("the Finishing execution event is published")
                .expect("the subscription remains open");
        assert!(matches!(
            execution_changed,
            EventFrame::State(event)
                if event.route() == EventRoute::Session { session_id }
                    && event.msg().session_kind()
                        == Some(SessionStateEventKind::SessionExecutionChanged)
                    && event.msg().session_snapshot().is_some_and(|snapshot|
                        snapshot.execution() == SessionExecutionView::Finishing)
        ));
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

    /// A credential source that always reports a missing credential. Model
    /// availability must never depend on it.
    struct MissingCredentialSource;

    impl CredentialSource for MissingCredentialSource {
        fn resolve(&self) -> CredentialSourceFuture<'_> {
            Box::pin(async { None })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_without_provider_config_opens_an_empty_model_catalog() {
        let root = TempRoot::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the Runtime opens without provider config");
        let (model_gateway, model_catalog) = runtime.inner.model_resources();
        assert_eq!(
            model_catalog.definition_count(),
            0,
            "no provider config keeps the existing empty-catalog behavior"
        );
        assert_eq!(
            model_gateway
                .build_reload_candidate()
                .await
                .expect("the empty Model catalog candidate rebuilds")
                .definition_count(),
            0
        );
        runtime.shutdown().await;
    }

    #[test]
    fn runtime_config_ask_user_tool_is_default_off_and_the_opt_in_is_idempotent() {
        let default = MiniCoreRuntimeConfig::new(PathBuf::from("/unused"));
        assert!(
            !default.ask_user_tool,
            "the default Runtime ToolSet stays empty"
        );
        let opted = default.with_ask_user_tool();
        assert!(opted.ask_user_tool);
        // The opt-in is idempotent: opting in twice keeps the exact same flag.
        let twice = opted.with_ask_user_tool();
        assert!(twice.ask_user_tool);
        // A fresh default config is unaffected, and the redacted Debug shape is unchanged
        // by the new field.
        let fresh = MiniCoreRuntimeConfig::new(PathBuf::from("/unused"));
        assert!(!fresh.ask_user_tool);
        assert_eq!(format!("{fresh:?}"), "MiniCoreRuntimeConfig { .. }");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_open_keeps_tools_default_off_and_double_opt_in_discloses_one_builtin() {
        for (enabled, expected_tools) in [(false, 0_usize), (true, 1_usize)] {
            let root = TempRoot::new();
            let workspace = TempWorkspace::new();
            let model = ScriptedModelFixture::new(vec!["final answer"]);
            let mut config = MiniCoreRuntimeConfig::new(root.path().to_owned());
            if enabled {
                config = config.with_ask_user_tool().with_ask_user_tool();
            }
            let runtime =
                MiniCoreRuntime::open_with_model_fixture(config, Handle::current(), &model)
                    .await
                    .expect("the Runtime opens with the selected ToolSet");
            let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
            let mut events = runtime
                .subscribe(SubscriptionRequest::new(
                    SubscriptionScope::Session { session_id },
                    false,
                ))
                .await
                .expect("the Session subscription opens");
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
                            PromptBodyIntent::Text(TextIntent::new("hello").unwrap()),
                            Vec::new(),
                        )
                        .unwrap(),
                    }),
                ))
                .await
                .expect("the ordinary Turn starts");
            let turn_id = started_turn(&submit);
            let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match events.recv().await {
                        Some(EventFrame::State(event))
                            if event.msg().session_kind()
                                == Some(SessionStateEventKind::TurnCompleted) =>
                        {
                            break event;
                        }
                        Some(EventFrame::State(_)) => continue,
                        other => panic!("unexpected frame before ordinary completion: {other:?}"),
                    }
                }
            })
            .await
            .expect("the ordinary Turn completes");
            assert!(matches!(
                terminal.msg().session_detail(),
                Some(SessionEventDetail::TurnTerminal {
                    turn_id: completed_turn,
                    terminal: TurnTerminalView::Completed { .. },
                }) if completed_turn == turn_id
            ));

            let requests = model.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].input().tools().len(), expected_tools);
            if enabled {
                assert_eq!(requests[0].input().tools()[0].name().as_str(), "ask_user");
            }
            runtime.shutdown().await;
        }
    }

    #[test]
    fn runtime_config_read_file_tool_is_default_off_and_the_opt_in_is_idempotent_and_independent() {
        let default = MiniCoreRuntimeConfig::new(PathBuf::from("/unused"));
        assert!(
            !default.read_file_tool,
            "the default Runtime ToolSet stays empty"
        );
        let opted = default.with_read_file_tool();
        assert!(opted.read_file_tool);
        // The opt-in is idempotent: opting in twice keeps the exact same flag.
        let twice = opted.with_read_file_tool();
        assert!(twice.read_file_tool);
        // The read_file opt-in is independent of ask_user in both directions.
        let read_only = MiniCoreRuntimeConfig::new(PathBuf::from("/unused")).with_read_file_tool();
        assert!(
            !read_only.ask_user_tool,
            "the read_file opt-in never enables ask_user"
        );
        let ask_only = MiniCoreRuntimeConfig::new(PathBuf::from("/unused")).with_ask_user_tool();
        assert!(
            !ask_only.read_file_tool,
            "the ask_user opt-in never enables read_file"
        );
        let both = ask_only.with_read_file_tool();
        assert!(both.ask_user_tool && both.read_file_tool);
        // A fresh default config is unaffected, and the redacted Debug shape is unchanged
        // by the new field.
        let fresh = MiniCoreRuntimeConfig::new(PathBuf::from("/unused"));
        assert!(!fresh.read_file_tool);
        assert_eq!(format!("{fresh:?}"), "MiniCoreRuntimeConfig { .. }");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_open_tool_opt_ins_disclose_exactly_the_opted_builtins() {
        // The frozen production Tool config discloses exactly the opted builtins once: the
        // default stays empty, each single opt-in discloses its one builtin, and the
        // combined opt-in discloses exactly ask_user then read_file (never twice, never
        // any unopted builtin).
        for (ask_user, read_file, expected_names) in [
            (false, false, Vec::<&str>::new()),
            (true, false, vec!["ask_user"]),
            (false, true, vec!["read_file"]),
            (true, true, vec!["ask_user", "read_file"]),
        ] {
            let root = TempRoot::new();
            let workspace = TempWorkspace::new();
            let model = ScriptedModelFixture::new(vec!["final answer"]);
            let mut config = MiniCoreRuntimeConfig::new(root.path().to_owned());
            if ask_user {
                config = config.with_ask_user_tool().with_ask_user_tool();
            }
            if read_file {
                config = config.with_read_file_tool().with_read_file_tool();
            }
            let runtime =
                MiniCoreRuntime::open_with_model_fixture(config, Handle::current(), &model)
                    .await
                    .expect("the Runtime opens with the selected Tool config");
            let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
            let mut events = runtime
                .subscribe(SubscriptionRequest::new(
                    SubscriptionScope::Session { session_id },
                    false,
                ))
                .await
                .expect("the Session subscription opens");
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
                            PromptBodyIntent::Text(TextIntent::new("hello").unwrap()),
                            Vec::new(),
                        )
                        .unwrap(),
                    }),
                ))
                .await
                .expect("the ordinary Turn starts");
            let turn_id = started_turn(&submit);
            let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match events.recv().await {
                        Some(EventFrame::State(event))
                            if event.msg().session_kind()
                                == Some(SessionStateEventKind::TurnCompleted) =>
                        {
                            break event;
                        }
                        Some(EventFrame::State(_)) => continue,
                        other => {
                            panic!("unexpected frame before ordinary completion: {other:?}")
                        }
                    }
                }
            })
            .await
            .expect("the ordinary Turn completes");
            assert!(matches!(
                terminal.msg().session_detail(),
                Some(SessionEventDetail::TurnTerminal {
                    turn_id: completed_turn,
                    terminal: TurnTerminalView::Completed { .. },
                }) if completed_turn == turn_id
            ));

            let requests = model.requests();
            assert_eq!(requests.len(), 1);
            let tools = requests[0].input().tools();
            let mut disclosed = tools
                .iter()
                .map(|tool| tool.name().as_str())
                .collect::<Vec<_>>();
            disclosed.sort_unstable();
            assert_eq!(disclosed, expected_names);
            if read_file {
                let schema: crate::wire::BoundedJsonSchema = r#"{"type":"object","properties":{"path":{"type":"string","maxLength":4096}},"required":["path"],"additionalProperties":false}"#
                    .parse()
                    .expect("the frozen read_file schema is valid");
                let read_file_tool = tools
                    .iter()
                    .find(|tool| tool.name().as_str() == "read_file")
                    .expect("the read_file opt-in discloses its builtin exactly once");
                assert_eq!(read_file_tool.input_schema(), &schema);
            }
            runtime.shutdown().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_file_tool_opt_in_reads_a_workspace_file_and_continues_the_turn_end_to_end() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        // A real UTF-8 file under the Workspace cwd (`src`): the production read_file
        // builtin must read it through the read-only authority ceiling over a Workspace
        // whose requested access is ReadWrite, tightened to ReadOnly at resolution.
        fs::write(
            workspace.path().join("src").join("note.txt"),
            "hello from the workspace\n",
        )
        .expect("the Workspace cwd file is written");
        // The scripted model emits one read_file ToolCall for the cwd-relative file, then
        // one final text response for the same Turn after the read result settles.
        let model = ScriptedModelFixture::with_tool_round(
            "call_read",
            "read_file",
            r#"{"path":"note.txt"}"#,
            "final public answer",
        );
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()).with_read_file_tool(),
            Handle::current(),
            &model,
        )
        .await
        .expect("the read_file opt-in Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("the Session subscription opens");
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
                        PromptBodyIntent::Text(TextIntent::new("read it").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("public Submit dispatches");
        let turn_id = started_turn(&submit);

        // The one Tool round runs the production read_file builtin against the exact
        // captured Workspace snapshot and the same Turn runs its final response.
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(EventFrame::State(event))
                        if event.msg().session_kind()
                            == Some(SessionStateEventKind::TurnCompleted) =>
                    {
                        break event;
                    }
                    Some(EventFrame::State(_)) => continue,
                    other => {
                        panic!("unexpected frame before read_file Turn completion: {other:?}")
                    }
                }
            }
        })
        .await
        .expect("the read_file Turn completes");
        assert_eq!(terminal.command_id(), Some(submit_command_id));
        assert!(matches!(
            terminal.msg().session_detail(),
            Some(SessionEventDetail::TurnTerminal {
                turn_id: completed_turn,
                terminal: TurnTerminalView::Completed { .. },
            }) if completed_turn == turn_id
        ));
        assert!(
            terminal
                .msg()
                .session_snapshot()
                .and_then(|snapshot| snapshot.usage())
                .is_some_and(|usage| usage.model_calls() == 2)
        );
        assert_eq!(model.request_count(), 2);

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        // The first request discloses the exact frozen read_file schema.
        assert_eq!(requests[0].input().tools().len(), 1);
        assert_eq!(requests[0].input().tools()[0].name().as_str(), "read_file");
        let schema: crate::wire::BoundedJsonSchema = r#"{"type":"object","properties":{"path":{"type":"string","maxLength":4096}},"required":["path"],"additionalProperties":false}"#
            .parse()
            .expect("the frozen read_file schema is valid");
        assert_eq!(requests[0].input().tools()[0].input_schema(), &schema);
        // The second request carries the exact immutable ToolResult content of the read:
        // one text part with the file's full contents.
        assert!(
            requests[1]
                .input()
                .messages()
                .iter()
                .any(|message| matches!(
                    message.as_ref(),
                    ModelMessageRef::Tool {
                        tool_call_id,
                        content,
                    } if tool_call_id.as_str() == "call_read"
                        && content.parts().len() == 1
                        && content.parts()[0].as_text() == "hello from the workspace\n"
                ))
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_file_authority_invalidation_revokes_read_access_and_restores_no_grant() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        fs::write(
            workspace.path().join("src").join("note.txt"),
            "hello from the workspace\n",
        )
        .expect("the Workspace cwd file is written");
        // The scripted model emits one read_file ToolCall per Turn round, then one final
        // text response for that same Turn, across two rounds: the first Turn proves the
        // read grant, the post-revocation recovery is inspected through the installed
        // snapshot, and the second Turn is the real future admission — every admission
        // materializes the read_file ToolSet against exactly the current snapshot.  The
        // two rounds use distinct tool_call_ids so their ToolResults are distinguishable.
        let model = ScriptedModelFixture::with_two_tool_rounds(
            (
                "call_read",
                "read_file",
                r#"{"path":"note.txt"}"#,
                "final public answer",
            ),
            (
                "call_read_again",
                "read_file",
                r#"{"path":"note.txt"}"#,
                "second final answer",
            ),
        );
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()).with_read_file_tool(),
            Handle::current(),
            &model,
        )
        .await
        .expect("the read_file opt-in Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("the Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));

        // One read_file Turn succeeds end-to-end: the read authority grant is genuinely
        // present before the host revocation.
        let submit_command_id = CommandId::generate().unwrap();
        let submit = runtime
            .dispatch(CommandRequest::new(
                submit_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("read it").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("public Submit dispatches");
        let turn_id = started_turn(&submit);
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(EventFrame::State(event))
                        if event.msg().session_kind()
                            == Some(SessionStateEventKind::TurnCompleted) =>
                    {
                        break event;
                    }
                    Some(EventFrame::State(_)) => continue,
                    other => {
                        panic!("unexpected frame before read_file Turn completion: {other:?}")
                    }
                }
            }
        })
        .await
        .expect("the read_file Turn completes");
        assert!(matches!(
            terminal.msg().session_detail(),
            Some(SessionEventDetail::TurnTerminal {
                turn_id: completed_turn,
                terminal: TurnTerminalView::Completed { .. },
            }) if completed_turn == turn_id
        ));
        assert_eq!(model.request_count(), 2);
        let requests = model.requests();
        assert!(requests[1].input().messages().iter().any(|message| {
            matches!(
                message.as_ref(),
                ModelMessageRef::Tool {
                    tool_call_id,
                    content,
                } if tool_call_id.as_str() == "call_read"
                    && content.parts().len() == 1
                    && content.parts()[0].as_text() == "hello from the workspace\n"
            )
        }));

        // The host Workspace authority invalidation seam revokes the read authority for this
        // Session and recovers the loaded Idle Session; it resolves only after the recovery
        // final state is installed.
        runtime
            .invalidate_session_workspace_authority(session_id)
            .await
            .expect("the host-only invalidation seam recovers the read_file Session");

        // The recovery re-resolves with the revoked authority: resolve succeeds with filesystem
        // None (never AuthorityDenied), so it finishes a snapshot and readiness returns to
        // Ready through the exact existing projection — Preparing then Ready, both with no
        // CommandId, never a WorkspaceReloaded event.
        let mut readiness_events = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while readiness_events.len() < 2 {
                let Some(EventFrame::State(event)) = events.recv().await else {
                    panic!("the Session stream stays open through the recovery");
                };
                assert_ne!(
                    event.msg().session_kind(),
                    Some(SessionStateEventKind::SessionWorkspaceReloaded),
                    "the host-only invalidation never publishes WorkspaceReloaded"
                );
                if event.msg().session_kind()
                    == Some(SessionStateEventKind::SessionReadinessChanged)
                {
                    readiness_events.push(event);
                }
            }
        })
        .await
        .expect("the recovery publishes both readiness events");
        let (preparing, ready) = (&readiness_events[0], &readiness_events[1]);
        assert_eq!(preparing.command_id(), None);
        assert_eq!(ready.command_id(), None);
        assert_eq!(
            preparing
                .msg()
                .session_snapshot()
                .expect("readiness events carry a Session snapshot")
                .readiness(),
            SessionReadinessView::Preparing
        );
        assert_eq!(
            ready
                .msg()
                .session_snapshot()
                .expect("readiness events carry a Session snapshot")
                .readiness(),
            SessionReadinessView::Ready
        );

        // The critical proof: the recovered installed WorkspaceSnapshot carries no read
        // grant — the exact snapshot every future admission materializes the read_file
        // ToolSet against, so a future Submit can start (readiness Ready) but the read_file
        // Tool cannot execute.
        let residency = runtime
            .inner
            .residency()
            .expect("the open Runtime retains its residency");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the recovered Session stays loaded");
        let recovered_snapshot = executor.published_snapshot();
        assert_eq!(recovered_snapshot.readiness(), SessionReadinessView::Ready);
        let workspace_snapshot = recovered_snapshot
            .workspace_optional()
            .expect("a Ready recovery installs its WorkspaceSnapshot");
        assert_eq!(workspace_snapshot.session_id(), session_id);
        assert_eq!(
            workspace_snapshot
                .tool_context()
                .access()
                .authorize_read(&"note.txt".parse().unwrap())
                .unwrap_err(),
            WorkspaceAccessError::NotAuthorized
        );
        // The revocation is permanent for this Runtime lifetime: the resolver's control was
        // the one revoked, and no further model call was made by the recovery.
        assert_eq!(model.request_count(), 2);

        // The real future admission: a second public Submit starts against the recovered
        // no-grant Workspace snapshot (readiness Ready), the same Turn path discloses the
        // read_file ToolSet against that snapshot, and the model's second read_file
        // ToolCall to the same note.txt settles PreExecution + Denied with the exact frozen
        // text before any start factory exists — the same Turn then receives its scripted
        // final text and completes.
        let second_command_id = CommandId::generate().unwrap();
        let second_submit = runtime
            .dispatch(CommandRequest::new(
                second_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("read it again").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the second public Submit dispatches");
        let second_turn_id = started_turn(&second_submit);
        let second_terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(EventFrame::State(event))
                        if event.msg().session_kind()
                            == Some(SessionStateEventKind::TurnCompleted) =>
                    {
                        break event;
                    }
                    Some(EventFrame::State(_)) => continue,
                    other => {
                        panic!(
                            "unexpected frame before the post-revocation read_file Turn completion: {other:?}"
                        )
                    }
                }
            }
        })
        .await
        .expect("the post-revocation read_file Turn completes");
        assert_eq!(second_terminal.command_id(), Some(second_command_id));
        assert!(matches!(
            second_terminal.msg().session_detail(),
            Some(SessionEventDetail::TurnTerminal {
                turn_id: completed_turn,
                terminal: TurnTerminalView::Completed { .. },
            }) if completed_turn == second_turn_id
        ));
        assert!(
            second_terminal
                .msg()
                .session_snapshot()
                .and_then(|snapshot| snapshot.usage())
                .is_some_and(|usage| usage.model_calls() == 4)
        );
        assert_eq!(model.request_count(), 4);

        // The exact model request sequence across both admissions, four requests total:
        // request 0 is the first Turn's first request disclosing read_file; request 1
        // carries the first success ToolResult with the exact file content; request 2 is
        // the second Turn's first request and still discloses read_file against the
        // recovered no-grant snapshot; request 3 carries the second ToolResult with the
        // exact denial text, never the file content.
        let requests = model.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].input().tools().len(), 1);
        assert_eq!(requests[0].input().tools()[0].name().as_str(), "read_file");
        assert!(requests[1].input().messages().iter().any(|message| {
            matches!(
                message.as_ref(),
                ModelMessageRef::Tool {
                    tool_call_id,
                    content,
                } if tool_call_id.as_str() == "call_read"
                    && content.parts().len() == 1
                    && content.parts()[0].as_text() == "hello from the workspace\n"
            )
        }));
        assert_eq!(requests[2].input().tools().len(), 1);
        assert_eq!(requests[2].input().tools()[0].name().as_str(), "read_file");
        assert!(requests[3].input().messages().iter().any(|message| {
            matches!(
                message.as_ref(),
                ModelMessageRef::Tool {
                    tool_call_id,
                    content,
                } if tool_call_id.as_str() == "call_read_again"
                    && content.parts().len() == 1
                    && content.parts()[0].as_text() == "workspace file access is denied"
            )
        }));
        // The denied second read never leaks the file content into its own ToolResult: the
        // call_read_again result is exactly the one denial part (the first Turn's success
        // ToolResult legitimately stays in the conversation history, but no second read
        // result ever carries the file content).
        assert!(!requests[3].input().messages().iter().any(|message| {
            matches!(
                message.as_ref(),
                ModelMessageRef::Tool {
                    tool_call_id,
                    content,
                } if tool_call_id.as_str() == "call_read_again"
                    && content
                        .parts()
                        .iter()
                        .any(|part| part.as_text() == "hello from the workspace\n")
            )
        }));
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_file_authority_revocation_stays_current_when_invalidation_meets_a_not_loaded_session()
     {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        fs::write(
            workspace.path().join("src").join("note.txt"),
            "hello from the workspace\n",
        )
        .expect("the Workspace cwd file is written");
        // The model fixture only keeps the catalog non-empty; this test runs no Turn.
        let model = ScriptedModelFixture::new(Vec::<&str>::new());
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()).with_read_file_tool(),
            Handle::current(),
            &model,
        )
        .await
        .expect("the read_file opt-in Runtime opens");
        // Create but do NOT load: the invalidation seam must still publish the permanent read
        // revocation before it reports the missing loaded executor.
        let session_id = create_runtime_session(&runtime, workspace.path()).await;
        assert_eq!(
            runtime
                .invalidate_session_workspace_authority(session_id)
                .await,
            Err(SessionWorkspaceInvalidationError::SessionNotLoaded)
        );

        // The later public Load resolves the exact same Workspace with the revocation
        // current: resolve succeeds with filesystem None (never AuthorityDenied), so the
        // Session loads Ready but the read Tool cannot execute.
        let load = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::Load { session_id }),
            ))
            .await
            .expect("public Load dispatches");
        assert_eq!(command_output(&load), "session loaded");
        let residency = runtime
            .inner
            .residency()
            .expect("the open Runtime retains its residency");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the revoked Session is loaded");
        let loaded_snapshot = executor.published_snapshot();
        assert_eq!(loaded_snapshot.readiness(), SessionReadinessView::Ready);
        let workspace_snapshot = loaded_snapshot
            .workspace_optional()
            .expect("a Ready load installs its WorkspaceSnapshot");
        assert_eq!(workspace_snapshot.session_id(), session_id);
        assert_eq!(
            workspace_snapshot
                .tool_context()
                .access()
                .authorize_read(&"note.txt".parse().unwrap())
                .unwrap_err(),
            WorkspaceAccessError::NotAuthorized
        );
        assert_eq!(model.request_count(), 0);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ask_user_tool_opt_in_presents_answers_and_continues_the_turn_end_to_end() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        // The scripted model emits one ask_user call with a closed valid arguments object,
        // then one final text response for the same Turn after the answer settles.
        let model = ScriptedModelFixture::with_tool_round(
            "call_ask",
            "ask_user",
            r#"{"title":null,"questions":[{"questionIndex":0,"prompt":"Continue?","required":true,"input":{"type":"text","data":{"multiline":false}}}]}"#,
            "final public answer",
        );
        let runtime = MiniCoreRuntime::open_with_model_fixture(
            MiniCoreRuntimeConfig::new(root.path().to_owned()).with_ask_user_tool(),
            Handle::current(),
            &model,
        )
        .await
        .expect("the opt-in Runtime opens");
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let residency = runtime.inner.residency().expect("residency is installed");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the loaded executor is installed");
        let hooks = executor.test_hooks();
        drop(executor);
        drop(residency);
        let mut events = runtime
            .subscribe(SubscriptionRequest::new(
                SubscriptionScope::Session { session_id },
                false,
            ))
            .await
            .expect("the Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(snapshot)))
                if snapshot.execution() == SessionExecutionView::Idle
        ));

        let submit_command_id = CommandId::generate().unwrap();
        let submit = runtime
            .dispatch(CommandRequest::new(
                submit_command_id,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("please ask").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("public Submit dispatches");
        let turn_id = started_turn(&submit);

        // The production builtin presents the typed pending UserQuestion before any next
        // model call: exactly one interaction, one model call, and the Turn waits on input.
        hooks.wait_pending_interaction_publication().await;
        let pending = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("the published pending Session snapshot is available");
        let SnapshotResponse::Session(pending) = pending else {
            panic!("the pending snapshot is a Session snapshot");
        };
        let interaction = pending
            .pending_interactions()
            .first()
            .expect("the publication contains the ask_user interaction");
        let item_id = interaction.item_id();
        let request_id = interaction.request_id();
        let request_view = interaction.request().clone();
        let pending_phase = pending
            .current_turn()
            .expect("the pending Turn is current")
            .phase();
        assert_eq!(model.request_count(), 1);
        let expected_request = crate::turn_item_interaction::InteractionRequestView::UserQuestion(
            crate::tools::UserQuestionRequest::reconstruct(
                None,
                vec![
                    crate::tools::UserQuestionField::reconstruct(
                        0,
                        "Continue?",
                        true,
                        crate::tools::UserQuestionInput::Text { multiline: false },
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );
        assert!(request_view == expected_request);
        assert_eq!(
            pending_phase,
            Some(TurnExecutionPhaseView::WaitingForUserInput)
        );

        // The host resolves the question through the public Interaction command; the answer
        // binds through the production builtin and the same Turn runs its final response.
        let resolve = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Interaction(InteractionCommand::Resolve {
                    session_id,
                    expected_turn_id: turn_id,
                    item_id,
                    request_id,
                    resolution: InteractionResolutionInput::UserAnswer(
                        UserQuestionAnswer::new(vec![
                            UserQuestionFieldAnswer::text(0, "yes").unwrap(),
                        ])
                        .unwrap(),
                    ),
                    resolution_key: "irk_99999999999999999999999999999999".parse().unwrap(),
                }),
            ))
            .await
            .expect("public Resolve dispatches");
        assert!(matches!(
            resolve.completion(),
            CommandCompletion::Completed {
                outcome: CommandOutcome::InteractionResolved,
                ..
            }
        ));

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(EventFrame::State(event))
                        if event.msg().session_kind()
                            == Some(SessionStateEventKind::TurnCompleted) =>
                    {
                        break event;
                    }
                    Some(EventFrame::State(_)) => continue,
                    other => panic!("unexpected frame before ask_user Turn completion: {other:?}"),
                }
            }
        })
        .await
        .expect("the answered question resumes and the Turn completes");
        assert_eq!(terminal.command_id(), Some(submit_command_id));
        assert!(matches!(
            terminal.msg().session_detail(),
            Some(SessionEventDetail::TurnTerminal {
                turn_id: completed_turn,
                terminal: TurnTerminalView::Completed { .. },
            }) if completed_turn == turn_id
        ));
        assert!(
            terminal
                .msg()
                .session_snapshot()
                .and_then(|snapshot| snapshot.usage())
                .is_some_and(|usage| usage.model_calls() == 2)
        );
        assert_eq!(model.request_count(), 2);

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].input().tools().len(), 1);
        assert_eq!(requests[0].input().tools()[0].name().as_str(), "ask_user");
        let expected_answer =
            r#"{"answers":[{"questionIndex":0,"value":{"type":"text","data":"yes"}}]}"#;
        assert!(
            requests[1]
                .input()
                .messages()
                .iter()
                .any(|message| matches!(
                    message.as_ref(),
                    ModelMessageRef::Tool {
                        tool_call_id,
                        content,
                    } if tool_call_id.as_str() == "call_ask"
                        && content.parts().len() == 1
                        && content.parts()[0].as_text() == expected_answer
                ))
        );

        // Recording keeps the truthful PreExecution Succeeded source/disposition; the exact
        // model-visible text was asserted above from the second immutable model request.
        let recording = fs::read_to_string(
            root.path()
                .join("sessions")
                .join(session_id.to_string())
                .join("conversation.jsonl"),
        )
        .expect("the recorded conversation is readable");
        assert!(recording.contains(r#"questionIndex":0"#));
        assert!(recording.contains(r#"pre_execution"#));
        assert!(recording.contains(r#"succeeded"#));
        assert!(!recording.contains("executed"));

        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_provider_installation_installs_models_independent_of_credentials() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        // The credential source reports None, yet the installed model definition must
        // resolve and the Session model availability must stay true: only the actual
        // attempt would surface AuthMissing.
        let config = MiniCoreRuntimeConfig::new(root.path().to_owned()).with_model_provider(
            ModelProviderConfig::openai_responses(
                "https://api.openai.com/v1/responses",
                ProviderEndpointPolicy::HttpsOnly,
                Arc::new(MissingCredentialSource),
                vec![
                    ModelProviderDescriptor::new(
                        ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
                        NonZeroU64::new(1).unwrap(),
                        "gpt-5",
                        NonZeroU32::new(4_096).unwrap(),
                        NonZeroU32::new(3).unwrap(),
                    )
                    .unwrap(),
                ],
            )
            .expect("the provider installation validates"),
        );
        let runtime = MiniCoreRuntime::open(config, Handle::current())
            .await
            .expect("the Runtime opens with the installed provider");
        let (_model_gateway, catalog) = runtime.inner.model_resources();
        assert_eq!(
            catalog.definition_count(),
            1,
            "the installed definition resolves without resolving any credential"
        );
        let (model_gateway, _catalog) = runtime.inner.model_resources();
        assert_eq!(
            model_gateway
                .build_reload_candidate()
                .await
                .expect("the shared-resource reload candidate rebuilds")
                .definition_count(),
            1,
            "reload reuses the installed source and never rebuilds a client"
        );
        let session_id = create_and_load_public_session(&runtime, workspace.path()).await;
        let SnapshotResponse::Session(snapshot) = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("the Session snapshot is available")
        else {
            panic!("the snapshot is a Session snapshot");
        };
        assert_eq!(
            snapshot.readiness(),
            SessionReadinessView::Ready,
            "model availability must stay independent from the dynamic credential source"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_duplicate_selections_across_installations_fail_open() {
        let root = TempRoot::new();
        let descriptor = ModelProviderDescriptor::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            NonZeroU64::new(1).unwrap(),
            "gpt-5",
            NonZeroU32::new(4_096).unwrap(),
            NonZeroU32::new(3).unwrap(),
        )
        .unwrap();
        let config = MiniCoreRuntimeConfig::new(root.path().to_owned())
            .with_model_provider(
                ModelProviderConfig::openai_responses(
                    "https://api.openai.com/v1/responses",
                    ProviderEndpointPolicy::HttpsOnly,
                    Arc::new(MissingCredentialSource),
                    vec![descriptor.clone()],
                )
                .expect("the first installation validates"),
            )
            .with_model_provider(
                ModelProviderConfig::openai_responses(
                    "https://api.openai.com/v1/responses",
                    ProviderEndpointPolicy::HttpsOnly,
                    Arc::new(MissingCredentialSource),
                    vec![descriptor],
                )
                .expect("the second installation validates"),
            );
        let error = MiniCoreRuntime::open(config, Handle::current())
            .await
            .expect_err("duplicate selections across installations must fail open");
        assert_eq!(error, RuntimeInitializationError::InvalidConfiguration);
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

    #[tokio::test(flavor = "current_thread")]
    async fn public_session_metadata_cas_while_running_keeps_execution_and_conversation_untouched()
    {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let model = ScriptedModelFixture::new(vec!["running metadata answer"]);
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
            .expect("the Session subscription opens");
        assert!(matches!(
            events.recv().await,
            Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
        ));

        let residency = runtime
            .inner
            .residency()
            .expect("residency remains installed");
        let executor = residency
            .executor_for_test(session_id)
            .expect("the loaded executor is installed");
        let hooks = executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        drop(executor);
        drop(residency);

        let submit = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(
                            TextIntent::new("run while metadata updates").unwrap(),
                        ),
                        Vec::new(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the public Submit dispatches");
        let running_turn = started_turn(&submit);
        hooks.wait_before_agent_run_attempt().await;

        let SnapshotResponse::Session(running) = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("the running Session snapshot is available")
        else {
            panic!("the running snapshot is a Session snapshot");
        };
        assert_eq!(running.execution(), SessionExecutionView::Running);
        assert_eq!(running.current_turn().unwrap().turn_id(), running_turn);
        assert_eq!(running.queues().submit_admissions().len(), 0);
        assert_eq!(running.queues().steers().len(), 0);
        assert_eq!(running.queues().follow_ups().len(), 0);
        let running_items = running.active_items().len();

        // The Running Turn is held at the agent-run hook, so the conversation bytes are quiescent
        // across the metadata CAS below.
        let conversation_path = root
            .path()
            .join("sessions")
            .join(session_id.to_string())
            .join("conversation.jsonl");
        let before =
            fs::read(&conversation_path).expect("the conversation is readable while Running");

        let update_id = CommandId::generate().unwrap();
        let updated = runtime
            .dispatch(CommandRequest::new(
                update_id,
                RuntimeCommand::Session(SessionCommand::UpdateMetadata {
                    session_id,
                    expected_revision: "smr_1".parse::<SessionMetadataRevision>().unwrap(),
                    patch: SessionMetadataPatch::new(
                        OptionalTextPatch::set("Running v2").unwrap(),
                        OptionalTextPatch::keep(),
                    )
                    .unwrap(),
                }),
            ))
            .await
            .expect("the Running Session metadata update dispatches");
        assert!(
            matches!(
                updated.completion(),
                CommandCompletion::Completed {
                    outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
                    output: None,
                } if metadata_revision.get() == 2
            ),
            "a Running Session accepts metadata updates without SessionBusy"
        );

        // The Subscription was opened before Submit, so Starting/Running execution events can
        // precede the metadata event on the same stream; drain until the matching update.
        let session_event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(EventFrame::State(event))
                        if event.msg().session_kind()
                            == Some(SessionStateEventKind::SessionMetadataUpdated)
                            && event.command_id() == Some(update_id) =>
                    {
                        return event;
                    }
                    Some(EventFrame::State(_)) => continue,
                    other => {
                        panic!("unexpected frame while draining to the metadata event: {other:?}")
                    }
                }
            }
        })
        .await
        .expect("the Running update publishes its Session MetadataUpdated event");
        assert_eq!(
            session_event.msg().session_kind(),
            Some(SessionStateEventKind::SessionMetadataUpdated)
        );
        let snapshot = session_event.msg().session_snapshot().unwrap();
        assert_eq!(snapshot.metadata().revision().get(), 2);
        assert_eq!(snapshot.metadata().name(), Some("Running v2"));
        assert_eq!(snapshot.execution(), SessionExecutionView::Running);
        assert_eq!(snapshot.current_turn().unwrap().turn_id(), running_turn);

        let SnapshotResponse::Session(after) = runtime
            .snapshot(SnapshotRequest::Session { session_id })
            .await
            .expect("the post-update Session snapshot is available")
        else {
            panic!("the post-update snapshot is a Session snapshot");
        };
        assert_eq!(after.metadata().revision().get(), 2);
        assert_eq!(after.metadata().name(), Some("Running v2"));
        assert_eq!(after.execution(), SessionExecutionView::Running);
        assert_eq!(after.current_turn().unwrap().turn_id(), running_turn);
        assert_eq!(after.queues().submit_admissions().len(), 0);
        assert_eq!(after.queues().steers().len(), 0);
        assert_eq!(after.queues().follow_ups().len(), 0);
        assert_eq!(after.active_items().len(), running_items);
        assert_eq!(
            fs::read(&conversation_path).expect("the conversation remains readable"),
            before,
            "a Running Session metadata update never touches conversation JSONL bytes"
        );

        hooks.release_before_agent_run_attempt();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(EventFrame::State(event))
                        if event.msg().session_kind()
                            == Some(SessionStateEventKind::TurnCompleted) =>
                    {
                        return event;
                    }
                    Some(EventFrame::State(_)) => continue,
                    other => panic!("unexpected frame while draining to TurnCompleted: {other:?}"),
                }
            }
        })
        .await
        .expect("the Running Turn reaches terminal state");
        assert_eq!(
            terminal.msg().session_kind(),
            Some(SessionStateEventKind::TurnCompleted)
        );

        runtime.shutdown().await;
    }
}
