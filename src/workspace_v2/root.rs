use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use cap_primitives::fs::FollowSymlinks;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{oneshot, watch};

use super::RelativePath;

const TEMP_NAME_ATTEMPTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkspaceError {
    #[error("workspace root is unavailable")]
    RootUnavailable,
    #[error("workspace root is not a directory")]
    RootNotDirectory,
    #[error("workspace root is a symbolic link")]
    RootSymlink,
    #[error("workspace path is invalid for this operation")]
    InvalidPath,
    #[error("workspace access is read-only")]
    ReadOnly,
    #[error("workspace target was not found")]
    NotFound,
    #[error("workspace parent directory is missing")]
    MissingParent,
    #[error("workspace target is a directory")]
    IsDirectory,
    #[error("workspace target is a symbolic link")]
    IsSymlink,
    #[error("workspace target is not a regular file")]
    NotRegularFile,
    #[error("workspace text exceeds its bound")]
    TooLarge,
    #[error("workspace text is not valid UTF-8")]
    InvalidUtf8,
    #[error("workspace directory listing exceeds its bounds")]
    ListingTooLarge,
    #[error("workspace directory entry name is not valid UTF-8")]
    InvalidEntryName,
    #[error("workspace I/O failed")]
    Io,
    #[error("workspace temporary cleanup failed")]
    CleanupFailed,
    #[error("workspace is closing")]
    Closing,
    #[error("workspace worker failed")]
    WorkerFailed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DirectoryEntry {
    name: String,
    kind: DirectoryEntryKind,
}

impl DirectoryEntry {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn kind(&self) -> DirectoryEntryKind {
        self.kind
    }
}

type BlockingJob = Box<dyn FnOnce(&Dir) + Send + 'static>;

struct WorkspaceInner {
    blocking: Arc<BlockingOwner>,
    root_path: PathBuf,
}

struct BlockingState {
    sender: Option<mpsc::Sender<BlockingJob>>,
    handle: Option<std::thread::JoinHandle<()>>,
    closing: bool,
    worker_failed: bool,
}

struct BlockingOwner {
    state: Mutex<BlockingState>,
    shutdown_gate: tokio::sync::Mutex<()>,
    exit: watch::Receiver<bool>,
}

struct WorkerExitGuard {
    sender: watch::Sender<bool>,
}

/// A single capability-backed workspace root.
///
/// Each bounded filesystem operation is admitted as one owner-tracked job on
/// a dedicated named worker thread and returns its result through a oneshot
/// channel. Dropping an operation future closes its result receiver, but the
/// worker retains queued and running jobs until the channel is closed and the
/// exact thread exits. Jobs are bounded filesystem closures and do not await
/// Tokio tasks. Filesystem wall-clock completion is not a timeout guarantee.
/// [`Workspace::shutdown`] closes admission and joins the worker.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<WorkspaceInner>,
    access: WorkspaceAccess,
}

impl Workspace {
    pub const MAX_READ_BYTES: usize = 256 * 1024;
    pub const MAX_WRITE_BYTES: usize = 256 * 1024;
    pub const MAX_LIST_ENTRIES: usize = 1_000;
    pub const MAX_LIST_NAME_BYTES: usize = 4 * 1024;
    pub const MAX_LIST_TOTAL_NAME_BYTES: usize = 256 * 1024;

    pub fn open<P: AsRef<Path>>(root: P, access: WorkspaceAccess) -> Result<Self, WorkspaceError> {
        let root = root.as_ref();
        if !root.is_absolute() || has_lexical_dot_component(root) {
            return Err(WorkspaceError::InvalidPath);
        }

        let root_dir = match open_root_capability(root) {
            Ok(root_dir) => root_dir,
            Err(error) => {
                if let Ok(metadata) = fs::symlink_metadata(root) {
                    if metadata.file_type().is_symlink() {
                        return Err(WorkspaceError::RootSymlink);
                    }
                    if !metadata.is_dir() {
                        return Err(WorkspaceError::RootNotDirectory);
                    }
                }
                return Err(error);
            }
        };
        let root_dir = Arc::new(root_dir);
        let blocking = Arc::new(BlockingOwner::new(Arc::clone(&root_dir))?);
        Ok(Self {
            inner: Arc::new(WorkspaceInner {
                blocking,
                root_path: root.to_owned(),
            }),
            access,
        })
    }

    pub const fn access(&self) -> WorkspaceAccess {
        self.access
    }

    /// Closes admission and joins every previously admitted operation. The
    /// async shutdown gate serializes concurrent callers while the worker exit
    /// notification keeps the executor nonblocking until synchronous join is safe.
    /// Repeated calls observe the same closed owner and sticky worker result.
    /// Omitting this method invokes the synchronous Drop fallback, which may
    /// block the dropping thread; production Session ownership must call it.
    pub async fn shutdown(&self) -> Result<(), WorkspaceError> {
        self.inner.blocking.shutdown().await
    }

    /// Validates an optional model-relative directory through the captured
    /// capability before returning the corresponding host path for a child
    /// process. This is pre-spawn validation on a trusted, non-adversarial host
    /// filesystem. `Command::current_dir(Path)` cannot receive the captured
    /// `Dir` identity: this method does not claim protection against a later
    /// host-filesystem replacement and does not provide a process sandbox.
    /// File operations remain capability-safe; this path is only a process
    /// working-directory hint.
    pub async fn command_cwd(
        &self,
        path: Option<&RelativePath>,
    ) -> Result<PathBuf, WorkspaceError> {
        if self.inner.blocking.is_closing() {
            return Err(WorkspaceError::Closing);
        }
        let relative = path.cloned().unwrap_or_default();
        let root_path = self.inner.root_path.clone();
        self.run_blocking(move |root_dir| {
            validate_command_directory(root_dir, &relative)?;
            Ok(if relative.is_empty() {
                root_path
            } else {
                root_path.join(relative.as_str())
            })
        })
        .await
    }

    /// Reads one bounded UTF-8 regular file through the captured capability.
    pub async fn read_text(
        &self,
        path: &RelativePath,
        max_bytes: usize,
    ) -> Result<String, WorkspaceError> {
        if self.inner.blocking.is_closing() {
            return Err(WorkspaceError::Closing);
        }
        if path.is_empty() || max_bytes > Self::MAX_READ_BYTES {
            return Err(if path.is_empty() {
                WorkspaceError::InvalidPath
            } else {
                WorkspaceError::TooLarge
            });
        }

        let target = path.as_path().to_owned();
        self.run_blocking(move |root_dir| read_text_sync(root_dir, &target, max_bytes))
            .await
    }

    /// Lists one bounded set of direct entries through the captured capability.
    pub async fn list(
        &self,
        path: &RelativePath,
        limit: usize,
    ) -> Result<Vec<DirectoryEntry>, WorkspaceError> {
        if self.inner.blocking.is_closing() {
            return Err(WorkspaceError::Closing);
        }
        if limit > Self::MAX_LIST_ENTRIES {
            return Err(WorkspaceError::ListingTooLarge);
        }

        let target = path.as_path().to_owned();
        self.run_blocking(move |root_dir| list_sync(root_dir, &target, limit))
            .await
    }

    /// Atomically replaces or creates one bounded UTF-8 text file.
    pub async fn write_text(
        &self,
        path: &RelativePath,
        content: &str,
    ) -> Result<(), WorkspaceError> {
        if self.inner.blocking.is_closing() {
            return Err(WorkspaceError::Closing);
        }
        if self.access == WorkspaceAccess::ReadOnly {
            return Err(WorkspaceError::ReadOnly);
        }
        if path.is_empty() {
            return Err(WorkspaceError::InvalidPath);
        }
        if content.len() > Self::MAX_WRITE_BYTES {
            return Err(WorkspaceError::TooLarge);
        }

        let target = path.as_path().to_owned();
        let content = content.as_bytes().to_owned();
        self.run_blocking(move |root_dir| write_text_sync(root_dir, &target, &content))
            .await
    }

    async fn run_blocking<T, F>(&self, operation: F) -> Result<T, WorkspaceError>
    where
        T: Send + 'static,
        F: FnOnce(&Dir) -> Result<T, WorkspaceError> + Send + 'static,
    {
        let blocking = Arc::clone(&self.inner.blocking);
        let (sender, receiver) = oneshot::channel();
        blocking.admit(Box::new(move |root_dir| {
            let result = match catch_unwind(AssertUnwindSafe(|| operation(root_dir))) {
                Ok(result) => result,
                Err(_) => Err(WorkspaceError::WorkerFailed),
            };
            let _ = sender.send(result);
        }))?;

        receiver.await.map_err(|_| WorkspaceError::WorkerFailed)?
    }
}

impl BlockingOwner {
    fn new(root_dir: Arc<Dir>) -> Result<Self, WorkspaceError> {
        let (sender, receiver) = mpsc::channel();
        let (exit_sender, exit) = watch::channel(false);
        let handle = std::thread::Builder::new()
            .name("minicore-workspace-worker".to_owned())
            .spawn(move || worker_loop(root_dir, receiver, exit_sender))
            .map_err(|_| WorkspaceError::WorkerFailed)?;
        Ok(Self {
            state: Mutex::new(BlockingState {
                sender: Some(sender),
                handle: Some(handle),
                closing: false,
                worker_failed: false,
            }),
            shutdown_gate: tokio::sync::Mutex::new(()),
            exit,
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, BlockingState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_closing(&self) -> bool {
        self.lock_state().closing
    }

    fn admit(&self, job: BlockingJob) -> Result<(), WorkspaceError> {
        let mut state = self.lock_state();
        if state.closing {
            return Err(WorkspaceError::Closing);
        }
        if state.worker_failed {
            return Err(WorkspaceError::WorkerFailed);
        }
        let Some(sender) = state.sender.as_ref() else {
            state.closing = true;
            state.worker_failed = true;
            return Err(WorkspaceError::WorkerFailed);
        };
        if sender.send(job).is_err() {
            state.closing = true;
            state.worker_failed = true;
            state.sender.take();
            return Err(WorkspaceError::WorkerFailed);
        }
        Ok(())
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "the async mutex intentionally serializes the single worker shutdown"
    )]
    async fn shutdown(&self) -> Result<(), WorkspaceError> {
        let _shutdown = self.shutdown_gate.lock().await;
        {
            let mut state = self.lock_state();
            state.closing = true;
            let sender = state.sender.take();
            drop(sender);
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
            let finished = {
                let state = self.lock_state();
                match state.handle.as_ref() {
                    Some(handle) => handle.is_finished(),
                    None => true,
                }
            };
            if finished {
                break;
            }
            tokio::task::yield_now().await;
        }

        let handle = {
            let mut state = self.lock_state();
            state.handle.take()
        };
        if let Some(handle) = handle {
            if exit_failed || handle.join().is_err() {
                self.lock_state().worker_failed = true;
            }
        }

        if self.lock_state().worker_failed {
            Err(WorkspaceError::WorkerFailed)
        } else {
            Ok(())
        }
    }
}

impl Drop for BlockingOwner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closing = true;
        let sender = state.sender.take();
        let handle = state.handle.take();
        drop(sender);
        if let Some(handle) = handle {
            if handle.join().is_err() {
                state.worker_failed = true;
            }
        }
    }
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(true);
    }
}

fn worker_loop(
    root_dir: Arc<Dir>,
    receiver: mpsc::Receiver<BlockingJob>,
    exit: watch::Sender<bool>,
) {
    let _exit_guard = WorkerExitGuard { sender: exit };
    while let Ok(job) = receiver.recv() {
        let _ = catch_unwind(AssertUnwindSafe(|| job(&root_dir)));
    }
}

fn validate_command_directory(
    root_dir: &Dir,
    relative: &RelativePath,
) -> Result<(), WorkspaceError> {
    let mut directory = root_dir.try_clone().map_err(|_| WorkspaceError::Io)?;
    for component in relative
        .as_str()
        .split('/')
        .filter(|component| !component.is_empty())
    {
        let component_path = Path::new(component);
        let metadata = directory
            .symlink_metadata(component_path)
            .map_err(map_command_cwd_error)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceError::IsSymlink);
        }
        if !metadata.is_dir() {
            return Err(WorkspaceError::NotRegularFile);
        }

        let parent_file = directory
            .try_clone()
            .map_err(|_| WorkspaceError::Io)?
            .into_std_file();
        let child_file = cap_primitives::fs::open_dir_nofollow(&parent_file, component_path)
            .map_err(|error| {
                if let Ok(metadata) = directory.symlink_metadata(component_path) {
                    if metadata.file_type().is_symlink() {
                        return WorkspaceError::IsSymlink;
                    }
                }
                map_command_cwd_error(error)
            })?;
        directory = Dir::from_std_file(child_file);
    }
    Ok(())
}

impl fmt::Debug for Workspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Workspace")
            .field("access", &self.access())
            .finish()
    }
}

fn read_text_sync(
    root_dir: &Dir,
    target: &Path,
    max_bytes: usize,
) -> Result<String, WorkspaceError> {
    let mut options = OpenOptions::new();
    options.read(true)._cap_fs_ext_nonblock(true);
    let mut file = root_dir
        .open_with(target, &options)
        .map_err(map_operation_error)?;
    if !file.metadata().map_err(map_operation_error)?.is_file() {
        return Err(WorkspaceError::NotRegularFile);
    }

    let capacity = max_bytes.checked_add(1).ok_or(WorkspaceError::TooLarge)?;
    let mut bytes = vec![0; capacity];
    let mut length = 0;
    while length < bytes.len() {
        let read = file
            .read(&mut bytes[length..])
            .map_err(map_operation_error)?;
        if read == 0 {
            break;
        }
        length += read;
    }
    if length > max_bytes {
        return Err(WorkspaceError::TooLarge);
    }
    bytes.truncate(length);
    String::from_utf8(bytes).map_err(|_| WorkspaceError::InvalidUtf8)
}

fn list_sync(
    root_dir: &Dir,
    target: &Path,
    limit: usize,
) -> Result<Vec<DirectoryEntry>, WorkspaceError> {
    let directory = if target.as_os_str().is_empty() {
        root_dir.entries().map_err(map_operation_error)?
    } else {
        root_dir.read_dir(target).map_err(map_operation_error)?
    };
    let mut entries = Vec::new();
    let mut total_name_bytes = 0usize;

    for entry in directory {
        if entries.len() >= limit {
            return Err(WorkspaceError::ListingTooLarge);
        }
        let entry = entry.map_err(map_operation_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| WorkspaceError::InvalidEntryName)?;
        if name.len() > Workspace::MAX_LIST_NAME_BYTES {
            return Err(WorkspaceError::ListingTooLarge);
        }
        total_name_bytes = total_name_bytes
            .checked_add(name.len())
            .ok_or(WorkspaceError::ListingTooLarge)?;
        if total_name_bytes > Workspace::MAX_LIST_TOTAL_NAME_BYTES {
            return Err(WorkspaceError::ListingTooLarge);
        }

        let file_type = entry.file_type().map_err(map_operation_error)?;
        let kind = if file_type.is_symlink() {
            DirectoryEntryKind::Symlink
        } else if file_type.is_file() {
            DirectoryEntryKind::File
        } else if file_type.is_dir() {
            DirectoryEntryKind::Directory
        } else {
            DirectoryEntryKind::Other
        };
        entries.push(DirectoryEntry { name, kind });
    }

    entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    Ok(entries)
}

fn write_text_sync(root_dir: &Dir, target: &Path, content: &[u8]) -> Result<(), WorkspaceError> {
    let parent_path = target.parent().unwrap_or_else(|| Path::new(""));
    let final_name = target.file_name().ok_or(WorkspaceError::InvalidPath)?;
    let parent = if parent_path.as_os_str().is_empty() {
        root_dir.try_clone().map_err(|_| WorkspaceError::Io)?
    } else {
        root_dir.open_dir(parent_path).map_err(map_parent_error)?
    };
    let final_path = Path::new(final_name);

    match parent.symlink_metadata(final_path) {
        Ok(metadata) if metadata.is_symlink() => return Err(WorkspaceError::IsSymlink),
        Ok(metadata) if metadata.is_dir() => return Err(WorkspaceError::IsDirectory),
        Ok(metadata) if !metadata.is_file() => return Err(WorkspaceError::NotRegularFile),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_operation_error(error)),
    }

    let (mut temporary, temporary_name) = create_temporary_file(&parent)?;
    let write_result = temporary
        .write_all(content)
        .and_then(|()| temporary.flush());
    drop(temporary);
    if write_result.is_err() {
        return Err(cleanup_temporary(&parent, &temporary_name));
    }

    match parent.rename(&temporary_name, &parent, final_path) {
        Ok(()) => Ok(()),
        Err(_) => Err(cleanup_temporary(&parent, &temporary_name)),
    }
}

fn cleanup_temporary(parent: &Dir, temporary_name: &Path) -> WorkspaceError {
    match parent.remove_file(temporary_name) {
        Ok(()) => WorkspaceError::Io,
        Err(_) => WorkspaceError::CleanupFailed,
    }
}

fn create_temporary_file(parent: &Dir) -> Result<(cap_std::fs::File, PathBuf), WorkspaceError> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| WorkspaceError::Io)?;
        let mut name = String::from(".minicore-workspace-tmp-");
        for byte in random {
            write!(&mut name, "{byte:02x}").map_err(|_| WorkspaceError::Io)?;
        }
        let path = PathBuf::from(&name);
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            ._cap_fs_ext_follow(FollowSymlinks::No);
        match parent.open_with(&path, &options) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(WorkspaceError::Io),
        }
    }
    Err(WorkspaceError::Io)
}

fn open_root_capability(root: &Path) -> Result<Dir, WorkspaceError> {
    let Some(final_name) = root.file_name() else {
        return Dir::open_ambient_dir(root, cap_std::ambient_authority()).map_err(map_root_error);
    };

    let parent_path = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent =
        Dir::open_ambient_dir(parent_path, cap_std::ambient_authority()).map_err(map_root_error)?;
    let parent_file = parent
        .try_clone()
        .map_err(|_| WorkspaceError::RootUnavailable)?
        .into_std_file();
    let root_file = cap_primitives::fs::open_dir_nofollow(&parent_file, Path::new(final_name))
        .map_err(map_root_error)?;
    Ok(Dir::from_std_file(root_file))
}

fn has_lexical_dot_component(path: &Path) -> bool {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    let mut components = text.split(|character| matches!(character, '/' | '\\'));
    #[cfg(not(windows))]
    let mut components = text.split('/');
    components.any(|component| matches!(component, "." | ".."))
}

fn map_root_error(error: std::io::Error) -> WorkspaceError {
    match error.kind() {
        std::io::ErrorKind::NotFound => WorkspaceError::RootUnavailable,
        std::io::ErrorKind::NotADirectory => WorkspaceError::RootNotDirectory,
        _ => WorkspaceError::RootUnavailable,
    }
}

fn map_parent_error(error: std::io::Error) -> WorkspaceError {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            WorkspaceError::MissingParent
        }
        _ => map_operation_error(error),
    }
}

fn map_operation_error(error: std::io::Error) -> WorkspaceError {
    match error.kind() {
        std::io::ErrorKind::NotFound => WorkspaceError::NotFound,
        _ => WorkspaceError::Io,
    }
}

fn map_command_cwd_error(error: std::io::Error) -> WorkspaceError {
    match error.kind() {
        std::io::ErrorKind::NotFound => WorkspaceError::NotFound,
        std::io::ErrorKind::NotADirectory => WorkspaceError::NotRegularFile,
        _ => WorkspaceError::Io,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use futures_util::task::noop_waker;

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn panicking_private_worker_is_redacted_and_owner_drains() {
        let root = std::env::temp_dir().join(format!(
            "minicore-workspace-v2-owner-panic-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();

        let error = workspace
            .run_blocking::<(), _>(|_| -> Result<(), WorkspaceError> {
                panic!("private worker panic payload")
            })
            .await
            .unwrap_err();
        assert_eq!(error, WorkspaceError::WorkerFailed);
        assert_eq!(workspace.run_blocking::<(), _>(|_| Ok(())).await, Ok(()));

        assert_eq!(workspace.shutdown().await, Ok(()));
        {
            let state = workspace.inner.blocking.lock_state();
            assert!(state.sender.is_none());
            assert!(state.handle.is_none());
        }
        assert_eq!(workspace.shutdown().await, Ok(()));
        assert_eq!(
            workspace
                .read_text(&RelativePath::new("after.txt").unwrap(), 1)
                .await
                .unwrap_err(),
            WorkspaceError::Closing
        );

        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_shutdown_keeps_worker_handle_recoverable() {
        let root = std::env::temp_dir().join(format!(
            "minicore-workspace-v2-shutdown-cancel-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = Arc::clone(&release);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let mut operation = Box::pin(workspace.run_blocking::<(), _>(move |_| {
            let _ = started_sender.send(());
            while !worker_release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(())
        }));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        let _admission_probe = operation.as_mut().poll(&mut context);
        started_receiver.await.unwrap();

        let mut shutdown = Box::pin(workspace.shutdown());
        assert!(matches!(
            shutdown.as_mut().poll(&mut context),
            Poll::Pending
        ));
        drop(shutdown);

        release.store(true, Ordering::Release);
        assert_eq!(operation.await, Ok(()));
        assert_eq!(workspace.shutdown().await, Ok(()));
        {
            let state = workspace.inner.blocking.lock_state();
            assert!(state.sender.is_none());
            assert!(state.handle.is_none());
        }
        assert_eq!(
            workspace
                .read_text(&RelativePath::new("after.txt").unwrap(), 1)
                .await
                .unwrap_err(),
            WorkspaceError::Closing
        );

        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dedicated_worker_keeps_current_thread_runtime_responsive() {
        let root = std::env::temp_dir().join(format!(
            "minicore-workspace-v2-worker-responsive-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = Arc::clone(&release);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let mut operation = Box::pin(workspace.run_blocking::<(), _>(move |_| {
            let _ = started_sender.send(());
            while !worker_release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(())
        }));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        let _admission_probe = operation.as_mut().poll(&mut context);
        let responsive = Arc::new(AtomicBool::new(false));
        let branch_responsive = Arc::clone(&responsive);

        tokio::join!(
            async {
                started_receiver.await.unwrap();
            },
            async {
                tokio::task::yield_now().await;
                branch_responsive.store(true, Ordering::Release);
            }
        );
        assert!(responsive.load(Ordering::Acquire));

        release.store(true, Ordering::Release);
        assert_eq!(operation.await, Ok(()));
        assert_eq!(workspace.shutdown().await, Ok(()));
        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dedicated_worker_serializes_same_workspace_jobs_fifo() {
        let root = std::env::temp_dir().join(format!(
            "minicore-workspace-v2-worker-fifo-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(AtomicBool::new(false));
        let first_order = Arc::clone(&order);
        let first_release = Arc::clone(&release);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let mut first = Box::pin(workspace.run_blocking::<(), _>(move |_| {
            first_order.lock().unwrap().push(1u8);
            let _ = started_sender.send(());
            while !first_release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(())
        }));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        let _first_probe = first.as_mut().poll(&mut context);
        started_receiver.await.unwrap();

        let second_order = Arc::clone(&order);
        let mut second = Box::pin(workspace.run_blocking::<(), _>(move |_| {
            second_order.lock().unwrap().push(2u8);
            Ok(())
        }));
        let _second_probe = second.as_mut().poll(&mut context);
        release.store(true, Ordering::Release);

        assert_eq!(first.await, Ok(()));
        assert_eq!(second.await, Ok(()));
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
        assert_eq!(workspace.shutdown().await, Ok(()));
        drop(workspace);
        let _ = fs::remove_dir_all(root);
    }
}
