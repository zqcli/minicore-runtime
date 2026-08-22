use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::agent::system_timestamp_source;
use crate::config::{RetryPolicy, RuntimeConfig, SessionConfig};
use crate::error::{RuntimeError, SessionError};
use crate::ids::{InteractionId, SessionId, TurnId};
use crate::model::{ModelGateway, ModelSelection, ReasoningPreference};
use crate::session::SessionSnapshot;
use crate::session::actor::{SessionActor, SessionActorDependencies};
use crate::session::event_stream::SessionEventStream;
use crate::session::transcript::TranscriptPage;
use crate::storage::conversation::{ConversationError, ConversationLog};
use crate::storage::store::{SessionStore, StoreError, StoredSessionConfig};
use crate::time::Timestamp;
use crate::tools::{AllowConfiguredTools, ToolName, ToolPolicy, ToolRegistry, UserAnswer};
use crate::workspace::{Workspace, WorkspaceAccess, WorkspaceError};

use super::session_manager::{JoinOnce, LoadedSessionId, ManagedSession, SessionManager};

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub model: ModelSelection,
    pub loaded: bool,
}

/// A typed P7 runtime owner.
///
/// Call [`Runtime::shutdown`] explicitly to observe cleanup. Dropping the last
/// clone starts the same asynchronous shutdown path and retains active actor or
/// shutdown owners when needed.
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    store: Arc<SessionStore>,
    gateway: ModelGateway,
    tools: ToolRegistry,
    policy: Arc<dyn ToolPolicy>,
    coding_instructions: Arc<str>,
    retry_policy: RetryPolicy,
    runtime: Handle,
    shutdown_timeout: Duration,
    command_capacity: usize,
    event_capacity: usize,
    runner_event_capacity: usize,
    manager: SessionManager,
    closing: CancellationToken,
    shutdown: Arc<JoinOnce<Result<(), RuntimeError>>>,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Runtime { .. }")
    }
}

impl Runtime {
    pub async fn open(config: RuntimeConfig, runtime: Handle) -> Result<Self, RuntimeError> {
        let store = SessionStore::open(config.data_dir().to_owned())
            .await
            .map_err(map_open_error)?;
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                store: Arc::new(store),
                gateway: ModelGateway::new(config.provider_registry()),
                tools: config.tool_registry(),
                policy: Arc::new(AllowConfiguredTools::new()),
                coding_instructions: config.coding_instructions(),
                retry_policy: config.retry_policy(),
                runtime,
                shutdown_timeout: config.shutdown_timeout(),
                command_capacity: config.command_capacity(),
                event_capacity: config.event_capacity(),
                runner_event_capacity: config.runner_event_capacity(),
                manager: SessionManager::new(),
                closing: CancellationToken::new(),
                shutdown: JoinOnce::pending(|| Err(RuntimeError::Internal)),
            }),
        })
    }

    pub async fn create_session(&self, config: SessionConfig) -> Result<SessionId, SessionError> {
        if self.inner.closing.is_cancelled() {
            return Err(SessionError::Closing);
        }
        let id = SessionId::new().map_err(|_| SessionError::Internal)?;
        let timestamp = Timestamp::now_utc().map_err(|_| SessionError::Internal)?;
        let stored = StoredSessionConfig::from_session_config(id, timestamp, &config)
            .map_err(|_| SessionError::InvalidInput)?;
        validate_stored(&self.inner.gateway, &self.inner.tools, &stored)?;
        let mut reservation = self.inner.manager.begin_load(id)?;
        self.inner
            .store
            .create(&stored)
            .await
            .map_err(map_store_session_error)?;
        let managed = self
            .prepare_session(id, reservation.loaded_session_id())
            .await?;
        if self.inner.manager.finish_load(&mut reservation, managed) {
            return Err(SessionError::Closing);
        }
        Ok(id)
    }

    pub async fn load_session(&self, id: SessionId) -> Result<(), SessionError> {
        let mut reservation = self.inner.manager.begin_load(id)?;
        let managed = self
            .prepare_session(id, reservation.loaded_session_id())
            .await?;
        if self.inner.manager.finish_load(&mut reservation, managed) {
            return Err(SessionError::Closing);
        }
        Ok(())
    }

    pub async fn close_session(&self, id: SessionId) -> Result<(), SessionError> {
        let Some(session) = self.inner.manager.get(id) else {
            return Err(SessionError::NotFound);
        };
        let result = session.close().await;
        self.inner.manager.remove_exact(id, &session);
        result
    }

    pub async fn delete_session(&self, id: SessionId) -> Result<(), SessionError> {
        let reservation = match self.inner.manager.begin_load(id) {
            Ok(reservation) => reservation,
            Err(SessionError::AlreadyLoaded) => return Err(SessionError::Busy),
            Err(error) => return Err(error),
        };
        let result = self
            .inner
            .store
            .delete(id)
            .await
            .map_err(map_store_session_error);
        drop(reservation);
        result
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionError> {
        if self.inner.closing.is_cancelled() || self.inner.manager.is_closing() {
            return Err(SessionError::Closing);
        }
        let loaded: BTreeSet<SessionId> = self
            .inner
            .manager
            .snapshot()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let ids = self
            .inner
            .store
            .list()
            .await
            .map_err(map_store_session_error)?;
        let mut summaries = Vec::with_capacity(ids.len());
        for id in ids {
            let config = self
                .inner
                .store
                .load_config(id)
                .await
                .map_err(map_store_session_error)?;
            summaries.push(SessionSummary {
                session_id: id,
                model: config.model().selection().clone(),
                loaded: loaded.contains(&id),
            });
        }
        Ok(summaries)
    }

    pub async fn submit(&self, id: SessionId, input: String) -> Result<TurnId, SessionError> {
        self.loaded(id)?.handle.submit(input).await
    }

    pub async fn answer(
        &self,
        id: SessionId,
        interaction_id: InteractionId,
        answer: UserAnswer,
    ) -> Result<(), SessionError> {
        self.loaded(id)?.handle.answer(interaction_id, answer).await
    }

    pub fn cancel(&self, id: SessionId) -> Result<(), SessionError> {
        self.loaded(id)?.handle.cancel()
    }

    pub fn snapshot(&self, id: SessionId) -> Result<SessionSnapshot, SessionError> {
        Ok(self.loaded(id)?.handle.snapshot())
    }

    pub fn subscribe(&self, id: SessionId) -> Result<SessionEventStream, SessionError> {
        self.loaded(id)?.handle.subscribe()
    }

    pub async fn transcript(
        &self,
        id: SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<TranscriptPage, SessionError> {
        if let Some(session) = self.inner.manager.get(id) {
            return session
                .conversation
                .transcript(after_seq, limit)
                .await
                .map_err(map_conversation_session_error);
        }
        if self.inner.closing.is_cancelled() || self.inner.manager.is_closing() {
            return Err(SessionError::Closing);
        }
        let conversation = Arc::new(
            ConversationLog::open(&self.inner.store, id)
                .await
                .map_err(map_conversation_session_error)?,
        );
        let result = conversation
            .transcript(after_seq, limit)
            .await
            .map_err(map_conversation_session_error);
        let close_result = conversation
            .close()
            .await
            .map_err(map_conversation_session_error);
        match (result, close_result) {
            (Ok(page), Ok(())) => Ok(page),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.inner.start_shutdown_sync();
        self.inner.shutdown.join().await
    }

    fn loaded(&self, id: SessionId) -> Result<Arc<ManagedSession>, SessionError> {
        if self.inner.closing.is_cancelled() {
            return Err(SessionError::Closing);
        }
        self.inner.manager.get(id).ok_or(SessionError::NotFound)
    }

    async fn prepare_session(
        &self,
        id: SessionId,
        loaded_session_id: LoadedSessionId,
    ) -> Result<Arc<ManagedSession>, SessionError> {
        let stored = self
            .inner
            .store
            .load_config(id)
            .await
            .map_err(map_store_session_error)?;
        let conversation = Arc::new(
            ConversationLog::open(&self.inner.store, id)
                .await
                .map_err(map_conversation_session_error)?,
        );
        let access = workspace_access(stored.execution().enabled_tools());
        let workspace = match Workspace::open(stored.workspace_root(), access) {
            Ok(workspace) => Arc::new(workspace),
            Err(error) => {
                let _ = conversation.close().await;
                return Err(map_workspace_error(error));
            }
        };
        let dependencies = SessionActorDependencies {
            model_gateway: self.inner.gateway.clone(),
            tool_registry: self.inner.tools.clone(),
            tool_policy: Arc::clone(&self.inner.policy),
            coding_instructions: Arc::clone(&self.inner.coding_instructions),
            retry_policy: self.inner.retry_policy,
            timestamp_source: system_timestamp_source(),
            runtime: self.inner.runtime.clone(),
            close_timeout: self.inner.shutdown_timeout,
            command_capacity: self.inner.command_capacity,
            event_capacity: self.inner.event_capacity,
            runner_event_capacity: self.inner.runner_event_capacity,
        };
        let (handle, actor) = match SessionActor::new(
            stored.clone(),
            Arc::clone(&conversation),
            Arc::clone(&workspace),
            dependencies,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                let _ = conversation.close().await;
                let _ = workspace.shutdown().await;
                return Err(error);
            }
        };
        let actor_task = self.inner.runtime.spawn(actor.run());
        Ok(ManagedSession::new(
            loaded_session_id,
            handle,
            conversation,
            actor_task,
        ))
    }
}

impl Clone for Runtime {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.inner.start_shutdown_sync();
        }
    }
}

struct RetainedRuntimeOwners {
    _manager: Option<SessionManager>,
    _shutdown: Option<Arc<JoinOnce<Result<(), RuntimeError>>>>,
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        self.start_shutdown_sync();
        let retain_manager = self.manager.has_loaded();
        let retain_shutdown = self.shutdown.needs_retention();
        if retain_manager || retain_shutdown {
            std::mem::forget(RetainedRuntimeOwners {
                _manager: retain_manager.then(|| self.manager.clone()),
                _shutdown: retain_shutdown.then(|| Arc::clone(&self.shutdown)),
            });
        }
    }
}

impl RuntimeInner {
    fn start_shutdown_sync(&self) {
        self.closing.cancel();
        for (_, session) in self.manager.begin_shutdown() {
            session.request_close();
        }
        let manager = self.manager.clone();
        let store = Arc::clone(&self.store);
        self.shutdown.start(&self.runtime, async move {
            shutdown_sessions(manager, store).await
        });
    }
}

fn validate_stored(
    gateway: &ModelGateway,
    tools: &ToolRegistry,
    config: &StoredSessionConfig,
) -> Result<(), SessionError> {
    let resolved = gateway
        .resolve(config.model().selection())
        .map_err(|_| SessionError::Unavailable)?;
    if resolved.descriptor().selection() != config.model().selection()
        || !resolved
            .descriptor()
            .supports_reasoning(ReasoningPreference::Auto)
        || !resolved
            .descriptor()
            .supports_reasoning(ReasoningPreference::Disabled)
    {
        return Err(SessionError::InvalidInput);
    }
    tools
        .specs(config.execution().enabled_tools())
        .map_err(|_| SessionError::InvalidInput)?;
    Ok(())
}

fn workspace_access(enabled: &BTreeSet<ToolName>) -> WorkspaceAccess {
    let write_file = "write_file"
        .parse::<ToolName>()
        .expect("write_file is a stable tool name");
    if enabled.contains(&write_file) {
        WorkspaceAccess::ReadWrite
    } else {
        WorkspaceAccess::ReadOnly
    }
}

fn map_open_error(error: StoreError) -> RuntimeError {
    match error {
        StoreError::InUse | StoreError::InvalidConfig => RuntimeError::InvalidConfiguration,
        StoreError::Closing => RuntimeError::Closing,
        StoreError::NotFound
        | StoreError::AlreadyExists
        | StoreError::Busy
        | StoreError::Corrupt
        | StoreError::ConversationCorrupt { .. }
        | StoreError::TooLarge
        | StoreError::CleanupFailed
        | StoreError::Io
        | StoreError::WorkerFailed => RuntimeError::Internal,
    }
}

async fn shutdown_sessions(
    manager: SessionManager,
    store: Arc<SessionStore>,
) -> Result<(), RuntimeError> {
    manager.wait_loading().await;
    let sessions = manager.snapshot();
    for (_, session) in &sessions {
        session.request_close();
    }
    let results = join_all(sessions.iter().map(|(_, session)| session.close())).await;
    let mut first_error = None;
    for ((id, session), result) in sessions.into_iter().zip(results) {
        if let Err(error) = result {
            first_error.get_or_insert(map_session_runtime_error(error));
        }
        manager.remove_exact(id, &session);
    }
    if let Err(error) = store.shutdown().await {
        first_error.get_or_insert(map_open_error(error));
    }
    first_error.map_or(Ok(()), Err)
}

fn map_session_runtime_error(error: SessionError) -> RuntimeError {
    match error {
        SessionError::Closing => RuntimeError::Closing,
        SessionError::InvalidInput => RuntimeError::InvalidConfiguration,
        SessionError::NotFound
        | SessionError::AlreadyLoaded
        | SessionError::Busy
        | SessionError::InteractionMismatch
        | SessionError::Unavailable
        | SessionError::Internal => RuntimeError::Internal,
    }
}

fn map_store_session_error(error: StoreError) -> SessionError {
    match error {
        StoreError::NotFound => SessionError::NotFound,
        StoreError::Busy | StoreError::AlreadyExists => SessionError::Busy,
        StoreError::Closing => SessionError::Closing,
        StoreError::InvalidConfig => SessionError::InvalidInput,
        StoreError::InUse
        | StoreError::Corrupt
        | StoreError::ConversationCorrupt { .. }
        | StoreError::TooLarge
        | StoreError::CleanupFailed
        | StoreError::Io
        | StoreError::WorkerFailed => SessionError::Internal,
    }
}

fn map_conversation_session_error(error: ConversationError) -> SessionError {
    match error {
        ConversationError::InvalidPage | ConversationError::InvalidEntry => {
            SessionError::InvalidInput
        }
        ConversationError::Closing => SessionError::Closing,
        ConversationError::Busy => SessionError::Busy,
        ConversationError::NotFound => SessionError::NotFound,
        ConversationError::Degraded | ConversationError::IncompleteToolExchange => {
            SessionError::Unavailable
        }
        ConversationError::Corrupt
        | ConversationError::CorruptAt { .. }
        | ConversationError::TooLarge
        | ConversationError::Io
        | ConversationError::WorkerFailed
        | ConversationError::Stale => SessionError::Internal,
    }
}

fn map_workspace_error(error: WorkspaceError) -> SessionError {
    match error {
        WorkspaceError::Closing => SessionError::Closing,
        WorkspaceError::RootUnavailable
        | WorkspaceError::RootNotDirectory
        | WorkspaceError::RootSymlink
        | WorkspaceError::NotFound
        | WorkspaceError::ReadOnly
        | WorkspaceError::InvalidPath
        | WorkspaceError::MissingParent
        | WorkspaceError::IsDirectory
        | WorkspaceError::IsSymlink
        | WorkspaceError::NotRegularFile
        | WorkspaceError::TooLarge
        | WorkspaceError::InvalidUtf8
        | WorkspaceError::ListingTooLarge
        | WorkspaceError::InvalidEntryName
        | WorkspaceError::Io
        | WorkspaceError::CleanupFailed
        | WorkspaceError::WorkerFailed => SessionError::Unavailable,
    }
}

const _: () = {
    let _ = Runtime::open;
    let _ = Runtime::create_session;
    let _ = Runtime::load_session;
    let _ = Runtime::close_session;
    let _ = Runtime::delete_session;
    let _ = Runtime::list_sessions;
    let _ = Runtime::submit;
    let _ = Runtime::answer;
    let _ = Runtime::cancel;
    let _ = Runtime::snapshot;
    let _ = Runtime::subscribe;
    let _ = Runtime::transcript;
    let _ = Runtime::shutdown;
};
