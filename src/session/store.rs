use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use fs4::fs_std::FileExt;
use serde::de::Error as _;
use serde::ser::{Error as _, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, oneshot, watch};

use super::time::Timestamp;
use crate::ids_v2::SessionId;
use crate::model_v2::{ModelId, ModelSelection, ProviderId};
use crate::tools_v2::ToolName;

const FORMAT_VERSION: u8 = 2;
const MAX_SESSION_JSON_BYTES: usize = 1_048_576;
const MAX_SYSTEM_PROMPT_BYTES: usize = 262_144;
const MAX_ENABLED_TOOLS: usize = 64;
const MAX_TOOL_ROUNDS: u8 = 64;
const WORKER_QUEUE_CAPACITY: usize = 64;
const TEMP_PREFIX: &str = ".session-tmp-";
const TEMP_NAME_ATTEMPTS: usize = 32;
type StoredConfigConstructor = fn(
    SessionId,
    Timestamp,
    Timestamp,
    PathBuf,
    StoredModelConfig,
    String,
    StoredExecutionConfig,
) -> Result<StoredSessionConfig, StoreError>;

// Keep this crate-private foundation type-checked before the SessionActor slice consumes it.
const _: () = {
    let _ = FORMAT_VERSION;
    let _ = MAX_SESSION_JSON_BYTES;
    let _ = MAX_SYSTEM_PROMPT_BYTES;
    let _ = MAX_ENABLED_TOOLS;
    let _ = MAX_TOOL_ROUNDS;
    let _ = WORKER_QUEUE_CAPACITY;
    let _ = TEMP_PREFIX;
    let _ = TEMP_NAME_ATTEMPTS;
    let _ = std::mem::size_of::<StoreError>();
    let _ = std::mem::size_of::<StoredModelConfig>();
    let _ = std::mem::size_of::<StoredCompactionConfig>();
    let _ = std::mem::size_of::<StoredExecutionConfig>();
    let _ = std::mem::size_of::<StoredSessionConfig>();
    let _ = std::mem::size_of::<SessionStore>();
    let _ = std::mem::size_of::<SessionRegistration>();
    let _ = StoredModelConfig::new;
    let _ = StoredCompactionConfig::new;
    let _ = StoredExecutionConfig::new;
    let _: StoredConfigConstructor = StoredSessionConfig::new;
    let _ = SessionStore::open;
    let _ = SessionStore::create;
    let _ = SessionStore::load_config;
    let _ = SessionStore::list;
    let _ = SessionStore::delete;
    let _ = SessionStore::shutdown;
    let _ = SessionStore::open_registration;
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum StoreError {
    #[error("session store is already in use")]
    InUse,
    #[error("session was not found")]
    NotFound,
    #[error("session already exists")]
    AlreadyExists,
    #[error("session store operation is busy")]
    Busy,
    #[error("session configuration is invalid")]
    InvalidConfig,
    #[error("session store data is corrupt")]
    Corrupt,
    #[error("session store data is too large")]
    TooLarge,
    #[error("session store cleanup failed")]
    CleanupFailed,
    #[error("session store I/O failed")]
    Io,
    #[error("session store worker failed")]
    WorkerFailed,
    #[error("session store is closing")]
    Closing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredModelConfig {
    selection: ModelSelection,
}

impl StoredModelConfig {
    pub(crate) const fn new(selection: ModelSelection) -> Self {
        Self { selection }
    }

    pub(crate) const fn selection(&self) -> &ModelSelection {
        &self.selection
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredCompactionConfig {
    trigger_tokens: u64,
    target_tokens: u64,
}

impl StoredCompactionConfig {
    pub(crate) fn new(trigger_tokens: u64, target_tokens: u64) -> Result<Self, StoreError> {
        if trigger_tokens == 0 || target_tokens == 0 || target_tokens >= trigger_tokens {
            return Err(StoreError::InvalidConfig);
        }
        Ok(Self {
            trigger_tokens,
            target_tokens,
        })
    }

    pub(crate) const fn trigger_tokens(&self) -> u64 {
        self.trigger_tokens
    }

    pub(crate) const fn target_tokens(&self) -> u64 {
        self.target_tokens
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredExecutionConfig {
    enabled_tools: BTreeSet<ToolName>,
    compaction: StoredCompactionConfig,
    max_tool_rounds: u8,
}

impl StoredExecutionConfig {
    pub(crate) fn new(
        enabled_tools: BTreeSet<ToolName>,
        compaction: StoredCompactionConfig,
        max_tool_rounds: u8,
    ) -> Result<Self, StoreError> {
        if enabled_tools.len() > MAX_ENABLED_TOOLS
            || !(1..=MAX_TOOL_ROUNDS).contains(&max_tool_rounds)
        {
            return Err(StoreError::InvalidConfig);
        }
        Ok(Self {
            enabled_tools,
            compaction,
            max_tool_rounds,
        })
    }

    pub(crate) fn enabled_tools(&self) -> &BTreeSet<ToolName> {
        &self.enabled_tools
    }

    pub(crate) const fn compaction(&self) -> &StoredCompactionConfig {
        &self.compaction
    }

    pub(crate) const fn max_tool_rounds(&self) -> u8 {
        self.max_tool_rounds
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredSessionConfig {
    format_version: u8,
    session_id: SessionId,
    created_at: Timestamp,
    updated_at: Timestamp,
    workspace_root: PathBuf,
    model: StoredModelConfig,
    system_prompt: String,
    execution: StoredExecutionConfig,
}

impl fmt::Debug for StoredSessionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredSessionConfig")
            .field("format_version", &self.format_version)
            .field("session_id", &self.session_id)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("workspace_root", &"<redacted>")
            .field("model", &self.model)
            .field("system_prompt_bytes", &self.system_prompt.len())
            .field("execution", &self.execution)
            .finish()
    }
}

impl StoredSessionConfig {
    pub(crate) fn new(
        session_id: SessionId,
        created_at: Timestamp,
        updated_at: Timestamp,
        workspace_root: PathBuf,
        model: StoredModelConfig,
        system_prompt: String,
        execution: StoredExecutionConfig,
    ) -> Result<Self, StoreError> {
        let workspace_root = validate_workspace_root(&workspace_root)?;
        if !valid_system_prompt(&system_prompt) {
            return Err(StoreError::InvalidConfig);
        }
        Ok(Self {
            format_version: FORMAT_VERSION,
            session_id,
            created_at,
            updated_at,
            workspace_root,
            model,
            system_prompt,
            execution,
        })
    }

    pub(crate) const fn format_version(&self) -> u8 {
        self.format_version
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn created_at(&self) -> &Timestamp {
        &self.created_at
    }

    pub(crate) fn updated_at(&self) -> &Timestamp {
        &self.updated_at
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) const fn model(&self) -> &StoredModelConfig {
        &self.model
    }

    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) fn execution(&self) -> &StoredExecutionConfig {
        &self.execution
    }
}

impl Serialize for StoredModelConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("StoredModelConfig", 2)?;
        state.serialize_field("provider", self.selection().provider_id())?;
        state.serialize_field("model", self.selection().model_id())?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredModelConfigRead {
    provider: ProviderId,
    model: ModelId,
}

impl<'de> Deserialize<'de> for StoredModelConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = StoredModelConfigRead::deserialize(deserializer)?;
        Ok(Self::new(ModelSelection::new(value.provider, value.model)))
    }
}

impl Serialize for StoredCompactionConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("StoredCompactionConfig", 2)?;
        state.serialize_field("trigger_tokens", &self.trigger_tokens())?;
        state.serialize_field("target_tokens", &self.target_tokens())?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCompactionConfigRead {
    trigger_tokens: u64,
    target_tokens: u64,
}

impl<'de> Deserialize<'de> for StoredCompactionConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = StoredCompactionConfigRead::deserialize(deserializer)?;
        Self::new(value.trigger_tokens, value.target_tokens).map_err(D::Error::custom)
    }
}

impl Serialize for StoredSessionConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let workspace_root = self
            .workspace_root()
            .to_str()
            .ok_or_else(|| S::Error::custom("workspace root is not UTF-8"))?;
        let mut state = serializer.serialize_struct("StoredSessionConfig", 10)?;
        state.serialize_field("format_version", &self.format_version)?;
        state.serialize_field("session_id", &self.session_id)?;
        state.serialize_field("created_at", self.created_at())?;
        state.serialize_field("updated_at", self.updated_at())?;
        state.serialize_field("workspace_root", workspace_root)?;
        state.serialize_field("model", self.model())?;
        state.serialize_field("system_prompt", self.system_prompt())?;
        state.serialize_field("enabled_tools", self.execution().enabled_tools())?;
        state.serialize_field("compaction", self.execution().compaction())?;
        state.serialize_field("max_tool_rounds", &self.execution().max_tool_rounds())?;
        state.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionConfigRead {
    format_version: u8,
    session_id: SessionId,
    created_at: Timestamp,
    updated_at: Timestamp,
    workspace_root: String,
    model: StoredModelConfig,
    system_prompt: String,
    enabled_tools: Vec<ToolName>,
    compaction: StoredCompactionConfig,
    max_tool_rounds: u8,
}

impl<'de> Deserialize<'de> for StoredSessionConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = StoredSessionConfigRead::deserialize(deserializer)?;
        if value.format_version != FORMAT_VERSION {
            return Err(D::Error::custom("unsupported session format version"));
        }
        let mut enabled_tools = BTreeSet::new();
        for tool in value.enabled_tools {
            if !enabled_tools.insert(tool) {
                return Err(D::Error::custom("duplicate enabled tool"));
            }
        }
        let execution =
            StoredExecutionConfig::new(enabled_tools, value.compaction, value.max_tool_rounds)
                .map_err(D::Error::custom)?;
        let config = Self::new(
            value.session_id,
            value.created_at,
            value.updated_at,
            value.workspace_root.into(),
            value.model,
            value.system_prompt,
            execution,
        )
        .map_err(D::Error::custom)?;
        Ok(config)
    }
}

#[derive(Clone)]
pub(crate) struct SessionStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    sessions: PathBuf,
    worker: Arc<WorkerOwner>,
    open_sessions: Arc<Mutex<BTreeSet<SessionId>>>,
}

pub(crate) struct SessionRegistration {
    open_sessions: Arc<Mutex<BTreeSet<SessionId>>>,
    id: SessionId,
}

impl Drop for SessionRegistration {
    fn drop(&mut self) {
        lock_open_sessions(&self.open_sessions).remove(&self.id);
    }
}

impl fmt::Debug for SessionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionStore")
            .field("root", &"<redacted>")
            .finish()
    }
}

impl SessionStore {
    pub(crate) async fn open(root: PathBuf) -> Result<Self, StoreError> {
        let (worker, readiness) = WorkerOwner::start(root)?;
        let sessions = readiness.await.map_err(|_| StoreError::WorkerFailed)??;
        Ok(Self {
            inner: Arc::new(StoreInner {
                sessions,
                worker,
                open_sessions: Arc::new(Mutex::new(BTreeSet::new())),
            }),
        })
    }

    pub(crate) async fn create(&self, config: &StoredSessionConfig) -> Result<(), StoreError> {
        let sessions = self.inner.sessions.clone();
        let config = config.clone();
        self.inner
            .worker
            .run(move || create_sync(&sessions, &config))
            .await
    }

    pub(crate) async fn load_config(
        &self,
        id: SessionId,
    ) -> Result<StoredSessionConfig, StoreError> {
        let sessions = self.inner.sessions.clone();
        self.inner
            .worker
            .run(move || load_config_sync(&sessions, id))
            .await
    }

    pub(crate) async fn list(&self) -> Result<Vec<SessionId>, StoreError> {
        let sessions = self.inner.sessions.clone();
        self.inner.worker.run(move || list_sync(&sessions)).await
    }

    pub(crate) async fn delete(&self, id: SessionId) -> Result<(), StoreError> {
        let sessions = self.inner.sessions.clone();
        let open_sessions = Arc::clone(&self.inner.open_sessions);
        self.inner
            .worker
            .run(move || delete_sync(&sessions, &open_sessions, id))
            .await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), StoreError> {
        self.inner.worker.shutdown().await
    }

    pub(crate) async fn open_registration(
        &self,
        id: SessionId,
    ) -> Result<SessionRegistration, StoreError> {
        let sessions = self.inner.sessions.clone();
        let open_sessions = Arc::clone(&self.inner.open_sessions);
        self.inner
            .worker
            .run(move || {
                load_config_sync(&sessions, id)?;
                let mut open = lock_open_sessions(&open_sessions);
                if !open.insert(id) {
                    return Err(StoreError::Busy);
                }
                drop(open);
                Ok(SessionRegistration { open_sessions, id })
            })
            .await
    }
}

type BlockingJob = Box<dyn FnOnce() + Send + 'static>;
type WorkerReadiness = oneshot::Receiver<Result<PathBuf, StoreError>>;

struct WorkerState {
    sender: Option<SyncSender<BlockingJob>>,
    handle: Option<JoinHandle<()>>,
    closing: bool,
    worker_failed: bool,
}

struct WorkerOwner {
    state: Mutex<WorkerState>,
    shutdown_gate: AsyncMutex<()>,
    exit: watch::Receiver<bool>,
}

struct WorkerExitGuard {
    sender: watch::Sender<bool>,
}

impl WorkerOwner {
    fn start(root: PathBuf) -> Result<(Arc<Self>, WorkerReadiness), StoreError> {
        let (sender, receiver) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let (exit_sender, exit) = watch::channel(false);
        let (ready_sender, ready_receiver) = oneshot::channel();
        let handle = std::thread::Builder::new()
            .name("minicore-session-store-worker".to_owned())
            .spawn(move || worker_loop(root, receiver, ready_sender, exit_sender))
            .map_err(|_| StoreError::WorkerFailed)?;
        let owner = Arc::new(Self {
            state: Mutex::new(WorkerState {
                sender: Some(sender),
                handle: Some(handle),
                closing: false,
                worker_failed: false,
            }),
            shutdown_gate: AsyncMutex::new(()),
            exit,
        });
        Ok((owner, ready_receiver))
    }

    fn lock_state(&self) -> MutexGuard<'_, WorkerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn admit(&self, job: BlockingJob) -> Result<(), StoreError> {
        let mut state = self.lock_state();
        if state.closing {
            return Err(StoreError::Closing);
        }
        if state.worker_failed {
            return Err(StoreError::WorkerFailed);
        }
        let Some(sender) = state.sender.as_ref() else {
            state.worker_failed = true;
            return Err(StoreError::WorkerFailed);
        };
        match sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(StoreError::Busy),
            Err(TrySendError::Disconnected(_)) => {
                state.sender.take();
                state.worker_failed = true;
                Err(StoreError::WorkerFailed)
            }
        }
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.admit(Box::new(move || {
            let result = match catch_unwind(AssertUnwindSafe(operation)) {
                Ok(result) => result,
                Err(_) => Err(StoreError::WorkerFailed),
            };
            let _ = sender.send(result);
        }))?;
        receiver.await.map_err(|_| StoreError::WorkerFailed)?
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "the async mutex serializes the single store worker shutdown"
    )]
    async fn shutdown(&self) -> Result<(), StoreError> {
        let _shutdown = self.shutdown_gate.lock().await;
        {
            let mut state = self.lock_state();
            state.closing = true;
            state.sender.take();
        }

        let mut exit = self.exit.clone();
        let mut exit_failed = false;
        while !*exit.borrow() {
            if exit.changed().await.is_err() {
                exit_failed = true;
                break;
            }
        }
        loop {
            let finished = self
                .lock_state()
                .handle
                .as_ref()
                .is_none_or(JoinHandle::is_finished);
            if finished {
                break;
            }
            tokio::task::yield_now().await;
        }
        let handle = self.lock_state().handle.take();
        if let Some(handle) = handle {
            if exit_failed || handle.join().is_err() {
                self.lock_state().worker_failed = true;
            }
        }
        if self.lock_state().worker_failed {
            Err(StoreError::WorkerFailed)
        } else {
            Ok(())
        }
    }
}

impl Drop for WorkerOwner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closing = true;
        state.sender.take();
        let handle = state.handle.take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                state.worker_failed = true;
            }
        }
    }
}

fn worker_loop(
    root: PathBuf,
    receiver: Receiver<BlockingJob>,
    ready: oneshot::Sender<Result<PathBuf, StoreError>>,
    exit: watch::Sender<bool>,
) {
    let _exit_guard = WorkerExitGuard { sender: exit };
    let bootstrap = catch_unwind(AssertUnwindSafe(|| bootstrap_sync(&root)))
        .unwrap_or(Err(StoreError::WorkerFailed));
    let (sessions, lock_file) = match bootstrap {
        Ok(ready_value) => ready_value,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(sessions.clone())).is_err() {
        return;
    }
    while let Ok(job) = receiver.recv() {
        let _ = catch_unwind(AssertUnwindSafe(job));
    }
    drop(lock_file);
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(true);
    }
}

fn bootstrap_sync(root: &Path) -> Result<(PathBuf, File), StoreError> {
    fs::create_dir_all(root).map_err(map_io)?;
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("runtime.lock"))
        .map_err(map_io)?;
    match lock_file.try_lock_exclusive() {
        Ok(true) => {}
        Ok(false) => return Err(StoreError::InUse),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(StoreError::InUse);
        }
        Err(_) => return Err(StoreError::Io),
    }
    let sessions = root.join("sessions");
    fs::create_dir_all(&sessions).map_err(map_io)?;
    remove_orphan_temps(&sessions)?;
    Ok((sessions, lock_file))
}

fn create_sync(sessions: &Path, config: &StoredSessionConfig) -> Result<(), StoreError> {
    let final_dir = sessions.join(config.session_id().to_string());
    match fs::symlink_metadata(&final_dir) {
        Ok(_) => return Err(StoreError::AlreadyExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(StoreError::Io),
    }
    let temp_dir = create_temp_dir(sessions)?;
    let result = (|| {
        write_session_json(&temp_dir, config)?;
        write_empty_conversation(&temp_dir)?;
        fs::rename(&temp_dir, &final_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                StoreError::AlreadyExists
            } else {
                StoreError::Io
            }
        })?;
        sync_directory_best_effort(sessions);
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            if remove_temp_path(&temp_dir).is_err() {
                return Err(StoreError::CleanupFailed);
            }
            Err(error)
        }
    }
}

fn load_config_sync(sessions: &Path, id: SessionId) -> Result<StoredSessionConfig, StoreError> {
    let session_dir = sessions.join(id.to_string());
    validate_session_dir(&session_dir)?;
    let path = session_dir.join("session.json");
    let bytes = read_bounded_file(&path)?;
    let config: StoredSessionConfig =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::Corrupt)?;
    if config.session_id() != id || config.format_version() != FORMAT_VERSION {
        return Err(StoreError::Corrupt);
    }
    Ok(config)
}

fn list_sync(sessions: &Path) -> Result<Vec<SessionId>, StoreError> {
    let mut ids = BTreeSet::new();
    for entry in fs::read_dir(sessions).map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(StoreError::Corrupt)?;
        let path = entry.path();
        if name.starts_with(TEMP_PREFIX) {
            remove_temp_path(&path)?;
            continue;
        }
        let id = name.parse::<SessionId>().map_err(|_| StoreError::Corrupt)?;
        load_config_sync(sessions, id)?;
        ids.insert(id);
    }
    Ok(ids.into_iter().collect())
}

fn delete_sync(
    sessions: &Path,
    open_sessions: &Mutex<BTreeSet<SessionId>>,
    id: SessionId,
) -> Result<(), StoreError> {
    if lock_open_sessions(open_sessions).contains(&id) {
        return Err(StoreError::Busy);
    }
    let path = sessions.join(id.to_string());
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            StoreError::NotFound
        } else {
            StoreError::Io
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(StoreError::Corrupt);
    }
    fs::remove_dir_all(path).map_err(map_io)
}

fn validate_session_dir(path: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            StoreError::NotFound
        } else {
            StoreError::Io
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(StoreError::Corrupt);
    }
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(path).map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or(StoreError::Corrupt)?
            .to_owned();
        let metadata = fs::symlink_metadata(entry.path()).map_err(map_io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(StoreError::Corrupt);
        }
        names.insert(name);
    }
    if names != BTreeSet::from(["conversation.jsonl".to_owned(), "session.json".to_owned()]) {
        return Err(StoreError::Corrupt);
    }
    Ok(())
}

fn read_bounded_file(path: &Path) -> Result<Vec<u8>, StoreError> {
    let file = File::open(path).map_err(map_io)?;
    let mut bytes = Vec::with_capacity(MAX_SESSION_JSON_BYTES + 1);
    file.take((MAX_SESSION_JSON_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(map_io)?;
    if bytes.len() > MAX_SESSION_JSON_BYTES {
        return Err(StoreError::TooLarge);
    }
    Ok(bytes)
}

fn write_session_json(temp_dir: &Path, config: &StoredSessionConfig) -> Result<(), StoreError> {
    let mut bytes = serde_json::to_vec(config).map_err(|_| StoreError::InvalidConfig)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_SESSION_JSON_BYTES {
        return Err(StoreError::TooLarge);
    }
    let path = temp_dir.join("session.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(map_io)?;
    file.write_all(&bytes).map_err(map_io)?;
    file.flush().map_err(map_io)?;
    file.sync_all().map_err(map_io)
}

fn write_empty_conversation(temp_dir: &Path) -> Result<(), StoreError> {
    let path = temp_dir.join("conversation.jsonl");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(map_io)?;
    file.flush().map_err(map_io)?;
    file.sync_all().map_err(map_io)
}

fn create_temp_dir(sessions: &Path) -> Result<PathBuf, StoreError> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| StoreError::Io)?;
        let mut name = String::from(TEMP_PREFIX);
        for byte in bytes {
            let _ = write!(name, "{byte:02x}");
        }
        let path = sessions.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(StoreError::Io),
        }
    }
    Err(StoreError::Busy)
}

fn remove_orphan_temps(sessions: &Path) -> Result<(), StoreError> {
    for entry in fs::read_dir(sessions).map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
        {
            remove_temp_path(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_temp_path(path: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(map_io)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(map_io)
    } else {
        fs::remove_file(path).map_err(map_io)
    }
}

// Parent-directory durability is best effort because directory fsync support differs by platform;
// file contents are flushed and synced before publication.
fn sync_directory_best_effort(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn validate_workspace_root(path: &Path) -> Result<PathBuf, StoreError> {
    let Some(raw) = path.to_str() else {
        return Err(StoreError::InvalidConfig);
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || raw
            .split(['/', '\\'])
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(StoreError::InvalidConfig);
    }
    Ok(path.to_owned())
}

fn valid_system_prompt(value: &str) -> bool {
    value.len() <= MAX_SYSTEM_PROMPT_BYTES
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

fn lock_open_sessions(
    open_sessions: &Mutex<BTreeSet<SessionId>>,
) -> MutexGuard<'_, BTreeSet<SessionId>> {
    open_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn map_io(error: io::Error) -> StoreError {
    if error.kind() == io::ErrorKind::NotFound {
        StoreError::NotFound
    } else {
        StoreError::Io
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::mpsc::channel;
    use std::task::{Context, Poll};

    use super::*;

    fn unique_root() -> PathBuf {
        std::env::temp_dir().join(format!("minicore-p4-{}", SessionId::new().unwrap()))
    }

    fn sample_config(id: SessionId, workspace_root: &Path) -> StoredSessionConfig {
        let model = StoredModelConfig::new(ModelSelection::new(
            "anthropic".parse().unwrap(),
            "claude-sonnet".parse().unwrap(),
        ));
        let execution = StoredExecutionConfig::new(
            BTreeSet::from(["read_file".parse().unwrap(), "write_file".parse().unwrap()]),
            StoredCompactionConfig::new(80_000, 30_000).unwrap(),
            16,
        )
        .unwrap();
        let created_at: Timestamp = "2026-08-19T12:34:56.789Z".parse().unwrap();
        StoredSessionConfig::new(
            id,
            created_at.clone(),
            created_at,
            workspace_root.to_owned(),
            model,
            "You are a coding agent\nUse bounded tools.".to_owned(),
            execution,
        )
        .unwrap()
    }

    async fn start_worker() -> (Arc<WorkerOwner>, PathBuf, PathBuf) {
        let root = unique_root();
        let (owner, ready) = WorkerOwner::start(root.clone()).unwrap();
        let sessions = ready.await.unwrap().unwrap();
        (owner, root, sessions)
    }

    #[test]
    fn stored_config_is_checked_and_has_canonical_field_order() {
        let workspace = PathBuf::from("/tmp/p4-workspace");
        let id = SessionId::new().unwrap();
        let config = sample_config(id, &workspace);
        let json_text = serde_json::to_string(&config).unwrap();
        assert_eq!(
            json_text,
            format!(
                "{{\"format_version\":2,\"session_id\":\"{}\",\"created_at\":\"2026-08-19T12:34:56.789Z\",\"updated_at\":\"2026-08-19T12:34:56.789Z\",\"workspace_root\":\"/tmp/p4-workspace\",\"model\":{{\"provider\":\"anthropic\",\"model\":\"claude-sonnet\"}},\"system_prompt\":\"You are a coding agent\\nUse bounded tools.\",\"enabled_tools\":[\"read_file\",\"write_file\"],\"compaction\":{{\"trigger_tokens\":80000,\"target_tokens\":30000}},\"max_tool_rounds\":16}}",
                id
            )
        );
        assert!(!json_text.contains("credential"));
        assert!(!json_text.contains("descriptor"));
        let debug = format!("{config:?}");
        assert!(!debug.contains("/tmp/p4-workspace"));
        assert!(!debug.contains("You are a coding agent"));
        assert_eq!(
            serde_json::from_str::<StoredSessionConfig>(&json_text).unwrap(),
            config
        );
        let mut wrong_version = serde_json::to_value(&config).unwrap();
        wrong_version["format_version"] = serde_json::json!(1);
        assert!(serde_json::from_value::<StoredSessionConfig>(wrong_version).is_err());
        let mut unknown = serde_json::to_value(&config).unwrap();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StoredSessionConfig>(unknown).is_err());
        let mut duplicate_tools = serde_json::to_value(&config).unwrap();
        duplicate_tools["enabled_tools"] = serde_json::json!(["read_file", "read_file"]);
        assert!(serde_json::from_value::<StoredSessionConfig>(duplicate_tools).is_err());
        let mut too_many_tools = serde_json::to_value(&config).unwrap();
        too_many_tools["enabled_tools"] = serde_json::json!(
            (0..65)
                .map(|index| format!("tool_{index}"))
                .collect::<Vec<_>>()
        );
        assert!(serde_json::from_value::<StoredSessionConfig>(too_many_tools).is_err());
        let mut unknown_model_field = serde_json::to_value(&config).unwrap();
        unknown_model_field["model"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StoredSessionConfig>(unknown_model_field).is_err());
    }

    #[test]
    fn stored_config_constraints_are_checked() {
        assert!(StoredCompactionConfig::new(0, 0).is_err());
        assert!(StoredCompactionConfig::new(10, 10).is_err());
        assert!(StoredCompactionConfig::new(10, 11).is_err());
        let model = StoredModelConfig::new(ModelSelection::new(
            "anthropic".parse().unwrap(),
            "claude".parse().unwrap(),
        ));
        let compaction = StoredCompactionConfig::new(2, 1).unwrap();
        let too_many_tools = (0..65)
            .map(|index| format!("tool_{index}").parse().unwrap())
            .collect::<BTreeSet<_>>();
        assert!(StoredExecutionConfig::new(too_many_tools, compaction.clone(), 1).is_err());
        for rounds in [0, 65] {
            assert!(
                StoredExecutionConfig::new(BTreeSet::new(), compaction.clone(), rounds).is_err()
            );
        }
        for root in [
            PathBuf::from("relative"),
            PathBuf::from("/tmp/./workspace"),
            PathBuf::from("/tmp/../workspace"),
        ] {
            assert!(
                StoredSessionConfig::new(
                    SessionId::new().unwrap(),
                    "2026-08-19T12:34:56.789Z".parse().unwrap(),
                    "2026-08-19T12:34:56.789Z".parse().unwrap(),
                    root,
                    model.clone(),
                    "system".to_owned(),
                    StoredExecutionConfig::new(BTreeSet::new(), compaction.clone(), 1).unwrap(),
                )
                .is_err()
            );
        }
        assert!(
            StoredSessionConfig::new(
                SessionId::new().unwrap(),
                "2026-08-19T12:34:56.789Z".parse().unwrap(),
                "2026-08-19T12:34:56.789Z".parse().unwrap(),
                PathBuf::from("/tmp/workspace"),
                model.clone(),
                "x".repeat(262_145),
                StoredExecutionConfig::new(BTreeSet::new(), compaction.clone(), 1).unwrap(),
            )
            .is_err()
        );
        assert!(
            StoredSessionConfig::new(
                SessionId::new().unwrap(),
                "2026-08-19T12:34:56.789Z".parse().unwrap(),
                "2026-08-19T12:34:56.789Z".parse().unwrap(),
                PathBuf::from("/tmp/workspace"),
                model,
                "bad\u{0001}prompt".to_owned(),
                StoredExecutionConfig::new(BTreeSet::new(), compaction, 1).unwrap(),
            )
            .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn store_create_load_list_delete_has_exact_v2_layout() {
        let root = unique_root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        let config = sample_config(id, Path::new("/tmp/p4-workspace"));
        store.create(&config).await.unwrap();
        assert!(!format!("{store:?}").contains(&root.to_string_lossy().to_string()));
        assert_eq!(store.load_config(id).await.unwrap(), config);
        assert_eq!(store.list().await.unwrap(), vec![id]);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        let session_dir = root.join("sessions").join(id.to_string());
        let entries = fs::read_dir(&session_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            entries,
            BTreeSet::from(["conversation.jsonl".to_owned(), "session.json".to_owned()])
        );
        let session_json = fs::read_to_string(session_dir.join("session.json")).unwrap();
        assert!(session_json.ends_with('\n'));
        assert_eq!(
            fs::read(session_dir.join("conversation.jsonl")).unwrap(),
            Vec::<u8>::new()
        );
        store.delete(id).await.unwrap();
        assert_eq!(store.list().await.unwrap(), Vec::<SessionId>::new());
        assert_eq!(store.load_config(id).await, Err(StoreError::NotFound));
        assert_eq!(store.delete(id).await, Err(StoreError::NotFound));
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn store_list_is_sorted_and_errors_are_redacted() {
        let root = unique_root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let first = SessionId::new().unwrap();
        let second = SessionId::new().unwrap();
        store
            .create(&sample_config(first, Path::new("/tmp/workspace")))
            .await
            .unwrap();
        store
            .create(&sample_config(second, Path::new("/tmp/workspace")))
            .await
            .unwrap();
        let mut expected = vec![first, second];
        expected.sort();
        assert_eq!(store.list().await.unwrap(), expected);
        for error in [
            StoreError::InUse,
            StoreError::NotFound,
            StoreError::AlreadyExists,
            StoreError::Busy,
            StoreError::InvalidConfig,
            StoreError::Corrupt,
            StoreError::TooLarge,
            StoreError::Io,
            StoreError::CleanupFailed,
            StoreError::WorkerFailed,
            StoreError::Closing,
        ] {
            assert!(!format!("{error:?}").contains(&root.to_string_lossy().to_string()));
        }
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn store_duplicate_lock_release_and_shutdown_are_typed() {
        let root = unique_root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let second = SessionStore::open(root.clone()).await;
        assert!(matches!(second, Err(StoreError::InUse)));
        let id = SessionId::new().unwrap();
        let config = sample_config(id, Path::new("/tmp/workspace"));
        store.create(&config).await.unwrap();
        assert_eq!(store.create(&config).await, Err(StoreError::AlreadyExists));
        let retained_clone = store.clone();
        store.shutdown().await.unwrap();
        store.shutdown().await.unwrap();
        assert_eq!(store.list().await, Err(StoreError::Closing));
        let reopened = SessionStore::open(root.clone()).await.unwrap();
        assert_eq!(reopened.list().await.unwrap(), vec![id]);
        reopened.shutdown().await.unwrap();
        retained_clone.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn store_open_cleans_only_orphan_temps_and_rejects_malformed_sessions() {
        let root = unique_root();
        fs::create_dir_all(root.join("sessions").join(".session-tmp-orphan")).unwrap();
        fs::write(
            root.join("sessions")
                .join(".session-tmp-orphan")
                .join("partial"),
            b"x",
        )
        .unwrap();
        let malformed = root.join("sessions").join("not-a-session");
        fs::create_dir_all(&malformed).unwrap();
        let store = SessionStore::open(root.clone()).await.unwrap();
        assert!(!root.join("sessions").join(".session-tmp-orphan").exists());
        assert_eq!(store.list().await, Err(StoreError::Corrupt));
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn store_load_rejects_bounded_plus_one_and_id_mismatch() {
        let root = unique_root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        store
            .create(&sample_config(id, Path::new("/tmp/workspace")))
            .await
            .unwrap();
        let session_json = root
            .join("sessions")
            .join(id.to_string())
            .join("session.json");
        fs::write(&session_json, vec![b'x'; MAX_SESSION_JSON_BYTES + 1]).unwrap();
        assert_eq!(store.load_config(id).await, Err(StoreError::TooLarge));
        let other_id = SessionId::new().unwrap();
        let other_json =
            serde_json::to_vec(&sample_config(other_id, Path::new("/tmp/workspace"))).unwrap();
        fs::write(&session_json, [other_json.as_slice(), b"\n"].concat()).unwrap();
        assert_eq!(store.load_config(id).await, Err(StoreError::Corrupt));
        fs::write(&session_json, b"{\"format_version\":2}\n").unwrap();
        assert_eq!(store.load_config(id).await, Err(StoreError::Corrupt));
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_registration_is_atomic_and_dropped_future_rolls_back() {
        let root = unique_root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        assert!(matches!(
            store.open_registration(id).await,
            Err(StoreError::NotFound)
        ));
        store
            .create(&sample_config(id, Path::new("/tmp/workspace")))
            .await
            .unwrap();
        let registration = store.open_registration(id).await.unwrap();
        assert!(matches!(
            store.open_registration(id).await,
            Err(StoreError::Busy)
        ));
        assert_eq!(store.delete(id).await, Err(StoreError::Busy));
        drop(registration);
        store.delete(id).await.unwrap();

        store
            .create(&sample_config(id, Path::new("/tmp/workspace")))
            .await
            .unwrap();
        let (started_sender, started_receiver) = channel();
        let (release_sender, release_receiver) = channel();
        store
            .inner
            .worker
            .admit(Box::new(move || {
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
            }))
            .unwrap();
        started_receiver.recv().unwrap();
        let mut future = Box::pin(store.open_registration(id));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        drop(future);
        release_sender.send(()).unwrap();
        store.delete(id).await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_queue_is_bounded_and_submitted_future_is_owner_owned() {
        let (owner, root, _sessions) = start_worker().await;
        let (started_sender, started_receiver) = channel();
        let (release_sender, release_receiver) = channel();
        owner
            .admit(Box::new(move || {
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
            }))
            .unwrap();
        started_receiver.recv().unwrap();
        for _ in 0..WORKER_QUEUE_CAPACITY - 1 {
            owner.admit(Box::new(|| {})).unwrap();
        }
        let (drained_sender, drained_receiver) = channel();
        owner
            .admit(Box::new(move || drained_sender.send(()).unwrap()))
            .unwrap();
        assert_eq!(owner.admit(Box::new(|| {})), Err(StoreError::Busy));
        release_sender.send(()).unwrap();
        drained_receiver.recv().unwrap();
        let (run_release_sender, run_release_receiver) = channel();
        {
            let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
            let future = owner.run(move || {
                let _ = started_sender.send(());
                run_release_receiver
                    .recv()
                    .map_err(|_| StoreError::WorkerFailed)?;
                Ok::<_, StoreError>(())
            });
            tokio::pin!(future);
            tokio::select! {
                result = &mut future => panic!("submitted future completed too early: {result:?}"),
                started = started_receiver => started.expect("worker started"),
            }
        }
        run_release_sender.send(()).unwrap();
        owner.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_and_bounded_read_are_source_and_behavior_contracts() {
        let root = unique_root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.json");
        fs::write(&path, vec![b'x'; MAX_SESSION_JSON_BYTES]).unwrap();
        assert_eq!(
            read_bounded_file(&path).unwrap().len(),
            MAX_SESSION_JSON_BYTES
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert_eq!(read_bounded_file(&path), Err(StoreError::TooLarge));
        let source = include_str!("store.rs");
        assert!(source.contains("take((MAX_SESSION_JSON_BYTES + 1) as u64)"));
        assert!(source.contains("StoreError::CleanupFailed"));
        fs::remove_dir_all(root).unwrap();
    }
}
