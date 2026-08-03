use std::fmt;
use std::fs::{self, DirEntry, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use fs4::fs_std::FileExt;

use crate::agent_session_lifecycle::{AgentMetadata, AgentStatus};
use crate::runtime_task::RuntimeTaskContext;
use crate::wire::{AgentId, AgentRevision, Timestamp};

const LOCK_FILE: &str = ".minicore.lock";
const FORMAT_MARKER: &str = "MINICORE_STORE_V1";
const RESERVATIONS_DIRECTORY: &str = "reservations";
const AGENTS_DIRECTORY: &str = "agents";
const SESSIONS_DIRECTORY: &str = "sessions";
const ROOT_ENTRY_CAP: usize = 5;
const RESERVATIONS_ENTRY_CAP: usize = 2;

/// The private physical generation ordinal used only by Store V1 documents and paths.
#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct StorageGeneration(u32);

#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
impl StorageGeneration {
    pub(crate) const fn new(value: u32) -> Option<Self> {
        if value == 0 || value > 1_000_000 {
            None
        } else {
            Some(Self(value))
        }
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }

    pub(crate) fn directory_name(self) -> String {
        format!("{:020}", self.0)
    }
}

impl fmt::Debug for StorageGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StorageGeneration")
            .field(&self.0)
            .finish()
    }
}

/// The closed, redacted construction failure for one physical Agent head document.
#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableAgentHeadError {
    InvalidInvariant,
}

impl fmt::Display for DurableAgentHeadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid durable agent head")
    }
}

impl std::error::Error for DurableAgentHeadError {}

/// The physical Store V1 Agent head representation. Adjacent-generation semantics remain with
/// DurableState recovery; this value validates only facts available in one document.
#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct DurableAgentHead {
    agent_id: AgentId,
    storage_generation: StorageGeneration,
    previous_storage_generation: Option<StorageGeneration>,
    current_definition_revision: AgentRevision,
    current_definition_storage_generation: StorageGeneration,
    metadata: AgentMetadata,
    status: AgentStatus,
    created_at: Timestamp,
}

#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
impl DurableAgentHead {
    #[allow(
        clippy::too_many_arguments,
        reason = "one Store V1 head has eight fixed facts"
    )]
    pub(crate) fn new(
        agent_id: AgentId,
        storage_generation: StorageGeneration,
        previous_storage_generation: Option<StorageGeneration>,
        current_definition_revision: AgentRevision,
        current_definition_storage_generation: StorageGeneration,
        metadata: AgentMetadata,
        status: AgentStatus,
        created_at: Timestamp,
    ) -> Result<Self, DurableAgentHeadError> {
        let expected_previous = storage_generation
            .get()
            .checked_sub(1)
            .and_then(StorageGeneration::new);
        if previous_storage_generation != expected_previous
            || current_definition_storage_generation > storage_generation
        {
            return Err(DurableAgentHeadError::InvalidInvariant);
        }
        Ok(Self {
            agent_id,
            storage_generation,
            previous_storage_generation,
            current_definition_revision,
            current_definition_storage_generation,
            metadata,
            status,
            created_at,
        })
    }

    pub(crate) const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub(crate) const fn storage_generation(&self) -> StorageGeneration {
        self.storage_generation
    }

    pub(crate) const fn previous_storage_generation(&self) -> Option<StorageGeneration> {
        self.previous_storage_generation
    }

    pub(crate) const fn current_definition_revision(&self) -> AgentRevision {
        self.current_definition_revision
    }

    pub(crate) const fn current_definition_storage_generation(&self) -> StorageGeneration {
        self.current_definition_storage_generation
    }

    pub(crate) const fn metadata(&self) -> &AgentMetadata {
        &self.metadata
    }

    pub(crate) const fn status(&self) -> AgentStatus {
        self.status
    }

    pub(crate) const fn created_at(&self) -> Timestamp {
        self.created_at
    }
}

impl fmt::Debug for DurableAgentHead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableAgentHead")
            .field("storage_generation", &self.storage_generation)
            .field(
                "current_definition_storage_generation",
                &self.current_definition_storage_generation,
            )
            .field("metadata", &"redacted")
            .field("status", &self.status)
            .finish()
    }
}

/// The closed, redacted failure taxonomy for the empty Store V1 opener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableOpenError {
    StoreInUse,
    UnsupportedStoreFormat,
    DurableStateCorrupt,
    DurableStateTooLarge,
    StorageUnavailable,
}

/// The private owner of the root lease for the empty Store V1 bootstrap slice.
pub(crate) struct DurableState {
    task_context: RuntimeTaskContext,
    lease: Arc<RootLease>,
}

impl DurableState {
    /// Opens an empty Store V1 root. Every filesystem operation is contained in one tracked
    /// blocking job so it never runs on a current-thread Tokio worker.
    pub(crate) async fn open(
        root: PathBuf,
        task_context: RuntimeTaskContext,
    ) -> Result<Self, DurableOpenError> {
        let job = task_context.spawn_blocking_tracked(move || open_root(root));
        match job.wait().await {
            Ok(Ok(lease)) => Ok(Self {
                task_context,
                lease,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(DurableOpenError::StorageUnavailable),
        }
    }

    /// Releases the root lease only after the shared task owner has joined accepted jobs.
    pub(crate) async fn close(&self) {
        self.task_context.shutdown().await;
        self.lease.release();
    }
}

struct RootLease {
    file: Mutex<Option<File>>,
}

impl RootLease {
    fn new(file: File) -> Self {
        Self {
            file: Mutex::new(Some(file)),
        }
    }

    fn release(&self) {
        let file = lock(&self.file).take();
        if let Some(file) = file {
            let _ = FileExt::unlock(&file);
            drop(file);
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DirectorySync {
    Supported,
    Unsupported,
}

fn open_root(root: PathBuf) -> Result<Arc<RootLease>, DurableOpenError> {
    prepare_root(&root)?;
    let marker_was_present = format_marker_exists(&root)?;
    let lock_file = open_lock_file(&root, marker_was_present)?;
    match lock_file.try_lock_exclusive() {
        Ok(true) => {}
        Ok(false) => return Err(DurableOpenError::StoreInUse),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(DurableOpenError::StoreInUse);
        }
        Err(_) => return Err(DurableOpenError::StorageUnavailable),
    }

    let directory_sync = classify_directory_sync(&root)?;
    let root_entries = read_entries_bounded(&root, ROOT_ENTRY_CAP)?;
    let marker_present = contains_named_entry(&root_entries, FORMAT_MARKER);

    if marker_present {
        validate_marked_root(&root, &root_entries)?;
    } else {
        validate_markerless_root(&root_entries)?;
        complete_markerless_scaffold(&root, directory_sync)?;
        create_format_marker(&root, directory_sync)?;
    }

    Ok(Arc::new(RootLease::new(lock_file)))
}

fn prepare_root(root: &Path) -> Result<(), DurableOpenError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => validate_root_directory(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(root)?;
            let metadata = metadata_without_following(root)?;
            validate_root_directory(&metadata)
        }
        Err(_) => Err(DurableOpenError::StorageUnavailable),
    }
}

fn validate_root_directory(metadata: &fs::Metadata) -> Result<(), DurableOpenError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DurableOpenError::StorageUnavailable);
    }
    validate_existing_directory_mode(metadata, DurableOpenError::StorageUnavailable)?;
    validate_existing_identity(metadata)
}

#[cfg(unix)]
fn validate_existing_directory_mode(
    metadata: &fs::Metadata,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(error);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_existing_directory_mode(
    _metadata: &fs::Metadata,
    _error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    Ok(())
}

#[cfg(unix)]
fn validate_existing_regular_file_mode(
    metadata: &fs::Metadata,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err(error);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_existing_regular_file_mode(
    _metadata: &fs::Metadata,
    _error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    Ok(())
}

fn format_marker_exists(root: &Path) -> Result<bool, DurableOpenError> {
    match fs::symlink_metadata(root.join(FORMAT_MARKER)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(DurableOpenError::StorageUnavailable),
    }
}

fn open_lock_file(root: &Path, marker_was_present: bool) -> Result<File, DurableOpenError> {
    let path = root.join(LOCK_FILE);
    let validation_error = if marker_was_present {
        DurableOpenError::DurableStateCorrupt
    } else {
        DurableOpenError::UnsupportedStoreFormat
    };
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            validate_existing_regular_file(&metadata, validation_error)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|_| DurableOpenError::StorageUnavailable)?;
            validate_open_regular_file(&path, &file, validation_error)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if marker_was_present {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            let file = create_new_private_file(&path)?;
            validate_new_file_mode(&file)?;
            validate_open_regular_file(&path, &file, validation_error)?;
            Ok(file)
        }
        Err(_) => Err(DurableOpenError::StorageUnavailable),
    }
}

fn validate_open_regular_file(
    path: &Path,
    file: &File,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    let handle_metadata = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    let path_metadata = metadata_without_following(path)?;
    validate_existing_regular_file(&handle_metadata, error)?;
    validate_existing_regular_file(&path_metadata, error)?;
    Ok(())
}

fn classify_directory_sync(root: &Path) -> Result<DirectorySync, DurableOpenError> {
    #[cfg(unix)]
    {
        let directory = File::open(root).map_err(|_| DurableOpenError::StorageUnavailable)?;
        match directory.sync_all() {
            Ok(()) => Ok(DirectorySync::Supported),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                Ok(DirectorySync::Unsupported)
            }
            Err(_) => Err(DurableOpenError::StorageUnavailable),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(DirectorySync::Unsupported)
    }
}

fn validate_markerless_root(entries: &[DirEntry]) -> Result<(), DurableOpenError> {
    for entry in entries {
        if entry_has_name(entry, LOCK_FILE) {
            validate_regular_entry(entry, DurableOpenError::UnsupportedStoreFormat)?;
        } else if entry_has_name(entry, RESERVATIONS_DIRECTORY)
            || entry_has_name(entry, AGENTS_DIRECTORY)
            || entry_has_name(entry, SESSIONS_DIRECTORY)
        {
            validate_directory_entry(entry, DurableOpenError::UnsupportedStoreFormat)?;
        } else {
            return Err(DurableOpenError::UnsupportedStoreFormat);
        }
    }
    Ok(())
}

fn complete_markerless_scaffold(
    root: &Path,
    directory_sync: DirectorySync,
) -> Result<(), DurableOpenError> {
    let reservations = root.join(RESERVATIONS_DIRECTORY);
    ensure_markerless_directory(&reservations, directory_sync, false)?;
    validate_markerless_reservations(&reservations)?;

    ensure_markerless_directory(&reservations.join(AGENTS_DIRECTORY), directory_sync, true)?;
    ensure_markerless_directory(&reservations.join(SESSIONS_DIRECTORY), directory_sync, true)?;
    ensure_markerless_directory(&root.join(AGENTS_DIRECTORY), directory_sync, true)?;
    ensure_markerless_directory(&root.join(SESSIONS_DIRECTORY), directory_sync, true)?;
    Ok(())
}

fn validate_markerless_reservations(path: &Path) -> Result<(), DurableOpenError> {
    let entries = read_entries_bounded(path, RESERVATIONS_ENTRY_CAP)?;
    for entry in &entries {
        if entry_has_name(entry, AGENTS_DIRECTORY) || entry_has_name(entry, SESSIONS_DIRECTORY) {
            validate_directory_entry(entry, DurableOpenError::UnsupportedStoreFormat)?;
        } else {
            return Err(DurableOpenError::UnsupportedStoreFormat);
        }
    }
    Ok(())
}

fn ensure_markerless_directory(
    path: &Path,
    directory_sync: DirectorySync,
    must_be_empty: bool,
) -> Result<(), DurableOpenError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_existing_directory(&metadata, DurableOpenError::UnsupportedStoreFormat)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(path)?;
            validate_new_directory_mode(&metadata_without_following(path)?)?;
            sync_direct_parent(path, directory_sync)?;
        }
        Err(_) => return Err(DurableOpenError::StorageUnavailable),
    }

    if must_be_empty && !directory_is_empty(path)? {
        return Err(DurableOpenError::UnsupportedStoreFormat);
    }
    Ok(())
}

fn create_format_marker(
    root: &Path,
    directory_sync: DirectorySync,
) -> Result<(), DurableOpenError> {
    let marker = root.join(FORMAT_MARKER);
    let file = create_new_private_file(&marker)?;
    validate_new_file_mode(&file)?;
    file.sync_all()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    validate_zero_regular_file(&marker)?;
    sync_directory(root, directory_sync)?;
    validate_zero_regular_file(&marker)
}

fn validate_marked_root(root: &Path, entries: &[DirEntry]) -> Result<(), DurableOpenError> {
    let mut has_lock = false;
    let mut has_marker = false;
    let mut has_reservations = false;
    let mut has_agents = false;
    let mut has_sessions = false;

    for entry in entries {
        if entry_has_name(entry, LOCK_FILE) {
            validate_regular_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            has_lock = true;
        } else if entry_has_name(entry, FORMAT_MARKER) {
            validate_regular_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            validate_zero_regular_file(&entry.path())?;
            has_marker = true;
        } else if entry_has_name(entry, RESERVATIONS_DIRECTORY) {
            validate_directory_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            has_reservations = true;
        } else if entry_has_name(entry, AGENTS_DIRECTORY) {
            validate_directory_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            has_agents = true;
        } else if entry_has_name(entry, SESSIONS_DIRECTORY) {
            validate_directory_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            has_sessions = true;
        } else {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }

    if !(has_lock && has_marker && has_reservations && has_agents && has_sessions) {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    validate_empty_directory(
        &root.join(AGENTS_DIRECTORY),
        DurableOpenError::DurableStateCorrupt,
    )?;
    validate_empty_directory(
        &root.join(SESSIONS_DIRECTORY),
        DurableOpenError::DurableStateCorrupt,
    )?;
    validate_marked_reservations(&root.join(RESERVATIONS_DIRECTORY))
}

fn validate_marked_reservations(path: &Path) -> Result<(), DurableOpenError> {
    let entries = read_entries_bounded(path, RESERVATIONS_ENTRY_CAP)?;
    let mut has_agents = false;
    let mut has_sessions = false;
    for entry in &entries {
        if entry_has_name(entry, AGENTS_DIRECTORY) {
            validate_directory_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            has_agents = true;
        } else if entry_has_name(entry, SESSIONS_DIRECTORY) {
            validate_directory_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            has_sessions = true;
        } else {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    if !(has_agents && has_sessions) {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    validate_empty_directory(
        &path.join(AGENTS_DIRECTORY),
        DurableOpenError::DurableStateCorrupt,
    )?;
    validate_empty_directory(
        &path.join(SESSIONS_DIRECTORY),
        DurableOpenError::DurableStateCorrupt,
    )
}

fn validate_empty_directory(path: &Path, error: DurableOpenError) -> Result<(), DurableOpenError> {
    let metadata = metadata_without_following(path)?;
    validate_existing_directory(&metadata, error)?;
    if !directory_is_empty(path)? {
        return Err(error);
    }
    Ok(())
}

fn validate_regular_entry(
    entry: &DirEntry,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    let metadata = metadata_without_following(&entry.path())?;
    validate_existing_regular_file(&metadata, error)
}

fn validate_directory_entry(
    entry: &DirEntry,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    let metadata = metadata_without_following(&entry.path())?;
    validate_existing_directory(&metadata, error)
}

fn validate_zero_regular_file(path: &Path) -> Result<(), DurableOpenError> {
    let metadata = metadata_without_following(path)?;
    validate_existing_regular_file(&metadata, DurableOpenError::DurableStateCorrupt)?;
    if metadata.len() != 0 {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    let mut file = File::open(path).map_err(|_| DurableOpenError::StorageUnavailable)?;
    let mut byte = [0_u8; 1];
    if file
        .read(&mut byte)
        .map_err(|_| DurableOpenError::StorageUnavailable)?
        != 0
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), DurableOpenError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    set_new_directory_mode(&mut builder);
    match builder.create(path) {
        Ok(()) => {
            set_private_permissions(path, 0o700)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(DurableOpenError::StorageUnavailable)
        }
        Err(_) => Err(DurableOpenError::StorageUnavailable),
    }
}

#[cfg(unix)]
fn set_new_directory_mode(builder: &mut fs::DirBuilder) {
    use std::os::unix::fs::DirBuilderExt;

    builder.mode(0o700);
}

#[cfg(not(unix))]
fn set_new_directory_mode(_builder: &mut fs::DirBuilder) {}

fn create_new_private_file(path: &Path) -> Result<File, DurableOpenError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    set_new_file_mode(&mut options);
    let file = options
        .open(path)
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    set_private_permissions(path, 0o600)?;
    Ok(file)
}

#[cfg(unix)]
fn set_new_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_new_file_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_private_permissions(path: &Path, mode: u32) -> Result<(), DurableOpenError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|_| DurableOpenError::StorageUnavailable)
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path, _mode: u32) -> Result<(), DurableOpenError> {
    Ok(())
}

#[cfg(unix)]
fn validate_new_directory_mode(metadata: &fs::Metadata) -> Result<(), DurableOpenError> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(DurableOpenError::StorageUnavailable);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_new_directory_mode(_metadata: &fs::Metadata) -> Result<(), DurableOpenError> {
    Ok(())
}

#[cfg(unix)]
fn validate_new_file_mode(file: &File) -> Result<(), DurableOpenError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?
        .permissions()
        .mode();
    if mode & 0o7777 != 0o600 {
        return Err(DurableOpenError::StorageUnavailable);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_new_file_mode(_file: &File) -> Result<(), DurableOpenError> {
    Ok(())
}

fn validate_existing_regular_file(
    metadata: &fs::Metadata,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(error);
    }
    validate_existing_regular_file_mode(metadata, error)?;
    validate_existing_identity(metadata)
}

fn validate_existing_directory(
    metadata: &fs::Metadata,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(error);
    }
    validate_existing_directory_mode(metadata, error)?;
    validate_existing_identity(metadata)
}

// Deliberately no-op for this slice: this seam is reserved only for future Unix ownership,
// effective-user, and device checks, plus Windows ACL and volume checks. Type and exact Unix
// mode validation are enforced by the callers today and must not be deferred through this hook.
fn validate_existing_identity(_metadata: &fs::Metadata) -> Result<(), DurableOpenError> {
    Ok(())
}

fn sync_direct_parent(path: &Path, directory_sync: DirectorySync) -> Result<(), DurableOpenError> {
    let parent = path.parent().ok_or(DurableOpenError::StorageUnavailable)?;
    sync_directory(parent, directory_sync)
}

fn sync_directory(path: &Path, directory_sync: DirectorySync) -> Result<(), DurableOpenError> {
    if directory_sync == DirectorySync::Unsupported {
        return Ok(());
    }
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| DurableOpenError::StorageUnavailable)
}

fn metadata_without_following(path: &Path) -> Result<fs::Metadata, DurableOpenError> {
    fs::symlink_metadata(path).map_err(|_| DurableOpenError::StorageUnavailable)
}

fn directory_is_empty(path: &Path) -> Result<bool, DurableOpenError> {
    let mut entries = fs::read_dir(path).map_err(|_| DurableOpenError::StorageUnavailable)?;
    match entries.next() {
        Some(Ok(_)) => Ok(false),
        Some(Err(_)) => Err(DurableOpenError::StorageUnavailable),
        None => Ok(true),
    }
}

fn read_entries_bounded(path: &Path, maximum: usize) -> Result<Vec<DirEntry>, DurableOpenError> {
    let entries = fs::read_dir(path).map_err(|_| DurableOpenError::StorageUnavailable)?;
    let mut bounded = Vec::with_capacity(maximum);
    for entry in entries {
        let entry = entry.map_err(|_| DurableOpenError::StorageUnavailable)?;
        if bounded.len() == maximum {
            return Err(DurableOpenError::DurableStateTooLarge);
        }
        bounded.push(entry);
    }
    Ok(bounded)
}

fn contains_named_entry(entries: &[DirEntry], name: &str) -> bool {
    entries.iter().any(|entry| entry_has_name(entry, name))
}

fn entry_has_name(entry: &DirEntry, name: &str) -> bool {
    entry.file_name() == std::ffi::OsStr::new(name)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::runtime::Handle;

    use super::{
        AGENTS_DIRECTORY, DurableOpenError, DurableState, FORMAT_MARKER, LOCK_FILE,
        RESERVATIONS_DIRECTORY, RESERVATIONS_ENTRY_CAP, ROOT_ENTRY_CAP, SESSIONS_DIRECTORY,
    };
    use crate::runtime_task::RuntimeTaskContext;

    static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn existing() -> Self {
            loop {
                let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
                assert_ne!(suffix, 0, "test root suffix must be nonzero");
                let path = std::env::temp_dir().join(format!(
                    "minicore-durable-state-{}-{suffix}",
                    std::process::id()
                ));
                if !path.exists() {
                    fs::create_dir(&path).expect("the test root is created");
                    set_private_directory_mode(&path);
                    return Self { path };
                }
            }
        }

        fn nonexistent() -> Self {
            let root = Self::existing();
            fs::remove_dir(&root.path).expect("the test root becomes nonexistent");
            root
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            if self.path.exists() {
                fs::remove_dir_all(&self.path).expect("the test root is removed deterministically");
            }
        }
    }

    #[cfg(unix)]
    fn set_private_directory_mode(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("the test directory receives its required private mode");
    }

    #[cfg(not(unix))]
    fn set_private_directory_mode(_path: &Path) {}

    #[cfg(unix)]
    fn set_private_file_mode(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("the test file receives its required private mode");
    }

    #[cfg(not(unix))]
    fn set_private_file_mode(_path: &Path) {}

    #[cfg(unix)]
    fn set_unix_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .expect("the test entry receives its requested mode");
    }

    fn create_directory(path: &Path) {
        fs::create_dir(path).expect("the scaffold directory is created");
        set_private_directory_mode(path);
    }

    fn create_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("the scaffold file is created");
        set_private_file_mode(path);
    }

    async fn open(root: &Path) -> Result<DurableState, DurableOpenError> {
        let context = RuntimeTaskContext::new(Handle::current())
            .await
            .expect("the Tokio test runtime has a time driver");
        let result = DurableState::open(root.to_owned(), context.clone()).await;
        if result.is_err() {
            context.shutdown().await;
        }
        result
    }

    fn root_entry_names(root: &Path) -> Vec<String> {
        let mut entries = fs::read_dir(root)
            .expect("the root can be inspected")
            .map(|entry| {
                entry
                    .expect("the root entry can be inspected")
                    .file_name()
                    .into_string()
                    .expect("Store V1 test names are UTF-8")
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    fn assert_empty_store_scaffold(root: &Path) {
        assert_eq!(
            root_entry_names(root),
            [
                LOCK_FILE.to_owned(),
                FORMAT_MARKER.to_owned(),
                AGENTS_DIRECTORY.to_owned(),
                RESERVATIONS_DIRECTORY.to_owned(),
                SESSIONS_DIRECTORY.to_owned(),
            ]
        );
        assert_eq!(
            fs::read(root.join(FORMAT_MARKER)).expect("the format marker is readable"),
            b""
        );
        let reservations = root.join(RESERVATIONS_DIRECTORY);
        assert!(reservations.is_dir(), "reservations is a fixed directory");
        assert_eq!(
            root_entry_names(&reservations),
            [AGENTS_DIRECTORY.to_owned(), SESSIONS_DIRECTORY.to_owned()]
        );
        for directory in [
            reservations.join(AGENTS_DIRECTORY),
            reservations.join(SESSIONS_DIRECTORY),
            root.join(AGENTS_DIRECTORY),
            root.join(SESSIONS_DIRECTORY),
        ] {
            assert!(
                directory.is_dir(),
                "{} is a fixed directory",
                directory.display()
            );
            assert!(
                fs::read_dir(&directory)
                    .expect("the fixed directory is readable")
                    .next()
                    .is_none(),
                "{} is empty",
                directory.display()
            );
        }
    }

    #[cfg(unix)]
    fn assert_exact_bootstrap_modes(root: &Path) {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(root)
                .expect("the root metadata is readable")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        for directory in [
            root.join(RESERVATIONS_DIRECTORY),
            root.join(RESERVATIONS_DIRECTORY).join(AGENTS_DIRECTORY),
            root.join(RESERVATIONS_DIRECTORY).join(SESSIONS_DIRECTORY),
            root.join(AGENTS_DIRECTORY),
            root.join(SESSIONS_DIRECTORY),
        ] {
            assert_eq!(
                fs::metadata(directory)
                    .expect("the directory metadata is readable")
                    .permissions()
                    .mode()
                    & 0o7777,
                0o700
            );
        }
        for file in [root.join(LOCK_FILE), root.join(FORMAT_MARKER)] {
            assert_eq!(
                fs::metadata(file)
                    .expect("the marker metadata is readable")
                    .permissions()
                    .mode()
                    & 0o7777,
                0o600
            );
        }
    }

    #[cfg(not(unix))]
    fn assert_exact_bootstrap_modes(_root: &Path) {}

    fn create_marked_empty_store(root: &Path) {
        create_file(&root.join(LOCK_FILE), b"");
        create_file(&root.join(FORMAT_MARKER), b"");
        create_directory(&root.join(RESERVATIONS_DIRECTORY));
        create_directory(&root.join(RESERVATIONS_DIRECTORY).join(AGENTS_DIRECTORY));
        create_directory(&root.join(RESERVATIONS_DIRECTORY).join(SESSIONS_DIRECTORY));
        create_directory(&root.join(AGENTS_DIRECTORY));
        create_directory(&root.join(SESSIONS_DIRECTORY));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_existing_root_and_exact_markerless_scaffold_open() {
        let empty = TempRoot::existing();
        let state = open(empty.path())
            .await
            .expect("an empty existing root bootstraps");
        assert_empty_store_scaffold(empty.path());
        assert_exact_bootstrap_modes(empty.path());
        state.close().await;

        let scaffold = TempRoot::existing();
        create_file(&scaffold.path().join(LOCK_FILE), b"");
        create_directory(&scaffold.path().join(RESERVATIONS_DIRECTORY));
        create_directory(
            &scaffold
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY),
        );
        create_directory(&scaffold.path().join(AGENTS_DIRECTORY));

        let state = open(scaffold.path())
            .await
            .expect("an exact markerless scaffold resumes bootstrap");
        assert_empty_store_scaffold(scaffold.path());
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn format_marker_and_scaffold_have_exact_final_contents_and_modes() {
        let root = TempRoot::nonexistent();

        let state = open(root.path())
            .await
            .expect("a nonexistent root bootstraps");

        assert_empty_store_scaffold(root.path());
        assert_exact_bootstrap_modes(root.path());
        state.close().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn existing_root_with_wrong_unix_mode_is_storage_unavailable() {
        let root = TempRoot::existing();
        set_unix_mode(root.path(), 0o755);

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::StorageUnavailable)
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn markerless_recognized_entries_with_wrong_unix_modes_are_unsupported() {
        let lock_root = TempRoot::existing();
        let lock = lock_root.path().join(LOCK_FILE);
        create_file(&lock, b"");
        set_unix_mode(&lock, 0o644);
        assert!(matches!(
            open(lock_root.path()).await,
            Err(DurableOpenError::UnsupportedStoreFormat)
        ));

        let root_directory_root = TempRoot::existing();
        let reservations = root_directory_root.path().join(RESERVATIONS_DIRECTORY);
        create_directory(&reservations);
        set_unix_mode(&reservations, 0o755);
        assert!(matches!(
            open(root_directory_root.path()).await,
            Err(DurableOpenError::UnsupportedStoreFormat)
        ));

        let nested_directory_root = TempRoot::existing();
        let reservations = nested_directory_root.path().join(RESERVATIONS_DIRECTORY);
        create_directory(&reservations);
        let agents = reservations.join(AGENTS_DIRECTORY);
        create_directory(&agents);
        set_unix_mode(&agents, 0o755);
        assert!(matches!(
            open(nested_directory_root.path()).await,
            Err(DurableOpenError::UnsupportedStoreFormat)
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn marked_recognized_entries_with_wrong_unix_modes_are_corrupt() {
        for (relative_path, wrong_mode) in [
            (PathBuf::from(LOCK_FILE), 0o644),
            (PathBuf::from(FORMAT_MARKER), 0o644),
            (PathBuf::from(RESERVATIONS_DIRECTORY), 0o755),
            (PathBuf::from(AGENTS_DIRECTORY), 0o755),
            (PathBuf::from(SESSIONS_DIRECTORY), 0o755),
            (
                PathBuf::from(RESERVATIONS_DIRECTORY).join(AGENTS_DIRECTORY),
                0o755,
            ),
            (
                PathBuf::from(RESERVATIONS_DIRECTORY).join(SESSIONS_DIRECTORY),
                0o755,
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            set_unix_mode(&root.path().join(&relative_path), wrong_mode);

            assert!(matches!(
                open(root.path()).await,
                Err(DurableOpenError::DurableStateCorrupt)
            ));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn markerless_unknown_nonempty_root_is_unsupported_without_deleting_content() {
        let root = TempRoot::existing();
        let unknown = root.path().join("foreign-store-data");
        create_file(&unknown, b"must survive format rejection");

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::UnsupportedStoreFormat)
        ));
        assert_eq!(
            fs::read(&unknown).expect("unknown content survives"),
            b"must survive format rejection"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn marker_present_root_missing_a_fixed_directory_is_corrupt() {
        let root = TempRoot::existing();
        create_file(&root.path().join(LOCK_FILE), b"");
        create_file(&root.path().join(FORMAT_MARKER), b"");
        create_directory(&root.path().join(RESERVATIONS_DIRECTORY));
        create_directory(
            &root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY),
        );
        create_directory(
            &root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY),
        );
        create_directory(&root.path().join(AGENTS_DIRECTORY));

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::DurableStateCorrupt)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn marker_present_root_never_bootstraps_a_missing_permanent_lock() {
        let root = TempRoot::existing();
        create_file(&root.path().join(FORMAT_MARKER), b"");
        create_directory(&root.path().join(RESERVATIONS_DIRECTORY));
        create_directory(
            &root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY),
        );
        create_directory(
            &root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY),
        );
        create_directory(&root.path().join(AGENTS_DIRECTORY));
        create_directory(&root.path().join(SESSIONS_DIRECTORY));

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            !root.path().join(LOCK_FILE).exists(),
            "a marked root is never repaired by inventing a lock file"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn marker_present_entity_or_reservation_entry_is_corrupt() {
        let entity_root = TempRoot::existing();
        create_marked_empty_store(entity_root.path());
        create_directory(&entity_root.path().join(AGENTS_DIRECTORY).join("agt_extra"));
        assert!(matches!(
            open(entity_root.path()).await,
            Err(DurableOpenError::DurableStateCorrupt)
        ));

        let reservation_root = TempRoot::existing();
        create_marked_empty_store(reservation_root.path());
        create_file(
            &reservation_root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join("agt_reserved"),
            b"",
        );
        assert!(matches!(
            open(reservation_root.path()).await,
            Err(DurableOpenError::DurableStateCorrupt)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn permanent_lock_file_survives_shutdown_and_reopen_without_replacement() {
        let root = TempRoot::nonexistent();
        let state = open(root.path()).await.expect("the store opens");
        state.close().await;

        let lock_path = root.path().join(LOCK_FILE);
        create_file(&lock_path, b"permanent lease identity");
        let before = fs::metadata(&lock_path).expect("the lock metadata is readable");

        let state = open(root.path()).await.expect("the store reopens");
        state.close().await;

        assert_eq!(
            fs::read(&lock_path).expect("the permanent lock remains"),
            b"permanent lease identity"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let after = fs::metadata(&lock_path).expect("the lock metadata remains readable");
            assert_eq!(before.dev(), after.dev());
            assert_eq!(before.ino(), after.ino());
        }
        #[cfg(not(unix))]
        let _ = before;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn root_direct_cap_plus_one_precedes_markerless_format_classification() {
        let root = TempRoot::existing();
        // The opener creates `.minicore.lock` before strict classification, so five foreign
        // entries become exactly the six-entry root cap+1 observation.
        for index in 0..ROOT_ENTRY_CAP {
            create_file(&root.path().join(format!("foreign-{index}")), b"");
        }

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(
            root.path().join("foreign-0").exists(),
            "the unsupported content remains untouched"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reservations_cap_plus_one_precedes_markerless_format_classification() {
        let root = TempRoot::existing();
        let reservations = root.path().join(RESERVATIONS_DIRECTORY);
        create_directory(&reservations);
        for index in 0..=RESERVATIONS_ENTRY_CAP {
            create_file(&reservations.join(format!("foreign-{index}")), b"");
        }

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(
            reservations.join("foreign-0").exists(),
            "the oversized unsupported scaffold remains untouched"
        );
    }
}
