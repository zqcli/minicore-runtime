use std::collections::BTreeSet;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio::sync::Notify;

use crate::agent_session_lifecycle::AgentRevisionRef;
use crate::compaction::StoredCompaction;
use crate::model_gateway::{
    ModelFinishReason, ModelResponseSummary, ModelUsage, ProviderResponseId,
    ProviderResponseMetadata, ReasoningContent,
};
use crate::prompt::CanonicalUserMessage;
use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedBlockingJob};
use crate::tools::{
    ToolAbandonReason, ToolApprovalRequestView, ToolApprovalResolution, ToolCallId, ToolName,
    ToolOutcomeSource, ToolResultContent, ToolResultDisposition, UserQuestionAnswer,
    UserQuestionRequest,
};
use crate::turn_item_interaction::{
    AssistantDisposition, InteractionCancelReason, UserMessageSource,
};
use crate::wire::conversation_jsonl::{
    ConversationCodecError, ConversationLineCodec, MAX_CONVERSATION_ENTRY_BYTES,
};
use crate::wire::conversation_jsonl_scanner::{
    ConversationJsonlScanner, ConversationLineCanonicality, ConversationLineFault,
    ConversationScanAccess, ConversationScanError, ConversationScanEvent,
    MAX_CONVERSATION_FILE_BYTES,
};
use crate::wire::{
    BoundedJsonObject, EntryId, InteractionResolutionKey, ItemId, RequestId,
    SessionDefinitionRevision, SessionId, Timestamp, TurnId,
};

#[path = "live_conversation.rs"]
#[allow(
    dead_code,
    reason = "the completed M4 live reducer is consumed by the pending M7 and M5.2 slices"
)]
pub(crate) mod live_conversation;

// Conversation Storage exposes only its immutable read projection. The reducer module remains
// the sole owner of the projection constructor and of all mutable live state.
#[allow(
    unused_imports,
    reason = "the read-only projection is re-exported for Conversation Storage consumers"
)]
pub(crate) use live_conversation::LiveConversationView;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum StoredValueError {
    #[error("stored user message violates its closed semantic contract")]
    UserMessage,
    #[error("stored assistant message violates its closed semantic contract")]
    Assistant,
    #[error("stored tool outcome violates its closed semantic contract")]
    ToolOutcome,
    #[error("stored interaction resolution violates its closed semantic contract")]
    InteractionResolution,
}

/// Opaque evidence that DurableState opened one physical conversation file for exclusive
/// writable use.
///
/// DurableState is the sole production issuer: while holding the root lease, it binds the Session
/// identity and physical length observed from the same-open file into this proof. The proof does
/// not acquire, hold, or emulate an OS lock; Conversation Storage only consumes its private
/// append-capable handle when it needs to perform the corresponding write work.
#[allow(
    dead_code,
    reason = "the DurableState-issued append handle is consumed by the pending Recorder seam"
)]
pub(crate) struct ExclusiveWritableConversationLease {
    session_id: SessionId,
    declared_file_bytes: u64,
    file: Option<File>,
}

#[allow(
    dead_code,
    reason = "the DurableState-issued append handle is consumed by the pending Recorder seam"
)]
impl ExclusiveWritableConversationLease {
    /// Constructs the production proof from DurableState's same-open append-capable handle.
    pub(crate) fn from_durable_state(
        session_id: SessionId,
        declared_file_bytes: u64,
        file: File,
    ) -> Self {
        Self {
            session_id,
            declared_file_bytes,
            file: Some(file),
        }
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn declared_file_bytes(&self) -> u64 {
        self.declared_file_bytes
    }

    /// Gives the append handle only to Conversation Storage's internal consumer.
    pub(crate) fn into_file(self) -> Option<File> {
        self.file
    }

    #[cfg(test)]
    pub(crate) const fn for_scanner_test(session_id: SessionId, declared_file_bytes: u64) -> Self {
        Self {
            session_id,
            declared_file_bytes,
            file: None,
        }
    }
}

impl fmt::Debug for ExclusiveWritableConversationLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExclusiveWritableConversationLease")
            .field("session_id", &"redacted")
            .field("declared_file_bytes", &self.declared_file_bytes)
            .finish()
    }
}

/// Opaque physical target for one published conversation opened by DurableState.
///
/// The target keeps only the Session identity, the same-open declared length, and an
/// append-capable handle. It never stores or exposes the target path. Conversation Storage may
/// consume it into the file and its paired writable proof; lifecycle callers see only this opaque
/// capability.
#[allow(
    dead_code,
    reason = "the DurableState-issued target is consumed by the pending Recorder seam"
)]
pub(crate) struct PublishedConversationTarget {
    session_id: SessionId,
    declared_file_bytes: u64,
    file: File,
    writable_lease: ExclusiveWritableConversationLease,
}

#[allow(
    dead_code,
    reason = "the DurableState-issued target is consumed by the pending Recorder seam"
)]
impl PublishedConversationTarget {
    /// Constructs the production target and paired proof from DurableState's same-open handles.
    pub(crate) fn from_durable_state(
        session_id: SessionId,
        declared_file_bytes: u64,
        file: File,
        writable_file: File,
    ) -> Self {
        Self {
            session_id,
            declared_file_bytes,
            file,
            writable_lease: ExclusiveWritableConversationLease::from_durable_state(
                session_id,
                declared_file_bytes,
                writable_file,
            ),
        }
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn declared_file_bytes(&self) -> u64 {
        self.declared_file_bytes
    }

    /// Splits the target into its append-capable handle and its DurableState-issued proof.
    pub(crate) fn into_parts(self) -> (File, ExclusiveWritableConversationLease) {
        (self.file, self.writable_lease)
    }
}

impl fmt::Debug for PublishedConversationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedConversationTarget")
            .field("session_id", &"redacted")
            .field("declared_file_bytes", &self.declared_file_bytes)
            .finish()
    }
}

/// Opaque same-open source capability for one unloaded recorded-history Fork capture.
///
/// DurableState is the sole issuer. The lease contains no path and exposes no raw file handle;
/// Conversation Storage consumes the held file for Header validation while DurableState retains
/// the root lease and independently revalidates the source path observation.
pub(crate) struct RecordedForkConversationLease {
    source_session_id: SessionId,
    declared_file_bytes: u64,
    file: File,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum RecordedForkSourceError {
    #[error("recorded Fork source exceeds its selected storage limit")]
    TooLarge,
    #[error("recorded Fork source is corrupt")]
    Corrupt,
    #[error("recorded Fork source is unavailable")]
    Unavailable,
}

impl RecordedForkConversationLease {
    pub(crate) fn from_durable_state(
        source_session_id: SessionId,
        declared_file_bytes: u64,
        file: File,
    ) -> Self {
        Self {
            source_session_id,
            declared_file_bytes,
            file,
        }
    }

    pub(crate) const fn source_session_id(&self) -> SessionId {
        self.source_session_id
    }

    pub(crate) const fn declared_file_bytes(&self) -> u64 {
        self.declared_file_bytes
    }

    pub(crate) fn validate_recorded_genesis(
        &mut self,
        expected_header: &SessionHeader,
    ) -> Result<(), RecordedForkSourceError> {
        let scanner = ConversationJsonlScanner::open(
            &mut self.file,
            self.declared_file_bytes,
            self.source_session_id,
            ConversationScanAccess::ReadOnly,
        )
        .map_err(map_recorded_fork_source_scan_error)?;
        if scanner
            .header()
            .map_err(map_recorded_fork_source_scan_error)?
            != expected_header
            || !scanner.header_is_canonical()
        {
            return Err(RecordedForkSourceError::Corrupt);
        }
        Ok(())
    }

    pub(crate) fn handle_metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }
}

impl fmt::Debug for RecordedForkConversationLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordedForkConversationLease")
            .field("source_session", &"redacted")
            .field("declared_file_bytes", &self.declared_file_bytes)
            .finish()
    }
}

fn map_recorded_fork_source_scan_error(error: ConversationScanError) -> RecordedForkSourceError {
    match error {
        ConversationScanError::FileTooLarge
        | ConversationScanError::HistoryTooLarge
        | ConversationScanError::HeaderCorrupt {
            code: ConversationCodecError::HeaderTooLarge,
        } => RecordedForkSourceError::TooLarge,
        ConversationScanError::HeaderCorrupt { .. }
        | ConversationScanError::UnsupportedFormatVersion
        | ConversationScanError::MissingHeader => RecordedForkSourceError::Corrupt,
        ConversationScanError::LeaseMismatch
        | ConversationScanError::InputChanged
        | ConversationScanError::InputUnavailable
        | ConversationScanError::CounterOverflow
        | ConversationScanError::InvariantViolation => RecordedForkSourceError::Unavailable,
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SessionHeader {
    format_version: u32,
    session_id: SessionId,
    created_at: Timestamp,
    initial_agent: AgentRevisionRef,
    initial_definition_revision: SessionDefinitionRevision,
}

impl SessionHeader {
    pub(crate) const fn reconstruct(
        format_version: u32,
        session_id: SessionId,
        created_at: Timestamp,
        initial_agent: AgentRevisionRef,
        initial_definition_revision: SessionDefinitionRevision,
    ) -> Self {
        Self {
            format_version,
            session_id,
            created_at,
            initial_agent,
            initial_definition_revision,
        }
    }

    pub(crate) const fn format_version(&self) -> u32 {
        self.format_version
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    pub(crate) const fn initial_agent(&self) -> AgentRevisionRef {
        self.initial_agent
    }

    pub(crate) const fn initial_definition_revision(&self) -> SessionDefinitionRevision {
        self.initial_definition_revision
    }
}

impl fmt::Debug for SessionHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHeader")
            .field("format_version", &self.format_version)
            .field("session_id", &self.session_id)
            .field("created_at", &self.created_at)
            .field("initial_agent", &self.initial_agent)
            .field(
                "initial_definition_revision",
                &self.initial_definition_revision,
            )
            .finish()
    }
}

/// A complete unpublished-conversation shape accepted for restart cleanup classification.
///
/// This is deliberately narrower than tolerant replay: an ordinary Session has only a Header,
/// while a Fork has one canonical root-to-leaf path (which may be empty). It is only used for
/// restart cleanup classification; it does not bind a source seed, provenance anchor, or expected
/// entry sequence, and it does not create or substitute for a future `PreparedConversationProof`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnpublishedConversationRecoveryShape {
    OrdinaryHeaderOnly,
    ForkCanonicalLinearFile,
}

/// Redacted result of strict readback classification for an unpublished conversation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum UnpublishedConversationRecoveryError {
    #[error("unpublished conversation recovery input exceeds a storage limit")]
    TooLarge,
    #[error("unpublished conversation recovery input is corrupt")]
    Corrupt,
    #[error("unpublished conversation recovery input is unavailable")]
    Unavailable,
}

/// Strictly classifies the full physical readback of one unpublished conversation for recovery.
///
/// The caller supplies only a readable input, its already-observed byte length, the exact Header
/// expected for that input, and its closed expected shape. This does not open files, resolve
/// paths, or alter the tolerant replay contract for published Sessions. It is only for restart
/// cleanup classification: it does not bind a source seed, provenance anchor, or expected entry
/// sequence, and it does not create or substitute for a future `PreparedConversationProof`.
pub(crate) fn validate_unpublished_conversation_for_recovery<R: Read>(
    reader: R,
    declared_file_bytes: u64,
    expected_header: &SessionHeader,
    expected_shape: UnpublishedConversationRecoveryShape,
) -> Result<(), UnpublishedConversationRecoveryError> {
    let mut scanner = ConversationJsonlScanner::open(
        reader,
        declared_file_bytes,
        expected_header.session_id(),
        ConversationScanAccess::ReadOnly,
    )
    .map_err(map_unpublished_conversation_recovery_scan_error)?;

    if scanner
        .header()
        .map_err(map_unpublished_conversation_recovery_scan_error)?
        != expected_header
        || !scanner.header_is_canonical()
    {
        return Err(UnpublishedConversationRecoveryError::Corrupt);
    }

    let mut seen_entry_ids = BTreeSet::new();
    let mut previous_entry_id = None;

    loop {
        match scanner
            .next_event()
            .map_err(map_unpublished_conversation_recovery_scan_error)?
        {
            None => return Ok(()),
            Some(ConversationScanEvent::Fault {
                fault: ConversationLineFault::OversizedLine,
                ..
            }) => return Err(UnpublishedConversationRecoveryError::TooLarge),
            Some(
                ConversationScanEvent::Fault { .. } | ConversationScanEvent::PartialTail { .. },
            ) => {
                return Err(UnpublishedConversationRecoveryError::Corrupt);
            }
            Some(ConversationScanEvent::Entry {
                canonicality,
                entry,
                ..
            }) => {
                if canonicality != ConversationLineCanonicality::Canonical {
                    return Err(UnpublishedConversationRecoveryError::Corrupt);
                }
                if expected_shape == UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly {
                    return Err(UnpublishedConversationRecoveryError::Corrupt);
                }
                if !seen_entry_ids.insert(entry.entry_id()) {
                    return Err(UnpublishedConversationRecoveryError::Corrupt);
                }
                if match previous_entry_id {
                    None => entry.parent_id().is_none(),
                    Some(previous_entry_id) => entry.parent_id() == Some(previous_entry_id),
                } {
                    previous_entry_id = Some(entry.entry_id());
                } else {
                    return Err(UnpublishedConversationRecoveryError::Corrupt);
                }
            }
        }
    }
}

fn map_unpublished_conversation_recovery_scan_error(
    error: ConversationScanError,
) -> UnpublishedConversationRecoveryError {
    match error {
        ConversationScanError::FileTooLarge
        | ConversationScanError::HistoryTooLarge
        | ConversationScanError::HeaderCorrupt {
            code: ConversationCodecError::HeaderTooLarge,
        } => UnpublishedConversationRecoveryError::TooLarge,
        ConversationScanError::HeaderCorrupt { .. }
        | ConversationScanError::UnsupportedFormatVersion
        | ConversationScanError::MissingHeader => UnpublishedConversationRecoveryError::Corrupt,
        ConversationScanError::LeaseMismatch
        | ConversationScanError::InputChanged
        | ConversationScanError::InputUnavailable
        | ConversationScanError::CounterOverflow
        | ConversationScanError::InvariantViolation => {
            UnpublishedConversationRecoveryError::Unavailable
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredSessionEntry {
    entry_id: EntryId,
    parent_id: Option<EntryId>,
    session_id: SessionId,
    turn_id: TurnId,
    timestamp: Timestamp,
    body: StoredEntryBody,
}

impl StoredSessionEntry {
    pub(crate) const fn reconstruct(
        entry_id: EntryId,
        parent_id: Option<EntryId>,
        session_id: SessionId,
        turn_id: TurnId,
        timestamp: Timestamp,
        body: StoredEntryBody,
    ) -> Self {
        Self {
            entry_id,
            parent_id,
            session_id,
            turn_id,
            timestamp,
            body,
        }
    }

    pub(crate) const fn entry_id(&self) -> EntryId {
        self.entry_id
    }

    pub(crate) const fn parent_id(&self) -> Option<EntryId> {
        self.parent_id
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub(crate) const fn body(&self) -> &StoredEntryBody {
        &self.body
    }
}

impl fmt::Debug for StoredSessionEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredSessionEntry")
            .field("entry_id", &self.entry_id)
            .field("parent_id", &self.parent_id)
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("timestamp", &self.timestamp)
            .field("body", &self.body)
            .finish()
    }
}

/// The redacted, crate-private failure taxonomy for one best-effort recording attempt.
#[allow(
    dead_code,
    reason = "the loaded SessionExecutor consumes the pending Recorder seam"
)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum SessionRecordingError {
    TargetInvariant,
    MetadataUnavailable,
    EntrySessionMismatch,
    Encode(ConversationCodecError),
    EntryTooLarge,
    FileTooLarge,
    WriteFailed,
    Runtime(RuntimeTaskError),
}

impl fmt::Debug for SessionRecordingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::TargetInvariant => "TargetInvariant",
            Self::MetadataUnavailable => "MetadataUnavailable",
            Self::EntrySessionMismatch => "EntrySessionMismatch",
            Self::Encode(_) => "Encode",
            Self::EntryTooLarge => "EntryTooLarge",
            Self::FileTooLarge => "FileTooLarge",
            Self::WriteFailed => "WriteFailed",
            Self::Runtime(_) => "Runtime",
        };
        formatter.write_str("SessionRecordingError::")?;
        formatter.write_str(name)
    }
}

impl fmt::Display for SessionRecordingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TargetInvariant => "conversation recording target invariant failed",
            Self::MetadataUnavailable => "conversation recording target metadata unavailable",
            Self::EntrySessionMismatch => "conversation entry session identity mismatch",
            Self::Encode(_) => "conversation entry encoding failed",
            Self::EntryTooLarge => "conversation entry exceeds the recording limit",
            Self::FileTooLarge => "conversation file exceeds the recording limit",
            Self::WriteFailed => "conversation append failed",
            Self::Runtime(_) => "conversation recording task failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SessionRecordingError {}

/// The result of one ordered, best-effort append attempt.
#[allow(
    dead_code,
    reason = "the loaded SessionExecutor consumes the pending Recorder seam"
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum RecordOutcome {
    Written,
    NotRecorded { health: RecordingHealth },
}

impl fmt::Debug for RecordOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Written => formatter.write_str("RecordOutcome::Written"),
            Self::NotRecorded { health } => formatter
                .debug_struct("RecordOutcome::NotRecorded")
                .field("health", health)
                .finish(),
        }
    }
}

impl fmt::Display for RecordOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Written => formatter.write_str("written"),
            Self::NotRecorded { .. } => formatter.write_str("not recorded"),
        }
    }
}

/// Redacted internal health for one loaded Session's conversation recorder.
#[allow(
    dead_code,
    reason = "the loaded SessionExecutor consumes the pending Recorder seam"
)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum RecordingHealth {
    Healthy,
    Degraded {
        failed_entry_id: Option<EntryId>,
        reason: SessionRecordingError,
    },
}

impl fmt::Debug for RecordingHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => formatter.write_str("RecordingHealth::Healthy"),
            Self::Degraded { reason, .. } => formatter
                .debug_struct("RecordingHealth::Degraded")
                .field("failed_entry_id", &"redacted")
                .field("reason", reason)
                .finish(),
        }
    }
}

impl fmt::Display for RecordingHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => formatter.write_str("healthy"),
            Self::Degraded { .. } => formatter.write_str("recording degraded"),
        }
    }
}

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the named write seam is consumed by focused Recorder tests"
)]
#[derive(Clone, Copy)]
enum RecorderWriteFault {
    Fail,
    Panic,
}

/// Test-only coordination immediately before the first physical append write.
#[cfg(test)]
#[allow(
    dead_code,
    reason = "deterministic Recorder tests consume this named write seam"
)]
pub(crate) struct RecorderWriteBarrier {
    entered: AtomicBool,
    entered_notify: Notify,
    release: Mutex<bool>,
    release_changed: std::sync::Condvar,
    fault: Mutex<Option<RecorderWriteFault>>,
}

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the named write seam is consumed by focused Recorder tests"
)]
impl RecorderWriteBarrier {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicBool::new(false),
            entered_notify: Notify::new(),
            release: Mutex::new(false),
            release_changed: std::sync::Condvar::new(),
            fault: Mutex::new(None),
        })
    }

    fn disabled() -> Arc<Self> {
        let barrier = Self::new();
        barrier.release();
        barrier
    }

    pub(crate) async fn wait_until_entered(&self) {
        loop {
            let notified = self.entered_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.entered.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn release(&self) {
        let mut released = lock_recorder(&self.release);
        *released = true;
        drop(released);
        self.release_changed.notify_all();
    }

    pub(crate) fn fail_before_write(&self) {
        *lock_recorder(&self.fault) = Some(RecorderWriteFault::Fail);
        self.release();
    }

    pub(crate) fn panic_before_write(&self) {
        *lock_recorder(&self.fault) = Some(RecorderWriteFault::Panic);
        self.release();
    }

    fn before_first_write(&self) -> Result<(), SessionRecordingError> {
        self.entered.store(true, Ordering::Release);
        self.entered_notify.notify_waiters();

        let mut released = lock_recorder(&self.release);
        while !*released {
            released = self
                .release_changed
                .wait(released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        drop(released);

        match lock_recorder(&self.fault).take() {
            Some(RecorderWriteFault::Fail) => Err(SessionRecordingError::WriteFailed),
            Some(RecorderWriteFault::Panic) => {
                panic!("RecorderWriteBarrier requested a write panic")
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
impl fmt::Debug for RecorderWriteBarrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecorderWriteBarrier { .. }")
    }
}

/// One started physical append: the pending File slot, the expected pre-write length, the failed
/// EntryId, the retained blocking job, and the settled outcome. The core keeps the exact started
/// or completed attempt in `last_attempt` until the next record or close reaps it.
struct RecorderAttempt {
    file: Mutex<Option<File>>,
    expected_file_bytes: u64,
    failed_entry_id: EntryId,
    job: Mutex<Option<TrackedBlockingJob<()>>>,
    // Closes the cross-thread window between the attempt becoming visible (InFlight
    // publication) and the publisher's synchronous `store_job` install: a reaper that starts
    // inside that window parks here instead of misclassifying the empty slot as a missing
    // worker. The slot is written exactly once per attempt, so waking means the exact job.
    job_ready: Notify,
    outcome: Mutex<Option<Result<(), SessionRecordingError>>>,
    changed: Notify,
}

impl RecorderAttempt {
    fn new(file: File, expected_file_bytes: u64, failed_entry_id: EntryId) -> Arc<Self> {
        Arc::new(Self {
            file: Mutex::new(Some(file)),
            expected_file_bytes,
            failed_entry_id,
            job: Mutex::new(None),
            job_ready: Notify::new(),
            outcome: Mutex::new(None),
            changed: Notify::new(),
        })
    }

    fn take_file(&self) -> Option<File> {
        lock_recorder(&self.file).take()
    }

    /// Installs the exact blocking job for this attempt and wakes any reaper that entered the
    /// publication-to-install window. The store precedes the wake so a waiter that already
    /// re-checked the slot cannot miss the install (`Notify` delivers a wakeup that lands
    /// before the awaited `notified` future).
    fn store_job(&self, job: TrackedBlockingJob<()>) {
        *lock_recorder(&self.job) = Some(job);
        self.job_ready.notify_waiters();
    }

    /// Waits until the exact job is installed, then clones it.
    ///
    /// A reaper can observe the attempt between `reserve_record` publishing `InFlight` and the
    /// publisher's synchronous `store_job`; the empty slot is a publication window, not a
    /// missing worker. The publisher has no await point inside that window, so this wait always
    /// completes. No mutex guard is held across the await: the slot is only re-checked under
    /// the short-lived job lock, and the `Notify` interest is enabled before every check, so an
    /// install racing the check cannot be lost.
    async fn wait_for_job(&self) -> TrackedBlockingJob<()> {
        loop {
            let notified = self.job_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(job) = self.job() {
                return job;
            }
            notified.await;
        }
    }

    /// Clones the retained blocking job so concurrent record/close reapers can share its exact
    /// `wait`; `TrackedBlockingJob` supports shared waiters, so a clone never consumes the slot
    /// out from under another reaper.
    fn job(&self) -> Option<TrackedBlockingJob<()>> {
        lock_recorder(&self.job).clone()
    }

    /// Exactly-once settlement for worker-observed outcomes.
    fn resolve(&self, result: Result<(), SessionRecordingError>) {
        let mut outcome = lock_recorder(&self.outcome);
        if outcome.is_none() {
            *outcome = Some(result);
            drop(outcome);
            self.changed.notify_waiters();
        }
    }

    /// Overwrites the stored outcome with a terminal task failure and notifies waiters.
    ///
    /// This is reserved for raw-task/join failures that can arrive after a provisional worker
    /// success; the normal worker path stays exactly-once through `resolve`.
    fn force_task_failure(&self, reason: SessionRecordingError) {
        *lock_recorder(&self.outcome) = Some(Err(reason));
        self.changed.notify_waiters();
    }

    fn outcome(&self) -> Option<Result<(), SessionRecordingError>> {
        *lock_recorder(&self.outcome)
    }
}

enum RecorderState {
    Ready { file: File, file_bytes: u64 },
    InFlight(Arc<RecorderAttempt>),
    Degraded,
    Closed,
}

struct SessionRecorderCore {
    session_id: SessionId,
    task_context: RuntimeTaskContext,
    state: Mutex<RecorderState>,
    health: Mutex<Arc<RecordingHealth>>,
    last_attempt: Mutex<Option<Arc<RecorderAttempt>>>,
    closing: AtomicBool,
    #[cfg(test)]
    write_barrier: Mutex<Arc<RecorderWriteBarrier>>,
}

impl SessionRecorderCore {
    fn current_health(&self) -> Arc<RecordingHealth> {
        Arc::clone(&lock_recorder(&self.health))
    }

    fn health_is_healthy_locked(&self) -> bool {
        matches!(&**lock_recorder(&self.health), RecordingHealth::Healthy)
    }

    fn install_degraded_locked(
        &self,
        failed_entry_id: Option<EntryId>,
        reason: SessionRecordingError,
    ) -> Arc<RecordingHealth> {
        let mut health = lock_recorder(&self.health);
        if matches!(&**health, RecordingHealth::Healthy) {
            *health = Arc::new(RecordingHealth::Degraded {
                failed_entry_id,
                reason,
            });
        }
        Arc::clone(&health)
    }

    fn may_attempt(&self) -> bool {
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        let state = lock_recorder(&self.state);
        self.health_is_healthy_locked()
            && matches!(
                &*state,
                RecorderState::Ready { .. } | RecorderState::InFlight(_)
            )
    }

    fn mark_degraded(
        &self,
        failed_entry_id: Option<EntryId>,
        reason: SessionRecordingError,
    ) -> Arc<RecordingHealth> {
        let mut state = lock_recorder(&self.state);
        if matches!(&*state, RecorderState::Closed) {
            return self.current_health();
        }
        let health = self.install_degraded_locked(failed_entry_id, reason);
        if matches!(&*state, RecorderState::Ready { .. }) {
            let previous = std::mem::replace(&mut *state, RecorderState::Degraded);
            drop(previous);
        }
        drop(state);
        health
    }

    fn reserve_record(&self, entry_id: EntryId, line_bytes: u64) -> ReserveRecord {
        let mut state = lock_recorder(&self.state);
        if self.closing.load(Ordering::Acquire) {
            return ReserveRecord::NotRecorded(self.current_health());
        }
        if !self.health_is_healthy_locked() {
            return ReserveRecord::NotRecorded(self.current_health());
        }
        match &*state {
            RecorderState::InFlight(attempt) => {
                return ReserveRecord::Wait(Arc::clone(attempt));
            }
            RecorderState::Degraded | RecorderState::Closed => {
                return ReserveRecord::NotRecorded(self.current_health());
            }
            RecorderState::Ready { .. } => {}
        }

        let previous = std::mem::replace(&mut *state, RecorderState::Closed);
        let RecorderState::Ready { file, file_bytes } = previous else {
            *state = previous;
            return ReserveRecord::NotRecorded(self.current_health());
        };
        if file_bytes
            .checked_add(line_bytes)
            .is_none_or(|next| next > MAX_CONVERSATION_FILE_BYTES)
        {
            let health =
                self.install_degraded_locked(Some(entry_id), SessionRecordingError::FileTooLarge);
            *state = RecorderState::Degraded;
            drop(state);
            drop(file);
            return ReserveRecord::NotRecorded(health);
        }

        let attempt = RecorderAttempt::new(file, file_bytes, entry_id);
        *state = RecorderState::InFlight(Arc::clone(&attempt));
        // The exact attempt is visible to reapers before any blocking work can start.
        *lock_recorder(&self.last_attempt) = Some(Arc::clone(&attempt));
        drop(state);
        ReserveRecord::Start { attempt }
    }

    fn succeed_attempt(&self, attempt: &Arc<RecorderAttempt>, file: File, file_bytes: u64) {
        let mut state = lock_recorder(&self.state);
        if matches!(&*state, RecorderState::InFlight(current) if Arc::ptr_eq(current, attempt)) {
            *state = RecorderState::Ready { file, file_bytes };
        }
        drop(state);
        attempt.resolve(Ok(()));
    }

    /// Settles a worker-observed failure of one attempt.
    ///
    /// Exact ownership is decided under the short state lock: the attempt is exact when it is
    /// the current `InFlight` attempt, or when the state is already `Degraded` while
    /// `last_attempt` still retains this same attempt (a duplicate completion racing the
    /// reaper). Only the exact attempt installs the sticky degraded health — under the same
    /// lock, immediately after the exact state transition, so a concurrent `record` can never
    /// observe a `Degraded` state with `Healthy` health — and only it drops the attempt's
    /// file. A stale completion (the state has moved on) still resolves its own attempt
    /// outcome so a waiting record/close reaper observes it, but it touches neither current
    /// state, health, nor file.
    fn fail_attempt(&self, attempt: &Arc<RecorderAttempt>, reason: SessionRecordingError) {
        let mut exact = false;
        {
            let mut state = lock_recorder(&self.state);
            match &*state {
                RecorderState::InFlight(current) if Arc::ptr_eq(current, attempt) => {
                    *state = RecorderState::Degraded;
                    exact = true;
                }
                RecorderState::Degraded
                    if lock_recorder(&self.last_attempt)
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, attempt)) =>
                {
                    // Duplicate completion of the exact attempt: the state already degraded
                    // for this same retained attempt, so the sticky install below is a no-op
                    // unless the first completer's health install raced this one.
                    exact = true;
                }
                _ => {}
            }
            if exact {
                let _ = self.install_degraded_locked(Some(attempt.failed_entry_id), reason);
            }
        }
        if exact {
            drop(attempt.take_file());
        }
        attempt.resolve(Err(reason));
    }

    fn clear_last_attempt(&self, attempt: &Arc<RecorderAttempt>) {
        let mut last_attempt = lock_recorder(&self.last_attempt);
        if last_attempt
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, attempt))
        {
            *last_attempt = None;
        }
    }

    /// Terminal degradation for a raw-task/join failure of the exact attempt.
    ///
    /// Moves the exact current attempt out of `InFlight` — or out of the `Ready` state it left
    /// behind after a provisional worker success — into `Degraded`, drops its file if present,
    /// installs sticky health under the same short state lock, and force-records the attempt
    /// error. Exact-attempt ownership is preserved through the state/last relationship: every
    /// state transition is guarded by `Arc::ptr_eq`, and serial admission means no later
    /// attempt can have started before this retained attempt is reaped, so a later attempt is
    /// never affected.
    fn fail_attempt_task(&self, attempt: &Arc<RecorderAttempt>, reason: SessionRecordingError) {
        let mut degraded = false;
        {
            let mut state = lock_recorder(&self.state);
            match &*state {
                RecorderState::InFlight(current) if Arc::ptr_eq(current, attempt) => {
                    *state = RecorderState::Degraded;
                    degraded = true;
                }
                RecorderState::Ready { .. } => {
                    // A `Ready` state belongs to this attempt exactly while `last_attempt` still
                    // retains it (the attempt's file moved into the state on worker success).
                    let retained = lock_recorder(&self.last_attempt)
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, attempt));
                    if retained {
                        let RecorderState::Ready { file, .. } =
                            std::mem::replace(&mut *state, RecorderState::Degraded)
                        else {
                            unreachable!("the matched state is the Ready variant");
                        };
                        drop(file);
                        degraded = true;
                    }
                }
                _ => {}
            }
            if degraded {
                // Install the sticky health under the same short state lock, immediately after
                // the exact transition: a concurrent `record` can never observe a `Degraded`
                // state with `Healthy` health.
                let _ = self.install_degraded_locked(Some(attempt.failed_entry_id), reason);
            }
        }
        if degraded {
            // Release the attempt's handle outside the state lock. The worker's RAII failure path
            // takes the same two locks in the opposite temporal order, so no lock is held while
            // transferring or dropping the physical File.
            drop(attempt.take_file());
        }
        // Overwrite any provisional worker result: a task/join failure is terminal for this
        // exact attempt even when the physical bytes were already appended.
        attempt.force_task_failure(reason);
    }

    async fn reap_attempt(&self, attempt: &Arc<RecorderAttempt>) {
        // A concurrent reaper can enter between InFlight publication and the publisher's
        // synchronous `store_job`; wait for the exact job install instead of classifying the
        // momentarily empty slot as a missing worker. The publisher has no await inside that
        // window, so the wait cannot hang, and no mutex guard is held across it.
        let job = attempt.wait_for_job().await;
        // Reap the exact tracked job before consulting any worker outcome: a task aborted before
        // its first poll never runs the worker guard, so only the join can settle it.
        if let Err(error) = job.wait().await {
            // Any raw-task/join failure is terminal for this exact attempt, even when the worker
            // already recorded a provisional `Ok(())` outcome.
            self.fail_attempt_task(attempt, SessionRecordingError::Runtime(error));
        } else if attempt.outcome().is_none() {
            // The task completed but no worker outcome was recorded; settle the exact attempt.
            self.fail_attempt_task(attempt, SessionRecordingError::WriteFailed);
        }
        self.clear_last_attempt(attempt);
    }

    async fn reap_last_attempt(&self) {
        // Clone, never take: caller cancellation while awaiting must leave the retained
        // exact-job anchor in place for the next record or close reaper.
        let attempt = lock_recorder(&self.last_attempt).clone();
        if let Some(attempt) = attempt {
            self.reap_attempt(&attempt).await;
        }
    }

    #[cfg(test)]
    fn write_barrier(&self) -> Arc<RecorderWriteBarrier> {
        Arc::clone(&lock_recorder(&self.write_barrier))
    }

    #[cfg(test)]
    fn set_write_barrier(&self, barrier: Arc<RecorderWriteBarrier>) {
        *lock_recorder(&self.write_barrier) = barrier;
    }

    async fn write_entry(
        self: &Arc<Self>,
        line: Vec<u8>,
        line_bytes: u64,
        attempt: &Arc<RecorderAttempt>,
    ) -> Result<(), SessionRecordingError> {
        let worker_core = Arc::clone(self);
        let worker_attempt = Arc::clone(attempt);
        #[cfg(test)]
        let barrier = self.write_barrier();

        let job = self.task_context.spawn_blocking_tracked(move || {
            let guard = RecorderWriteCompletionGuard::new(
                Arc::clone(&worker_core),
                Arc::clone(&worker_attempt),
            );
            let Some(mut file) = worker_attempt.take_file() else {
                guard.fail(SessionRecordingError::WriteFailed);
                return;
            };
            match file.metadata() {
                Ok(metadata) if metadata.len() == worker_attempt.expected_file_bytes => {}
                Ok(_) => {
                    drop(file);
                    guard.fail(SessionRecordingError::TargetInvariant);
                    return;
                }
                Err(_) => {
                    drop(file);
                    guard.fail(SessionRecordingError::MetadataUnavailable);
                    return;
                }
            }
            #[cfg(test)]
            if let Err(error) = barrier.before_first_write() {
                drop(file);
                guard.fail(error);
                return;
            }
            if file.write_all(&line).is_err() {
                drop(file);
                guard.fail(SessionRecordingError::WriteFailed);
                return;
            }
            guard.success(file, worker_attempt.expected_file_bytes + line_bytes);
        });

        // Retain the exact job in the attempt: a caller drop after this point still leaves the
        // worker and its job owned by `last_attempt` until the next record or close reaps them.
        attempt.store_job(job);
        // Reaping settles this exact attempt — including any raw-task/join failure — before the
        // caller observes its outcome; there is no separate manual job wait path.
        self.reap_attempt(attempt).await;
        attempt
            .outcome()
            .expect("reap_attempt settles the exact attempt outcome before returning")
    }
}

enum ReserveRecord {
    Start { attempt: Arc<RecorderAttempt> },
    Wait(Arc<RecorderAttempt>),
    NotRecorded(Arc<RecordingHealth>),
}

/// Small RAII completion guard: the blocking worker always settles its exact attempt, even when
/// the worker unwinds (panic) before taking an explicit success or failure path.
struct RecorderWriteCompletionGuard {
    core: Arc<SessionRecorderCore>,
    attempt: Arc<RecorderAttempt>,
    armed: bool,
}

impl RecorderWriteCompletionGuard {
    fn new(core: Arc<SessionRecorderCore>, attempt: Arc<RecorderAttempt>) -> Self {
        Self {
            core,
            attempt,
            armed: true,
        }
    }

    fn success(mut self, file: File, file_bytes: u64) {
        self.core.succeed_attempt(&self.attempt, file, file_bytes);
        self.armed = false;
    }

    fn fail(mut self, reason: SessionRecordingError) {
        self.core.fail_attempt(&self.attempt, reason);
        self.armed = false;
    }
}

impl Drop for RecorderWriteCompletionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let reason = if std::thread::panicking() {
            SessionRecordingError::Runtime(RuntimeTaskError::OperationPanicked)
        } else {
            SessionRecordingError::WriteFailed
        };
        self.core.fail_attempt(&self.attempt, reason);
    }
}

/// One loaded Session's ordered, best-effort JSONL recorder.
#[allow(
    dead_code,
    reason = "the loaded SessionExecutor consumes the pending Recorder seam"
)]
#[derive(Clone)]
pub(crate) struct SessionRecorder {
    core: Arc<SessionRecorderCore>,
}

#[allow(
    dead_code,
    reason = "the loaded SessionExecutor consumes the pending Recorder seam"
)]
impl SessionRecorder {
    pub(crate) fn from_published_target(
        target: PublishedConversationTarget,
        task_context: RuntimeTaskContext,
    ) -> Self {
        let target_session_id = target.session_id();
        let target_file_bytes = target.declared_file_bytes();
        let (file, writable_lease) = target.into_parts();

        // The writable lease is only a same-open binding proof. Its private File is deliberately
        // not consumed here; the published target File remains the recorder's sole append handle.
        let validation = if writable_lease.session_id() != target_session_id
            || writable_lease.declared_file_bytes() != target_file_bytes
        {
            Err(SessionRecordingError::TargetInvariant)
        } else {
            match file.metadata() {
                Ok(metadata) if metadata.len() != target_file_bytes => {
                    Err(SessionRecordingError::TargetInvariant)
                }
                Ok(metadata) if metadata.len() > MAX_CONVERSATION_FILE_BYTES => {
                    Err(SessionRecordingError::FileTooLarge)
                }
                Ok(_) => Ok(()),
                Err(_) => Err(SessionRecordingError::MetadataUnavailable),
            }
        };
        drop(writable_lease);

        let health = Arc::new(match validation {
            Ok(()) => RecordingHealth::Healthy,
            Err(reason) => RecordingHealth::Degraded {
                failed_entry_id: None,
                reason,
            },
        });
        let state = if matches!(&*health, RecordingHealth::Healthy) {
            RecorderState::Ready {
                file,
                file_bytes: target_file_bytes,
            }
        } else {
            drop(file);
            RecorderState::Degraded
        };

        Self {
            core: Arc::new(SessionRecorderCore {
                session_id: target_session_id,
                task_context,
                state: Mutex::new(state),
                health: Mutex::new(health),
                last_attempt: Mutex::new(None),
                closing: AtomicBool::new(false),
                #[cfg(test)]
                write_barrier: Mutex::new(RecorderWriteBarrier::disabled()),
            }),
        }
    }

    pub(crate) async fn record(&self, entry: Arc<StoredSessionEntry>) -> RecordOutcome {
        if self.core.closing.load(Ordering::Acquire) {
            return self.not_recorded();
        }
        // Reap the exact prior attempt (including a caller-dropped record) before admission.
        self.core.reap_last_attempt().await;

        let entry_id = entry.entry_id();
        if entry.session_id() != self.core.session_id {
            self.core
                .mark_degraded(Some(entry_id), SessionRecordingError::EntrySessionMismatch);
            return self.not_recorded();
        }
        let mut encoded = match ConversationLineCodec::encode_entry(&entry) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.core
                    .mark_degraded(Some(entry_id), SessionRecordingError::Encode(error));
                return self.not_recorded();
            }
        };
        // The physical line carries one LF; the buffer may reach the entry cap plus that LF.
        let line_len = match encoded.len().checked_add(1) {
            Some(line_len) if encoded.len() <= MAX_CONVERSATION_ENTRY_BYTES => line_len,
            _ => {
                self.core
                    .mark_degraded(Some(entry_id), SessionRecordingError::EntryTooLarge);
                return self.not_recorded();
            }
        };
        encoded.push(b'\n');
        let line_bytes = u64::try_from(line_len).expect("a recorded entry line fits u64");

        loop {
            match self.core.reserve_record(entry_id, line_bytes) {
                ReserveRecord::Start { attempt } => {
                    let result = self.core.write_entry(encoded, line_bytes, &attempt).await;
                    return match result {
                        Ok(()) => RecordOutcome::Written,
                        Err(_) => self.not_recorded(),
                    };
                }
                ReserveRecord::Wait(attempt) => {
                    self.core.reap_attempt(&attempt).await;
                    match attempt.outcome() {
                        Some(Ok(())) if self.core.may_attempt() => continue,
                        Some(Ok(())) | Some(Err(_)) | None => return self.not_recorded(),
                    }
                }
                ReserveRecord::NotRecorded(health) => {
                    return RecordOutcome::NotRecorded { health: *health };
                }
            }
        }
    }

    pub(crate) fn health(&self) -> Arc<RecordingHealth> {
        self.core.current_health()
    }

    pub(crate) async fn close(&self) {
        self.core.closing.store(true, Ordering::Release);
        loop {
            self.core.reap_last_attempt().await;
            let action = {
                let mut state = lock_recorder(&self.core.state);
                match &*state {
                    RecorderState::InFlight(attempt) => CloseAction::Wait(Arc::clone(attempt)),
                    RecorderState::Ready { .. } => {
                        let previous = std::mem::replace(&mut *state, RecorderState::Closed);
                        drop(state);
                        drop(previous);
                        CloseAction::Done
                    }
                    RecorderState::Degraded | RecorderState::Closed => {
                        *state = RecorderState::Closed;
                        CloseAction::Done
                    }
                }
            };
            match action {
                CloseAction::Done => return,
                CloseAction::Wait(attempt) => self.core.reap_attempt(&attempt).await,
            }
        }
    }

    fn not_recorded(&self) -> RecordOutcome {
        RecordOutcome::NotRecorded {
            health: *self.core.current_health(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_write_barrier_for_test(&self, barrier: Arc<RecorderWriteBarrier>) {
        self.core.set_write_barrier(barrier);
    }
}

impl fmt::Debug for SessionRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionRecorder { .. }")
    }
}

enum CloseAction {
    Done,
    Wait(Arc<RecorderAttempt>),
}

fn lock_recorder<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum StoredEntryBody {
    UserMessage(StoredUserMessage),
    AssistantMessage(StoredAssistantMessage),
    ToolMessage(StoredToolMessage),
    InteractionRequested(StoredInteractionRequest),
    InteractionResolved(StoredInteractionResolution),
    Compaction(StoredCompaction),
}

impl StoredEntryBody {
    pub(crate) fn validate_for_wire(&self) -> Result<(), StoredValueError> {
        match self {
            Self::UserMessage(value) => value.validate_for_wire(),
            Self::InteractionRequested(_) | Self::Compaction(_) => Ok(()),
            Self::AssistantMessage(value) => value.validate_for_wire(),
            Self::ToolMessage(value) => value.outcome.validate_for_wire(),
            Self::InteractionResolved(value) => value.validate_for_wire(),
        }
    }
}

impl fmt::Debug for StoredEntryBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserMessage(value) => formatter.debug_tuple("UserMessage").field(value).finish(),
            Self::AssistantMessage(value) => formatter
                .debug_tuple("AssistantMessage")
                .field(value)
                .finish(),
            Self::ToolMessage(value) => formatter.debug_tuple("ToolMessage").field(value).finish(),
            Self::InteractionRequested(value) => formatter
                .debug_tuple("InteractionRequested")
                .field(value)
                .finish(),
            Self::InteractionResolved(value) => formatter
                .debug_tuple("InteractionResolved")
                .field(value)
                .finish(),
            Self::Compaction(value) => formatter.debug_tuple("Compaction").field(value).finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredUserMessage {
    item_id: ItemId,
    source: UserMessageSource,
    content: CanonicalUserMessage,
}

impl StoredUserMessage {
    pub(crate) const fn reconstruct(
        item_id: ItemId,
        source: UserMessageSource,
        content: CanonicalUserMessage,
    ) -> Self {
        Self {
            item_id,
            source,
            content,
        }
    }

    pub(crate) const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub(crate) const fn source(&self) -> UserMessageSource {
        self.source
    }

    pub(crate) const fn content(&self) -> &CanonicalUserMessage {
        &self.content
    }

    fn validate_for_wire(&self) -> Result<(), StoredValueError> {
        self.content
            .validate_for_wire()
            .map_err(|_| StoredValueError::UserMessage)
    }
}

impl fmt::Debug for StoredUserMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredUserMessage")
            .field("item_id", &self.item_id)
            .field("source", &self.source)
            .field("content", &self.content)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredAssistantMessage {
    disposition: AssistantDisposition,
    content: Arc<[StoredAssistantContent]>,
    model: ModelResponseSummary,
    response_id: Option<ProviderResponseId>,
    finish_reason: ModelFinishReason,
    effective_max_output_tokens: NonZeroU32,
    usage: Option<ModelUsage>,
    logical_retry_count: u8,
    metadata: ProviderResponseMetadata,
}

impl StoredAssistantMessage {
    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the frozen V1 assistant shape"
    )]
    pub(crate) fn reconstruct(
        disposition: AssistantDisposition,
        content: Vec<StoredAssistantContent>,
        model: ModelResponseSummary,
        response_id: Option<ProviderResponseId>,
        finish_reason: ModelFinishReason,
        effective_max_output_tokens: NonZeroU32,
        usage: Option<ModelUsage>,
        logical_retry_count: u8,
        metadata: ProviderResponseMetadata,
    ) -> Result<Self, StoredValueError> {
        if content.is_empty() || content.len() > 128 || logical_retry_count > 3 {
            return Err(StoredValueError::Assistant);
        }
        if matches!(
            finish_reason,
            ModelFinishReason::Length | ModelFinishReason::ContentFiltered
        ) {
            return Err(StoredValueError::Assistant);
        }

        let mut item_ids = BTreeSet::new();
        let mut tool_call_ids = BTreeSet::new();
        let mut tool_call_count = 0_usize;
        let mut text_count = 0_usize;
        let mut aggregate = 0_usize;
        for item in &content {
            item.validate_for_wire()?;
            if matches!(item, StoredAssistantContent::Text { .. }) {
                text_count += 1;
            }
            if !item_ids.insert(item.item_id()) {
                return Err(StoredValueError::Assistant);
            }
            aggregate = aggregate
                .checked_add(item.semantic_bytes())
                .ok_or(StoredValueError::Assistant)?;
            if aggregate > 786_432 {
                return Err(StoredValueError::Assistant);
            }
            if let StoredAssistantContent::ToolCall { tool_call_id, .. } = item {
                tool_call_count += 1;
                if !tool_call_ids.insert(tool_call_id.clone()) {
                    return Err(StoredValueError::Assistant);
                }
            }
        }
        if (tool_call_count != 0
            && (disposition != AssistantDisposition::Intermediate
                || !matches!(
                    finish_reason,
                    ModelFinishReason::ToolCalls | ModelFinishReason::Unknown
                )))
            || (finish_reason == ModelFinishReason::ToolCalls && tool_call_count == 0)
            || (tool_call_count == 0
                && matches!(
                    finish_reason,
                    ModelFinishReason::Stop
                        | ModelFinishReason::Refused
                        | ModelFinishReason::Unknown
                )
                && text_count == 0)
        {
            return Err(StoredValueError::Assistant);
        }

        Ok(Self {
            disposition,
            content: content.into(),
            model,
            response_id,
            finish_reason,
            effective_max_output_tokens,
            usage,
            logical_retry_count,
            metadata,
        })
    }

    pub(crate) fn validate_for_wire(&self) -> Result<(), StoredValueError> {
        Self::reconstruct(
            self.disposition,
            self.content.to_vec(),
            self.model.clone(),
            self.response_id.clone(),
            self.finish_reason,
            self.effective_max_output_tokens,
            self.usage.clone(),
            self.logical_retry_count,
            self.metadata.clone(),
        )
        .map(|_| ())
    }

    pub(crate) const fn disposition(&self) -> AssistantDisposition {
        self.disposition
    }

    pub(crate) fn content(&self) -> &[StoredAssistantContent] {
        &self.content
    }

    pub(crate) const fn model(&self) -> &ModelResponseSummary {
        &self.model
    }

    pub(crate) const fn response_id(&self) -> Option<&ProviderResponseId> {
        self.response_id.as_ref()
    }

    pub(crate) const fn finish_reason(&self) -> ModelFinishReason {
        self.finish_reason
    }

    pub(crate) const fn effective_max_output_tokens(&self) -> NonZeroU32 {
        self.effective_max_output_tokens
    }

    pub(crate) const fn usage(&self) -> Option<&ModelUsage> {
        self.usage.as_ref()
    }

    pub(crate) const fn logical_retry_count(&self) -> u8 {
        self.logical_retry_count
    }

    pub(crate) const fn metadata(&self) -> &ProviderResponseMetadata {
        &self.metadata
    }
}

impl fmt::Debug for StoredAssistantMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredAssistantMessage")
            .field("disposition", &self.disposition)
            .field("content", &self.content)
            .field("model", &self.model)
            .field("has_response_id", &self.response_id.is_some())
            .field("finish_reason", &self.finish_reason)
            .field(
                "effective_max_output_tokens",
                &self.effective_max_output_tokens,
            )
            .field("has_usage", &self.usage.is_some())
            .field("logical_retry_count", &self.logical_retry_count)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum StoredAssistantContent {
    Reasoning {
        item_id: ItemId,
        content: ReasoningContent,
    },
    Text {
        item_id: ItemId,
        text: Arc<str>,
    },
    ToolCall {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}

impl StoredAssistantContent {
    pub(crate) const fn item_id(&self) -> ItemId {
        match self {
            Self::Reasoning { item_id, .. }
            | Self::Text { item_id, .. }
            | Self::ToolCall { item_id, .. } => *item_id,
        }
    }

    fn semantic_bytes(&self) -> usize {
        match self {
            Self::Reasoning { content, .. } => [
                content.text(),
                content.summary(),
                content.encrypted(),
                content.signature(),
            ]
            .into_iter()
            .flatten()
            .map(str::len)
            .sum::<usize>(),
            Self::Text { text, .. } => text.len(),
            Self::ToolCall {
                tool_call_id,
                name,
                arguments,
                ..
            } => tool_call_id
                .as_str()
                .len()
                .saturating_add(name.as_str().len())
                .saturating_add(arguments.canonical_bytes().len()),
        }
    }

    fn validate_for_wire(&self) -> Result<(), StoredValueError> {
        match self {
            Self::Text { text, .. } => {
                crate::wire::lexical::validate_safe_text(text, 65_536, false)
                    .map_err(|_| StoredValueError::Assistant)
            }
            Self::Reasoning { .. } | Self::ToolCall { .. } => Ok(()),
        }
    }
}

impl fmt::Debug for StoredAssistantContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reasoning { item_id, content } => formatter
                .debug_struct("StoredAssistantContent::Reasoning")
                .field("item_id", item_id)
                .field("content", content)
                .finish(),
            Self::Text { item_id, text } => formatter
                .debug_struct("StoredAssistantContent::Text")
                .field("item_id", item_id)
                .field("text_bytes", &text.len())
                .finish(),
            Self::ToolCall {
                item_id,
                tool_call_id: _,
                name,
                arguments,
            } => formatter
                .debug_struct("StoredAssistantContent::ToolCall")
                .field("item_id", item_id)
                .field("has_tool_call_id", &true)
                .field("name", name)
                .field("argument_bytes", &arguments.canonical_bytes().len())
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredToolMessage {
    item_id: ItemId,
    tool_call_id: ToolCallId,
    outcome: StoredToolOutcome,
}

impl StoredToolMessage {
    pub(crate) const fn reconstruct(
        item_id: ItemId,
        tool_call_id: ToolCallId,
        outcome: StoredToolOutcome,
    ) -> Self {
        Self {
            item_id,
            tool_call_id,
            outcome,
        }
    }

    pub(crate) const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub(crate) const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub(crate) const fn outcome(&self) -> &StoredToolOutcome {
        &self.outcome
    }
}

impl fmt::Debug for StoredToolMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredToolMessage")
            .field("item_id", &self.item_id)
            .field("has_tool_call_id", &true)
            .field("outcome", &self.outcome)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum StoredToolOutcome {
    Completed {
        source: ToolOutcomeSource,
        disposition: ToolResultDisposition,
        content: ToolResultContent,
    },
    Abandoned {
        reason: ToolAbandonReason,
    },
}

impl StoredToolOutcome {
    pub(crate) fn completed(
        source: ToolOutcomeSource,
        disposition: ToolResultDisposition,
        content: ToolResultContent,
    ) -> Result<Self, StoredValueError> {
        if source == ToolOutcomeSource::Executed && disposition == ToolResultDisposition::Denied {
            return Err(StoredValueError::ToolOutcome);
        }
        Ok(Self::Completed {
            source,
            disposition,
            content,
        })
    }

    fn validate_for_wire(&self) -> Result<(), StoredValueError> {
        match self {
            Self::Completed {
                source,
                disposition,
                content,
            } => Self::completed(*source, *disposition, content.clone()).map(|_| ()),
            Self::Abandoned { .. } => Ok(()),
        }
    }
}

impl fmt::Debug for StoredToolOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed {
                source,
                disposition,
                content,
            } => formatter
                .debug_struct("StoredToolOutcome::Completed")
                .field("source", source)
                .field("disposition", disposition)
                .field("content", content)
                .finish(),
            Self::Abandoned { reason } => formatter
                .debug_struct("StoredToolOutcome::Abandoned")
                .field("reason", reason)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredInteractionRequest {
    request_id: RequestId,
    item_id: ItemId,
    request: StoredInteractionRequestBody,
}

impl StoredInteractionRequest {
    pub(crate) const fn reconstruct(
        request_id: RequestId,
        item_id: ItemId,
        request: StoredInteractionRequestBody,
    ) -> Self {
        Self {
            request_id,
            item_id,
            request,
        }
    }

    pub(crate) const fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub(crate) const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub(crate) const fn request(&self) -> &StoredInteractionRequestBody {
        &self.request
    }
}

impl fmt::Debug for StoredInteractionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredInteractionRequest")
            .field("request_id", &self.request_id)
            .field("item_id", &self.item_id)
            .field("request", &self.request)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum StoredInteractionRequestBody {
    ToolApproval(ToolApprovalRequestView),
    UserQuestion(UserQuestionRequest),
}

impl fmt::Debug for StoredInteractionRequestBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ToolApproval(_) => formatter
                .debug_tuple("ToolApproval")
                .field(&"redacted")
                .finish(),
            Self::UserQuestion(_) => formatter
                .debug_tuple("UserQuestion")
                .field(&"redacted")
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredInteractionResolution {
    request_id: RequestId,
    item_id: ItemId,
    resolution: StoredInteractionResolutionBody,
    resolution_key: Option<InteractionResolutionKey>,
}

impl StoredInteractionResolution {
    pub(crate) fn reconstruct(
        request_id: RequestId,
        item_id: ItemId,
        resolution: StoredInteractionResolutionBody,
        resolution_key: Option<InteractionResolutionKey>,
    ) -> Result<Self, StoredValueError> {
        let value = Self {
            request_id,
            item_id,
            resolution,
            resolution_key,
        };
        value.validate_for_wire()?;
        Ok(value)
    }

    pub(crate) const fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub(crate) const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub(crate) const fn resolution(&self) -> &StoredInteractionResolutionBody {
        &self.resolution
    }

    pub(crate) const fn resolution_key(&self) -> Option<&InteractionResolutionKey> {
        self.resolution_key.as_ref()
    }

    pub(crate) fn validate_for_wire(&self) -> Result<(), StoredValueError> {
        let requires_resolution_key = match self.resolution {
            StoredInteractionResolutionBody::ToolApproval(_)
            | StoredInteractionResolutionBody::UserAnswer(_) => true,
            StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::HostCancelled) => {
                true
            }
            StoredInteractionResolutionBody::Cancelled(_) => false,
        };
        if requires_resolution_key != self.resolution_key.is_some() {
            return Err(StoredValueError::InteractionResolution);
        }
        Ok(())
    }
}

impl fmt::Debug for StoredInteractionResolution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredInteractionResolution")
            .field("request_id", &self.request_id)
            .field("item_id", &self.item_id)
            .field("resolution", &self.resolution)
            .field("has_resolution_key", &self.resolution_key.is_some())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum StoredInteractionResolutionBody {
    ToolApproval(ToolApprovalResolution),
    UserAnswer(UserQuestionAnswer),
    Cancelled(InteractionCancelReason),
}

impl fmt::Debug for StoredInteractionResolutionBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ToolApproval(value) => {
                formatter.debug_tuple("ToolApproval").field(value).finish()
            }
            Self::UserAnswer(value) => formatter
                .debug_struct("UserAnswer")
                .field("answers", &value.answers().len())
                .finish(),
            Self::Cancelled(value) => formatter.debug_tuple("Cancelled").field(value).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::future::Future;
    use std::io::{self, Cursor, Read, Write};
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::Poll;

    use super::*;
    use crate::agent_session_lifecycle::AgentRevisionRef;
    use crate::model_gateway::{
        ModelId, ModelReasoningSummary, ModelServiceClass, ProviderId, ProviderResponseMetadata,
    };
    use crate::tools::{ToolApprovalResolutionRef, UserQuestionFieldAnswer};
    use crate::wire::conversation_jsonl::{
        ConversationLineCodec, MAX_CONVERSATION_ENTRY_BYTES, MAX_CONVERSATION_HEADER_BYTES,
    };

    const HEADER_ONLY: &[u8] =
        include_bytes!("../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl");
    const FORK_CHILD: &[u8] = include_bytes!("../docs/fixtures/durable-store-v1/fork-child.jsonl");
    static RECORDER_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    fn recorder_target() -> (PathBuf, PublishedConversationTarget) {
        let ordinal = RECORDER_TEST_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "minicore-recorder-{}-{ordinal}.jsonl",
            std::process::id()
        ));
        let mut initial_file = std::fs::File::create(&path).expect("recorder test target creates");
        initial_file
            .write_all(HEADER_ONLY)
            .expect("recorder test header writes");
        drop(initial_file);
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .expect("recorder test target opens");
        let file_bytes = file.metadata().expect("recorder test metadata reads").len();
        let writable_file = file.try_clone().expect("recorder test file clones");
        let session_id = standard_header().session_id();
        (
            path,
            PublishedConversationTarget::from_durable_state(
                session_id,
                file_bytes,
                file,
                writable_file,
            ),
        )
    }

    async fn recorder_fixture() -> (RuntimeTaskContext, SessionRecorder, PathBuf) {
        let task_context = RuntimeTaskContext::new(tokio::runtime::Handle::current())
            .await
            .expect("the Tokio test runtime has a time driver");
        let (path, target) = recorder_target();
        let recorder = SessionRecorder::from_published_target(target, task_context.clone());
        (task_context, recorder, path)
    }

    async fn poll_to_pending<F>(future: &mut Pin<Box<F>>)
    where
        F: Future,
    {
        std::future::poll_fn(|context| match future.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("test future completed before its named barrier"),
        })
        .await;
    }

    fn recorder_entry(text: &str) -> Arc<StoredSessionEntry> {
        Arc::new(entry(StoredEntryBody::AssistantMessage(assistant(vec![
            text_content(text.to_owned()),
        ]))))
    }

    fn mismatched_recorder_entry() -> Arc<StoredSessionEntry> {
        let value = entry(StoredEntryBody::AssistantMessage(assistant(vec![
            text_content("mismatch"),
        ])));
        Arc::new(StoredSessionEntry::reconstruct(
            value.entry_id(),
            value.parent_id(),
            "ses_99999999999999999999999999999999"
                .parse()
                .expect("test Session ID is valid"),
            value.turn_id(),
            value.timestamp(),
            value.body().clone(),
        ))
    }

    async fn finish_recorder_fixture(task_context: RuntimeTaskContext, recorder: &SessionRecorder) {
        recorder.close().await;
        task_context.shutdown().await;
    }

    fn header(session_id: &str, created_at: &str, agent_id: &str) -> SessionHeader {
        SessionHeader::reconstruct(
            1,
            session_id
                .parse()
                .expect("worked-example Session ID is valid"),
            created_at
                .parse()
                .expect("worked-example timestamp is valid"),
            AgentRevisionRef::new(
                agent_id.parse().expect("worked-example Agent ID is valid"),
                "ar_1"
                    .parse()
                    .expect("worked-example Agent revision is valid"),
            ),
            "sdr_1"
                .parse()
                .expect("worked-example Session definition revision is valid"),
        )
    }

    fn standard_header() -> SessionHeader {
        header(
            "ses_11111111111111111111111111111111",
            "2026-07-31T12:00:00.000Z",
            "agt_22222222222222222222222222222222",
        )
    }

    fn fork_header() -> SessionHeader {
        header(
            "ses_33333333333333333333333333333333",
            "2026-08-03T10:02:00.789Z",
            "agt_11111111111111111111111111111111",
        )
    }

    fn classify_for_restart_cleanup(
        bytes: &[u8],
        expected_header: &SessionHeader,
        expected_shape: UnpublishedConversationRecoveryShape,
    ) -> Result<(), UnpublishedConversationRecoveryError> {
        validate_unpublished_conversation_for_recovery(
            Cursor::new(bytes.to_vec()),
            u64::try_from(bytes.len()).expect("worked-example file length fits u64"),
            expected_header,
            expected_shape,
        )
    }

    fn lines_after_standard_header(lines: &[&[u8]]) -> Vec<u8> {
        let mut bytes = HEADER_ONLY.to_vec();
        for line in lines {
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
        }
        bytes
    }

    fn fork_line(line_number: usize) -> Vec<u8> {
        FORK_CHILD
            .split(|byte| *byte == b'\n')
            .nth(line_number)
            .unwrap_or_else(|| panic!("authoritative Fork fixture has line {line_number}"))
            .to_vec()
    }

    fn fork_with_entries(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = fork_line(0);
        bytes.push(b'\n');
        for entry in entries {
            bytes.extend_from_slice(entry);
            bytes.push(b'\n');
        }
        bytes
    }

    fn additive_entry(line: &[u8]) -> Vec<u8> {
        assert!(line.ends_with(b"}}"), "canonical entry has closing braces");
        let mut output = line[..line.len() - 2].to_vec();
        output.extend_from_slice(b",\"futureField\":true}}");
        output
    }

    fn replace_once(line: &[u8], expected: &str, replacement: &str) -> Vec<u8> {
        let line = std::str::from_utf8(line).expect("worked-example line is UTF-8");
        assert!(
            line.contains(expected),
            "worked-example line contains its replacement target"
        );
        line.replacen(expected, replacement, 1).into_bytes()
    }

    fn assert_canonical_fork_entry(
        line: &[u8],
        expected_entry_id: &str,
        expected_parent_id: Option<&str>,
    ) {
        let entry =
            ConversationLineCodec::decode_entry_for_session(line, fork_header().session_id())
                .expect("rewritten Fork line remains a production-codec accepted Entry");
        assert_eq!(
            ConversationLineCodec::encode_entry(&entry)
                .expect("production codec can re-encode accepted Fork Entry"),
            line,
            "rewritten Fork line remains byte-for-byte canonical"
        );
        assert_eq!(
            entry.entry_id(),
            expected_entry_id
                .parse::<EntryId>()
                .expect("expected Entry ID is valid")
        );
        assert_eq!(
            entry.parent_id(),
            expected_parent_id.map(|parent_id| {
                parent_id
                    .parse::<EntryId>()
                    .expect("expected parent Entry ID is valid")
            })
        );
    }

    struct UnavailableReader;

    impl Read for UnavailableReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("private read failure"))
        }
    }

    #[test]
    fn unpublished_conversation_recovery_accepts_the_canonical_header_only_shapes() {
        let expected_header = standard_header();
        for expected_shape in [
            UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
        ] {
            assert_eq!(
                classify_for_restart_cleanup(HEADER_ONLY, &expected_header, expected_shape),
                Ok(())
            );
        }
    }

    #[test]
    fn unpublished_conversation_recovery_accepts_the_authoritative_canonical_fork_child() {
        assert_eq!(
            classify_for_restart_cleanup(
                FORK_CHILD,
                &fork_header(),
                UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
            ),
            Ok(())
        );
    }

    #[test]
    fn unpublished_conversation_recovery_accepts_an_authoritative_fork_header_and_first_entry_prefix_for_cleanup_only()
     {
        let first_entry_prefix = fork_with_entries(&[fork_line(1)]);
        // This says the prefix can be safely classified for restart cleanup only; it does not
        // prove that the original Fork fully materialized.
        assert_eq!(
            classify_for_restart_cleanup(
                &first_entry_prefix,
                &fork_header(),
                UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
            ),
            Ok(())
        );
    }

    #[test]
    fn unpublished_conversation_recovery_rejects_entries_for_an_ordinary_header_only_session() {
        let first_fork_entry = fork_line(1);
        let bytes = lines_after_standard_header(&[&replace_once(
            &first_fork_entry,
            "ses_33333333333333333333333333333333",
            "ses_11111111111111111111111111111111",
        )]);
        assert_eq!(
            classify_for_restart_cleanup(
                &bytes,
                &standard_header(),
                UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );
    }

    #[test]
    fn unpublished_conversation_recovery_rejects_an_expected_header_mismatch() {
        let different_expected_header = header(
            "ses_11111111111111111111111111111111",
            "2026-07-31T12:00:01.000Z",
            "agt_22222222222222222222222222222222",
        );
        assert_eq!(
            classify_for_restart_cleanup(
                HEADER_ONLY,
                &different_expected_header,
                UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );
    }

    #[test]
    fn unpublished_conversation_recovery_classifies_header_failures_as_corrupt() {
        let expected_header = standard_header();
        for bytes in [
            b"".as_slice(),
            b"not JSON\n".as_slice(),
            include_bytes!(
                "../docs/fixtures/wire-v1/conversation/corruption/duplicate-header-key.jsonl"
            )
            .as_slice(),
            include_bytes!(
                "../docs/fixtures/wire-v1/conversation/corruption/unsupported-version-header.jsonl"
            )
            .as_slice(),
            include_bytes!(
                "../docs/fixtures/wire-v1/conversation/corruption/wrong-session-header.jsonl"
            )
            .as_slice(),
        ] {
            assert_eq!(
                classify_for_restart_cleanup(
                    bytes,
                    &expected_header,
                    UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
                ),
                Err(UnpublishedConversationRecoveryError::Corrupt)
            );
        }
    }

    #[test]
    fn unpublished_conversation_recovery_rejects_noncanonical_header_and_accepted_entry_lines() {
        let expected_header = standard_header();
        let mut crlf_header = HEADER_ONLY
            .strip_suffix(b"\n")
            .expect("authoritative header has LF")
            .to_vec();
        crlf_header.extend_from_slice(b"\r\n");
        assert_eq!(
            classify_for_restart_cleanup(
                &crlf_header,
                &expected_header,
                UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );

        let mut header_with_trailing_whitespace = HEADER_ONLY
            .strip_suffix(b"\n")
            .expect("authoritative header has LF")
            .to_vec();
        header_with_trailing_whitespace.extend_from_slice(b" \n");
        assert_eq!(
            classify_for_restart_cleanup(
                &header_with_trailing_whitespace,
                &expected_header,
                UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );

        let entry = replace_once(
            &fork_line(1),
            "ses_33333333333333333333333333333333",
            "ses_11111111111111111111111111111111",
        );
        let mut crlf_entry = HEADER_ONLY.to_vec();
        crlf_entry.extend_from_slice(&entry);
        crlf_entry.extend_from_slice(b"\r\n");
        assert_eq!(
            classify_for_restart_cleanup(
                &crlf_entry,
                &expected_header,
                UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );

        let additive = additive_entry(&entry);
        let additive = lines_after_standard_header(&[&additive]);
        assert_eq!(
            classify_for_restart_cleanup(
                &additive,
                &expected_header,
                UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );

        let salvage = include_bytes!(
            "../docs/fixtures/wire-v1/conversation/corruption/contribution-stamp-salvage.jsonl"
        );
        let salvage_header = header(
            "ses_16161616161616161616161616161616",
            "2026-07-31T18:00:00.000Z",
            "agt_27272727272727272727272727272727",
        );
        assert_eq!(
            classify_for_restart_cleanup(
                salvage,
                &salvage_header,
                UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );
    }

    #[test]
    fn unpublished_conversation_recovery_classifies_complete_faults_tails_and_caps() {
        for line in [
            b"not JSON".as_slice(),
            b"{\"type\":\"future_record\",\"data\":{}}".as_slice(),
        ] {
            let bytes = lines_after_standard_header(&[line]);
            assert_eq!(
                classify_for_restart_cleanup(
                    &bytes,
                    &standard_header(),
                    UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
                ),
                Err(UnpublishedConversationRecoveryError::Corrupt)
            );
        }

        let mut partial_tail = HEADER_ONLY.to_vec();
        partial_tail.extend_from_slice(b"partial record");
        assert_eq!(
            classify_for_restart_cleanup(
                &partial_tail,
                &standard_header(),
                UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
            ),
            Err(UnpublishedConversationRecoveryError::Corrupt)
        );

        let mut oversized_entry = HEADER_ONLY.to_vec();
        oversized_entry.extend(std::iter::repeat_n(b' ', MAX_CONVERSATION_ENTRY_BYTES + 1));
        oversized_entry.push(b'\n');
        assert_eq!(
            classify_for_restart_cleanup(
                &oversized_entry,
                &standard_header(),
                UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
            ),
            Err(UnpublishedConversationRecoveryError::TooLarge)
        );

        let header_without_lf = HEADER_ONLY
            .strip_suffix(b"\n")
            .expect("authoritative header has LF");
        let mut oversized_header = header_without_lf.to_vec();
        oversized_header.extend(std::iter::repeat_n(
            b' ',
            MAX_CONVERSATION_HEADER_BYTES - header_without_lf.len() + 1,
        ));
        oversized_header.push(b'\n');
        assert_eq!(
            classify_for_restart_cleanup(
                &oversized_header,
                &standard_header(),
                UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            ),
            Err(UnpublishedConversationRecoveryError::TooLarge)
        );
    }

    #[test]
    fn unpublished_conversation_recovery_rejects_canonical_fork_second_root_orphan_nonlinear_and_duplicate_entries()
     {
        let root = fork_line(1);
        let child = fork_line(2);
        let second_root = replace_once(
            &child,
            "\"parentId\":\"ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            "\"parentId\":null",
        );
        let orphan = replace_once(
            &child,
            "ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ent_cccccccccccccccccccccccccccccccc",
        );
        let third_from_root = replace_once(
            &child,
            "ent_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "ent_cccccccccccccccccccccccccccccccc",
        );
        let duplicate_id = replace_once(
            &child,
            "ent_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );

        // Attribute each rejection to relationship or identity rules, rather than a replacement
        // mistake or a codec failure: every rewritten line remains canonical and accepted by the
        // production Entry codec before recovery classification.
        assert_canonical_fork_entry(&second_root, "ent_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", None);
        assert_canonical_fork_entry(
            &orphan,
            "ent_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            Some("ent_cccccccccccccccccccccccccccccccc"),
        );
        assert_canonical_fork_entry(
            &third_from_root,
            "ent_cccccccccccccccccccccccccccccccc",
            Some("ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        assert_canonical_fork_entry(
            &duplicate_id,
            "ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );

        for (case, entries) in [
            ("second root", vec![root.clone(), second_root]),
            ("orphan", vec![root.clone(), orphan]),
            (
                "nonlinear third entry from root",
                vec![root.clone(), child.clone(), third_from_root],
            ),
            ("duplicate entry ID", vec![root, duplicate_id]),
        ] {
            let bytes = fork_with_entries(&entries);
            assert_eq!(
                classify_for_restart_cleanup(
                    &bytes,
                    &fork_header(),
                    UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
                ),
                Err(UnpublishedConversationRecoveryError::Corrupt),
                "canonical Fork {case} is corrupt for recovery classification"
            );
        }
    }

    #[test]
    fn unpublished_conversation_recovery_maps_input_availability_without_details() {
        let expected_header = standard_header();
        assert_eq!(
            validate_unpublished_conversation_for_recovery(
                UnavailableReader,
                u64::try_from(HEADER_ONLY.len()).expect("fixture length fits u64"),
                &expected_header,
                UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            ),
            Err(UnpublishedConversationRecoveryError::Unavailable)
        );
        assert_eq!(
            validate_unpublished_conversation_for_recovery(
                Cursor::new(HEADER_ONLY.to_vec()),
                u64::try_from(HEADER_ONLY.len() + 1).expect("fixture length fits u64"),
                &expected_header,
                UnpublishedConversationRecoveryShape::OrdinaryHeaderOnly,
            ),
            Err(UnpublishedConversationRecoveryError::Unavailable)
        );
    }

    #[test]
    fn unpublished_conversation_recovery_maps_scanner_counter_overflow_to_unavailable() {
        assert_eq!(
            map_unpublished_conversation_recovery_scan_error(
                ConversationScanError::CounterOverflow
            ),
            UnpublishedConversationRecoveryError::Unavailable
        );
    }

    #[test]
    fn unpublished_conversation_recovery_errors_are_redacted() {
        let bytes = lines_after_standard_header(&[
            b"secret conversation text /private/path ent_99999999999999999999999999999999",
        ]);
        let error = classify_for_restart_cleanup(
            &bytes,
            &standard_header(),
            UnpublishedConversationRecoveryShape::ForkCanonicalLinearFile,
        )
        .expect_err("complete malformed entry is corrupt");
        for secret in [
            "ses_11111111111111111111111111111111",
            "ent_99999999999999999999999999999999",
            "secret conversation text",
            "/private/path",
            "MalformedJson",
            "HeaderCorrupt",
        ] {
            assert!(
                !format!("{error:?}").contains(secret) && !error.to_string().contains(secret),
                "unpublished conversation recovery error leaked {secret}"
            );
        }
    }

    fn model() -> ModelResponseSummary {
        ModelResponseSummary::reconstruct(
            "fixture".parse::<ProviderId>().unwrap(),
            "scripted".parse::<ModelId>().unwrap(),
            ModelReasoningSummary::Disabled,
            ModelServiceClass::Standard,
        )
    }

    fn metadata() -> ProviderResponseMetadata {
        ProviderResponseMetadata::reconstruct(None, None, None)
    }

    fn text_content(text: impl Into<Arc<str>>) -> StoredAssistantContent {
        StoredAssistantContent::Text {
            item_id: "itm_11111111111111111111111111111111".parse().unwrap(),
            text: text.into(),
        }
    }

    fn reasoning_content() -> StoredAssistantContent {
        StoredAssistantContent::Reasoning {
            item_id: "itm_11111111111111111111111111111111".parse().unwrap(),
            content: ReasoningContent::reconstruct(
                None,
                Some("brief".to_owned()),
                None,
                None,
                None,
            )
            .unwrap(),
        }
    }

    fn assistant(content: Vec<StoredAssistantContent>) -> StoredAssistantMessage {
        StoredAssistantMessage::reconstruct(
            AssistantDisposition::Final,
            content,
            model(),
            None,
            ModelFinishReason::Stop,
            NonZeroU32::new(1).unwrap(),
            None,
            0,
            metadata(),
        )
        .unwrap()
    }

    fn entry(body: StoredEntryBody) -> StoredSessionEntry {
        StoredSessionEntry::reconstruct(
            "ent_11111111111111111111111111111111".parse().unwrap(),
            None,
            "ses_11111111111111111111111111111111".parse().unwrap(),
            "trn_11111111111111111111111111111111".parse().unwrap(),
            "2026-07-31T12:00:00.000Z".parse().unwrap(),
            body,
        )
    }

    fn resolution(
        resolution: StoredInteractionResolutionBody,
        resolution_key: Option<InteractionResolutionKey>,
    ) -> Result<StoredInteractionResolution, StoredValueError> {
        StoredInteractionResolution::reconstruct(
            "req_11111111111111111111111111111111".parse().unwrap(),
            "itm_11111111111111111111111111111111".parse().unwrap(),
            resolution,
            resolution_key,
        )
    }

    fn resolution_key() -> InteractionResolutionKey {
        "irk_11111111111111111111111111111111".parse().unwrap()
    }

    #[test]
    fn assistant_text_construction_and_encoder_defense_share_the_wire_contract() {
        for text in [
            "".to_owned(),
            "unsafe\u{001b}".to_owned(),
            "x".repeat(65_537),
        ] {
            assert_eq!(
                StoredAssistantMessage::reconstruct(
                    AssistantDisposition::Final,
                    vec![text_content(text)],
                    model(),
                    None,
                    ModelFinishReason::Stop,
                    NonZeroU32::new(1).unwrap(),
                    None,
                    0,
                    metadata(),
                ),
                Err(StoredValueError::Assistant)
            );
        }

        let mut malformed = assistant(vec![text_content("safe")]);
        malformed.content = vec![text_content("")].into();
        assert_eq!(
            ConversationLineCodec::encode_entry(&entry(StoredEntryBody::AssistantMessage(
                malformed
            ))),
            Err(crate::wire::conversation_jsonl::ConversationCodecError::InvalidSemantic)
        );

        for finish_reason in [
            ModelFinishReason::Stop,
            ModelFinishReason::Refused,
            ModelFinishReason::Unknown,
        ] {
            assert_eq!(
                StoredAssistantMessage::reconstruct(
                    AssistantDisposition::Final,
                    vec![reasoning_content()],
                    model(),
                    None,
                    finish_reason,
                    NonZeroU32::new(1).unwrap(),
                    None,
                    0,
                    metadata(),
                ),
                Err(StoredValueError::Assistant)
            );
        }
    }

    #[test]
    fn stored_resolution_uses_owner_specific_values_and_enforces_key_matrix_on_write() {
        let denied = ToolApprovalResolution::reconstruct_denied();
        assert_eq!(
            resolution(StoredInteractionResolutionBody::ToolApproval(denied), None),
            Err(StoredValueError::InteractionResolution)
        );
        let approval = resolution(
            StoredInteractionResolutionBody::ToolApproval(denied),
            Some(resolution_key()),
        )
        .unwrap();
        assert!(matches!(
            approval.resolution(),
            StoredInteractionResolutionBody::ToolApproval(value)
                if value.as_ref() == ToolApprovalResolutionRef::Denied
        ));

        let answer =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "answer").unwrap()])
                .unwrap();
        assert_eq!(
            resolution(
                StoredInteractionResolutionBody::UserAnswer(answer.clone()),
                None
            ),
            Err(StoredValueError::InteractionResolution)
        );
        assert!(
            resolution(
                StoredInteractionResolutionBody::UserAnswer(answer),
                Some(resolution_key())
            )
            .is_ok()
        );

        assert_eq!(
            resolution(
                StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::HostCancelled),
                None,
            ),
            Err(StoredValueError::InteractionResolution)
        );
        assert!(
            resolution(
                StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::HostCancelled),
                Some(resolution_key()),
            )
            .is_ok()
        );
        assert!(
            resolution(
                StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::TurnCancelled),
                None,
            )
            .is_ok()
        );
        assert_eq!(
            resolution(
                StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::TurnCancelled),
                Some(resolution_key()),
            ),
            Err(StoredValueError::InteractionResolution)
        );

        let mut owner_cancelled = resolution(
            StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::TurnCancelled),
            None,
        )
        .unwrap();
        owner_cancelled.resolution_key = Some(resolution_key());
        assert_eq!(
            ConversationLineCodec::encode_entry(&entry(StoredEntryBody::InteractionResolved(
                owner_cancelled
            ))),
            Err(crate::wire::conversation_jsonl::ConversationCodecError::InvalidSemantic)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_appends_canonical_entry_bytes_and_one_lf() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let entry = recorder_entry("canonical");
        let encoded = ConversationLineCodec::encode_entry(&entry).expect("entry encodes");

        assert_eq!(recorder.record(entry).await, RecordOutcome::Written);
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        finish_recorder_fixture(task_context, &recorder).await;

        let mut expected = HEADER_ONLY.to_vec();
        expected.extend_from_slice(&encoded);
        expected.push(b'\n');
        assert_eq!(
            std::fs::read(&path).expect("recorder test target reads"),
            expected
        );
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_constructor_invariant_mismatch_is_terminally_degraded() {
        let task_context = RuntimeTaskContext::new(tokio::runtime::Handle::current())
            .await
            .expect("the Tokio test runtime has a time driver");
        let (path, target) = recorder_target();
        let (file, original_lease) = target.into_parts();
        let target_session_id = standard_header().session_id();
        let declared_file_bytes = file.metadata().expect("recorder test metadata reads").len();
        let invalid_declared_file_bytes = declared_file_bytes + 1;
        let writable_file = file.try_clone().expect("recorder test file clones");
        drop(original_lease);
        let target = PublishedConversationTarget {
            session_id: target_session_id,
            declared_file_bytes: invalid_declared_file_bytes,
            file,
            writable_lease: ExclusiveWritableConversationLease::from_durable_state(
                target_session_id,
                invalid_declared_file_bytes,
                writable_file,
            ),
        };
        let recorder = SessionRecorder::from_published_target(target, task_context.clone());
        assert!(matches!(
            *recorder.health(),
            RecordingHealth::Degraded {
                failed_entry_id: None,
                reason: SessionRecordingError::TargetInvariant,
            }
        ));
        assert!(matches!(
            recorder.record(recorder_entry("not written")).await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded { .. }
            }
        ));
        finish_recorder_fixture(task_context.clone(), &recorder).await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        assert_eq!(
            std::fs::read(&path).expect("recorder test target reads"),
            HEADER_ONLY
        );
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_allows_only_one_in_flight_append() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));

        let mut first = Box::pin(recorder.record(recorder_entry("first")));
        poll_to_pending(&mut first).await;
        barrier.wait_until_entered().await;

        let mut second = Box::pin(recorder.record(recorder_entry("second")));
        poll_to_pending(&mut second).await;
        assert_eq!(
            task_context.registered_task_count_for_test(),
            1,
            "only the one admitted blocking append is registered while the second waits"
        );

        barrier.release();
        let (first_outcome, second_outcome) = tokio::join!(first, second);
        assert_eq!(first_outcome, RecordOutcome::Written);
        assert_eq!(second_outcome, RecordOutcome::Written);

        finish_recorder_fixture(task_context.clone(), &recorder).await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        let bytes = std::fs::read(&path).expect("recorder test target reads");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 3);
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_degraded_health_stops_the_suffix() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        barrier.fail_before_write();

        let first = recorder.record(recorder_entry("failed"));
        assert!(matches!(
            first.await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded { .. }
            }
        ));
        let after_failure = recorder.record(recorder_entry("suffix")).await;
        assert!(matches!(
            after_failure,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded { .. }
            }
        ));

        recorder.close().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        task_context.shutdown().await;
        assert_eq!(
            std::fs::read(&path).expect("recorder test target reads"),
            HEADER_ONLY
        );
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_worker_panic_degrades_and_settles() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        barrier.panic_before_write();

        assert!(matches!(
            recorder.record(recorder_entry("panic")).await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded { .. }
            }
        ));
        recorder.close().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        task_context.shutdown().await;
        assert_eq!(
            std::fs::read(&path).expect("recorder test target reads"),
            HEADER_ONLY
        );
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_failure_completion_is_exact_or_inert() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let entry = recorder_entry("exactness");

        // Publish the exact current attempt exactly as `record` does.
        let ReserveRecord::Start { attempt } = recorder.core.reserve_record(entry.entry_id(), 1)
        else {
            panic!("the fresh recorder admits its first attempt");
        };

        // A stale completion of a previous attempt must not touch the current state, health,
        // or its own file; it still settles its own attempt outcome.
        let (path_stale, target_stale) = recorder_target();
        let (file_stale, _) = target_stale.into_parts();
        let stale = RecorderAttempt::new(file_stale, 0, entry.entry_id());
        recorder
            .core
            .fail_attempt(&stale, SessionRecordingError::WriteFailed);
        assert!(
            lock_recorder(&stale.file).is_some(),
            "a stale completion does not consume its file"
        );
        assert_eq!(
            stale.outcome(),
            Some(Err(SessionRecordingError::WriteFailed)),
            "a stale completion still settles its own attempt outcome"
        );
        assert!(
            matches!(*recorder.health(), RecordingHealth::Healthy),
            "a stale completion never degrades the current health"
        );
        assert!(
            matches!(
                &*lock_recorder(&recorder.core.state),
                RecorderState::InFlight(current) if Arc::ptr_eq(current, &attempt)
            ),
            "a stale completion never touches the current state"
        );

        // Duplicate completion of the exact attempt while the state already degraded but
        // `last_attempt` still retains it: the retained attempt is the exact owner and may
        // install the sticky health.
        *lock_recorder(&recorder.core.state) = RecorderState::Degraded;
        recorder
            .core
            .fail_attempt(&attempt, SessionRecordingError::TargetInvariant);
        assert!(matches!(
            *recorder.health(),
            RecordingHealth::Degraded {
                reason: SessionRecordingError::TargetInvariant,
                ..
            }
        ));
        assert_eq!(
            attempt.outcome(),
            Some(Err(SessionRecordingError::TargetInvariant))
        );

        // Clean up: install the exact job so the close reaper settles the retained attempt.
        attempt.store_job(task_context.spawn_blocking_tracked(|| ()));
        finish_recorder_fixture(task_context.clone(), &recorder).await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        std::fs::remove_file(path).expect("recorder test target removes");
        std::fs::remove_file(path_stale).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_spawn_rejection_degrades_without_an_append() {
        let (task_context, recorder, path) = recorder_fixture().await;
        task_context.request_closing();

        assert!(matches!(
            recorder.record(recorder_entry("not started")).await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded {
                    reason: SessionRecordingError::Runtime(_),
                    ..
                }
            }
        ));
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        recorder.close().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        task_context.shutdown().await;
        assert_eq!(
            std::fs::read(&path).expect("recorder test target reads"),
            HEADER_ONLY
        );
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_caller_drop_still_settles_the_owner_work() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));

        let mut dropped = Box::pin(recorder.record(recorder_entry("caller drop")));
        poll_to_pending(&mut dropped).await;
        barrier.wait_until_entered().await;
        drop(dropped);

        barrier.release();
        recorder.close().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        task_context.shutdown().await;

        let bytes = std::fs::read(&path).expect("recorder test target reads");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_cancelled_close_keeps_the_exact_attempt_anchor() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));

        let mut record = Box::pin(recorder.record(recorder_entry("cancelled close")));
        poll_to_pending(&mut record).await;
        barrier.wait_until_entered().await;

        let mut close = Box::pin(recorder.close());
        poll_to_pending(&mut close).await;
        assert!(
            lock_recorder(&recorder.core.last_attempt).is_some(),
            "close is reaping the retained in-flight attempt"
        );
        drop(close);

        // Cancelling a reaper while it awaits must not consume the retained exact-job anchor;
        // otherwise a later operation could bypass reaping the completed raw task.
        assert!(
            lock_recorder(&recorder.core.last_attempt).is_some(),
            "a cancelled reaper leaves the exact attempt anchor in place"
        );

        barrier.release();
        assert_eq!(record.await, RecordOutcome::Written);

        // The next close still reaps the exact job the cancelled close left behind.
        recorder.close().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        task_context.shutdown().await;
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[test]
    fn session_recorder_pre_poll_abort_is_settled_by_reap_without_hanging() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .expect("the recorder abort test runtime builds");
        runtime.block_on(async {
            let (task_context, recorder, path) = recorder_fixture().await;

            // Occupy the single blocking-pool thread so the append worker stays queued and can
            // be aborted before its first poll: its guard then never runs, and no worker
            // outcome is ever recorded.
            let blocker_entered = Arc::new(std::sync::Barrier::new(2));
            let blocker_release = Arc::new(std::sync::Barrier::new(2));
            let entered_by_blocker = Arc::clone(&blocker_entered);
            let released_by_blocker = Arc::clone(&blocker_release);
            let _blocker = task_context.spawn_blocking_tracked(move || {
                entered_by_blocker.wait();
                released_by_blocker.wait();
            });
            blocker_entered.wait();

            let mut record = Box::pin(recorder.record(recorder_entry("pre-poll abort")));
            poll_to_pending(&mut record).await;
            // Drop the caller before it reaps: the dropped join guard restores the raw handle,
            // and the still-queued worker stays unstarted.
            drop(record);

            task_context.abort_latest_registered_task();
            blocker_release.wait();

            // Reaping must wait the exact tracked job first; the aborted join failure settles
            // the attempt that no worker guard can settle.
            recorder.close().await;
            assert!(matches!(
                *recorder.health(),
                RecordingHealth::Degraded {
                    reason: SessionRecordingError::Runtime(_),
                    ..
                }
            ));

            task_context.shutdown().await;
            assert_eq!(task_context.registered_task_count_for_test(), 0);
            assert_eq!(
                std::fs::read(&path).expect("recorder test target reads"),
                HEADER_ONLY
            );
            std::fs::remove_file(path).expect("recorder test target removes");
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_reaper_parks_until_the_exact_job_install() {
        let (task_context, recorder, path) = recorder_fixture().await;

        // Publish the exact attempt (InFlight + last_attempt) exactly as `record` does, but
        // hold the job back: a reaper that starts inside the publication-to-install window must
        // park on the install instead of misclassifying the empty slot as WorkerUnavailable.
        let entry = recorder_entry("held job");
        let ReserveRecord::Start { attempt } = recorder.core.reserve_record(entry.entry_id(), 1)
        else {
            panic!("the fresh recorder admits its first attempt");
        };

        let mut reaper = Box::pin(recorder.core.reap_attempt(&attempt));
        poll_to_pending(&mut reaper).await;
        assert!(
            attempt.job().is_none(),
            "the reaper parks on the job install instead of failing the empty slot"
        );
        assert!(
            matches!(*recorder.health(), RecordingHealth::Healthy),
            "the parked reaper did not settle the attempt"
        );
        assert!(
            lock_recorder(&recorder.core.last_attempt).is_some(),
            "the parked reaper keeps the exact attempt anchor"
        );

        // The publisher's synchronous install (the production `write_entry` has no await
        // between publication and this call) releases the parked reaper.
        attempt.store_job(task_context.spawn_blocking_tracked(|| ()));
        reaper.await;

        // The reaper settled the exact attempt: job installed, outcome recorded, anchor cleared.
        assert!(attempt.job().is_some(), "the exact job is retained");
        assert!(
            attempt.outcome().is_some(),
            "the reaper settles the exact attempt outcome"
        );
        assert!(
            lock_recorder(&recorder.core.last_attempt).is_none(),
            "the finished reaper clears the exact attempt anchor"
        );
        assert!(matches!(
            *recorder.health(),
            RecordingHealth::Degraded {
                reason: SessionRecordingError::WriteFailed,
                ..
            }
        ));

        assert_eq!(task_context.registered_task_count_for_test(), 0);
        finish_recorder_fixture(task_context, &recorder).await;
        assert_eq!(
            std::fs::read(&path).expect("recorder test target reads"),
            HEADER_ONLY
        );
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_close_waits_for_in_flight_and_is_idempotent() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));

        let mut record = Box::pin(recorder.record(recorder_entry("close")));
        poll_to_pending(&mut record).await;
        barrier.wait_until_entered().await;

        let mut close = Box::pin(recorder.close());
        poll_to_pending(&mut close).await;
        assert!(matches!(
            recorder
                .record(recorder_entry("after close admission"))
                .await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Healthy
            }
        ));
        barrier.release();
        let (outcome, ()) = tokio::join!(record, close);
        assert_eq!(outcome, RecordOutcome::Written);
        assert_eq!(task_context.registered_task_count_for_test(), 0);

        recorder.close().await;
        assert!(matches!(
            recorder.record(recorder_entry("after close")).await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Healthy
            }
        ));
        task_context.shutdown().await;
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recorder_debug_and_display_redact_entry_identity() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let outcome = recorder.record(mismatched_recorder_entry()).await;
        let health = recorder.health();
        let debug = format!("{health:?} {health} {outcome:?} {outcome}");
        for secret in [
            "ses_99999999999999999999999999999999",
            "ent_11111111111111111111111111111111",
            "mismatch",
        ] {
            assert!(!debug.contains(secret), "Recorder output leaked {secret}");
        }
        assert!(matches!(
            *health,
            RecordingHealth::Degraded {
                failed_entry_id: Some(_),
                reason: SessionRecordingError::EntrySessionMismatch,
            }
        ));

        finish_recorder_fixture(task_context.clone(), &recorder).await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        std::fs::remove_file(path).expect("recorder test target removes");
    }
}
