use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::agent_session_lifecycle::SealedSessionLifecycleAttempt;
use crate::durable_state::{DurableOpenError, DurableState};
use crate::runtime_task::RuntimeTaskContext;
use crate::session_execution::{SessionExecutorSnapshot, SessionWorkspaceDefinitionOutcome};
use crate::session_residency::{
    SessionResidencyLifecycleError, SessionResidencyLoadError, SessionResidencyLoadOutcome,
    SessionResidencyRegistry, SessionResidencySnapshotError, SessionResidencyStartError,
    SessionResidencyUnloadError, SessionResidencyUnloadOutcome,
    SessionResidencyWorkspaceDefinitionError,
};
use crate::wire::{SessionDefinitionRevision, SessionId, Timestamp};
use crate::workspace::{Workspace, WorkspaceResolver};

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
        let task_context = RuntimeTaskContext::new(handle)
            .await
            .map_err(|_| RuntimeInitializationError::RuntimeDependencyUnavailable)?;
        let durable_state =
            match DurableState::open(config.durable_root, task_context.clone()).await {
                Ok(durable_state) => durable_state,
                Err(error) => {
                    task_context.shutdown().await;
                    return Err(error.into());
                }
            };

        let resolver = Arc::new(WorkspaceResolver::new(task_context.clone()));
        let session_residency = match SessionResidencyRegistry::start(
            task_context.clone(),
            durable_state.clone(),
            resolver,
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
    retained_until_shutdown: Mutex<Option<Arc<RuntimeInner>>>,
    session_residency: Mutex<Option<Arc<SessionResidencyRegistry>>>,
    durable_state: Mutex<Option<DurableState>>,
    lifecycle: Mutex<RuntimeLifecycle>,
    lifecycle_changed: Notify,
}

impl RuntimeInner {
    fn new(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        session_residency: Arc<SessionResidencyRegistry>,
    ) -> Self {
        Self {
            task_context,
            retained_until_shutdown: Mutex::new(None),
            session_residency: Mutex::new(Some(session_residency)),
            durable_state: Mutex::new(Some(durable_state)),
            lifecycle: Mutex::new(RuntimeLifecycle::Open),
            lifecycle_changed: Notify::new(),
        }
    }

    // A dropped facade must only request Closing; it cannot release the lease before an
    // awaited shutdown has drained the owner. Explicit shutdown breaks this retention.
    fn retain_until_shutdown(self: &Arc<Self>) {
        *lock(&self.retained_until_shutdown) = Some(Arc::clone(self));
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
    use crate::model_gateway::{ModelSelection, ReasoningPreference};
    use crate::prompt::{AgentPromptSelection, SessionPromptSelection};
    use crate::runtime_task::RuntimeTaskError;
    use crate::session_execution::SessionExecutionState;
    use crate::session_residency::{
        SessionResidencyLifecycleError, SessionResidencyLoadError, SessionResidencyLoadOutcome,
        SessionResidencyUnloadOutcome,
    };
    use crate::wire::conversation_jsonl::ConversationLineCodec;
    use crate::wire::{CanonicalFileUri, FileUriFamily, SessionId};
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

    fn workspace_with_revision(path: &Path, revision: &str) -> Workspace {
        let key: WorkspaceRootKey = "repo".parse().unwrap();
        lower_workspace(
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
            .unwrap(),
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

    async fn create_runtime_session(runtime: &MiniCoreRuntime, workspace_root: &Path) -> SessionId {
        let durable_state = super::lock(&runtime.inner.durable_state)
            .as_ref()
            .cloned()
            .expect("the open Runtime retains DurableState");
        let created_at = "2026-08-03T10:01:00.456Z".parse().unwrap();
        let agent = durable_state
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
            .expect("the Runtime test Agent is published");
        durable_state
            .create_session(
                SealedSessionCreateAttempt::new(
                    agent.agent_id(),
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
    async fn runtime_owns_load_unload_and_lifecycle_residency() {
        let root = TempRoot::new();
        let workspace = TempWorkspace::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
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
