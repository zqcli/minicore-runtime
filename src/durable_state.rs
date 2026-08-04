use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, DirEntry, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use fs4::fs_std::FileExt;

use crate::agent_session_lifecycle::{
    AgentDefinition, AgentMetadata, AgentStatus, SessionForkProvenance, SessionLifecycle,
    SessionMetadata, agent_definitions_have_same_canonical_execution_content,
    agent_metadata_has_same_canonical_content, is_legal_agent_status_transition,
};
use crate::runtime_task::RuntimeTaskContext;
use crate::wire::durable_store::{
    DurableStoreCodecError, DurableStoreV1Codec, MAX_DURABLE_DOCUMENT_BYTES,
};
use crate::wire::{
    AgentId, AgentMetadataRevision, AgentRevision, SessionDefinitionRevision, SessionId, Timestamp,
};

const LOCK_FILE: &str = ".minicore.lock";
const FORMAT_MARKER: &str = "MINICORE_STORE_V1";
const RESERVATIONS_DIRECTORY: &str = "reservations";
const AGENTS_DIRECTORY: &str = "agents";
const SESSIONS_DIRECTORY: &str = "sessions";
const ROOT_ENTRY_CAP: usize = 5;
const RESERVATIONS_ENTRY_CAP: usize = 2;
const AGENT_RESERVATION_ENTRY_CAP: usize = 1_000_000;
const SESSION_RESERVATION_ENTRY_CAP: usize = 1_000_000;
const AGENT_ENTITY_ENTRY_CAP: usize = 2;
const ROOT_AGENT_ENTRY_CAP: usize = 1_000_000;
const ROOT_SESSION_ENTRY_CAP: usize = 1_000_000;
const GENERATION_ENTRY_CAP: usize = 1_000_000;
const GENERATION_PAYLOAD_ENTRY_CAP: usize = 3;

/// The private physical generation ordinal used only by Store V1 documents and paths.
#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct StorageGeneration(u32);

/// The closed, redacted failure for a physical Store V1 generation directory name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageGenerationDirectoryNameError {
    InvalidDirectoryName,
}

impl fmt::Display for StorageGenerationDirectoryNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid storage generation directory name")
    }
}

impl std::error::Error for StorageGenerationDirectoryNameError {}

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

    pub(crate) fn parse_directory_name(
        value: &OsStr,
    ) -> Result<Self, StorageGenerationDirectoryNameError> {
        value
            .to_str()
            .ok_or(StorageGenerationDirectoryNameError::InvalidDirectoryName)
            .and_then(Self::parse_directory_name_str)
    }

    pub(crate) fn parse_directory_name_str(
        value: &str,
    ) -> Result<Self, StorageGenerationDirectoryNameError> {
        let bytes = value.as_bytes();
        if bytes.len() != 20 || !bytes.iter().all(u8::is_ascii_digit) {
            return Err(StorageGenerationDirectoryNameError::InvalidDirectoryName);
        }
        let generation = value
            .parse::<u32>()
            .ok()
            .and_then(Self::new)
            .ok_or(StorageGenerationDirectoryNameError::InvalidDirectoryName)?;
        if generation.directory_name() != value {
            return Err(StorageGenerationDirectoryNameError::InvalidDirectoryName);
        }
        Ok(generation)
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

/// The closed, redacted construction failure for one physical Session head document.
#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableSessionHeadError {
    InvalidInvariant,
}

impl fmt::Display for DurableSessionHeadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid durable session head")
    }
}

impl std::error::Error for DurableSessionHeadError {}

/// The physical Store V1 Session head representation. Recovery alone validates adjacent
/// generation semantics; this value checks only invariants observable in one document.
#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct DurableSessionHead {
    session_id: SessionId,
    storage_generation: StorageGeneration,
    previous_storage_generation: Option<StorageGeneration>,
    current_definition_revision: SessionDefinitionRevision,
    current_definition_storage_generation: StorageGeneration,
    metadata: SessionMetadata,
    lifecycle: SessionLifecycle,
    fork_provenance: Option<SessionForkProvenance>,
    created_at: Timestamp,
}

#[allow(
    dead_code,
    reason = "M5 Store V1 codec precedes DurableState entity publication and recovery"
)]
impl DurableSessionHead {
    #[allow(
        clippy::too_many_arguments,
        reason = "one Store V1 Session head has nine fixed facts"
    )]
    pub(crate) fn new(
        session_id: SessionId,
        storage_generation: StorageGeneration,
        previous_storage_generation: Option<StorageGeneration>,
        current_definition_revision: SessionDefinitionRevision,
        current_definition_storage_generation: StorageGeneration,
        metadata: SessionMetadata,
        lifecycle: SessionLifecycle,
        fork_provenance: Option<SessionForkProvenance>,
        created_at: Timestamp,
    ) -> Result<Self, DurableSessionHeadError> {
        let expected_previous = storage_generation
            .get()
            .checked_sub(1)
            .and_then(StorageGeneration::new);
        if previous_storage_generation != expected_previous
            || current_definition_storage_generation > storage_generation
            || fork_provenance
                .as_ref()
                .is_some_and(|provenance| provenance.source_session_id() == session_id)
        {
            return Err(DurableSessionHeadError::InvalidInvariant);
        }
        Ok(Self {
            session_id,
            storage_generation,
            previous_storage_generation,
            current_definition_revision,
            current_definition_storage_generation,
            metadata,
            lifecycle,
            fork_provenance,
            created_at,
        })
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn storage_generation(&self) -> StorageGeneration {
        self.storage_generation
    }

    pub(crate) const fn previous_storage_generation(&self) -> Option<StorageGeneration> {
        self.previous_storage_generation
    }

    pub(crate) const fn current_definition_revision(&self) -> SessionDefinitionRevision {
        self.current_definition_revision
    }

    pub(crate) const fn current_definition_storage_generation(&self) -> StorageGeneration {
        self.current_definition_storage_generation
    }

    pub(crate) const fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    pub(crate) const fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle
    }

    pub(crate) const fn fork_provenance(&self) -> Option<&SessionForkProvenance> {
        self.fork_provenance.as_ref()
    }

    pub(crate) const fn created_at(&self) -> Timestamp {
        self.created_at
    }
}

impl fmt::Debug for DurableSessionHead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableSessionHead")
            .field("storage_generation", &self.storage_generation)
            .field(
                "current_definition_storage_generation",
                &self.current_definition_storage_generation,
            )
            .field("metadata", &"redacted")
            .field("lifecycle", &self.lifecycle)
            .field("fork_provenance", &"redacted")
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

/// One recovered Agent's immutable current facts. Historical heads are folded away after
/// validation; retained definitions remain privately addressable through a compact index.
struct DurableAgentCatalogEntry {
    current_head: Arc<DurableAgentHead>,
    current_definition: Arc<AgentDefinition>,
    definition_index: BTreeMap<AgentRevision, StorageGeneration>,
}

impl fmt::Debug for DurableAgentCatalogEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableAgentCatalogEntry")
            .field("current_head", &"redacted")
            .field("current_definition", &"redacted")
            .field("definition_index", &"redacted")
            .finish()
    }
}

/// The private owner of the Store V1 root lease and recovered immutable catalog.
pub(crate) struct DurableState {
    task_context: RuntimeTaskContext,
    lease: Arc<RootLease>,
    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later runtime read paths"
    )]
    agents: Arc<BTreeMap<AgentId, DurableAgentCatalogEntry>>,
}

impl DurableState {
    /// Opens a Store V1 root and recovers the currently supported committed Agent chains.
    /// Every filesystem operation runs in one tracked blocking job, never on a Tokio worker.
    pub(crate) async fn open(
        root: PathBuf,
        task_context: RuntimeTaskContext,
    ) -> Result<Self, DurableOpenError> {
        let job = task_context.spawn_blocking_tracked(move || open_root(root));
        match job.wait().await {
            Ok(Ok(opened)) => Ok(Self {
                task_context,
                lease: opened.lease,
                agents: opened.agents,
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

    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later runtime read paths"
    )]
    pub(crate) fn agent_head(&self, agent_id: AgentId) -> Option<Arc<DurableAgentHead>> {
        self.agents
            .get(&agent_id)
            .map(|entry| Arc::clone(&entry.current_head))
    }

    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later Agent revision resolution"
    )]
    pub(crate) fn agent_current_definition(
        &self,
        agent_id: AgentId,
    ) -> Option<Arc<AgentDefinition>> {
        self.agents
            .get(&agent_id)
            .map(|entry| Arc::clone(&entry.current_definition))
    }

    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later Agent revision resolution"
    )]
    pub(crate) fn contains_agent_definition(
        &self,
        agent_id: AgentId,
        revision: AgentRevision,
    ) -> bool {
        self.agents
            .get(&agent_id)
            .is_some_and(|entry| entry.definition_index.contains_key(&revision))
    }
}

struct RootLease {
    file: Mutex<Option<File>>,
}

#[derive(Clone)]
struct OpenRoot {
    lease: Arc<RootLease>,
    agents: Arc<BTreeMap<AgentId, DurableAgentCatalogEntry>>,
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

fn open_root(root: PathBuf) -> Result<OpenRoot, DurableOpenError> {
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

    let agents = if marker_present {
        recover_marked_root(&root, &root_entries)?
    } else {
        validate_markerless_root(&root_entries)?;
        complete_markerless_scaffold(&root, directory_sync)?;
        create_format_marker(&root, directory_sync)?;
        BTreeMap::new()
    };

    Ok(OpenRoot {
        lease: Arc::new(RootLease::new(lock_file)),
        agents: Arc::new(agents),
    })
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
    validate_same_file_identity(&handle_metadata, &path_metadata)
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

#[derive(Clone, Copy)]
struct RecoveryCaps {
    agent_reservations: usize,
    session_reservations: usize,
    root_agents: usize,
    root_sessions: usize,
    generations: usize,
}

impl RecoveryCaps {
    const PRODUCTION: Self = Self {
        agent_reservations: AGENT_RESERVATION_ENTRY_CAP,
        session_reservations: SESSION_RESERVATION_ENTRY_CAP,
        root_agents: ROOT_AGENT_ENTRY_CAP,
        root_sessions: ROOT_SESSION_ENTRY_CAP,
        generations: GENERATION_ENTRY_CAP,
    };
}

fn recover_marked_root(
    root: &Path,
    entries: &[DirEntry],
) -> Result<BTreeMap<AgentId, DurableAgentCatalogEntry>, DurableOpenError> {
    recover_marked_root_with_caps(root, entries, RecoveryCaps::PRODUCTION)
}

fn recover_marked_root_with_caps(
    root: &Path,
    entries: &[DirEntry],
    caps: RecoveryCaps,
) -> Result<BTreeMap<AgentId, DurableAgentCatalogEntry>, DurableOpenError> {
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

    let reservations = recover_marked_reservations(&root.join(RESERVATIONS_DIRECTORY), caps)?;
    validate_required_empty_directory(&root.join(SESSIONS_DIRECTORY), caps.root_sessions)?;
    recover_agents(
        &root.join(AGENTS_DIRECTORY),
        &reservations,
        caps.root_agents,
        caps.generations,
    )
}

fn recover_marked_reservations(
    path: &Path,
    caps: RecoveryCaps,
) -> Result<BTreeSet<AgentId>, DurableOpenError> {
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

    let agents = scan_agent_reservations(&path.join(AGENTS_DIRECTORY), caps.agent_reservations)?;
    validate_required_empty_directory(&path.join(SESSIONS_DIRECTORY), caps.session_reservations)?;
    Ok(agents)
}

fn scan_agent_reservations(
    path: &Path,
    maximum: usize,
) -> Result<BTreeSet<AgentId>, DurableOpenError> {
    let metadata = metadata_without_following(path)?;
    validate_existing_directory(&metadata, DurableOpenError::DurableStateCorrupt)?;
    let entries = read_entries_bounded(path, maximum)?;
    let mut reservation_paths = BTreeMap::new();
    for entry in entries {
        let agent_id = parse_agent_id_name(&entry.file_name())?;
        if reservation_paths.insert(agent_id, entry.path()).is_some() {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    for path in reservation_paths.values() {
        validate_zero_regular_file(path)?;
    }
    Ok(reservation_paths.into_keys().collect())
}

fn validate_required_empty_directory(path: &Path, maximum: usize) -> Result<(), DurableOpenError> {
    let metadata = metadata_without_following(path)?;
    validate_existing_directory(&metadata, DurableOpenError::DurableStateCorrupt)?;
    if read_entries_bounded(path, maximum)?.is_empty() {
        Ok(())
    } else {
        Err(DurableOpenError::DurableStateCorrupt)
    }
}

fn recover_agents(
    path: &Path,
    reservations: &BTreeSet<AgentId>,
    maximum: usize,
    generation_maximum: usize,
) -> Result<BTreeMap<AgentId, DurableAgentCatalogEntry>, DurableOpenError> {
    let metadata = metadata_without_following(path)?;
    validate_existing_directory(&metadata, DurableOpenError::DurableStateCorrupt)?;
    let entries = read_entries_bounded(path, maximum)?;
    let mut entity_paths = BTreeMap::new();
    for entry in entries {
        let agent_id = parse_agent_id_name(&entry.file_name())?;
        if entity_paths.insert(agent_id, entry.path()).is_some() {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    let mut agents = BTreeMap::new();
    for (agent_id, entity_path) in entity_paths {
        let metadata = metadata_without_following(&entity_path)?;
        validate_existing_directory(&metadata, DurableOpenError::DurableStateCorrupt)?;
        if !reservations.contains(&agent_id) {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        let entry = recover_agent_entity(&entity_path, agent_id, generation_maximum)?;
        if agents.insert(agent_id, entry).is_some() {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    Ok(agents)
}

fn recover_agent_entity(
    path: &Path,
    agent_id: AgentId,
    generation_maximum: usize,
) -> Result<DurableAgentCatalogEntry, DurableOpenError> {
    let entries = read_entries_bounded(path, AGENT_ENTITY_ENTRY_CAP)?;
    let mut published = false;
    let mut generations = None;
    for entry in entries {
        if entry_has_name(&entry, "PUBLISHED") {
            validate_zero_regular_file(&entry.path())?;
            published = true;
        } else if entry_has_name(&entry, "generations") {
            validate_directory_entry(&entry, DurableOpenError::DurableStateCorrupt)?;
            generations = Some(entry.path());
        } else {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    let Some(generations) = generations else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };
    if !published {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    recover_agent_generation_chain(&generations, agent_id, generation_maximum)
}

/// Recovers a fully committed, contiguous Agent generation chain. Markerless trailing
/// generations are deliberately rejected here: staging cleanup remains a later slice.
fn recover_agent_generation_chain(
    path: &Path,
    agent_id: AgentId,
    maximum: usize,
) -> Result<DurableAgentCatalogEntry, DurableOpenError> {
    let entries = read_entries_bounded(path, maximum)?;
    let mut generation_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let generation = StorageGeneration::parse_directory_name(&entry.file_name())
            .map_err(|_| DurableOpenError::DurableStateCorrupt)?;
        generation_entries.push((generation, entry));
    }
    generation_entries.sort_unstable_by_key(|(generation, _)| *generation);
    for (_, entry) in &generation_entries {
        validate_directory_entry(entry, DurableOpenError::DurableStateCorrupt)?;
    }

    let mut generations = generation_entries.into_iter();
    let Some((first_generation, first_entry)) = generations.next() else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };
    if first_generation.get() != 1 {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    let (first_head, Some(first_definition)) =
        recover_agent_generation_payload(&first_entry.path())?
    else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };
    validate_generation_one_agent_semantics(
        agent_id,
        first_generation,
        &first_head,
        &first_definition,
    )?;

    let mut current_head = first_head;
    let mut current_definition = first_definition;
    let mut definition_index = BTreeMap::new();
    if definition_index
        .insert(current_definition.revision(), first_generation)
        .is_some()
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    for (generation, generation_entry) in generations {
        if generation.get()
            != current_head
                .storage_generation()
                .get()
                .checked_add(1)
                .ok_or(DurableOpenError::DurableStateCorrupt)?
        {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        let (head, definition) = recover_agent_generation_payload(&generation_entry.path())?;
        validate_agent_generation_transition(
            agent_id,
            generation,
            &current_head,
            &current_definition,
            &head,
            definition.as_ref(),
        )?;
        if let Some(definition) = definition {
            if definition_index
                .insert(definition.revision(), generation)
                .is_some()
            {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            current_definition = definition;
        }
        current_head = head;
    }

    Ok(DurableAgentCatalogEntry {
        current_head: Arc::new(current_head),
        current_definition: Arc::new(current_definition),
        definition_index,
    })
}

fn recover_agent_generation_payload(
    path: &Path,
) -> Result<(DurableAgentHead, Option<AgentDefinition>), DurableOpenError> {
    let mut entries = read_entries_bounded(path, GENERATION_PAYLOAD_ENTRY_CAP)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut head_path = None;
    let mut definition_path = None;
    let mut committed = false;
    for entry in entries {
        if entry_has_name(&entry, "head.json") {
            head_path = Some(entry.path());
        } else if entry_has_name(&entry, "definition.json") {
            definition_path = Some(entry.path());
        } else if entry_has_name(&entry, "COMMITTED") {
            validate_zero_regular_file(&entry.path())?;
            committed = true;
        } else {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    let Some(head_path) = head_path else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };
    if !committed {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    let head = decode_agent_head_document(&head_path)?;
    let definition = definition_path
        .as_deref()
        .map(decode_agent_definition_document)
        .transpose()?;
    Ok((head, definition))
}

fn parse_agent_id_name(name: &OsStr) -> Result<AgentId, DurableOpenError> {
    let value = name.to_str().ok_or(DurableOpenError::DurableStateCorrupt)?;
    let agent_id = AgentId::from_str(value).map_err(|_| DurableOpenError::DurableStateCorrupt)?;
    if agent_id.to_string() != value {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(agent_id)
}

fn validate_generation_one_agent_semantics(
    path_agent_id: AgentId,
    path_generation: StorageGeneration,
    head: &DurableAgentHead,
    definition: &AgentDefinition,
) -> Result<(), DurableOpenError> {
    if head.agent_id() != path_agent_id
        || definition.agent_id() != path_agent_id
        || head.storage_generation() != path_generation
        || head.storage_generation().get() != 1
        || head.previous_storage_generation().is_some()
        || head.current_definition_revision().get() != 1
        || head.current_definition_storage_generation() != path_generation
        || definition.revision().get() != 1
        || head.metadata().revision().get() != 1
        || head.status() != AgentStatus::Enabled
        || head.created_at() != definition.created_at()
        || head.created_at() != head.metadata().updated_at()
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(())
}

fn validate_agent_generation_transition(
    path_agent_id: AgentId,
    path_generation: StorageGeneration,
    previous_head: &DurableAgentHead,
    current_definition: &AgentDefinition,
    head: &DurableAgentHead,
    definition: Option<&AgentDefinition>,
) -> Result<(), DurableOpenError> {
    if head.agent_id() != path_agent_id
        || head.storage_generation() != path_generation
        || head.previous_storage_generation() != Some(previous_head.storage_generation())
        || previous_head.status() == AgentStatus::Deleted
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    match definition {
        Some(definition) => validate_agent_definition_transition(
            path_agent_id,
            path_generation,
            previous_head,
            current_definition,
            head,
            definition,
        ),
        None if agent_metadata_transition_is_valid(previous_head, head) => Ok(()),
        None if agent_status_transition_is_valid(previous_head, head) => Ok(()),
        None => Err(DurableOpenError::DurableStateCorrupt),
    }
}

fn validate_agent_definition_transition(
    path_agent_id: AgentId,
    path_generation: StorageGeneration,
    previous_head: &DurableAgentHead,
    current_definition: &AgentDefinition,
    head: &DurableAgentHead,
    definition: &AgentDefinition,
) -> Result<(), DurableOpenError> {
    if definition.agent_id() != path_agent_id
        || !is_exact_next_agent_revision(
            previous_head.current_definition_revision(),
            head.current_definition_revision(),
        )
        || definition.revision() != head.current_definition_revision()
        || head.current_definition_storage_generation() != path_generation
        || head.metadata() != previous_head.metadata()
        || head.status() != previous_head.status()
        || head.created_at() != previous_head.created_at()
        || agent_definitions_have_same_canonical_execution_content(current_definition, definition)
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(())
}

fn agent_metadata_transition_is_valid(
    previous_head: &DurableAgentHead,
    head: &DurableAgentHead,
) -> bool {
    head.current_definition_revision() == previous_head.current_definition_revision()
        && head.current_definition_storage_generation()
            == previous_head.current_definition_storage_generation()
        && is_exact_next_agent_metadata_revision(
            previous_head.metadata().revision(),
            head.metadata().revision(),
        )
        && !agent_metadata_has_same_canonical_content(previous_head.metadata(), head.metadata())
        && head.status() == previous_head.status()
        && head.created_at() == previous_head.created_at()
}

fn agent_status_transition_is_valid(
    previous_head: &DurableAgentHead,
    head: &DurableAgentHead,
) -> bool {
    head.current_definition_revision() == previous_head.current_definition_revision()
        && head.current_definition_storage_generation()
            == previous_head.current_definition_storage_generation()
        && head.metadata() == previous_head.metadata()
        && head.created_at() == previous_head.created_at()
        && is_legal_agent_status_transition(previous_head.status(), head.status())
}

fn is_exact_next_agent_revision(previous: AgentRevision, next: AgentRevision) -> bool {
    previous.get().checked_add(1) == Some(next.get())
}

fn is_exact_next_agent_metadata_revision(
    previous: AgentMetadataRevision,
    next: AgentMetadataRevision,
) -> bool {
    previous.get().checked_add(1) == Some(next.get())
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

fn decode_agent_head_document(path: &Path) -> Result<DurableAgentHead, DurableOpenError> {
    let bytes = read_durable_document(path)?;
    DurableStoreV1Codec::decode_agent_head(&bytes).map_err(map_durable_document_codec_error)
}

fn decode_agent_definition_document(path: &Path) -> Result<AgentDefinition, DurableOpenError> {
    let bytes = read_durable_document(path)?;
    DurableStoreV1Codec::decode_agent_definition(&bytes).map_err(map_durable_document_codec_error)
}

fn map_durable_document_codec_error(error: DurableStoreCodecError) -> DurableOpenError {
    match error {
        DurableStoreCodecError::DocumentTooLarge | DurableStoreCodecError::JsonStructure => {
            DurableOpenError::DurableStateTooLarge
        }
        DurableStoreCodecError::InvalidDocument
        | DurableStoreCodecError::InvalidShape
        | DurableStoreCodecError::InvalidScalar
        | DurableStoreCodecError::InvalidSemantic
        | DurableStoreCodecError::Noncanonical => DurableOpenError::DurableStateCorrupt,
    }
}

/// Reads one Store V1 document while retaining a bounded, same-open physical observation.
/// Paths, lengths, and OS errors deliberately remain within this private bridge.
fn read_durable_document(path: &Path) -> Result<Vec<u8>, DurableOpenError> {
    let initial_path_metadata = metadata_without_following(path)?;
    validate_existing_regular_file(
        &initial_path_metadata,
        DurableOpenError::DurableStateCorrupt,
    )?;
    if initial_path_metadata.len() > MAX_DURABLE_DOCUMENT_BYTES as u64 {
        return Err(DurableOpenError::DurableStateTooLarge);
    }

    let mut file = File::open(path).map_err(|_| DurableOpenError::StorageUnavailable)?;
    let handle_metadata = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    let opened_path_metadata = metadata_without_following(path)?;
    validate_open_file_observation(
        &initial_path_metadata,
        &handle_metadata,
        &opened_path_metadata,
    )?;

    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8_192];
    let maximum_read = MAX_DURABLE_DOCUMENT_BYTES + 1;
    while bytes.len() < maximum_read {
        let remaining = maximum_read - bytes.len();
        let request_length = remaining.min(buffer.len());
        let read = file
            .read(&mut buffer[..request_length])
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }

    let final_handle_metadata = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    let final_path_metadata = metadata_without_following(path)?;
    validate_open_file_observation(
        &initial_path_metadata,
        &final_handle_metadata,
        &final_path_metadata,
    )?;
    let expected_length = usize::try_from(initial_path_metadata.len())
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    if bytes.len() != expected_length {
        return Err(DurableOpenError::StorageUnavailable);
    }
    if bytes.len() > MAX_DURABLE_DOCUMENT_BYTES {
        return Err(DurableOpenError::DurableStateTooLarge);
    }
    Ok(bytes)
}

fn validate_open_file_observation(
    initial_path_metadata: &fs::Metadata,
    handle_metadata: &fs::Metadata,
    path_metadata: &fs::Metadata,
) -> Result<(), DurableOpenError> {
    validate_existing_regular_file(handle_metadata, DurableOpenError::StorageUnavailable)?;
    validate_existing_regular_file(path_metadata, DurableOpenError::StorageUnavailable)?;
    if initial_path_metadata.len() != handle_metadata.len()
        || initial_path_metadata.len() != path_metadata.len()
    {
        return Err(DurableOpenError::StorageUnavailable);
    }
    validate_same_file_identity(initial_path_metadata, handle_metadata)?;
    validate_same_file_identity(handle_metadata, path_metadata)
}

#[cfg(unix)]
fn validate_same_file_identity(
    first: &fs::Metadata,
    second: &fs::Metadata,
) -> Result<(), DurableOpenError> {
    use std::os::unix::fs::MetadataExt;

    if first.dev() != second.dev() || first.ino() != second.ino() {
        return Err(DurableOpenError::StorageUnavailable);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_same_file_identity(
    _first: &fs::Metadata,
    _second: &fs::Metadata,
) -> Result<(), DurableOpenError> {
    Ok(())
}

fn validate_zero_regular_file(path: &Path) -> Result<(), DurableOpenError> {
    let initial_path_metadata = metadata_without_following(path)?;
    validate_existing_regular_file(
        &initial_path_metadata,
        DurableOpenError::DurableStateCorrupt,
    )?;
    if initial_path_metadata.len() != 0 {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    let mut file = File::open(path).map_err(|_| DurableOpenError::StorageUnavailable)?;
    let handle_metadata = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    let opened_path_metadata = metadata_without_following(path)?;
    validate_open_file_observation(
        &initial_path_metadata,
        &handle_metadata,
        &opened_path_metadata,
    )?;

    let mut byte = [0_u8; 1];
    if file
        .read(&mut byte)
        .map_err(|_| DurableOpenError::StorageUnavailable)?
        != 0
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    let final_handle_metadata = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    let final_path_metadata = metadata_without_following(path)?;
    validate_open_file_observation(
        &initial_path_metadata,
        &final_handle_metadata,
        &final_path_metadata,
    )
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
    let mut bounded = Vec::new();
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
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::runtime::Handle;

    use super::{
        AGENTS_DIRECTORY, DurableOpenError, DurableSessionHead, DurableState, FORMAT_MARKER,
        GENERATION_PAYLOAD_ENTRY_CAP, LOCK_FILE, RESERVATIONS_DIRECTORY, RESERVATIONS_ENTRY_CAP,
        ROOT_ENTRY_CAP, RecoveryCaps, SESSIONS_DIRECTORY, StorageGeneration, parse_agent_id_name,
        read_durable_document, read_entries_bounded, recover_marked_root_with_caps,
    };
    use crate::agent_session_lifecycle::{
        AgentStatus, ForkAnchor, ForkSourceKind, SessionForkProvenance, SessionLifecycle,
        SessionMetadata,
    };
    use crate::runtime_task::RuntimeTaskContext;
    use crate::wire::{
        AgentId, AgentRevision, ItemId, SessionDefinitionRevision, SessionId,
        SessionMetadataRevision, Timestamp,
    };

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

    const AGENT_ONE: &str = "agt_11111111111111111111111111111111";
    const AGENT_TWO: &str = "agt_22222222222222222222222222222222";
    const GENERATION_ONE: &str = "00000000000000000001";
    const GENERATION_TWO: &str = "00000000000000000002";
    const GENERATION_THREE: &str = "00000000000000000003";
    const GENERATION_FOUR: &str = "00000000000000000004";

    fn agent_head_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/agent-head.json")
    }

    fn agent_definition_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/agent-definition.json")
    }

    fn agent_head_definition_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/agent-head-2-definition.json")
    }

    fn agent_definition_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/agent-definition-2.json")
    }

    fn agent_head_metadata_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/agent-head-2-metadata.json")
    }

    fn agent_head_status_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/agent-head-2-status.json")
    }

    fn replace_fixture(input: &[u8], from: &str, to: &str) -> Vec<u8> {
        let input = std::str::from_utf8(input).expect("fixture bytes are UTF-8");
        assert_eq!(
            input.matches(from).count(),
            1,
            "fixture replacement must be fixed and unique"
        );
        input.replacen(from, to, 1).into_bytes()
    }

    fn agent_path(root: &Path, agent_id: &str) -> PathBuf {
        root.join(AGENTS_DIRECTORY).join(agent_id)
    }

    fn generation_path(root: &Path, agent_id: &str) -> PathBuf {
        agent_path(root, agent_id)
            .join("generations")
            .join(GENERATION_ONE)
    }

    fn generation_path_named(root: &Path, agent_id: &str, generation: &str) -> PathBuf {
        agent_path(root, agent_id)
            .join("generations")
            .join(generation)
    }

    fn create_exact_g1_agent(root: &Path, agent_id: &str, head: &[u8], definition: &[u8]) {
        create_file(
            &root
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(agent_id),
            b"",
        );
        let entity = agent_path(root, agent_id);
        create_directory(&entity);
        create_file(&entity.join("PUBLISHED"), b"");
        let generations = entity.join("generations");
        create_directory(&generations);
        let generation = generations.join(GENERATION_ONE);
        create_directory(&generation);
        create_file(&generation.join("head.json"), head);
        create_file(&generation.join("definition.json"), definition);
        create_file(&generation.join("COMMITTED"), b"");
    }

    fn create_valid_g1_agent(root: &Path) {
        create_exact_g1_agent(
            root,
            AGENT_ONE,
            agent_head_fixture(),
            agent_definition_fixture(),
        );
    }

    fn create_agent_generation(
        root: &Path,
        agent_id: &str,
        generation: &str,
        head: &[u8],
        definition: Option<&[u8]>,
    ) {
        let path = generation_path_named(root, agent_id, generation);
        create_directory(&path);
        create_file(&path.join("head.json"), head);
        if let Some(definition) = definition {
            create_file(&path.join("definition.json"), definition);
        }
        create_file(&path.join("COMMITTED"), b"");
    }

    fn g3_deleted_status_head() -> Vec<u8> {
        let top_level = replace_fixture(
            agent_head_status_g2_fixture(),
            "\"storageGeneration\":2,\"previousStorageGeneration\":1",
            "\"storageGeneration\":3,\"previousStorageGeneration\":2",
        );
        replace_fixture(
            &top_level,
            "\"status\":\"disabled\"",
            "\"status\":\"deleted\"",
        )
    }

    fn create_chain_through_g3_deleted(root: &Path) {
        create_valid_g1_agent(root);
        create_agent_generation(
            root,
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_status_g2_fixture(),
            None,
        );
        create_agent_generation(
            root,
            AGENT_ONE,
            GENERATION_THREE,
            &g3_deleted_status_head(),
            None,
        );
    }

    fn assert_corrupt(result: Result<DurableState, DurableOpenError>) {
        assert!(matches!(result, Err(DurableOpenError::DurableStateCorrupt)));
    }

    fn marked_entries(root: &Path) -> Vec<std::fs::DirEntry> {
        read_entries_bounded(root, ROOT_ENTRY_CAP).expect("the fixed root is bounded")
    }

    #[test]
    fn private_session_head_enforces_only_single_document_invariants_and_redacts_debug() {
        let child = SessionId::from_str("ses_22222222222222222222222222222222").unwrap();
        let source = SessionId::from_str("ses_33333333333333333333333333333333").unwrap();
        let timestamp = Timestamp::from_str("2026-08-03T10:01:00.456Z").unwrap();
        let metadata = SessionMetadata::new(
            SessionMetadataRevision::new(NonZeroU64::new(1).unwrap()),
            Some("session secret"),
            Some("description secret"),
            timestamp,
        )
        .unwrap();
        let revision = SessionDefinitionRevision::new(NonZeroU64::new(1).unwrap());
        let generation_one = StorageGeneration::new(1).unwrap();
        let head = DurableSessionHead::new(
            child,
            generation_one,
            None,
            revision,
            generation_one,
            metadata.clone(),
            SessionLifecycle::Open,
            Some(SessionForkProvenance::new(
                source,
                ForkSourceKind::RecordedHistory,
                ForkAnchor::AfterUserMessage {
                    item_id: ItemId::from_str("itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
                },
            )),
            timestamp,
        )
        .unwrap();
        assert_eq!(head.session_id(), child);
        assert_eq!(head.storage_generation(), generation_one);
        assert_eq!(head.previous_storage_generation(), None);
        assert_eq!(head.current_definition_revision(), revision);
        assert_eq!(head.current_definition_storage_generation(), generation_one);
        assert_eq!(head.metadata(), &metadata);
        assert_eq!(head.lifecycle(), SessionLifecycle::Open);
        assert_eq!(head.created_at(), timestamp);

        assert!(
            DurableSessionHead::new(
                child,
                generation_one,
                Some(generation_one),
                revision,
                generation_one,
                metadata.clone(),
                SessionLifecycle::Open,
                None,
                timestamp,
            )
            .is_err()
        );
        assert!(
            DurableSessionHead::new(
                child,
                generation_one,
                None,
                revision,
                generation_one,
                metadata,
                SessionLifecycle::Open,
                Some(SessionForkProvenance::new(
                    child,
                    ForkSourceKind::LiveSnapshot,
                    ForkAnchor::Genesis,
                )),
                timestamp,
            )
            .is_err()
        );

        let debug = format!("{head:?}");
        for secret in [
            "ses_22222222222222222222222222222222",
            "ses_33333333333333333333333333333333",
            "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "session secret",
            "description secret",
        ] {
            assert!(!debug.contains(secret), "head debug leaked {secret:?}");
        }
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

    #[test]
    fn storage_generation_directory_names_are_exact_and_redacted() {
        for value in [1, 1_000_000] {
            let generation = StorageGeneration::new(value).expect("the boundary is valid");
            assert_eq!(
                StorageGeneration::parse_directory_name(std::ffi::OsStr::new(
                    &generation.directory_name(),
                ))
                .expect("the canonical name parses"),
                generation
            );
            assert_eq!(
                StorageGeneration::parse_directory_name_str(&generation.directory_name())
                    .expect("the canonical UTF-8 name parses"),
                generation
            );
        }

        for invalid in [
            "00000000000000000000",
            "00000000000001000001",
            "1",
            "0000000000000000001",
            "+0000000000000000001",
            "000000000000000000001",
            "0000000000000000001１",
        ] {
            assert!(StorageGeneration::parse_directory_name_str(invalid).is_err());
        }
        let error = StorageGeneration::parse_directory_name_str("not-a-secret").unwrap_err();
        let debug = format!("{error:?}");
        assert!(!debug.contains("not-a-secret"));
        assert!(!format!("{error}").contains("not-a-secret"));

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let non_utf8 = std::ffi::OsStr::from_bytes(b"agt_\xff");
            assert!(matches!(
                parse_agent_id_name(non_utf8),
                Err(DurableOpenError::DurableStateCorrupt)
            ));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn marked_empty_store_opens_with_an_empty_private_agent_catalog() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());

        let state = open(root.path())
            .await
            .expect("the marked empty store opens");
        let unknown = AgentId::from_str(AGENT_ONE).expect("the fixture ID is valid");
        assert!(state.agent_head(unknown).is_none());
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exact_published_committed_g1_agent_recovers_as_the_same_arc() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());

        let state = open(root.path()).await.expect("the exact G1 Agent opens");
        let agent_id = AgentId::from_str(AGENT_ONE).expect("the fixture ID is valid");
        let first = state.agent_head(agent_id).expect("the head is catalogued");
        let second = state
            .agent_head(agent_id)
            .expect("the same head is catalogued");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.agent_id(), agent_id);
        assert_eq!(first.storage_generation().get(), 1);
        assert_eq!(first.current_definition_revision().get(), 1);
        assert_eq!(first.current_definition_storage_generation().get(), 1);
        assert_eq!(first.metadata().revision().get(), 1);
        assert_eq!(
            first.status(),
            crate::agent_session_lifecycle::AgentStatus::Enabled
        );
        let definition = state
            .agent_current_definition(agent_id)
            .expect("the G1 current definition is retained");
        let same_definition = state
            .agent_current_definition(agent_id)
            .expect("the same G1 current definition is retained");
        assert!(Arc::ptr_eq(&definition, &same_definition));
        assert_eq!(definition.revision().get(), 1);
        assert!(
            state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(1).unwrap())
            )
        );
        assert!(
            !state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(2).unwrap())
            )
        );
        let catalog_debug = format!("{:?}", state.agents.get(&agent_id).unwrap());
        for secret in [AGENT_ONE, "base", "safety"] {
            assert!(
                !catalog_debug.contains(secret),
                "catalog debug leaked {secret:?}"
            );
        }
        assert!(
            state
                .agent_head(AgentId::from_str(AGENT_TWO).expect("a fixed second ID is valid"))
                .is_none()
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_g2_metadata_fixture_retains_the_g1_current_definition() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_agent_generation(
            root.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_metadata_g2_fixture(),
            None,
        );

        let state = open(root.path())
            .await
            .expect("the authoritative G1/G2 metadata chain opens");
        let agent_id = AgentId::from_str(AGENT_ONE).unwrap();
        let head = state.agent_head(agent_id).unwrap();
        assert_eq!(head.storage_generation().get(), 2);
        assert_eq!(head.metadata().revision().get(), 2);
        assert_eq!(
            state
                .agent_current_definition(agent_id)
                .unwrap()
                .revision()
                .get(),
            1
        );
        assert!(
            state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(1).unwrap())
            )
        );
        assert!(
            !state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(2).unwrap())
            )
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_g2_status_fixture_retains_the_g1_current_definition() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_agent_generation(
            root.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_status_g2_fixture(),
            None,
        );

        let state = open(root.path())
            .await
            .expect("the authoritative G1/G2 status chain opens");
        let agent_id = AgentId::from_str(AGENT_ONE).unwrap();
        let head = state.agent_head(agent_id).unwrap();
        assert_eq!(head.storage_generation().get(), 2);
        assert_eq!(head.status(), AgentStatus::Disabled);
        assert_eq!(
            state
                .agent_current_definition(agent_id)
                .unwrap()
                .revision()
                .get(),
            1
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metadata_after_definition_retains_the_latest_definition_and_complete_index() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_agent_generation(
            root.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_definition_g2_fixture(),
            Some(agent_definition_g2_fixture()),
        );
        let g3_metadata = replace_fixture(
            &replace_fixture(
                agent_head_metadata_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":3,\"previousStorageGeneration\":2",
            ),
            "\"currentDefinition\":{\"revision\":\"ar_1\",\"storageGeneration\":1}",
            "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":2}",
        );
        create_agent_generation(root.path(), AGENT_ONE, GENERATION_THREE, &g3_metadata, None);

        let state = open(root.path())
            .await
            .expect("metadata after a definition change retains the G2 definition");
        let agent_id = AgentId::from_str(AGENT_ONE).unwrap();
        let head = state.agent_head(agent_id).unwrap();
        assert_eq!(head.storage_generation().get(), 3);
        assert_eq!(head.metadata().revision().get(), 2);
        assert_eq!(
            state
                .agent_current_definition(agent_id)
                .unwrap()
                .revision()
                .get(),
            2
        );
        for revision in 1..=2 {
            assert!(state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(revision).unwrap())
            ));
        }
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_g3_status_chain_uses_numeric_generation_order_not_creation_order() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        // Materialize G3 before G2 so recovery cannot accidentally trust read_dir order.
        create_agent_generation(
            root.path(),
            AGENT_ONE,
            GENERATION_THREE,
            &g3_deleted_status_head(),
            None,
        );
        create_agent_generation(
            root.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_status_g2_fixture(),
            None,
        );

        let state = open(root.path())
            .await
            .expect("the numeric G1/G2/G3 status chain opens");
        let head = state
            .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
            .unwrap();
        assert_eq!(head.storage_generation().get(), 3);
        assert_eq!(head.status(), AgentStatus::Deleted);
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_delete_and_reenable_status_edges_are_valid() {
        let direct_delete = TempRoot::existing();
        create_marked_empty_store(direct_delete.path());
        create_valid_g1_agent(direct_delete.path());
        create_agent_generation(
            direct_delete.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_status_g2_fixture(),
                "\"status\":\"disabled\"",
                "\"status\":\"deleted\"",
            ),
            None,
        );
        let state = open(direct_delete.path())
            .await
            .expect("an Enabled Agent may become Deleted directly");
        assert_eq!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .unwrap()
                .status(),
            AgentStatus::Deleted
        );
        state.close().await;

        let reenable = TempRoot::existing();
        create_marked_empty_store(reenable.path());
        create_valid_g1_agent(reenable.path());
        create_agent_generation(
            reenable.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_status_g2_fixture(),
            None,
        );
        let g3_enabled = replace_fixture(
            &replace_fixture(
                agent_head_status_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":3,\"previousStorageGeneration\":2",
            ),
            "\"status\":\"disabled\"",
            "\"status\":\"enabled\"",
        );
        create_agent_generation(
            reenable.path(),
            AGENT_ONE,
            GENERATION_THREE,
            &g3_enabled,
            None,
        );
        let state = open(reenable.path())
            .await
            .expect("a Disabled Agent may become Enabled again");
        assert_eq!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .unwrap()
                .status(),
            AgentStatus::Enabled
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn definition_rollback_to_nonadjacent_content_is_valid_when_it_changes_from_current() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_agent_generation(
            root.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_definition_g2_fixture(),
            Some(agent_definition_g2_fixture()),
        );
        let g3_head = replace_fixture(
            &replace_fixture(
                agent_head_definition_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":3,\"previousStorageGeneration\":2",
            ),
            "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":2}",
            "\"currentDefinition\":{\"revision\":\"ar_3\",\"storageGeneration\":3}",
        );
        let g3_definition = replace_fixture(
            &replace_fixture(
                agent_definition_fixture(),
                "\"revision\":\"ar_1\"",
                "\"revision\":\"ar_3\"",
            ),
            "\"createdAt\":\"2026-08-03T10:00:00.123Z\"",
            "\"createdAt\":\"2026-08-03T10:00:06.000Z\"",
        );
        create_agent_generation(
            root.path(),
            AGENT_ONE,
            GENERATION_THREE,
            &g3_head,
            Some(&g3_definition),
        );

        let state = open(root.path())
            .await
            .expect("a definition may roll back to earlier nonadjacent execution content");
        let agent_id = AgentId::from_str(AGENT_ONE).unwrap();
        assert_eq!(
            state
                .agent_current_definition(agent_id)
                .unwrap()
                .revision()
                .get(),
            3
        );
        for revision in 1..=3 {
            assert!(state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(revision).unwrap())
            ));
        }
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn definition_metadata_and_status_canonical_no_ops_are_corrupt() {
        let definition_no_op = TempRoot::existing();
        create_marked_empty_store(definition_no_op.path());
        create_valid_g1_agent(definition_no_op.path());
        let repeated_execution_content = replace_fixture(
            &replace_fixture(
                agent_definition_fixture(),
                "\"revision\":\"ar_1\"",
                "\"revision\":\"ar_2\"",
            ),
            "\"createdAt\":\"2026-08-03T10:00:00.123Z\"",
            "\"createdAt\":\"2026-08-03T10:00:01.000Z\"",
        );
        create_agent_generation(
            definition_no_op.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_definition_g2_fixture(),
            Some(&repeated_execution_content),
        );
        assert_corrupt(open(definition_no_op.path()).await);

        let metadata_no_op = TempRoot::existing();
        create_marked_empty_store(metadata_no_op.path());
        create_valid_g1_agent(metadata_no_op.path());
        let repeated_metadata_content = replace_fixture(
            &replace_fixture(
                agent_head_metadata_g2_fixture(),
                "\"name\":\"Planner revised\"",
                "\"name\":\"Planner\"",
            ),
            "\"updatedAt\":\"2026-08-03T10:00:00.123Z\"",
            "\"updatedAt\":\"2026-08-03T10:00:01.000Z\"",
        );
        create_agent_generation(
            metadata_no_op.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &repeated_metadata_content,
            None,
        );
        assert_corrupt(open(metadata_no_op.path()).await);

        let same_status = TempRoot::existing();
        create_marked_empty_store(same_status.path());
        create_valid_g1_agent(same_status.path());
        create_agent_generation(
            same_status.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_status_g2_fixture(),
                "\"status\":\"disabled\"",
                "\"status\":\"enabled\"",
            ),
            None,
        );
        assert_corrupt(open(same_status.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_categories_revision_reuse_and_pointer_or_payload_mismatches_are_corrupt() {
        let mixed = TempRoot::existing();
        create_marked_empty_store(mixed.path());
        create_valid_g1_agent(mixed.path());
        create_agent_generation(
            mixed.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_definition_g2_fixture(),
                "\"status\":\"enabled\"",
                "\"status\":\"disabled\"",
            ),
            Some(agent_definition_g2_fixture()),
        );
        assert_corrupt(open(mixed.path()).await);

        let mixed_metadata_status = TempRoot::existing();
        create_marked_empty_store(mixed_metadata_status.path());
        create_valid_g1_agent(mixed_metadata_status.path());
        create_agent_generation(
            mixed_metadata_status.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_metadata_g2_fixture(),
                "\"status\":\"enabled\"",
                "\"status\":\"disabled\"",
            ),
            None,
        );
        assert_corrupt(open(mixed_metadata_status.path()).await);

        let revision_jump = TempRoot::existing();
        create_marked_empty_store(revision_jump.path());
        create_valid_g1_agent(revision_jump.path());
        create_agent_generation(
            revision_jump.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_definition_g2_fixture(),
                "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":2}",
                "\"currentDefinition\":{\"revision\":\"ar_3\",\"storageGeneration\":2}",
            ),
            Some(&replace_fixture(
                agent_definition_g2_fixture(),
                "\"revision\":\"ar_2\"",
                "\"revision\":\"ar_3\"",
            )),
        );
        assert_corrupt(open(revision_jump.path()).await);

        let metadata_revision_reuse = TempRoot::existing();
        create_marked_empty_store(metadata_revision_reuse.path());
        create_valid_g1_agent(metadata_revision_reuse.path());
        create_agent_generation(
            metadata_revision_reuse.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_metadata_g2_fixture(),
                "\"revision\":\"amr_2\"",
                "\"revision\":\"amr_1\"",
            ),
            None,
        );
        assert_corrupt(open(metadata_revision_reuse.path()).await);

        let pointer_mismatch = TempRoot::existing();
        create_marked_empty_store(pointer_mismatch.path());
        create_valid_g1_agent(pointer_mismatch.path());
        create_agent_generation(
            pointer_mismatch.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_definition_g2_fixture(),
                "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":2}",
                "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":1}",
            ),
            Some(agent_definition_g2_fixture()),
        );
        assert_corrupt(open(pointer_mismatch.path()).await);

        let pointer_without_definition = TempRoot::existing();
        create_marked_empty_store(pointer_without_definition.path());
        create_valid_g1_agent(pointer_without_definition.path());
        create_agent_generation(
            pointer_without_definition.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_metadata_g2_fixture(),
                "\"currentDefinition\":{\"revision\":\"ar_1\",\"storageGeneration\":1}",
                "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":2}",
            ),
            None,
        );
        assert_corrupt(open(pointer_without_definition.path()).await);

        let missing_definition = TempRoot::existing();
        create_marked_empty_store(missing_definition.path());
        create_valid_g1_agent(missing_definition.path());
        create_agent_generation(
            missing_definition.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_definition_g2_fixture(),
            None,
        );
        assert_corrupt(open(missing_definition.path()).await);

        let unexpected_definition = TempRoot::existing();
        create_marked_empty_store(unexpected_definition.path());
        create_valid_g1_agent(unexpected_definition.path());
        create_agent_generation(
            unexpected_definition.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_metadata_g2_fixture(),
            Some(agent_definition_g2_fixture()),
        );
        assert_corrupt(open(unexpected_definition.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn generation_gap_g2_alone_and_corrupt_highest_generation_never_fall_back() {
        let gap = TempRoot::existing();
        create_marked_empty_store(gap.path());
        create_valid_g1_agent(gap.path());
        create_agent_generation(
            gap.path(),
            AGENT_ONE,
            GENERATION_THREE,
            &g3_deleted_status_head(),
            None,
        );
        assert_corrupt(open(gap.path()).await);

        let g2_alone = TempRoot::existing();
        create_marked_empty_store(g2_alone.path());
        create_valid_g1_agent(g2_alone.path());
        fs::rename(
            generation_path(g2_alone.path(), AGENT_ONE),
            generation_path_named(g2_alone.path(), AGENT_ONE, GENERATION_TWO),
        )
        .unwrap();
        assert_corrupt(open(g2_alone.path()).await);

        let corrupt_highest = TempRoot::existing();
        create_marked_empty_store(corrupt_highest.path());
        create_valid_g1_agent(corrupt_highest.path());
        create_agent_generation(
            corrupt_highest.path(),
            AGENT_ONE,
            GENERATION_TWO,
            b"not canonical JSON\n",
            None,
        );
        assert_corrupt(open(corrupt_highest.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_markerless_trailing_generation_fails_closed_until_cleanup_is_implemented() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let pending = generation_path_named(root.path(), AGENT_ONE, GENERATION_TWO);
        create_directory(&pending);
        create_file(&pending.join("head.json"), agent_head_metadata_g2_fixture());

        assert_corrupt(open(root.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deleted_agent_rejects_every_later_definition_metadata_or_status_generation() {
        let definition = TempRoot::existing();
        create_marked_empty_store(definition.path());
        create_chain_through_g3_deleted(definition.path());
        let definition_after_deleted = replace_fixture(
            &replace_fixture(
                agent_head_definition_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":4,\"previousStorageGeneration\":3",
            ),
            "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":2}",
            "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":4}",
        );
        let definition_after_deleted = replace_fixture(
            &definition_after_deleted,
            "\"status\":\"enabled\"",
            "\"status\":\"deleted\"",
        );
        create_agent_generation(
            definition.path(),
            AGENT_ONE,
            GENERATION_FOUR,
            &definition_after_deleted,
            Some(agent_definition_g2_fixture()),
        );
        assert_corrupt(open(definition.path()).await);

        let metadata = TempRoot::existing();
        create_marked_empty_store(metadata.path());
        create_chain_through_g3_deleted(metadata.path());
        let metadata_after_deleted = replace_fixture(
            &replace_fixture(
                agent_head_metadata_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":4,\"previousStorageGeneration\":3",
            ),
            "\"status\":\"enabled\"",
            "\"status\":\"deleted\"",
        );
        create_agent_generation(
            metadata.path(),
            AGENT_ONE,
            GENERATION_FOUR,
            &metadata_after_deleted,
            None,
        );
        assert_corrupt(open(metadata.path()).await);

        let status = TempRoot::existing();
        create_marked_empty_store(status.path());
        create_chain_through_g3_deleted(status.path());
        let status_after_deleted = replace_fixture(
            &replace_fixture(
                &g3_deleted_status_head(),
                "\"storageGeneration\":3,\"previousStorageGeneration\":2",
                "\"storageGeneration\":4,\"previousStorageGeneration\":3",
            ),
            "\"status\":\"deleted\"",
            "\"status\":\"disabled\"",
        );
        create_agent_generation(
            status.path(),
            AGENT_ONE,
            GENERATION_FOUR,
            &status_after_deleted,
            None,
        );
        assert_corrupt(open(status.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_g2_definition_recovers_the_current_definition_and_complete_revision_index() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let generation = agent_path(root.path(), AGENT_ONE)
            .join("generations")
            .join(GENERATION_TWO);
        create_directory(&generation);
        create_file(
            &generation.join("head.json"),
            agent_head_definition_g2_fixture(),
        );
        create_file(
            &generation.join("definition.json"),
            agent_definition_g2_fixture(),
        );
        create_file(&generation.join("COMMITTED"), b"");

        let state = open(root.path())
            .await
            .expect("the authoritative G1/G2 definition chain opens");
        let agent_id = AgentId::from_str(AGENT_ONE).expect("the fixture ID is valid");
        let head = state
            .agent_head(agent_id)
            .expect("the latest head is catalogued");
        assert_eq!(head.storage_generation().get(), 2);
        assert_eq!(head.current_definition_revision().get(), 2);
        assert_eq!(
            state
                .agent_current_definition(agent_id)
                .expect("the current definition is retained")
                .revision()
                .get(),
            2
        );
        assert!(
            state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(1).unwrap())
            )
        );
        assert!(
            state.contains_agent_definition(
                agent_id,
                AgentRevision::new(NonZeroU64::new(2).unwrap())
            )
        );
        let index = &state.agents.get(&agent_id).unwrap().definition_index;
        assert_eq!(index.len(), 2);
        assert_eq!(
            index
                .get(&AgentRevision::new(NonZeroU64::new(1).unwrap()))
                .unwrap()
                .get(),
            1
        );
        assert_eq!(
            index
                .get(&AgentRevision::new(NonZeroU64::new(2).unwrap()))
                .unwrap()
                .get(),
            2
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn two_distinct_exact_g1_agents_recover_and_an_orphan_remains_invisible() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_exact_g1_agent(
            root.path(),
            AGENT_TWO,
            &replace_fixture(agent_head_fixture(), AGENT_ONE, AGENT_TWO),
            &replace_fixture(agent_definition_fixture(), AGENT_ONE, AGENT_TWO),
        );
        let orphan = "agt_33333333333333333333333333333333";
        create_file(
            &root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(orphan),
            b"",
        );

        let state = open(root.path()).await.expect("both exact Agents open");
        assert!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).expect("fixture ID is valid"))
                .is_some()
        );
        assert!(
            state
                .agent_head(AgentId::from_str(AGENT_TWO).expect("fixture ID is valid"))
                .is_some()
        );
        assert!(
            state
                .agent_head(AgentId::from_str(orphan).expect("orphan ID is valid"))
                .is_none()
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn published_agent_requires_its_exact_permanent_reservation() {
        let missing = TempRoot::existing();
        create_marked_empty_store(missing.path());
        create_valid_g1_agent(missing.path());
        fs::remove_file(
            missing
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
        )
        .expect("the matching reservation is removed");
        assert_corrupt(open(missing.path()).await);

        let mismatched = TempRoot::existing();
        create_marked_empty_store(mismatched.path());
        create_valid_g1_agent(mismatched.path());
        fs::rename(
            mismatched
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
            mismatched
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_TWO),
        )
        .expect("the reservation becomes a different canonical ID");
        assert_corrupt(open(mismatched.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_agent_reservations_are_corrupt() {
        let invalid_name = TempRoot::existing();
        create_marked_empty_store(invalid_name.path());
        create_file(
            &invalid_name
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join("not-an-agent"),
            b"",
        );
        assert_corrupt(open(invalid_name.path()).await);

        let nonzero = TempRoot::existing();
        create_marked_empty_store(nonzero.path());
        create_file(
            &nonzero
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
            b"not-zero",
        );
        assert_corrupt(open(nonzero.path()).await);

        let directory = TempRoot::existing();
        create_marked_empty_store(directory.path());
        create_directory(
            &directory
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
        );
        assert_corrupt(open(directory.path()).await);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn linked_or_wrong_mode_agent_reservations_are_corrupt() {
        use std::os::unix::fs::symlink;

        let linked = TempRoot::existing();
        create_marked_empty_store(linked.path());
        let target_root = TempRoot::existing();
        let target = target_root.path().join("reservation-target");
        create_file(&target, b"");
        symlink(
            &target,
            linked
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
        )
        .expect("the test creates a symlink");
        assert_corrupt(open(linked.path()).await);

        let wrong_mode = TempRoot::existing();
        create_marked_empty_store(wrong_mode.path());
        let reservation = wrong_mode
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(AGENTS_DIRECTORY)
            .join(AGENT_ONE);
        create_file(&reservation, b"");
        set_unix_mode(&reservation, 0o644);
        assert_corrupt(open(wrong_mode.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_path_head_and_definition_ids_must_agree() {
        let path_mismatch = TempRoot::existing();
        create_marked_empty_store(path_mismatch.path());
        create_valid_g1_agent(path_mismatch.path());
        fs::rename(
            path_mismatch
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
            path_mismatch
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_TWO),
        )
        .expect("the matching reservation is renamed");
        fs::rename(
            agent_path(path_mismatch.path(), AGENT_ONE),
            agent_path(path_mismatch.path(), AGENT_TWO),
        )
        .expect("the entity path is renamed");
        assert_corrupt(open(path_mismatch.path()).await);

        let head_mismatch = TempRoot::existing();
        create_marked_empty_store(head_mismatch.path());
        create_exact_g1_agent(
            head_mismatch.path(),
            AGENT_ONE,
            &replace_fixture(agent_head_fixture(), AGENT_ONE, AGENT_TWO),
            agent_definition_fixture(),
        );
        assert_corrupt(open(head_mismatch.path()).await);

        let definition_mismatch = TempRoot::existing();
        create_marked_empty_store(definition_mismatch.path());
        create_exact_g1_agent(
            definition_mismatch.path(),
            AGENT_ONE,
            agent_head_fixture(),
            &replace_fixture(agent_definition_fixture(), AGENT_ONE, AGENT_TWO),
        );
        assert_corrupt(open(definition_mismatch.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_entity_shape_is_exact_and_entity_cap_precedes_classification() {
        let missing_published = TempRoot::existing();
        create_marked_empty_store(missing_published.path());
        create_valid_g1_agent(missing_published.path());
        fs::remove_file(agent_path(missing_published.path(), AGENT_ONE).join("PUBLISHED"))
            .expect("the visibility marker is removed");
        assert_corrupt(open(missing_published.path()).await);

        let wrong_published = TempRoot::existing();
        create_marked_empty_store(wrong_published.path());
        create_valid_g1_agent(wrong_published.path());
        let published = agent_path(wrong_published.path(), AGENT_ONE).join("PUBLISHED");
        fs::remove_file(&published).expect("the marker is removed before replacing its type");
        create_directory(&published);
        assert_corrupt(open(wrong_published.path()).await);

        let nonzero_published = TempRoot::existing();
        create_marked_empty_store(nonzero_published.path());
        create_valid_g1_agent(nonzero_published.path());
        create_file(
            &agent_path(nonzero_published.path(), AGENT_ONE).join("PUBLISHED"),
            b"not-zero",
        );
        assert_corrupt(open(nonzero_published.path()).await);

        let generations_not_directory = TempRoot::existing();
        create_marked_empty_store(generations_not_directory.path());
        create_valid_g1_agent(generations_not_directory.path());
        let generations =
            agent_path(generations_not_directory.path(), AGENT_ONE).join("generations");
        fs::remove_dir_all(&generations).expect("the generations directory is removed");
        create_file(&generations, b"");
        assert_corrupt(open(generations_not_directory.path()).await);

        let too_many = TempRoot::existing();
        create_marked_empty_store(too_many.path());
        create_valid_g1_agent(too_many.path());
        create_file(
            &agent_path(too_many.path(), AGENT_ONE).join("invalid-third"),
            b"",
        );
        assert!(matches!(
            open(too_many.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn generation_chain_namespace_requires_canonical_names_and_a_contiguous_g1_start() {
        let unpadded = TempRoot::existing();
        create_marked_empty_store(unpadded.path());
        create_valid_g1_agent(unpadded.path());
        fs::rename(
            generation_path(unpadded.path(), AGENT_ONE),
            agent_path(unpadded.path(), AGENT_ONE)
                .join("generations")
                .join("1"),
        )
        .expect("the generation is made unpadded");
        assert_corrupt(open(unpadded.path()).await);

        let g2 = TempRoot::existing();
        create_marked_empty_store(g2.path());
        create_valid_g1_agent(g2.path());
        fs::rename(
            generation_path(g2.path(), AGENT_ONE),
            agent_path(g2.path(), AGENT_ONE)
                .join("generations")
                .join("00000000000000000002"),
        )
        .expect("the lone generation becomes G2");
        assert_corrupt(open(g2.path()).await);

        let multiple = TempRoot::existing();
        create_marked_empty_store(multiple.path());
        create_valid_g1_agent(multiple.path());
        create_directory(
            &agent_path(multiple.path(), AGENT_ONE)
                .join("generations")
                .join("00000000000000000002"),
        );
        assert_corrupt(open(multiple.path()).await);

        let missing = TempRoot::existing();
        create_marked_empty_store(missing.path());
        create_valid_g1_agent(missing.path());
        fs::remove_dir_all(generation_path(missing.path(), AGENT_ONE))
            .expect("the only generation is removed");
        assert_corrupt(open(missing.path()).await);

        let extra = TempRoot::existing();
        create_marked_empty_store(extra.path());
        create_valid_g1_agent(extra.path());
        create_directory(
            &agent_path(extra.path(), AGENT_ONE)
                .join("generations")
                .join("foreign"),
        );
        assert_corrupt(open(extra.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn g1_payload_is_exact_and_payload_cap_precedes_classification() {
        let missing = TempRoot::existing();
        create_marked_empty_store(missing.path());
        create_valid_g1_agent(missing.path());
        fs::remove_file(generation_path(missing.path(), AGENT_ONE).join("head.json"))
            .expect("the required head is removed");
        assert_corrupt(open(missing.path()).await);

        let missing_definition = TempRoot::existing();
        create_marked_empty_store(missing_definition.path());
        create_valid_g1_agent(missing_definition.path());
        fs::remove_file(
            generation_path(missing_definition.path(), AGENT_ONE).join("definition.json"),
        )
        .expect("the required definition is removed");
        assert_corrupt(open(missing_definition.path()).await);

        let missing_committed = TempRoot::existing();
        create_marked_empty_store(missing_committed.path());
        create_valid_g1_agent(missing_committed.path());
        fs::remove_file(generation_path(missing_committed.path(), AGENT_ONE).join("COMMITTED"))
            .expect("the required committed marker is removed");
        assert_corrupt(open(missing_committed.path()).await);

        let wrong_marker = TempRoot::existing();
        create_marked_empty_store(wrong_marker.path());
        create_valid_g1_agent(wrong_marker.path());
        let committed = generation_path(wrong_marker.path(), AGENT_ONE).join("COMMITTED");
        fs::remove_file(&committed).expect("the marker is removed before replacement");
        create_directory(&committed);
        assert_corrupt(open(wrong_marker.path()).await);

        let nonzero_marker = TempRoot::existing();
        create_marked_empty_store(nonzero_marker.path());
        create_valid_g1_agent(nonzero_marker.path());
        create_file(
            &generation_path(nonzero_marker.path(), AGENT_ONE).join("COMMITTED"),
            b"not-zero",
        );
        assert_corrupt(open(nonzero_marker.path()).await);

        let wrong_document_type = TempRoot::existing();
        create_marked_empty_store(wrong_document_type.path());
        create_valid_g1_agent(wrong_document_type.path());
        let definition =
            generation_path(wrong_document_type.path(), AGENT_ONE).join("definition.json");
        fs::remove_file(&definition).expect("the document is removed before type replacement");
        create_directory(&definition);
        assert_corrupt(open(wrong_document_type.path()).await);

        let too_many = TempRoot::existing();
        create_marked_empty_store(too_many.path());
        create_valid_g1_agent(too_many.path());
        create_file(
            &generation_path(too_many.path(), AGENT_ONE).join("invalid-fourth"),
            b"",
        );
        assert_eq!(GENERATION_PAYLOAD_ENTRY_CAP, 3);
        assert!(matches!(
            open(too_many.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn g1_payload_wrong_unix_mode_is_corrupt() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let head = generation_path(root.path(), AGENT_ONE).join("head.json");
        set_unix_mode(&head, 0o644);
        assert_corrupt(open(root.path()).await);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn g1_payload_symlink_is_corrupt() {
        use std::os::unix::fs::symlink;

        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let head = generation_path(root.path(), AGENT_ONE).join("head.json");
        fs::remove_file(&head).expect("the head is removed before linking");
        let target_root = TempRoot::existing();
        let target = target_root.path().join("head-target");
        create_file(&target, agent_head_fixture());
        symlink(&target, &head).expect("the payload symlink is created");
        assert_corrupt(open(root.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn g1_semantic_matrix_rejects_noninitial_revisions_status_pointer_and_timestamps() {
        let cases: [(&str, Vec<u8>, Vec<u8>); 8] = [
            (
                "head agent revision",
                replace_fixture(
                    agent_head_fixture(),
                    "\"revision\":\"ar_1\",\"storageGeneration\":1",
                    "\"revision\":\"ar_2\",\"storageGeneration\":1",
                ),
                agent_definition_fixture().to_vec(),
            ),
            (
                "definition agent revision",
                agent_head_fixture().to_vec(),
                replace_fixture(
                    agent_definition_fixture(),
                    "\"revision\":\"ar_1\"",
                    "\"revision\":\"ar_2\"",
                ),
            ),
            (
                "metadata revision",
                replace_fixture(
                    agent_head_fixture(),
                    "\"revision\":\"amr_1\"",
                    "\"revision\":\"amr_2\"",
                ),
                agent_definition_fixture().to_vec(),
            ),
            (
                "disabled",
                replace_fixture(
                    agent_head_fixture(),
                    "\"status\":\"enabled\"",
                    "\"status\":\"disabled\"",
                ),
                agent_definition_fixture().to_vec(),
            ),
            (
                "deleted",
                replace_fixture(
                    agent_head_fixture(),
                    "\"status\":\"enabled\"",
                    "\"status\":\"deleted\"",
                ),
                agent_definition_fixture().to_vec(),
            ),
            (
                "definition pointer",
                replace_fixture(
                    agent_head_fixture(),
                    "\"currentDefinition\":{\"revision\":\"ar_1\",\"storageGeneration\":1}",
                    "\"currentDefinition\":{\"revision\":\"ar_2\",\"storageGeneration\":1}",
                ),
                agent_definition_fixture().to_vec(),
            ),
            (
                "created timestamp",
                replace_fixture(
                    agent_head_fixture(),
                    "\"createdAt\":\"2026-08-03T10:00:00.123Z\"",
                    "\"createdAt\":\"2026-08-03T10:00:00.124Z\"",
                ),
                agent_definition_fixture().to_vec(),
            ),
            (
                "metadata timestamp",
                replace_fixture(
                    agent_head_fixture(),
                    "\"updatedAt\":\"2026-08-03T10:00:00.123Z\"",
                    "\"updatedAt\":\"2026-08-03T10:00:00.124Z\"",
                ),
                agent_definition_fixture().to_vec(),
            ),
        ];

        for (name, head, definition) in cases {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_exact_g1_agent(root.path(), AGENT_ONE, &head, &definition);
            assert!(
                matches!(
                    open(root.path()).await,
                    Err(DurableOpenError::DurableStateCorrupt)
                ),
                "{name} must not be accepted as G1"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_and_structurally_oversized_documents_map_to_the_closed_errors() {
        let malformed = TempRoot::existing();
        create_marked_empty_store(malformed.path());
        create_exact_g1_agent(
            malformed.path(),
            AGENT_ONE,
            b"not JSON\n",
            agent_definition_fixture(),
        );
        assert_corrupt(open(malformed.path()).await);

        let structural = TempRoot::existing();
        create_marked_empty_store(structural.path());
        let mut deeply_nested = b"{\"x\":".repeat(65);
        deeply_nested.push(b'0');
        deeply_nested.extend(std::iter::repeat_n(b'}', 65));
        deeply_nested.push(b'\n');
        create_exact_g1_agent(
            structural.path(),
            AGENT_ONE,
            agent_head_fixture(),
            &deeply_nested,
        );
        assert!(matches!(
            open(structural.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));

        let too_large = TempRoot::existing();
        create_marked_empty_store(too_large.path());
        let oversized = vec![b'x'; super::MAX_DURABLE_DOCUMENT_BYTES + 1];
        create_exact_g1_agent(
            too_large.path(),
            AGENT_ONE,
            agent_head_fixture(),
            &oversized,
        );
        assert!(matches!(
            open(too_large.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
    }

    #[test]
    fn physical_document_reader_is_bounded_and_redacts_its_failures() {
        let root = TempRoot::existing();
        let exact = root.path().join("private-document-name");
        let exact_bytes = vec![b'x'; super::MAX_DURABLE_DOCUMENT_BYTES];
        create_file(&exact, &exact_bytes);
        assert_eq!(
            read_durable_document(&exact)
                .expect("an exact-cap regular document is accepted")
                .len(),
            super::MAX_DURABLE_DOCUMENT_BYTES
        );

        let plus_one = root.path().join("private-document-name-plus-one");
        create_file(
            &plus_one,
            &vec![b'x'; super::MAX_DURABLE_DOCUMENT_BYTES + 1],
        );
        let error = read_durable_document(&plus_one).unwrap_err();
        assert_eq!(error, DurableOpenError::DurableStateTooLarge);
        let debug = format!("{error:?}");
        assert!(!debug.contains("private-document-name"));
        assert!(!debug.contains("1048577"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sessions_are_required_empty_and_small_caps_take_precedence() {
        let root_sessions = TempRoot::existing();
        create_marked_empty_store(root_sessions.path());
        create_file(
            &root_sessions
                .path()
                .join(SESSIONS_DIRECTORY)
                .join("invalid"),
            b"",
        );
        assert_corrupt(open(root_sessions.path()).await);

        let reservation_sessions = TempRoot::existing();
        create_marked_empty_store(reservation_sessions.path());
        create_file(
            &reservation_sessions
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join("invalid"),
            b"",
        );
        assert_corrupt(open(reservation_sessions.path()).await);

        let cap_root_sessions = TempRoot::existing();
        create_marked_empty_store(cap_root_sessions.path());
        create_file(
            &cap_root_sessions
                .path()
                .join(SESSIONS_DIRECTORY)
                .join("invalid-one"),
            b"",
        );
        create_file(
            &cap_root_sessions
                .path()
                .join(SESSIONS_DIRECTORY)
                .join("invalid-two"),
            b"",
        );
        let caps = RecoveryCaps {
            agent_reservations: 1,
            session_reservations: 1,
            root_agents: 1,
            root_sessions: 1,
            generations: 1,
        };
        assert!(matches!(
            recover_marked_root_with_caps(
                cap_root_sessions.path(),
                &marked_entries(cap_root_sessions.path()),
                caps
            ),
            Err(DurableOpenError::DurableStateTooLarge)
        ));

        let cap_reservation_sessions = TempRoot::existing();
        create_marked_empty_store(cap_reservation_sessions.path());
        let sessions = cap_reservation_sessions
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(SESSIONS_DIRECTORY);
        create_file(&sessions.join("invalid-one"), b"");
        create_file(&sessions.join("invalid-two"), b"");
        assert!(matches!(
            recover_marked_root_with_caps(
                cap_reservation_sessions.path(),
                &marked_entries(cap_reservation_sessions.path()),
                caps,
            ),
            Err(DurableOpenError::DurableStateTooLarge)
        ));
    }

    #[test]
    fn recovery_collection_caps_precede_agent_name_or_shape_classification() {
        let caps = RecoveryCaps {
            agent_reservations: 1,
            session_reservations: 1,
            root_agents: 1,
            root_sessions: 1,
            generations: 1,
        };

        let reservations = TempRoot::existing();
        create_marked_empty_store(reservations.path());
        let reservation_directory = reservations
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(AGENTS_DIRECTORY);
        create_file(&reservation_directory.join("invalid-one"), b"");
        create_file(&reservation_directory.join("invalid-two"), b"");
        assert!(matches!(
            recover_marked_root_with_caps(
                reservations.path(),
                &marked_entries(reservations.path()),
                caps
            ),
            Err(DurableOpenError::DurableStateTooLarge)
        ));

        let entities = TempRoot::existing();
        create_marked_empty_store(entities.path());
        create_directory(&entities.path().join(AGENTS_DIRECTORY).join("invalid-one"));
        create_directory(&entities.path().join(AGENTS_DIRECTORY).join("invalid-two"));
        assert!(matches!(
            recover_marked_root_with_caps(entities.path(), &marked_entries(entities.path()), caps),
            Err(DurableOpenError::DurableStateTooLarge)
        ));

        let generations = TempRoot::existing();
        create_marked_empty_store(generations.path());
        create_valid_g1_agent(generations.path());
        create_directory(
            &agent_path(generations.path(), AGENT_ONE)
                .join("generations")
                .join("invalid-second"),
        );
        assert!(matches!(
            recover_marked_root_with_caps(
                generations.path(),
                &marked_entries(generations.path()),
                caps
            ),
            Err(DurableOpenError::DurableStateTooLarge)
        ));
    }
}
