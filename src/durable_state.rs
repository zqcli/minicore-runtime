use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, DirEntry, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use fs4::fs_std::FileExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent_session_lifecycle::{
    AgentDefinition, AgentMetadata, AgentStatus, SessionDefinition, SessionForkProvenance,
    SessionLifecycle, SessionMetadata, agent_definitions_have_same_canonical_execution_content,
    agent_metadata_has_same_canonical_content, is_legal_agent_status_transition,
    is_legal_session_lifecycle_transition,
    session_definitions_have_same_canonical_execution_content,
    session_metadata_has_same_canonical_content,
};
use crate::conversation_storage::{
    SessionHeader, UnpublishedConversationRecoveryError, UnpublishedConversationRecoveryShape,
    validate_unpublished_conversation_for_recovery,
};
use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedTask};
use crate::wire::conversation_jsonl_scanner::MAX_CONVERSATION_FILE_BYTES;
use crate::wire::durable_store::{
    DurableStoreCodecError, DurableStoreV1Codec, MAX_DURABLE_DOCUMENT_BYTES,
};
use crate::wire::{
    AgentId, AgentMetadataRevision, AgentRevision, SessionDefinitionRevision, SessionId,
    SessionMetadataRevision, Timestamp,
};
use crate::workspace::workspace_revision_transition_is_valid;

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
const SESSION_ENTITY_ENTRY_CAP: usize = 3;
const ROOT_AGENT_ENTRY_CAP: usize = 1_000_000;
const ROOT_SESSION_ENTRY_CAP: usize = 1_000_000;
const GENERATION_ENTRY_CAP: usize = 1_000_000;
const GENERATION_PAYLOAD_ENTRY_CAP: usize = 3;
const DURABLE_STATE_ACTOR_QUEUE_CAPACITY: usize = 1;

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

/// One recovered Session's immutable current facts. Rolling adjacent-generation validation
/// carries the current definition forward, and Store V1 has no separately addressable durable
/// historical Session reference, so the catalog deliberately needs no historical Session index.
struct DurableSessionCatalogEntry {
    current_head: Arc<DurableSessionHead>,
    current_definition: Arc<SessionDefinition>,
}

impl fmt::Debug for DurableSessionCatalogEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableSessionCatalogEntry")
            .field("current_head", &"redacted")
            .field("current_definition", &"redacted")
            .finish()
    }
}

struct RecoveredCatalog {
    agents: BTreeMap<AgentId, DurableAgentCatalogEntry>,
    sessions: BTreeMap<SessionId, DurableSessionCatalogEntry>,
}

/// A fully validated recovery result. The catalog cannot become observable until its optional
/// cleanup has completed under the still-held root lease.
struct RecoveryCandidate {
    catalog: RecoveredCatalog,
    cleanup: Option<DeferredCleanup>,
}

/// The physical files which make up a committed generation. These paths are deliberately
/// private scanner output: callers only use them for the existing domain decode/fold pass.
struct CommittedGenerationFiles {
    generation: StorageGeneration,
    head_path: PathBuf,
    definition_path: Option<PathBuf>,
}

/// A markerless generation's exact, non-semantic physical shape. Its documents must never be
/// decoded because each allowed subset can be an interrupted write or an interrupted cleanup.
struct MarkerlessGenerationFiles {
    generation: StorageGeneration,
    generation_path: PathBuf,
    generations_parent: PathBuf,
    has_head: bool,
    has_definition: bool,
}

struct PhysicalGenerationScan {
    committed: Vec<CommittedGenerationFiles>,
    markerless: Option<MarkerlessGenerationFiles>,
}

enum PhysicalGenerationPayload {
    Committed(CommittedGenerationFiles),
    Markerless(MarkerlessGenerationFiles),
}

/// The completed physical scan of one direct entity namespace. `PUBLISHED` is the only branch
/// point: an observed marker, even an invalid one, never falls through to unpublished staging.
enum ScannedAgentEntity {
    Published(PhysicalGenerationScan),
    Unpublished(UnpublishedEntityFiles),
}

enum ScannedSessionEntity {
    Published(PhysicalGenerationScan),
    Unpublished(UnpublishedEntityFiles),
}

/// Exact markerless new-entity facts. The plan later owns only these paths and booleans, never
/// scanner handles. `conversation_path` is meaningful only for Session staging.
struct UnpublishedEntityFiles {
    entity_path: PathBuf,
    entity_shape: CleanupNodeIdentityShape,
    generations_path: Option<PathBuf>,
    generations_shape: Option<CleanupNodeIdentityShape>,
    generation: Option<UnpublishedGenerationFiles>,
    conversation_path: Option<PathBuf>,
    conversation_shape: Option<CleanupRegularFileShape>,
}

/// The sole permitted physical generation inside an unpublished entity: canonical G1 only.
struct UnpublishedGenerationFiles {
    generation_path: PathBuf,
    generation_shape: CleanupNodeIdentityShape,
    has_committed: bool,
    committed_shape: Option<CleanupNodeIdentityShape>,
    has_head: bool,
    has_definition: bool,
    head_shape: Option<CleanupRegularFileShape>,
    definition_shape: Option<CleanupRegularFileShape>,
}

/// Current platform-observable identity facts retained only for cleanup-time revalidation. Unix
/// uses device plus inode; other platforms intentionally retain an empty seam pending the M5.0
/// native identity/reparse process gate. It contains neither a path nor an open handle.
#[derive(Clone, Eq, PartialEq)]
struct CleanupNodeIdentityShape {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// A closed regular-file observation. In addition to its node identity it records the observed
/// byte length, so a same-node content-size drift is not removed.
#[derive(Clone, Eq, PartialEq)]
struct CleanupRegularFileShape {
    length: u64,
    node: CleanupNodeIdentityShape,
}

/// Owned revalidation data for the sole deferred destructive action allowed during one open.
/// It contains no iterator, directory entry, or open file handle, and its Debug form deliberately
/// suppresses the store-local paths.
struct DeferredGenerationCleanup {
    generation_path: PathBuf,
    generations_parent: PathBuf,
    reservation_path: PathBuf,
    published_path: PathBuf,
    has_head: bool,
    has_definition: bool,
}

/// The intentionally narrow set of recovery deletion plans. It is one shared registration slot
/// for both a published trailing generation and a complete invisible new entity.
enum DeferredCleanup {
    TrailingGeneration(DeferredGenerationCleanup),
    UnpublishedEntity(Box<DeferredUnpublishedEntityCleanup>),
}

/// Owned exact-shape facts for a whole invisible Agent or Session entity cleanup.
struct DeferredUnpublishedEntityCleanup {
    entity_path: PathBuf,
    entity_shape: CleanupNodeIdentityShape,
    collection_parent: PathBuf,
    entity_entry_cap: usize,
    generation_collection_cap: usize,
    reservation_path: PathBuf,
    reservation_shape: CleanupNodeIdentityShape,
    published_path: PathBuf,
    generations_path: Option<PathBuf>,
    generations_shape: Option<CleanupNodeIdentityShape>,
    generation_path: Option<PathBuf>,
    generation_shape: Option<CleanupNodeIdentityShape>,
    conversation_path: Option<PathBuf>,
    conversation_shape: Option<CleanupRegularFileShape>,
    has_committed: bool,
    committed_shape: Option<CleanupNodeIdentityShape>,
    has_head: bool,
    has_definition: bool,
    head_shape: Option<CleanupRegularFileShape>,
    definition_shape: Option<CleanupRegularFileShape>,
}

impl fmt::Debug for DeferredGenerationCleanup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeferredGenerationCleanup")
            .field("generation_path", &"redacted")
            .field("generations_parent", &"redacted")
            .field("reservation_path", &"redacted")
            .field("published_path", &"redacted")
            .field("has_head", &self.has_head)
            .field("has_definition", &self.has_definition)
            .finish()
    }
}

impl fmt::Debug for DeferredUnpublishedEntityCleanup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeferredUnpublishedEntityCleanup")
            .field("entity_path", &"redacted")
            .field("collection_parent", &"redacted")
            .field("entity_entry_cap", &self.entity_entry_cap)
            .field("generation_collection_cap", &self.generation_collection_cap)
            .field("reservation_path", &"redacted")
            .field("published_path", &"redacted")
            .field("generations_path", &self.generations_path.is_some())
            .field("generation_path", &self.generation_path.is_some())
            .field("conversation_path", &self.conversation_path.is_some())
            .field("has_committed", &self.has_committed)
            .field("has_head", &self.has_head)
            .field("has_definition", &self.has_definition)
            .finish()
    }
}

impl fmt::Debug for DeferredCleanup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrailingGeneration(cleanup) => formatter
                .debug_tuple("TrailingGeneration")
                .field(cleanup)
                .finish(),
            Self::UnpublishedEntity(cleanup) => formatter
                .debug_tuple("UnpublishedEntity")
                .field(cleanup)
                .finish(),
        }
    }
}

struct RecoveredAgentGenerationChain {
    entry: DurableAgentCatalogEntry,
    markerless: Option<MarkerlessGenerationFiles>,
}

struct RecoveredSessionGenerationChain {
    entry: DurableSessionCatalogEntry,
    markerless: Option<MarkerlessGenerationFiles>,
}

/// The private serial owner for future DurableState mutations. This slice establishes the
/// request/settlement lifecycle only; reservation and durable payload requests stay out of it.
struct DurableStateActor {
    receiver: mpsc::Receiver<DurableStateActorRequestEnvelope>,
    closing: CancellationToken,
    #[cfg(test)]
    receiver_closed: Arc<tokio::sync::Notify>,
}

enum DurableStateActorRequest {
    #[cfg(test)]
    Probe(DurableStateActorProbe),
    #[cfg(not(test))]
    #[allow(
        dead_code,
        reason = "the private probe preserves the actor request seam"
    )]
    Probe,
}

/// A redacted terminal response for a request owned by the durable-state actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActorRequestError {
    Closing,
    Unavailable,
}

impl fmt::Display for ActorRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("durable state actor request rejected")
    }
}

impl std::error::Error for ActorRequestError {}

/// Keeps a request's reply capability with its queued payload until exactly one terminal path
/// consumes it. Dropping an active local request, a queued receiver item, or a failed `try_send`
/// return therefore cannot leave a requester waiting forever.
struct DurableStateActorRequestEnvelope {
    request: DurableStateActorRequest,
    response: Option<oneshot::Sender<Result<(), ActorRequestError>>>,
    drop_error: ActorRequestError,
}

impl DurableStateActorRequestEnvelope {
    #[cfg(test)]
    fn new(
        request: DurableStateActorRequest,
    ) -> (Self, oneshot::Receiver<Result<(), ActorRequestError>>) {
        let (response, waiter) = oneshot::channel();
        (
            Self {
                request,
                response: Some(response),
                drop_error: ActorRequestError::Unavailable,
            },
            waiter,
        )
    }

    fn settle(&mut self, outcome: Result<(), ActorRequestError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    #[cfg(test)]
    fn complete(&mut self) {
        self.settle(Ok(()));
    }

    fn reject_closing(&mut self) {
        self.settle(Err(ActorRequestError::Closing));
    }

    #[cfg(not(test))]
    fn reject_unavailable(&mut self) {
        self.settle(Err(ActorRequestError::Unavailable));
    }

    #[cfg(test)]
    fn reject_on_drop_as(&mut self, error: ActorRequestError) {
        self.drop_error = error;
    }

    #[cfg(test)]
    fn probe_signals(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        match &self.request {
            DurableStateActorRequest::Probe(probe) => {
                (Arc::clone(&probe.entered), Arc::clone(&probe.release))
            }
        }
    }
}

impl Drop for DurableStateActorRequestEnvelope {
    fn drop(&mut self) {
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(self.drop_error));
        }
    }
}

#[cfg(test)]
struct DurableStateActorProbe {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
struct DurableStateActorProbeWaiter {
    response: oneshot::Receiver<Result<(), ActorRequestError>>,
}

#[cfg(test)]
impl DurableStateActorProbeWaiter {
    async fn wait(self) -> Result<(), ActorRequestError> {
        self.response
            .await
            .unwrap_or(Err(ActorRequestError::Unavailable))
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurableStateActorProbeDelivery {
    Accepted,
    Rejected,
}

/// Opaque owner-side control for the one tracked DurableState actor.
#[derive(Clone)]
struct DurableStateActorHandle {
    #[allow(
        dead_code,
        reason = "retains the actor channel's sender end until future durable requests use it"
    )]
    sender: mpsc::Sender<DurableStateActorRequestEnvelope>,
    closing: CancellationToken,
    task: TrackedTask,
    #[cfg(test)]
    receiver_closed: Arc<tokio::sync::Notify>,
}

impl DurableStateActorHandle {
    fn start(task_context: &RuntimeTaskContext) -> Result<Self, RuntimeTaskError> {
        let (sender, receiver) = mpsc::channel(DURABLE_STATE_ACTOR_QUEUE_CAPACITY);
        let closing = CancellationToken::new();
        #[cfg(test)]
        let receiver_closed = Arc::new(tokio::sync::Notify::new());
        let actor = DurableStateActor {
            receiver,
            closing: closing.clone(),
            #[cfg(test)]
            receiver_closed: Arc::clone(&receiver_closed),
        };
        let task = task_context.spawn_tracked(actor.run())?;
        Ok(Self {
            sender,
            closing,
            task,
            #[cfg(test)]
            receiver_closed,
        })
    }

    fn request_closing(&self) {
        self.closing.cancel();
    }

    async fn wait(&self) -> Result<(), RuntimeTaskError> {
        self.task.wait().await
    }

    #[cfg(test)]
    fn enqueue_probe(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) -> DurableStateActorProbeWaiter {
        let (mut request, response) = DurableStateActorRequestEnvelope::new(
            DurableStateActorRequest::Probe(DurableStateActorProbe { entered, release }),
        );
        if self.closing.is_cancelled() {
            request.reject_closing();
            return DurableStateActorProbeWaiter { response };
        }
        let _ = self.try_enqueue(request);
        DurableStateActorProbeWaiter { response }
    }

    /// Test-only deterministic gate between the cancellation fast-path and `try_send`.
    /// The gate runs on a helper thread so the current-thread actor cannot close its receiver
    /// until the test deliberately yields it a turn.
    #[cfg(test)]
    fn enqueue_probe_after_fast_reject_gate(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        checked: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> (DurableStateActorProbeWaiter, DurableStateActorProbeDelivery) {
        let (mut request, response) = DurableStateActorRequestEnvelope::new(
            DurableStateActorRequest::Probe(DurableStateActorProbe { entered, release }),
        );
        if self.closing.is_cancelled() {
            request.reject_closing();
            return (
                DurableStateActorProbeWaiter { response },
                DurableStateActorProbeDelivery::Rejected,
            );
        }
        checked
            .send(())
            .expect("the test retains the fast-reject gate receiver");
        resume
            .recv()
            .expect("the test releases the fast-reject gate sender");
        let delivery = self.try_enqueue(request);
        (DurableStateActorProbeWaiter { response }, delivery)
    }

    /// Test-only deterministic gate after the cancellation fast-path and the capacity
    /// reservation which starts `try_send`. The retained permit represents a sender that won
    /// the pre-close channel gate but has not yet published its envelope.
    #[cfg(test)]
    fn enqueue_probe_after_fast_reject_and_sender_gate(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        checked: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> (DurableStateActorProbeWaiter, DurableStateActorProbeDelivery) {
        let (mut request, response) = DurableStateActorRequestEnvelope::new(
            DurableStateActorRequest::Probe(DurableStateActorProbe { entered, release }),
        );
        if self.closing.is_cancelled() {
            request.reject_closing();
            return (
                DurableStateActorProbeWaiter { response },
                DurableStateActorProbeDelivery::Rejected,
            );
        }
        let permit = match self.sender.try_reserve() {
            Ok(permit) => permit,
            Err(_) => {
                let rejection = if self.closing.is_cancelled() {
                    ActorRequestError::Closing
                } else {
                    ActorRequestError::Unavailable
                };
                request.reject_on_drop_as(rejection);
                drop(request);
                return (
                    DurableStateActorProbeWaiter { response },
                    DurableStateActorProbeDelivery::Rejected,
                );
            }
        };
        checked
            .send(())
            .expect("the test retains the sender-gate receiver");
        resume
            .recv()
            .expect("the test releases the sender-gate sender");
        permit.send(request);
        (
            DurableStateActorProbeWaiter { response },
            DurableStateActorProbeDelivery::Accepted,
        )
    }

    #[cfg(test)]
    fn try_enqueue(
        &self,
        request: DurableStateActorRequestEnvelope,
    ) -> DurableStateActorProbeDelivery {
        match self.sender.try_send(request) {
            Ok(()) => DurableStateActorProbeDelivery::Accepted,
            Err(error) => {
                let mut request = error.into_inner();
                let rejection = if self.closing.is_cancelled() {
                    ActorRequestError::Closing
                } else {
                    ActorRequestError::Unavailable
                };
                request.reject_on_drop_as(rejection);
                drop(request);
                DurableStateActorProbeDelivery::Rejected
            }
        }
    }
}

impl DurableStateActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                _ = self.closing.cancelled() => {
                    self.close_receiver_and_reject_queued().await;
                    return;
                }
                request = self.receiver.recv() => match request {
                    None => return,
                    #[cfg(test)]
                    Some(mut request) => {
                        if self.closing.is_cancelled() {
                            self.close_current_and_reject_queued(&mut request).await;
                            return;
                        }

                        let (entered, release) = request.probe_signals();
                        entered.notify_one();
                        tokio::select! {
                            biased;
                            _ = self.closing.cancelled() => {
                                self.close_current_and_reject_queued(&mut request).await;
                                return;
                            }
                            _ = release.notified() => request.complete(),
                        }
                    }
                    #[cfg(not(test))]
                    Some(mut request) => {
                        let DurableStateActorRequest::Probe = &request.request;
                        request.reject_unavailable();
                    }
                },
            }
        }
    }

    /// `Receiver::close` is the linearization point with concurrent `try_send` calls. Only
    /// after it returns may closing safely settle every request that was accepted before it.
    async fn close_receiver_and_reject_queued(&mut self) {
        self.receiver.close();
        #[cfg(test)]
        self.receiver_closed.notify_waiters();
        self.reject_queued_as_closing().await;
    }

    #[cfg(test)]
    async fn close_current_and_reject_queued(
        &mut self,
        current: &mut DurableStateActorRequestEnvelope,
    ) {
        self.receiver.close();
        self.receiver_closed.notify_waiters();
        current.reject_closing();
        self.reject_queued_as_closing().await;
    }

    async fn reject_queued_as_closing(&mut self) {
        while let Some(mut request) = self.receiver.recv().await {
            request.reject_closing();
        }
    }
}

/// The private owner of the Store V1 root lease and recovered immutable catalog.
#[derive(Clone)]
pub(crate) struct DurableState {
    task_context: RuntimeTaskContext,
    actor: DurableStateActorHandle,
    lease: Arc<RootLease>,
    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later runtime read paths"
    )]
    agents: Arc<BTreeMap<AgentId, DurableAgentCatalogEntry>>,
    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later runtime read paths"
    )]
    sessions: Arc<BTreeMap<SessionId, DurableSessionCatalogEntry>>,
}

impl DurableState {
    /// Opens a Store V1 root and recovers the currently supported immutable catalog slice.
    /// Every filesystem operation runs in one tracked blocking job, never on a Tokio worker.
    pub(crate) async fn open(
        root: PathBuf,
        task_context: RuntimeTaskContext,
    ) -> Result<Self, DurableOpenError> {
        let job = task_context.spawn_blocking_tracked(move || open_root(root));
        let opened = match job.wait().await {
            Ok(Ok(opened)) => opened,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(DurableOpenError::StorageUnavailable),
        };
        let actor = match DurableStateActorHandle::start(&task_context) {
            Ok(actor) => actor,
            Err(_) => {
                // The successful open job remains owner-retained until shutdown joins its raw
                // handle. Do not release the root lease while that work is still outstanding.
                task_context.shutdown().await;
                opened.lease.release();
                return Err(DurableOpenError::StorageUnavailable);
            }
        };

        Ok(Self {
            task_context,
            actor,
            lease: opened.lease,
            agents: opened.agents,
            sessions: opened.sessions,
        })
    }

    /// Synchronously asks the actor to stop without releasing the root lease.
    pub(crate) fn request_closing(&self) {
        self.actor.request_closing();
    }

    /// Requests actor closure, joins the actor and all owner-retained work, then releases the
    /// root lease. The actor settlement inspection also covers abort-before-first-poll.
    pub(crate) async fn close(&self) {
        self.request_closing();
        self.task_context.shutdown().await;
        let _ = self.actor.wait().await;
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

    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later runtime read paths"
    )]
    pub(crate) fn session_head(&self, session_id: SessionId) -> Option<Arc<DurableSessionHead>> {
        self.sessions
            .get(&session_id)
            .map(|entry| Arc::clone(&entry.current_head))
    }

    #[allow(
        dead_code,
        reason = "recovery catalog is consumed by later Session definition resolution"
    )]
    pub(crate) fn session_current_definition(
        &self,
        session_id: SessionId,
    ) -> Option<Arc<SessionDefinition>> {
        self.sessions
            .get(&session_id)
            .map(|entry| Arc::clone(&entry.current_definition))
    }
}

struct RootLease {
    file: Mutex<Option<File>>,
}

#[derive(Clone)]
struct OpenRoot {
    lease: Arc<RootLease>,
    agents: Arc<BTreeMap<AgentId, DurableAgentCatalogEntry>>,
    sessions: Arc<BTreeMap<SessionId, DurableSessionCatalogEntry>>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectorySync {
    Supported,
    Unsupported,
}

fn open_root(root: PathBuf) -> Result<OpenRoot, DurableOpenError> {
    let filesystem = LocalFilesystem;
    open_root_with_cleanup_filesystem(root, &filesystem)
}

fn open_root_with_cleanup_filesystem(
    root: PathBuf,
    filesystem: &dyn CleanupFilesystem,
) -> Result<OpenRoot, DurableOpenError> {
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

    let catalog = if marker_present {
        let RecoveryCandidate { catalog, cleanup } = recover_marked_root(&root, &root_entries)?;
        // No read iterator, DirEntry, or document File may outlive recovery into a destructive
        // step. The cleanup plan above owns only paths and boolean shape facts.
        drop(root_entries);
        if let Some(cleanup) = cleanup {
            finalize_deferred_cleanup(&cleanup, directory_sync, filesystem)?;
        }
        catalog
    } else {
        validate_markerless_root(&root_entries)?;
        drop(root_entries);
        complete_markerless_scaffold(&root, directory_sync)?;
        create_format_marker(&root, directory_sync)?;
        RecoveredCatalog {
            agents: BTreeMap::new(),
            sessions: BTreeMap::new(),
        }
    };

    Ok(OpenRoot {
        lease: Arc::new(RootLease::new(lock_file)),
        agents: Arc::new(catalog.agents),
        sessions: Arc::new(catalog.sessions),
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
    validate_zero_regular_file(&marker).map(|_| ())
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

/// A deliberately narrow physical-scan error accumulator. Recovery still retains the first
/// non-size physical failure in traversal order, but a size/cap observation anywhere in the
/// entered Agent/Session physical scan dominates it after every reachable entity scan finishes.
#[derive(Default)]
struct PhysicalScanErrors {
    first: Option<DurableOpenError>,
    too_large: bool,
}

impl PhysicalScanErrors {
    fn record<T>(&mut self, result: Result<T, DurableOpenError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(DurableOpenError::DurableStateTooLarge) => {
                self.too_large = true;
                None
            }
            Err(error) => {
                if self.first.is_none() {
                    self.first = Some(error);
                }
                None
            }
        }
    }

    fn finish(self) -> Result<(), DurableOpenError> {
        if self.too_large {
            Err(DurableOpenError::DurableStateTooLarge)
        } else if let Some(error) = self.first {
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn recover_marked_root(
    root: &Path,
    entries: &[DirEntry],
) -> Result<RecoveryCandidate, DurableOpenError> {
    recover_marked_root_with_caps(root, entries, RecoveryCaps::PRODUCTION)
}

fn recover_marked_root_with_caps(
    root: &Path,
    entries: &[DirEntry],
    caps: RecoveryCaps,
) -> Result<RecoveryCandidate, DurableOpenError> {
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

    let mut physical_errors = PhysicalScanErrors::default();
    let (agent_reservations, session_reservations) = recover_marked_reservations(
        &root.join(RESERVATIONS_DIRECTORY),
        caps,
        &mut physical_errors,
    );
    let agent_entities = scan_agent_entities(
        &root.join(AGENTS_DIRECTORY),
        &agent_reservations,
        caps.root_agents,
        &mut physical_errors,
    );
    let session_entities = scan_session_entities(
        &root.join(SESSIONS_DIRECTORY),
        &session_reservations,
        caps.root_sessions,
        &mut physical_errors,
    );
    // Fully scan both entity namespaces, including every generation/payload cap, before
    // interpreting any candidate semantics or deciding that there is more than one cleanup
    // slot. A later TooLarge observation must not be masked by an earlier second candidate.
    let mut agent_scans = BTreeMap::new();
    for (agent_id, entity_path) in agent_entities {
        if let Some(scan) =
            physical_errors.record(scan_agent_entity(&entity_path, caps.generations))
        {
            agent_scans.insert(agent_id, scan);
        }
    }
    let mut session_scans = BTreeMap::new();
    for (session_id, entity_path) in session_entities {
        if let Some(scan) =
            physical_errors.record(scan_session_entity(&entity_path, caps.generations))
        {
            session_scans.insert(session_id, scan);
        }
    }
    physical_errors.finish()?;
    if physical_recovery_candidate_count(&agent_scans, &session_scans) > 1 {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    let mut cleanup = None;
    let agents = recover_agents(
        agent_scans,
        root,
        &mut cleanup,
        &agent_reservations,
        caps.generations,
    )?;
    let sessions = recover_sessions(
        session_scans,
        &agents,
        root,
        &mut cleanup,
        &session_reservations,
        caps.generations,
    )?;
    Ok(RecoveryCandidate {
        catalog: RecoveredCatalog { agents, sessions },
        cleanup,
    })
}

fn recover_marked_reservations(
    path: &Path,
    caps: RecoveryCaps,
    errors: &mut PhysicalScanErrors,
) -> (
    BTreeMap<AgentId, CleanupNodeIdentityShape>,
    BTreeMap<SessionId, CleanupNodeIdentityShape>,
) {
    let entries = errors.record(read_entries_bounded(path, RESERVATIONS_ENTRY_CAP));
    let mut has_agents = false;
    let mut has_sessions = false;
    for entry in entries.as_deref().unwrap_or_default() {
        if entry_has_name(entry, AGENTS_DIRECTORY) {
            errors.record(validate_directory_entry(
                entry,
                DurableOpenError::DurableStateCorrupt,
            ));
            has_agents = true;
        } else if entry_has_name(entry, SESSIONS_DIRECTORY) {
            errors.record(validate_directory_entry(
                entry,
                DurableOpenError::DurableStateCorrupt,
            ));
            has_sessions = true;
        } else {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
        }
    }
    if !(has_agents && has_sessions) {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }

    let agents = scan_agent_reservations(
        &path.join(AGENTS_DIRECTORY),
        caps.agent_reservations,
        errors,
    );
    let sessions = scan_session_reservations(
        &path.join(SESSIONS_DIRECTORY),
        caps.session_reservations,
        errors,
    );
    (agents, sessions)
}

fn scan_agent_reservations(
    path: &Path,
    maximum: usize,
    errors: &mut PhysicalScanErrors,
) -> BTreeMap<AgentId, CleanupNodeIdentityShape> {
    let Some(metadata) = errors.record(metadata_without_following(path)) else {
        return BTreeMap::new();
    };
    errors.record(validate_existing_directory(
        &metadata,
        DurableOpenError::DurableStateCorrupt,
    ));
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return BTreeMap::new();
    }
    let Some(entries) = errors.record(read_entries_bounded(path, maximum)) else {
        return BTreeMap::new();
    };
    let mut reservations = BTreeMap::new();
    for entry in entries {
        let Some(agent_id) = errors.record(parse_agent_id_name(&entry.file_name())) else {
            continue;
        };
        if reservations.contains_key(&agent_id) {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
            continue;
        }
        if let Some(shape) = errors.record(validate_zero_regular_file(&entry.path())) {
            reservations.insert(agent_id, shape);
        }
    }
    reservations
}

fn scan_session_reservations(
    path: &Path,
    maximum: usize,
    errors: &mut PhysicalScanErrors,
) -> BTreeMap<SessionId, CleanupNodeIdentityShape> {
    let Some(metadata) = errors.record(metadata_without_following(path)) else {
        return BTreeMap::new();
    };
    errors.record(validate_existing_directory(
        &metadata,
        DurableOpenError::DurableStateCorrupt,
    ));
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return BTreeMap::new();
    }
    let Some(entries) = errors.record(read_entries_bounded(path, maximum)) else {
        return BTreeMap::new();
    };
    let mut reservations = BTreeMap::new();
    for entry in entries {
        let Some(session_id) = errors.record(parse_session_id_name(&entry.file_name())) else {
            continue;
        };
        if reservations.contains_key(&session_id) {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
            continue;
        }
        if let Some(shape) = errors.record(validate_zero_regular_file(&entry.path())) {
            reservations.insert(session_id, shape);
        }
    }
    reservations
}

fn scan_agent_entities(
    path: &Path,
    reservations: &BTreeMap<AgentId, CleanupNodeIdentityShape>,
    maximum: usize,
    errors: &mut PhysicalScanErrors,
) -> BTreeMap<AgentId, PathBuf> {
    let Some(metadata) = errors.record(metadata_without_following(path)) else {
        return BTreeMap::new();
    };
    errors.record(validate_existing_directory(
        &metadata,
        DurableOpenError::DurableStateCorrupt,
    ));
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return BTreeMap::new();
    }
    let Some(entries) = errors.record(read_entries_bounded(path, maximum)) else {
        return BTreeMap::new();
    };
    let mut entity_paths = BTreeMap::new();
    for entry in entries {
        let Some(agent_id) = errors.record(parse_agent_id_name(&entry.file_name())) else {
            continue;
        };
        if entity_paths.insert(agent_id, entry.path()).is_some() {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
        }
    }
    entity_paths.retain(|agent_id, entity_path| {
        let Some(metadata) = errors.record(metadata_without_following(entity_path)) else {
            return false;
        };
        errors.record(validate_existing_directory(
            &metadata,
            DurableOpenError::DurableStateCorrupt,
        ));
        if !reservations.contains_key(agent_id) {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
        }
        !metadata.file_type().is_symlink() && metadata.is_dir()
    });
    entity_paths
}

fn scan_session_entities(
    path: &Path,
    reservations: &BTreeMap<SessionId, CleanupNodeIdentityShape>,
    maximum: usize,
    errors: &mut PhysicalScanErrors,
) -> BTreeMap<SessionId, PathBuf> {
    let Some(metadata) = errors.record(metadata_without_following(path)) else {
        return BTreeMap::new();
    };
    errors.record(validate_existing_directory(
        &metadata,
        DurableOpenError::DurableStateCorrupt,
    ));
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return BTreeMap::new();
    }
    let Some(entries) = errors.record(read_entries_bounded(path, maximum)) else {
        return BTreeMap::new();
    };
    let mut entity_paths = BTreeMap::new();
    for entry in entries {
        let Some(session_id) = errors.record(parse_session_id_name(&entry.file_name())) else {
            continue;
        };
        if entity_paths.insert(session_id, entry.path()).is_some() {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
        }
    }

    entity_paths.retain(|session_id, entity_path| {
        let Some(metadata) = errors.record(metadata_without_following(entity_path)) else {
            return false;
        };
        errors.record(validate_existing_directory(
            &metadata,
            DurableOpenError::DurableStateCorrupt,
        ));
        if !reservations.contains_key(session_id) {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
        }
        !metadata.file_type().is_symlink() && metadata.is_dir()
    });
    entity_paths
}

fn physical_recovery_candidate_count(
    agents: &BTreeMap<AgentId, ScannedAgentEntity>,
    sessions: &BTreeMap<SessionId, ScannedSessionEntity>,
) -> usize {
    let agents = agents
        .values()
        .filter(|entity| {
            matches!(entity, ScannedAgentEntity::Unpublished(_))
                || matches!(entity, ScannedAgentEntity::Published(scan) if scan.markerless.is_some())
        })
        .count();
    let sessions = sessions
        .values()
        .filter(|entity| {
            matches!(entity, ScannedSessionEntity::Unpublished(_))
                || matches!(entity, ScannedSessionEntity::Published(scan) if scan.markerless.is_some())
        })
        .count();
    agents + sessions
}

fn scan_session_entity(
    path: &Path,
    generation_maximum: usize,
) -> Result<ScannedSessionEntity, DurableOpenError> {
    let mut entries = read_entries_bounded(path, SESSION_ENTITY_ENTRY_CAP)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut published_path = None;
    let mut conversation_path = None;
    let mut generations = None;
    let mut unknown_child = false;
    for entry in entries {
        if entry_has_name(&entry, "PUBLISHED") {
            if published_path.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else if entry_has_name(&entry, "conversation.jsonl") {
            // This is intentionally a physical-only check. Header and entry bytes remain owned
            // by Conversation Storage and must not make whole-store recovery read or rewrite it.
            if conversation_path.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else if entry_has_name(&entry, "generations") {
            if generations.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else {
            unknown_child = true;
        }
    }
    let mut errors = PhysicalScanErrors::default();
    let published = published_path.is_some();
    let generations_are_enterable = generations.as_deref().is_some_and(|generations| {
        errors
            .record(metadata_without_following(generations))
            .is_some_and(|metadata| !metadata.file_type().is_symlink() && metadata.is_dir())
    });
    let published_scan = if published {
        generations
            .as_deref()
            .filter(|_| generations_are_enterable)
            .and_then(|generations| {
                errors.record(scan_physical_generation_chain(
                    generations,
                    generation_maximum,
                ))
            })
    } else {
        None
    };
    let unpublished_scan = if published {
        None
    } else {
        errors.record(scan_unpublished_entity(
            path,
            generations
                .as_ref()
                .filter(|_| generations_are_enterable)
                .cloned(),
            conversation_path.clone(),
            true,
            generation_maximum,
        ))
    };

    if let Some(published_path) = published_path {
        errors.record(validate_zero_regular_file(&published_path));
    }
    if let Some(generations) = &generations {
        if let Some(metadata) = errors.record(metadata_without_following(generations)) {
            errors.record(validate_existing_directory(
                &metadata,
                DurableOpenError::DurableStateCorrupt,
            ));
        }
    }
    if let Some(conversation_path) = &conversation_path {
        errors.record(validate_regular_entry_path(
            conversation_path,
            DurableOpenError::DurableStateCorrupt,
        ));
    }
    if unknown_child || (published && (generations.is_none() || conversation_path.is_none())) {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    errors.finish()?;
    match (published, published_scan, unpublished_scan) {
        (true, Some(scan), _) => Ok(ScannedSessionEntity::Published(scan)),
        (false, _, Some(files)) => Ok(ScannedSessionEntity::Unpublished(files)),
        _ => Err(DurableOpenError::DurableStateCorrupt),
    }
}

fn recover_agents(
    entities: BTreeMap<AgentId, ScannedAgentEntity>,
    root: &Path,
    cleanup: &mut Option<DeferredCleanup>,
    reservations: &BTreeMap<AgentId, CleanupNodeIdentityShape>,
    generation_collection_cap: usize,
) -> Result<BTreeMap<AgentId, DurableAgentCatalogEntry>, DurableOpenError> {
    let mut agents = BTreeMap::new();
    for (agent_id, entity) in entities {
        match entity {
            ScannedAgentEntity::Published(scan) => {
                let RecoveredAgentGenerationChain { entry, markerless } =
                    recover_agent_generation_chain(scan, agent_id)?;
                if let Some(markerless) = markerless {
                    register_deferred_cleanup(
                        cleanup,
                        DeferredCleanup::TrailingGeneration(deferred_agent_generation_cleanup(
                            root, agent_id, markerless,
                        )),
                    )?;
                }
                if agents.insert(agent_id, entry).is_some() {
                    return Err(DurableOpenError::DurableStateCorrupt);
                }
            }
            ScannedAgentEntity::Unpublished(files) => {
                validate_unpublished_agent_candidate(agent_id, &files)?;
                let reservation_shape = reservations
                    .get(&agent_id)
                    .cloned()
                    .ok_or(DurableOpenError::DurableStateCorrupt)?;
                register_deferred_cleanup(
                    cleanup,
                    DeferredCleanup::UnpublishedEntity(Box::new(
                        deferred_unpublished_agent_cleanup(
                            root,
                            agent_id,
                            files,
                            generation_collection_cap,
                            reservation_shape,
                        ),
                    )),
                )?;
            }
        }
    }
    Ok(agents)
}

/// Recovers committed Session chains. Agent recovery is deliberately complete before this
/// function runs so every Session pin can be checked against the retained exact Agent definition
/// index rather than an Agent's mutable current definition or status.
fn recover_sessions(
    entities: BTreeMap<SessionId, ScannedSessionEntity>,
    agents: &BTreeMap<AgentId, DurableAgentCatalogEntry>,
    root: &Path,
    cleanup: &mut Option<DeferredCleanup>,
    reservations: &BTreeMap<SessionId, CleanupNodeIdentityShape>,
    generation_collection_cap: usize,
) -> Result<BTreeMap<SessionId, DurableSessionCatalogEntry>, DurableOpenError> {
    let mut sessions = BTreeMap::new();
    let mut unpublished = Vec::new();
    for (session_id, entity) in entities {
        match entity {
            ScannedSessionEntity::Published(scan) => {
                let RecoveredSessionGenerationChain { entry, markerless } =
                    recover_session_generation_chain(scan, session_id, agents)?;
                if let Some(markerless) = markerless {
                    register_deferred_cleanup(
                        cleanup,
                        DeferredCleanup::TrailingGeneration(deferred_session_generation_cleanup(
                            root, session_id, markerless,
                        )),
                    )?;
                }
                if sessions.insert(session_id, entry).is_some() {
                    return Err(DurableOpenError::DurableStateCorrupt);
                }
            }
            ScannedSessionEntity::Unpublished(files) => unpublished.push((session_id, files)),
        }
    }
    validate_fork_provenance_references_and_cycles(&sessions)?;
    for (session_id, files) in unpublished {
        validate_unpublished_session_candidate(session_id, &files, agents, &sessions)?;
        let reservation_shape = reservations
            .get(&session_id)
            .cloned()
            .ok_or(DurableOpenError::DurableStateCorrupt)?;
        register_deferred_cleanup(
            cleanup,
            DeferredCleanup::UnpublishedEntity(Box::new(deferred_unpublished_session_cleanup(
                root,
                session_id,
                files,
                generation_collection_cap,
                reservation_shape,
            ))),
        )?;
    }
    Ok(sessions)
}

fn recover_session_generation_chain(
    scan: PhysicalGenerationScan,
    session_id: SessionId,
    agents: &BTreeMap<AgentId, DurableAgentCatalogEntry>,
) -> Result<RecoveredSessionGenerationChain, DurableOpenError> {
    let PhysicalGenerationScan {
        committed,
        markerless,
    } = scan;
    let mut generations = committed.into_iter();
    let Some(first_generation) = generations.next() else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };

    let (first_head, Some(first_definition)) =
        recover_session_generation_payload(&first_generation)?
    else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };
    validate_session_generation_one(
        session_id,
        first_generation.generation,
        &first_head,
        &first_definition,
        agents,
    )?;

    let mut current_head = first_head;
    let mut current_definition = first_definition;
    for generation in generations {
        if generation.generation.get()
            != current_head
                .storage_generation()
                .get()
                .checked_add(1)
                .ok_or(DurableOpenError::DurableStateCorrupt)?
        {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        let (head, definition) = recover_session_generation_payload(&generation)?;
        validate_session_generation_transition(
            session_id,
            generation.generation,
            &current_head,
            &current_definition,
            &head,
            definition.as_ref(),
            agents,
        )?;
        if let Some(definition) = definition {
            current_definition = definition;
        }
        current_head = head;
    }

    if markerless.is_some() && current_head.lifecycle() == SessionLifecycle::Deleted {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    Ok(RecoveredSessionGenerationChain {
        entry: DurableSessionCatalogEntry {
            current_head: Arc::new(current_head),
            current_definition: Arc::new(current_definition),
        },
        markerless,
    })
}

fn recover_session_generation_payload(
    files: &CommittedGenerationFiles,
) -> Result<(DurableSessionHead, Option<SessionDefinition>), DurableOpenError> {
    let definition = files
        .definition_path
        .as_deref()
        .map(decode_session_definition_document)
        .transpose()?;
    Ok((decode_session_head_document(&files.head_path)?, definition))
}

fn validate_session_generation_one(
    path_session_id: SessionId,
    path_generation: StorageGeneration,
    head: &DurableSessionHead,
    definition: &SessionDefinition,
    agents: &BTreeMap<AgentId, DurableAgentCatalogEntry>,
) -> Result<(), DurableOpenError> {
    let agent = definition.agent();
    let retained_agent_definition = agents
        .get(&agent.agent_id())
        .is_some_and(|entry| entry.definition_index.contains_key(&agent.revision()));
    if head.session_id() != path_session_id
        || definition.session_id() != path_session_id
        || head.storage_generation() != path_generation
        || head.storage_generation().get() != 1
        || head.previous_storage_generation().is_some()
        || head.current_definition_revision().get() != 1
        || head.current_definition_storage_generation() != path_generation
        || definition.revision().get() != 1
        || definition.workspace().revision().get() != 1
        || head.metadata().revision().get() != 1
        || head.lifecycle() != SessionLifecycle::Open
        || head.fork_provenance().is_some_and(|_| {
            head.metadata().name().is_some() || head.metadata().description().is_some()
        })
        || head.created_at() != definition.created_at()
        || head.created_at() != head.metadata().updated_at()
        || !retained_agent_definition
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(())
}

fn validate_session_generation_transition(
    path_session_id: SessionId,
    path_generation: StorageGeneration,
    previous_head: &DurableSessionHead,
    current_definition: &SessionDefinition,
    head: &DurableSessionHead,
    definition: Option<&SessionDefinition>,
    agents: &BTreeMap<AgentId, DurableAgentCatalogEntry>,
) -> Result<(), DurableOpenError> {
    if head.session_id() != path_session_id
        || head.storage_generation() != path_generation
        || head.previous_storage_generation() != Some(previous_head.storage_generation())
        || previous_head.lifecycle() == SessionLifecycle::Deleted
        || head.fork_provenance() != previous_head.fork_provenance()
        || head.created_at() != previous_head.created_at()
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    match definition {
        Some(definition) => validate_session_definition_transition(
            path_session_id,
            path_generation,
            previous_head,
            current_definition,
            head,
            definition,
            agents,
        ),
        None if session_metadata_transition_is_valid(previous_head, head) => Ok(()),
        None if session_lifecycle_transition_is_valid(previous_head, head) => Ok(()),
        None => Err(DurableOpenError::DurableStateCorrupt),
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ForkProvenanceVisit {
    Visiting,
    Done,
}

/// Validates published Fork sources and detects their bounded out-degree-one cycles without
/// inspecting source conversation bytes or current Session facts.
fn validate_fork_provenance_references_and_cycles(
    sessions: &BTreeMap<SessionId, DurableSessionCatalogEntry>,
) -> Result<(), DurableOpenError> {
    let mut visits = BTreeMap::new();

    for session_id in sessions.keys().copied() {
        if visits.get(&session_id) == Some(&ForkProvenanceVisit::Done) {
            continue;
        }

        let mut chain = Vec::new();
        let mut current = session_id;
        loop {
            match visits.get(&current) {
                Some(ForkProvenanceVisit::Visiting) => {
                    return Err(DurableOpenError::DurableStateCorrupt);
                }
                Some(ForkProvenanceVisit::Done) => break,
                None => {
                    visits.insert(current, ForkProvenanceVisit::Visiting);
                    chain.push(current);
                }
            }

            let entry = sessions
                .get(&current)
                .ok_or(DurableOpenError::DurableStateCorrupt)?;
            let Some(provenance) = entry.current_head.fork_provenance() else {
                break;
            };
            let source_session_id = provenance.source_session_id();
            if !sessions.contains_key(&source_session_id) {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            current = source_session_id;
        }

        for session_id in chain {
            visits.insert(session_id, ForkProvenanceVisit::Done);
        }
    }

    Ok(())
}

fn validate_session_definition_transition(
    path_session_id: SessionId,
    path_generation: StorageGeneration,
    previous_head: &DurableSessionHead,
    current_definition: &SessionDefinition,
    head: &DurableSessionHead,
    definition: &SessionDefinition,
    agents: &BTreeMap<AgentId, DurableAgentCatalogEntry>,
) -> Result<(), DurableOpenError> {
    let agent = definition.agent();
    let retained_agent_definition = agents
        .get(&agent.agent_id())
        .is_some_and(|entry| entry.definition_index.contains_key(&agent.revision()));
    if previous_head.lifecycle() != SessionLifecycle::Open
        || current_definition.session_id() != path_session_id
        || current_definition.revision() != previous_head.current_definition_revision()
        || definition.session_id() != path_session_id
        || definition.agent().agent_id() != current_definition.agent().agent_id()
        || !retained_agent_definition
        || !is_exact_next_session_definition_revision(
            previous_head.current_definition_revision(),
            head.current_definition_revision(),
        )
        || definition.revision() != head.current_definition_revision()
        || head.current_definition_storage_generation() != path_generation
        || !workspace_revision_transition_is_valid(
            current_definition.workspace(),
            definition.workspace(),
        )
        || session_definitions_have_same_canonical_execution_content(current_definition, definition)
        || head.metadata() != previous_head.metadata()
        || head.lifecycle() != previous_head.lifecycle()
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(())
}

fn session_metadata_transition_is_valid(
    previous_head: &DurableSessionHead,
    head: &DurableSessionHead,
) -> bool {
    head.current_definition_revision() == previous_head.current_definition_revision()
        && head.current_definition_storage_generation()
            == previous_head.current_definition_storage_generation()
        && is_exact_next_session_metadata_revision(
            previous_head.metadata().revision(),
            head.metadata().revision(),
        )
        && !session_metadata_has_same_canonical_content(previous_head.metadata(), head.metadata())
        && head.lifecycle() == previous_head.lifecycle()
}

fn session_lifecycle_transition_is_valid(
    previous_head: &DurableSessionHead,
    head: &DurableSessionHead,
) -> bool {
    head.current_definition_revision() == previous_head.current_definition_revision()
        && head.current_definition_storage_generation()
            == previous_head.current_definition_storage_generation()
        && head.metadata() == previous_head.metadata()
        && is_legal_session_lifecycle_transition(previous_head.lifecycle(), head.lifecycle())
}

fn is_exact_next_session_definition_revision(
    previous: SessionDefinitionRevision,
    next: SessionDefinitionRevision,
) -> bool {
    previous.get().checked_add(1) == Some(next.get())
}

fn is_exact_next_session_metadata_revision(
    previous: SessionMetadataRevision,
    next: SessionMetadataRevision,
) -> bool {
    previous.get().checked_add(1) == Some(next.get())
}

fn scan_agent_entity(
    path: &Path,
    generation_maximum: usize,
) -> Result<ScannedAgentEntity, DurableOpenError> {
    let mut entries = read_entries_bounded(path, AGENT_ENTITY_ENTRY_CAP)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut published_path = None;
    let mut generations = None;
    let mut unknown_child = false;
    for entry in entries {
        if entry_has_name(&entry, "PUBLISHED") {
            if published_path.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else if entry_has_name(&entry, "generations") {
            if generations.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else {
            unknown_child = true;
        }
    }
    let mut errors = PhysicalScanErrors::default();
    let published = published_path.is_some();
    let generations_are_enterable = generations.as_deref().is_some_and(|generations| {
        errors
            .record(metadata_without_following(generations))
            .is_some_and(|metadata| !metadata.file_type().is_symlink() && metadata.is_dir())
    });
    let published_scan = if published {
        generations
            .as_deref()
            .filter(|_| generations_are_enterable)
            .and_then(|generations| {
                errors.record(scan_physical_generation_chain(
                    generations,
                    generation_maximum,
                ))
            })
    } else {
        None
    };
    let unpublished_scan = if published {
        None
    } else {
        errors.record(scan_unpublished_entity(
            path,
            generations
                .as_ref()
                .filter(|_| generations_are_enterable)
                .cloned(),
            None,
            false,
            generation_maximum,
        ))
    };
    if let Some(published_path) = published_path {
        errors.record(validate_zero_regular_file(&published_path));
    }
    if let Some(generations) = &generations {
        if let Some(metadata) = errors.record(metadata_without_following(generations)) {
            errors.record(validate_existing_directory(
                &metadata,
                DurableOpenError::DurableStateCorrupt,
            ));
        }
    }
    if unknown_child || (published && generations.is_none()) {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    errors.finish()?;
    match (published, published_scan, unpublished_scan) {
        (true, Some(scan), _) => Ok(ScannedAgentEntity::Published(scan)),
        (false, _, Some(files)) => Ok(ScannedAgentEntity::Unpublished(files)),
        _ => Err(DurableOpenError::DurableStateCorrupt),
    }
}

fn scan_unpublished_entity(
    entity_path: &Path,
    generations_path: Option<PathBuf>,
    conversation_path: Option<PathBuf>,
    is_session: bool,
    generation_maximum: usize,
) -> Result<UnpublishedEntityFiles, DurableOpenError> {
    let entity_metadata = metadata_without_following(entity_path)?;
    let entity_shape = cleanup_node_identity_shape(&entity_metadata);
    // Keep the G1 and conversation observations in one deliberately narrow accumulator. In
    // particular, a corrupt G1/COMMITTED observation must not make us skip a reachable sparse
    // conversation's 1 GiB metadata bound: TooLarge wins after both physical scans complete.
    let mut errors = PhysicalScanErrors::default();
    let generations_shape = generations_path.as_deref().and_then(|path| {
        errors
            .record(metadata_without_following(path))
            .map(|metadata| cleanup_node_identity_shape(&metadata))
    });
    let generation = generations_path.as_deref().and_then(|path| {
        errors
            .record(scan_unpublished_generation(path, generation_maximum))
            .flatten()
    });
    let conversation_shape = conversation_path
        .as_deref()
        .and_then(|path| errors.record(validate_unpublished_conversation_physical_shape(path)));

    // These are relations between the two completed physical scans, rather than reasons to
    // short-circuit either one.
    if conversation_path.is_some() && !is_session {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    if conversation_path.is_some() && generation.is_none() {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    if is_session
        && generation
            .as_ref()
            .is_some_and(|generation| generation.has_committed)
        && conversation_path.is_none()
    {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    errors.finish()?;

    Ok(UnpublishedEntityFiles {
        entity_path: entity_path.to_owned(),
        entity_shape,
        generations_path,
        generations_shape,
        generation,
        conversation_path,
        conversation_shape,
    })
}

fn scan_unpublished_generation(
    generations_path: &Path,
    maximum: usize,
) -> Result<Option<UnpublishedGenerationFiles>, DurableOpenError> {
    let entries = read_entries_bounded(generations_path, maximum)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let mut errors = PhysicalScanErrors::default();
    let mut generation_one = None;
    for entry in &entries {
        let Some(generation) = errors.record(
            StorageGeneration::parse_directory_name(&entry.file_name())
                .map_err(|_| DurableOpenError::DurableStateCorrupt),
        ) else {
            continue;
        };
        let Some(metadata) = errors.record(metadata_without_following(&entry.path())) else {
            continue;
        };
        errors.record(validate_existing_directory(
            &metadata,
            DurableOpenError::DurableStateCorrupt,
        ));
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Some(payload) = errors.record(scan_unpublished_generation_payload(
            entry.path(),
            cleanup_node_identity_shape(&metadata),
        )) else {
            continue;
        };
        let invalid_generation = generation.get() != 1 || generation_one.replace(payload).is_some();
        if invalid_generation {
            errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
        }
    }
    if entries.len() != 1 || generation_one.is_none() {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    errors.finish()?;
    Ok(generation_one)
}

fn scan_unpublished_generation_payload(
    generation_path: PathBuf,
    generation_shape: CleanupNodeIdentityShape,
) -> Result<UnpublishedGenerationFiles, DurableOpenError> {
    let mut entries = read_entries_bounded(&generation_path, GENERATION_PAYLOAD_ENTRY_CAP)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut head_path = None;
    let mut definition_path = None;
    let mut committed_path = None;
    let mut unknown_child = false;
    for entry in entries {
        if entry_has_name(&entry, "head.json") {
            if head_path.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else if entry_has_name(&entry, "definition.json") {
            if definition_path.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else if entry_has_name(&entry, "COMMITTED") {
            if committed_path.replace(entry.path()).is_some() {
                unknown_child = true;
            }
        } else {
            unknown_child = true;
        }
    }
    let mut errors = PhysicalScanErrors::default();
    let head_shape = head_path
        .as_deref()
        .and_then(|path| errors.record(validate_generation_document_physical_shape(path)));
    let definition_shape = definition_path
        .as_deref()
        .and_then(|path| errors.record(validate_generation_document_physical_shape(path)));
    let committed_shape = committed_path
        .as_deref()
        .and_then(|path| errors.record(validate_zero_regular_file(path)));
    if unknown_child
        || (committed_path.is_some() && (head_path.is_none() || definition_path.is_none()))
    {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    errors.finish()?;
    Ok(UnpublishedGenerationFiles {
        generation_path,
        generation_shape,
        has_committed: committed_path.is_some(),
        committed_shape,
        has_head: head_path.is_some(),
        has_definition: definition_path.is_some(),
        head_shape,
        definition_shape,
    })
}

fn validate_unpublished_conversation_physical_shape(
    path: &Path,
) -> Result<CleanupRegularFileShape, DurableOpenError> {
    let metadata = metadata_without_following(path)?;
    validate_existing_regular_file(&metadata, DurableOpenError::DurableStateCorrupt)?;
    if metadata.len() > MAX_CONVERSATION_FILE_BYTES {
        return Err(DurableOpenError::DurableStateTooLarge);
    }
    Ok(cleanup_regular_file_shape(&metadata))
}

fn cleanup_regular_file_shape(metadata: &fs::Metadata) -> CleanupRegularFileShape {
    CleanupRegularFileShape {
        length: metadata.len(),
        node: cleanup_node_identity_shape(metadata),
    }
}

fn cleanup_node_identity_shape(metadata: &fs::Metadata) -> CleanupNodeIdentityShape {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        CleanupNodeIdentityShape {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        CleanupNodeIdentityShape {}
    }
}

/// Scans the shared physical generation layout before either domain's decode/fold pass. A
/// COMMITTED name is authoritative: once observed it must be a valid marker and a complete
/// committed payload; it can never be reclassified as removable staging.
fn scan_physical_generation_chain(
    path: &Path,
    maximum: usize,
) -> Result<PhysicalGenerationScan, DurableOpenError> {
    let entries = read_entries_bounded(path, maximum)?;
    let mut errors = PhysicalScanErrors::default();
    let mut generation_paths = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(generation) = errors.record(
            StorageGeneration::parse_directory_name(&entry.file_name())
                .map_err(|_| DurableOpenError::DurableStateCorrupt),
        ) {
            generation_paths.push((generation, entry.path()));
        }
    }
    generation_paths.sort_unstable_by_key(|(generation, _)| *generation);

    let mut payloads = Vec::with_capacity(generation_paths.len());
    for (generation, generation_path) in generation_paths {
        let Some(metadata) = errors.record(metadata_without_following(&generation_path)) else {
            continue;
        };
        errors.record(validate_existing_directory(
            &metadata,
            DurableOpenError::DurableStateCorrupt,
        ));
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if let Some(payload) = errors.record(scan_physical_generation_payload(
            generation,
            generation_path,
            path,
        )) {
            payloads.push(payload);
        }
    }

    let mut committed = Vec::with_capacity(payloads.len());
    let mut markerless = None;
    let mut highest_committed = None;
    for payload in payloads {
        match payload {
            PhysicalGenerationPayload::Committed(files) => {
                let expected = highest_committed
                    .map(|previous: StorageGeneration| {
                        previous
                            .get()
                            .checked_add(1)
                            .and_then(StorageGeneration::new)
                    })
                    .unwrap_or_else(|| StorageGeneration::new(1));
                if markerless.is_some() || expected != Some(files.generation) {
                    errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
                    continue;
                }
                highest_committed = Some(files.generation);
                committed.push(files);
            }
            PhysicalGenerationPayload::Markerless(files) => {
                let expected = highest_committed
                    .and_then(|previous| {
                        previous
                            .get()
                            .checked_add(1)
                            .and_then(StorageGeneration::new)
                    })
                    .or_else(|| {
                        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
                        None
                    });
                let Some(expected) = expected else {
                    continue;
                };
                if markerless.is_some() || files.generation != expected {
                    errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
                    continue;
                }
                markerless = Some(files);
            }
        }
    }

    if committed.is_empty() {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    errors.finish()?;
    Ok(PhysicalGenerationScan {
        committed,
        markerless,
    })
}

fn scan_physical_generation_payload(
    generation: StorageGeneration,
    generation_path: PathBuf,
    generations_parent: &Path,
) -> Result<PhysicalGenerationPayload, DurableOpenError> {
    let mut entries = read_entries_bounded(&generation_path, GENERATION_PAYLOAD_ENTRY_CAP)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut head_path = None;
    let mut definition_path = None;
    let mut committed_path = None;
    let mut unknown_child = false;

    for entry in entries {
        let slot = if entry_has_name(&entry, "head.json") {
            &mut head_path
        } else if entry_has_name(&entry, "definition.json") {
            &mut definition_path
        } else if entry_has_name(&entry, "COMMITTED") {
            &mut committed_path
        } else {
            unknown_child = true;
            continue;
        };
        if slot.replace(entry.path()).is_some() {
            unknown_child = true;
        }
    }

    let mut errors = PhysicalScanErrors::default();
    if let Some(head_path) = &head_path {
        errors.record(validate_generation_document_physical_shape(head_path));
    } else if committed_path.is_some() {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    if let Some(definition_path) = &definition_path {
        errors.record(validate_generation_document_physical_shape(definition_path));
    }
    if let Some(committed_path) = &committed_path {
        errors.record(validate_zero_regular_file(committed_path));
    }
    if unknown_child {
        errors.record::<()>(Err(DurableOpenError::DurableStateCorrupt));
    }
    errors.finish()?;

    if committed_path.is_some() {
        let head_path = head_path.expect("validated as present above");
        return Ok(PhysicalGenerationPayload::Committed(
            CommittedGenerationFiles {
                generation,
                head_path,
                definition_path,
            },
        ));
    }

    Ok(PhysicalGenerationPayload::Markerless(
        MarkerlessGenerationFiles {
            generation,
            generation_path,
            generations_parent: generations_parent.to_owned(),
            has_head: head_path.is_some(),
            has_definition: definition_path.is_some(),
        },
    ))
}

fn validate_generation_document_physical_shape(
    path: &Path,
) -> Result<CleanupRegularFileShape, DurableOpenError> {
    let metadata = metadata_without_following(path)?;
    validate_existing_regular_file(&metadata, DurableOpenError::DurableStateCorrupt)?;
    if metadata.len() > MAX_DURABLE_DOCUMENT_BYTES as u64 {
        return Err(DurableOpenError::DurableStateTooLarge);
    }
    Ok(cleanup_regular_file_shape(&metadata))
}

fn deferred_agent_generation_cleanup(
    root: &Path,
    agent_id: AgentId,
    markerless: MarkerlessGenerationFiles,
) -> DeferredGenerationCleanup {
    DeferredGenerationCleanup {
        generation_path: markerless.generation_path,
        generations_parent: markerless.generations_parent,
        reservation_path: root
            .join(RESERVATIONS_DIRECTORY)
            .join(AGENTS_DIRECTORY)
            .join(agent_id.to_string()),
        published_path: root
            .join(AGENTS_DIRECTORY)
            .join(agent_id.to_string())
            .join("PUBLISHED"),
        has_head: markerless.has_head,
        has_definition: markerless.has_definition,
    }
}

fn deferred_session_generation_cleanup(
    root: &Path,
    session_id: SessionId,
    markerless: MarkerlessGenerationFiles,
) -> DeferredGenerationCleanup {
    DeferredGenerationCleanup {
        generation_path: markerless.generation_path,
        generations_parent: markerless.generations_parent,
        reservation_path: root
            .join(RESERVATIONS_DIRECTORY)
            .join(SESSIONS_DIRECTORY)
            .join(session_id.to_string()),
        published_path: root
            .join(SESSIONS_DIRECTORY)
            .join(session_id.to_string())
            .join("PUBLISHED"),
        has_head: markerless.has_head,
        has_definition: markerless.has_definition,
    }
}

fn register_deferred_cleanup(
    cleanup: &mut Option<DeferredCleanup>,
    candidate: DeferredCleanup,
) -> Result<(), DurableOpenError> {
    if cleanup.is_some() {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    *cleanup = Some(candidate);
    Ok(())
}

fn deferred_unpublished_agent_cleanup(
    root: &Path,
    agent_id: AgentId,
    files: UnpublishedEntityFiles,
    generation_collection_cap: usize,
    reservation_shape: CleanupNodeIdentityShape,
) -> DeferredUnpublishedEntityCleanup {
    let reservation_path = root
        .join(RESERVATIONS_DIRECTORY)
        .join(AGENTS_DIRECTORY)
        .join(agent_id.to_string());
    deferred_unpublished_entity_cleanup(
        files,
        root.join(AGENTS_DIRECTORY),
        AGENT_ENTITY_ENTRY_CAP,
        generation_collection_cap,
        reservation_path,
        reservation_shape,
    )
}

fn deferred_unpublished_session_cleanup(
    root: &Path,
    session_id: SessionId,
    files: UnpublishedEntityFiles,
    generation_collection_cap: usize,
    reservation_shape: CleanupNodeIdentityShape,
) -> DeferredUnpublishedEntityCleanup {
    let reservation_path = root
        .join(RESERVATIONS_DIRECTORY)
        .join(SESSIONS_DIRECTORY)
        .join(session_id.to_string());
    deferred_unpublished_entity_cleanup(
        files,
        root.join(SESSIONS_DIRECTORY),
        SESSION_ENTITY_ENTRY_CAP,
        generation_collection_cap,
        reservation_path,
        reservation_shape,
    )
}

fn deferred_unpublished_entity_cleanup(
    files: UnpublishedEntityFiles,
    collection_parent: PathBuf,
    entity_entry_cap: usize,
    generation_collection_cap: usize,
    reservation_path: PathBuf,
    reservation_shape: CleanupNodeIdentityShape,
) -> DeferredUnpublishedEntityCleanup {
    let UnpublishedEntityFiles {
        entity_path,
        entity_shape,
        generations_path,
        generations_shape,
        generation,
        conversation_path,
        conversation_shape,
    } = files;
    let (
        generation_path,
        generation_shape,
        has_committed,
        committed_shape,
        has_head,
        has_definition,
        head_shape,
        definition_shape,
    ) = match generation {
        Some(generation) => (
            Some(generation.generation_path),
            Some(generation.generation_shape),
            generation.has_committed,
            generation.committed_shape,
            generation.has_head,
            generation.has_definition,
            generation.head_shape,
            generation.definition_shape,
        ),
        None => (None, None, false, None, false, false, None, None),
    };
    DeferredUnpublishedEntityCleanup {
        published_path: entity_path.join("PUBLISHED"),
        entity_path,
        entity_shape,
        collection_parent,
        entity_entry_cap,
        generation_collection_cap,
        reservation_path,
        reservation_shape,
        generations_path,
        generations_shape,
        generation_path,
        generation_shape,
        conversation_path,
        conversation_shape,
        has_committed,
        committed_shape,
        has_head,
        has_definition,
        head_shape,
        definition_shape,
    }
}

fn validate_unpublished_agent_candidate(
    agent_id: AgentId,
    files: &UnpublishedEntityFiles,
) -> Result<(), DurableOpenError> {
    let Some(generation) = &files.generation else {
        return Ok(());
    };
    if !generation.has_committed {
        return Ok(());
    }
    let head = decode_agent_head_document(&generation.generation_path.join("head.json"))?;
    let definition =
        decode_agent_definition_document(&generation.generation_path.join("definition.json"))?;
    validate_generation_one_agent_semantics(
        agent_id,
        StorageGeneration::new(1).ok_or(DurableOpenError::DurableStateCorrupt)?,
        &head,
        &definition,
    )
}

fn validate_unpublished_session_candidate(
    session_id: SessionId,
    files: &UnpublishedEntityFiles,
    agents: &BTreeMap<AgentId, DurableAgentCatalogEntry>,
    published_sessions: &BTreeMap<SessionId, DurableSessionCatalogEntry>,
) -> Result<(), DurableOpenError> {
    let Some(generation) = &files.generation else {
        return Ok(());
    };
    if !generation.has_committed {
        return Ok(());
    }

    let head = decode_session_head_document(&generation.generation_path.join("head.json"))?;
    let definition =
        decode_session_definition_document(&generation.generation_path.join("definition.json"))?;
    validate_session_generation_one(
        session_id,
        StorageGeneration::new(1).ok_or(DurableOpenError::DurableStateCorrupt)?,
        &head,
        &definition,
        agents,
    )?;
    let agent = agents
        .get(&definition.agent().agent_id())
        .ok_or(DurableOpenError::DurableStateCorrupt)?;
    if agent.current_head.status() != AgentStatus::Enabled
        || (head.fork_provenance().is_none()
            && (definition.agent().revision() != agent.current_head.current_definition_revision()
                || definition.agent().agent_id() != agent.current_head.agent_id()))
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    if head
        .fork_provenance()
        .is_some_and(|provenance| !published_sessions.contains_key(&provenance.source_session_id()))
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    let expected_header = SessionHeader::reconstruct(
        1,
        session_id,
        head.created_at(),
        definition.agent(),
        definition.revision(),
    );
    let expected_shape = if head.fork_provenance().is_some() {
        UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile
    } else {
        UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly
    };
    let conversation_path = files
        .conversation_path
        .as_deref()
        .ok_or(DurableOpenError::DurableStateCorrupt)?;
    let (mut file, declared_file_bytes, initial_path_metadata) =
        open_unpublished_conversation_for_recovery(conversation_path)?;
    let classification = validate_unpublished_conversation_for_recovery(
        &mut file,
        declared_file_bytes,
        &expected_header,
        expected_shape,
    );
    let final_handle_metadata = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    let final_path_metadata = metadata_without_following(conversation_path)?;
    validate_open_file_observation(
        &initial_path_metadata,
        &final_handle_metadata,
        &final_path_metadata,
    )?;
    classification.map_err(map_unpublished_conversation_recovery_error)
}

fn open_unpublished_conversation_for_recovery(
    path: &Path,
) -> Result<(File, u64, fs::Metadata), DurableOpenError> {
    let initial_path_metadata = metadata_without_following(path)?;
    validate_existing_regular_file(&initial_path_metadata, DurableOpenError::StorageUnavailable)?;
    if initial_path_metadata.len() > MAX_CONVERSATION_FILE_BYTES {
        return Err(DurableOpenError::DurableStateTooLarge);
    }
    let file = File::open(path).map_err(|_| DurableOpenError::StorageUnavailable)?;
    let handle_metadata = file
        .metadata()
        .map_err(|_| DurableOpenError::StorageUnavailable)?;
    let opened_path_metadata = metadata_without_following(path)?;
    validate_open_file_observation(
        &initial_path_metadata,
        &handle_metadata,
        &opened_path_metadata,
    )?;
    Ok((file, initial_path_metadata.len(), initial_path_metadata))
}

fn map_unpublished_conversation_recovery_error(
    error: UnpublishedConversationRecoveryError,
) -> DurableOpenError {
    match error {
        UnpublishedConversationRecoveryError::TooLarge => DurableOpenError::DurableStateTooLarge,
        UnpublishedConversationRecoveryError::Corrupt => DurableOpenError::DurableStateCorrupt,
        UnpublishedConversationRecoveryError::Unavailable => DurableOpenError::StorageUnavailable,
    }
}

/// Recovers the committed Agent prefix while retaining one exact trailing markerless staging
/// candidate for deferred cleanup after complete-store validation.
fn recover_agent_generation_chain(
    scan: PhysicalGenerationScan,
    agent_id: AgentId,
) -> Result<RecoveredAgentGenerationChain, DurableOpenError> {
    let PhysicalGenerationScan {
        committed,
        markerless,
    } = scan;
    let mut generations = committed.into_iter();
    let Some(first_generation) = generations.next() else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };

    let (first_head, Some(first_definition)) = recover_agent_generation_payload(&first_generation)?
    else {
        return Err(DurableOpenError::DurableStateCorrupt);
    };
    validate_generation_one_agent_semantics(
        agent_id,
        first_generation.generation,
        &first_head,
        &first_definition,
    )?;

    let mut current_head = first_head;
    let mut current_definition = first_definition;
    let mut definition_index = BTreeMap::new();
    if definition_index
        .insert(current_definition.revision(), first_generation.generation)
        .is_some()
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    for generation in generations {
        if generation.generation.get()
            != current_head
                .storage_generation()
                .get()
                .checked_add(1)
                .ok_or(DurableOpenError::DurableStateCorrupt)?
        {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        let (head, definition) = recover_agent_generation_payload(&generation)?;
        validate_agent_generation_transition(
            agent_id,
            generation.generation,
            &current_head,
            &current_definition,
            &head,
            definition.as_ref(),
        )?;
        if let Some(definition) = definition {
            if definition_index
                .insert(definition.revision(), generation.generation)
                .is_some()
            {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            current_definition = definition;
        }
        current_head = head;
    }

    if markerless.is_some() && current_head.status() == AgentStatus::Deleted {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    Ok(RecoveredAgentGenerationChain {
        entry: DurableAgentCatalogEntry {
            current_head: Arc::new(current_head),
            current_definition: Arc::new(current_definition),
            definition_index,
        },
        markerless,
    })
}

fn recover_agent_generation_payload(
    files: &CommittedGenerationFiles,
) -> Result<(DurableAgentHead, Option<AgentDefinition>), DurableOpenError> {
    let head = decode_agent_head_document(&files.head_path)?;
    let definition = files
        .definition_path
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

fn parse_session_id_name(name: &OsStr) -> Result<SessionId, DurableOpenError> {
    let value = name.to_str().ok_or(DurableOpenError::DurableStateCorrupt)?;
    let session_id =
        SessionId::from_str(value).map_err(|_| DurableOpenError::DurableStateCorrupt)?;
    if session_id.to_string() != value {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(session_id)
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
    validate_regular_entry_path(&entry.path(), error)
}

fn validate_regular_entry_path(
    path: &Path,
    error: DurableOpenError,
) -> Result<(), DurableOpenError> {
    let metadata = metadata_without_following(path)?;
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

fn decode_session_head_document(path: &Path) -> Result<DurableSessionHead, DurableOpenError> {
    let bytes = read_durable_document(path)?;
    DurableStoreV1Codec::decode_session_head(&bytes).map_err(map_durable_document_codec_error)
}

fn decode_session_definition_document(path: &Path) -> Result<SessionDefinition, DurableOpenError> {
    let bytes = read_durable_document(path)?;
    DurableStoreV1Codec::decode_session_definition(&bytes).map_err(map_durable_document_codec_error)
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

fn validate_zero_regular_file(path: &Path) -> Result<CleanupNodeIdentityShape, DurableOpenError> {
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
    )?;
    Ok(cleanup_node_identity_shape(&final_path_metadata))
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

/// The intentionally small private destructive-I/O seam. Store scanning always uses the local
/// standard-library read path; only deferred cleanup needs injectable remove/sync behavior.
trait CleanupFilesystem {
    fn remove_file(&self, path: &Path) -> Result<(), ()>;
    fn remove_dir(&self, path: &Path) -> Result<(), ()>;
    fn sync_directory(&self, path: &Path, directory_sync: DirectorySync) -> Result<(), ()>;
}

struct LocalFilesystem;

impl CleanupFilesystem for LocalFilesystem {
    fn remove_file(&self, path: &Path) -> Result<(), ()> {
        fs::remove_file(path).map_err(|_| ())
    }

    fn remove_dir(&self, path: &Path) -> Result<(), ()> {
        fs::remove_dir(path).map_err(|_| ())
    }

    fn sync_directory(&self, path: &Path, directory_sync: DirectorySync) -> Result<(), ()> {
        sync_directory(path, directory_sync).map_err(|_| ())
    }
}

#[derive(Clone, Copy)]
struct CleanupPayloadSubset {
    has_head: bool,
    has_definition: bool,
}

/// Revalidates every still-destructive precondition after all namespace and semantic recovery
/// handles have ended. A changed marker, child, or payload subset is corruption, never an excuse
/// to remove the candidate recursively or to fall back to the old catalog.
fn revalidate_deferred_generation_cleanup(
    cleanup: &DeferredGenerationCleanup,
) -> Result<CleanupPayloadSubset, DurableOpenError> {
    validate_cleanup_zero_regular_file(&cleanup.reservation_path)?;
    validate_cleanup_zero_regular_file(&cleanup.published_path)?;

    let parent_metadata = cleanup_metadata_without_following(&cleanup.generations_parent)?;
    validate_existing_directory(&parent_metadata, DurableOpenError::DurableStateCorrupt)?;
    let generation_metadata = cleanup_metadata_without_following(&cleanup.generation_path)?;
    validate_existing_directory(&generation_metadata, DurableOpenError::DurableStateCorrupt)?;

    let mut entries =
        read_cleanup_entries_bounded(&cleanup.generation_path, GENERATION_PAYLOAD_ENTRY_CAP)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut has_head = false;
    let mut has_definition = false;
    for entry in entries {
        if entry_has_name(&entry, "COMMITTED") {
            // The marker's name alone makes this a committed candidate. Never inspect it as a
            // fallback staging shape and never delete after seeing it.
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        let present = if entry_has_name(&entry, "head.json") {
            &mut has_head
        } else if entry_has_name(&entry, "definition.json") {
            &mut has_definition
        } else {
            return Err(DurableOpenError::DurableStateCorrupt);
        };
        if *present {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        validate_cleanup_generation_document(&entry.path())?;
        *present = true;
    }

    if has_head != cleanup.has_head || has_definition != cleanup.has_definition {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    Ok(CleanupPayloadSubset {
        has_head,
        has_definition,
    })
}

fn finalize_deferred_generation_cleanup(
    cleanup: &DeferredGenerationCleanup,
    directory_sync: DirectorySync,
    filesystem: &dyn CleanupFilesystem,
) -> Result<(), DurableOpenError> {
    // A partial prior cleanup is retried only after the next open scans anew and creates a new
    // plan. This plan never accepts a payload-shape change from its initial scan.
    let contents = revalidate_deferred_generation_cleanup(cleanup)?;
    if contents.has_definition {
        filesystem
            .remove_file(&cleanup.generation_path.join("definition.json"))
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    if contents.has_head {
        filesystem
            .remove_file(&cleanup.generation_path.join("head.json"))
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    if directory_sync == DirectorySync::Supported {
        filesystem
            .sync_directory(&cleanup.generation_path, directory_sync)
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    if filesystem.remove_dir(&cleanup.generation_path).is_err()
        && !cleanup_candidate_is_absent_after_remove_dir_error(&cleanup.generation_path)
    {
        return Err(DurableOpenError::StorageUnavailable);
    }
    if directory_sync == DirectorySync::Supported {
        filesystem
            .sync_directory(&cleanup.generations_parent, directory_sync)
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    Ok(())
}

fn finalize_deferred_cleanup(
    cleanup: &DeferredCleanup,
    directory_sync: DirectorySync,
    filesystem: &dyn CleanupFilesystem,
) -> Result<(), DurableOpenError> {
    match cleanup {
        DeferredCleanup::TrailingGeneration(cleanup) => {
            finalize_deferred_generation_cleanup(cleanup, directory_sync, filesystem)
        }
        DeferredCleanup::UnpublishedEntity(cleanup) => {
            finalize_deferred_unpublished_entity_cleanup(cleanup, directory_sync, filesystem)
        }
    }
}

/// Revalidates the whole exact invisible-entity shape before any deletion. It intentionally
/// accepts no drift: a later open can classify a known partial cleanup prefix afresh, but this
/// plan cannot treat a changed marker, child, type, mode, or payload as removable.
fn revalidate_deferred_unpublished_entity_cleanup(
    cleanup: &DeferredUnpublishedEntityCleanup,
) -> Result<(), DurableOpenError> {
    if validate_cleanup_zero_regular_file(&cleanup.reservation_path)? != cleanup.reservation_shape {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    validate_cleanup_absent(&cleanup.published_path)?;

    let entity_metadata = cleanup_metadata_without_following(&cleanup.entity_path)?;
    validate_existing_directory(&entity_metadata, DurableOpenError::DurableStateCorrupt)?;
    if cleanup_node_identity_shape(&entity_metadata) != cleanup.entity_shape {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    let mut entries = read_cleanup_entries_bounded(&cleanup.entity_path, cleanup.entity_entry_cap)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut generations_path = None;
    let mut generations_shape = None;
    let mut conversation_path = None;
    let mut conversation_shape = None;
    for entry in entries {
        if entry_has_name(&entry, "PUBLISHED") {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        if entry_has_name(&entry, "generations") {
            if generations_path.replace(entry.path()).is_some() {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            validate_directory_entry(&entry, DurableOpenError::DurableStateCorrupt)?;
            let metadata = cleanup_metadata_without_following(&entry.path())?;
            generations_shape = Some(cleanup_node_identity_shape(&metadata));
        } else if entry_has_name(&entry, "conversation.jsonl") {
            if conversation_path.replace(entry.path()).is_some() {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            conversation_shape = Some(validate_unpublished_conversation_physical_shape(
                &entry.path(),
            )?);
        } else {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    if generations_path.as_deref() != cleanup.generations_path.as_deref()
        || generations_shape != cleanup.generations_shape
        || conversation_path.as_deref() != cleanup.conversation_path.as_deref()
        || conversation_shape != cleanup.conversation_shape
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }

    let Some(generations_path) = cleanup.generations_path.as_deref() else {
        return Ok(());
    };
    let entries =
        read_cleanup_entries_bounded(generations_path, cleanup.generation_collection_cap)?;
    match cleanup.generation_path.as_deref() {
        None if entries.is_empty() => Ok(()),
        Some(expected_generation_path) if entries.len() == 1 => {
            let entry = &entries[0];
            if entry.path() != expected_generation_path
                || StorageGeneration::parse_directory_name(&entry.file_name())
                    .map_err(|_| DurableOpenError::DurableStateCorrupt)?
                    .get()
                    != 1
            {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            validate_directory_entry(entry, DurableOpenError::DurableStateCorrupt)?;
            let metadata = cleanup_metadata_without_following(&entry.path())?;
            if Some(cleanup_node_identity_shape(&metadata)) != cleanup.generation_shape {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            revalidate_unpublished_generation_payload(cleanup)
        }
        _ => Err(DurableOpenError::DurableStateCorrupt),
    }
}

fn revalidate_unpublished_generation_payload(
    cleanup: &DeferredUnpublishedEntityCleanup,
) -> Result<(), DurableOpenError> {
    let generation_path = cleanup
        .generation_path
        .as_deref()
        .ok_or(DurableOpenError::DurableStateCorrupt)?;
    let mut entries = read_cleanup_entries_bounded(generation_path, GENERATION_PAYLOAD_ENTRY_CAP)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    let mut has_committed = false;
    let mut has_head = false;
    let mut has_definition = false;
    let mut head_shape = None;
    let mut definition_shape = None;
    for entry in entries {
        if entry_has_name(&entry, "COMMITTED") {
            if has_committed {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            if Some(validate_cleanup_zero_regular_file(&entry.path())?) != cleanup.committed_shape {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            has_committed = true;
        } else if entry_has_name(&entry, "head.json") {
            if has_head {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            head_shape = Some(validate_cleanup_generation_document(&entry.path())?);
            has_head = true;
        } else if entry_has_name(&entry, "definition.json") {
            if has_definition {
                return Err(DurableOpenError::DurableStateCorrupt);
            }
            definition_shape = Some(validate_cleanup_generation_document(&entry.path())?);
            has_definition = true;
        } else {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
    }
    if has_committed != cleanup.has_committed
        || has_head != cleanup.has_head
        || has_definition != cleanup.has_definition
        || head_shape != cleanup.head_shape
        || definition_shape != cleanup.definition_shape
    {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(())
}

fn finalize_deferred_unpublished_entity_cleanup(
    cleanup: &DeferredUnpublishedEntityCleanup,
    directory_sync: DirectorySync,
    filesystem: &dyn CleanupFilesystem,
) -> Result<(), DurableOpenError> {
    revalidate_deferred_unpublished_entity_cleanup(cleanup)?;

    let generation_path = cleanup.generation_path.as_deref();
    if cleanup.has_committed {
        filesystem
            .remove_file(
                &generation_path
                    .ok_or(DurableOpenError::DurableStateCorrupt)?
                    .join("COMMITTED"),
            )
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    if cleanup.has_definition {
        filesystem
            .remove_file(
                &generation_path
                    .ok_or(DurableOpenError::DurableStateCorrupt)?
                    .join("definition.json"),
            )
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    if cleanup.has_head {
        filesystem
            .remove_file(
                &generation_path
                    .ok_or(DurableOpenError::DurableStateCorrupt)?
                    .join("head.json"),
            )
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    if let Some(conversation_path) = &cleanup.conversation_path {
        filesystem
            .remove_file(conversation_path)
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }

    if let Some(generation_path) = generation_path {
        sync_cleanup_directory(filesystem, generation_path, directory_sync)?;
        remove_cleanup_directory(filesystem, generation_path)?;
        sync_cleanup_directory(
            filesystem,
            cleanup
                .generations_path
                .as_deref()
                .ok_or(DurableOpenError::DurableStateCorrupt)?,
            directory_sync,
        )?;
    }
    if let Some(generations_path) = &cleanup.generations_path {
        if generation_path.is_none() {
            sync_cleanup_directory(filesystem, generations_path, directory_sync)?;
        }
        remove_cleanup_directory(filesystem, generations_path)?;
        sync_cleanup_directory(filesystem, &cleanup.entity_path, directory_sync)?;
    } else {
        sync_cleanup_directory(filesystem, &cleanup.entity_path, directory_sync)?;
    }
    remove_cleanup_directory(filesystem, &cleanup.entity_path)?;
    sync_cleanup_directory(filesystem, &cleanup.collection_parent, directory_sync)
}

fn remove_cleanup_directory(
    filesystem: &dyn CleanupFilesystem,
    path: &Path,
) -> Result<(), DurableOpenError> {
    if filesystem.remove_dir(path).is_err()
        && !cleanup_candidate_is_absent_after_remove_dir_error(path)
    {
        return Err(DurableOpenError::StorageUnavailable);
    }
    Ok(())
}

fn sync_cleanup_directory(
    filesystem: &dyn CleanupFilesystem,
    path: &Path,
    directory_sync: DirectorySync,
) -> Result<(), DurableOpenError> {
    if directory_sync == DirectorySync::Supported {
        filesystem
            .sync_directory(path, directory_sync)
            .map_err(|_| DurableOpenError::StorageUnavailable)?;
    }
    Ok(())
}

/// Reconciles only the ambiguous `remove_dir` result immediately after its error. Any existing
/// entry or unreadable observation remains unavailable; initial revalidation never accepts this
/// absence.
fn cleanup_candidate_is_absent_after_remove_dir_error(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

fn cleanup_metadata_without_following(path: &Path) -> Result<fs::Metadata, DurableOpenError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(DurableOpenError::DurableStateCorrupt)
        }
        Err(_) => Err(DurableOpenError::StorageUnavailable),
    }
}

fn validate_cleanup_absent(path: &Path) -> Result<(), DurableOpenError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(DurableOpenError::DurableStateCorrupt),
        Err(_) => Err(DurableOpenError::StorageUnavailable),
    }
}

fn validate_cleanup_zero_regular_file(
    path: &Path,
) -> Result<CleanupNodeIdentityShape, DurableOpenError> {
    let metadata = cleanup_metadata_without_following(path)?;
    validate_existing_regular_file(&metadata, DurableOpenError::DurableStateCorrupt)?;
    if metadata.len() != 0 {
        return Err(DurableOpenError::DurableStateCorrupt);
    }
    Ok(cleanup_node_identity_shape(&metadata))
}

fn validate_cleanup_generation_document(
    path: &Path,
) -> Result<CleanupRegularFileShape, DurableOpenError> {
    let metadata = cleanup_metadata_without_following(path)?;
    validate_existing_regular_file(&metadata, DurableOpenError::DurableStateCorrupt)?;
    if metadata.len() > MAX_DURABLE_DOCUMENT_BYTES as u64 {
        return Err(DurableOpenError::DurableStateTooLarge);
    }
    Ok(cleanup_regular_file_shape(&metadata))
}

fn read_cleanup_entries_bounded(
    path: &Path,
    maximum: usize,
) -> Result<Vec<DirEntry>, DurableOpenError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(DurableOpenError::DurableStateCorrupt);
        }
        Err(_) => return Err(DurableOpenError::StorageUnavailable),
    };
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
    use std::fs::{self, OpenOptions};
    use std::future::{Future, poll_fn};
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(unix)]
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::sync::Mutex;

    use tokio::runtime::{Builder, Handle};
    use tokio::sync::Notify;

    use super::{
        AGENTS_DIRECTORY, DurableOpenError, DurableSessionHead, DurableState, FORMAT_MARKER,
        GENERATION_PAYLOAD_ENTRY_CAP, LOCK_FILE, RESERVATIONS_DIRECTORY, RESERVATIONS_ENTRY_CAP,
        ROOT_ENTRY_CAP, RecoveryCaps, SESSIONS_DIRECTORY, StorageGeneration, parse_agent_id_name,
        read_durable_document, read_entries_bounded, recover_marked_root_with_caps,
    };
    #[cfg(unix)]
    use super::{
        CleanupFilesystem, DirectorySync, classify_directory_sync, open_root,
        open_root_with_cleanup_filesystem,
    };
    use crate::agent_session_lifecycle::{
        AgentStatus, ForkAnchor, ForkSourceKind, SessionForkProvenance, SessionLifecycle,
        SessionMetadata,
    };
    use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError};
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
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CleanupOperation {
        RemoveCommitted,
        RemoveDefinition,
        RemoveHead,
        RemoveConversation,
        SyncCandidateDirectory,
        RemoveCandidateDirectory,
        SyncGenerationsDirectory,
        RemoveGenerationsDirectory,
        SyncEntityDirectory,
        RemoveEntityDirectory,
        SyncCollectionDirectory,
    }

    #[cfg(unix)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CleanupFault {
        Before(CleanupOperation),
        After(CleanupOperation),
    }

    #[cfg(unix)]
    impl CleanupFault {
        const fn operation(self) -> CleanupOperation {
            match self {
                Self::Before(operation) | Self::After(operation) => operation,
            }
        }
    }

    /// A deliberately narrow persistent fault adapter: it performs the real cleanup operation
    /// against the same temporary root, then can report failure before or after that effect.
    #[cfg(unix)]
    struct DeterministicPersistentFaultFilesystem {
        faults: Mutex<VecDeque<CleanupFault>>,
        operations: Mutex<Vec<CleanupOperation>>,
    }

    #[cfg(unix)]
    impl DeterministicPersistentFaultFilesystem {
        fn new(faults: impl IntoIterator<Item = CleanupFault>) -> Self {
            Self {
                faults: Mutex::new(faults.into_iter().collect()),
                operations: Mutex::new(Vec::new()),
            }
        }

        fn operations(&self) -> Vec<CleanupOperation> {
            super::lock(&self.operations).clone()
        }

        fn perform(
            &self,
            operation: CleanupOperation,
            action: impl FnOnce() -> std::io::Result<()>,
        ) -> Result<(), ()> {
            super::lock(&self.operations).push(operation);
            let fault = {
                let mut faults = super::lock(&self.faults);
                match faults.front().copied() {
                    Some(fault @ CleanupFault::Before(expected))
                    | Some(fault @ CleanupFault::After(expected))
                        if expected == operation =>
                    {
                        faults.pop_front();
                        Some(fault)
                    }
                    _ => None,
                }
            };
            if matches!(fault, Some(CleanupFault::Before(_))) {
                return Err(());
            }
            action().map_err(|_| ())?;
            if matches!(fault, Some(CleanupFault::After(_))) {
                return Err(());
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    impl CleanupFilesystem for DeterministicPersistentFaultFilesystem {
        fn remove_file(&self, path: &Path) -> Result<(), ()> {
            let operation = match path.file_name().and_then(|name| name.to_str()) {
                Some("COMMITTED") => CleanupOperation::RemoveCommitted,
                Some("definition.json") => CleanupOperation::RemoveDefinition,
                Some("head.json") => CleanupOperation::RemoveHead,
                Some("conversation.jsonl") => CleanupOperation::RemoveConversation,
                _ => return Err(()),
            };
            self.perform(operation, || fs::remove_file(path))
        }

        fn remove_dir(&self, path: &Path) -> Result<(), ()> {
            let operation = match path.file_name().and_then(|name| name.to_str()) {
                Some("generations") => CleanupOperation::RemoveGenerationsDirectory,
                Some(name) if name.starts_with("agt_") || name.starts_with("ses_") => {
                    CleanupOperation::RemoveEntityDirectory
                }
                _ => CleanupOperation::RemoveCandidateDirectory,
            };
            self.perform(operation, || fs::remove_dir(path))
        }

        fn sync_directory(&self, path: &Path, _directory_sync: DirectorySync) -> Result<(), ()> {
            let operation = match path.file_name().and_then(|name| name.to_str()) {
                Some("generations") => CleanupOperation::SyncGenerationsDirectory,
                Some(name) if name.starts_with("agt_") || name.starts_with("ses_") => {
                    CleanupOperation::SyncEntityDirectory
                }
                Some(AGENTS_DIRECTORY) | Some(SESSIONS_DIRECTORY) => {
                    CleanupOperation::SyncCollectionDirectory
                }
                _ => CleanupOperation::SyncCandidateDirectory,
            };
            self.perform(operation, || {
                #[cfg(unix)]
                {
                    std::fs::File::open(path).and_then(|directory| directory.sync_all())
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    Ok(())
                }
            })
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

    async fn poll_once_pending<F>(mut future: Pin<&mut F>) -> bool
    where
        F: Future<Output = ()>,
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
    async fn durable_actor_starts_closes_and_releases_its_lease_on_a_current_thread_host() {
        let root = TempRoot::nonexistent();

        let state = open(root.path())
            .await
            .expect("the actor starts after successful root recovery");
        state.close().await;

        let reopened = open(root.path())
            .await
            .expect("actor shutdown precedes root-lease release");
        reopened.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_request_completes_its_envelope_before_shutdown() {
        let root = TempRoot::nonexistent();
        let state = open(root.path()).await.expect("the store and actor open");
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let waiter = state
            .actor
            .enqueue_probe(Arc::clone(&entered), Arc::clone(&release));

        entered.notified().await;
        release.notify_one();
        assert_eq!(waiter.wait().await, Ok(()));

        state.close().await;
    }

    #[test]
    fn durable_actor_starts_closes_and_releases_its_lease_on_a_multi_thread_host() {
        let root = TempRoot::nonexistent();
        let host = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .expect("the test host builds");

        host.block_on(async {
            let state = open(root.path())
                .await
                .expect("the actor starts after successful root recovery");
            state.close().await;

            let reopened = open(root.path())
                .await
                .expect("actor shutdown precedes root-lease release");
            reopened.close().await;
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn root_lease_is_held_until_a_blocked_actor_and_its_queued_request_settle() {
        let root = TempRoot::nonexistent();
        let state = open(root.path())
            .await
            .expect("the store and its actor open");
        let first_entered = Arc::new(Notify::new());
        let first_release = Arc::new(Notify::new());
        let first = state
            .actor
            .enqueue_probe(Arc::clone(&first_entered), Arc::clone(&first_release));
        first_entered.notified().await;

        let queued_entered = Arc::new(Notify::new());
        let queued_release = Arc::new(Notify::new());
        let queued = state
            .actor
            .enqueue_probe(Arc::clone(&queued_entered), Arc::clone(&queued_release));
        state.request_closing();
        // Both branches are now ready; the actor's biased close branch must win.
        first_release.notify_one();

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::StoreInUse)
        ));

        // The active probe observes Closing before its release barrier; the queued one is
        // explicitly completed as rejected rather than silently discarded.
        state.close().await;
        assert_eq!(first.wait().await, Err(super::ActorRequestError::Closing));
        assert_eq!(queued.wait().await, Err(super::ActorRequestError::Closing));

        let reopened = open(root.path())
            .await
            .expect("the lease releases only after the actor task settles");
        reopened.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closing_rejects_new_actor_admission_and_aborts_a_prebarrier_probe() {
        let root = TempRoot::nonexistent();
        let state = open(root.path()).await.expect("the store and actor open");
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let active = state
            .actor
            .enqueue_probe(Arc::clone(&entered), Arc::clone(&release));
        entered.notified().await;

        state.request_closing();
        let rejected = state
            .actor
            .enqueue_probe(Arc::new(Notify::new()), Arc::new(Notify::new()));
        assert_eq!(
            rejected.wait().await,
            Err(super::ActorRequestError::Closing)
        );

        state.close().await;
        assert_eq!(active.wait().await, Err(super::ActorRequestError::Closing));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aborting_the_actor_settles_its_active_and_queued_request_waiters() {
        let root = TempRoot::nonexistent();
        let state = open(root.path()).await.expect("the store and actor open");
        let actor_task = state.actor.task.clone();

        let active_entered = Arc::new(Notify::new());
        let active = state
            .actor
            .enqueue_probe(Arc::clone(&active_entered), Arc::new(Notify::new()));
        active_entered.notified().await;
        let queued = state
            .actor
            .enqueue_probe(Arc::new(Notify::new()), Arc::new(Notify::new()));

        state.task_context.abort_latest_registered_task();

        assert_eq!(
            active.wait().await,
            Err(super::ActorRequestError::Unavailable),
            "dropping the active actor local settles its envelope"
        );
        assert_eq!(
            queued.wait().await,
            Err(super::ActorRequestError::Unavailable),
            "dropping the receiver settles every queued envelope"
        );
        assert_eq!(
            actor_task.wait().await,
            Err(RuntimeTaskError::WorkerUnavailable)
        );

        state.close().await;
        let reopened = open(root.path())
            .await
            .expect("the aborted actor cannot retain the root lease");
        reopened.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_sender_race_across_receiver_close_always_settles_the_waiter() {
        let accepted_root = TempRoot::nonexistent();
        let accepted_state = open(accepted_root.path())
            .await
            .expect("the store and actor open");
        let (checked_sender, checked_receiver) = std::sync::mpsc::channel();
        let (resume_sender, resume_receiver) = std::sync::mpsc::channel();
        let actor = accepted_state.actor.clone();
        let sender = std::thread::spawn(move || {
            actor.enqueue_probe_after_fast_reject_and_sender_gate(
                Arc::new(Notify::new()),
                Arc::new(Notify::new()),
                checked_sender,
                resume_receiver,
            )
        });
        checked_receiver
            .recv()
            .expect("the sender passed its cancellation fast-path and sender gate");
        accepted_state.request_closing();

        let receiver_closed = accepted_state.actor.receiver_closed.notified();
        tokio::pin!(receiver_closed);
        receiver_closed.as_mut().enable();
        let mut close = Box::pin(accepted_state.close());
        assert!(poll_once_pending(close.as_mut()).await);
        receiver_closed.await;
        assert!(
            poll_once_pending(close.as_mut()).await,
            "close waits for the outstanding sender gate rather than stopping at Empty"
        );

        resume_sender
            .send(())
            .expect("the sender remains gated after receiver close");
        let (accepted_waiter, accepted_delivery) = sender
            .join()
            .expect("the deterministic sender helper does not panic");
        assert_eq!(
            accepted_delivery,
            super::DurableStateActorProbeDelivery::Accepted,
            "the pre-close sender gate publishes an accepted request"
        );
        close.await;
        assert_eq!(
            accepted_waiter.wait().await,
            Err(super::ActorRequestError::Closing),
            "an accepted request is drained with a typed close rejection"
        );

        let rejected_root = TempRoot::nonexistent();
        let rejected_state = open(rejected_root.path())
            .await
            .expect("the store and actor open");
        let (checked_sender, checked_receiver) = std::sync::mpsc::channel();
        let (resume_sender, resume_receiver) = std::sync::mpsc::channel();
        let actor = rejected_state.actor.clone();
        let sender = std::thread::spawn(move || {
            actor.enqueue_probe_after_fast_reject_gate(
                Arc::new(Notify::new()),
                Arc::new(Notify::new()),
                checked_sender,
                resume_receiver,
            )
        });
        checked_receiver
            .recv()
            .expect("the sender passed its cancellation fast-path");
        rejected_state.request_closing();
        rejected_state
            .actor
            .wait()
            .await
            .expect("the actor closes its receiver before the gated send resumes");
        resume_sender
            .send(())
            .expect("the sender remains gated until receiver close completes");
        let (rejected_waiter, rejected_delivery) = sender
            .join()
            .expect("the deterministic sender helper does not panic");
        assert_eq!(
            rejected_delivery,
            super::DurableStateActorProbeDelivery::Rejected,
            "the post-close try_send is rejected synchronously"
        );
        assert_eq!(
            rejected_waiter.wait().await,
            Err(super::ActorRequestError::Closing),
            "the returned failed-send envelope settles its waiter before dropping"
        );
        rejected_state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aborting_the_actor_before_its_first_poll_still_settles_close_and_releases_the_lease() {
        let root = TempRoot::nonexistent();
        let state = open(root.path()).await.expect("the store and actor open");
        let actor_task = state.actor.task.clone();
        state.task_context.abort_latest_registered_task();

        state.close().await;
        assert_eq!(
            actor_task.wait().await,
            Err(RuntimeTaskError::WorkerUnavailable)
        );

        let reopened = open(root.path())
            .await
            .expect("an aborted unpolled actor cannot retain the root lease");
        reopened.close().await;
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
    const SESSION_ONE: &str = "ses_22222222222222222222222222222222";
    const SESSION_TWO: &str = "ses_33333333333333333333333333333333";
    const SESSION_THREE: &str = "ses_44444444444444444444444444444444";
    const SESSION_BEFORE_SOURCE: &str = "ses_11111111111111111111111111111111";
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

    fn session_head_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/session-head.json")
    }

    #[cfg(windows)]
    fn current_host_session_fixture_uri() -> &'static str {
        "file:///C:/work/project"
    }

    fn session_definition_fixture_from(fixture: &[u8]) -> Vec<u8> {
        #[cfg(windows)]
        {
            return replace_fixture(
                fixture,
                "file:///Users/example/project",
                current_host_session_fixture_uri(),
            );
        }
        #[cfg(not(windows))]
        fixture.to_vec()
    }

    fn session_definition_fixture() -> Vec<u8> {
        session_definition_fixture_from(include_bytes!(
            "../docs/fixtures/durable-store-v1/session-definition.json"
        ))
    }

    fn session_head_definition_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-2-definition.json")
    }

    fn session_definition_g2_fixture() -> Vec<u8> {
        session_definition_fixture_from(include_bytes!(
            "../docs/fixtures/durable-store-v1/session-definition-2.json"
        ))
    }

    fn session_head_workspace_definition_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-2-workspace-definition.json")
    }

    fn session_definition_workspace_g2_fixture() -> Vec<u8> {
        session_definition_fixture_from(include_bytes!(
            "../docs/fixtures/durable-store-v1/session-definition-2-workspace.json"
        ))
    }

    fn session_head_metadata_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-2-metadata.json")
    }

    fn session_head_archived_g2_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-2-lifecycle.json")
    }

    fn session_head_unarchived_g3_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-3-unarchive.json")
    }

    fn session_head_deleted_g3_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-3-deleted.json")
    }

    fn fork_session_head_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/fork-session-head.json")
    }

    fn fork_session_definition_fixture() -> Vec<u8> {
        session_definition_fixture_from(include_bytes!(
            "../docs/fixtures/durable-store-v1/fork-session-definition.json"
        ))
    }

    fn genesis_fork_session_head_fixture() -> &'static [u8] {
        include_bytes!("../docs/fixtures/durable-store-v1/genesis-fork-session-head.json")
    }

    fn genesis_fork_session_definition_fixture() -> Vec<u8> {
        session_definition_fixture_from(include_bytes!(
            "../docs/fixtures/durable-store-v1/genesis-fork-session-definition.json"
        ))
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

    fn replace_all_fixture(input: &[u8], from: &str, to: &str) -> Vec<u8> {
        let input = std::str::from_utf8(input).expect("fixture bytes are UTF-8");
        assert!(input.contains(from), "fixture must contain {from:?}");
        input.replace(from, to).into_bytes()
    }

    fn valid_ordinary_conversation(session_id: &str) -> Vec<u8> {
        replace_all_fixture(
            include_bytes!("../docs/fixtures/durable-store-v1/fork-source.jsonl"),
            SESSION_ONE,
            session_id,
        )
    }

    fn valid_fork_conversation(session_id: &str) -> Vec<u8> {
        replace_all_fixture(
            include_bytes!("../docs/fixtures/durable-store-v1/fork-child.jsonl"),
            SESSION_TWO,
            session_id,
        )
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

    fn create_unpublished_agent(
        root: &Path,
        agent_id: &str,
        generations: bool,
        generation: bool,
        head: Option<&[u8]>,
        definition: Option<&[u8]>,
        committed: bool,
    ) -> PathBuf {
        create_file(
            &root
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(agent_id),
            b"",
        );
        let entity = agent_path(root, agent_id);
        create_directory(&entity);
        if generations {
            let generations_path = entity.join("generations");
            create_directory(&generations_path);
            if generation {
                let generation_path = generations_path.join(GENERATION_ONE);
                create_directory(&generation_path);
                if let Some(head) = head {
                    create_file(&generation_path.join("head.json"), head);
                }
                if let Some(definition) = definition {
                    create_file(&generation_path.join("definition.json"), definition);
                }
                if committed {
                    create_file(&generation_path.join("COMMITTED"), b"");
                }
            }
        }
        entity
    }

    fn session_path(root: &Path, session_id: &str) -> PathBuf {
        root.join(SESSIONS_DIRECTORY).join(session_id)
    }

    fn session_generation_path(root: &Path, session_id: &str) -> PathBuf {
        session_path(root, session_id)
            .join("generations")
            .join(GENERATION_ONE)
    }

    fn session_generation_path_named(root: &Path, session_id: &str, generation: &str) -> PathBuf {
        session_path(root, session_id)
            .join("generations")
            .join(generation)
    }

    fn create_exact_g1_session(
        root: &Path,
        session_id: &str,
        head: &[u8],
        definition: &[u8],
        conversation: &[u8],
    ) {
        create_file(
            &root
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(session_id),
            b"",
        );
        let entity = session_path(root, session_id);
        create_directory(&entity);
        create_file(&entity.join("PUBLISHED"), b"");
        create_file(&entity.join("conversation.jsonl"), conversation);
        let generations = entity.join("generations");
        create_directory(&generations);
        let generation = generations.join(GENERATION_ONE);
        create_directory(&generation);
        create_file(&generation.join("head.json"), head);
        create_file(&generation.join("definition.json"), definition);
        create_file(&generation.join("COMMITTED"), b"");
    }

    fn create_valid_g1_session(root: &Path, conversation: &[u8]) {
        let definition = session_definition_fixture();
        create_exact_g1_session(
            root,
            SESSION_ONE,
            session_head_fixture(),
            &definition,
            conversation,
        );
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test helper mirrors one physical entity shape"
    )]
    fn create_unpublished_session(
        root: &Path,
        session_id: &str,
        generations: bool,
        generation: bool,
        head: Option<&[u8]>,
        definition: Option<&[u8]>,
        committed: bool,
        conversation: Option<&[u8]>,
    ) -> PathBuf {
        create_file(
            &root
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(session_id),
            b"",
        );
        let entity = session_path(root, session_id);
        create_directory(&entity);
        if let Some(conversation) = conversation {
            create_file(&entity.join("conversation.jsonl"), conversation);
        }
        if generations {
            let generations_path = entity.join("generations");
            create_directory(&generations_path);
            if generation {
                let generation_path = generations_path.join(GENERATION_ONE);
                create_directory(&generation_path);
                if let Some(head) = head {
                    create_file(&generation_path.join("head.json"), head);
                }
                if let Some(definition) = definition {
                    create_file(&generation_path.join("definition.json"), definition);
                }
                if committed {
                    create_file(&generation_path.join("COMMITTED"), b"");
                }
            }
        }
        entity
    }

    fn ordinary_header_only_conversation(session_id: &str) -> Vec<u8> {
        let session = replace_fixture(
            include_bytes!("../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl"),
            "ses_11111111111111111111111111111111",
            session_id,
        );
        let agent = replace_fixture(&session, "agt_22222222222222222222222222222222", AGENT_ONE);
        replace_fixture(
            &agent,
            "2026-07-31T12:00:00.000Z",
            "2026-08-03T10:01:00.456Z",
        )
    }

    fn create_ordinary_g1_session(root: &Path, session_id: &str, conversation: &[u8]) {
        let head = replace_fixture(session_head_fixture(), SESSION_ONE, session_id);
        let definition = replace_fixture(&session_definition_fixture(), SESSION_ONE, session_id);
        create_exact_g1_session(root, session_id, &head, &definition, conversation);
    }

    fn fork_provenance_json(source_session_id: &str, source: &str, anchor: &str) -> String {
        format!(
            r#"{{"sourceSessionId":"{source_session_id}","source":"{source}","anchor":{anchor}}}"#
        )
    }

    fn fork_session_head_for(
        session_id: &str,
        source_session_id: &str,
        source: &str,
        anchor: &str,
    ) -> Vec<u8> {
        let child = replace_fixture(
            fork_session_head_fixture(),
            r#""sessionId":"ses_33333333333333333333333333333333""#,
            &format!(r#""sessionId":"{session_id}""#),
        );
        let source_session = replace_fixture(
            &child,
            r#""sourceSessionId":"ses_22222222222222222222222222222222""#,
            &format!(r#""sourceSessionId":"{source_session_id}""#),
        );
        let source = replace_fixture(
            &source_session,
            r#""source":"recorded_history""#,
            &format!(r#""source":"{source}""#),
        );
        replace_fixture(
            &source,
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            anchor,
        )
    }

    fn fork_session_definition_for(session_id: &str) -> Vec<u8> {
        replace_fixture(&fork_session_definition_fixture(), SESSION_TWO, session_id)
    }

    fn create_fork_g1_session(
        root: &Path,
        session_id: &str,
        source_session_id: &str,
        source: &str,
        anchor: &str,
        conversation: &[u8],
    ) {
        let head = fork_session_head_for(session_id, source_session_id, source, anchor);
        let definition = fork_session_definition_for(session_id);
        create_exact_g1_session(root, session_id, &head, &definition, conversation);
    }

    fn fork_g2_head_from(fixture: &[u8], session_id: &str, provenance: Option<&str>) -> Vec<u8> {
        let session = replace_fixture(
            fixture,
            r#""sessionId":"ses_22222222222222222222222222222222""#,
            &format!(r#""sessionId":"{session_id}""#),
        );
        let provenance = match provenance {
            Some(provenance) => replace_fixture(
                &session,
                r#""forkProvenance":null"#,
                &format!(r#""forkProvenance":{provenance}"#),
            ),
            None => session,
        };
        replace_fixture(
            &provenance,
            r#""createdAt":"2026-08-03T10:01:00.456Z"#,
            r#""createdAt":"2026-08-03T10:02:00.789Z"#,
        )
    }

    fn create_session_generation(
        root: &Path,
        session_id: &str,
        generation: &str,
        head: &[u8],
        definition: Option<&[u8]>,
    ) {
        let path = session_generation_path_named(root, session_id, generation);
        create_directory(&path);
        create_file(&path.join("head.json"), head);
        if let Some(definition) = definition {
            create_file(&path.join("definition.json"), definition);
        }
        create_file(&path.join("COMMITTED"), b"");
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

    fn create_markerless_agent_generation(
        root: &Path,
        agent_id: &str,
        generation: &str,
        head: Option<&[u8]>,
        definition: Option<&[u8]>,
    ) -> PathBuf {
        let path = generation_path_named(root, agent_id, generation);
        create_directory(&path);
        if let Some(head) = head {
            create_file(&path.join("head.json"), head);
        }
        if let Some(definition) = definition {
            create_file(&path.join("definition.json"), definition);
        }
        path
    }

    fn create_markerless_session_generation(
        root: &Path,
        session_id: &str,
        generation: &str,
        head: Option<&[u8]>,
        definition: Option<&[u8]>,
    ) -> PathBuf {
        let path = session_generation_path_named(root, session_id, generation);
        create_directory(&path);
        if let Some(head) = head {
            create_file(&path.join("head.json"), head);
        }
        if let Some(definition) = definition {
            create_file(&path.join("definition.json"), definition);
        }
        path
    }

    fn create_agent_both_payload_staging(root: &Path) -> PathBuf {
        create_marked_empty_store(root);
        create_valid_g1_agent(root);
        create_markerless_agent_generation(
            root,
            AGENT_ONE,
            GENERATION_TWO,
            Some(b"interrupted head JSON"),
            Some(b"interrupted definition JSON"),
        )
    }

    fn create_unpublished_committed_session_staging(root: &Path) -> PathBuf {
        create_marked_empty_store(root);
        create_valid_g1_agent(root);
        create_unpublished_session(
            root,
            SESSION_ONE,
            true,
            true,
            Some(session_head_fixture()),
            Some(&session_definition_fixture()),
            true,
            Some(&ordinary_header_only_conversation(SESSION_ONE)),
        )
    }

    fn unpublished_entity_cleanup_plan(root: &Path) -> super::DeferredUnpublishedEntityCleanup {
        let entries = read_entries_bounded(root, ROOT_ENTRY_CAP).expect("the marked root is read");
        let cleanup = super::recover_marked_root(root, &entries)
            .expect("the exact invisible entity has a cleanup plan")
            .cleanup
            .expect("one exact candidate registers cleanup");
        drop(entries);
        let super::DeferredCleanup::UnpublishedEntity(cleanup) = cleanup else {
            panic!("the helper is only used for whole-entity candidates");
        };
        *cleanup
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

    async fn assert_ordinary_session_g2_is_corrupt(head: &[u8], definition: Option<&[u8]>) {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_valid_g1_session(root.path(), b"conversation is opaque during store recovery");
        create_session_generation(root.path(), SESSION_ONE, GENERATION_TWO, head, definition);
        assert_corrupt(open(root.path()).await);
    }

    fn create_ordinary_session_chain_through_g3_deleted(root: &Path) {
        create_valid_g1_agent(root);
        create_valid_g1_session(root, b"conversation is opaque during store recovery");
        create_session_generation(
            root,
            SESSION_ONE,
            GENERATION_TWO,
            session_head_archived_g2_fixture(),
            None,
        );
        create_session_generation(
            root,
            SESSION_ONE,
            GENERATION_THREE,
            session_head_deleted_g3_fixture(),
            None,
        );
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
    async fn unpublished_committed_g1_agent_is_invisible_and_cleaned_whole() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let entity = agent_path(root.path(), AGENT_ONE);
        let reservation = root
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(AGENTS_DIRECTORY)
            .join(AGENT_ONE);
        fs::remove_file(entity.join("PUBLISHED"))
            .expect("the staged entity has no visibility marker");

        let state = open(root.path())
            .await
            .expect("an exact unpublished committed Agent is invisible staging");
        assert!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .is_none()
        );
        assert!(!entity.exists(), "the whole unpublished entity is removed");
        assert!(
            reservation.is_file(),
            "the permanent reservation remains burned"
        );
        state.close().await;

        let reopened = open(root.path())
            .await
            .expect("whole-entity cleanup leaves the Store reopenable");
        assert!(
            reopened
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .is_none()
        );
        reopened.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unpublished_agent_exact_shape_matrix_is_invisible_and_preserves_reservation() {
        for (name, generations, generation, head, definition, committed) in [
            ("entity only", false, false, None, None, false),
            ("empty generations", true, false, None, None, false),
            ("empty G1", true, true, None, None, false),
            (
                "head only",
                true,
                true,
                Some(b"partial Agent head".as_slice()),
                None,
                false,
            ),
            (
                "definition only",
                true,
                true,
                None,
                Some(b"partial Agent definition".as_slice()),
                false,
            ),
            (
                "both payloads",
                true,
                true,
                Some(b"partial Agent head".as_slice()),
                Some(b"partial Agent definition".as_slice()),
                false,
            ),
            (
                "committed G1",
                true,
                true,
                Some(agent_head_fixture()),
                Some(agent_definition_fixture()),
                true,
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            let entity = create_unpublished_agent(
                root.path(),
                AGENT_ONE,
                generations,
                generation,
                head,
                definition,
                committed,
            );
            let reservation = root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE);

            let state = open(root.path()).await.unwrap_or_else(|error| {
                panic!("{name} is exact invisible Agent staging: {error:?}")
            });
            assert!(
                state
                    .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                    .is_none()
            );
            assert!(!entity.exists(), "{name} entity is whole-cleaned");
            assert!(reservation.exists(), "{name} reservation remains permanent");
            state.close().await;
            open(root.path())
                .await
                .unwrap_or_else(|error| panic!("{name} cleanup reopens: {error:?}"))
                .close()
                .await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unpublished_session_precommit_and_committed_shapes_are_invisible_after_strict_recovery()
     {
        for (name, generations, generation, head, definition, conversation) in [
            ("entity only", false, false, None, None, None),
            ("empty generations", true, false, None, None, None),
            ("empty G1", true, true, None, None, None),
            (
                "opaque precommit conversation and partial head",
                true,
                true,
                Some(b"partial Session head".as_slice()),
                None,
                Some(b"\xff precommit conversation bytes are never parsed".as_slice()),
            ),
            (
                "opaque precommit conversation and both payloads",
                true,
                true,
                Some(b"partial Session head".as_slice()),
                Some(b"partial Session definition".as_slice()),
                Some(b"not a JSONL Header".as_slice()),
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            let entity = create_unpublished_session(
                root.path(),
                SESSION_ONE,
                generations,
                generation,
                head,
                definition,
                false,
                conversation,
            );
            let state = open(root.path())
                .await
                .unwrap_or_else(|error| panic!("{name} is invisible Session staging: {error:?}"));
            assert!(
                state
                    .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                    .is_none()
            );
            assert!(!entity.exists(), "{name} entity is whole-cleaned");
            state.close().await;
        }

        let ordinary = TempRoot::existing();
        create_marked_empty_store(ordinary.path());
        create_valid_g1_agent(ordinary.path());
        let entity = create_unpublished_session(
            ordinary.path(),
            SESSION_ONE,
            true,
            true,
            Some(session_head_fixture()),
            Some(&session_definition_fixture()),
            true,
            Some(&ordinary_header_only_conversation(SESSION_ONE)),
        );
        let state = open(ordinary.path())
            .await
            .expect("committed ordinary Header-only Session staging is cleanup-only");
        assert!(
            state
                .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                .is_none()
        );
        assert!(!entity.exists());
        state.close().await;

        for (name, session_id, head, definition, conversation) in [
            (
                "canonical Fork",
                SESSION_TWO,
                fork_session_head_fixture().to_vec(),
                fork_session_definition_fixture(),
                valid_fork_conversation(SESSION_TWO),
            ),
            (
                "Genesis Header-only Fork",
                SESSION_THREE,
                genesis_fork_session_head_fixture().to_vec(),
                genesis_fork_session_definition_fixture(),
                replace_fixture(
                    &ordinary_header_only_conversation(SESSION_THREE),
                    "2026-08-03T10:01:00.456Z",
                    "2026-08-03T10:03:00.000Z",
                ),
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_valid_g1_session(root.path(), b"published source bytes stay opaque");
            let entity = create_unpublished_session(
                root.path(),
                session_id,
                true,
                true,
                Some(&head),
                Some(&definition),
                true,
                Some(&conversation),
            );
            let state = open(root.path())
                .await
                .unwrap_or_else(|error| panic!("{name} candidate validates: {error:?}"));
            assert!(
                state
                    .session_head(SessionId::from_str(session_id).unwrap())
                    .is_none()
            );
            assert!(
                state
                    .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                    .is_some()
            );
            assert!(!entity.exists());
            state.close().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unpublished_committed_sessions_apply_the_current_enabled_agent_gate() {
        let stale_ordinary = TempRoot::existing();
        create_marked_empty_store(stale_ordinary.path());
        create_valid_g1_agent(stale_ordinary.path());
        create_agent_generation(
            stale_ordinary.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_definition_g2_fixture(),
            Some(agent_definition_g2_fixture()),
        );
        let stale = create_unpublished_session(
            stale_ordinary.path(),
            SESSION_ONE,
            true,
            true,
            Some(session_head_fixture()),
            Some(&session_definition_fixture()),
            true,
            Some(&ordinary_header_only_conversation(SESSION_ONE)),
        );
        assert_corrupt(open(stale_ordinary.path()).await);
        assert!(
            stale.exists(),
            "a stale ordinary candidate is never deleted"
        );

        for (name, status_head) in [
            ("disabled", agent_head_status_g2_fixture().to_vec()),
            (
                "deleted",
                replace_fixture(
                    agent_head_status_g2_fixture(),
                    "\"status\":\"disabled\"",
                    "\"status\":\"deleted\"",
                ),
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_agent_generation(root.path(), AGENT_ONE, GENERATION_TWO, &status_head, None);
            let ordinary = create_unpublished_session(
                root.path(),
                SESSION_ONE,
                true,
                true,
                Some(session_head_fixture()),
                Some(&session_definition_fixture()),
                true,
                Some(&ordinary_header_only_conversation(SESSION_ONE)),
            );
            assert_corrupt(open(root.path()).await);
            assert!(ordinary.exists(), "{name} ordinary candidate is retained");

            let fork_root = TempRoot::existing();
            create_marked_empty_store(fork_root.path());
            create_valid_g1_agent(fork_root.path());
            create_agent_generation(
                fork_root.path(),
                AGENT_ONE,
                GENERATION_TWO,
                &status_head,
                None,
            );
            create_valid_g1_session(
                fork_root.path(),
                b"published source stays physically opaque",
            );
            let fork = create_unpublished_session(
                fork_root.path(),
                SESSION_TWO,
                true,
                true,
                Some(fork_session_head_fixture()),
                Some(&fork_session_definition_fixture()),
                true,
                Some(&valid_fork_conversation(SESSION_TWO)),
            );
            assert_corrupt(open(fork_root.path()).await);
            assert!(fork.exists(), "{name} Fork candidate is retained");
        }

        let retained_old_fork = TempRoot::existing();
        create_marked_empty_store(retained_old_fork.path());
        create_valid_g1_agent(retained_old_fork.path());
        create_agent_generation(
            retained_old_fork.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_definition_g2_fixture(),
            Some(agent_definition_g2_fixture()),
        );
        create_valid_g1_session(
            retained_old_fork.path(),
            b"published source remains an opaque recovery concern",
        );
        let fork = create_unpublished_session(
            retained_old_fork.path(),
            SESSION_TWO,
            true,
            true,
            Some(fork_session_head_fixture()),
            Some(&fork_session_definition_fixture()),
            true,
            Some(&valid_fork_conversation(SESSION_TWO)),
        );
        let state = open(retained_old_fork.path())
            .await
            .expect("an Enabled Agent retains an old exact Fork pin for cleanup");
        assert!(
            !fork.exists(),
            "the valid old-revision Fork is invisible and cleaned"
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_unpublished_entities_and_conversations_fail_closed_without_cleanup() {
        let unknown = TempRoot::existing();
        create_marked_empty_store(unknown.path());
        let entity =
            create_unpublished_agent(unknown.path(), AGENT_ONE, true, true, None, None, false);
        create_file(
            &entity
                .join("generations")
                .join(GENERATION_ONE)
                .join("unknown"),
            b"",
        );
        assert_corrupt(open(unknown.path()).await);
        assert!(entity.exists());

        let g2 = TempRoot::existing();
        create_marked_empty_store(g2.path());
        let entity = create_unpublished_agent(g2.path(), AGENT_ONE, true, true, None, None, false);
        fs::rename(
            entity.join("generations").join(GENERATION_ONE),
            entity.join("generations").join(GENERATION_TWO),
        )
        .unwrap();
        assert_corrupt(open(g2.path()).await);
        assert!(entity.exists());

        let missing_reservation = TempRoot::existing();
        create_marked_empty_store(missing_reservation.path());
        let entity = create_unpublished_agent(
            missing_reservation.path(),
            AGENT_ONE,
            false,
            false,
            None,
            None,
            false,
        );
        fs::remove_file(
            missing_reservation
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
        )
        .unwrap();
        assert_corrupt(open(missing_reservation.path()).await);
        assert!(entity.exists());

        let conversation_without_g1 = TempRoot::existing();
        create_marked_empty_store(conversation_without_g1.path());
        let entity = create_unpublished_session(
            conversation_without_g1.path(),
            SESSION_ONE,
            false,
            false,
            None,
            None,
            false,
            Some(b"opaque bytes"),
        );
        assert_corrupt(open(conversation_without_g1.path()).await);
        assert!(entity.exists());

        for (name, conversation) in [
            ("missing", None),
            ("invalid Header", Some(b"not JSONL".as_slice())),
            (
                "partial tail",
                Some(b"{\"type\":\"session_header\"".as_slice()),
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            let entity = create_unpublished_session(
                root.path(),
                SESSION_ONE,
                true,
                true,
                Some(session_head_fixture()),
                Some(&session_definition_fixture()),
                true,
                conversation,
            );
            assert_corrupt(open(root.path()).await);
            assert!(
                entity.exists(),
                "{name} committed Session remains for diagnosis"
            );
        }

        let oversized = TempRoot::existing();
        create_marked_empty_store(oversized.path());
        let entity = create_unpublished_session(
            oversized.path(),
            SESSION_ONE,
            true,
            true,
            None,
            None,
            false,
            Some(b""),
        );
        let conversation = entity.join("conversation.jsonl");
        OpenOptions::new()
            .write(true)
            .open(&conversation)
            .unwrap()
            .set_len(super::MAX_CONVERSATION_FILE_BYTES + 1)
            .unwrap();
        assert!(matches!(
            open(oversized.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(entity.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_unpublished_conversation_dominates_an_invalid_committed_marker() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        let entity = create_unpublished_session(
            root.path(),
            SESSION_ONE,
            true,
            true,
            Some(session_head_fixture()),
            Some(&session_definition_fixture()),
            false,
            Some(b""),
        );
        create_file(
            &entity
                .join("generations")
                .join(GENERATION_ONE)
                .join("COMMITTED"),
            b"not-zero",
        );
        OpenOptions::new()
            .write(true)
            .open(entity.join("conversation.jsonl"))
            .unwrap()
            .set_len(super::MAX_CONVERSATION_FILE_BYTES + 1)
            .unwrap();

        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(
            entity.exists(),
            "an oversized entity with an invalid COMMITTED marker is never deleted"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_operation_candidates_are_corrupt_and_never_partly_cleaned() {
        let two_agents = TempRoot::existing();
        create_marked_empty_store(two_agents.path());
        let first = create_unpublished_agent(
            two_agents.path(),
            AGENT_ONE,
            false,
            false,
            None,
            None,
            false,
        );
        let second = create_unpublished_agent(
            two_agents.path(),
            AGENT_TWO,
            false,
            false,
            None,
            None,
            false,
        );
        assert_corrupt(open(two_agents.path()).await);
        assert!(first.exists() && second.exists());

        let agent_and_session = TempRoot::existing();
        create_marked_empty_store(agent_and_session.path());
        let agent = create_unpublished_agent(
            agent_and_session.path(),
            AGENT_ONE,
            false,
            false,
            None,
            None,
            false,
        );
        let session = create_unpublished_session(
            agent_and_session.path(),
            SESSION_ONE,
            false,
            false,
            None,
            None,
            false,
            None,
        );
        assert_corrupt(open(agent_and_session.path()).await);
        assert!(agent.exists() && session.exists());

        let published_tail_and_entity = TempRoot::existing();
        create_marked_empty_store(published_tail_and_entity.path());
        create_valid_g1_agent(published_tail_and_entity.path());
        let tail = create_markerless_agent_generation(
            published_tail_and_entity.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        let entity = create_unpublished_session(
            published_tail_and_entity.path(),
            SESSION_ONE,
            false,
            false,
            None,
            None,
            false,
            None,
        );
        assert_corrupt(open(published_tail_and_entity.path()).await);
        assert!(tail.exists() && entity.exists());
    }

    #[test]
    fn candidate_multiplicity_never_masks_a_later_nested_generation_cap() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        let candidate =
            create_unpublished_agent(root.path(), AGENT_ONE, false, false, None, None, false);
        let oversized = create_unpublished_session(
            root.path(),
            SESSION_ONE,
            true,
            true,
            None,
            None,
            false,
            None,
        );
        create_directory(&oversized.join("generations").join(GENERATION_TWO));
        let caps = RecoveryCaps {
            agent_reservations: 1,
            session_reservations: 1,
            root_agents: 1,
            root_sessions: 1,
            generations: 1,
        };
        assert!(matches!(
            recover_marked_root_with_caps(root.path(), &marked_entries(root.path()), caps),
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(candidate.exists() && oversized.exists());
    }

    #[test]
    fn later_session_generation_caps_dominate_each_earlier_agent_entity_physical_error() {
        for kind in ["wrong marker type", "invalid marker", "unknown child"] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_file(
                &root
                    .path()
                    .join(RESERVATIONS_DIRECTORY)
                    .join(AGENTS_DIRECTORY)
                    .join(AGENT_ONE),
                b"",
            );
            let agent = agent_path(root.path(), AGENT_ONE);
            create_directory(&agent);
            match kind {
                "wrong marker type" => create_directory(&agent.join("PUBLISHED")),
                "invalid marker" => create_file(&agent.join("PUBLISHED"), b"nonzero"),
                "unknown child" => create_file(&agent.join("unknown"), b""),
                _ => unreachable!("the table is closed"),
            }
            let session = create_unpublished_session(
                root.path(),
                SESSION_ONE,
                true,
                true,
                None,
                None,
                false,
                None,
            );
            create_directory(&session.join("generations").join(GENERATION_TWO));
            let caps = RecoveryCaps {
                agent_reservations: 1,
                session_reservations: 1,
                root_agents: 1,
                root_sessions: 1,
                generations: 1,
            };
            assert!(matches!(
                recover_marked_root_with_caps(root.path(), &marked_entries(root.path()), caps),
                Err(DurableOpenError::DurableStateTooLarge)
            ));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_g1_document_dominates_an_invalid_committed_marker() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        let entity = create_unpublished_agent(
            root.path(),
            AGENT_ONE,
            true,
            true,
            Some(&vec![b'x'; super::MAX_DURABLE_DOCUMENT_BYTES + 1]),
            Some(agent_definition_fixture()),
            false,
        );
        create_file(
            &entity
                .join("generations")
                .join(GENERATION_ONE)
                .join("COMMITTED"),
            b"nonzero",
        );
        assert!(matches!(
            open(root.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(
            entity.exists(),
            "the invalid oversized candidate is not deleted"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_published_fork_validation_failure_keeps_an_unpublished_candidate() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let candidate =
            create_unpublished_agent(root.path(), AGENT_TWO, false, false, None, None, false);
        create_fork_g1_session(
            root.path(),
            SESSION_ONE,
            SESSION_TWO,
            "recorded_history",
            r#"{"type":"genesis"}"#,
            &valid_fork_conversation(SESSION_ONE),
        );

        assert_corrupt(open(root.path()).await);
        assert!(
            candidate.exists(),
            "cleanup waits for the complete published Fork graph validation"
        );
    }

    #[test]
    fn unpublished_cleanup_revalidation_rejects_published_or_conversation_drift_without_deleting() {
        let published = TempRoot::existing();
        create_marked_empty_store(published.path());
        let entity =
            create_unpublished_agent(published.path(), AGENT_ONE, false, false, None, None, false);
        let root_entries = read_entries_bounded(published.path(), ROOT_ENTRY_CAP).unwrap();
        let super::DeferredCleanup::UnpublishedEntity(cleanup) =
            super::recover_marked_root(published.path(), &root_entries)
                .unwrap()
                .cleanup
                .expect("an unpublished entity has a cleanup plan")
        else {
            panic!("the plan is whole-entity cleanup");
        };
        drop(root_entries);
        create_file(&entity.join("PUBLISHED"), b"");
        assert!(matches!(
            super::finalize_deferred_cleanup(
                &super::DeferredCleanup::UnpublishedEntity(cleanup),
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(entity.exists());

        let conversation = TempRoot::existing();
        create_marked_empty_store(conversation.path());
        let entity = create_unpublished_session(
            conversation.path(),
            SESSION_ONE,
            true,
            true,
            Some(b"partial head"),
            None,
            false,
            Some(b"opaque first bytes"),
        );
        let root_entries = read_entries_bounded(conversation.path(), ROOT_ENTRY_CAP).unwrap();
        let super::DeferredCleanup::UnpublishedEntity(cleanup) =
            super::recover_marked_root(conversation.path(), &root_entries)
                .unwrap()
                .cleanup
                .expect("the Session staging has a cleanup plan")
        else {
            panic!("the plan is whole-entity cleanup");
        };
        drop(root_entries);
        create_file(
            &entity.join("conversation.jsonl"),
            b"a deliberately much longer opaque conversation value",
        );
        assert!(matches!(
            super::finalize_deferred_cleanup(
                &super::DeferredCleanup::UnpublishedEntity(cleanup),
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(entity.exists());
    }

    #[cfg(unix)]
    #[test]
    fn whole_entity_cleanup_revalidation_rejects_same_shape_node_replacements() {
        let committed = TempRoot::existing();
        let entity = create_unpublished_committed_session_staging(committed.path());
        let cleanup = unpublished_entity_cleanup_plan(committed.path());
        let marker = entity
            .join("generations")
            .join(GENERATION_ONE)
            .join("COMMITTED");
        let replacement = marker.with_extension("replacement");
        create_file(&replacement, b"");
        fs::rename(&replacement, &marker).expect("the zero marker is atomically replaced");
        assert!(matches!(
            super::finalize_deferred_cleanup(
                &super::DeferredCleanup::UnpublishedEntity(Box::new(cleanup)),
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            entity.exists(),
            "changed COMMITTED identity causes zero deletion"
        );

        let generation = TempRoot::existing();
        create_marked_empty_store(generation.path());
        let entity =
            create_unpublished_agent(generation.path(), AGENT_ONE, true, true, None, None, false);
        let cleanup = unpublished_entity_cleanup_plan(generation.path());
        let g1 = entity.join("generations").join(GENERATION_ONE);
        fs::rename(&g1, generation.path().join("old-g1")).expect("the original G1 is moved aside");
        create_directory(&g1);
        assert!(matches!(
            super::finalize_deferred_cleanup(
                &super::DeferredCleanup::UnpublishedEntity(Box::new(cleanup)),
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            entity.exists(),
            "same-shape G1 replacement causes zero deletion"
        );

        let entity_root = TempRoot::existing();
        create_marked_empty_store(entity_root.path());
        let entity = create_unpublished_agent(
            entity_root.path(),
            AGENT_ONE,
            false,
            false,
            None,
            None,
            false,
        );
        let cleanup = unpublished_entity_cleanup_plan(entity_root.path());
        fs::rename(&entity, entity_root.path().join("old-entity"))
            .expect("the original entity is moved aside");
        create_directory(&entity);
        assert!(matches!(
            super::finalize_deferred_cleanup(
                &super::DeferredCleanup::UnpublishedEntity(Box::new(cleanup)),
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            entity.exists(),
            "same-shape entity replacement causes zero deletion"
        );

        let reservation_root = TempRoot::existing();
        create_marked_empty_store(reservation_root.path());
        let entity = create_unpublished_agent(
            reservation_root.path(),
            AGENT_ONE,
            false,
            false,
            None,
            None,
            false,
        );
        let cleanup = unpublished_entity_cleanup_plan(reservation_root.path());
        let reservation = reservation_root
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(AGENTS_DIRECTORY)
            .join(AGENT_ONE);
        let replacement = reservation.with_extension("replacement");
        create_file(&replacement, b"");
        fs::rename(&replacement, &reservation)
            .expect("the permanent zero reservation is atomically replaced");
        assert!(matches!(
            super::finalize_deferred_cleanup(
                &super::DeferredCleanup::UnpublishedEntity(Box::new(cleanup)),
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            entity.exists(),
            "reservation replacement causes zero deletion"
        );
    }

    #[test]
    fn whole_entity_cleanup_revalidation_reuses_its_initial_generation_collection_cap() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        let entity =
            create_unpublished_agent(root.path(), AGENT_ONE, true, true, None, None, false);
        let caps = RecoveryCaps {
            agent_reservations: 1,
            session_reservations: 1,
            root_agents: 1,
            root_sessions: 1,
            generations: 1,
        };
        let entries = marked_entries(root.path());
        let cleanup = recover_marked_root_with_caps(root.path(), &entries, caps)
            .expect("the one-G1 candidate is scanned with the small cap")
            .cleanup
            .expect("the candidate has a deferred cleanup plan");
        drop(entries);
        let super::DeferredCleanup::UnpublishedEntity(cleanup) = cleanup else {
            panic!("the candidate is whole-entity cleanup");
        };
        assert_eq!(cleanup.generation_collection_cap, 1);
        create_directory(&entity.join("generations").join(GENERATION_TWO));
        assert!(matches!(
            super::finalize_deferred_cleanup(
                &super::DeferredCleanup::UnpublishedEntity(cleanup),
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(entity.exists());
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
    async fn published_agent_markerless_trailing_generation_is_cleaned_before_catalog_install() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let pending = generation_path_named(root.path(), AGENT_ONE, GENERATION_TWO);
        create_directory(&pending);
        // A markerless payload is deliberately not decoded: this partial JSON may be an
        // interrupted write and the prior committed G1 remains the recovered head.
        create_file(&pending.join("head.json"), b"partial JSON is never decoded");

        let state = open(root.path())
            .await
            .expect("the trailing uncommitted generation is deferred-cleaned");
        assert_eq!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            1
        );
        assert!(
            !pending.exists(),
            "cleanup completes before the recovered catalog is installed"
        );
        state.close().await;
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
    async fn agent_entity_shape_keeps_invalid_published_corrupt_and_accepts_exact_unpublished_g1() {
        let missing_published = TempRoot::existing();
        create_marked_empty_store(missing_published.path());
        create_valid_g1_agent(missing_published.path());
        let entity = agent_path(missing_published.path(), AGENT_ONE);
        let reservation = missing_published
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(AGENTS_DIRECTORY)
            .join(AGENT_ONE);
        fs::remove_file(entity.join("PUBLISHED")).expect("the visibility marker is removed");
        let state = open(missing_published.path())
            .await
            .expect("missing PUBLISHED is exact invisible Agent staging");
        assert!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .is_none()
        );
        assert!(!entity.exists());
        assert!(reservation.exists());
        state.close().await;

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
    async fn generation_chain_namespace_requires_canonical_names_a_committed_g1_and_one_trailing_markerless_directory()
     {
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
        let markerless = generation_path_named(multiple.path(), AGENT_ONE, GENERATION_TWO);
        let state = open(multiple.path())
            .await
            .expect("one empty G2 staging directory is safely deferred-cleaned");
        assert!(!markerless.exists());
        assert_eq!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            1
        );
        state.close().await;

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
    async fn marked_empty_store_has_an_empty_private_session_catalog() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());

        let state = open(root.path())
            .await
            .expect("the marked empty store opens");
        let session_id = SessionId::from_str(SESSION_ONE).unwrap();
        assert!(state.session_head(session_id).is_none());
        assert!(state.session_current_definition(session_id).is_none());
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exact_ordinary_g1_session_recovers_same_arcs_without_reading_conversation_bytes() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let conversation = b"\xff entirely arbitrary non-header bytes\x00\n";
        create_valid_g1_session(root.path(), conversation);
        let conversation_path = session_path(root.path(), SESSION_ONE).join("conversation.jsonl");
        let before = fs::read(&conversation_path).unwrap();

        let state = open(root.path())
            .await
            .expect("a regular conversation is physical-only at store open");
        let session_id = SessionId::from_str(SESSION_ONE).unwrap();
        let first = state
            .session_head(session_id)
            .expect("the Session is catalogued");
        let second = state
            .session_head(session_id)
            .expect("the same head is retained");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.session_id(), session_id);
        assert_eq!(first.storage_generation().get(), 1);
        assert_eq!(first.current_definition_revision().get(), 1);
        assert_eq!(first.current_definition_storage_generation().get(), 1);
        assert_eq!(first.metadata().revision().get(), 1);
        assert_eq!(first.lifecycle(), SessionLifecycle::Open);
        assert!(first.fork_provenance().is_none());

        let definition = state
            .session_current_definition(session_id)
            .expect("the G1 Session definition is retained");
        let same_definition = state
            .session_current_definition(session_id)
            .expect("the same G1 Session definition is retained");
        assert!(Arc::ptr_eq(&definition, &same_definition));
        assert_eq!(definition.session_id(), session_id);
        assert_eq!(definition.revision().get(), 1);
        assert_eq!(definition.workspace().revision().get(), 1);

        let debug = format!("{:?}", state.sessions.get(&session_id).unwrap());
        for secret in [SESSION_ONE, AGENT_ONE, "session-notes", "openai"] {
            assert!(!debug.contains(secret), "catalog debug leaked {secret:?}");
        }
        state.close().await;
        assert_eq!(fs::read(conversation_path).unwrap(), before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn orphan_session_reservation_is_invisible_but_published_session_requires_an_exact_match()
    {
        let orphan = TempRoot::existing();
        create_marked_empty_store(orphan.path());
        create_file(
            &orphan
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_ONE),
            b"",
        );
        let state = open(orphan.path())
            .await
            .expect("an orphan Session reservation is permanent but invisible");
        assert!(
            state
                .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                .is_none()
        );
        state.close().await;

        let missing = TempRoot::existing();
        create_marked_empty_store(missing.path());
        create_valid_g1_agent(missing.path());
        create_valid_g1_session(missing.path(), b"bad Header bytes are not scanned");
        fs::remove_file(
            missing
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_ONE),
        )
        .unwrap();
        assert_corrupt(open(missing.path()).await);

        let mismatched = TempRoot::existing();
        create_marked_empty_store(mismatched.path());
        create_valid_g1_agent(mismatched.path());
        create_valid_g1_session(mismatched.path(), b"bad Header bytes are not scanned");
        fs::rename(
            mismatched
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_ONE),
            mismatched
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_TWO),
        )
        .unwrap();
        assert_corrupt(open(mismatched.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_entity_keeps_invalid_published_corrupt_and_accepts_exact_unpublished_g1() {
        let missing_published = TempRoot::existing();
        create_marked_empty_store(missing_published.path());
        create_valid_g1_agent(missing_published.path());
        create_valid_g1_session(missing_published.path(), b"arbitrary");
        let entity = session_path(missing_published.path(), SESSION_ONE);
        let reservation = missing_published
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(SESSIONS_DIRECTORY)
            .join(SESSION_ONE);
        fs::remove_file(entity.join("PUBLISHED")).unwrap();
        // G1 COMMITTED requires a conversation, but published Session behavior remains
        // physical-only. The unpublished path validates this canonical Header-only file.
        let header = replace_fixture(
            &replace_fixture(
                &replace_fixture(
                    include_bytes!(
                        "../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl"
                    ),
                    "ses_11111111111111111111111111111111",
                    SESSION_ONE,
                ),
                "agt_22222222222222222222222222222222",
                AGENT_ONE,
            ),
            "2026-07-31T12:00:00.000Z",
            "2026-08-03T10:01:00.456Z",
        );
        create_file(&entity.join("conversation.jsonl"), &header);
        let state = open(missing_published.path())
            .await
            .expect("missing PUBLISHED is exact invisible Session staging");
        assert!(
            state
                .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                .is_none()
        );
        assert!(!entity.exists());
        assert!(reservation.exists());
        state.close().await;

        let missing_conversation = TempRoot::existing();
        create_marked_empty_store(missing_conversation.path());
        create_valid_g1_agent(missing_conversation.path());
        create_valid_g1_session(missing_conversation.path(), b"arbitrary");
        fs::remove_file(
            session_path(missing_conversation.path(), SESSION_ONE).join("conversation.jsonl"),
        )
        .unwrap();
        assert_corrupt(open(missing_conversation.path()).await);

        let generations_file = TempRoot::existing();
        create_marked_empty_store(generations_file.path());
        create_valid_g1_agent(generations_file.path());
        create_valid_g1_session(generations_file.path(), b"arbitrary");
        let generations = session_path(generations_file.path(), SESSION_ONE).join("generations");
        fs::remove_dir_all(&generations).unwrap();
        create_file(&generations, b"");
        assert_corrupt(open(generations_file.path()).await);

        let too_many = TempRoot::existing();
        create_marked_empty_store(too_many.path());
        create_valid_g1_agent(too_many.path());
        create_valid_g1_session(too_many.path(), b"arbitrary");
        create_file(
            &session_path(too_many.path(), SESSION_ONE).join("foreign-fourth"),
            b"",
        );
        assert!(matches!(
            open(too_many.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_g1_payload_requires_exact_head_definition_and_committed_and_obeys_its_cap() {
        for missing_name in ["head.json", "definition.json", "COMMITTED"] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_valid_g1_session(root.path(), b"arbitrary");
            fs::remove_file(session_generation_path(root.path(), SESSION_ONE).join(missing_name))
                .unwrap();
            assert_corrupt(open(root.path()).await);
        }

        let too_many = TempRoot::existing();
        create_marked_empty_store(too_many.path());
        create_valid_g1_agent(too_many.path());
        create_valid_g1_session(too_many.path(), b"arbitrary");
        create_file(
            &session_generation_path(too_many.path(), SESSION_ONE).join("foreign-fourth"),
            b"",
        );
        assert!(matches!(
            open(too_many.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn session_conversation_must_be_a_private_regular_non_link_file_without_reading_it() {
        use std::os::unix::fs::symlink;

        let directory = TempRoot::existing();
        create_marked_empty_store(directory.path());
        create_valid_g1_agent(directory.path());
        create_valid_g1_session(directory.path(), b"arbitrary");
        let conversation = session_path(directory.path(), SESSION_ONE).join("conversation.jsonl");
        fs::remove_file(&conversation).unwrap();
        create_directory(&conversation);
        assert_corrupt(open(directory.path()).await);

        let linked = TempRoot::existing();
        create_marked_empty_store(linked.path());
        create_valid_g1_agent(linked.path());
        create_valid_g1_session(linked.path(), b"arbitrary");
        let conversation = session_path(linked.path(), SESSION_ONE).join("conversation.jsonl");
        fs::remove_file(&conversation).unwrap();
        let target_root = TempRoot::existing();
        let target = target_root.path().join("conversation-target");
        create_file(&target, b"not a Header");
        symlink(&target, &conversation).unwrap();
        assert_corrupt(open(linked.path()).await);

        let wrong_mode = TempRoot::existing();
        create_marked_empty_store(wrong_mode.path());
        create_valid_g1_agent(wrong_mode.path());
        create_valid_g1_session(wrong_mode.path(), b"arbitrary");
        let conversation = session_path(wrong_mode.path(), SESSION_ONE).join("conversation.jsonl");
        set_unix_mode(&conversation, 0o644);
        assert_corrupt(open(wrong_mode.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_path_head_definition_and_g1_semantic_facts_must_agree() {
        let path_mismatch = TempRoot::existing();
        create_marked_empty_store(path_mismatch.path());
        create_valid_g1_agent(path_mismatch.path());
        create_valid_g1_session(path_mismatch.path(), b"arbitrary");
        fs::rename(
            path_mismatch
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_ONE),
            path_mismatch
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_TWO),
        )
        .unwrap();
        fs::rename(
            session_path(path_mismatch.path(), SESSION_ONE),
            session_path(path_mismatch.path(), SESSION_TWO),
        )
        .unwrap();
        assert_corrupt(open(path_mismatch.path()).await);

        let cases = [
            (
                "head Session ID",
                replace_fixture(session_head_fixture(), SESSION_ONE, SESSION_TWO),
                session_definition_fixture(),
            ),
            (
                "definition Session ID",
                session_head_fixture().to_vec(),
                replace_fixture(&session_definition_fixture(), SESSION_ONE, SESSION_TWO),
            ),
            (
                "head definition revision",
                replace_fixture(
                    session_head_fixture(),
                    "\"revision\":\"sdr_1\",\"storageGeneration\":1",
                    "\"revision\":\"sdr_2\",\"storageGeneration\":1",
                ),
                session_definition_fixture(),
            ),
            (
                "definition revision",
                session_head_fixture().to_vec(),
                replace_fixture(
                    &session_definition_fixture(),
                    "\"revision\":\"sdr_1\"",
                    "\"revision\":\"sdr_2\"",
                ),
            ),
            (
                "definition workspace revision",
                session_head_fixture().to_vec(),
                replace_fixture(
                    &session_definition_fixture(),
                    "\"revision\":\"wr_1\"",
                    "\"revision\":\"wr_2\"",
                ),
            ),
            (
                "metadata revision",
                replace_fixture(
                    session_head_fixture(),
                    "\"revision\":\"smr_1\"",
                    "\"revision\":\"smr_2\"",
                ),
                session_definition_fixture(),
            ),
            (
                "archived lifecycle",
                replace_fixture(
                    session_head_fixture(),
                    "\"lifecycle\":\"open\"",
                    "\"lifecycle\":\"archived\"",
                ),
                session_definition_fixture(),
            ),
            (
                "head created timestamp",
                replace_fixture(
                    session_head_fixture(),
                    "\"createdAt\":\"2026-08-03T10:01:00.456Z\"",
                    "\"createdAt\":\"2026-08-03T10:01:00.457Z\"",
                ),
                session_definition_fixture(),
            ),
            (
                "definition created timestamp",
                session_head_fixture().to_vec(),
                replace_fixture(
                    &session_definition_fixture(),
                    "\"createdAt\":\"2026-08-03T10:01:00.456Z\"",
                    "\"createdAt\":\"2026-08-03T10:01:00.457Z\"",
                ),
            ),
            (
                "metadata timestamp",
                replace_fixture(
                    session_head_fixture(),
                    "\"updatedAt\":\"2026-08-03T10:01:00.456Z\"",
                    "\"updatedAt\":\"2026-08-03T10:01:00.457Z\"",
                ),
                session_definition_fixture(),
            ),
        ];

        for (name, head, definition) in cases {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_exact_g1_session(root.path(), SESSION_ONE, &head, &definition, b"arbitrary");
            assert!(
                matches!(
                    open(root.path()).await,
                    Err(DurableOpenError::DurableStateCorrupt)
                ),
                "{name} must fail the G1 semantic matrix"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_agent_ref_must_resolve_to_a_retained_exact_agent_definition() {
        let missing_agent = TempRoot::existing();
        create_marked_empty_store(missing_agent.path());
        create_valid_g1_session(missing_agent.path(), b"arbitrary");
        assert_corrupt(open(missing_agent.path()).await);

        let missing_revision = TempRoot::existing();
        create_marked_empty_store(missing_revision.path());
        create_valid_g1_agent(missing_revision.path());
        let missing_definition = replace_fixture(
            &session_definition_fixture(),
            "\"revision\":\"ar_1\"",
            "\"revision\":\"ar_2\"",
        );
        create_exact_g1_session(
            missing_revision.path(),
            SESSION_ONE,
            session_head_fixture(),
            &missing_definition,
            b"arbitrary",
        );
        assert_corrupt(open(missing_revision.path()).await);

        let retained_old_revision = TempRoot::existing();
        create_marked_empty_store(retained_old_revision.path());
        create_valid_g1_agent(retained_old_revision.path());
        create_agent_generation(
            retained_old_revision.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_definition_g2_fixture(),
            Some(agent_definition_g2_fixture()),
        );
        create_valid_g1_session(retained_old_revision.path(), b"arbitrary");
        let state = open(retained_old_revision.path())
            .await
            .expect("a Session may pin retained AR1 while Agent current is AR2");
        let agent_id = AgentId::from_str(AGENT_ONE).unwrap();
        assert_eq!(
            state
                .agent_head(agent_id)
                .unwrap()
                .current_definition_revision()
                .get(),
            2
        );
        assert_eq!(
            state
                .session_current_definition(SessionId::from_str(SESSION_ONE).unwrap())
                .unwrap()
                .agent()
                .revision()
                .get(),
            1
        );
        state.close().await;

        let disabled_agent = TempRoot::existing();
        create_marked_empty_store(disabled_agent.path());
        create_valid_g1_agent(disabled_agent.path());
        create_agent_generation(
            disabled_agent.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_status_g2_fixture(),
            None,
        );
        create_valid_g1_session(disabled_agent.path(), b"arbitrary");
        let state = open(disabled_agent.path())
            .await
            .expect("recovery does not require a Session's Agent to remain Enabled");
        state.close().await;

        let deleted_agent = TempRoot::existing();
        create_marked_empty_store(deleted_agent.path());
        create_valid_g1_agent(deleted_agent.path());
        create_agent_generation(
            deleted_agent.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_status_g2_fixture(),
                "\"status\":\"disabled\"",
                "\"status\":\"deleted\"",
            ),
            None,
        );
        create_valid_g1_session(deleted_agent.path(), b"arbitrary");
        let state = open(deleted_agent.path())
            .await
            .expect("a Deleted Agent still retains exact definitions for Session history");
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_g1_and_fork_recover_and_markerless_staging_is_deferred_cleaned() {
        let fork = TempRoot::existing();
        create_marked_empty_store(fork.path());
        create_valid_g1_agent(fork.path());
        let source_conversation = b"source arbitrary conversation bytes";
        create_valid_g1_session(fork.path(), source_conversation);
        let fork_definition = fork_session_definition_fixture();
        create_exact_g1_session(
            fork.path(),
            SESSION_TWO,
            fork_session_head_fixture(),
            &fork_definition,
            b"child arbitrary conversation bytes",
        );
        let fork_head = session_generation_path(fork.path(), SESSION_TWO).join("head.json");
        let before = fs::read(&fork_head).unwrap();
        let source_conversation_path =
            session_path(fork.path(), SESSION_ONE).join("conversation.jsonl");
        let child_conversation_path =
            session_path(fork.path(), SESSION_TWO).join("conversation.jsonl");
        let source_conversation_before = fs::read(&source_conversation_path).unwrap();
        let child_conversation_before = fs::read(&child_conversation_path).unwrap();
        let state = open(fork.path())
            .await
            .expect("an authoritative recorded-history Fork G1 recovers beside its source");
        let child = state
            .session_head(SessionId::from_str(SESSION_TWO).unwrap())
            .expect("the Fork child is catalogued");
        assert_eq!(
            child.fork_provenance(),
            Some(&SessionForkProvenance::new(
                SessionId::from_str(SESSION_ONE).unwrap(),
                ForkSourceKind::RecordedHistory,
                ForkAnchor::AfterUserMessage {
                    item_id: ItemId::from_str("itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
                },
            )),
            "recovery preserves every typed fork-provenance fact"
        );
        state.close().await;
        assert_eq!(fs::read(fork_head).unwrap(), before);
        assert_eq!(
            fs::read(source_conversation_path).unwrap(),
            source_conversation_before
        );
        assert_eq!(
            fs::read(child_conversation_path).unwrap(),
            child_conversation_before
        );

        let g2 = TempRoot::existing();
        create_marked_empty_store(g2.path());
        create_valid_g1_agent(g2.path());
        create_valid_g1_session(g2.path(), b"arbitrary");
        let generation = session_path(g2.path(), SESSION_ONE)
            .join("generations")
            .join(GENERATION_TWO);
        create_directory(&generation);
        create_file(
            &generation.join("head.json"),
            session_head_definition_g2_fixture(),
        );
        let g2_definition = session_definition_g2_fixture();
        create_file(&generation.join("definition.json"), &g2_definition);
        create_file(&generation.join("COMMITTED"), b"");
        let state = open(g2.path())
            .await
            .expect("a valid committed ordinary G2 definition is recovered");
        let head = state
            .session_head(SessionId::from_str(SESSION_ONE).unwrap())
            .expect("the G2 Session head is catalogued");
        assert_eq!(head.storage_generation().get(), 2);
        assert_eq!(
            state
                .session_current_definition(SessionId::from_str(SESSION_ONE).unwrap())
                .unwrap()
                .revision()
                .get(),
            2
        );
        state.close().await;

        let markerless = TempRoot::existing();
        create_marked_empty_store(markerless.path());
        create_valid_g1_agent(markerless.path());
        create_valid_g1_session(markerless.path(), b"arbitrary");
        let generation = session_path(markerless.path(), SESSION_ONE)
            .join("generations")
            .join(GENERATION_TWO);
        create_directory(&generation);
        create_file(
            &generation.join("head.json"),
            b"markerless partial JSON is never decoded",
        );
        let before = fs::read(generation.join("head.json")).unwrap();
        let conversation = session_path(markerless.path(), SESSION_ONE).join("conversation.jsonl");
        let conversation_before = fs::read(&conversation).unwrap();
        let state = open(markerless.path())
            .await
            .expect("a published Session's trailing staging is deferred-cleaned");
        assert_eq!(
            state
                .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            1
        );
        assert!(!generation.exists());
        assert_eq!(before, b"markerless partial JSON is never decoded");
        assert_eq!(fs::read(&conversation).unwrap(), conversation_before);
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authoritative_genesis_and_live_snapshot_fork_g1s_recover_with_exact_provenance() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_valid_g1_session(root.path(), b"source conversation is opaque");
        let genesis_definition = genesis_fork_session_definition_fixture();
        create_exact_g1_session(
            root.path(),
            SESSION_THREE,
            genesis_fork_session_head_fixture(),
            &genesis_definition,
            b"genesis child conversation is opaque",
        );
        create_fork_g1_session(
            root.path(),
            SESSION_TWO,
            SESSION_ONE,
            "live_snapshot",
            r#"{"type":"before_final_agent_message","data":{"itemId":"itm_cccccccccccccccccccccccccccccccc"}}"#,
            b"live-snapshot child conversation is opaque",
        );

        let state = open(root.path())
            .await
            .expect("authoritative Genesis and canonical LiveSnapshot Forks recover");
        let source = SessionId::from_str(SESSION_ONE).unwrap();
        assert_eq!(
            state
                .session_head(SessionId::from_str(SESSION_THREE).unwrap())
                .unwrap()
                .fork_provenance(),
            Some(&SessionForkProvenance::new(
                source,
                ForkSourceKind::RecordedHistory,
                ForkAnchor::Genesis,
            ))
        );
        assert_eq!(
            state
                .session_head(SessionId::from_str(SESSION_TWO).unwrap())
                .unwrap()
                .fork_provenance(),
            Some(&SessionForkProvenance::new(
                source,
                ForkSourceKind::LiveSnapshot,
                ForkAnchor::BeforeFinalAgentMessage {
                    item_id: ItemId::from_str("itm_cccccccccccccccccccccccccccccccc").unwrap(),
                },
            )),
            "Store open retains a typed anchor without relating it to opaque source bytes"
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_source_must_be_a_published_session_catalog_entry() {
        for source_reservation_exists in [false, true] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            if source_reservation_exists {
                create_file(
                    &root
                        .path()
                        .join(RESERVATIONS_DIRECTORY)
                        .join(SESSIONS_DIRECTORY)
                        .join(SESSION_ONE),
                    b"",
                );
            }
            create_fork_g1_session(
                root.path(),
                SESSION_TWO,
                SESSION_ONE,
                "recorded_history",
                r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                &valid_fork_conversation(SESSION_TWO),
            );

            assert_corrupt(open(root.path()).await);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nested_fork_child_sorting_before_its_source_does_not_affect_post_pass_recovery() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_valid_g1_session(root.path(), &valid_ordinary_conversation(SESSION_ONE));
        create_fork_g1_session(
            root.path(),
            SESSION_TWO,
            SESSION_ONE,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            &valid_fork_conversation(SESSION_TWO),
        );
        create_fork_g1_session(
            root.path(),
            SESSION_BEFORE_SOURCE,
            SESSION_TWO,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            &valid_fork_conversation(SESSION_BEFORE_SOURCE),
        );

        let state = open(root.path())
            .await
            .expect("a nested Fork child may sort before its published source");
        for session_id in [SESSION_BEFORE_SOURCE, SESSION_TWO] {
            assert!(
                state
                    .session_head(SessionId::from_str(session_id).unwrap())
                    .is_some()
            );
        }
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_sources_may_currently_be_archived_or_deleted() {
        for (name, deleted) in [("archived", false), ("deleted", true)] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_valid_g1_session(root.path(), b"source conversation is opaque");
            create_session_generation(
                root.path(),
                SESSION_ONE,
                GENERATION_TWO,
                session_head_archived_g2_fixture(),
                None,
            );
            if deleted {
                create_session_generation(
                    root.path(),
                    SESSION_ONE,
                    GENERATION_THREE,
                    session_head_deleted_g3_fixture(),
                    None,
                );
            }
            create_fork_g1_session(
                root.path(),
                SESSION_TWO,
                SESSION_ONE,
                "recorded_history",
                r#"{"type":"genesis"}"#,
                b"child conversation is opaque",
            );

            let state = open(root.path()).await.unwrap_or_else(|_| {
                panic!("a {name} source remains a valid recovered Fork target")
            });
            assert!(
                state
                    .session_head(SessionId::from_str(SESSION_TWO).unwrap())
                    .is_some()
            );
            state.close().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_provenance_cycles_are_rejected_iteratively() {
        let two_node = TempRoot::existing();
        create_marked_empty_store(two_node.path());
        create_valid_g1_agent(two_node.path());
        create_fork_g1_session(
            two_node.path(),
            SESSION_ONE,
            SESSION_TWO,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            &valid_fork_conversation(SESSION_ONE),
        );
        create_fork_g1_session(
            two_node.path(),
            SESSION_TWO,
            SESSION_ONE,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            &valid_fork_conversation(SESSION_TWO),
        );
        assert_corrupt(open(two_node.path()).await);

        let three_node = TempRoot::existing();
        create_marked_empty_store(three_node.path());
        create_valid_g1_agent(three_node.path());
        create_fork_g1_session(
            three_node.path(),
            SESSION_ONE,
            SESSION_TWO,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            &valid_fork_conversation(SESSION_ONE),
        );
        create_fork_g1_session(
            three_node.path(),
            SESSION_TWO,
            SESSION_THREE,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            &valid_fork_conversation(SESSION_TWO),
        );
        create_fork_g1_session(
            three_node.path(),
            SESSION_THREE,
            SESSION_ONE,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            &valid_fork_conversation(SESSION_THREE),
        );
        assert_corrupt(open(three_node.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_definition_metadata_and_lifecycle_generations_preserve_exact_provenance() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_valid_g1_session(root.path(), b"source conversation is opaque");
        create_fork_g1_session(
            root.path(),
            SESSION_TWO,
            SESSION_ONE,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
            b"child conversation is opaque",
        );
        let provenance = fork_provenance_json(
            SESSION_ONE,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
        );
        let definition_g2_head = replace_fixture(
            &fork_g2_head_from(
                session_head_definition_g2_fixture(),
                SESSION_TWO,
                Some(&provenance),
            ),
            r#""updatedAt":"2026-08-03T10:01:00.456Z"#,
            r#""updatedAt":"2026-08-03T10:02:00.789Z"#,
        );
        let definition_g2 =
            replace_fixture(&session_definition_g2_fixture(), SESSION_ONE, SESSION_TWO);
        create_session_generation(
            root.path(),
            SESSION_TWO,
            GENERATION_TWO,
            &definition_g2_head,
            Some(&definition_g2),
        );
        let metadata_g3_head = replace_fixture(
            &replace_fixture(
                &fork_g2_head_from(
                    session_head_metadata_g2_fixture(),
                    SESSION_TWO,
                    Some(&provenance),
                ),
                r#""storageGeneration":2,"previousStorageGeneration":1"#,
                r#""storageGeneration":3,"previousStorageGeneration":2"#,
            ),
            r#""currentDefinition":{"revision":"sdr_1","storageGeneration":1}"#,
            r#""currentDefinition":{"revision":"sdr_2","storageGeneration":2}"#,
        );
        create_session_generation(
            root.path(),
            SESSION_TWO,
            GENERATION_THREE,
            &metadata_g3_head,
            None,
        );
        let lifecycle_g4_head = replace_fixture(
            &replace_fixture(
                &metadata_g3_head,
                r#""storageGeneration":3,"previousStorageGeneration":2"#,
                r#""storageGeneration":4,"previousStorageGeneration":3"#,
            ),
            r#""lifecycle":"open""#,
            r#""lifecycle":"archived""#,
        );
        create_session_generation(
            root.path(),
            SESSION_TWO,
            GENERATION_FOUR,
            &lifecycle_g4_head,
            None,
        );

        let state = open(root.path())
            .await
            .expect("all later Fork categories retain exact immutable provenance");
        let child = state
            .session_head(SessionId::from_str(SESSION_TWO).unwrap())
            .unwrap();
        assert_eq!(child.storage_generation().get(), 4);
        assert_eq!(child.current_definition_revision().get(), 2);
        assert_eq!(child.metadata().revision().get(), 2);
        assert_eq!(child.lifecycle(), SessionLifecycle::Archived);
        assert_eq!(
            child.fork_provenance(),
            Some(&SessionForkProvenance::new(
                SessionId::from_str(SESSION_ONE).unwrap(),
                ForkSourceKind::RecordedHistory,
                ForkAnchor::AfterUserMessage {
                    item_id: ItemId::from_str("itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
                },
            ))
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn later_session_generations_require_exact_typed_fork_provenance_equality() {
        let original = fork_provenance_json(
            SESSION_ONE,
            "recorded_history",
            r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
        );
        let cases = [
            ("Some to None", None, false),
            (
                "source SessionId",
                Some(fork_provenance_json(
                    SESSION_THREE,
                    "recorded_history",
                    r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                )),
                true,
            ),
            (
                "source kind",
                Some(fork_provenance_json(
                    SESSION_ONE,
                    "live_snapshot",
                    r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                )),
                false,
            ),
            (
                "anchor variant",
                Some(fork_provenance_json(
                    SESSION_ONE,
                    "recorded_history",
                    r#"{"type":"before_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                )),
                false,
            ),
            (
                "anchor itemId",
                Some(fork_provenance_json(
                    SESSION_ONE,
                    "recorded_history",
                    r#"{"type":"after_user_message","data":{"itemId":"itm_cccccccccccccccccccccccccccccccc"}}"#,
                )),
                false,
            ),
        ];

        for (name, provenance, needs_second_source) in cases {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_valid_g1_session(root.path(), &valid_ordinary_conversation(SESSION_ONE));
            if needs_second_source {
                create_ordinary_g1_session(
                    root.path(),
                    SESSION_THREE,
                    &valid_ordinary_conversation(SESSION_THREE),
                );
            }
            create_fork_g1_session(
                root.path(),
                SESSION_TWO,
                SESSION_ONE,
                "recorded_history",
                r#"{"type":"after_user_message","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                &valid_fork_conversation(SESSION_TWO),
            );
            let head = replace_fixture(
                &fork_g2_head_from(
                    session_head_archived_g2_fixture(),
                    SESSION_TWO,
                    provenance.as_deref(),
                ),
                r#""updatedAt":"2026-08-03T10:01:00.456Z"#,
                r#""updatedAt":"2026-08-03T10:02:00.789Z"#,
            );
            create_session_generation(root.path(), SESSION_TWO, GENERATION_TWO, &head, None);

            assert!(
                matches!(
                    open(root.path()).await,
                    Err(DurableOpenError::DurableStateCorrupt)
                ),
                "Fork provenance {name} mutation is corrupt"
            );
        }

        let ordinary = TempRoot::existing();
        create_marked_empty_store(ordinary.path());
        create_valid_g1_agent(ordinary.path());
        create_valid_g1_session(ordinary.path(), &valid_ordinary_conversation(SESSION_ONE));
        create_ordinary_g1_session(
            ordinary.path(),
            SESSION_TWO,
            &valid_ordinary_conversation(SESSION_TWO),
        );
        let none_to_some = replace_fixture(
            &replace_fixture(session_head_archived_g2_fixture(), SESSION_ONE, SESSION_TWO),
            r#""forkProvenance":null"#,
            &format!(r#""forkProvenance":{original}"#),
        );
        create_session_generation(
            ordinary.path(),
            SESSION_TWO,
            GENERATION_TWO,
            &none_to_some,
            None,
        );
        assert_corrupt(open(ordinary.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_g1_metadata_must_be_empty() {
        for (name, from, to) in [
            ("name", r#""name":null"#, r#""name":"Fork child"#),
            (
                "description",
                r#""description":null"#,
                r#""description":"Fork child description"#,
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_valid_g1_session(root.path(), &valid_ordinary_conversation(SESSION_ONE));
            let head = replace_fixture(fork_session_head_fixture(), from, to);
            let definition = fork_session_definition_fixture();
            create_exact_g1_session(
                root.path(),
                SESSION_TWO,
                &head,
                &definition,
                &valid_fork_conversation(SESSION_TWO),
            );

            assert!(
                matches!(
                    open(root.path()).await,
                    Err(DurableOpenError::DurableStateCorrupt)
                ),
                "Fork G1 {name} must remain null"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authoritative_ordinary_session_definition_metadata_and_lifecycle_chains_recover() {
        let definition = TempRoot::existing();
        create_marked_empty_store(definition.path());
        create_valid_g1_agent(definition.path());
        create_valid_g1_session(definition.path(), b"conversation remains opaque");
        let definition_g2 = session_definition_g2_fixture();
        create_session_generation(
            definition.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_definition_g2_fixture(),
            Some(&definition_g2),
        );
        let state = open(definition.path())
            .await
            .expect("the authoritative model definition G2 chain opens");
        let session_id = SessionId::from_str(SESSION_ONE).unwrap();
        assert_eq!(
            state
                .session_head(session_id)
                .unwrap()
                .storage_generation()
                .get(),
            2
        );
        assert_eq!(
            state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            2
        );
        state.close().await;

        let direct_metadata = TempRoot::existing();
        create_marked_empty_store(direct_metadata.path());
        create_valid_g1_agent(direct_metadata.path());
        create_valid_g1_session(direct_metadata.path(), b"conversation remains opaque");
        create_session_generation(
            direct_metadata.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_metadata_g2_fixture(),
            None,
        );
        let state = open(direct_metadata.path())
            .await
            .expect("the authoritative metadata G2 chain opens");
        assert_eq!(
            state
                .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                .unwrap()
                .metadata()
                .revision()
                .get(),
            2
        );
        state.close().await;

        let same_metadata_timestamp = TempRoot::existing();
        create_marked_empty_store(same_metadata_timestamp.path());
        create_valid_g1_agent(same_metadata_timestamp.path());
        create_valid_g1_session(
            same_metadata_timestamp.path(),
            b"conversation remains opaque",
        );
        let unchanged_timestamp_metadata = replace_fixture(
            session_head_metadata_g2_fixture(),
            "\"updatedAt\":\"2026-08-03T10:01:05.000Z\"",
            "\"updatedAt\":\"2026-08-03T10:01:00.456Z\"",
        );
        create_session_generation(
            same_metadata_timestamp.path(),
            SESSION_ONE,
            GENERATION_TWO,
            &unchanged_timestamp_metadata,
            None,
        );
        let state = open(same_metadata_timestamp.path())
            .await
            .expect("changed metadata may retain its millisecond-truncated timestamp");
        state.close().await;

        let workspace = TempRoot::existing();
        create_marked_empty_store(workspace.path());
        create_valid_g1_agent(workspace.path());
        create_valid_g1_session(workspace.path(), b"conversation remains opaque");
        let workspace_g2 = session_definition_workspace_g2_fixture();
        create_session_generation(
            workspace.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_workspace_definition_g2_fixture(),
            Some(&workspace_g2),
        );
        let state = open(workspace.path())
            .await
            .expect("the authoritative Workspace-changing definition G2 chain opens");
        assert_eq!(
            state
                .session_current_definition(SessionId::from_str(SESSION_ONE).unwrap())
                .unwrap()
                .workspace()
                .revision()
                .get(),
            2
        );
        state.close().await;

        let metadata = TempRoot::existing();
        create_marked_empty_store(metadata.path());
        create_valid_g1_agent(metadata.path());
        create_valid_g1_session(metadata.path(), b"conversation remains opaque");
        let definition_g2 = session_definition_g2_fixture();
        create_session_generation(
            metadata.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_definition_g2_fixture(),
            Some(&definition_g2),
        );
        let metadata_g3 = replace_fixture(
            &replace_fixture(
                session_head_metadata_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":3,\"previousStorageGeneration\":2",
            ),
            "\"currentDefinition\":{\"revision\":\"sdr_1\",\"storageGeneration\":1}",
            "\"currentDefinition\":{\"revision\":\"sdr_2\",\"storageGeneration\":2}",
        );
        create_session_generation(
            metadata.path(),
            SESSION_ONE,
            GENERATION_THREE,
            &metadata_g3,
            None,
        );
        let state = open(metadata.path())
            .await
            .expect("metadata after a definition retains the current definition");
        let session_id = SessionId::from_str(SESSION_ONE).unwrap();
        assert_eq!(
            state
                .session_head(session_id)
                .unwrap()
                .storage_generation()
                .get(),
            3
        );
        assert_eq!(
            state
                .session_current_definition(session_id)
                .unwrap()
                .revision()
                .get(),
            2
        );
        state.close().await;

        for (name, final_head, lifecycle) in [
            (
                "unarchive",
                session_head_unarchived_g3_fixture(),
                SessionLifecycle::Open,
            ),
            (
                "delete",
                session_head_deleted_g3_fixture(),
                SessionLifecycle::Deleted,
            ),
        ] {
            let lifecycle_root = TempRoot::existing();
            create_marked_empty_store(lifecycle_root.path());
            create_valid_g1_agent(lifecycle_root.path());
            create_valid_g1_session(lifecycle_root.path(), b"conversation remains opaque");
            // Materialize G3 before G2: recovery must sort canonical numeric generations.
            create_session_generation(
                lifecycle_root.path(),
                SESSION_ONE,
                GENERATION_THREE,
                final_head,
                None,
            );
            create_session_generation(
                lifecycle_root.path(),
                SESSION_ONE,
                GENERATION_TWO,
                session_head_archived_g2_fixture(),
                None,
            );
            let state = open(lifecycle_root.path())
                .await
                .unwrap_or_else(|_| panic!("the authoritative {name} G3 chain opens"));
            let head = state
                .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                .unwrap();
            assert_eq!(head.storage_generation().get(), 3);
            assert_eq!(head.lifecycle(), lifecycle);
            state.close().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_definition_changes_allow_same_agent_retained_revision_upgrade_and_rollback() {
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
        create_valid_g1_session(root.path(), b"conversation remains opaque");

        let upgrade = replace_fixture(
            &replace_fixture(
                &replace_fixture(
                    &session_definition_fixture(),
                    "\"revision\":\"sdr_1\"",
                    "\"revision\":\"sdr_2\"",
                ),
                "\"revision\":\"ar_1\"",
                "\"revision\":\"ar_2\"",
            ),
            "\"createdAt\":\"2026-08-03T10:01:00.456Z\"",
            "\"createdAt\":\"2026-08-03T10:01:01.000Z\"",
        );
        create_session_generation(
            root.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_definition_g2_fixture(),
            Some(&upgrade),
        );
        let state = open(root.path())
            .await
            .expect("a retained same-Agent revision may be pinned directly");
        let upgraded = state
            .session_current_definition(SessionId::from_str(SESSION_ONE).unwrap())
            .unwrap();
        assert_eq!(upgraded.revision().get(), 2);
        assert_eq!(upgraded.agent().revision().get(), 2);
        state.close().await;

        let rollback_head = replace_fixture(
            &replace_fixture(
                session_head_definition_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":3,\"previousStorageGeneration\":2",
            ),
            "\"currentDefinition\":{\"revision\":\"sdr_2\",\"storageGeneration\":2}",
            "\"currentDefinition\":{\"revision\":\"sdr_3\",\"storageGeneration\":3}",
        );
        let rollback_definition = replace_fixture(
            &session_definition_fixture(),
            "\"revision\":\"sdr_1\"",
            "\"revision\":\"sdr_3\"",
        );
        // Definition timestamps are wall-clock facts, not monotonic generation-order proofs.
        create_session_generation(
            root.path(),
            SESSION_ONE,
            GENERATION_THREE,
            &rollback_head,
            Some(&rollback_definition),
        );

        let state = open(root.path())
            .await
            .expect("same-Agent retained revision upgrade and rollback are legal");
        let definition = state
            .session_current_definition(SessionId::from_str(SESSION_ONE).unwrap())
            .unwrap();
        assert_eq!(definition.revision().get(), 3);
        assert_eq!(definition.agent().revision().get(), 1);
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_session_definition_may_reference_retained_agent_content_when_current_agent_is_disabled_or_deleted()
     {
        for (name, agent_head) in [
            ("disabled", agent_head_status_g2_fixture().to_vec()),
            (
                "deleted",
                replace_fixture(
                    agent_head_status_g2_fixture(),
                    "\"status\":\"disabled\"",
                    "\"status\":\"deleted\"",
                ),
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            create_agent_generation(root.path(), AGENT_ONE, GENERATION_TWO, &agent_head, None);
            create_valid_g1_session(root.path(), b"conversation remains opaque");
            let definition = session_definition_g2_fixture();
            create_session_generation(
                root.path(),
                SESSION_ONE,
                GENERATION_TWO,
                session_head_definition_g2_fixture(),
                Some(&definition),
            );

            let state = open(root.path())
                .await
                .unwrap_or_else(|_| panic!("a retained Agent definition remains usable in {name}"));
            assert_eq!(
                state
                    .session_current_definition(SessionId::from_str(SESSION_ONE).unwrap())
                    .unwrap()
                    .revision()
                    .get(),
                2
            );
            state.close().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_session_definition_and_workspace_no_ops_mismatches_and_cross_agent_rebinds_are_corrupt()
     {
        let definition_no_op = replace_fixture(
            &session_definition_g2_fixture(),
            "\"maxOutputTokens\":8192",
            "\"maxOutputTokens\":4096",
        );
        assert_ordinary_session_g2_is_corrupt(
            session_head_definition_g2_fixture(),
            Some(&definition_no_op),
        )
        .await;

        let workspace_unchanged_bumped = replace_fixture(
            &session_definition_g2_fixture(),
            "\"revision\":\"wr_1\"",
            "\"revision\":\"wr_2\"",
        );
        assert_ordinary_session_g2_is_corrupt(
            session_head_definition_g2_fixture(),
            Some(&workspace_unchanged_bumped),
        )
        .await;

        let workspace_changed_same_revision = replace_fixture(
            &session_definition_workspace_g2_fixture(),
            "\"revision\":\"wr_2\"",
            "\"revision\":\"wr_1\"",
        );
        assert_ordinary_session_g2_is_corrupt(
            session_head_workspace_definition_g2_fixture(),
            Some(&workspace_changed_same_revision),
        )
        .await;

        let workspace_changed_jump = replace_fixture(
            &session_definition_workspace_g2_fixture(),
            "\"revision\":\"wr_2\"",
            "\"revision\":\"wr_3\"",
        );
        assert_ordinary_session_g2_is_corrupt(
            session_head_workspace_definition_g2_fixture(),
            Some(&workspace_changed_jump),
        )
        .await;

        let missing_exact_agent_revision = replace_fixture(
            &session_definition_g2_fixture(),
            "\"revision\":\"ar_1\"",
            "\"revision\":\"ar_2\"",
        );
        assert_ordinary_session_g2_is_corrupt(
            session_head_definition_g2_fixture(),
            Some(&missing_exact_agent_revision),
        )
        .await;

        let cross_agent = TempRoot::existing();
        create_marked_empty_store(cross_agent.path());
        create_valid_g1_agent(cross_agent.path());
        create_exact_g1_agent(
            cross_agent.path(),
            AGENT_TWO,
            &replace_fixture(agent_head_fixture(), AGENT_ONE, AGENT_TWO),
            &replace_fixture(agent_definition_fixture(), AGENT_ONE, AGENT_TWO),
        );
        create_valid_g1_session(cross_agent.path(), b"conversation remains opaque");
        let rebind = replace_fixture(&session_definition_g2_fixture(), AGENT_ONE, AGENT_TWO);
        create_session_generation(
            cross_agent.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_definition_g2_fixture(),
            Some(&rebind),
        );
        assert_corrupt(open(cross_agent.path()).await);

        let mixed_definition_lifecycle = replace_fixture(
            session_head_definition_g2_fixture(),
            "\"lifecycle\":\"open\"",
            "\"lifecycle\":\"archived\"",
        );
        let definition = session_definition_g2_fixture();
        assert_ordinary_session_g2_is_corrupt(&mixed_definition_lifecycle, Some(&definition)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_session_metadata_and_lifecycle_no_ops_mixed_changes_and_invalid_edges_are_corrupt()
     {
        let metadata_no_op = replace_fixture(
            session_head_metadata_g2_fixture(),
            "\"name\":\"Project session\"",
            "\"name\":null",
        );
        assert_ordinary_session_g2_is_corrupt(&metadata_no_op, None).await;

        let metadata_revision_jump = replace_fixture(
            session_head_metadata_g2_fixture(),
            "\"revision\":\"smr_2\"",
            "\"revision\":\"smr_3\"",
        );
        assert_ordinary_session_g2_is_corrupt(&metadata_revision_jump, None).await;

        let mixed_metadata_lifecycle = replace_fixture(
            session_head_metadata_g2_fixture(),
            "\"lifecycle\":\"open\"",
            "\"lifecycle\":\"archived\"",
        );
        assert_ordinary_session_g2_is_corrupt(&mixed_metadata_lifecycle, None).await;

        let same_lifecycle = replace_fixture(
            session_head_archived_g2_fixture(),
            "\"lifecycle\":\"archived\"",
            "\"lifecycle\":\"open\"",
        );
        assert_ordinary_session_g2_is_corrupt(&same_lifecycle, None).await;

        let open_to_deleted = replace_fixture(
            session_head_archived_g2_fixture(),
            "\"lifecycle\":\"archived\"",
            "\"lifecycle\":\"deleted\"",
        );
        assert_ordinary_session_g2_is_corrupt(&open_to_deleted, None).await;

        let archived_definition = TempRoot::existing();
        create_marked_empty_store(archived_definition.path());
        create_valid_g1_agent(archived_definition.path());
        create_valid_g1_session(archived_definition.path(), b"conversation remains opaque");
        create_session_generation(
            archived_definition.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_archived_g2_fixture(),
            None,
        );
        let archived_definition_head = replace_fixture(
            &replace_fixture(
                session_head_definition_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":3,\"previousStorageGeneration\":2",
            ),
            "\"lifecycle\":\"open\"",
            "\"lifecycle\":\"archived\"",
        );
        let definition = session_definition_g2_fixture();
        create_session_generation(
            archived_definition.path(),
            SESSION_ONE,
            GENERATION_THREE,
            &archived_definition_head,
            Some(&definition),
        );
        assert_corrupt(open(archived_definition.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deleted_ordinary_session_rejects_every_later_category() {
        let definition = TempRoot::existing();
        create_marked_empty_store(definition.path());
        create_ordinary_session_chain_through_g3_deleted(definition.path());
        let definition_head = replace_fixture(
            &replace_fixture(
                &replace_fixture(
                    session_head_definition_g2_fixture(),
                    "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                    "\"storageGeneration\":4,\"previousStorageGeneration\":3",
                ),
                "\"currentDefinition\":{\"revision\":\"sdr_2\",\"storageGeneration\":2}",
                "\"currentDefinition\":{\"revision\":\"sdr_2\",\"storageGeneration\":4}",
            ),
            "\"lifecycle\":\"open\"",
            "\"lifecycle\":\"deleted\"",
        );
        let definition_payload = session_definition_g2_fixture();
        create_session_generation(
            definition.path(),
            SESSION_ONE,
            GENERATION_FOUR,
            &definition_head,
            Some(&definition_payload),
        );
        assert_corrupt(open(definition.path()).await);

        let metadata = TempRoot::existing();
        create_marked_empty_store(metadata.path());
        create_ordinary_session_chain_through_g3_deleted(metadata.path());
        let metadata_head = replace_fixture(
            &replace_fixture(
                session_head_metadata_g2_fixture(),
                "\"storageGeneration\":2,\"previousStorageGeneration\":1",
                "\"storageGeneration\":4,\"previousStorageGeneration\":3",
            ),
            "\"lifecycle\":\"open\"",
            "\"lifecycle\":\"deleted\"",
        );
        create_session_generation(
            metadata.path(),
            SESSION_ONE,
            GENERATION_FOUR,
            &metadata_head,
            None,
        );
        assert_corrupt(open(metadata.path()).await);

        let lifecycle = TempRoot::existing();
        create_marked_empty_store(lifecycle.path());
        create_ordinary_session_chain_through_g3_deleted(lifecycle.path());
        let lifecycle_head = replace_fixture(
            session_head_deleted_g3_fixture(),
            "\"storageGeneration\":3,\"previousStorageGeneration\":2",
            "\"storageGeneration\":4,\"previousStorageGeneration\":3",
        );
        create_session_generation(
            lifecycle.path(),
            SESSION_ONE,
            GENERATION_FOUR,
            &lifecycle_head,
            None,
        );
        assert_corrupt(open(lifecycle.path()).await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_session_chain_payload_provenance_generation_and_pointer_mismatches_fail_closed_without_fallback()
     {
        assert_ordinary_session_g2_is_corrupt(session_head_definition_g2_fixture(), None).await;
        let unexpected_definition = session_definition_g2_fixture();
        assert_ordinary_session_g2_is_corrupt(
            session_head_metadata_g2_fixture(),
            Some(&unexpected_definition),
        )
        .await;

        let storage_generation_mismatch = replace_fixture(
            session_head_archived_g2_fixture(),
            "\"storageGeneration\":2,\"previousStorageGeneration\":1",
            "\"storageGeneration\":3,\"previousStorageGeneration\":2",
        );
        assert_ordinary_session_g2_is_corrupt(&storage_generation_mismatch, None).await;

        let pointer_mismatch = replace_fixture(
            session_head_definition_g2_fixture(),
            "\"currentDefinition\":{\"revision\":\"sdr_2\",\"storageGeneration\":2}",
            "\"currentDefinition\":{\"revision\":\"sdr_2\",\"storageGeneration\":1}",
        );
        let definition = session_definition_g2_fixture();
        assert_ordinary_session_g2_is_corrupt(&pointer_mismatch, Some(&definition)).await;

        let revision_mismatch = replace_fixture(
            session_head_definition_g2_fixture(),
            "\"currentDefinition\":{\"revision\":\"sdr_2\",\"storageGeneration\":2}",
            "\"currentDefinition\":{\"revision\":\"sdr_3\",\"storageGeneration\":2}",
        );
        let revision_payload = replace_fixture(
            &session_definition_g2_fixture(),
            "\"revision\":\"sdr_2\"",
            "\"revision\":\"sdr_3\"",
        );
        assert_ordinary_session_g2_is_corrupt(&revision_mismatch, Some(&revision_payload)).await;

        let g2_alone = TempRoot::existing();
        create_marked_empty_store(g2_alone.path());
        create_valid_g1_agent(g2_alone.path());
        create_valid_g1_session(g2_alone.path(), b"conversation remains opaque");
        fs::rename(
            session_generation_path(g2_alone.path(), SESSION_ONE),
            session_generation_path_named(g2_alone.path(), SESSION_ONE, GENERATION_TWO),
        )
        .unwrap();
        assert_corrupt(open(g2_alone.path()).await);

        let gap = TempRoot::existing();
        create_marked_empty_store(gap.path());
        create_valid_g1_agent(gap.path());
        create_valid_g1_session(gap.path(), b"conversation remains opaque");
        create_session_generation(
            gap.path(),
            SESSION_ONE,
            GENERATION_THREE,
            session_head_unarchived_g3_fixture(),
            None,
        );
        assert_corrupt(open(gap.path()).await);

        let corrupt_highest = TempRoot::existing();
        create_marked_empty_store(corrupt_highest.path());
        create_valid_g1_agent(corrupt_highest.path());
        create_valid_g1_session(corrupt_highest.path(), b"conversation remains opaque");
        create_session_generation(
            corrupt_highest.path(),
            SESSION_ONE,
            GENERATION_TWO,
            b"not canonical JSON\n",
            None,
        );
        assert_corrupt(open(corrupt_highest.path()).await);

        let corrupt_after_valid_generation = TempRoot::existing();
        create_marked_empty_store(corrupt_after_valid_generation.path());
        create_valid_g1_agent(corrupt_after_valid_generation.path());
        create_valid_g1_session(
            corrupt_after_valid_generation.path(),
            b"conversation remains opaque",
        );
        create_session_generation(
            corrupt_after_valid_generation.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_archived_g2_fixture(),
            None,
        );
        create_session_generation(
            corrupt_after_valid_generation.path(),
            SESSION_ONE,
            GENERATION_THREE,
            b"not canonical JSON\n",
            None,
        );
        assert_corrupt(open(corrupt_after_valid_generation.path()).await);

        let provenance = TempRoot::existing();
        create_marked_empty_store(provenance.path());
        create_valid_g1_agent(provenance.path());
        create_valid_g1_session(provenance.path(), b"conversation remains opaque");
        let intermediate_provenance = replace_fixture(
            session_head_archived_g2_fixture(),
            "\"forkProvenance\":null",
            "\"forkProvenance\":{\"sourceSessionId\":\"ses_33333333333333333333333333333333\",\"source\":\"recorded_history\",\"anchor\":{\"type\":\"genesis\"}}",
        );
        create_session_generation(
            provenance.path(),
            SESSION_ONE,
            GENERATION_TWO,
            &intermediate_provenance,
            None,
        );
        create_session_generation(
            provenance.path(),
            SESSION_ONE,
            GENERATION_THREE,
            session_head_unarchived_g3_fixture(),
            None,
        );
        let final_head =
            session_generation_path_named(provenance.path(), SESSION_ONE, GENERATION_THREE)
                .join("head.json");
        let before = fs::read(&final_head).unwrap();
        assert_corrupt(open(provenance.path()).await);
        assert_eq!(fs::read(final_head).unwrap(), before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_collection_names_and_small_caps_precede_shape_classification() {
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
    fn all_entity_namespaces_are_inventoried_before_generation_semantics() {
        let later_session_collection_cap = TempRoot::existing();
        create_marked_empty_store(later_session_collection_cap.path());
        create_valid_g1_agent(later_session_collection_cap.path());
        create_file(
            &generation_path(later_session_collection_cap.path(), AGENT_ONE).join("head.json"),
            b"corrupt Agent head\n",
        );
        let sessions = later_session_collection_cap.path().join(SESSIONS_DIRECTORY);
        create_file(&sessions.join("invalid-one"), b"");
        create_file(&sessions.join("invalid-two"), b"");
        let caps = RecoveryCaps {
            agent_reservations: 1,
            session_reservations: 1,
            root_agents: 1,
            root_sessions: 1,
            generations: 1,
        };
        assert!(matches!(
            recover_marked_root_with_caps(
                later_session_collection_cap.path(),
                &marked_entries(later_session_collection_cap.path()),
                caps,
            ),
            Err(DurableOpenError::DurableStateTooLarge)
        ));

        let later_session_entity_cap = TempRoot::existing();
        create_marked_empty_store(later_session_entity_cap.path());
        create_valid_g1_agent(later_session_entity_cap.path());
        create_valid_g1_session(later_session_entity_cap.path(), b"arbitrary");
        create_file(
            &session_generation_path(later_session_entity_cap.path(), SESSION_ONE)
                .join("head.json"),
            &replace_fixture(
                session_head_fixture(),
                "\"lifecycle\":\"open\"",
                "\"lifecycle\":\"archived\"",
            ),
        );
        create_file(
            &later_session_entity_cap
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_TWO),
            b"",
        );
        let second_entity = session_path(later_session_entity_cap.path(), SESSION_TWO);
        create_directory(&second_entity);
        for name in ["one", "two", "three", "four"] {
            create_file(&second_entity.join(name), b"");
        }
        assert!(matches!(
            recover_marked_root_with_caps(
                later_session_entity_cap.path(),
                &marked_entries(later_session_entity_cap.path()),
                RecoveryCaps::PRODUCTION,
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

    #[tokio::test(flavor = "current_thread")]
    async fn published_agent_all_markerless_payload_subsets_cleanup_and_reopen() {
        for (name, has_head, has_definition) in [
            ("empty", false, false),
            ("head only", true, false),
            ("definition only", false, true),
            ("both", true, true),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            let staging = create_markerless_agent_generation(
                root.path(),
                AGENT_ONE,
                GENERATION_TWO,
                has_head.then_some(b"partial Agent head JSON".as_slice()),
                has_definition.then_some(b"partial Agent definition JSON".as_slice()),
            );
            let reservation = root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE);
            let published = agent_path(root.path(), AGENT_ONE).join("PUBLISHED");
            let committed = generation_path(root.path(), AGENT_ONE);

            let state = open(root.path())
                .await
                .unwrap_or_else(|error| panic!("{name} trailing Agent subset opens: {error:?}"));
            assert_eq!(
                state
                    .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                    .unwrap()
                    .storage_generation()
                    .get(),
                1
            );
            assert!(
                !staging.exists(),
                "{name} staging is gone before catalog install"
            );
            assert!(reservation.is_file(), "the permanent reservation survives");
            assert!(published.is_file(), "PUBLISHED survives");
            assert!(
                committed.join("COMMITTED").is_file(),
                "G1 remains committed"
            );
            state.close().await;

            let reopened = open(root.path()).await.unwrap_or_else(|error| {
                panic!("{name} cleanup leaves a reopenable store: {error:?}")
            });
            assert_eq!(
                reopened
                    .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                    .unwrap()
                    .storage_generation()
                    .get(),
                1
            );
            reopened.close().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn published_session_all_markerless_payload_subsets_cleanup_without_touching_conversation()
     {
        for (name, has_head, has_definition) in [
            ("empty", false, false),
            ("head only", true, false),
            ("definition only", false, true),
            ("both", true, true),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            let conversation = b"opaque Session conversation bytes must survive";
            create_valid_g1_session(root.path(), conversation);
            let conversation_path =
                session_path(root.path(), SESSION_ONE).join("conversation.jsonl");
            let before = fs::read(&conversation_path).unwrap();
            let staging = create_markerless_session_generation(
                root.path(),
                SESSION_ONE,
                GENERATION_TWO,
                has_head.then_some(b"partial Session head JSON".as_slice()),
                has_definition.then_some(b"partial Session definition JSON".as_slice()),
            );

            let state = open(root.path())
                .await
                .unwrap_or_else(|error| panic!("{name} trailing Session subset opens: {error:?}"));
            assert_eq!(
                state
                    .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                    .unwrap()
                    .storage_generation()
                    .get(),
                1
            );
            assert!(!staging.exists(), "{name} Session staging is removed");
            assert_eq!(fs::read(&conversation_path).unwrap(), before);
            state.close().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committed_g1_g2_prefix_recovers_old_head_after_g3_staging_for_agent_and_session() {
        let agent_root = TempRoot::existing();
        create_marked_empty_store(agent_root.path());
        create_valid_g1_agent(agent_root.path());
        create_agent_generation(
            agent_root.path(),
            AGENT_ONE,
            GENERATION_TWO,
            agent_head_metadata_g2_fixture(),
            None,
        );
        let agent_staging = create_markerless_agent_generation(
            agent_root.path(),
            AGENT_ONE,
            GENERATION_THREE,
            Some(b"G3 partial Agent head"),
            Some(b"G3 partial Agent definition"),
        );
        let state = open(agent_root.path())
            .await
            .expect("the committed Agent G1/G2 prefix survives G3 staging");
        assert_eq!(
            state
                .agent_head(AgentId::from_str(AGENT_ONE).unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            2
        );
        assert!(!agent_staging.exists());
        assert!(
            generation_path_named(agent_root.path(), AGENT_ONE, GENERATION_TWO)
                .join("COMMITTED")
                .is_file()
        );
        state.close().await;

        let session_root = TempRoot::existing();
        create_marked_empty_store(session_root.path());
        create_valid_g1_agent(session_root.path());
        create_valid_g1_session(session_root.path(), b"opaque committed Session bytes");
        create_session_generation(
            session_root.path(),
            SESSION_ONE,
            GENERATION_TWO,
            session_head_metadata_g2_fixture(),
            None,
        );
        let session_staging = create_markerless_session_generation(
            session_root.path(),
            SESSION_ONE,
            GENERATION_THREE,
            Some(b"G3 partial Session head"),
            None,
        );
        let state = open(session_root.path())
            .await
            .expect("the committed Session G1/G2 prefix survives G3 staging");
        assert_eq!(
            state
                .session_head(SessionId::from_str(SESSION_ONE).unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            2
        );
        assert!(!session_staging.exists());
        assert!(
            session_generation_path_named(session_root.path(), SESSION_ONE, GENERATION_TWO)
                .join("COMMITTED")
                .is_file()
        );
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_conversation_bytes_are_untouched_by_child_markerless_generation_cleanup() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        create_valid_g1_session(root.path(), b"source opaque bytes");
        let child_definition = fork_session_definition_fixture();
        create_exact_g1_session(
            root.path(),
            SESSION_TWO,
            fork_session_head_fixture(),
            &child_definition,
            b"child opaque bytes",
        );
        let source_conversation = session_path(root.path(), SESSION_ONE).join("conversation.jsonl");
        let child_conversation = session_path(root.path(), SESSION_TWO).join("conversation.jsonl");
        let source_before = fs::read(&source_conversation).unwrap();
        let child_before = fs::read(&child_conversation).unwrap();
        let staging = create_markerless_session_generation(
            root.path(),
            SESSION_TWO,
            GENERATION_TWO,
            Some(b"child partial JSON"),
            Some(b"child partial JSON"),
        );

        let state = open(root.path())
            .await
            .expect("the child tail does not make the Fork conversation writable or readable");
        assert!(!staging.exists());
        assert_eq!(fs::read(&source_conversation).unwrap(), source_before);
        assert_eq!(fs::read(&child_conversation).unwrap(), child_before);
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn markerless_generation_order_and_committed_marker_fail_closed_without_cleanup() {
        let gap = TempRoot::existing();
        create_marked_empty_store(gap.path());
        create_valid_g1_agent(gap.path());
        let gap_staging =
            create_markerless_agent_generation(gap.path(), AGENT_ONE, GENERATION_THREE, None, None);
        assert_corrupt(open(gap.path()).await);
        assert!(gap_staging.exists(), "a G3 gap is never removed");

        let multiple = TempRoot::existing();
        create_marked_empty_store(multiple.path());
        create_valid_g1_agent(multiple.path());
        let first = create_markerless_agent_generation(
            multiple.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        let second = create_markerless_agent_generation(
            multiple.path(),
            AGENT_ONE,
            GENERATION_THREE,
            None,
            None,
        );
        assert_corrupt(open(multiple.path()).await);
        assert!(
            first.exists() && second.exists(),
            "multiple tails are never partly removed"
        );

        let after_markerless = TempRoot::existing();
        create_marked_empty_store(after_markerless.path());
        create_valid_g1_agent(after_markerless.path());
        let staging = create_markerless_agent_generation(
            after_markerless.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        create_agent_generation(
            after_markerless.path(),
            AGENT_ONE,
            GENERATION_THREE,
            b"a later committed payload must not be decoded as fallback",
            None,
        );
        assert_corrupt(open(after_markerless.path()).await);
        assert!(
            staging.exists(),
            "a committed generation after staging prevents cleanup"
        );

        for (name, head) in [
            ("missing committed payload", None),
            (
                "invalid committed payload",
                Some(b"not canonical JSON".as_slice()),
            ),
        ] {
            let root = TempRoot::existing();
            create_marked_empty_store(root.path());
            create_valid_g1_agent(root.path());
            let committed = create_markerless_agent_generation(
                root.path(),
                AGENT_ONE,
                GENERATION_TWO,
                head,
                None,
            );
            create_file(&committed.join("COMMITTED"), b"");
            assert_corrupt(open(root.path()).await);
            assert!(committed.exists(), "{name} stays in place for diagnosis");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn markerless_shape_missing_visibility_and_terminal_heads_fail_closed_without_cleanup() {
        let unknown = TempRoot::existing();
        create_marked_empty_store(unknown.path());
        create_valid_g1_agent(unknown.path());
        let staging = create_markerless_agent_generation(
            unknown.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        create_file(&staging.join("unknown"), b"");
        assert_corrupt(open(unknown.path()).await);
        assert!(staging.exists());

        let type_mismatch = TempRoot::existing();
        create_marked_empty_store(type_mismatch.path());
        create_valid_g1_agent(type_mismatch.path());
        let staging = create_markerless_agent_generation(
            type_mismatch.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        create_directory(&staging.join("head.json"));
        assert_corrupt(open(type_mismatch.path()).await);
        assert!(staging.exists());

        let oversized = TempRoot::existing();
        create_marked_empty_store(oversized.path());
        create_valid_g1_agent(oversized.path());
        let staging = create_markerless_agent_generation(
            oversized.path(),
            AGENT_ONE,
            GENERATION_TWO,
            Some(&vec![b'x'; super::MAX_DURABLE_DOCUMENT_BYTES + 1]),
            None,
        );
        assert!(matches!(
            open(oversized.path()).await,
            Err(DurableOpenError::DurableStateTooLarge)
        ));
        assert!(staging.exists());

        let missing_reservation = TempRoot::existing();
        create_marked_empty_store(missing_reservation.path());
        create_valid_g1_agent(missing_reservation.path());
        let staging = create_markerless_agent_generation(
            missing_reservation.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        fs::remove_file(
            missing_reservation
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(AGENTS_DIRECTORY)
                .join(AGENT_ONE),
        )
        .unwrap();
        assert_corrupt(open(missing_reservation.path()).await);
        assert!(staging.exists());

        let missing_published = TempRoot::existing();
        create_marked_empty_store(missing_published.path());
        create_valid_g1_agent(missing_published.path());
        let staging = create_markerless_agent_generation(
            missing_published.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        fs::remove_file(agent_path(missing_published.path(), AGENT_ONE).join("PUBLISHED")).unwrap();
        assert_corrupt(open(missing_published.path()).await);
        assert!(staging.exists());

        let deleted_agent = TempRoot::existing();
        create_marked_empty_store(deleted_agent.path());
        create_valid_g1_agent(deleted_agent.path());
        create_agent_generation(
            deleted_agent.path(),
            AGENT_ONE,
            GENERATION_TWO,
            &replace_fixture(
                agent_head_status_g2_fixture(),
                "\"status\":\"disabled\"",
                "\"status\":\"deleted\"",
            ),
            None,
        );
        let staging = create_markerless_agent_generation(
            deleted_agent.path(),
            AGENT_ONE,
            GENERATION_THREE,
            None,
            None,
        );
        assert_corrupt(open(deleted_agent.path()).await);
        assert!(staging.exists());

        let deleted_session = TempRoot::existing();
        create_marked_empty_store(deleted_session.path());
        create_ordinary_session_chain_through_g3_deleted(deleted_session.path());
        let staging = create_markerless_session_generation(
            deleted_session.path(),
            SESSION_ONE,
            GENERATION_FOUR,
            None,
            None,
        );
        assert_corrupt(open(deleted_session.path()).await);
        assert!(staging.exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn markerless_link_and_wrong_mode_are_corrupt_without_cleanup() {
        use std::os::unix::fs::symlink;

        let linked = TempRoot::existing();
        create_marked_empty_store(linked.path());
        create_valid_g1_agent(linked.path());
        let staging = create_markerless_agent_generation(
            linked.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        let target_root = TempRoot::existing();
        let target = target_root.path().join("markerless-head-target");
        create_file(&target, b"partial");
        symlink(&target, staging.join("head.json")).unwrap();
        assert_corrupt(open(linked.path()).await);
        assert!(staging.exists());

        let wrong_mode = TempRoot::existing();
        create_marked_empty_store(wrong_mode.path());
        create_valid_g1_agent(wrong_mode.path());
        let staging = create_markerless_agent_generation(
            wrong_mode.path(),
            AGENT_ONE,
            GENERATION_TWO,
            Some(b"partial"),
            None,
        );
        set_unix_mode(&staging.join("head.json"), 0o644);
        assert_corrupt(open(wrong_mode.path()).await);
        assert!(staging.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_staging_or_later_semantic_corruption_prevents_all_cleanup() {
        let multiple = TempRoot::existing();
        create_marked_empty_store(multiple.path());
        create_valid_g1_agent(multiple.path());
        create_exact_g1_agent(
            multiple.path(),
            AGENT_TWO,
            &replace_fixture(agent_head_fixture(), AGENT_ONE, AGENT_TWO),
            &replace_fixture(agent_definition_fixture(), AGENT_ONE, AGENT_TWO),
        );
        let first = create_markerless_agent_generation(
            multiple.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        let second = create_markerless_agent_generation(
            multiple.path(),
            AGENT_TWO,
            GENERATION_TWO,
            None,
            None,
        );
        assert_corrupt(open(multiple.path()).await);
        assert!(first.exists() && second.exists());

        let later_semantic_corruption = TempRoot::existing();
        create_marked_empty_store(later_semantic_corruption.path());
        create_valid_g1_agent(later_semantic_corruption.path());
        let first = create_markerless_agent_generation(
            later_semantic_corruption.path(),
            AGENT_ONE,
            GENERATION_TWO,
            None,
            None,
        );
        create_exact_g1_agent(
            later_semantic_corruption.path(),
            AGENT_TWO,
            b"invalid later Agent head",
            &replace_fixture(agent_definition_fixture(), AGENT_ONE, AGENT_TWO),
        );
        assert_corrupt(open(later_semantic_corruption.path()).await);
        assert!(
            first.exists(),
            "the first candidate remains when a later entity fails semantic recovery"
        );
    }

    #[test]
    fn cleanup_revalidation_rejects_marker_child_and_payload_shape_changes_without_deleting() {
        let root = TempRoot::existing();
        create_marked_empty_store(root.path());
        create_valid_g1_agent(root.path());
        let staging =
            create_markerless_agent_generation(root.path(), AGENT_ONE, GENERATION_TWO, None, None);
        let root_entries = read_entries_bounded(root.path(), ROOT_ENTRY_CAP).unwrap();
        let super::DeferredCleanup::TrailingGeneration(cleanup) =
            super::recover_marked_root(root.path(), &root_entries)
                .unwrap()
                .cleanup
                .expect("the empty staging directory produces a cleanup plan")
        else {
            panic!("a published trailing generation uses its narrow cleanup variant");
        };
        drop(root_entries);
        assert!(!cleanup.has_head && !cleanup.has_definition);
        let debug = format!("{cleanup:?}");
        assert!(!debug.contains("minicore-durable-state-"));
        assert!(!debug.contains(AGENT_ONE));

        create_file(&staging.join("COMMITTED"), b"");
        assert!(matches!(
            super::finalize_deferred_generation_cleanup(
                &cleanup,
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(staging.exists(), "a newly observed marker is never deleted");

        fs::remove_file(staging.join("COMMITTED")).unwrap();
        create_file(&staging.join("unknown"), b"");
        assert!(matches!(
            super::finalize_deferred_generation_cleanup(
                &cleanup,
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            staging.exists(),
            "a malformed revalidation shape is never deleted"
        );

        fs::remove_file(staging.join("unknown")).unwrap();
        create_file(&staging.join("head.json"), agent_head_metadata_g2_fixture());
        assert!(matches!(
            super::finalize_deferred_generation_cleanup(
                &cleanup,
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            staging.join("head.json").is_file(),
            "a legal head added after an empty scan is never deleted"
        );

        fs::remove_file(staging.join("head.json")).unwrap();
        create_file(&staging.join("head.json"), agent_head_metadata_g2_fixture());
        create_file(
            &staging.join("definition.json"),
            agent_definition_g2_fixture(),
        );
        let root_entries = read_entries_bounded(root.path(), ROOT_ENTRY_CAP).unwrap();
        let super::DeferredCleanup::TrailingGeneration(both_cleanup) =
            super::recover_marked_root(root.path(), &root_entries)
                .unwrap()
                .cleanup
                .expect("the complete staging subset produces a cleanup plan")
        else {
            panic!("a published trailing generation uses its narrow cleanup variant");
        };
        drop(root_entries);
        assert!(both_cleanup.has_head && both_cleanup.has_definition);
        fs::remove_file(staging.join("head.json")).unwrap();
        assert!(matches!(
            super::finalize_deferred_generation_cleanup(
                &both_cleanup,
                super::DirectorySync::Unsupported,
                &super::LocalFilesystem,
            ),
            Err(DurableOpenError::DurableStateCorrupt)
        ));
        assert!(
            staging.exists(),
            "a changed both-payload plan is never deleted"
        );
        assert!(
            staging.join("definition.json").is_file(),
            "the remaining legal child is never deleted after shape drift"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_faults_are_persistent_ordered_and_retryable() {
        let before_definition = TempRoot::existing();
        let staging = create_agent_both_payload_staging(before_definition.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::Before(
            CleanupOperation::RemoveDefinition,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(before_definition.path().to_owned(), &filesystem),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(staging.join("definition.json").exists());
        assert!(staging.join("head.json").exists());
        assert_eq!(
            filesystem.operations(),
            [CleanupOperation::RemoveDefinition]
        );

        let after_definition = TempRoot::existing();
        let staging = create_agent_both_payload_staging(after_definition.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::After(
            CleanupOperation::RemoveDefinition,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(after_definition.path().to_owned(), &filesystem),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(!staging.join("definition.json").exists());
        assert!(staging.join("head.json").exists());
        assert_eq!(
            filesystem.operations(),
            [CleanupOperation::RemoveDefinition]
        );
        let reopened = open_root(after_definition.path().to_owned())
            .expect("LocalFilesystem retries the post-effect partial cleanup");
        assert!(!staging.exists());
        drop(reopened);

        let before_head = TempRoot::existing();
        let staging = create_agent_both_payload_staging(before_head.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::Before(
            CleanupOperation::RemoveHead,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(before_head.path().to_owned(), &filesystem),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(!staging.join("definition.json").exists());
        assert!(staging.join("head.json").exists());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead
            ]
        );
        drop(open_root(before_head.path().to_owned()).expect("head-removal retry opens"));
        assert!(!staging.exists());

        let after_head = TempRoot::existing();
        let staging = create_agent_both_payload_staging(after_head.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::After(
            CleanupOperation::RemoveHead,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(after_head.path().to_owned(), &filesystem),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(!staging.join("definition.json").exists());
        assert!(!staging.join("head.json").exists());
        assert!(staging.is_dir());
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
            ]
        );
        drop(open_root(after_head.path().to_owned()).expect("post-head-removal retry opens"));
        assert!(!staging.exists());

        let before_remove_dir = TempRoot::existing();
        let staging = create_agent_both_payload_staging(before_remove_dir.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::Before(
            CleanupOperation::RemoveCandidateDirectory,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(before_remove_dir.path().to_owned(), &filesystem),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(staging.is_dir());
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::SyncCandidateDirectory,
                CleanupOperation::RemoveCandidateDirectory,
            ]
        );
        drop(open_root(before_remove_dir.path().to_owned()).expect("remove-dir retry opens"));
        assert!(!staging.exists());

        let before_sync = TempRoot::existing();
        let staging = create_agent_both_payload_staging(before_sync.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::Before(
            CleanupOperation::SyncCandidateDirectory,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(before_sync.path().to_owned(), &filesystem),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(staging.is_dir());
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::SyncCandidateDirectory,
            ]
        );
        drop(open_root(before_sync.path().to_owned()).expect("sync retry opens"));
        assert!(!staging.exists());

        let after_sync_candidate = TempRoot::existing();
        let staging = create_agent_both_payload_staging(after_sync_candidate.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::After(
            CleanupOperation::SyncCandidateDirectory,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(after_sync_candidate.path().to_owned(), &filesystem),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(staging.is_dir());
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::SyncCandidateDirectory,
            ]
        );
        drop(
            open_root(after_sync_candidate.path().to_owned())
                .expect("post-candidate-sync retry opens"),
        );
        assert!(!staging.exists());

        let after_remove_dir = TempRoot::existing();
        let staging = create_agent_both_payload_staging(after_remove_dir.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::After(
            CleanupOperation::RemoveCandidateDirectory,
        )]);
        let opened =
            open_root_with_cleanup_filesystem(after_remove_dir.path().to_owned(), &filesystem)
                .expect("an absent readback reconciles the post-effect remove-dir failure");
        assert!(!staging.exists());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::SyncCandidateDirectory,
                CleanupOperation::RemoveCandidateDirectory,
                CleanupOperation::SyncGenerationsDirectory,
            ]
        );
        drop(opened);
        drop(
            open_root(after_remove_dir.path().to_owned())
                .expect("reconciled remove-dir cleanup leaves a reopenable store"),
        );

        let before_generations_sync = TempRoot::existing();
        let staging = create_agent_both_payload_staging(before_generations_sync.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::Before(
            CleanupOperation::SyncGenerationsDirectory,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(
                before_generations_sync.path().to_owned(),
                &filesystem,
            ),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(!staging.exists());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::SyncCandidateDirectory,
                CleanupOperation::RemoveCandidateDirectory,
                CleanupOperation::SyncGenerationsDirectory,
            ]
        );
        drop(
            open_root(before_generations_sync.path().to_owned())
                .expect("missing candidate after pre-sync failure reopens"),
        );

        let after_generations_sync = TempRoot::existing();
        let staging = create_agent_both_payload_staging(after_generations_sync.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([CleanupFault::After(
            CleanupOperation::SyncGenerationsDirectory,
        )]);
        assert!(matches!(
            open_root_with_cleanup_filesystem(
                after_generations_sync.path().to_owned(),
                &filesystem,
            ),
            Err(DurableOpenError::StorageUnavailable)
        ));
        assert!(!staging.exists());
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::SyncCandidateDirectory,
                CleanupOperation::RemoveCandidateDirectory,
                CleanupOperation::SyncGenerationsDirectory,
            ]
        );
        drop(
            open_root(after_generations_sync.path().to_owned())
                .expect("missing candidate after post-sync failure reopens"),
        );

        let ordered = TempRoot::existing();
        create_agent_both_payload_staging(ordered.path());
        let filesystem = DeterministicPersistentFaultFilesystem::new([]);
        let opened = open_root_with_cleanup_filesystem(ordered.path().to_owned(), &filesystem)
            .expect("the faultless cleanup succeeds");
        drop(opened);
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::SyncCandidateDirectory,
                CleanupOperation::RemoveCandidateDirectory,
                CleanupOperation::SyncGenerationsDirectory,
            ]
        );
        assert_eq!(
            classify_directory_sync(ordered.path()).unwrap(),
            DirectorySync::Supported,
            "the Unix deterministic adapter verifies both required directory sync points"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unpublished_entity_cleanup_is_committed_first_ordered_and_retryable_at_every_namespace() {
        let ordered = TempRoot::existing();
        let entity = create_unpublished_committed_session_staging(ordered.path());
        let reservation = ordered
            .path()
            .join(RESERVATIONS_DIRECTORY)
            .join(SESSIONS_DIRECTORY)
            .join(SESSION_ONE);
        let filesystem = DeterministicPersistentFaultFilesystem::new([]);
        drop(
            open_root_with_cleanup_filesystem(ordered.path().to_owned(), &filesystem)
                .expect("faultless whole-entity cleanup succeeds"),
        );
        assert!(!entity.exists());
        assert!(
            reservation.exists(),
            "whole cleanup never removes reservation"
        );
        assert_eq!(
            filesystem.operations(),
            [
                CleanupOperation::RemoveCommitted,
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::RemoveConversation,
                CleanupOperation::SyncCandidateDirectory,
                CleanupOperation::RemoveCandidateDirectory,
                CleanupOperation::SyncGenerationsDirectory,
                CleanupOperation::RemoveGenerationsDirectory,
                CleanupOperation::SyncEntityDirectory,
                CleanupOperation::RemoveEntityDirectory,
                CleanupOperation::SyncCollectionDirectory,
            ],
            "COMMITTED precedes every payload removal, and each removed namespace syncs its direct parent exactly once",
        );

        for fault in [
            CleanupFault::Before(CleanupOperation::RemoveCommitted),
            CleanupFault::After(CleanupOperation::RemoveCommitted),
            CleanupFault::Before(CleanupOperation::RemoveDefinition),
            CleanupFault::After(CleanupOperation::RemoveDefinition),
            CleanupFault::Before(CleanupOperation::RemoveHead),
            CleanupFault::After(CleanupOperation::RemoveHead),
            CleanupFault::Before(CleanupOperation::RemoveConversation),
            CleanupFault::After(CleanupOperation::RemoveConversation),
            CleanupFault::Before(CleanupOperation::SyncCandidateDirectory),
            CleanupFault::After(CleanupOperation::SyncCandidateDirectory),
            CleanupFault::Before(CleanupOperation::RemoveCandidateDirectory),
            CleanupFault::After(CleanupOperation::RemoveCandidateDirectory),
            CleanupFault::Before(CleanupOperation::SyncGenerationsDirectory),
            CleanupFault::After(CleanupOperation::SyncGenerationsDirectory),
            CleanupFault::Before(CleanupOperation::RemoveGenerationsDirectory),
            CleanupFault::After(CleanupOperation::RemoveGenerationsDirectory),
            CleanupFault::Before(CleanupOperation::SyncEntityDirectory),
            CleanupFault::After(CleanupOperation::SyncEntityDirectory),
            CleanupFault::Before(CleanupOperation::RemoveEntityDirectory),
            CleanupFault::After(CleanupOperation::RemoveEntityDirectory),
            CleanupFault::Before(CleanupOperation::SyncCollectionDirectory),
            CleanupFault::After(CleanupOperation::SyncCollectionDirectory),
        ] {
            let root = TempRoot::existing();
            let entity = create_unpublished_committed_session_staging(root.path());
            let reservation = root
                .path()
                .join(RESERVATIONS_DIRECTORY)
                .join(SESSIONS_DIRECTORY)
                .join(SESSION_ONE);
            let filesystem = DeterministicPersistentFaultFilesystem::new([fault]);
            let first_open = open_root_with_cleanup_filesystem(root.path().to_owned(), &filesystem);
            let reconciled_remove_directory = matches!(
                fault,
                CleanupFault::After(
                    CleanupOperation::RemoveCandidateDirectory
                        | CleanupOperation::RemoveGenerationsDirectory
                        | CleanupOperation::RemoveEntityDirectory
                )
            );
            if reconciled_remove_directory {
                drop(first_open.expect("immediately absent remove-dir failure is reconciled"));
            } else {
                assert!(matches!(
                    first_open,
                    Err(DurableOpenError::StorageUnavailable)
                ));
            }
            let ordered = [
                CleanupOperation::RemoveCommitted,
                CleanupOperation::RemoveDefinition,
                CleanupOperation::RemoveHead,
                CleanupOperation::RemoveConversation,
                CleanupOperation::SyncCandidateDirectory,
                CleanupOperation::RemoveCandidateDirectory,
                CleanupOperation::SyncGenerationsDirectory,
                CleanupOperation::RemoveGenerationsDirectory,
                CleanupOperation::SyncEntityDirectory,
                CleanupOperation::RemoveEntityDirectory,
                CleanupOperation::SyncCollectionDirectory,
            ];
            let coordinate = ordered
                .iter()
                .position(|operation| *operation == fault.operation())
                .expect("the fault is one ordered whole-cleanup coordinate");
            assert_eq!(
                &filesystem.operations()[..=coordinate],
                &ordered[..=coordinate],
                "the observed operation prefix reaches exactly {fault:?}",
            );

            drop(open_root(root.path().to_owned()).unwrap_or_else(|error| {
                panic!("{fault:?} leaves an exact retry prefix: {error:?}")
            }));
            assert!(!entity.exists(), "{fault:?} retry removes the whole entity");
            assert!(reservation.exists(), "{fault:?} retains reservation");
        }
    }
}
