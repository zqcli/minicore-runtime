use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
#[cfg(test)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio::sync::Notify;

use crate::agent_session_lifecycle::{AgentRevisionRef, ForkAnchor, ForkSourceKind};
use crate::compaction::{
    CompactionUnitKind, LiveCompactionUnit, PreparedLiveCompactionUnit, StoredCompaction,
};
use crate::model_gateway::{
    ModelFinishReason, ModelResponseSummary, ModelUsage, ProviderResponseId,
    ProviderResponseMetadata, ReasoningContent,
};
use crate::prompt::{CanonicalUserMessage, ModelAssistantContent, ModelMessage};
use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedBlockingJob};
use crate::tools::{
    ToolAbandonReason, ToolApprovalRequestView, ToolApprovalResolution, ToolCallId, ToolName,
    ToolOutcomeSource, ToolResultContent, ToolResultDisposition, UserQuestionAnswer,
    UserQuestionRequest,
};
use crate::turn_item_interaction::{
    AssistantDisposition, InteractionCancelReason, ItemRelation, UserMessageSource,
};
use crate::wire::conversation_jsonl::{
    ConversationCodecError, ConversationDecodeFacts, ConversationLineCodec,
    MAX_CONVERSATION_ENTRY_BYTES,
};
use crate::wire::conversation_jsonl_scanner::{
    ConversationJsonlScanner, ConversationLineCanonicality, ConversationLineFault,
    ConversationPartialTailAction, ConversationPhysicalLocation, ConversationScanAccess,
    ConversationScanError, ConversationScanEvent, MAX_CONVERSATION_ENTRY_RECORDS,
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

use live_conversation::{ConversationRevision, LiveSessionState};

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
/// identity and physical length across a proven same-file append/truncation handle pair. The proof
/// does not acquire, hold, or emulate an OS lock; Conversation Storage only consumes its private
/// truncation-capable handle when it needs to perform the corresponding write work.
#[allow(
    dead_code,
    reason = "the DurableState-issued truncation handle is consumed by the pending Recorder seam"
)]
pub(crate) struct ExclusiveWritableConversationLease {
    session_id: SessionId,
    declared_file_bytes: u64,
    file: Option<File>,
}

#[allow(
    dead_code,
    reason = "the DurableState-issued truncation handle is consumed by the pending Recorder seam"
)]
impl ExclusiveWritableConversationLease {
    /// Constructs the production proof from DurableState's same-file truncation-capable handle.
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

    /// Gives the truncation handle only to Conversation Storage's internal consumer.
    #[cfg(test)]
    pub(crate) fn into_file(self) -> Option<File> {
        self.file
    }

    /// Truncates only the final partial tail offset returned by the paired writable scan and
    /// updates the proof's declared length for the Recorder that will consume these exact parts.
    /// No caller can provide an arbitrary writable proof or offset.
    fn truncate_to(&mut self, offset: u64) -> Result<(), ()> {
        if offset > self.declared_file_bytes {
            return Err(());
        }
        let file = self.file.as_mut().ok_or(())?;
        if file.metadata().map_err(|_| ())?.len() != self.declared_file_bytes {
            return Err(());
        }
        file.set_len(offset).map_err(|_| ())?;
        if file.metadata().map_err(|_| ())?.len() != offset {
            return Err(());
        }
        self.declared_file_bytes = offset;
        Ok(())
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
/// The target keeps only the Session identity, the declared length, an append-capable handle, and
/// its proven same-file truncation proof. It never stores or exposes the target path. Conversation
/// Storage may consume the pair; lifecycle callers see only this opaque capability.
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
    /// Constructs the production target and paired proof from DurableState's same-file handles.
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
    #[error("recorded Fork anchor is invalid for the selected source path")]
    InvalidAnchor,
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

    pub(crate) fn capture_selected_path(
        &mut self,
        expected_header: &SessionHeader,
        anchor: ForkAnchor,
    ) -> Result<CapturedForkConversation, RecordedForkSourceError> {
        let view = replay_conversation(
            &mut self.file,
            self.declared_file_bytes,
            self.source_session_id,
            ConversationScanAccess::ReadOnly,
        )
        .map_err(map_recorded_fork_replay_error)?;
        if view.header() != expected_header || !view.header_is_canonical() {
            return Err(RecordedForkSourceError::Corrupt);
        }
        CapturedForkConversation::from_selected_path(
            self.source_session_id,
            ForkSourceKind::RecordedHistory,
            anchor,
            view.selected_entries(),
            view.relations(),
        )
        .map_err(|error| match error {
            ForkAnchorResolutionError::InvalidAnchor => RecordedForkSourceError::InvalidAnchor,
            ForkAnchorResolutionError::TooLarge => RecordedForkSourceError::TooLarge,
            ForkAnchorResolutionError::InvalidSource
            | ForkAnchorResolutionError::Encode
            | ForkAnchorResolutionError::Unavailable => RecordedForkSourceError::Unavailable,
        })
    }

    pub(crate) fn handle_metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }
}

fn map_recorded_fork_replay_error(error: ConversationReplayError) -> RecordedForkSourceError {
    match error {
        ConversationReplayError::HistoryTooLarge => RecordedForkSourceError::TooLarge,
        ConversationReplayError::HeaderCorrupt
        | ConversationReplayError::UnsupportedFormatVersion
        | ConversationReplayError::MissingHeader => RecordedForkSourceError::Corrupt,
        ConversationReplayError::LeaseMismatch
        | ConversationReplayError::InputChanged
        | ConversationReplayError::InputUnavailable
        | ConversationReplayError::CounterOverflow
        | ConversationReplayError::InvariantViolation => RecordedForkSourceError::Unavailable,
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ForkAnchorResolutionError {
    #[error("Fork anchor is invalid for the selected source path")]
    InvalidAnchor,
    #[error("Fork source path violates its immutable identity contract")]
    InvalidSource,
    #[error("Fork child conversation exceeds its selected storage limit")]
    TooLarge,
    #[error("Fork child conversation cannot be encoded")]
    Encode,
    #[error("Fork child conversation is unavailable")]
    Unavailable,
}

/// One immutable, anchor-resolved source path captured at the Fork linearization point.
///
/// The selected entries retain their original Arcs and stable historical identities. The value
/// contains no file handle, path, child identity, or publication authority.
#[derive(Clone)]
pub(crate) struct CapturedForkConversation {
    source_session_id: SessionId,
    source: ForkSourceKind,
    anchor: ForkAnchor,
    selected_entries: Arc<[Arc<StoredSessionEntry>]>,
}

impl CapturedForkConversation {
    fn from_selected_path(
        source_session_id: SessionId,
        source: ForkSourceKind,
        anchor: ForkAnchor,
        selected_entries: &[Arc<StoredSessionEntry>],
        relations: &[ItemRelation],
    ) -> Result<Self, ForkAnchorResolutionError> {
        let prefix_len = resolve_fork_anchor(selected_entries, relations, &anchor)
            .ok_or(ForkAnchorResolutionError::InvalidAnchor)?;
        let selected_entries = selected_entries[..prefix_len].to_vec();
        validate_captured_fork_path(source_session_id, &selected_entries)?;
        Ok(Self {
            source_session_id,
            source,
            anchor,
            selected_entries: selected_entries.into(),
        })
    }

    pub(crate) const fn source_session_id(&self) -> SessionId {
        self.source_session_id
    }

    pub(crate) const fn source(&self) -> ForkSourceKind {
        self.source
    }

    pub(crate) const fn anchor(&self) -> &ForkAnchor {
        &self.anchor
    }

    pub(crate) fn write_for_child<W: Write>(
        &self,
        header: &SessionHeader,
        writer: &mut W,
    ) -> Result<u64, ForkAnchorResolutionError> {
        validate_captured_fork_path(self.source_session_id, &self.selected_entries)?;
        if u64::try_from(self.selected_entries.len())
            .map_err(|_| ForkAnchorResolutionError::TooLarge)?
            > MAX_CONVERSATION_ENTRY_RECORDS
        {
            return Err(ForkAnchorResolutionError::TooLarge);
        }

        let header_line = ConversationLineCodec::encode_header(header)
            .map_err(|_| ForkAnchorResolutionError::Encode)?;
        let mut written = encoded_line_bytes(0, header_line.len())?;
        writer
            .write_all(&header_line)
            .and_then(|()| writer.write_all(b"\n"))
            .map_err(|_| ForkAnchorResolutionError::Unavailable)?;

        for source_entry in self.selected_entries.iter() {
            let child_entry = fork_child_entry(source_entry, header.session_id());
            let line = ConversationLineCodec::encode_entry(&child_entry)
                .map_err(|_| ForkAnchorResolutionError::Encode)?;
            written = encoded_line_bytes(written, line.len())?;
            writer
                .write_all(&line)
                .and_then(|()| writer.write_all(b"\n"))
                .map_err(|_| ForkAnchorResolutionError::Unavailable)?;
        }
        Ok(written)
    }

    pub(crate) fn validate_reencoded_child<R: Read>(
        &self,
        reader: R,
        declared_file_bytes: u64,
        expected_header: &SessionHeader,
    ) -> Result<(), ForkAnchorResolutionError> {
        validate_captured_fork_path(self.source_session_id, &self.selected_entries)?;
        let mut scanner = ConversationJsonlScanner::open(
            reader,
            declared_file_bytes,
            expected_header.session_id(),
            ConversationScanAccess::ReadOnly,
        )
        .map_err(map_fork_child_scan_error)?;
        if scanner.header().map_err(map_fork_child_scan_error)? != expected_header
            || !scanner.header_is_canonical()
        {
            return Err(ForkAnchorResolutionError::InvalidSource);
        }

        let mut entry_index = 0usize;
        loop {
            match scanner.next_event().map_err(map_fork_child_scan_error)? {
                None if entry_index == self.selected_entries.len() => return Ok(()),
                None => return Err(ForkAnchorResolutionError::InvalidSource),
                Some(ConversationScanEvent::Fault {
                    fault: ConversationLineFault::OversizedLine,
                    ..
                }) => return Err(ForkAnchorResolutionError::TooLarge),
                Some(
                    ConversationScanEvent::Fault { .. } | ConversationScanEvent::PartialTail { .. },
                ) => return Err(ForkAnchorResolutionError::InvalidSource),
                Some(ConversationScanEvent::Entry {
                    canonicality,
                    entry,
                    ..
                }) => {
                    let Some(source_entry) = self.selected_entries.get(entry_index) else {
                        return Err(ForkAnchorResolutionError::InvalidSource);
                    };
                    let expected_entry =
                        fork_child_entry(source_entry, expected_header.session_id());
                    if canonicality != ConversationLineCanonicality::Canonical
                        || entry.as_ref() != &expected_entry
                    {
                        return Err(ForkAnchorResolutionError::InvalidSource);
                    }
                    entry_index = entry_index
                        .checked_add(1)
                        .ok_or(ForkAnchorResolutionError::TooLarge)?;
                }
            }
        }
    }
}

fn encoded_line_bytes(
    already_written: u64,
    line_bytes: usize,
) -> Result<u64, ForkAnchorResolutionError> {
    let line_bytes = u64::try_from(line_bytes).map_err(|_| ForkAnchorResolutionError::TooLarge)?;
    let written = already_written
        .checked_add(line_bytes)
        .and_then(|value| value.checked_add(1))
        .ok_or(ForkAnchorResolutionError::TooLarge)?;
    if written > MAX_CONVERSATION_FILE_BYTES {
        Err(ForkAnchorResolutionError::TooLarge)
    } else {
        Ok(written)
    }
}

fn fork_child_entry(
    source_entry: &StoredSessionEntry,
    child_session_id: SessionId,
) -> StoredSessionEntry {
    StoredSessionEntry::reconstruct(
        source_entry.entry_id(),
        source_entry.parent_id(),
        child_session_id,
        source_entry.turn_id(),
        source_entry.timestamp(),
        source_entry.body().clone(),
    )
}

fn map_fork_child_scan_error(error: ConversationScanError) -> ForkAnchorResolutionError {
    match error {
        ConversationScanError::FileTooLarge
        | ConversationScanError::HistoryTooLarge
        | ConversationScanError::HeaderCorrupt {
            code: ConversationCodecError::HeaderTooLarge,
        } => ForkAnchorResolutionError::TooLarge,
        ConversationScanError::LeaseMismatch
        | ConversationScanError::InputChanged
        | ConversationScanError::InputUnavailable
        | ConversationScanError::CounterOverflow => ForkAnchorResolutionError::Unavailable,
        ConversationScanError::HeaderCorrupt { .. }
        | ConversationScanError::UnsupportedFormatVersion
        | ConversationScanError::MissingHeader
        | ConversationScanError::InvariantViolation => ForkAnchorResolutionError::InvalidSource,
    }
}

impl fmt::Debug for CapturedForkConversation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedForkConversation")
            .field("source_session", &"redacted")
            .field("source", &self.source)
            .field("anchor", &self.anchor)
            .field("selected_entries", &self.selected_entries.len())
            .finish()
    }
}

fn resolve_fork_anchor(
    selected_entries: &[Arc<StoredSessionEntry>],
    relations: &[ItemRelation],
    anchor: &ForkAnchor,
) -> Option<usize> {
    let relation_matches = |item_id: ItemId, family| {
        relations
            .iter()
            .any(|relation| relation.item_id() == item_id && relation.family() == family)
    };
    match anchor {
        ForkAnchor::Genesis => Some(0),
        ForkAnchor::BeforeUserMessage { item_id } | ForkAnchor::AfterUserMessage { item_id } => {
            selected_entries
                .iter()
                .position(|entry| {
                    matches!(
                        entry.body(),
                        StoredEntryBody::UserMessage(message)
                            if message.item_id() == *item_id
                                && relation_matches(
                                    *item_id,
                                    crate::turn_item_interaction::ItemContentFamily::UserMessage,
                                )
                    )
                })
                .map(|index| {
                    index + usize::from(matches!(anchor, ForkAnchor::AfterUserMessage { .. }))
                })
        }
        ForkAnchor::BeforeFinalAgentMessage { item_id }
        | ForkAnchor::AfterFinalAgentMessage { item_id } => selected_entries
            .iter()
            .position(|entry| {
                matches!(
                    entry.body(),
                    StoredEntryBody::AssistantMessage(message)
                        if message.disposition() == AssistantDisposition::Final
                            && message.content().iter().any(|content| matches!(
                                content,
                                StoredAssistantContent::Text { item_id: actual, .. }
                                    if actual == item_id
                            ))
                            && relation_matches(
                                *item_id,
                                crate::turn_item_interaction::ItemContentFamily::AgentMessage,
                            )
                )
            })
            .map(|index| {
                index + usize::from(matches!(anchor, ForkAnchor::AfterFinalAgentMessage { .. }))
            }),
    }
}

fn validate_captured_fork_path(
    source_session_id: SessionId,
    selected_entries: &[Arc<StoredSessionEntry>],
) -> Result<(), ForkAnchorResolutionError> {
    let mut parent = None;
    let mut entry_ids = BTreeSet::new();
    for entry in selected_entries {
        if entry.session_id() != source_session_id
            || entry.parent_id() != parent
            || !entry_ids.insert(entry.entry_id())
        {
            return Err(ForkAnchorResolutionError::InvalidSource);
        }
        parent = Some(entry.entry_id());
    }
    Ok(())
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

/// Closed crate-private diagnostic code set for M5 tolerant replay.
///
/// The set is exact and closed: every replay outcome maps to one code, and no free-form
/// diagnostic string can be produced. Codes are snake_case in `Display` to match the authoritative
/// fixture metadata vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum ConversationReplayDiagnosticCode {
    PartialTail,
    OversizedLine,
    InvalidUtf8,
    MalformedJson,
    InvalidEntry,
    UnknownRecordVariant,
    UnknownEntryVariant,
    DuplicateEntryId,
    MissingParent,
    SessionMismatch,
    InvalidRelation,
    InvalidContributionStamp,
    DuplicateContributionStamp,
    InvalidToolExchange,
    InvalidInteractionRelation,
    InvalidCompactionMarker,
    DiagnosticsTruncated,
    #[allow(
        dead_code,
        reason = "reserved by the closed V1 diagnostic code set; the scanner surfaces the 1,000,000-entry cap as the typed HistoryTooLarge replay error instead of a detail fact"
    )]
    HistoryTooLarge,
}

impl ConversationReplayDiagnosticCode {
    const fn name(self) -> &'static str {
        match self {
            Self::PartialTail => "partial_tail",
            Self::OversizedLine => "oversized_line",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::MalformedJson => "malformed_json",
            Self::InvalidEntry => "invalid_entry",
            Self::UnknownRecordVariant => "unknown_record_variant",
            Self::UnknownEntryVariant => "unknown_entry_variant",
            Self::DuplicateEntryId => "duplicate_entry_id",
            Self::MissingParent => "missing_parent",
            Self::SessionMismatch => "session_mismatch",
            Self::InvalidRelation => "invalid_relation",
            Self::InvalidContributionStamp => "invalid_contribution_stamp",
            Self::DuplicateContributionStamp => "duplicate_contribution_stamp",
            Self::InvalidToolExchange => "invalid_tool_exchange",
            Self::InvalidInteractionRelation => "invalid_interaction_relation",
            Self::InvalidCompactionMarker => "invalid_compaction_marker",
            Self::DiagnosticsTruncated => "diagnostics_truncated",
            Self::HistoryTooLarge => "history_too_large",
        }
    }

    /// Resolves the exact closed snake_case name back to its code. Crate-private so tests can
    /// interpret the authoritative fixture vocabulary.
    #[cfg(test)]
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "partial_tail" => Self::PartialTail,
            "oversized_line" => Self::OversizedLine,
            "invalid_utf8" => Self::InvalidUtf8,
            "malformed_json" => Self::MalformedJson,
            "invalid_entry" => Self::InvalidEntry,
            "unknown_record_variant" => Self::UnknownRecordVariant,
            "unknown_entry_variant" => Self::UnknownEntryVariant,
            "duplicate_entry_id" => Self::DuplicateEntryId,
            "missing_parent" => Self::MissingParent,
            "session_mismatch" => Self::SessionMismatch,
            "invalid_relation" => Self::InvalidRelation,
            "invalid_contribution_stamp" => Self::InvalidContributionStamp,
            "duplicate_contribution_stamp" => Self::DuplicateContributionStamp,
            "invalid_tool_exchange" => Self::InvalidToolExchange,
            "invalid_interaction_relation" => Self::InvalidInteractionRelation,
            "invalid_compaction_marker" => Self::InvalidCompactionMarker,
            "diagnostics_truncated" => Self::DiagnosticsTruncated,
            "history_too_large" => Self::HistoryTooLarge,
            _ => return None,
        })
    }
}

impl fmt::Display for ConversationReplayDiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One bounded diagnostic detail fact: an exact closed code plus the physical line/byte location.
///
/// The detail never carries IDs, user text, tool output, provider data, a raw line, or a path,
/// so both `Debug` and `Display` of the enclosing diagnostics are inherently redacted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConversationReplayDiagnosticDetail {
    code: ConversationReplayDiagnosticCode,
    line_number: u64,
    offset: u64,
}

#[cfg(test)]
impl ConversationReplayDiagnosticDetail {
    pub(crate) const fn code(self) -> ConversationReplayDiagnosticCode {
        self.code
    }

    pub(crate) const fn line_number(self) -> u64 {
        self.line_number
    }
}

/// One truncation summary: how many detail facts were omitted beyond the retained first 100, plus
/// the exact aggregate totals at the end of replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConversationReplayTruncationSummary {
    omitted_details: u64,
    totals: BTreeMap<ConversationReplayDiagnosticCode, u64>,
}

#[cfg(test)]
impl ConversationReplayTruncationSummary {
    pub(crate) const fn omitted_details(&self) -> u64 {
        self.omitted_details
    }

    pub(crate) fn totals(&self) -> &BTreeMap<ConversationReplayDiagnosticCode, u64> {
        &self.totals
    }
}

/// Bounded aggregate diagnostics for one tolerant replay.
///
/// Every fact increments its closed-code aggregate counter; at most the first 100 detail facts
/// are retained in physical order. When more facts arrive, one truncation summary records the
/// omitted count and the final totals, and the aggregate `DiagnosticsTruncated` counter becomes
/// exactly one.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ConversationReplayDiagnostics {
    details: Vec<ConversationReplayDiagnosticDetail>,
    truncation: Option<ConversationReplayTruncationSummary>,
    counts: BTreeMap<ConversationReplayDiagnosticCode, u64>,
    total_facts: u64,
}

impl ConversationReplayDiagnostics {
    const MAX_RETAINED_DETAILS: usize = 100;

    pub(crate) fn new() -> Self {
        Self {
            details: Vec::with_capacity(Self::MAX_RETAINED_DETAILS),
            truncation: None,
            counts: BTreeMap::new(),
            total_facts: 0,
        }
    }

    /// Records one bounded fact and keeps the retained details in physical order with a
    /// deterministic tie-break.
    ///
    /// Scan facts arrive first (pass one, physical order) and semantic projection facts arrive
    /// later (pass two, physical order over accepted entries), so a later projection fact at an
    /// earlier physical offset must not be appended ahead of earlier physical scan facts.
    /// Inserting by physical location and trimming to the first 100 keeps the retained details
    /// equal to the first 100 observed facts in physical/emission order at every point, with
    /// equal locations resolved by emission order (scan facts before projection facts).
    fn record(
        &mut self,
        code: ConversationReplayDiagnosticCode,
        location: ConversationPhysicalLocation,
    ) {
        let count = self.counts.entry(code).or_insert(0);
        *count = count
            .checked_add(1)
            .expect("replay diagnostic aggregate counters cannot overflow");
        self.total_facts = self
            .total_facts
            .checked_add(1)
            .expect("replay diagnostic fact counters cannot overflow");
        let detail = ConversationReplayDiagnosticDetail {
            code,
            line_number: location.line_number(),
            offset: location.offset(),
        };
        let position = self.details.partition_point(|retained| {
            (retained.line_number, retained.offset) <= (detail.line_number, detail.offset)
        });
        if position < Self::MAX_RETAINED_DETAILS {
            if self.details.len() == Self::MAX_RETAINED_DETAILS {
                self.details.pop();
            }
            self.details.insert(position, detail);
        }
    }

    /// Freezes the diagnostics: snapshots the final totals into the truncation summary and adds
    /// the single `DiagnosticsTruncated` aggregate fact when any detail was omitted. The totals
    /// snapshot contains every observed source fact but not the synthetic `DiagnosticsTruncated`
    /// fact itself.
    fn finish(mut self) -> Self {
        if self.total_facts > Self::MAX_RETAINED_DETAILS as u64 {
            self.truncation = Some(ConversationReplayTruncationSummary {
                omitted_details: self.total_facts - Self::MAX_RETAINED_DETAILS as u64,
                totals: self.counts.clone(),
            });
            let count = self
                .counts
                .entry(ConversationReplayDiagnosticCode::DiagnosticsTruncated)
                .or_insert(0);
            *count = count
                .checked_add(1)
                .expect("replay diagnostic aggregate counters cannot overflow");
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn count(&self, code: ConversationReplayDiagnosticCode) -> u64 {
        self.counts.get(&code).copied().unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> &BTreeMap<ConversationReplayDiagnosticCode, u64> {
        &self.counts
    }

    /// The first 100 retained detail facts in physical order.
    #[cfg(test)]
    pub(crate) fn details(&self) -> &[ConversationReplayDiagnosticDetail] {
        &self.details
    }

    #[cfg(test)]
    pub(crate) fn truncation(&self) -> Option<&ConversationReplayTruncationSummary> {
        self.truncation.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}

impl fmt::Debug for ConversationReplayDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversationReplayDiagnostics")
            .field("counts", &self.counts)
            .field("retained_details", &self.details.len())
            .field("truncation", &self.truncation)
            .finish()
    }
}

impl fmt::Display for ConversationReplayDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (code, count)) in self.counts.iter().enumerate() {
            if index != 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "{code}={count}")?;
        }
        Ok(())
    }
}

/// Redacted typed failure for one tolerant replay attempt.
///
/// Strict Header failures and bounded scan caps stop replay with this typed error. The variants
/// carry no IDs, text, raw line, or path, so the derived `Debug` and the `Display` below are both
/// redacted.
#[allow(
    dead_code,
    reason = "the M5.2 replay seam is consumed by focused replay tests and the pending Load seam"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConversationReplayError {
    HistoryTooLarge,
    HeaderCorrupt,
    UnsupportedFormatVersion,
    MissingHeader,
    LeaseMismatch,
    InputChanged,
    InputUnavailable,
    CounterOverflow,
    InvariantViolation,
}

impl fmt::Display for ConversationReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::HistoryTooLarge => {
                "conversation history exceeds the physical file or entry-count limit"
            }
            Self::HeaderCorrupt => "conversation header is corrupt",
            Self::UnsupportedFormatVersion => {
                "conversation header has an unsupported format version"
            }
            Self::MissingHeader => "conversation file does not contain a complete Header",
            Self::LeaseMismatch => "conversation writable lease does not match the opened file",
            Self::InputChanged => "conversation file changed after its metadata was read",
            Self::InputUnavailable => "conversation replay input is unavailable",
            Self::CounterOverflow => "conversation replay counter overflow",
            Self::InvariantViolation => "conversation replay invariant was violated",
        };
        formatter.write_str(message)
    }
}

fn map_replay_scan_error(error: ConversationScanError) -> ConversationReplayError {
    match error {
        // The wire boundary recipe/fixture maps both the 1 GiB physical file cap and the
        // 1,000,000-complete-entry cap to the same typed HistoryTooLarge outcome.
        ConversationScanError::FileTooLarge | ConversationScanError::HistoryTooLarge => {
            ConversationReplayError::HistoryTooLarge
        }
        ConversationScanError::HeaderCorrupt { .. } => ConversationReplayError::HeaderCorrupt,
        ConversationScanError::UnsupportedFormatVersion => {
            ConversationReplayError::UnsupportedFormatVersion
        }
        ConversationScanError::MissingHeader => ConversationReplayError::MissingHeader,
        ConversationScanError::LeaseMismatch => ConversationReplayError::LeaseMismatch,
        ConversationScanError::InputChanged => ConversationReplayError::InputChanged,
        ConversationScanError::InputUnavailable => ConversationReplayError::InputUnavailable,
        ConversationScanError::CounterOverflow => ConversationReplayError::CounterOverflow,
        ConversationScanError::InvariantViolation => ConversationReplayError::InvariantViolation,
    }
}

/// One accepted entry plus the physical facts the replay projection needs.
struct ScanEntry {
    entry: Arc<StoredSessionEntry>,
    location: ConversationPhysicalLocation,
    /// Physical index among accepted entries; the semantic pass uses it to re-read an earlier
    /// entry exactly (for Interaction request bodies) without cloning payloads.
    index: usize,
    /// The disconnected history-tree component this entry belongs to.
    component: u64,
}

/// Physical scan state: first-valid identity reservation, component isolation, and the bounded
/// structural diagnostics. The scanner remains the physical owner; this state only consumes its
/// typed events.
struct ReplayScanState {
    reserved_ids: Vec<EntryId>,
    reserved_set: BTreeSet<EntryId>,
    entries: Vec<ScanEntry>,
    /// EntryId to physical index into `entries`: every accepted entry gets exactly one index, so
    /// ancestor/path lookups are O(log n) instead of rescanning the Vec for every ID. Bounded at
    /// the 1,000,000-entry cap alongside the other per-entry state.
    entry_index: BTreeMap<EntryId, usize>,
    components: BTreeMap<EntryId, u64>,
    next_component: u64,
    canonical_component: Option<u64>,
    diagnostics: ConversationReplayDiagnostics,
    tail_action: Option<ConversationPartialTailAction>,
    last_location: Option<ConversationPhysicalLocation>,
}

impl ReplayScanState {
    fn new() -> Self {
        Self {
            reserved_ids: Vec::new(),
            reserved_set: BTreeSet::new(),
            entries: Vec::new(),
            entry_index: BTreeMap::new(),
            components: BTreeMap::new(),
            next_component: 0,
            canonical_component: None,
            diagnostics: ConversationReplayDiagnostics::new(),
            tail_action: None,
            last_location: None,
        }
    }

    fn record(
        &mut self,
        code: ConversationReplayDiagnosticCode,
        location: ConversationPhysicalLocation,
    ) {
        self.diagnostics.record(code, location);
    }

    fn observe(&mut self, event: ConversationScanEvent) {
        self.last_location = match &event {
            ConversationScanEvent::Entry { location, .. }
            | ConversationScanEvent::Fault { location, .. }
            | ConversationScanEvent::PartialTail { location, .. } => Some(*location),
        };
        match event {
            ConversationScanEvent::Entry {
                location,
                entry,
                decode_facts,
                ..
            } => self.observe_entry(*entry, location, decode_facts),
            ConversationScanEvent::Fault { location, fault } => {
                self.record(replay_fault_code(fault), location);
            }
            ConversationScanEvent::PartialTail { location, action } => {
                self.tail_action = Some(action);
                self.record(ConversationReplayDiagnosticCode::PartialTail, location);
            }
        }
    }

    fn observe_entry(
        &mut self,
        entry: StoredSessionEntry,
        location: ConversationPhysicalLocation,
        decode_facts: ConversationDecodeFacts,
    ) {
        // Session mismatch was already rejected by the scanner before this event, so reservation
        // happens only for first-valid, session-matching entries.
        let entry_id = entry.entry_id();
        if !self.reserved_set.insert(entry_id) {
            self.record(ConversationReplayDiagnosticCode::DuplicateEntryId, location);
            return;
        }
        self.reserved_ids.push(entry_id);

        // The codec counted independently degradable contribution stamps on this exact line.
        for _ in 0..decode_facts.invalid_contribution_stamps {
            self.record(
                ConversationReplayDiagnosticCode::InvalidContributionStamp,
                location,
            );
        }
        for _ in 0..decode_facts.duplicate_contribution_stamps {
            self.record(
                ConversationReplayDiagnosticCode::DuplicateContributionStamp,
                location,
            );
        }

        let component = match entry.parent_id() {
            None => {
                let component = self.next_component;
                self.next_component = self
                    .next_component
                    .checked_add(1)
                    .expect("component counters cannot overflow");
                if self.canonical_component.is_none() {
                    self.canonical_component = Some(component);
                } else {
                    // A second root is isolated into its own component and diagnosed; the first
                    // accepted root remains canonical.
                    self.record(ConversationReplayDiagnosticCode::InvalidRelation, location);
                }
                component
            }
            Some(parent_id) if !self.reserved_set.contains(&parent_id) => {
                self.record(ConversationReplayDiagnosticCode::MissingParent, location);
                let component = self.next_component;
                self.next_component = self
                    .next_component
                    .checked_add(1)
                    .expect("component counters cannot overflow");
                component
            }
            Some(parent_id) => *self
                .components
                .get(&parent_id)
                .expect("a reserved parent always has a recorded component"),
        };
        self.components.insert(entry_id, component);
        self.entry_index.insert(entry_id, self.entries.len());
        self.entries.push(ScanEntry {
            entry: Arc::new(entry),
            location,
            index: self.entries.len(),
            component,
        });
    }

    /// The selected path: the ancestor chain from the first accepted root to the physical-last
    /// accepted entry of its canonical component. Isolated roots and orphans never appear.
    ///
    /// Each ancestor lookup resolves through the EntryId-to-index map, so a pathological 1 GiB
    /// chain costs O(n log n) total instead of rescanning the accepted Vec for every ancestor.
    fn selected_path(&self) -> (Vec<EntryId>, Option<EntryId>) {
        let Some(canonical_component) = self.canonical_component else {
            return (Vec::new(), None);
        };
        let leaf = self
            .entries
            .iter()
            .rev()
            .find(|scan_entry| scan_entry.component == canonical_component)
            .expect("the canonical component has its first root as a member");
        let mut path = Vec::new();
        let mut current = Some(leaf.entry.entry_id());
        while let Some(entry_id) = current {
            path.push(entry_id);
            let index = *self
                .entry_index
                .get(&entry_id)
                .expect("every accepted entry has one physical index");
            current = self.entries[index].entry.parent_id();
        }
        path.reverse();
        (path, Some(leaf.entry.entry_id()))
    }
}

fn replay_fault_code(fault: ConversationLineFault) -> ConversationReplayDiagnosticCode {
    match fault {
        ConversationLineFault::OversizedLine => ConversationReplayDiagnosticCode::OversizedLine,
        ConversationLineFault::InvalidUtf8 => ConversationReplayDiagnosticCode::InvalidUtf8,
        ConversationLineFault::MalformedJson => ConversationReplayDiagnosticCode::MalformedJson,
        ConversationLineFault::InvalidEntry => ConversationReplayDiagnosticCode::InvalidEntry,
        ConversationLineFault::UnknownRecordVariant => {
            ConversationReplayDiagnosticCode::UnknownRecordVariant
        }
        ConversationLineFault::UnknownEntryVariant => {
            ConversationReplayDiagnosticCode::UnknownEntryVariant
        }
        ConversationLineFault::SessionMismatch => ConversationReplayDiagnosticCode::SessionMismatch,
    }
}

/// One assistant tool call expected by a pending Tool exchange, with the first valid terminal
/// outcome. Owned by the selected-path sanitizer pass.
struct ExpectedToolCall {
    item_id: ItemId,
    tool_call_id: ToolCallId,
    terminal: Option<StoredToolOutcome>,
}

/// The model-visible pending Tool exchange owned by the selected-path sanitizer pass. Completed
/// exchanges are emitted immediately in assistant call order; an exchange left open when a
/// selected-path user/assistant/compaction or EOF arrives is excluded with one
/// `invalid_tool_exchange` fact at the closing boundary. Off-path entries never create, close,
/// complete, or replace this exchange (ADR 0124/0132 branch isolation).
struct PendingToolExchange {
    assistant_entry_id: EntryId,
    assistant_message: ModelMessage,
    expected: Vec<ExpectedToolCall>,
}

/// One sanitized stable unit over the selected path: the origin EntryId plus the model-visible
/// messages it contributes. An ordinary User and a plain Assistant are units; a complete Tool
/// exchange is one unit with the assistant origin; a rolling summary is the leading unit with
/// the compaction entry origin. Tool results and Interaction/internal entries never get their
/// own unit, so a marker naming one of them can never match a unit origin.
struct ReplayUnit {
    origin: EntryId,
    kind: CompactionUnitKind,
    messages: Vec<ModelMessage>,
}

/// One accepted interaction request fact available to later resolutions. The request body is not
/// cloned: the resolution re-reads the exact earlier entry by physical index, so interaction
/// state stays O(requests) with no payload duplication.
struct AcceptedInteraction {
    item_id: ItemId,
    request_entry_index: usize,
    resolved: bool,
}

/// One accepted assistant ToolCall fact keyed by its exact (ItemId, ToolCallId) pair: the owning
/// assistant entry plus the first valid terminal outcome. Item identities are first-valid
/// file-wide, so each pair resolves to at most one accepted call fact.
struct AcceptedToolCallFact {
    assistant_entry_id: EntryId,
    terminal: Option<StoredToolOutcome>,
}

/// The per-tool classification of an invalid ToolMessage fact, owned by the semantic facts pass.
/// The pass records exactly one owning diagnostic per fact; the selected-path sanitizer pass
/// consumes the class so it can skip invalid selected facts without re-emitting diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InvalidToolFactKind {
    /// No accepted assistant ToolCall matches the result's (ItemId, ToolCallId) pair.
    Orphan,
    /// The result's ToolCallId matches an accepted call under a different ItemId.
    IdentityConflict,
    /// A first valid terminal outcome already won for the matching accepted call.
    DuplicateTerminal,
    /// The first terminal outcome for the matching accepted call is Abandoned.
    AbandonedFirst,
}

/// One invalid ToolMessage fact plus its owning assistant exchange when that owner is uniquely
/// identified. `AbandonedFirst` always carries its exact-pair owner with `invalidates_owner`
/// set; `DuplicateTerminal` carries its exact-pair owner without invalidation; and
/// `IdentityConflict` carries the unique live candidate when exactly one exists, with
/// invalidation only when that candidate's exact call fact has no terminal yet — a candidate
/// that already has a terminal retains its valid exchange and only the malformed fact is
/// diagnosed. `Orphan` and ambiguous (multi-candidate) `IdentityConflict` facts carry `None`
/// and never invalidate. The selected-path sanitizer pass consumes the fact so it can clear
/// its pending exchange only when `invalidates_owner` is set and the owner matches the pending
/// exchange's assistant entry: a fact for an already-closed, other, retained-valid, or
/// off-path exchange never clears the current pending exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidToolFact {
    kind: InvalidToolFactKind,
    owner: Option<EntryId>,
    /// True only when the semantic facts pass invalidated `owner`'s exchange (removed its
    /// calls from the live-call index and marked the exchange invalidated). The sanitizer
    /// pass clears a matching pending exchange only for facts with this set.
    invalidates_owner: bool,
}

/// The all-history semantic facts one tolerant replay owns. The semantic facts pass produces
/// these in physical order over every accepted entry; the selected-path sanitizer pass consumes
/// them so invalid selected entries and tool facts are skipped without duplicate diagnostics.
struct SemanticFacts {
    /// First-valid semantic item ids in physical order over all accepted entries.
    historical_item_ids: Vec<ItemId>,
    /// EntryId → the item ids accepted (first-valid, non-duplicate) inside that entry, in
    /// content order. The sanitizer pass rebuilds only these items; entries with no accepted
    /// items yield no unit and no re-emitted relation diagnostic.
    valid_items_by_entry: BTreeMap<EntryId, Vec<ItemId>>,
    /// Tool entry id → the first valid completed terminal outcome accepted for its exchange.
    valid_tool_terminals: BTreeMap<EntryId, StoredToolOutcome>,
    /// Tool entry id → the invalid fact class plus its owning exchange; the sanitizer pass
    /// skips these facts and clears its pending exchange only for a matching invalidated
    /// owner.
    invalid_tool_facts: BTreeMap<EntryId, InvalidToolFact>,
}

/// The all-history semantic facts pass over every accepted entry in physical order.
///
/// This pass is deliberately separate from the physical scan and from the live reducer: live
/// Turn/ID rules are strict, replay validates relations tolerantly, isolates every invalid fact
/// with exactly one owning diagnostic, and continues. It owns the first-valid item index (and
/// therefore `historical_item_ids`), the accepted ToolCall facts by (ItemId, ToolCallId) plus
/// the bounded live-call index for response-local ToolCallId reuse, the item set for
/// Interaction requests, the all-history Tool outcome validity (first valid terminal outcome
/// wins per accepted call; exactly one `invalid_tool_exchange` diagnostic for every orphan,
/// identity-conflicting, duplicate-terminal, or abandoned-first result fact, each carrying its
/// owning exchange when uniquely identified), and the Interaction request/resolution relation.
/// It never maintains a model-visible pending exchange and never builds stable units; those
/// belong to the selected-path sanitizer pass, which consumes the per-entry and per-tool
/// validity facts exposed in `SemanticFacts`.
struct ReplaySemanticFactsPass<'a> {
    entries: &'a [ScanEntry],
    seen_items: BTreeSet<ItemId>,
    historical_item_ids: Vec<ItemId>,
    /// Accepted ToolCall items; an Interaction request must reference one of these.
    tool_call_items: BTreeSet<ItemId>,
    /// Accepted assistant ToolCall facts by exact (ItemId, ToolCallId) pair.
    call_facts: BTreeMap<(ItemId, ToolCallId), AcceptedToolCallFact>,
    /// Live accepted assistant calls by ToolCallId, as exact (ItemId, EntryId) pairs. Live
    /// means accepted and not owned by an invalidated exchange; terminal calls are retained
    /// because ToolCallId is response-local: a completed call still constrains later conflict
    /// resolution, and retiring it on completion would let a malformed result with the same
    /// ToolCallId invalidate an unrelated live exchange that legally reused the id.
    /// Identity-conflict resolution examines only one ToolCallId's live candidates, so under
    /// response-local ToolCallId reuse it never rescans the history and stays bounded by the
    /// accepted calls.
    live_calls_by_id: BTreeMap<ToolCallId, BTreeSet<(ItemId, EntryId)>>,
    /// Assistant entry → its accepted (ItemId, ToolCallId) calls, for removal from
    /// `live_calls_by_id` when the exchange is invalidated.
    accepted_calls_by_assistant: BTreeMap<EntryId, BTreeSet<(ItemId, ToolCallId)>>,
    /// Assistant entries whose exchange was invalidated by an identity-conflicting or
    /// abandoned-first result fact; a later result for one of their exact pairs is a late
    /// orphan fact and never re-enters ToolCallId conflict matching.
    invalidated_exchanges: BTreeSet<EntryId>,
    valid_items_by_entry: BTreeMap<EntryId, Vec<ItemId>>,
    valid_tool_terminals: BTreeMap<EntryId, StoredToolOutcome>,
    invalid_tool_facts: BTreeMap<EntryId, InvalidToolFact>,
    interactions: BTreeMap<RequestId, AcceptedInteraction>,
    diagnostics: &'a mut ConversationReplayDiagnostics,
}

impl ReplaySemanticFactsPass<'_> {
    fn record(
        &mut self,
        code: ConversationReplayDiagnosticCode,
        location: ConversationPhysicalLocation,
    ) {
        self.diagnostics.record(code, location);
    }

    /// Runs the pass over all accepted entries in physical order and returns the semantic facts
    /// the selected-path sanitizer pass consumes.
    fn run(mut self) -> SemanticFacts {
        for scan_entry in self.entries {
            self.project_entry(scan_entry);
        }
        SemanticFacts {
            historical_item_ids: self.historical_item_ids,
            valid_items_by_entry: self.valid_items_by_entry,
            valid_tool_terminals: self.valid_tool_terminals,
            invalid_tool_facts: self.invalid_tool_facts,
        }
    }

    fn project_entry(&mut self, scan_entry: &ScanEntry) {
        match scan_entry.entry.body() {
            StoredEntryBody::UserMessage(message) => self.project_user(scan_entry, message),
            StoredEntryBody::AssistantMessage(message) => {
                self.project_assistant(scan_entry, message);
            }
            StoredEntryBody::ToolMessage(message) => self.project_tool(scan_entry, message),
            StoredEntryBody::InteractionRequested(request) => {
                self.project_interaction_request(scan_entry, request);
            }
            StoredEntryBody::InteractionResolved(resolution) => {
                self.project_interaction_resolution(scan_entry, resolution);
            }
            StoredEntryBody::Compaction(_) => {}
        }
    }

    fn project_user(&mut self, scan_entry: &ScanEntry, message: &StoredUserMessage) {
        if !self.seen_items.insert(message.item_id()) {
            // A duplicate item identity across User/Assistant content is isolated: the entry
            // yields no semantic items.
            self.record(
                ConversationReplayDiagnosticCode::InvalidRelation,
                scan_entry.location,
            );
            return;
        }
        self.historical_item_ids.push(message.item_id());
        self.valid_items_by_entry
            .insert(scan_entry.entry.entry_id(), vec![message.item_id()]);
    }

    fn project_assistant(&mut self, scan_entry: &ScanEntry, message: &StoredAssistantMessage) {
        let mut valid_items = Vec::with_capacity(message.content().len());
        for item in message.content() {
            if !self.seen_items.insert(item.item_id()) {
                // Duplicate content items are isolated one by one; each gets one owning fact.
                self.record(
                    ConversationReplayDiagnosticCode::InvalidRelation,
                    scan_entry.location,
                );
                continue;
            }
            self.historical_item_ids.push(item.item_id());
            valid_items.push(item.item_id());
            if let StoredAssistantContent::ToolCall {
                item_id,
                tool_call_id,
                ..
            } = item
            {
                // The call is an accepted ToolCall fact for later Interaction requests and for
                // the first-valid-terminal outcome resolution. It enters the live-call index
                // and remains there until its exchange is invalidated; terminal calls are
                // retained because ToolCallId is response-local.
                self.tool_call_items.insert(*item_id);
                self.live_calls_by_id
                    .entry(tool_call_id.clone())
                    .or_default()
                    .insert((*item_id, scan_entry.entry.entry_id()));
                self.accepted_calls_by_assistant
                    .entry(scan_entry.entry.entry_id())
                    .or_default()
                    .insert((*item_id, tool_call_id.clone()));
                self.call_facts.insert(
                    (*item_id, tool_call_id.clone()),
                    AcceptedToolCallFact {
                        assistant_entry_id: scan_entry.entry.entry_id(),
                        terminal: None,
                    },
                );
            }
        }
        if !valid_items.is_empty() {
            self.valid_items_by_entry
                .insert(scan_entry.entry.entry_id(), valid_items);
        }
    }

    /// Invalidates one assistant exchange: marks it, removes every one of its accepted calls
    /// from the live-call index, and leaves its exact-pair facts in place so a later result
    /// for one of those pairs is classified as a late orphan instead of re-entering
    /// ToolCallId conflict matching.
    fn invalidate_exchange(&mut self, assistant_entry_id: EntryId) {
        if !self.invalidated_exchanges.insert(assistant_entry_id) {
            return;
        }
        let Some(calls) = self.accepted_calls_by_assistant.remove(&assistant_entry_id) else {
            return;
        };
        for (item_id, tool_call_id) in calls {
            self.retire_call(item_id, &tool_call_id, assistant_entry_id);
        }
    }

    /// Removes one accepted call from the live-call index when its exchange is invalidated.
    /// Terminal calls are retained: ToolCallId is response-local, so a completed call still
    /// constrains later conflict resolution. The exact-pair fact registry is untouched.
    fn retire_call(
        &mut self,
        item_id: ItemId,
        tool_call_id: &ToolCallId,
        assistant_entry_id: EntryId,
    ) {
        if let Some(live) = self.live_calls_by_id.get_mut(tool_call_id) {
            live.remove(&(item_id, assistant_entry_id));
        }
    }

    fn project_tool(&mut self, scan_entry: &ScanEntry, message: &StoredToolMessage) {
        let location = scan_entry.location;
        let item = message.item_id();
        let call = message.tool_call_id();

        // Exact (ItemId, ToolCallId) resolution first: item identities are first-valid
        // file-wide, so an exact pair belongs to at most one accepted call. A result whose
        // exact pair exists but whose exchange was already invalidated is a late fact for a
        // dead exchange: it is an orphan immediately and never falls through to the
        // ToolCallId conflict index, so it can never invalidate another assistant exchange
        // that legally reused the same ToolCallId in a different response.
        match self.call_facts.get(&(item, call.clone())) {
            Some(fact)
                if !self
                    .invalidated_exchanges
                    .contains(&fact.assistant_entry_id) =>
            {
                let assistant_entry_id = fact.assistant_entry_id;
                let terminal_taken = fact.terminal.is_some();
                if terminal_taken {
                    // First valid terminal outcome already won; the later result is skipped.
                    self.record(
                        ConversationReplayDiagnosticCode::InvalidToolExchange,
                        location,
                    );
                    self.invalid_tool_facts.insert(
                        scan_entry.entry.entry_id(),
                        InvalidToolFact {
                            kind: InvalidToolFactKind::DuplicateTerminal,
                            owner: Some(assistant_entry_id),
                            invalidates_owner: false,
                        },
                    );
                } else {
                    match message.outcome() {
                        StoredToolOutcome::Abandoned { .. } => {
                            // Abandoned-first terminal invalidates the whole exchange.
                            self.record(
                                ConversationReplayDiagnosticCode::InvalidToolExchange,
                                location,
                            );
                            self.invalidate_exchange(assistant_entry_id);
                            self.invalid_tool_facts.insert(
                                scan_entry.entry.entry_id(),
                                InvalidToolFact {
                                    kind: InvalidToolFactKind::AbandonedFirst,
                                    owner: Some(assistant_entry_id),
                                    invalidates_owner: true,
                                },
                            );
                        }
                        StoredToolOutcome::Completed { .. } => {
                            let fact = self
                                .call_facts
                                .get_mut(&(item, call.clone()))
                                .expect("the live exact call fact is still present");
                            fact.terminal = Some(message.outcome().clone());
                            // The call stays in the live-call index: ToolCallId is
                            // response-local, so the completed call still constrains later
                            // no-exact conflict resolution.
                            self.valid_tool_terminals
                                .insert(scan_entry.entry.entry_id(), message.outcome().clone());
                        }
                    }
                }
            }
            Some(_) => {
                // The exact pair exists but its owning exchange is already invalidated: the
                // result is a late fact for a closed exchange and is isolated as an orphan.
                // It must not fall through to ToolCallId conflict matching.
                self.record(
                    ConversationReplayDiagnosticCode::InvalidToolExchange,
                    location,
                );
                self.invalid_tool_facts.insert(
                    scan_entry.entry.entry_id(),
                    InvalidToolFact {
                        kind: InvalidToolFactKind::Orphan,
                        owner: None,
                        invalidates_owner: false,
                    },
                );
            }
            None => {
                // No accepted call matches the exact pair: resolve by ToolCallId against the
                // live calls (every accepted call whose owning exchange is not invalidated,
                // including terminal calls). The scan is bounded to two candidates because
                // only the unique single candidate can be invalidated; an ambiguous
                // multi-candidate conflict never arbitrarily invalidates an exchange. A
                // single terminal candidate is never invalidated either: its exchange stays
                // valid and only the malformed fact is diagnosed.
                let candidates: Vec<(ItemId, EntryId)> = self
                    .live_calls_by_id
                    .get(call)
                    .into_iter()
                    .flat_map(|live| live.iter())
                    .filter(|(candidate_item, _)| *candidate_item != item)
                    .take(2)
                    .copied()
                    .collect();
                self.record(
                    ConversationReplayDiagnosticCode::InvalidToolExchange,
                    location,
                );
                match candidates.as_slice() {
                    [] => {
                        self.invalid_tool_facts.insert(
                            scan_entry.entry.entry_id(),
                            InvalidToolFact {
                                kind: InvalidToolFactKind::Orphan,
                                owner: None,
                                invalidates_owner: false,
                            },
                        );
                    }
                    [(candidate_item, assistant_entry_id)] => {
                        // The unique live candidate owns the fact. Invalidate its exchange
                        // only when that candidate's exact call fact has no terminal yet; a
                        // candidate that already has a terminal retains its valid exchange
                        // and only the malformed fact is diagnosed.
                        let has_terminal = self
                            .call_facts
                            .get(&(*candidate_item, call.clone()))
                            .is_some_and(|fact| fact.terminal.is_some());
                        if !has_terminal {
                            self.invalidate_exchange(*assistant_entry_id);
                        }
                        self.invalid_tool_facts.insert(
                            scan_entry.entry.entry_id(),
                            InvalidToolFact {
                                kind: InvalidToolFactKind::IdentityConflict,
                                owner: Some(*assistant_entry_id),
                                invalidates_owner: !has_terminal,
                            },
                        );
                    }
                    _ => {
                        // Multiple live candidates under one ToolCallId: the owner is
                        // ambiguous, so no exchange is arbitrarily invalidated.
                        self.invalid_tool_facts.insert(
                            scan_entry.entry.entry_id(),
                            InvalidToolFact {
                                kind: InvalidToolFactKind::IdentityConflict,
                                owner: None,
                                invalidates_owner: false,
                            },
                        );
                    }
                }
            }
        }
    }

    fn project_interaction_request(
        &mut self,
        scan_entry: &ScanEntry,
        request: &StoredInteractionRequest,
    ) {
        let location = scan_entry.location;
        if self.interactions.contains_key(&request.request_id()) {
            // Duplicate RequestId: first valid wins.
            self.record(
                ConversationReplayDiagnosticCode::InvalidInteractionRelation,
                location,
            );
            return;
        }
        if !self.tool_call_items.contains(&request.item_id()) {
            // The request must reference an accepted ToolCall.
            self.record(
                ConversationReplayDiagnosticCode::InvalidInteractionRelation,
                location,
            );
            return;
        }
        self.interactions.insert(
            request.request_id(),
            AcceptedInteraction {
                item_id: request.item_id(),
                request_entry_index: scan_entry.index,
                resolved: false,
            },
        );
    }

    fn project_interaction_resolution(
        &mut self,
        scan_entry: &ScanEntry,
        resolution: &StoredInteractionResolution,
    ) {
        let location = scan_entry.location;
        // Resolve the interaction fact first so no mutable borrow of `self.interactions` spans
        // the diagnostics and exact-request-body lookups.
        let valid = match self.interactions.get_mut(&resolution.request_id()) {
            None => {
                // Orphan resolution: no earlier request with this RequestId.
                false
            }
            Some(interaction) => {
                if interaction.item_id != resolution.item_id() {
                    false
                } else if interaction.resolved {
                    // First terminal resolution wins; later resolutions are isolated.
                    false
                } else {
                    let StoredEntryBody::InteractionRequested(stored_request) =
                        self.entries[interaction.request_entry_index].entry.body()
                    else {
                        unreachable!(
                            "an accepted interaction request entry is InteractionRequested"
                        )
                    };
                    let valid = validate_interaction_resolution(
                        stored_request.request(),
                        resolution.resolution(),
                    );
                    if valid {
                        interaction.resolved = true;
                    }
                    valid
                }
            }
        };
        if !valid {
            self.record(
                ConversationReplayDiagnosticCode::InvalidInteractionRelation,
                location,
            );
        }
    }
}

/// The selected-path-only sanitizer pass over accepted entries in physical order.
///
/// Only entries whose EntryId is in the canonical selected path are projected: off-path branch
/// User/Assistant/Tool/Compaction entries never mutate this pass's pending exchange, stable
/// units, or sanitized messages (ADR 0124/0132 branch isolation). It owns the model-visible
/// stable units: a selected-path ordinary User and a plain Assistant are units; a pending Tool
/// exchange opens only for a selected-path Assistant with accepted ToolCall items and is
/// completed only by selected-path Tool children; an exchange left open when a selected-path
/// User/Assistant/Compaction or EOF arrives is excluded with one `invalid_tool_exchange` fact
/// at the closing boundary; a completed exchange becomes one stable unit in assistant call
/// order. Compaction markers are interpreted against the current stable-unit origins per
/// ADR 0132: `Some(id)` is valid only when it exactly matches a current unit origin at index >
/// 0, and `None` only when the current effective units are non-empty. Interaction entries have
/// no model effect here. Entries the semantic facts pass marked invalid are skipped without
/// re-emitting their diagnostics; a skipped invalid entry never closes an unfinished selected
/// exchange. An invalid selected Tool fact clears the pending exchange only when the semantic
/// facts pass invalidated its owner and that owner matches the pending exchange's assistant
/// entry (a fact for an already-closed, other, retained-valid, or off-path exchange never
/// clears it); an orphan, duplicate-terminal, or ambiguous conflict fact never mutates the
/// pending exchange. A globally valid first-terminal selected Tool fact
/// that is not expected by the current pending exchange owns exactly one
/// `invalid_tool_exchange` fact at its own location and leaves the pending exchange untouched.
struct ReplaySanitizerPass<'a> {
    entries: &'a [ScanEntry],
    path_members: &'a BTreeSet<EntryId>,
    facts: &'a SemanticFacts,
    units: Vec<ReplayUnit>,
    relations: Vec<ItemRelation>,
    pending_exchange: Option<PendingToolExchange>,
    revision_operations: u64,
    revision_overflowed: bool,
    diagnostics: &'a mut ConversationReplayDiagnostics,
}

struct ReplaySanitizedProjection {
    units: Vec<ReplayUnit>,
    messages: Vec<ModelMessage>,
    relations: Vec<ItemRelation>,
    revision_operations: u64,
}

impl ReplaySanitizerPass<'_> {
    fn record(
        &mut self,
        code: ConversationReplayDiagnosticCode,
        location: ConversationPhysicalLocation,
    ) {
        self.diagnostics.record(code, location);
    }

    /// Runs the pass over the canonical selected-path entries in physical order and returns the
    /// flattened sanitized messages of the final stable units.
    fn run(
        &mut self,
        eof_location: ConversationPhysicalLocation,
    ) -> Result<ReplaySanitizedProjection, ConversationReplayError> {
        for scan_entry in self.entries {
            if self.path_members.contains(&scan_entry.entry.entry_id()) {
                self.project_entry(scan_entry);
            }
        }
        let projection = self.finish(eof_location);
        if self.revision_overflowed {
            return Err(ConversationReplayError::InvariantViolation);
        }
        Ok(projection)
    }

    fn advance_revision(&mut self) {
        if let Some(next) = self.revision_operations.checked_add(1) {
            self.revision_operations = next;
        } else {
            self.revision_overflowed = true;
        }
    }

    fn project_entry(&mut self, scan_entry: &ScanEntry) {
        match scan_entry.entry.body() {
            StoredEntryBody::UserMessage(message) => self.project_user(scan_entry, message),
            StoredEntryBody::AssistantMessage(message) => {
                self.project_assistant(scan_entry, message);
            }
            StoredEntryBody::ToolMessage(message) => self.project_tool(scan_entry, message),
            StoredEntryBody::InteractionRequested(_) | StoredEntryBody::InteractionResolved(_) => {
                // Interactions have no model effect in the sanitized projection.
            }
            StoredEntryBody::Compaction(stored) => self.project_compaction(scan_entry, stored),
        }
    }

    fn project_user(&mut self, scan_entry: &ScanEntry, message: &StoredUserMessage) {
        if !self
            .facts
            .valid_items_by_entry
            .contains_key(&scan_entry.entry.entry_id())
        {
            // Duplicate item identity: the semantic facts pass owns the diagnostic; the entry
            // yields no unit and must not close an unfinished exchange or emit an extra
            // incomplete diagnostic.
            return;
        }
        // Any selected-path user/assistant/compaction arrival closes an unfinished exchange; a
        // complete exchange emitted here keeps its messages ahead of this entry's. Only a
        // semantically valid projected User reaches this boundary.
        self.close_exchange(scan_entry.location);
        self.relations.push(ItemRelation::user_message(
            message.item_id(),
            scan_entry.entry.turn_id(),
        ));
        self.units.push(ReplayUnit {
            origin: scan_entry.entry.entry_id(),
            kind: CompactionUnitKind::UserMessage,
            messages: vec![ModelMessage::canonical_user(message.content().clone())],
        });
        self.advance_revision();
    }

    fn project_assistant(&mut self, scan_entry: &ScanEntry, message: &StoredAssistantMessage) {
        let Some(valid_items) = self
            .facts
            .valid_items_by_entry
            .get(&scan_entry.entry.entry_id())
        else {
            // Every content item was invalid: the message yields no unit at all; the semantic
            // facts pass owns the relation diagnostics. The invalid entry must not close an
            // unfinished exchange or emit an extra incomplete diagnostic.
            return;
        };
        let mut content = Vec::with_capacity(message.content().len());
        let mut expected = Vec::new();
        for item in message.content() {
            if !valid_items.contains(&item.item_id()) {
                continue;
            }
            match item {
                StoredAssistantContent::Reasoning { content: value, .. } => {
                    content.push(ModelAssistantContent::reasoning(value.clone()));
                }
                StoredAssistantContent::Text { text, .. } => {
                    let Ok(text) = ModelAssistantContent::text(text.clone()) else {
                        // The codec already validated the stored text; this cannot fail.
                        continue;
                    };
                    content.push(text);
                }
                StoredAssistantContent::ToolCall {
                    item_id,
                    tool_call_id,
                    name,
                    arguments,
                } => {
                    content.push(ModelAssistantContent::tool_call(
                        tool_call_id.clone(),
                        name.clone(),
                        arguments.clone(),
                    ));
                    expected.push(ExpectedToolCall {
                        item_id: *item_id,
                        tool_call_id: tool_call_id.clone(),
                        terminal: None,
                    });
                }
            }
        }
        if content.is_empty() {
            // An empty projected message yields no unit and does not close an unfinished
            // exchange.
            return;
        }
        let Ok(projected) = ModelMessage::assistant(content.into()) else {
            return;
        };
        // Only a semantically valid projected Assistant closes an unfinished selected-path
        // exchange.
        self.close_exchange(scan_entry.location);
        for item in message.content() {
            if !valid_items.contains(&item.item_id()) {
                continue;
            }
            let relation = match item {
                StoredAssistantContent::Reasoning { item_id, .. } => {
                    ItemRelation::reasoning(*item_id, scan_entry.entry.turn_id())
                }
                StoredAssistantContent::Text { item_id, .. } => {
                    ItemRelation::agent_message(*item_id, scan_entry.entry.turn_id())
                }
                StoredAssistantContent::ToolCall {
                    item_id,
                    tool_call_id,
                    ..
                } => ItemRelation::tool_invocation(
                    *item_id,
                    scan_entry.entry.turn_id(),
                    tool_call_id.clone(),
                ),
            };
            self.relations.push(relation);
        }
        self.advance_revision();
        if expected.is_empty() {
            // A plain Assistant is its own unit.
            self.units.push(ReplayUnit {
                origin: scan_entry.entry.entry_id(),
                kind: CompactionUnitKind::AssistantMessage,
                messages: vec![projected],
            });
        } else {
            // The message is held until its exchange completes or a selected-path
            // user/assistant/compaction arrival or EOF closes it.
            self.pending_exchange = Some(PendingToolExchange {
                assistant_entry_id: scan_entry.entry.entry_id(),
                assistant_message: projected,
                expected,
            });
        }
    }

    fn project_tool(&mut self, scan_entry: &ScanEntry, message: &StoredToolMessage) {
        if let Some(outcome) = self
            .facts
            .valid_tool_terminals
            .get(&scan_entry.entry.entry_id())
        {
            // First valid terminal outcome: complete the pending selected exchange when it
            // expects this (ItemId, ToolCallId) pair.
            let Some(exchange) = self.pending_exchange.as_mut() else {
                // Valid fact whose exchange already closed on the selected path (closure after
                // a late result): it is not model-visible and owns exactly one
                // `invalid_tool_exchange` fact at this tool location.
                self.record(
                    ConversationReplayDiagnosticCode::InvalidToolExchange,
                    scan_entry.location,
                );
                return;
            };
            let Some(index) = exchange.expected.iter().position(|expected| {
                expected.item_id == message.item_id()
                    && expected.tool_call_id == *message.tool_call_id()
            }) else {
                // Valid fact belonging to another exchange (already closed or off-path in the
                // selected projection): it is not model-visible and owns exactly one
                // `invalid_tool_exchange` fact at this tool location. The current pending
                // exchange stays untouched.
                self.record(
                    ConversationReplayDiagnosticCode::InvalidToolExchange,
                    scan_entry.location,
                );
                return;
            };
            exchange.expected[index].terminal = Some(outcome.clone());
            if exchange
                .expected
                .iter()
                .all(|expected| expected.terminal.is_some())
            {
                let exchange = self
                    .pending_exchange
                    .take()
                    .expect("the completed exchange is still pending");
                self.emit_exchange(exchange);
            }
            return;
        }
        let Some(fact) = self
            .facts
            .invalid_tool_facts
            .get(&scan_entry.entry.entry_id())
        else {
            unreachable!("every accepted tool entry is classified by the facts pass")
        };
        match fact.kind {
            InvalidToolFactKind::IdentityConflict | InvalidToolFactKind::AbandonedFirst => {
                // The semantic facts pass owns the fact's diagnostic. The fact clears the
                // pending exchange only when the owner was actually invalidated by the
                // semantic facts pass and matches the pending exchange's assistant entry; a
                // fact for an already-closed, other, retained-valid (terminal candidate), or
                // off-path exchange never clears the current pending exchange.
                if fact.invalidates_owner {
                    if let Some(owner) = fact.owner {
                        if self
                            .pending_exchange
                            .as_ref()
                            .is_some_and(|exchange| exchange.assistant_entry_id == owner)
                        {
                            self.pending_exchange = None;
                        }
                    }
                }
            }
            InvalidToolFactKind::Orphan | InvalidToolFactKind::DuplicateTerminal => {
                // These facts never mutate the pending exchange.
            }
        }
    }

    /// Closes an unfinished exchange at a selected-path user/assistant/compaction arrival or
    /// EOF. An incomplete exchange is excluded with one `invalid_tool_exchange` fact at the
    /// closing location; late selected-path results for the closed exchange are isolated: an
    /// invalid fact owns its fact diagnostic, and a globally valid fact owns one sanitizer
    /// diagnostic at its own tool location, leaving any current pending exchange untouched.
    fn close_exchange(&mut self, closing_location: ConversationPhysicalLocation) {
        let Some(exchange) = self.pending_exchange.take() else {
            return;
        };
        if exchange
            .expected
            .iter()
            .all(|expected| expected.terminal.is_some())
        {
            self.emit_exchange(exchange);
            return;
        }
        self.record(
            ConversationReplayDiagnosticCode::InvalidToolExchange,
            closing_location,
        );
        // The exchange is dropped; late selected-path results for it are isolated facts with
        // their own diagnostics and never complete or clear a later pending exchange.
    }

    /// Emits a completed exchange in assistant call order as one stable unit. The exchange's
    /// assistant entry is always on the selected path: the sanitizer pass opens pending
    /// exchanges only for selected-path assistants.
    fn emit_exchange(&mut self, exchange: PendingToolExchange) {
        let mut messages = vec![exchange.assistant_message];
        for expected in &exchange.expected {
            let StoredToolOutcome::Completed { content, .. } = expected
                .terminal
                .as_ref()
                .expect("a completed exchange has every terminal outcome")
            else {
                unreachable!("abandoned-first never leaves a completed exchange")
            };
            messages.push(ModelMessage::tool_result(
                expected.tool_call_id.clone(),
                content.clone(),
            ));
        }
        self.units.push(ReplayUnit {
            origin: exchange.assistant_entry_id,
            kind: CompactionUnitKind::ToolExchange,
            messages,
        });
        self.advance_revision();
    }

    /// Applies one recorded Compaction to the current stable units per ADR 0132. A valid marker
    /// exactly matches a current unit origin at index > 0; the units are then replaced by
    /// `[new summary] + units[index..]`. `None` is valid only when the current effective units
    /// are non-empty and replaces them with the summary alone. Every other marker (first unit,
    /// ToolResult/internal entry, missing/orphan/ignored entry, already-removed unit, or a
    /// future entry) is invalid and has no effect, with one owning diagnostic.
    fn project_compaction(&mut self, scan_entry: &ScanEntry, stored: &StoredCompaction) {
        let Ok(summary) = ModelMessage::rolling_summary(stored.summary().into()) else {
            // The codec already validated the summary text; defensively isolate the marker.
            // The compaction is still a selected-path boundary: it closes an unfinished
            // exchange before recording the invalid marker.
            self.close_exchange(scan_entry.location);
            self.record(
                ConversationReplayDiagnosticCode::InvalidCompactionMarker,
                scan_entry.location,
            );
            return;
        };
        // The summary is constructed and validated first; the compaction then closes any
        // unfinished exchange before either a valid or an invalid marker is applied.
        self.close_exchange(scan_entry.location);
        let summary_unit = ReplayUnit {
            origin: scan_entry.entry.entry_id(),
            kind: CompactionUnitKind::RollingSummary,
            messages: vec![summary],
        };
        match stored.first_kept_entry_id() {
            None => {
                if self.units.is_empty() {
                    // `None` covers all units only when the effective conversation is non-empty.
                    self.record(
                        ConversationReplayDiagnosticCode::InvalidCompactionMarker,
                        scan_entry.location,
                    );
                    return;
                }
                self.units = vec![summary_unit];
                self.advance_revision();
            }
            Some(marker) => {
                let Some(index) = self.units.iter().position(|unit| unit.origin == marker) else {
                    self.record(
                        ConversationReplayDiagnosticCode::InvalidCompactionMarker,
                        scan_entry.location,
                    );
                    return;
                };
                if index == 0 {
                    // The marker must not point at the leading unit.
                    self.record(
                        ConversationReplayDiagnosticCode::InvalidCompactionMarker,
                        scan_entry.location,
                    );
                    return;
                }
                let retained = self.units.split_off(index);
                self.units = vec![summary_unit];
                self.units.extend(retained);
                self.advance_revision();
            }
        }
    }

    /// EOF: closes any unfinished selected-path exchange and returns the final stable units.
    fn finish(&mut self, eof_location: ConversationPhysicalLocation) -> ReplaySanitizedProjection {
        self.close_exchange(eof_location);
        let units = std::mem::take(&mut self.units);
        let messages = units
            .iter()
            .flat_map(|unit| unit.messages.iter().cloned())
            .collect();
        ReplaySanitizedProjection {
            units,
            messages,
            relations: std::mem::take(&mut self.relations),
            revision_operations: self.revision_operations,
        }
    }
}
/// Validates one recorded Interaction resolution against its exact earlier request body using
/// the Tools/UserQuestion owner validators. `Cancelled` is a valid owner closure for every
/// request family; every other family must match and pass the owner value check.
fn validate_interaction_resolution(
    request: &StoredInteractionRequestBody,
    resolution: &StoredInteractionResolutionBody,
) -> bool {
    match (request, resolution) {
        (
            StoredInteractionRequestBody::ToolApproval(view),
            StoredInteractionResolutionBody::ToolApproval(resolution),
        ) => view.validate_recorded_resolution(*resolution).is_ok(),
        (
            StoredInteractionRequestBody::UserQuestion(request),
            StoredInteractionResolutionBody::UserAnswer(answer),
        ) => request.validate_answer(answer.clone()).is_ok(),
        (_, StoredInteractionResolutionBody::Cancelled(_)) => true,
        _ => false,
    }
}

/// The cold, tolerant replay result owned by Conversation Storage.
///
/// `current_turn` is deliberately not reconstructed: replay rebuilds conversation facts only, so
/// the view exposes the explicit cold-state fact `None`.
#[allow(
    dead_code,
    reason = "the M5.2 replay seam is consumed by focused replay tests and the pending Load seam"
)]
#[derive(Clone)]
pub(crate) struct ReplayedConversationView {
    header: SessionHeader,
    header_is_canonical: bool,
    accepted_entries: Arc<[Arc<StoredSessionEntry>]>,
    selected_entries: Arc<[Arc<StoredSessionEntry>]>,
    selected_path: Arc<[EntryId]>,
    selected_head: Option<EntryId>,
    reserved_ids: Arc<[EntryId]>,
    sanitized_messages: Arc<[ModelMessage]>,
    stable_units: Arc<[LiveCompactionUnit]>,
    relations: Arc<[ItemRelation]>,
    revision: ConversationRevision,
    historical_item_ids: Arc<[ItemId]>,
    diagnostics: ConversationReplayDiagnostics,
    tail_action: Option<ConversationPartialTailAction>,
}

#[allow(
    dead_code,
    reason = "the M5.2 replay seam is consumed by focused replay tests and the pending Load seam"
)]
impl ReplayedConversationView {
    pub(crate) fn header(&self) -> &SessionHeader {
        &self.header
    }

    pub(crate) const fn header_is_canonical(&self) -> bool {
        self.header_is_canonical
    }

    pub(crate) fn accepted_entry_ids(&self) -> Vec<EntryId> {
        self.accepted_entries
            .iter()
            .map(|entry| entry.entry_id())
            .collect()
    }

    pub(crate) fn selected_entries(&self) -> &[Arc<StoredSessionEntry>] {
        &self.selected_entries
    }

    /// The selected root-to-leaf path of the canonical component, in path order.
    pub(crate) fn selected_path(&self) -> &[EntryId] {
        &self.selected_path
    }

    /// The physical-last leaf of the canonical component.
    pub(crate) fn selected_head(&self) -> Option<EntryId> {
        self.selected_head
    }

    /// Every first-valid accepted EntryId in physical order; this seeds the future collision
    /// guard so later appends can never reuse a replayed identity.
    pub(crate) fn reserved_ids(&self) -> &[EntryId] {
        &self.reserved_ids
    }

    pub(crate) fn sanitized_messages(&self) -> &[ModelMessage] {
        &self.sanitized_messages
    }

    pub(crate) fn stable_units(&self) -> &[LiveCompactionUnit] {
        &self.stable_units
    }

    pub(crate) fn relations(&self) -> &[ItemRelation] {
        &self.relations
    }

    pub(crate) const fn revision(&self) -> ConversationRevision {
        self.revision
    }

    /// Historical item ids in physical order over all accepted entries.
    pub(crate) fn historical_item_ids(&self) -> &[ItemId] {
        &self.historical_item_ids
    }

    pub(crate) fn diagnostics(&self) -> &ConversationReplayDiagnostics {
        &self.diagnostics
    }

    pub(crate) fn tail_action(&self) -> Option<ConversationPartialTailAction> {
        self.tail_action
    }

    /// Explicit cold-state fact: replay never reconstructs the live current Turn.
    pub(crate) const fn current_turn(&self) -> Option<TurnId> {
        None
    }
}

impl fmt::Debug for ReplayedConversationView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayedConversationView")
            .field("header_is_canonical", &self.header_is_canonical)
            .field("accepted_entries", &self.accepted_entries.len())
            .field("selected_entries", &self.selected_entries.len())
            .field("selected_path_len", &self.selected_path.len())
            .field("has_selected_head", &self.selected_head.is_some())
            .field("reserved_ids", &self.reserved_ids.len())
            .field("sanitized_messages", &self.sanitized_messages.len())
            .field("stable_units", &self.stable_units.len())
            .field("relations", &self.relations.len())
            .field("revision", &"redacted")
            .field("historical_item_ids", &self.historical_item_ids.len())
            .field("diagnostics", &self.diagnostics)
            .field("tail_action", &self.tail_action)
            .finish()
    }
}

/// M5 tolerant semantic replay, owned by Conversation Storage.
///
/// The scanner remains the physical bounded owner (strict Header, file/line/entry caps, complete
/// malformed-line skip, one final partial-tail action). This seam consumes only typed scanner
/// events and rebuilds the cold conversation facts in two owner-specific passes: the all-history
/// semantic facts pass (first-valid identity reservation stays with the scan; historical items,
/// accepted ToolCall/Interaction facts, and per-fact owning diagnostics) and the selected-path
/// sanitizer pass (sanitized model messages and stable units, isolated from off-path branches
/// per ADR 0124/0132). It never calls live reducer apply methods: replay is deliberately
/// separate.
#[allow(
    dead_code,
    reason = "the M5.2 replay seam is consumed by focused replay tests and the pending Load seam"
)]
pub(crate) fn replay_conversation<R: Read>(
    reader: R,
    declared_file_bytes: u64,
    expected_session_id: SessionId,
    access: ConversationScanAccess<'_>,
) -> Result<ReplayedConversationView, ConversationReplayError> {
    let mut scanner =
        ConversationJsonlScanner::open(reader, declared_file_bytes, expected_session_id, access)
            .map_err(map_replay_scan_error)?;
    let header = scanner.header().map_err(map_replay_scan_error)?.clone();
    let header_is_canonical = scanner.header_is_canonical();

    let mut scan = ReplayScanState::new();
    loop {
        match scanner.next_event().map_err(map_replay_scan_error)? {
            None => break,
            Some(event) => scan.observe(event),
        }
    }
    let eof_location = scan
        .last_location
        .unwrap_or_else(|| ConversationPhysicalLocation::new(0, 0));

    let (selected_path, selected_head) = scan.selected_path();
    let path_members = selected_path.iter().copied().collect::<BTreeSet<_>>();
    let selected_entries = selected_path
        .iter()
        .map(|entry_id| {
            let index = scan
                .entry_index
                .get(entry_id)
                .expect("selected path entries are present in the replay index");
            Arc::clone(&scan.entries[*index].entry)
        })
        .collect::<Vec<_>>();

    // Pass one: all-history semantic facts in physical order (historical items, accepted
    // ToolCall facts, per-fact Tool/Interaction validity and their owning diagnostics). It
    // never builds a model-visible pending exchange or stable units.
    let facts = {
        let facts_pass = ReplaySemanticFactsPass {
            entries: &scan.entries,
            seen_items: BTreeSet::new(),
            historical_item_ids: Vec::new(),
            tool_call_items: BTreeSet::new(),
            call_facts: BTreeMap::new(),
            live_calls_by_id: BTreeMap::new(),
            accepted_calls_by_assistant: BTreeMap::new(),
            invalidated_exchanges: BTreeSet::new(),
            valid_items_by_entry: BTreeMap::new(),
            valid_tool_terminals: BTreeMap::new(),
            invalid_tool_facts: BTreeMap::new(),
            interactions: BTreeMap::new(),
            diagnostics: &mut scan.diagnostics,
        };
        facts_pass.run()
    };

    // Pass two: the selected-path-only sanitizer, which owns the pending Tool exchange and the
    // stable units. Off-path entries never mutate it; invalid facts from pass one are skipped
    // without duplicate diagnostics.
    let projection = {
        let mut sanitizer = ReplaySanitizerPass {
            entries: &scan.entries,
            path_members: &path_members,
            facts: &facts,
            units: Vec::new(),
            relations: Vec::new(),
            pending_exchange: None,
            revision_operations: 0,
            revision_overflowed: false,
            diagnostics: &mut scan.diagnostics,
        };
        sanitizer.run(eof_location)?
    };
    let stable_units = projection
        .units
        .iter()
        .map(|unit| {
            PreparedLiveCompactionUnit::for_replay(unit.kind, unit.messages.clone().into())
                .map(|prepared| prepared.bind_origin(unit.origin))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ConversationReplayError::InvariantViolation)?;

    Ok(ReplayedConversationView {
        header,
        header_is_canonical,
        accepted_entries: scan
            .entries
            .iter()
            .map(|scan_entry| Arc::clone(&scan_entry.entry))
            .collect::<Vec<_>>()
            .into(),
        selected_entries: selected_entries.into(),
        selected_path: selected_path.into(),
        selected_head,
        reserved_ids: scan.reserved_ids.into(),
        sanitized_messages: projection.messages.into(),
        stable_units: stable_units.into(),
        relations: projection.relations.into(),
        revision: ConversationRevision::from_replay_operations(projection.revision_operations),
        historical_item_ids: facts.historical_item_ids.into(),
        diagnostics: scan.diagnostics.finish(),
        tail_action: scan.tail_action,
    })
}

/// Redacted failures from the replay-backed loaded-conversation preparation owner.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ConversationLoadError {
    #[error("conversation replay failed")]
    Replay(ConversationReplayError),
    #[error("conversation tail truncation failed")]
    TailTruncateFailed,
    #[error("conversation live state seed violated its owner invariant")]
    LiveStateInvariant,
    #[error("conversation load task failed")]
    Runtime(RuntimeTaskError),
}

/// Test-only coordination at named points inside one replay-backed Load preparation.
///
/// The production load path never constructs a barrier, so production behavior is identical
/// across build configurations.  The type itself exists in every build only so the
/// owner-tracked blocking worker signature stays uniform; every behavior is `#[cfg(test)]`.
#[allow(
    dead_code,
    reason = "the replay preparation seam type keeps the owner-tracked worker signature uniform in production builds"
)]
pub(crate) struct ReplayPreparationBarrier {
    #[cfg(test)]
    inner: Arc<ReplayPreparationBarrierInner>,
}

#[cfg(test)]
struct ReplayPreparationBarrierInner {
    before_spawn_armed: AtomicBool,
    before_spawn_entered: AtomicBool,
    before_spawn_released: AtomicBool,
    before_spawn_changed: Notify,
    before_recorder_armed: AtomicBool,
    before_recorder_entered: AtomicBool,
    before_recorder_changed: Notify,
    before_recorder_release_lock: Mutex<bool>,
    before_recorder_wake: Condvar,
    panic_before_recorder: AtomicBool,
    corrupt_length_before_recorder: AtomicBool,
}

#[cfg(test)]
impl ReplayPreparationBarrier {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(ReplayPreparationBarrierInner {
                before_spawn_armed: AtomicBool::new(false),
                before_spawn_entered: AtomicBool::new(false),
                before_spawn_released: AtomicBool::new(false),
                before_spawn_changed: Notify::new(),
                before_recorder_armed: AtomicBool::new(false),
                before_recorder_entered: AtomicBool::new(false),
                before_recorder_changed: Notify::new(),
                before_recorder_release_lock: Mutex::new(false),
                before_recorder_wake: Condvar::new(),
                panic_before_recorder: AtomicBool::new(false),
                corrupt_length_before_recorder: AtomicBool::new(false),
            }),
        })
    }

    /// Pauses the next admitted Load just before its replay blocking worker is spawned.
    ///
    /// Arming resets the previous cycle's entered/released state, so the same seam can be
    /// reused without an already-passed cycle making the next wait return immediately.
    pub(crate) fn arm_before_spawn(&self) {
        self.inner
            .before_spawn_entered
            .store(false, Ordering::Release);
        self.inner
            .before_spawn_released
            .store(false, Ordering::Release);
        self.inner.before_spawn_armed.store(true, Ordering::Release);
        self.inner.before_spawn_changed.notify_waiters();
    }

    pub(crate) async fn wait_until_before_spawn(&self) {
        loop {
            let notified = self.inner.before_spawn_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.before_spawn_entered.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn release_before_spawn(&self) {
        self.inner
            .before_spawn_released
            .store(true, Ordering::Release);
        self.inner.before_spawn_changed.notify_waiters();
    }

    /// Pauses the admitted Load inside the replay worker, between replay preparation and
    /// Recorder construction from the same published target parts.
    ///
    /// Arming resets the previous cycle's entered/release state, so the same seam can be reused
    /// without an already-passed cycle making the next wait return immediately.  The release
    /// lock is touched only synchronously here, never across an await.
    pub(crate) fn arm_before_recorder(&self) {
        self.inner
            .before_recorder_entered
            .store(false, Ordering::Release);
        {
            let mut released = lock_recorder(&self.inner.before_recorder_release_lock);
            *released = false;
        }
        self.inner
            .before_recorder_armed
            .store(true, Ordering::Release);
        self.inner.before_recorder_changed.notify_waiters();
    }

    pub(crate) async fn wait_until_before_recorder(&self) {
        loop {
            let notified = self.inner.before_recorder_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.before_recorder_entered.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn release_before_recorder(&self) {
        let mut released = lock_recorder(&self.inner.before_recorder_release_lock);
        *released = true;
        drop(released);
        self.inner.before_recorder_wake.notify_all();
    }

    /// Makes the same physical target fail its declared-length invariant at Recorder
    /// construction, without manufacturing a second target or writable proof.
    pub(crate) fn corrupt_length_before_recorder(&self) {
        self.inner
            .corrupt_length_before_recorder
            .store(true, Ordering::Release);
    }

    /// Panics the next replay blocking worker at the named seam.  The panic payload is caught
    /// and redacted by the owner-tracked job boundary.
    pub(crate) fn panic_before_recorder(&self) {
        self.inner
            .panic_before_recorder
            .store(true, Ordering::Release);
    }

    async fn before_spawn(&self) {
        if self.inner.before_spawn_armed.swap(false, Ordering::AcqRel) {
            self.inner
                .before_spawn_entered
                .store(true, Ordering::Release);
            self.inner.before_spawn_changed.notify_waiters();
            loop {
                let notified = self.inner.before_spawn_changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.inner.before_spawn_released.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        }
    }

    fn before_recorder(&self, file: &File) {
        if self
            .inner
            .before_recorder_armed
            .swap(false, Ordering::AcqRel)
        {
            self.inner
                .before_recorder_entered
                .store(true, Ordering::Release);
            self.inner.before_recorder_changed.notify_waiters();
            let mut released = lock_recorder(&self.inner.before_recorder_release_lock);
            while !*released {
                released = self
                    .inner
                    .before_recorder_wake
                    .wait(released)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            drop(released);
        }
        if self
            .inner
            .panic_before_recorder
            .swap(false, Ordering::AcqRel)
        {
            panic!("ReplayPreparationBarrier requested a replay worker panic");
        }
        if self
            .inner
            .corrupt_length_before_recorder
            .swap(false, Ordering::AcqRel)
        {
            // The same published target: append one byte so the Recorder's declared-length
            // invariant fails for this exact target and proof pair.
            let mut sink = file;
            let _ = sink.write_all(b"x");
        }
    }
}

/// The cold conversation objects installed into one loaded Session.
pub(crate) struct ReplayedConversationLoad {
    pub(crate) live_state: LiveSessionState,
    pub(crate) recorder: SessionRecorder,
    pub(crate) diagnostics: ConversationReplayDiagnostics,
}

/// Replays and prepares one DurableState-issued target while retaining all physical handles in
/// the same owner-tracked blocking job. The target is never exposed to residency as a path or a
/// separately manufactured writable lease.
#[allow(
    dead_code,
    reason = "unit tests route the same owner-tracked worker through the named replay barrier entry point"
)]
pub(crate) async fn load_replayed_conversation(
    target: PublishedConversationTarget,
    task_context: RuntimeTaskContext,
) -> Result<ReplayedConversationLoad, ConversationLoadError> {
    load_replayed_conversation_inner(target, task_context, None).await
}

/// Test-only entry point that routes one named replay preparation barrier into the same
/// owner-tracked worker.  Production Load always passes `None`.
#[cfg(test)]
pub(crate) async fn load_replayed_conversation_with_barrier_for_test(
    target: PublishedConversationTarget,
    task_context: RuntimeTaskContext,
    barrier: Option<Arc<ReplayPreparationBarrier>>,
) -> Result<ReplayedConversationLoad, ConversationLoadError> {
    load_replayed_conversation_inner(target, task_context, barrier).await
}

async fn load_replayed_conversation_inner(
    target: PublishedConversationTarget,
    task_context: RuntimeTaskContext,
    barrier: Option<Arc<ReplayPreparationBarrier>>,
) -> Result<ReplayedConversationLoad, ConversationLoadError> {
    let session_id = target.session_id();
    let declared_file_bytes = target.declared_file_bytes();
    let (file, writable_lease) = target.into_parts();
    let result_slot: Arc<Mutex<Option<Result<ReplayedConversationLoad, ConversationLoadError>>>> =
        Arc::new(Mutex::new(None));
    let worker_slot = Arc::clone(&result_slot);
    let recorder_task_context = task_context.clone();
    #[cfg(test)]
    if let Some(barrier) = &barrier {
        barrier.before_spawn().await;
    }
    let job = task_context.spawn_blocking_tracked(move || {
        let result = prepare_replayed_conversation_blocking(
            session_id,
            declared_file_bytes,
            file,
            writable_lease,
            recorder_task_context,
            barrier,
        );
        *lock_recorder(&worker_slot) = Some(result);
    });

    match job.wait().await {
        Ok(()) => lock_recorder(&result_slot)
            .take()
            .ok_or(ConversationLoadError::Runtime(
                RuntimeTaskError::WorkerUnavailable,
            ))?,
        Err(error) => Err(ConversationLoadError::Runtime(error)),
    }
}

fn prepare_replayed_conversation_blocking(
    session_id: SessionId,
    mut declared_file_bytes: u64,
    file: File,
    mut writable_lease: ExclusiveWritableConversationLease,
    task_context: RuntimeTaskContext,
    _barrier: Option<Arc<ReplayPreparationBarrier>>,
) -> Result<ReplayedConversationLoad, ConversationLoadError> {
    let replay_reader = file
        .try_clone()
        .map_err(|_| ConversationLoadError::Replay(ConversationReplayError::InputUnavailable))?;
    let view = replay_conversation(
        replay_reader,
        declared_file_bytes,
        session_id,
        ConversationScanAccess::ExclusiveWritable(&writable_lease),
    )
    .map_err(ConversationLoadError::Replay)?;

    if let Some(ConversationPartialTailAction::TruncateTo { offset }) = view.tail_action() {
        writable_lease
            .truncate_to(offset)
            .map_err(|_| ConversationLoadError::TailTruncateFailed)?;
        declared_file_bytes = offset;
    }

    let live_state = LiveSessionState::from_replayed_view(session_id, &view)
        .map_err(|_| ConversationLoadError::LiveStateInvariant)?;
    let diagnostics = view.diagnostics().clone();
    #[cfg(test)]
    if let Some(barrier) = &_barrier {
        barrier.before_recorder(&file);
    }
    let recorder = SessionRecorder::from_published_parts(
        session_id,
        declared_file_bytes,
        file,
        writable_lease,
        task_context,
    );
    Ok(ReplayedConversationLoad {
        live_state,
        recorder,
        diagnostics,
    })
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
    /// Write only the canonical encoded JSON without the trailing LF, then fail the exact
    /// attempt: the file keeps an unterminated tail.
    PartialWrite,
    /// Write the complete newline-terminated line, then panic before the attempt settles: the
    /// line is kept but the Recorder degrades through the operation failure.
    PanicAfterWrite,
}

/// Test-only coordination around the first physical append write.
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
    written: AtomicBool,
    written_notify: Notify,
    after_write_release: Mutex<bool>,
    after_write_changed: std::sync::Condvar,
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
            written: AtomicBool::new(false),
            written_notify: Notify::new(),
            after_write_release: Mutex::new(true),
            after_write_changed: std::sync::Condvar::new(),
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

    pub(crate) async fn wait_until_written(&self) {
        loop {
            let notified = self.written_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.written.load(Ordering::Acquire) {
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

    pub(crate) fn hold_after_write(&self) {
        *lock_recorder(&self.after_write_release) = false;
    }

    pub(crate) fn release_after_write(&self) {
        let mut released = lock_recorder(&self.after_write_release);
        *released = true;
        drop(released);
        self.after_write_changed.notify_all();
    }

    pub(crate) fn fail_before_write(&self) {
        *lock_recorder(&self.fault) = Some(RecorderWriteFault::Fail);
        self.release();
    }

    pub(crate) fn panic_before_write(&self) {
        *lock_recorder(&self.fault) = Some(RecorderWriteFault::Panic);
        self.release();
    }

    pub(crate) fn partial_write(&self) {
        *lock_recorder(&self.fault) = Some(RecorderWriteFault::PartialWrite);
        self.release();
    }

    pub(crate) fn panic_after_write(&self) {
        *lock_recorder(&self.fault) = Some(RecorderWriteFault::PanicAfterWrite);
        self.release();
    }

    /// Waits for the before-write gate and returns the exact consumed fault. The worker keeps
    /// ownership of the post-gate faults and performs the corresponding physical write shape;
    /// `Fail` and `Panic` are fully consumed here and never reach the worker.
    fn before_first_write(&self) -> Result<Option<RecorderWriteFault>, SessionRecordingError> {
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

        let fault = lock_recorder(&self.fault).take();
        match fault {
            Some(RecorderWriteFault::Fail) => Err(SessionRecordingError::WriteFailed),
            Some(RecorderWriteFault::Panic) => {
                panic!("RecorderWriteBarrier requested a write panic")
            }
            other => Ok(other),
        }
    }

    fn after_first_write(&self) {
        self.written.store(true, Ordering::Release);
        self.written_notify.notify_waiters();

        let mut released = lock_recorder(&self.after_write_release);
        while !*released {
            released = self
                .after_write_changed
                .wait(released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            match barrier.before_first_write() {
                Err(error) => {
                    drop(file);
                    guard.fail(error);
                    return;
                }
                Ok(Some(RecorderWriteFault::PartialWrite)) => {
                    // Keep only the canonical encoded JSON without the trailing LF, then fail
                    // the exact attempt: the file retains an unterminated tail.
                    if file.write_all(&line[..line.len() - 1]).is_err() {
                        drop(file);
                        guard.fail(SessionRecordingError::WriteFailed);
                        return;
                    }
                    drop(file);
                    guard.fail(SessionRecordingError::WriteFailed);
                    return;
                }
                Ok(Some(RecorderWriteFault::PanicAfterWrite)) => {
                    // Write the complete newline-terminated line, then unwind before the
                    // attempt settles: the line is kept and the operation failure degrades the
                    // exact attempt.
                    if file.write_all(&line).is_err() {
                        drop(file);
                        guard.fail(SessionRecordingError::WriteFailed);
                        return;
                    }
                    panic!("RecorderWriteBarrier requested a post-write panic");
                }
                Ok(Some(RecorderWriteFault::Fail | RecorderWriteFault::Panic)) => {
                    unreachable!("Fail and Panic faults are consumed inside the barrier")
                }
                Ok(None) => {}
            }
            if file.write_all(&line).is_err() {
                drop(file);
                guard.fail(SessionRecordingError::WriteFailed);
                return;
            }
            #[cfg(test)]
            barrier.after_first_write();
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
    #[cfg(test)]
    pub(crate) fn from_published_target(
        target: PublishedConversationTarget,
        task_context: RuntimeTaskContext,
    ) -> Self {
        let target_session_id = target.session_id();
        let target_file_bytes = target.declared_file_bytes();
        let (file, writable_lease) = target.into_parts();

        Self::from_published_parts(
            target_session_id,
            target_file_bytes,
            file,
            writable_lease,
            task_context,
        )
    }

    /// Consumes the exact File/proof pair transferred from one PublishedConversationTarget. This
    /// is used by the replay-backed Load owner after it has optionally truncated the scanner's
    /// final partial tail; the method does not issue or reconstruct a production proof.
    pub(crate) fn from_published_parts(
        target_session_id: SessionId,
        target_file_bytes: u64,
        file: File,
        writable_lease: ExclusiveWritableConversationLease,
        task_context: RuntimeTaskContext,
    ) -> Self {
        // The writable lease is only a same-file binding proof. Its private File is deliberately
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

    use serde_json::Value;

    use super::*;
    use crate::agent_session_lifecycle::AgentRevisionRef;
    use crate::model_gateway::{
        ModelId, ModelReasoningSummary, ModelServiceClass, ProviderId, ProviderResponseMetadata,
    };
    use crate::prompt::{
        CanonicalUserMessage, MessageContent, MessageRecord, ModelAssistantContentRef,
        ModelMessageRef,
    };
    use crate::tools::{
        ToolApprovalOptionKindView, ToolApprovalOptionView, ToolApprovalResolutionRef,
        ToolRequirementSummaryView, UserQuestionField, UserQuestionFieldAnswer, UserQuestionInput,
        UserQuestionRequest,
    };
    use crate::wire::BoundedJsonObject;
    use crate::wire::conversation_jsonl::{
        ConversationLineCodec, ConversationRecord, MAX_CONVERSATION_ENTRY_BYTES,
        MAX_CONVERSATION_HEADER_BYTES,
    };

    const HEADER_ONLY: &[u8] =
        include_bytes!("../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl");
    const FORK_SOURCE: &[u8] =
        include_bytes!("../docs/fixtures/durable-store-v1/fork-source.jsonl");
    const FORK_CHILD: &[u8] = include_bytes!("../docs/fixtures/durable-store-v1/fork-child.jsonl");
    static RECORDER_TEST_FILE: AtomicU64 = AtomicU64::new(0);
    const PREFIX_ENTRY_ID: &str = "ent_10000000000000000000000000000001";
    const CHILD_ENTRY_ID: &str = "ent_10000000000000000000000000000002";
    const AFTER_ENTRY_ID: &str = "ent_10000000000000000000000000000003";

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

    /// One assistant entry with an explicit EntryId and optional parent, so a recorder test can
    /// append a linked suffix that cold replay keeps on the selected path.
    fn recorder_entry_with_id(
        entry_id: &str,
        parent_id: Option<&str>,
        text: &str,
    ) -> Arc<StoredSessionEntry> {
        // Each test entry needs its own ItemId: replay only accepts first-valid item identity,
        // so two entries sharing the helper's default item id would drop the second message.
        let item_id = format!("itm_{}", &entry_id[4..]);
        let value = entry(StoredEntryBody::AssistantMessage(assistant(vec![
            StoredAssistantContent::Text {
                item_id: item_id.parse().expect("test ItemId is valid"),
                text: text.to_owned().into(),
            },
        ])));
        Arc::new(StoredSessionEntry::reconstruct(
            entry_id.parse().expect("test EntryId is valid"),
            parent_id.map(|parent_id| parent_id.parse().expect("test parent EntryId is valid")),
            value.session_id(),
            value.turn_id(),
            value.timestamp(),
            value.body().clone(),
        ))
    }

    fn assert_replayed_assistant_texts(view: &ReplayedConversationView, texts: &[&str]) {
        let messages = view.sanitized_messages();
        assert_eq!(
            messages.len(),
            texts.len(),
            "sanitized message count matches the recorded assistant lines"
        );
        for (message, expected) in messages.iter().zip(texts) {
            match message.as_ref() {
                ModelMessageRef::Assistant { content } => {
                    assert_eq!(content.len(), 1, "one assistant text block");
                    match content[0].as_ref() {
                        ModelAssistantContentRef::Text(actual) => {
                            assert_eq!(actual, *expected, "assistant text")
                        }
                        other => panic!("expected a text block, got {other:?}"),
                    }
                }
                other => panic!("expected an assistant message, got {other:?}"),
            }
        }
    }

    fn cold_replay_at(path: &std::path::Path) -> (Vec<u8>, ReplayedConversationView) {
        let bytes = std::fs::read(path).expect("recorder test target reads");
        let view = replay_fixture(&bytes, standard_header().session_id())
            .expect("the recorder file cold-replays tolerantly");
        (bytes, view)
    }

    async fn writable_cold_load(
        path: &std::path::Path,
        task_context: RuntimeTaskContext,
    ) -> ReplayedConversationLoad {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .expect("recorder test target opens");
        let file_bytes = file.metadata().expect("recorder test metadata reads").len();
        let writable_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("recorder test writable lease opens");
        let target = PublishedConversationTarget::from_durable_state(
            standard_header().session_id(),
            file_bytes,
            file,
            writable_file,
        );
        load_replayed_conversation(target, task_context)
            .await
            .expect("the writable cold load prepares the exact target")
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

    #[derive(Default)]
    struct ForkStreamingWriter {
        bytes: Vec<u8>,
        writes: usize,
        largest_write: usize,
    }

    impl Write for ForkStreamingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.largest_write = self.largest_write.max(bytes.len());
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fork_child_reencode_and_readback_stream_one_bounded_canonical_path() {
        let source_session_id = fixture_session(FORK_SOURCE);
        let source = replay_fixture(FORK_SOURCE, source_session_id)
            .expect("the authoritative Fork source replays");
        let anchor = ForkAnchor::AfterUserMessage {
            item_id: "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .parse()
                .expect("the authoritative anchor ItemId is valid"),
        };
        let captured = CapturedForkConversation::from_selected_path(
            source_session_id,
            ForkSourceKind::RecordedHistory,
            anchor,
            source.selected_entries(),
            source.relations(),
        )
        .expect("the authoritative selected path captures");
        let header = fork_header();
        let mut sink = ForkStreamingWriter::default();

        let written = captured
            .write_for_child(&header, &mut sink)
            .expect("the captured path streams");

        assert_eq!(written, u64::try_from(FORK_CHILD.len()).unwrap());
        assert_eq!(sink.bytes, FORK_CHILD);
        assert!(
            sink.writes > captured.selected_entries.len(),
            "Header, entries, and LF separators are written incrementally"
        );
        assert!(
            sink.largest_write <= MAX_CONVERSATION_ENTRY_BYTES,
            "the sink never receives a whole-file allocation"
        );
        captured
            .validate_reencoded_child(Cursor::new(sink.bytes), written, &header)
            .expect("the full streamed readback matches the captured source path");
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
    async fn recorder_caller_drop_after_barrier() {
        let (task_context, recorder, path) = recorder_fixture().await;
        let barrier = RecorderWriteBarrier::new();
        barrier.hold_after_write();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));

        let mut record = Box::pin(recorder.record(recorder_entry("after physical write")));
        poll_to_pending(&mut record).await;
        barrier.release();
        barrier.wait_until_written().await;

        // The bytes are already newline-terminated, but the exact tracked attempt has not yet
        // settled. Dropping the caller must leave that attempt for close() to reap.
        drop(record);
        let mut close = Box::pin(recorder.close());
        poll_to_pending(&mut close).await;
        barrier.release_after_write();
        close.await;

        assert!(matches!(*recorder.health(), RecordingHealth::Healthy));
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        let bytes = std::fs::read(&path).expect("recorder test target reads");
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
        let entry_line = bytes
            .split(|byte| *byte == b'\n')
            .nth(1)
            .expect("the completed append has one entry line");
        ConversationLineCodec::decode_entry_for_session(entry_line, standard_header().session_id())
            .expect("the complete append remains replayable");

        // The dropped caller still left a complete newline-terminated line; cold replay accepts
        // it as a line (matrix `recorder.caller_drop.after_write`: complete_line_replayed).
        let view = replay_fixture(&bytes, standard_header().session_id())
            .expect("the complete line cold-replays after the caller drop");
        assert_eq!(
            view.tail_action(),
            None,
            "no partial tail after the caller drop"
        );
        assert_eq!(view.accepted_entry_ids().len(), 1);
        assert_eq!(view.selected_path().len(), 1);
        assert_replayed_assistant_texts(&view, &["after physical write"]);
        assert!(view.diagnostics().is_empty());

        task_context.shutdown().await;
        std::fs::remove_file(path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_partial_tail_replay() {
        // Matrix `recorder.write.partial_tail`: a partial physical write leaves an unterminated
        // tail, the exact attempt terminally degrades, and cold replay ignores or truncates that
        // tail while keeping the recorded prefix.
        let (task_context, recorder, path) = recorder_fixture().await;
        let prefix = recorder_entry_with_id(PREFIX_ENTRY_ID, None, "prefix");
        assert_eq!(
            recorder.record(prefix.clone()).await,
            RecordOutcome::Written
        );

        let partial = recorder_entry_with_id(CHILD_ENTRY_ID, Some(PREFIX_ENTRY_ID), "partial tail");
        let partial_entry_id = partial.entry_id();
        let partial_encoded = ConversationLineCodec::encode_entry(&partial).expect("entry encodes");
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        barrier.partial_write();

        assert!(matches!(
            recorder.record(partial.clone()).await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded {
                    failed_entry_id: Some(failed_entry_id),
                    reason: SessionRecordingError::WriteFailed,
                }
            } if failed_entry_id == partial_entry_id
        ));
        recorder.close().await;
        task_context.shutdown().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);

        // The file keeps the unterminated tail: the canonical JSON without the trailing LF.
        let (bytes, view) = cold_replay_at(&path);
        assert!(
            bytes.ends_with(&partial_encoded) && !bytes.ends_with(b"\n"),
            "the failed append leaves the canonical JSON without the trailing LF"
        );
        assert_eq!(
            view.tail_action(),
            Some(ConversationPartialTailAction::Ignore),
            "read-only replay ignores the partial tail"
        );
        assert_eq!(view.accepted_entry_ids(), [prefix.entry_id()]);
        assert_eq!(view.selected_path(), [prefix.entry_id()].as_slice());
        assert_replayed_assistant_texts(&view, &["prefix"]);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::PartialTail),
            1
        );

        // A writable cold load of the same target truncates to the last LF; the prefix then
        // replays cleanly and the fresh Recorder is Healthy.
        let load_context = RuntimeTaskContext::new(tokio::runtime::Handle::current())
            .await
            .expect("the Tokio test runtime has a time driver");
        let loaded = writable_cold_load(&path, load_context.clone()).await;
        let truncated = std::fs::read(&path).expect("recorder test target reads");
        let mut expected_prefix = HEADER_ONLY.to_vec();
        expected_prefix.extend_from_slice(
            &ConversationLineCodec::encode_entry(&prefix).expect("entry encodes"),
        );
        expected_prefix.push(b'\n');
        assert_eq!(
            truncated, expected_prefix,
            "the writable load truncates the partial tail to the last LF"
        );
        assert_eq!(
            loaded
                .diagnostics
                .count(ConversationReplayDiagnosticCode::PartialTail),
            1
        );
        assert!(matches!(
            *loaded.recorder.health(),
            RecordingHealth::Healthy
        ));

        // The truncated prefix is again writable: a fresh append replays as a complete line.
        let after =
            recorder_entry_with_id(AFTER_ENTRY_ID, Some(PREFIX_ENTRY_ID), "after truncation");
        assert_eq!(
            loaded.recorder.record(after.clone()).await,
            RecordOutcome::Written
        );
        loaded.recorder.close().await;
        load_context.shutdown().await;
        assert_eq!(load_context.registered_task_count_for_test(), 0);
        let (final_bytes, final_view) = cold_replay_at(&path);
        assert!(final_bytes.ends_with(b"\n"), "no unterminated tail remains");
        assert_eq!(final_view.tail_action(), None);
        assert_eq!(
            final_view.accepted_entry_ids(),
            [prefix.entry_id(), after.entry_id()]
        );
        assert_eq!(
            final_view.selected_path(),
            [prefix.entry_id(), after.entry_id()].as_slice()
        );
        assert_replayed_assistant_texts(&final_view, &["prefix", "after truncation"]);
        assert!(final_view.diagnostics().is_empty());
        std::fs::remove_file(&path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_full_line_replay() {
        // Matrix `recorder.write.full_line_after_side_effect`: the complete newline-terminated
        // line is physically written, then the worker fails before the attempt settles; the line
        // is kept, the Recorder degrades, and cold replay accepts the complete line.
        let (task_context, recorder, path) = recorder_fixture().await;
        let prefix = recorder_entry_with_id(PREFIX_ENTRY_ID, None, "prefix");
        assert_eq!(
            recorder.record(prefix.clone()).await,
            RecordOutcome::Written
        );

        let child = recorder_entry_with_id(CHILD_ENTRY_ID, Some(PREFIX_ENTRY_ID), "complete line");
        let child_entry_id = child.entry_id();
        let child_encoded = ConversationLineCodec::encode_entry(&child).expect("entry encodes");
        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        barrier.panic_after_write();

        assert!(matches!(
            recorder.record(child.clone()).await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded {
                    failed_entry_id: Some(failed_entry_id),
                    reason: SessionRecordingError::Runtime(RuntimeTaskError::OperationPanicked),
                }
            } if failed_entry_id == child_entry_id
        ));
        recorder.close().await;
        task_context.shutdown().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);

        let (bytes, view) = cold_replay_at(&path);
        assert!(
            bytes.len() > child_encoded.len()
                && bytes.ends_with(b"\n")
                && bytes[..bytes.len() - 1].ends_with(&child_encoded),
            "the complete newline-terminated line survives the worker failure"
        );
        assert_eq!(
            view.tail_action(),
            None,
            "a complete line is never treated as a partial tail"
        );
        assert_eq!(
            view.accepted_entry_ids(),
            [prefix.entry_id(), child.entry_id()]
        );
        assert_eq!(
            view.selected_path(),
            [prefix.entry_id(), child.entry_id()].as_slice()
        );
        assert_replayed_assistant_texts(&view, &["prefix", "complete line"]);
        assert!(view.diagnostics().is_empty());

        // A writable cold load of the same target must not mis-truncate the complete line.
        let load_context = RuntimeTaskContext::new(tokio::runtime::Handle::current())
            .await
            .expect("the Tokio test runtime has a time driver");
        let loaded = writable_cold_load(&path, load_context.clone()).await;
        assert_eq!(
            std::fs::read(&path).expect("recorder test target reads"),
            bytes,
            "the writable load keeps the complete line untouched"
        );
        assert!(loaded.diagnostics.is_empty());
        assert!(matches!(
            *loaded.recorder.health(),
            RecordingHealth::Healthy
        ));
        loaded.recorder.close().await;
        load_context.shutdown().await;
        assert_eq!(load_context.registered_task_count_for_test(), 0);
        std::fs::remove_file(&path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_spawn_failure() {
        // Matrix `recorder.job.spawn_failure`: the owner stops admitting tracked work, the exact
        // append spawn is rejected, and only the recorded prefix cold-replays.
        let (task_context, recorder, path) = recorder_fixture().await;
        let prefix = recorder_entry_with_id(PREFIX_ENTRY_ID, None, "prefix");
        assert_eq!(
            recorder.record(prefix.clone()).await,
            RecordOutcome::Written
        );

        task_context.request_closing();
        let suffix =
            recorder_entry_with_id(CHILD_ENTRY_ID, Some(PREFIX_ENTRY_ID), "spawn rejected");
        let suffix_entry_id = suffix.entry_id();
        assert!(matches!(
            recorder.record(suffix).await,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded {
                    failed_entry_id: Some(failed_entry_id),
                    reason: SessionRecordingError::Runtime(RuntimeTaskError::OwnerClosing),
                }
            } if failed_entry_id == suffix_entry_id
        ));
        assert_eq!(
            task_context.registered_task_count_for_test(),
            0,
            "a rejected spawn never registers a worker"
        );
        recorder.close().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        task_context.shutdown().await;

        let (bytes, view) = cold_replay_at(&path);
        let mut expected = HEADER_ONLY.to_vec();
        expected.extend_from_slice(
            &ConversationLineCodec::encode_entry(&prefix).expect("entry encodes"),
        );
        expected.push(b'\n');
        assert_eq!(bytes, expected, "only the recorded prefix is on the file");
        assert_eq!(view.tail_action(), None);
        assert_eq!(view.accepted_entry_ids(), [prefix.entry_id()]);
        assert_eq!(view.selected_path(), [prefix.entry_id()].as_slice());
        assert_replayed_assistant_texts(&view, &["prefix"]);
        assert!(view.diagnostics().is_empty());
        std::fs::remove_file(&path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_caller_drop_before_barrier() {
        // Matrix `recorder.caller_drop.before_write`: the caller enters the write barrier and
        // drops; the exact attempt then fails before any write and terminally degrades the
        // Recorder, while the recorded prefix cold-replays.
        let (task_context, recorder, path) = recorder_fixture().await;
        let prefix = recorder_entry_with_id(PREFIX_ENTRY_ID, None, "prefix");
        assert_eq!(
            recorder.record(prefix.clone()).await,
            RecordOutcome::Written
        );

        let barrier = RecorderWriteBarrier::new();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        let child = recorder_entry_with_id(
            CHILD_ENTRY_ID,
            Some(PREFIX_ENTRY_ID),
            "dropped before write",
        );
        let child_entry_id = child.entry_id();
        let mut dropped = Box::pin(recorder.record(child));
        poll_to_pending(&mut dropped).await;
        barrier.wait_until_entered().await;
        drop(dropped);

        // The caller is gone before the write gate opens; the exact attempt then fails without
        // writing and terminally degrades the Recorder.
        barrier.fail_before_write();
        recorder.close().await;
        assert!(matches!(
            *recorder.health(),
            RecordingHealth::Degraded {
                failed_entry_id: Some(failed_entry_id),
                reason: SessionRecordingError::WriteFailed,
            } if failed_entry_id == child_entry_id
        ));
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        task_context.shutdown().await;

        let (bytes, view) = cold_replay_at(&path);
        let mut expected = HEADER_ONLY.to_vec();
        expected.extend_from_slice(
            &ConversationLineCodec::encode_entry(&prefix).expect("entry encodes"),
        );
        expected.push(b'\n');
        assert_eq!(
            bytes, expected,
            "the dropped-before-write entry is never appended"
        );
        assert_eq!(view.tail_action(), None);
        assert_eq!(view.accepted_entry_ids(), [prefix.entry_id()]);
        assert_eq!(view.selected_path(), [prefix.entry_id()].as_slice());
        assert_replayed_assistant_texts(&view, &["prefix"]);
        assert!(view.diagnostics().is_empty());
        std::fs::remove_file(&path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_join_panic() {
        // Matrix `recorder.job.join_panic`: the exact worker appends the complete canonical
        // line and settles its provisional success, then unwinds after the operation so the
        // raw Tokio join really fails; the terminal join failure degrades the Recorder and
        // replay accepts the complete line the worker already wrote.
        let (task_context, recorder, path) = recorder_fixture().await;
        let prefix = recorder_entry_with_id(PREFIX_ENTRY_ID, None, "prefix");
        assert_eq!(
            recorder.record(prefix.clone()).await,
            RecordOutcome::Written
        );

        // Hold after the complete physical write so `record` owns the exact raw join while
        // `close` becomes a second waiter on the same attempt. Releasing the worker lets it
        // settle provisional success and then fail its raw join; neither waiter may escape the
        // provisional result.
        let barrier = RecorderWriteBarrier::new();
        barrier.hold_after_write();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        task_context.inject_next_blocking_job_post_operation_panic();
        let child = recorder_entry_with_id(CHILD_ENTRY_ID, Some(PREFIX_ENTRY_ID), "join failure");
        let child_entry_id = child.entry_id();
        let child_encoded = ConversationLineCodec::encode_entry(&child).expect("entry encodes");

        let mut record = Box::pin(recorder.record(child.clone()));
        poll_to_pending(&mut record).await;
        barrier.release();
        barrier.wait_until_written().await;
        let mut close = Box::pin(recorder.close());
        poll_to_pending(&mut close).await;
        barrier.release_after_write();
        let (outcome, ()) = tokio::join!(record, close);
        assert!(matches!(
            outcome,
            RecordOutcome::NotRecorded {
                health: RecordingHealth::Degraded {
                    failed_entry_id: Some(failed_entry_id),
                    reason: SessionRecordingError::Runtime(RuntimeTaskError::WorkerUnavailable),
                }
            } if failed_entry_id == child_entry_id
        ));
        task_context.shutdown().await;
        assert_eq!(task_context.registered_task_count_for_test(), 0);

        // The provisional success already committed the complete line before the raw join
        // failed; cold replay accepts it as a complete line with no partial tail.
        let (bytes, view) = cold_replay_at(&path);
        assert!(
            bytes.len() > child_encoded.len()
                && bytes.ends_with(b"\n")
                && bytes[..bytes.len() - 1].ends_with(&child_encoded),
            "the complete newline-terminated line survives the raw join failure"
        );
        assert_eq!(
            view.tail_action(),
            None,
            "a complete line is never treated as a partial tail"
        );
        assert_eq!(
            view.accepted_entry_ids(),
            [prefix.entry_id(), child.entry_id()]
        );
        assert_eq!(
            view.selected_path(),
            [prefix.entry_id(), child.entry_id()].as_slice()
        );
        assert_replayed_assistant_texts(&view, &["prefix", "join failure"]);
        assert!(view.diagnostics().is_empty());
        std::fs::remove_file(&path).expect("recorder test target removes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_shutdown_join() {
        // Matrix `recorder.shutdown.waits_settlement`: the physical write happened but the exact
        // attempt is not yet settled; owner shutdown waits for the exact Recorder job, and both
        // the record and the shutdown complete with a Healthy Recorder and a complete replayed
        // line.
        let (task_context, recorder, path) = recorder_fixture().await;
        let prefix = recorder_entry_with_id(PREFIX_ENTRY_ID, None, "prefix");
        assert_eq!(
            recorder.record(prefix.clone()).await,
            RecordOutcome::Written
        );

        let barrier = RecorderWriteBarrier::new();
        barrier.hold_after_write();
        recorder.set_write_barrier_for_test(Arc::clone(&barrier));
        let child = recorder_entry_with_id(CHILD_ENTRY_ID, Some(PREFIX_ENTRY_ID), "complete line");
        let mut record = Box::pin(recorder.record(child.clone()));
        poll_to_pending(&mut record).await;
        barrier.release();
        barrier.wait_until_written().await;

        // The line is physically written but the attempt is not settled: shutdown must wait for
        // the exact Recorder job instead of closing over it.
        let mut shutdown = Box::pin(task_context.shutdown());
        poll_to_pending(&mut shutdown).await;
        assert!(
            task_context.is_closing(),
            "the owner shutdown is in its closing phase while it waits"
        );

        barrier.release_after_write();
        assert_eq!(record.await, RecordOutcome::Written);
        shutdown.await;

        assert!(matches!(*recorder.health(), RecordingHealth::Healthy));
        assert_eq!(task_context.registered_task_count_for_test(), 0);
        recorder.close().await;

        let (bytes, view) = cold_replay_at(&path);
        assert_eq!(
            bytes.iter().filter(|byte| **byte == b'\n').count(),
            3,
            "header plus two complete lines"
        );
        assert_eq!(view.tail_action(), None);
        assert_eq!(
            view.accepted_entry_ids(),
            [prefix.entry_id(), child.entry_id()]
        );
        assert_eq!(
            view.selected_path(),
            [prefix.entry_id(), child.entry_id()].as_slice()
        );
        assert_replayed_assistant_texts(&view, &["prefix", "complete line"]);
        assert!(view.diagnostics().is_empty());
        std::fs::remove_file(&path).expect("recorder test target removes");
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

    const TOOL_EXCHANGE_GOLDEN: &[u8] =
        include_bytes!("../docs/fixtures/wire-v1/conversation/golden/tool-exchange.jsonl");
    const INTERACTION_COMPACTION_GOLDEN: &[u8] =
        include_bytes!("../docs/fixtures/wire-v1/conversation/golden/interaction-compaction.jsonl");
    const INTERACTION_VARIANTS_GOLDEN: &[u8] =
        include_bytes!("../docs/fixtures/wire-v1/conversation/golden/interaction-variants.jsonl");

    struct CorruptionCase {
        name: &'static str,
        bytes: &'static [u8],
        expected: &'static str,
    }

    macro_rules! corruption_case {
        ($name:literal) => {
            CorruptionCase {
                name: $name,
                bytes: include_bytes!(concat!(
                    "../docs/fixtures/wire-v1/conversation/corruption/",
                    $name,
                    ".jsonl"
                )),
                expected: include_str!(concat!(
                    "../docs/fixtures/wire-v1/conversation/corruption/",
                    $name,
                    ".expected.json"
                )),
            }
        };
    }

    const CORRUPTION_CASES: &[CorruptionCase] = &[
        corruption_case!("branch-last-leaf"),
        corruption_case!("contribution-stamp-salvage"),
        corruption_case!("crlf-input"),
        corruption_case!("duplicate-entry-id"),
        corruption_case!("duplicate-header-key"),
        corruption_case!("entry-session-mismatch"),
        corruption_case!("incomplete-tool-exchange"),
        corruption_case!("invalid-compaction-marker"),
        corruption_case!("invalid-item-relation"),
        corruption_case!("malformed-middle"),
        corruption_case!("multiple-root-isolation"),
        corruption_case!("orphan-interaction-resolution"),
        corruption_case!("orphan-parent"),
        corruption_case!("partial-tail"),
        corruption_case!("tool-terminal-conflict"),
        corruption_case!("unknown-body-variant"),
        corruption_case!("unknown-entry-field"),
        corruption_case!("unknown-record-variant"),
        corruption_case!("unsupported-version-header"),
        corruption_case!("wrong-session-header"),
    ];

    fn fixture_session(bytes: &[u8]) -> SessionId {
        let first_line = bytes
            .split(|byte| *byte == b'\n')
            .next()
            .expect("fixture has a first line")
            .strip_suffix(b"\r")
            .unwrap_or_else(|| {
                bytes
                    .split(|byte| *byte == b'\n')
                    .next()
                    .expect("fixture has a first line")
            });
        let ConversationRecord::Header(header) = ConversationLineCodec::decode_record(first_line)
            .expect("replay fixture starts with a decodable Header")
        else {
            panic!("replay fixture must start with a Header");
        };
        header.session_id()
    }

    fn replay_fixture(
        bytes: &[u8],
        session_id: SessionId,
    ) -> Result<ReplayedConversationView, ConversationReplayError> {
        replay_conversation(
            Cursor::new(bytes.to_vec()),
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            session_id,
            ConversationScanAccess::ReadOnly,
        )
    }

    fn entry_ids(value: &Value) -> Vec<EntryId> {
        value
            .as_array()
            .expect("expected entry id array")
            .iter()
            .map(|value| value.as_str().expect("entry id string").parse().unwrap())
            .collect()
    }

    fn item_ids(value: &Value) -> Vec<ItemId> {
        value
            .as_array()
            .expect("expected item id array")
            .iter()
            .map(|value| value.as_str().expect("item id string").parse().unwrap())
            .collect()
    }

    fn assert_messages_match(messages: &[ModelMessage], expected: &Value, case: &str) {
        let expected = expected
            .as_array()
            .expect("expected sanitizedModelMessages array");
        assert_eq!(
            messages.len(),
            expected.len(),
            "{case}: sanitized message count"
        );
        for (index, (message, expected_message)) in messages.iter().zip(expected).enumerate() {
            match message.as_ref() {
                ModelMessageRef::User { content } => {
                    assert_eq!(
                        expected_message["role"], "user",
                        "{case}: message {index} role"
                    );
                    let expected_parts = expected_message["content"]
                        .as_array()
                        .expect("user content array");
                    assert_eq!(
                        content.len(),
                        expected_parts.len(),
                        "{case}: message {index} user part count"
                    );
                    for (part_index, (part, expected_part)) in
                        content.iter().zip(expected_parts).enumerate()
                    {
                        assert_eq!(
                            expected_part["type"], "text",
                            "{case}: message {index} part {part_index} type"
                        );
                        assert_eq!(
                            part.as_text(),
                            expected_part["data"]["text"].as_str().unwrap(),
                            "{case}: message {index} part {part_index} text"
                        );
                    }
                }
                ModelMessageRef::Assistant { content } => {
                    assert_eq!(
                        expected_message["role"], "assistant",
                        "{case}: message {index} role"
                    );
                    let expected_blocks = expected_message["content"]
                        .as_array()
                        .expect("assistant content array");
                    assert_eq!(
                        content.len(),
                        expected_blocks.len(),
                        "{case}: message {index} assistant block count"
                    );
                    for (block_index, (block, expected_block)) in
                        content.iter().zip(expected_blocks).enumerate()
                    {
                        match block.as_ref() {
                            ModelAssistantContentRef::Reasoning(value) => {
                                assert_eq!(
                                    expected_block["type"], "reasoning",
                                    "{case}: message {index} block {block_index} type"
                                );
                                assert_eq!(
                                    value.text(),
                                    expected_block["data"]["text"].as_str(),
                                    "{case}: message {index} block {block_index} reasoning text"
                                );
                                assert_eq!(
                                    value.summary(),
                                    expected_block["data"]["summary"].as_str(),
                                    "{case}: message {index} block {block_index} reasoning summary"
                                );
                                assert_eq!(
                                    value.encrypted(),
                                    expected_block["data"]["encrypted"].as_str(),
                                    "{case}: message {index} block {block_index} reasoning encrypted"
                                );
                                assert_eq!(
                                    value.signature(),
                                    expected_block["data"]["signature"].as_str(),
                                    "{case}: message {index} block {block_index} reasoning signature"
                                );
                                assert_eq!(
                                    value.provider_item_id().is_none(),
                                    expected_block["data"]["providerItemId"].is_null(),
                                    "{case}: message {index} block {block_index} provider item"
                                );
                            }
                            ModelAssistantContentRef::Text(text) => {
                                assert_eq!(
                                    expected_block["type"], "text",
                                    "{case}: message {index} block {block_index} type"
                                );
                                assert_eq!(
                                    text,
                                    expected_block["data"]["text"].as_str().unwrap(),
                                    "{case}: message {index} block {block_index} text"
                                );
                            }
                            ModelAssistantContentRef::ToolCall {
                                tool_call_id,
                                name,
                                arguments,
                            } => {
                                assert_eq!(
                                    expected_block["type"], "tool_call",
                                    "{case}: message {index} block {block_index} type"
                                );
                                assert_eq!(
                                    tool_call_id.as_str(),
                                    expected_block["data"]["toolCallId"].as_str().unwrap(),
                                    "{case}: message {index} block {block_index} tool call id"
                                );
                                assert_eq!(
                                    name.as_str(),
                                    expected_block["data"]["name"].as_str().unwrap(),
                                    "{case}: message {index} block {block_index} tool name"
                                );
                                let expected_arguments = BoundedJsonObject::from_slice(
                                    expected_block["data"]["arguments"].to_string().as_bytes(),
                                )
                                .expect("expected arguments parse");
                                assert_eq!(
                                    arguments.canonical_bytes(),
                                    expected_arguments.canonical_bytes(),
                                    "{case}: message {index} block {block_index} arguments"
                                );
                            }
                        }
                    }
                }
                ModelMessageRef::Tool {
                    tool_call_id,
                    content,
                } => {
                    assert_eq!(
                        expected_message["role"], "tool",
                        "{case}: message {index} role"
                    );
                    assert_eq!(
                        tool_call_id.as_str(),
                        expected_message["toolCallId"].as_str().unwrap(),
                        "{case}: message {index} tool call id"
                    );
                    let expected_parts = expected_message["content"]["parts"]
                        .as_array()
                        .expect("tool content parts array");
                    assert_eq!(
                        content.parts().len(),
                        expected_parts.len(),
                        "{case}: message {index} tool part count"
                    );
                    for (part_index, (part, expected_part)) in
                        content.parts().iter().zip(expected_parts).enumerate()
                    {
                        assert_eq!(
                            expected_part["type"], "text",
                            "{case}: message {index} part {part_index} type"
                        );
                        assert_eq!(
                            part.as_text(),
                            expected_part["data"]["text"].as_str().unwrap(),
                            "{case}: message {index} part {part_index} text"
                        );
                    }
                }
            }
        }
    }

    fn assert_diagnostic_counts(
        diagnostics: &ConversationReplayDiagnostics,
        expected: &Value,
        case: &str,
    ) {
        let expected = expected.as_array().expect("expected diagnostics array");
        assert_eq!(
            diagnostics.counts().len(),
            expected.len(),
            "{case}: diagnostic aggregate code set"
        );
        for group in expected {
            let code = ConversationReplayDiagnosticCode::from_name(
                group["code"].as_str().expect("diagnostic code name"),
            )
            .expect("expected diagnostic code is in the closed set");
            assert_eq!(
                diagnostics.count(code),
                group["count"].as_u64().expect("diagnostic count"),
                "{case}: aggregate count for {}",
                group["code"]
            );
        }
    }

    fn assert_tail_action(
        actual: Option<ConversationPartialTailAction>,
        expected_action: &Value,
        expected_offset: Option<u64>,
        case: &str,
    ) {
        match expected_action.as_str().expect("tail action name") {
            "none" => assert_eq!(actual, None, "{case}: tail action"),
            "ignore" => assert_eq!(
                actual,
                Some(ConversationPartialTailAction::Ignore),
                "{case}: tail action"
            ),
            "truncate" => assert_eq!(
                actual,
                Some(ConversationPartialTailAction::TruncateTo {
                    offset: expected_offset.expect("truncate offset"),
                }),
                "{case}: tail action"
            ),
            other => panic!("{case}: unexpected expected tail action {other}"),
        }
    }

    #[test]
    fn replay_executes_every_corruption_fixture_sidecar() {
        assert_eq!(CORRUPTION_CASES.len(), 20, "corruption fixture inventory");
        for case in CORRUPTION_CASES {
            let expected: Value =
                serde_json::from_str(case.expected).expect("expected sidecar is JSON");
            let length = u64::try_from(case.bytes.len()).expect("fixture length fits u64");
            let session_id = match expected["load"].as_str().expect("load outcome") {
                "fails" => expected["openedSessionId"]
                    .as_str()
                    .expect("opened session id")
                    .parse()
                    .expect("opened session id is valid"),
                _ => fixture_session(case.bytes),
            };

            let result = replay_fixture(case.bytes, session_id);
            match expected["load"].as_str().expect("load outcome") {
                "fails" => {
                    let error = result.expect_err("expected a typed load failure");
                    let expected_error = match expected["error"].as_str().expect("error name") {
                        "HeaderCorrupt" => ConversationReplayError::HeaderCorrupt,
                        "UnsupportedFormatVersion" => {
                            ConversationReplayError::UnsupportedFormatVersion
                        }
                        other => panic!("{}: unexpected expected error {other}", case.name),
                    };
                    assert_eq!(error, expected_error, "{}: typed load error", case.name);
                }
                _ => {
                    let view = result.unwrap_or_else(|error| {
                        panic!("{}: tolerant replay must load: {error}", case.name)
                    });
                    assert_eq!(
                        view.accepted_entry_ids(),
                        entry_ids(&expected["acceptedEntryIds"]),
                        "{}: accepted entry ids",
                        case.name
                    );
                    let expected_path = entry_ids(&expected["selectedPath"]);
                    assert_eq!(
                        view.selected_path(),
                        expected_path.as_slice(),
                        "{}: selected path",
                        case.name
                    );
                    assert_eq!(
                        view.selected_head(),
                        expected_path.last().copied(),
                        "{}: selected head",
                        case.name
                    );
                    assert_messages_match(
                        view.sanitized_messages(),
                        &expected["sanitizedModelMessages"],
                        case.name,
                    );
                    assert_eq!(
                        view.historical_item_ids(),
                        item_ids(&expected["historicalItemIds"]).as_slice(),
                        "{}: historical item ids",
                        case.name
                    );
                    let diagnostics = expected
                        .get("diagnostics")
                        .or_else(|| expected["diagnosticsByMode"].get("readOnly"))
                        .expect("expected diagnostics");
                    assert_diagnostic_counts(view.diagnostics(), diagnostics, case.name);
                    assert_tail_action(
                        view.tail_action(),
                        &expected["tail"]["readOnlyAction"],
                        None,
                        case.name,
                    );
                    assert_eq!(
                        view.current_turn(),
                        None,
                        "{}: cold state has no current turn",
                        case.name
                    );

                    let accepted = view.accepted_entry_ids();
                    assert_eq!(
                        view.reserved_ids(),
                        accepted.as_slice(),
                        "{}: reserved ids are exactly the first-valid accepted ids",
                        case.name
                    );

                    if case.name == "entry-session-mismatch" {
                        let assertions = &expected["identityAssertions"];
                        assert_eq!(
                            assertions["mismatchedLineReservesEntryId"], false,
                            "session mismatch must not reserve the EntryId"
                        );
                        assert_eq!(
                            assertions["laterMatchingSessionReuseAccepted"], true,
                            "the later matching line reuses the id first-valid"
                        );
                    }

                    // The exclusive-writable pass returns the writable tail action and keeps
                    // the same semantic projection and diagnostics.
                    let lease =
                        ExclusiveWritableConversationLease::for_scanner_test(session_id, length);
                    let writable_view = replay_conversation(
                        Cursor::new(case.bytes.to_vec()),
                        length,
                        session_id,
                        ConversationScanAccess::ExclusiveWritable(&lease),
                    )
                    .unwrap_or_else(|error| {
                        panic!("{}: writable replay must load: {error}", case.name)
                    });
                    assert_messages_match(
                        writable_view.sanitized_messages(),
                        &expected["sanitizedModelMessages"],
                        case.name,
                    );
                    let writable_diagnostics = expected
                        .get("diagnostics")
                        .or_else(|| expected["diagnosticsByMode"].get("exclusiveWritable"))
                        .expect("expected writable diagnostics");
                    assert_diagnostic_counts(
                        writable_view.diagnostics(),
                        writable_diagnostics,
                        case.name,
                    );
                    assert_tail_action(
                        writable_view.tail_action(),
                        &expected["tail"]["exclusiveWritableAction"],
                        expected["tail"]["truncateOffset"].as_u64(),
                        case.name,
                    );
                }
            }
        }
    }

    #[test]
    fn replay_projects_the_golden_complete_tool_exchange() {
        let session_id = fixture_session(TOOL_EXCHANGE_GOLDEN);
        let view =
            replay_fixture(TOOL_EXCHANGE_GOLDEN, session_id).expect("golden tool exchange replays");
        let messages = view.sanitized_messages();
        let roles = messages
            .iter()
            .map(|message| match message.as_ref() {
                ModelMessageRef::User { .. } => "user",
                ModelMessageRef::Assistant { .. } => "assistant",
                ModelMessageRef::Tool { .. } => "tool",
            })
            .collect::<Vec<_>>();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);

        match messages[1].as_ref() {
            ModelMessageRef::Assistant { content } => {
                assert_eq!(content.len(), 1);
                match content[0].as_ref() {
                    ModelAssistantContentRef::ToolCall {
                        tool_call_id,
                        name,
                        arguments,
                    } => {
                        assert_eq!(tool_call_id.as_str(), "call_weather_1");
                        assert_eq!(name.as_str(), "get_weather");
                        assert_eq!(
                            arguments.canonical_json(),
                            "{\"city\":\"Paris\",\"days\":1,\"units\":\"metric\"}"
                        );
                    }
                    other => panic!("expected a tool call block, got {other:?}"),
                }
            }
            _ => panic!("second message is the assistant tool call"),
        }

        match messages[2].as_ref() {
            ModelMessageRef::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id.as_str(), "call_weather_1");
                assert_eq!(content.parts().len(), 1);
                assert_eq!(content.parts()[0].as_text(), "18 C, clear");
            }
            _ => panic!("third message is the tool result"),
        }

        match messages[3].as_ref() {
            ModelMessageRef::Assistant { content } => {
                assert_eq!(content.len(), 2);
                match content[0].as_ref() {
                    ModelAssistantContentRef::Reasoning(value) => {
                        assert_eq!(value.summary(), Some("Checked weather tool output."));
                    }
                    other => panic!("expected reasoning block, got {other:?}"),
                }
                match content[1].as_ref() {
                    ModelAssistantContentRef::Text(text) => {
                        assert_eq!(text, "It is 18 C and clear in Paris.");
                    }
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            _ => panic!("fourth message is the final assistant"),
        }

        assert!(view.diagnostics().is_empty());
        assert_eq!(
            view.historical_item_ids(),
            [
                "itm_44444444444444444444444444444444"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_55555555555555555555555555555555"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_77777777777777777777777777777777"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_66666666666666666666666666666666"
                    .parse::<ItemId>()
                    .unwrap(),
            ]
        );
    }

    #[test]
    fn replay_seed_preserves_stable_units_relations_and_revision_for_live_state() {
        let session_id = fixture_session(TOOL_EXCHANGE_GOLDEN);
        let view =
            replay_fixture(TOOL_EXCHANGE_GOLDEN, session_id).expect("golden tool exchange replays");
        assert_eq!(view.revision().operation_count_for_test(), 4);
        assert_eq!(
            view.stable_units()
                .iter()
                .map(LiveCompactionUnit::kind)
                .collect::<Vec<_>>(),
            [
                CompactionUnitKind::UserMessage,
                CompactionUnitKind::ToolExchange,
                CompactionUnitKind::AssistantMessage,
            ]
        );

        let state = LiveSessionState::from_replayed_view(session_id, &view)
            .expect("the replay projection seeds live state without applying history");
        let captured = state
            .capture_conversation_views()
            .expect("the replay seed preserves a valid stable source");
        assert_eq!(captured.conversation().messages().len(), 4);
        assert_eq!(
            captured
                .conversation()
                .revision()
                .operation_count_for_test(),
            4
        );
        assert_eq!(captured.relations().len(), 4);
        assert!(captured.pending_interactions().is_empty());
        assert!(
            state.entry_id_is_reserved_for_test(
                "ent_00000000000000000000000000000004"
                    .parse()
                    .expect("the golden EntryId is valid")
            )
        );
    }

    #[test]
    fn replay_applies_the_golden_null_marker_compaction_as_a_rolling_summary() {
        let session_id = fixture_session(INTERACTION_COMPACTION_GOLDEN);
        let view = replay_fixture(INTERACTION_COMPACTION_GOLDEN, session_id)
            .expect("golden interaction compaction replays");
        let messages = view.sanitized_messages();
        assert_eq!(messages.len(), 1);
        match messages[0].as_ref() {
            ModelMessageRef::User { content } => {
                assert_eq!(content.len(), 1);
                assert_eq!(
                    content[0].as_text(),
                    "The user selected production deployment."
                );
            }
            _ => panic!("a valid null-marker compaction projects one rolling summary"),
        }
        assert!(view.diagnostics().is_empty());
        assert_eq!(
            view.historical_item_ids(),
            [
                "itm_10000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_10000000000000000000000000000002"
                    .parse::<ItemId>()
                    .unwrap(),
            ]
        );
    }

    #[test]
    fn replay_orders_golden_interaction_variants_by_assistant_calls() {
        let session_id = fixture_session(INTERACTION_VARIANTS_GOLDEN);
        let view = replay_fixture(INTERACTION_VARIANTS_GOLDEN, session_id)
            .expect("golden interaction variants replay");
        let messages = view.sanitized_messages();
        assert_eq!(messages.len(), 5, "user, assistant, and three tool results");
        let assistant_calls = match messages[1].as_ref() {
            ModelMessageRef::Assistant { content } => content
                .iter()
                .map(|block| match block.as_ref() {
                    ModelAssistantContentRef::ToolCall { tool_call_id, .. } => {
                        tool_call_id.as_str().to_owned()
                    }
                    other => panic!("expected tool call block, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            _ => panic!("second message is the assistant"),
        };
        assert_eq!(
            assistant_calls,
            ["call_allow", "call_deny", "call_cancel_question"]
        );
        let model_visible_tool_order = messages[2..]
            .iter()
            .map(|message| match message.as_ref() {
                ModelMessageRef::Tool { tool_call_id, .. } => tool_call_id.as_str().to_owned(),
                _ => panic!("messages after the assistant are tool results"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            model_visible_tool_order,
            ["call_allow", "call_deny", "call_cancel_question"],
            "complete exchange projects in assistant call order, not physical result order"
        );

        // The golden sidecar asserts the same ordering facts about the fixture's physical bytes.
        let sidecar: Value = serde_json::from_str(include_str!(
            "../docs/fixtures/wire-v1/conversation/golden/interaction-variants.expected.json"
        ))
        .expect("interaction variants sidecar is JSON");
        let sidecar = &sidecar["assertions"];
        let expected_physical_terminal_order = sidecar["toolOrdering"]["physicalTerminalOrder"]
            .as_array()
            .expect("physical terminal order array")
            .iter()
            .map(|value| value.as_str().expect("tool call id").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            expected_physical_terminal_order,
            ["call_cancel_question", "call_allow", "call_deny"].as_slice()
        );
        let expected_model_visible_order = sidecar["toolOrdering"]["modelVisibleOrder"]
            .as_array()
            .expect("model visible order array")
            .iter()
            .map(|value| value.as_str().expect("tool call id").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            expected_model_visible_order,
            model_visible_tool_order.as_slice()
        );

        let mut physical_entry_ids = Vec::new();
        let mut physical_terminal_order = Vec::new();
        for line in INTERACTION_VARIANTS_GOLDEN.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let ConversationRecord::Entry(entry) =
                ConversationLineCodec::decode_record(line).expect("golden line decodes")
            else {
                continue;
            };
            physical_entry_ids.push(entry.entry_id());
            if let StoredEntryBody::ToolMessage(message) = entry.body() {
                physical_terminal_order.push(message.tool_call_id().as_str().to_owned());
            }
        }
        assert_eq!(
            physical_terminal_order,
            ["call_cancel_question", "call_allow", "call_deny"],
            "physical terminal order"
        );
        let required_physical_order = sidecar["askUserExclusiveFirst"]["requiredPhysicalOrder"]
            .as_array()
            .expect("required physical order array")
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .expect("entry id")
                    .parse::<EntryId>()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut previous_position = None;
        for entry_id in &required_physical_order {
            let position = physical_entry_ids
                .iter()
                .position(|current| current == entry_id)
                .unwrap_or_else(|| panic!("required order entry {entry_id} is in the fixture"));
            assert!(
                previous_position.is_none_or(|previous| previous < position),
                "required physical order is strictly increasing"
            );
            previous_position = Some(position);
        }
    }

    fn synthetic_entry(
        entry_id: &str,
        parent: Option<&str>,
        body: StoredEntryBody,
    ) -> StoredSessionEntry {
        StoredSessionEntry::reconstruct(
            entry_id.parse().expect("synthetic Entry ID is valid"),
            parent.map(|parent| parent.parse().expect("synthetic parent ID is valid")),
            "ses_11111111111111111111111111111111"
                .parse()
                .expect("synthetic Session ID is valid"),
            "trn_33333333333333333333333333333333"
                .parse()
                .expect("synthetic Turn ID is valid"),
            "2026-07-31T12:00:01.000Z"
                .parse()
                .expect("synthetic timestamp is valid"),
            body,
        )
    }

    fn synthetic_user(
        entry_id: &str,
        parent: Option<&str>,
        item_id: &str,
        text: &str,
    ) -> StoredSessionEntry {
        synthetic_entry(
            entry_id,
            parent,
            StoredEntryBody::UserMessage(StoredUserMessage::reconstruct(
                item_id.parse().expect("synthetic Item ID is valid"),
                UserMessageSource::Input,
                CanonicalUserMessage::reconstruct(
                    MessageRecord::reconstruct(vec![
                        MessageContent::reconstruct_text(text)
                            .expect("synthetic user text is safe"),
                    ])
                    .expect("synthetic message record"),
                    Vec::new(),
                )
                .expect("synthetic canonical user message"),
            )),
        )
    }

    fn synthetic_compaction(
        entry_id: &str,
        parent: Option<&str>,
        summary: &str,
        marker: Option<&str>,
    ) -> StoredSessionEntry {
        synthetic_entry(
            entry_id,
            parent,
            StoredEntryBody::Compaction(
                StoredCompaction::reconstruct(
                    summary,
                    marker
                        .map(|marker| marker.parse().expect("synthetic marker Entry ID is valid")),
                    None,
                )
                .expect("synthetic stored compaction"),
            ),
        )
    }

    fn synthetic_assistant_with_tool_calls(
        entry_id: &str,
        parent: Option<&str>,
        calls: &[&str],
        item_ids: &[&str],
    ) -> StoredSessionEntry {
        assert_eq!(calls.len(), item_ids.len(), "one item per synthetic call");
        let content = calls
            .iter()
            .zip(item_ids)
            .map(|(call, item_id)| StoredAssistantContent::ToolCall {
                item_id: item_id.parse().expect("synthetic Item ID is valid"),
                tool_call_id: call.parse().expect("synthetic ToolCall ID is valid"),
                name: "fixture_tool".parse().expect("synthetic ToolName is valid"),
                arguments: BoundedJsonObject::from_slice(b"{}").expect("synthetic arguments parse"),
            })
            .collect();
        let message = StoredAssistantMessage::reconstruct(
            AssistantDisposition::Intermediate,
            content,
            model(),
            None,
            ModelFinishReason::ToolCalls,
            NonZeroU32::new(1).unwrap(),
            None,
            0,
            metadata(),
        )
        .expect("synthetic assistant tool-call message");
        synthetic_entry(entry_id, parent, StoredEntryBody::AssistantMessage(message))
    }

    fn synthetic_assistant_text(
        entry_id: &str,
        parent: Option<&str>,
        item_id: &str,
        text: &str,
    ) -> StoredSessionEntry {
        synthetic_entry(
            entry_id,
            parent,
            StoredEntryBody::AssistantMessage(assistant(vec![StoredAssistantContent::Text {
                item_id: item_id.parse().expect("synthetic Item ID is valid"),
                text: text.into(),
            }])),
        )
    }

    fn synthetic_tool(
        entry_id: &str,
        parent: Option<&str>,
        item_id: &str,
        call: &str,
        text: &str,
    ) -> StoredSessionEntry {
        synthetic_entry(
            entry_id,
            parent,
            StoredEntryBody::ToolMessage(StoredToolMessage::reconstruct(
                item_id.parse().expect("synthetic Item ID is valid"),
                call.parse().expect("synthetic ToolCall ID is valid"),
                StoredToolOutcome::completed(
                    ToolOutcomeSource::PreExecution,
                    ToolResultDisposition::Succeeded,
                    ToolResultContent::from_text_parts(vec![text.to_owned()])
                        .expect("synthetic tool text is safe"),
                )
                .expect("synthetic completed outcome"),
            )),
        )
    }

    fn synthetic_abandoned_tool(
        entry_id: &str,
        parent: Option<&str>,
        item_id: &str,
        call: &str,
    ) -> StoredSessionEntry {
        synthetic_entry(
            entry_id,
            parent,
            StoredEntryBody::ToolMessage(StoredToolMessage::reconstruct(
                item_id.parse().expect("synthetic Item ID is valid"),
                call.parse().expect("synthetic ToolCall ID is valid"),
                StoredToolOutcome::Abandoned {
                    reason: ToolAbandonReason::OutcomeUnknown,
                },
            )),
        )
    }

    fn synthetic_user_question_request() -> StoredInteractionRequestBody {
        StoredInteractionRequestBody::UserQuestion(
            UserQuestionRequest::reconstruct(
                Some("Choose target".to_owned()),
                vec![
                    UserQuestionField::reconstruct(
                        0,
                        "Where?",
                        true,
                        UserQuestionInput::Text { multiline: false },
                    )
                    .expect("synthetic question field"),
                ],
            )
            .expect("synthetic user question request"),
        )
    }

    fn synthetic_tool_approval_request() -> StoredInteractionRequestBody {
        let requirements = ToolRequirementSummaryView::reconstruct(None, None, None)
            .expect("synthetic requirements");
        let option = ToolApprovalOptionView::reconstruct(
            0,
            ToolApprovalOptionKindView::AsRequested,
            "Allow once",
            requirements.clone(),
        )
        .expect("synthetic option");
        StoredInteractionRequestBody::ToolApproval(
            ToolApprovalRequestView::reconstruct(
                "write_file".parse().expect("synthetic ToolName is valid"),
                "path: src/lib.rs",
                "write requested",
                requirements,
                vec![option],
            )
            .expect("synthetic tool approval request"),
        )
    }

    fn synthetic_interaction_request(
        entry_id: &str,
        parent: Option<&str>,
        request_id: &str,
        item_id: &str,
        request: StoredInteractionRequestBody,
    ) -> StoredSessionEntry {
        synthetic_entry(
            entry_id,
            parent,
            StoredEntryBody::InteractionRequested(StoredInteractionRequest::reconstruct(
                request_id.parse().expect("synthetic Request ID is valid"),
                item_id.parse().expect("synthetic Item ID is valid"),
                request,
            )),
        )
    }

    fn synthetic_interaction_resolution(
        entry_id: &str,
        parent: Option<&str>,
        request_id: &str,
        item_id: &str,
        resolution: StoredInteractionResolutionBody,
    ) -> StoredSessionEntry {
        let requires_key = !matches!(resolution, StoredInteractionResolutionBody::Cancelled(_))
            || matches!(
                resolution,
                StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::HostCancelled)
            );
        synthetic_entry(
            entry_id,
            parent,
            StoredEntryBody::InteractionResolved(
                StoredInteractionResolution::reconstruct(
                    request_id.parse().expect("synthetic Request ID is valid"),
                    item_id.parse().expect("synthetic Item ID is valid"),
                    resolution,
                    requires_key.then(|| {
                        "irk_11111111111111111111111111111111"
                            .parse()
                            .expect("synthetic resolution key is valid")
                    }),
                )
                .expect("synthetic stored interaction resolution"),
            ),
        )
    }

    fn synthetic_file(entries: &[StoredSessionEntry]) -> Vec<u8> {
        let mut bytes = HEADER_ONLY.to_vec();
        for entry in entries {
            let encoded =
                ConversationLineCodec::encode_entry(entry).expect("synthetic entry encodes");
            bytes.extend_from_slice(&encoded);
            bytes.push(b'\n');
        }
        bytes
    }

    fn replay_synthetic(bytes: &[u8]) -> ReplayedConversationView {
        replay_fixture(
            bytes,
            "ses_11111111111111111111111111111111"
                .parse()
                .expect("synthetic Session ID is valid"),
        )
        .expect("synthetic conversation replays")
    }

    fn synthetic_message_texts(view: &ReplayedConversationView) -> Vec<String> {
        view.sanitized_messages()
            .iter()
            .map(|message| match message.as_ref() {
                ModelMessageRef::User { content } => {
                    assert_eq!(content.len(), 1, "synthetic user messages have one part");
                    content[0].as_text().to_owned()
                }
                _ => panic!("synthetic projection only contains user messages"),
            })
            .collect()
    }

    #[test]
    fn replay_isolates_missing_and_ignored_compaction_markers() {
        // The marker references an entry id that never appears in the file.
        let missing = synthetic_file(&[
            synthetic_user(
                "ent_01000000000000000000000000000001",
                None,
                "itm_01000000000000000000000000000001",
                "keep me",
            ),
            synthetic_compaction(
                "ent_01000000000000000000000000000002",
                Some("ent_01000000000000000000000000000001"),
                "missing marker",
                Some("ent_99999999999999999999999999999999"),
            ),
        ]);
        let view = replay_synthetic(&missing);
        assert_eq!(synthetic_message_texts(&view), ["keep me"]);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidCompactionMarker),
            1
        );

        // The marker references an entry that is accepted later but isolated as an orphan.
        let ignored = synthetic_file(&[
            synthetic_user(
                "ent_02000000000000000000000000000001",
                None,
                "itm_02000000000000000000000000000001",
                "keep me too",
            ),
            synthetic_compaction(
                "ent_02000000000000000000000000000002",
                Some("ent_02000000000000000000000000000001"),
                "ignored marker",
                Some("ent_02000000000000000000000000000003"),
            ),
            synthetic_user(
                "ent_02000000000000000000000000000003",
                Some("ent_ffffffffffffffffffffffffffffffff"),
                "itm_02000000000000000000000000000003",
                "orphaned marker target",
            ),
        ]);
        let view = replay_synthetic(&ignored);
        assert_eq!(synthetic_message_texts(&view), ["keep me too"]);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::MissingParent),
            1
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidCompactionMarker),
            1
        );
        // The root and the compaction form the canonical path; the orphan target is isolated.
        assert_eq!(
            view.selected_path(),
            [
                "ent_02000000000000000000000000000001"
                    .parse::<EntryId>()
                    .unwrap(),
                "ent_02000000000000000000000000000002"
                    .parse::<EntryId>()
                    .unwrap(),
            ]
        );
    }

    #[test]
    fn replay_compaction_markers_use_prior_stable_unit_origins() {
        // ADR 0132: `first_kept_entry_id` is valid only when it exactly matches a current unit
        // origin at index > 0. Here the marker names the second user unit, so the units become
        // [new summary] + units[index..].
        let valid = synthetic_file(&[
            synthetic_user(
                "ent_03000000000000000000000000000001",
                None,
                "itm_03000000000000000000000000000001",
                "root text",
            ),
            synthetic_user(
                "ent_03000000000000000000000000000002",
                Some("ent_03000000000000000000000000000001"),
                "itm_03000000000000000000000000000002",
                "retained unit",
            ),
            synthetic_compaction(
                "ent_03000000000000000000000000000003",
                Some("ent_03000000000000000000000000000002"),
                "rolling summary",
                Some("ent_03000000000000000000000000000002"),
            ),
        ]);
        let view = replay_synthetic(&valid);
        assert_eq!(
            synthetic_message_texts(&view),
            ["rolling summary", "retained unit"],
            "a valid prior-unit marker replaces units[0..index] with the summary"
        );
        assert!(view.diagnostics().is_empty());

        // A marker pointing at the leading unit (index 0) is invalid and has no effect.
        let first = synthetic_file(&[
            synthetic_user(
                "ent_04000000000000000000000000000001",
                None,
                "itm_04000000000000000000000000000001",
                "root text",
            ),
            synthetic_compaction(
                "ent_04000000000000000000000000000002",
                Some("ent_04000000000000000000000000000001"),
                "rolling summary",
                Some("ent_04000000000000000000000000000001"),
            ),
        ]);
        let view = replay_synthetic(&first);
        assert_eq!(
            synthetic_message_texts(&view),
            ["root text"],
            "a marker naming the first unit is invalid and ignored"
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidCompactionMarker),
            1
        );

        // A marker naming an entry that appears later in the file (a future entry) cannot be a
        // prior unit origin: it is invalid and ignored.
        let future = synthetic_file(&[
            synthetic_user(
                "ent_05000000000000000000000000000001",
                None,
                "itm_05000000000000000000000000000001",
                "root text",
            ),
            synthetic_compaction(
                "ent_05000000000000000000000000000002",
                Some("ent_05000000000000000000000000000001"),
                "rolling summary",
                Some("ent_05000000000000000000000000000003"),
            ),
            synthetic_user(
                "ent_05000000000000000000000000000003",
                Some("ent_05000000000000000000000000000002"),
                "itm_05000000000000000000000000000003",
                "future target",
            ),
        ]);
        let view = replay_synthetic(&future);
        assert_eq!(
            synthetic_message_texts(&view),
            ["root text", "future target"],
            "a future marker is invalid and has no effect"
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidCompactionMarker),
            1
        );

        // A marker naming a unit already removed by an earlier compaction is invalid: only
        // current unit origins can be matched.
        let already_removed = synthetic_file(&[
            synthetic_user(
                "ent_06000000000000000000000000000001",
                None,
                "itm_06000000000000000000000000000001",
                "root text",
            ),
            synthetic_user(
                "ent_06000000000000000000000000000002",
                Some("ent_06000000000000000000000000000001"),
                "itm_06000000000000000000000000000002",
                "kept after first compaction",
            ),
            synthetic_compaction(
                "ent_06000000000000000000000000000003",
                Some("ent_06000000000000000000000000000002"),
                "first summary",
                Some("ent_06000000000000000000000000000002"),
            ),
            synthetic_compaction(
                "ent_06000000000000000000000000000004",
                Some("ent_06000000000000000000000000000003"),
                "second summary",
                Some("ent_06000000000000000000000000000001"),
            ),
        ]);
        let view = replay_synthetic(&already_removed);
        assert_eq!(
            synthetic_message_texts(&view),
            ["first summary", "kept after first compaction"],
            "a marker naming an already-removed unit is invalid and ignored"
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidCompactionMarker),
            1
        );

        // `None` is valid only when the effective units are non-empty.
        let empty = synthetic_file(&[synthetic_compaction(
            "ent_07000000000000000000000000000001",
            None,
            "summary over nothing",
            None,
        )]);
        let view = replay_synthetic(&empty);
        assert_eq!(
            synthetic_message_texts(&view),
            Vec::<String>::new(),
            "a null marker over empty units is invalid and has no effect"
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidCompactionMarker),
            1
        );

        // A marker naming a ToolResult entry is invalid: tool results are internal to the
        // assistant-origin exchange unit and are never a unit origin.
        let tool_internal = synthetic_file(&[
            synthetic_user(
                "ent_08000000000000000000000000000001",
                None,
                "itm_08000000000000000000000000000001",
                "root text",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_08000000000000000000000000000002",
                Some("ent_08000000000000000000000000000001"),
                &["call_internal"],
                &["itm_08000000000000000000000000000002"],
            ),
            synthetic_tool(
                "ent_08000000000000000000000000000003",
                Some("ent_08000000000000000000000000000002"),
                "itm_08000000000000000000000000000002",
                "call_internal",
                "done",
            ),
            synthetic_compaction(
                "ent_08000000000000000000000000000004",
                Some("ent_08000000000000000000000000000003"),
                "rolling summary",
                Some("ent_08000000000000000000000000000003"),
            ),
        ]);
        let view = replay_synthetic(&tool_internal);
        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool"],
            "a marker naming a ToolResult entry is invalid and ignored"
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidCompactionMarker),
            1
        );
    }

    fn synthetic_message_roles(view: &ReplayedConversationView) -> Vec<&'static str> {
        view.sanitized_messages()
            .iter()
            .map(|message| match message.as_ref() {
                ModelMessageRef::User { .. } => "user",
                ModelMessageRef::Assistant { .. } => "assistant",
                ModelMessageRef::Tool { .. } => "tool",
            })
            .collect()
    }

    #[test]
    fn replay_history_items_include_only_semantically_valid_item_ids() {
        // An orphan ToolMessage (no earlier accepted Assistant ToolCall matches its
        // (ItemId, ToolCallId) pair) contributes no historical item id.
        let orphan = synthetic_file(&[
            synthetic_user(
                "ent_0c000000000000000000000000000001",
                None,
                "itm_0c000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_0c000000000000000000000000000002",
                Some("ent_0c000000000000000000000000000001"),
                &["call_orphan"],
                &["itm_0c000000000000000000000000000002"],
            ),
            synthetic_tool(
                "ent_0c000000000000000000000000000003",
                Some("ent_0c000000000000000000000000000002"),
                "itm_0c000000000000000000000000000003",
                "call_unknown",
                "late",
            ),
        ]);
        let view = replay_synthetic(&orphan);
        assert_eq!(
            view.historical_item_ids(),
            [
                "itm_0c000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_0c000000000000000000000000000002"
                    .parse::<ItemId>()
                    .unwrap(),
            ],
            "orphan tool items never enter the historical index"
        );
        assert_eq!(synthetic_message_roles(&view), ["user"]);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            2,
            "the orphan result and the then-incomplete exchange each own one fact"
        );

        // An identity-conflicting result (matching ToolCallId, different ItemId) invalidates the
        // exchange, and its item never enters the historical index.
        let conflict = synthetic_file(&[
            synthetic_user(
                "ent_0d000000000000000000000000000001",
                None,
                "itm_0d000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_0d000000000000000000000000000002",
                Some("ent_0d000000000000000000000000000001"),
                &["call_conflict"],
                &["itm_0d000000000000000000000000000002"],
            ),
            synthetic_tool(
                "ent_0d000000000000000000000000000003",
                Some("ent_0d000000000000000000000000000002"),
                "itm_0d000000000000000000000000000003",
                "call_conflict",
                "wrong identity",
            ),
            synthetic_tool(
                "ent_0d000000000000000000000000000004",
                Some("ent_0d000000000000000000000000000003"),
                "itm_0d000000000000000000000000000002",
                "call_conflict",
                "late correct",
            ),
        ]);
        let view = replay_synthetic(&conflict);
        assert_eq!(
            view.historical_item_ids(),
            [
                "itm_0d000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_0d000000000000000000000000000002"
                    .parse::<ItemId>()
                    .unwrap(),
            ],
            "identity-conflicting tool items never enter the historical index"
        );
        assert_eq!(synthetic_message_roles(&view), ["user"]);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            2,
            "the identity conflict and the later orphan each own one fact"
        );

        // A duplicate item identity across User and Assistant content is isolated: the assistant
        // message yields no unit and contributes no item id.
        let duplicate = synthetic_file(&[
            synthetic_user(
                "ent_0e000000000000000000000000000001",
                None,
                "itm_0e000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_text(
                "ent_0e000000000000000000000000000002",
                Some("ent_0e000000000000000000000000000001"),
                "itm_0e000000000000000000000000000001",
                "duplicate identity",
            ),
        ]);
        let view = replay_synthetic(&duplicate);
        assert_eq!(
            view.historical_item_ids(),
            ["itm_0e000000000000000000000000000001"
                .parse::<ItemId>()
                .unwrap(),],
            "duplicate assistant content adds no historical item"
        );
        assert_eq!(synthetic_message_roles(&view), ["user"]);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidRelation),
            1
        );
    }

    #[test]
    fn replay_tool_exchanges_match_by_item_call_pair_and_first_terminal_wins() {
        // The first completed terminal wins; a later duplicate result is isolated with one
        // fact and the exchange stays model-visible.
        let duplicate_terminal = synthetic_file(&[
            synthetic_user(
                "ent_11000000000000000000000000000001",
                None,
                "itm_11000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_11000000000000000000000000000002",
                Some("ent_11000000000000000000000000000001"),
                &["call_1"],
                &["itm_11000000000000000000000000000002"],
            ),
            synthetic_tool(
                "ent_11000000000000000000000000000003",
                Some("ent_11000000000000000000000000000002"),
                "itm_11000000000000000000000000000002",
                "call_1",
                "ok",
            ),
            synthetic_tool(
                "ent_11000000000000000000000000000004",
                Some("ent_11000000000000000000000000000003"),
                "itm_11000000000000000000000000000002",
                "call_1",
                "duplicate",
            ),
        ]);
        let view = replay_synthetic(&duplicate_terminal);
        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool"]
        );
        let ModelMessageRef::Tool { content, .. } = view.sanitized_messages()[2].as_ref() else {
            panic!("third message is the first valid tool result")
        };
        assert_eq!(content.parts().len(), 1);
        assert_eq!(content.parts()[0].as_text(), "ok");
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            1,
            "the later duplicate terminal owns one fact"
        );
        assert_eq!(
            view.historical_item_ids(),
            [
                "itm_11000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_11000000000000000000000000000002"
                    .parse::<ItemId>()
                    .unwrap(),
            ]
        );

        // Abandoned-first excludes the whole exchange from the model conversation.
        let abandoned_first = synthetic_file(&[
            synthetic_user(
                "ent_12000000000000000000000000000001",
                None,
                "itm_12000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_12000000000000000000000000000002",
                Some("ent_12000000000000000000000000000001"),
                &["call_2"],
                &["itm_12000000000000000000000000000002"],
            ),
            synthetic_abandoned_tool(
                "ent_12000000000000000000000000000003",
                Some("ent_12000000000000000000000000000002"),
                "itm_12000000000000000000000000000002",
                "call_2",
            ),
        ]);
        let view = replay_synthetic(&abandoned_first);
        assert_eq!(synthetic_message_roles(&view), ["user"]);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            1,
            "abandoned-first owns one fact and excludes the exchange"
        );
    }

    #[test]
    fn replay_late_invalid_fact_for_a_closed_exchange_never_clears_the_next_exchange() {
        // Two selected-path exchanges: the first is left unfinished when the second assistant
        // opens (its boundary close owns one fact), and a late Abandoned-first fact for the
        // first exchange arrives while the second exchange is pending. The late fact is owned
        // by the semantic facts pass and must not clear the second assistant's pending
        // exchange: the owner-aware sanitizer clears only a pending exchange whose assistant
        // entry matches the fact owner. The second exchange still projects as one stable unit
        // with its own tool result, and the late fact emits exactly its one fact-owned
        // diagnostic (no sanitizer diagnostic at the fact location).
        let bytes = synthetic_file(&[
            synthetic_user(
                "ent_20000000000000000000000000000001",
                None,
                "itm_20000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_20000000000000000000000000000002",
                Some("ent_20000000000000000000000000000001"),
                &["call_first"],
                &["itm_20000000000000000000000000000002"],
            ),
            synthetic_assistant_with_tool_calls(
                "ent_20000000000000000000000000000003",
                Some("ent_20000000000000000000000000000002"),
                &["call_second"],
                &["itm_20000000000000000000000000000003"],
            ),
            // Late invalid fact for the already-boundary-closed first exchange, arriving
            // while the second exchange is pending.
            synthetic_abandoned_tool(
                "ent_20000000000000000000000000000004",
                Some("ent_20000000000000000000000000000003"),
                "itm_20000000000000000000000000000002",
                "call_first",
            ),
            synthetic_tool(
                "ent_20000000000000000000000000000005",
                Some("ent_20000000000000000000000000000004"),
                "itm_20000000000000000000000000000003",
                "call_second",
                "second done",
            ),
        ]);
        let view = replay_synthetic(&bytes);
        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool"],
            "the second exchange projects despite the late invalid fact for the first"
        );
        let ModelMessageRef::Tool { content, .. } = view.sanitized_messages()[2].as_ref() else {
            panic!("third message is the second exchange's tool result")
        };
        assert_eq!(content.parts()[0].as_text(), "second done");
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            2,
            "one boundary fact closing the first exchange and one fact-owned diagnostic for \
             the late abandoned fact"
        );
        let details = view.diagnostics().details();
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 4)
                .count(),
            1,
            "the second assistant's boundary close owns exactly one fact"
        );
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 5)
                .count(),
            1,
            "the late abandoned fact owns exactly its own fact and no sanitizer diagnostic"
        );
    }

    #[test]
    fn replay_late_valid_result_after_a_user_boundary_owns_one_diagnostic() {
        // A selected-path assistant exchange is closed by a valid User boundary; a matching
        // Completed result arriving afterwards is globally valid but is not model-visible, and
        // owns exactly one `invalid_tool_exchange` diagnostic at its own tool location.
        let bytes = synthetic_file(&[
            synthetic_user(
                "ent_21000000000000000000000000000001",
                None,
                "itm_21000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_21000000000000000000000000000002",
                Some("ent_21000000000000000000000000000001"),
                &["call_late"],
                &["itm_21000000000000000000000000000002"],
            ),
            synthetic_user(
                "ent_21000000000000000000000000000003",
                Some("ent_21000000000000000000000000000002"),
                "itm_21000000000000000000000000000003",
                "boundary",
            ),
            synthetic_tool(
                "ent_21000000000000000000000000000004",
                Some("ent_21000000000000000000000000000003"),
                "itm_21000000000000000000000000000002",
                "call_late",
                "late result",
            ),
        ]);
        let view = replay_synthetic(&bytes);
        assert_eq!(
            synthetic_message_texts(&view),
            ["root", "boundary"],
            "the late result is not model-visible"
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            2,
            "one boundary fact closing the unfinished exchange and one fact for the late \
             result"
        );
        let details = view.diagnostics().details();
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 4)
                .count(),
            1,
            "the user boundary close owns exactly one fact"
        );
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 5)
                .count(),
            1,
            "the late result owns exactly one diagnostic at its own location"
        );
    }

    #[test]
    fn replay_invalid_user_and_assistant_entries_do_not_close_a_pending_exchange() {
        // Invalid selected-path User/Assistant projections are isolated facts, not legal
        // exchange boundaries. A duplicate User item and an Assistant whose only item is the
        // same duplicate must not discard the earlier pending exchange before its valid Tool
        // result arrives.
        let bytes = synthetic_file(&[
            synthetic_user(
                "ent_25000000000000000000000000000001",
                None,
                "itm_25000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_25000000000000000000000000000002",
                Some("ent_25000000000000000000000000000001"),
                &["call_kept"],
                &["itm_25000000000000000000000000000002"],
            ),
            synthetic_user(
                "ent_25000000000000000000000000000003",
                Some("ent_25000000000000000000000000000002"),
                "itm_25000000000000000000000000000001",
                "duplicate user",
            ),
            synthetic_assistant_text(
                "ent_25000000000000000000000000000004",
                Some("ent_25000000000000000000000000000003"),
                "itm_25000000000000000000000000000001",
                "duplicate assistant",
            ),
            synthetic_tool(
                "ent_25000000000000000000000000000005",
                Some("ent_25000000000000000000000000000004"),
                "itm_25000000000000000000000000000002",
                "call_kept",
                "kept result",
            ),
        ]);
        let view = replay_synthetic(&bytes);

        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool"],
            "both invalid projections are skipped without closing the valid pending exchange"
        );
        let ModelMessageRef::Tool { content, .. } = view.sanitized_messages()[2].as_ref() else {
            panic!("third message is the retained tool result")
        };
        assert_eq!(content.parts()[0].as_text(), "kept result");
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidRelation),
            2,
            "the duplicate User and duplicate Assistant each own their relation fact"
        );
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            0,
            "invalid projections are not exchange-closing boundaries"
        );
    }

    #[test]
    fn replay_reused_tool_call_id_never_invalidates_a_legal_response_local_owner() {
        // Response-local ToolCallId reuse: the same ToolCallId is issued by two different
        // assistants with different item ids. The first exchange is invalidated by its own
        // abandoned-first fact; a late exact-pair result for the first exchange then arrives.
        // The late result is a fact for a dead exchange and must be isolated as an orphan
        // without falling through to ToolCallId conflict matching: it must not invalidate the
        // second assistant's exchange, which legally reused the ToolCallId in another
        // response. The second exchange stays model-visible with its own completed result.
        let bytes = synthetic_file(&[
            synthetic_user(
                "ent_22000000000000000000000000000001",
                None,
                "itm_22000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_22000000000000000000000000000002",
                Some("ent_22000000000000000000000000000001"),
                &["call_reused"],
                &["itm_22000000000000000000000000000002"],
            ),
            synthetic_abandoned_tool(
                "ent_22000000000000000000000000000003",
                Some("ent_22000000000000000000000000000002"),
                "itm_22000000000000000000000000000002",
                "call_reused",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_22000000000000000000000000000004",
                Some("ent_22000000000000000000000000000003"),
                &["call_reused"],
                &["itm_22000000000000000000000000000003"],
            ),
            // Late exact-pair result for the invalidated first exchange, arriving while the
            // second exchange is pending.
            synthetic_tool(
                "ent_22000000000000000000000000000005",
                Some("ent_22000000000000000000000000000004"),
                "itm_22000000000000000000000000000002",
                "call_reused",
                "late first",
            ),
            synthetic_tool(
                "ent_22000000000000000000000000000006",
                Some("ent_22000000000000000000000000000005"),
                "itm_22000000000000000000000000000003",
                "call_reused",
                "second done",
            ),
        ]);
        let view = replay_synthetic(&bytes);
        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool"],
            "the legal response-local owner stays model-visible; the late first result never \
             leaks"
        );
        let ModelMessageRef::Tool { content, .. } = view.sanitized_messages()[2].as_ref() else {
            panic!("third message is the second exchange's tool result")
        };
        assert_eq!(content.parts()[0].as_text(), "second done");
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            2,
            "the abandoned-first fact and the late orphan exact-pair result each own one fact; \
             no collateral invalidation of the second exchange"
        );
        let details = view.diagnostics().details();
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 4)
                .count(),
            1,
            "the abandoned-first fact owns exactly one diagnostic"
        );
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 6)
                .count(),
            1,
            "the late exact-pair result owns exactly one orphan diagnostic"
        );
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 5)
                .count(),
            0,
            "the second assistant opening owns no diagnostic: the first exchange was already \
             invalidated, so nothing closes at this boundary"
        );
    }

    #[test]
    fn replay_completed_call_id_reuse_keeps_the_pending_response_local_owner() {
        // ToolCallId is response-local: assistant A's call_reused completes with a valid
        // terminal, then assistant B legally reuses the same call_reused in a later response
        // and is still pending. A malformed ToolMessage whose ItemId is never accepted
        // anywhere (so no exact pair matches) plus call_reused is ambiguous between A and B:
        // the live-call index retains A's terminal call, so the conflict has two live
        // candidates and B's exchange is never invalidated. B's complete exchange stays
        // model-visible, and only the malformed result owns one `invalid_tool_exchange`
        // diagnostic.
        let bytes = synthetic_file(&[
            synthetic_user(
                "ent_23000000000000000000000000000001",
                None,
                "itm_23000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_23000000000000000000000000000002",
                Some("ent_23000000000000000000000000000001"),
                &["call_reused"],
                &["itm_23000000000000000000000000000002"],
            ),
            synthetic_tool(
                "ent_23000000000000000000000000000003",
                Some("ent_23000000000000000000000000000002"),
                "itm_23000000000000000000000000000002",
                "call_reused",
                "first done",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_23000000000000000000000000000004",
                Some("ent_23000000000000000000000000000003"),
                &["call_reused"],
                &["itm_23000000000000000000000000000003"],
            ),
            // Malformed result before B's correct result: the ItemId is never accepted, so
            // no exact pair matches; the ToolCallId is ambiguous between A's terminal call
            // and B's pending call.
            synthetic_tool(
                "ent_23000000000000000000000000000005",
                Some("ent_23000000000000000000000000000004"),
                "itm_99999999999999999999999999999999",
                "call_reused",
                "malformed",
            ),
            synthetic_tool(
                "ent_23000000000000000000000000000006",
                Some("ent_23000000000000000000000000000005"),
                "itm_23000000000000000000000000000003",
                "call_reused",
                "second done",
            ),
        ]);
        let view = replay_synthetic(&bytes);
        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool", "assistant", "tool"],
            "both exchanges are model-visible: A's completed exchange and B's completed \
             exchange"
        );
        let ModelMessageRef::Tool { content, .. } = view.sanitized_messages()[4].as_ref() else {
            panic!("fifth message is B's tool result")
        };
        assert_eq!(content.parts()[0].as_text(), "second done");
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            1,
            "only the malformed result owns one fact; B's pending exchange is never \
             invalidated by an ambiguous response-local ToolCallId"
        );
        let details = view.diagnostics().details();
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 6)
                .count(),
            1,
            "the malformed result owns exactly one diagnostic at its own location"
        );
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 5)
                .count(),
            0,
            "B's assistant opening owns no diagnostic: A's exchange already completed, so \
             nothing closes at this boundary"
        );
    }

    #[test]
    fn replay_ambiguous_result_against_a_terminal_call_retains_the_valid_exchange() {
        // A malformed ToolMessage with a never-accepted ItemId plus a ToolCallId whose only
        // live candidate already has a valid terminal: the fact is diagnosed as an identity
        // conflict owned by that candidate, but the exchange is retained. The owner's pending
        // exchange (still waiting for its second call's result) is not cleared: only a fact
        // whose owner was actually invalidated by the semantic facts pass clears a matching
        // pending exchange.
        let bytes = synthetic_file(&[
            synthetic_user(
                "ent_24000000000000000000000000000001",
                None,
                "itm_24000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_24000000000000000000000000000002",
                Some("ent_24000000000000000000000000000001"),
                &["call_first", "call_second"],
                &[
                    "itm_24000000000000000000000000000002",
                    "itm_24000000000000000000000000000003",
                ],
            ),
            synthetic_tool(
                "ent_24000000000000000000000000000003",
                Some("ent_24000000000000000000000000000002"),
                "itm_24000000000000000000000000000002",
                "call_first",
                "first done",
            ),
            // Malformed result: only the terminal call_first is a live candidate for this
            // ToolCallId, so it owns the fact, but its terminal is retained and the exchange
            // is not invalidated.
            synthetic_tool(
                "ent_24000000000000000000000000000004",
                Some("ent_24000000000000000000000000000003"),
                "itm_99999999999999999999999999999999",
                "call_first",
                "malformed",
            ),
            synthetic_tool(
                "ent_24000000000000000000000000000005",
                Some("ent_24000000000000000000000000000004"),
                "itm_24000000000000000000000000000003",
                "call_second",
                "second done",
            ),
        ]);
        let view = replay_synthetic(&bytes);
        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool", "tool"],
            "the exchange is retained and both tool results stay model-visible"
        );
        let ModelMessageRef::Tool { content, .. } = view.sanitized_messages()[3].as_ref() else {
            panic!("fourth message is the second tool result")
        };
        assert_eq!(content.parts()[0].as_text(), "second done");
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidToolExchange),
            1,
            "only the malformed result owns one fact; the terminal candidate's exchange is \
             retained"
        );
        let details = view.diagnostics().details();
        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.line_number() == 5)
                .count(),
            1,
            "the malformed result owns exactly one diagnostic at its own location"
        );
    }

    #[test]
    fn replay_isolates_off_path_branch_tool_exchanges_from_the_selected_projection() {
        // ADR 0124/0132 branch isolation: an off-path branch assistant/tool exchange physically
        // interleaved before the selected leaf must never close, complete, or replace the
        // selected-path pending exchange, and an off-path incomplete exchange must never close
        // it at a boundary. The selected assistant's exchange still projects as one stable unit
        // in assistant call order, and a compaction marker naming its assistant origin still
        // resolves against the current stable units.
        let bytes = synthetic_file(&[
            synthetic_user(
                "ent_1b000000000000000000000000000001",
                None,
                "itm_1b000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_1b000000000000000000000000000002",
                Some("ent_1b000000000000000000000000000001"),
                &["call_selected"],
                &["itm_1b000000000000000000000000000002"],
            ),
            // Off-path branch: assistant + completed tool exchange, physically before the
            // selected tool result.
            synthetic_assistant_with_tool_calls(
                "ent_1b000000000000000000000000000003",
                Some("ent_1b000000000000000000000000000001"),
                &["call_branch"],
                &["itm_1b000000000000000000000000000003"],
            ),
            synthetic_tool(
                "ent_1b000000000000000000000000000004",
                Some("ent_1b000000000000000000000000000003"),
                "itm_1b000000000000000000000000000003",
                "call_branch",
                "branch done",
            ),
            synthetic_tool(
                "ent_1b000000000000000000000000000005",
                Some("ent_1b000000000000000000000000000002"),
                "itm_1b000000000000000000000000000002",
                "call_selected",
                "selected done",
            ),
            // Off-path incomplete exchange: its boundary/EOF close must not touch the selected
            // exchange.
            synthetic_assistant_with_tool_calls(
                "ent_1b000000000000000000000000000006",
                Some("ent_1b000000000000000000000000000001"),
                &["call_incomplete"],
                &["itm_1b000000000000000000000000000006"],
            ),
            synthetic_user(
                "ent_1b000000000000000000000000000007",
                Some("ent_1b000000000000000000000000000005"),
                "itm_1b000000000000000000000000000007",
                "selected leaf",
            ),
            // The marker names the selected exchange's assistant origin: valid only when the
            // exchange is a current stable unit at index > 0.
            synthetic_compaction(
                "ent_1b000000000000000000000000000008",
                Some("ent_1b000000000000000000000000000007"),
                "rolling summary",
                Some("ent_1b000000000000000000000000000002"),
            ),
            synthetic_assistant_text(
                "ent_1b000000000000000000000000000009",
                Some("ent_1b000000000000000000000000000008"),
                "itm_1b000000000000000000000000000009",
                "final answer",
            ),
        ]);
        let view = replay_synthetic(&bytes);

        assert_eq!(
            view.selected_path(),
            [
                "ent_1b000000000000000000000000000001"
                    .parse::<EntryId>()
                    .unwrap(),
                "ent_1b000000000000000000000000000002"
                    .parse::<EntryId>()
                    .unwrap(),
                "ent_1b000000000000000000000000000005"
                    .parse::<EntryId>()
                    .unwrap(),
                "ent_1b000000000000000000000000000007"
                    .parse::<EntryId>()
                    .unwrap(),
                "ent_1b000000000000000000000000000008"
                    .parse::<EntryId>()
                    .unwrap(),
                "ent_1b000000000000000000000000000009"
                    .parse::<EntryId>()
                    .unwrap(),
            ],
            "the off-path branch entries never enter the selected path"
        );

        // The selected exchange survives the interleaved branch: it projects as one stable unit
        // (assistant call in call order, then its tool result), and the compaction marker
        // resolves only because that exchange is still a current stable unit.
        assert_eq!(
            synthetic_message_roles(&view),
            ["user", "assistant", "tool", "user", "assistant"],
            "the branch exchange never leaks into the selected projection"
        );
        let messages = view.sanitized_messages();
        match messages[0].as_ref() {
            ModelMessageRef::User { content } => {
                assert_eq!(content[0].as_text(), "rolling summary");
            }
            _ => panic!("first message is the applied compaction summary"),
        }
        match messages[1].as_ref() {
            ModelMessageRef::Assistant { content } => {
                assert_eq!(content.len(), 1);
                match content[0].as_ref() {
                    ModelAssistantContentRef::ToolCall { tool_call_id, .. } => {
                        assert_eq!(tool_call_id.as_str(), "call_selected");
                    }
                    other => panic!("selected assistant projects its tool call, got {other:?}"),
                }
            }
            _ => panic!("second message is the selected assistant exchange"),
        }
        match messages[2].as_ref() {
            ModelMessageRef::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id.as_str(), "call_selected");
                assert_eq!(content.parts().len(), 1);
                assert_eq!(content.parts()[0].as_text(), "selected done");
            }
            _ => panic!("third message is the selected tool result"),
        }
        match messages[3].as_ref() {
            ModelMessageRef::User { content } => {
                assert_eq!(content[0].as_text(), "selected leaf");
            }
            _ => panic!("fourth message is the selected leaf user"),
        }
        match messages[4].as_ref() {
            ModelMessageRef::Assistant { content } => match content[0].as_ref() {
                ModelAssistantContentRef::Text(text) => {
                    assert_eq!(text, "final answer");
                }
                other => panic!("final assistant is plain text, got {other:?}"),
            },
            _ => panic!("fifth message is the final selected assistant"),
        }

        assert!(
            view.diagnostics().is_empty(),
            "the complete branch exchange and the off-path incomplete exchange never leak \
             diagnostics into the selected projection"
        );
        assert_eq!(
            view.historical_item_ids(),
            [
                "itm_1b000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_1b000000000000000000000000000002"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_1b000000000000000000000000000003"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_1b000000000000000000000000000006"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_1b000000000000000000000000000007"
                    .parse::<ItemId>()
                    .unwrap(),
                "itm_1b000000000000000000000000000009"
                    .parse::<ItemId>()
                    .unwrap(),
            ],
            "all-history semantic facts still cover every accepted entry, including off-path \
             items; tool result items never enter the historical index"
        );
    }

    #[test]
    fn replay_interaction_relations_validate_requests_resolutions_and_terminal_wins() {
        // A valid request must reference an accepted ToolCall item and have a unique RequestId;
        // a valid resolution must reference the earlier request with matching item/family and
        // pass the owner validator.
        let valid = synthetic_file(&[
            synthetic_user(
                "ent_13000000000000000000000000000001",
                None,
                "itm_13000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_13000000000000000000000000000002",
                Some("ent_13000000000000000000000000000001"),
                &["call_q"],
                &["itm_13000000000000000000000000000002"],
            ),
            synthetic_interaction_request(
                "ent_13000000000000000000000000000003",
                Some("ent_13000000000000000000000000000002"),
                "req_13000000000000000000000000000001",
                "itm_13000000000000000000000000000002",
                synthetic_user_question_request(),
            ),
            synthetic_interaction_resolution(
                "ent_13000000000000000000000000000004",
                Some("ent_13000000000000000000000000000003"),
                "req_13000000000000000000000000000001",
                "itm_13000000000000000000000000000002",
                StoredInteractionResolutionBody::UserAnswer(
                    UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "yes").unwrap()])
                        .expect("synthetic answer"),
                ),
            ),
        ]);
        let view = replay_synthetic(&valid);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            0,
            "the golden request/resolution relation is valid"
        );

        // A resolution without any earlier request is orphaned.
        let orphan_resolution = synthetic_file(&[
            synthetic_user(
                "ent_14000000000000000000000000000001",
                None,
                "itm_14000000000000000000000000000001",
                "root",
            ),
            synthetic_interaction_resolution(
                "ent_14000000000000000000000000000002",
                Some("ent_14000000000000000000000000000001"),
                "req_14000000000000000000000000000001",
                "itm_14000000000000000000000000000001",
                StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::TurnCancelled),
            ),
        ]);
        let view = replay_synthetic(&orphan_resolution);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            1
        );

        // A request referencing a User item (not an accepted ToolCall) is invalid.
        let not_tool_call = synthetic_file(&[
            synthetic_user(
                "ent_15000000000000000000000000000001",
                None,
                "itm_15000000000000000000000000000001",
                "root",
            ),
            synthetic_interaction_request(
                "ent_15000000000000000000000000000002",
                Some("ent_15000000000000000000000000000001"),
                "req_15000000000000000000000000000001",
                "itm_15000000000000000000000000000001",
                synthetic_user_question_request(),
            ),
        ]);
        let view = replay_synthetic(&not_tool_call);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            1
        );

        // A duplicate RequestId is invalid: first valid wins.
        let duplicate_request = synthetic_file(&[
            synthetic_user(
                "ent_16000000000000000000000000000001",
                None,
                "itm_16000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_16000000000000000000000000000002",
                Some("ent_16000000000000000000000000000001"),
                &["call_a"],
                &["itm_16000000000000000000000000000002"],
            ),
            synthetic_interaction_request(
                "ent_16000000000000000000000000000003",
                Some("ent_16000000000000000000000000000002"),
                "req_16000000000000000000000000000001",
                "itm_16000000000000000000000000000002",
                synthetic_user_question_request(),
            ),
            synthetic_interaction_request(
                "ent_16000000000000000000000000000004",
                Some("ent_16000000000000000000000000000003"),
                "req_16000000000000000000000000000001",
                "itm_16000000000000000000000000000002",
                synthetic_user_question_request(),
            ),
        ]);
        let view = replay_synthetic(&duplicate_request);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            1
        );

        // A resolution with the right RequestId but the wrong item is invalid.
        let wrong_item = synthetic_file(&[
            synthetic_user(
                "ent_17000000000000000000000000000001",
                None,
                "itm_17000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_17000000000000000000000000000002",
                Some("ent_17000000000000000000000000000001"),
                &["call_b"],
                &["itm_17000000000000000000000000000002"],
            ),
            synthetic_interaction_request(
                "ent_17000000000000000000000000000003",
                Some("ent_17000000000000000000000000000002"),
                "req_17000000000000000000000000000001",
                "itm_17000000000000000000000000000002",
                synthetic_user_question_request(),
            ),
            synthetic_interaction_resolution(
                "ent_17000000000000000000000000000004",
                Some("ent_17000000000000000000000000000003"),
                "req_17000000000000000000000000000001",
                "itm_17000000000000000000000000000099",
                StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::TurnCancelled),
            ),
        ]);
        let view = replay_synthetic(&wrong_item);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            1
        );

        // A family mismatch (UserQuestion request answered by a ToolApproval resolution) is
        // invalid even though both values are owner-valid in isolation.
        let family_mismatch = synthetic_file(&[
            synthetic_user(
                "ent_18000000000000000000000000000001",
                None,
                "itm_18000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_18000000000000000000000000000002",
                Some("ent_18000000000000000000000000000001"),
                &["call_c"],
                &["itm_18000000000000000000000000000002"],
            ),
            synthetic_interaction_request(
                "ent_18000000000000000000000000000003",
                Some("ent_18000000000000000000000000000002"),
                "req_18000000000000000000000000000001",
                "itm_18000000000000000000000000000002",
                synthetic_user_question_request(),
            ),
            synthetic_interaction_resolution(
                "ent_18000000000000000000000000000004",
                Some("ent_18000000000000000000000000000003"),
                "req_18000000000000000000000000000001",
                "itm_18000000000000000000000000000002",
                StoredInteractionResolutionBody::ToolApproval(
                    ToolApprovalResolution::reconstruct_allowed(
                        0,
                        ToolApprovalOptionKindView::AsRequested,
                    ),
                ),
            ),
        ]);
        let view = replay_synthetic(&family_mismatch);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            1
        );

        // A UserAnswer that fails the UserQuestion validator (unknown question index) is
        // invalid even though the RequestId/item/family all match.
        let invalid_answer = synthetic_file(&[
            synthetic_user(
                "ent_19000000000000000000000000000001",
                None,
                "itm_19000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_19000000000000000000000000000002",
                Some("ent_19000000000000000000000000000001"),
                &["call_d"],
                &["itm_19000000000000000000000000000002"],
            ),
            synthetic_interaction_request(
                "ent_19000000000000000000000000000003",
                Some("ent_19000000000000000000000000000002"),
                "req_19000000000000000000000000000001",
                "itm_19000000000000000000000000000002",
                synthetic_user_question_request(),
            ),
            synthetic_interaction_resolution(
                "ent_19000000000000000000000000000004",
                Some("ent_19000000000000000000000000000003"),
                "req_19000000000000000000000000000001",
                "itm_19000000000000000000000000000002",
                StoredInteractionResolutionBody::UserAnswer(
                    UserQuestionAnswer::new(vec![
                        UserQuestionFieldAnswer::text(7, "nope").unwrap(),
                    ])
                    .expect("synthetic out-of-range answer"),
                ),
            ),
        ]);
        let view = replay_synthetic(&invalid_answer);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            1
        );

        // First terminal resolution wins: a second resolution for the same request is isolated
        // even when it is owner-valid on its own.
        let second_resolution = synthetic_file(&[
            synthetic_user(
                "ent_1a000000000000000000000000000001",
                None,
                "itm_1a000000000000000000000000000001",
                "root",
            ),
            synthetic_assistant_with_tool_calls(
                "ent_1a000000000000000000000000000002",
                Some("ent_1a000000000000000000000000000001"),
                &["call_e"],
                &["itm_1a000000000000000000000000000002"],
            ),
            synthetic_interaction_request(
                "ent_1a000000000000000000000000000003",
                Some("ent_1a000000000000000000000000000002"),
                "req_1a000000000000000000000000000001",
                "itm_1a000000000000000000000000000002",
                synthetic_tool_approval_request(),
            ),
            synthetic_interaction_resolution(
                "ent_1a000000000000000000000000000004",
                Some("ent_1a000000000000000000000000000003"),
                "req_1a000000000000000000000000000001",
                "itm_1a000000000000000000000000000002",
                StoredInteractionResolutionBody::ToolApproval(
                    ToolApprovalResolution::reconstruct_allowed(
                        0,
                        ToolApprovalOptionKindView::AsRequested,
                    ),
                ),
            ),
            synthetic_interaction_resolution(
                "ent_1a000000000000000000000000000005",
                Some("ent_1a000000000000000000000000000004"),
                "req_1a000000000000000000000000000001",
                "itm_1a000000000000000000000000000002",
                StoredInteractionResolutionBody::ToolApproval(
                    ToolApprovalResolution::reconstruct_allowed(
                        0,
                        ToolApprovalOptionKindView::AsRequested,
                    ),
                ),
            ),
        ]);
        let view = replay_synthetic(&second_resolution);
        assert_eq!(
            view.diagnostics()
                .count(ConversationReplayDiagnosticCode::InvalidInteractionRelation),
            1,
            "first terminal resolution wins; the later one is isolated"
        );
    }

    #[test]
    fn replay_diagnostics_keep_physical_order_across_scan_and_semantic_passes() {
        // The semantic pass runs after the physical scan, so its invalid_relation fact at line 3
        // must still land ahead of the 150 later malformed-line scan facts in the retained
        // details, and the truncation totals cover every observed source fact.
        let mut bytes = synthetic_file(&[
            synthetic_user(
                "ent_0f000000000000000000000000000001",
                None,
                "itm_0f000000000000000000000000000001",
                "root",
            ),
            synthetic_user(
                "ent_0f000000000000000000000000000002",
                Some("ent_0f000000000000000000000000000001"),
                "itm_0f000000000000000000000000000001",
                "duplicate item",
            ),
        ]);
        for _ in 0..150 {
            bytes.extend_from_slice(b"not json\n");
        }
        let view = replay_synthetic(&bytes);
        let diagnostics = view.diagnostics();
        assert_eq!(diagnostics.details().len(), 100);
        assert_eq!(diagnostics.details()[0].line_number(), 3);
        assert_eq!(
            diagnostics.details()[0].code(),
            ConversationReplayDiagnosticCode::InvalidRelation
        );
        assert_eq!(diagnostics.details()[1].line_number(), 4);
        assert_eq!(
            diagnostics.details()[1].code(),
            ConversationReplayDiagnosticCode::MalformedJson
        );
        assert_eq!(
            diagnostics.count(ConversationReplayDiagnosticCode::MalformedJson),
            150
        );
        assert_eq!(
            diagnostics.count(ConversationReplayDiagnosticCode::InvalidRelation),
            1
        );
        assert_eq!(
            diagnostics.count(ConversationReplayDiagnosticCode::DiagnosticsTruncated),
            1
        );
        let truncation = diagnostics
            .truncation()
            .expect("omitted details produce one truncation summary");
        assert_eq!(
            truncation.omitted_details(),
            51,
            "151 facts minus 100 retained"
        );
        assert_eq!(
            truncation
                .totals()
                .get(&ConversationReplayDiagnosticCode::MalformedJson),
            Some(&150)
        );
        assert_eq!(
            truncation
                .totals()
                .get(&ConversationReplayDiagnosticCode::InvalidRelation),
            Some(&1)
        );
        assert_eq!(
            truncation.totals().len(),
            2,
            "the totals carry every observed source fact and never diagnostics_truncated"
        );
    }

    #[test]
    fn replay_diagnostics_retain_one_hundred_details_then_truncate() {
        let mut bytes = HEADER_ONLY.to_vec();
        for _ in 0..150 {
            bytes.extend_from_slice(b"not json\n");
        }
        let view = replay_synthetic(&bytes);
        let diagnostics = view.diagnostics();
        assert_eq!(
            diagnostics.count(ConversationReplayDiagnosticCode::MalformedJson),
            150,
            "every malformed line is counted in the aggregate"
        );
        assert_eq!(
            diagnostics.count(ConversationReplayDiagnosticCode::DiagnosticsTruncated),
            1,
            "exactly one truncation summary"
        );
        assert_eq!(
            diagnostics.details().len(),
            100,
            "first 100 details retained"
        );
        assert_eq!(
            diagnostics.details()[0].line_number(),
            2,
            "the first retained detail is the first physical fault"
        );
        assert_eq!(
            diagnostics.details()[0].code(),
            ConversationReplayDiagnosticCode::MalformedJson
        );
        let truncation = diagnostics
            .truncation()
            .expect("omitted details produce one truncation summary");
        assert_eq!(truncation.omitted_details(), 50);
        assert_eq!(
            truncation
                .totals()
                .get(&ConversationReplayDiagnosticCode::MalformedJson),
            Some(&150),
            "the truncation summary carries the final aggregate totals"
        );
        assert_eq!(truncation.totals().len(), 1);
    }

    #[test]
    fn replay_cold_view_diagnostics_and_errors_are_redacted() {
        let bytes = synthetic_file(&[synthetic_user(
            "ent_0b000000000000000000000000000001",
            None,
            "itm_0b000000000000000000000000000001",
            "secret conversation text /private/path",
        )]);
        let view = replay_synthetic(&bytes);
        let rendered = format!("{view:?} {:?} {}", view.diagnostics(), view.diagnostics());
        for secret in [
            "ses_11111111111111111111111111111111",
            "ent_0b000000000000000000000000000001",
            "itm_0b000000000000000000000000000001",
            "trn_33333333333333333333333333333333",
            "secret conversation text",
            "/private/path",
        ] {
            assert!(
                !rendered.contains(secret),
                "replay Debug/Display leaked {secret}"
            );
        }

        let missing_header = replay_fixture(
            b"secret conversation text /private/path\n",
            "ses_11111111111111111111111111111111"
                .parse()
                .expect("session id is valid"),
        );
        let error = missing_header.expect_err("malformed header line must fail replay");
        let rendered_error = format!("{error:?} {error}");
        for secret in ["secret conversation text", "/private/path"] {
            assert!(
                !rendered_error.contains(secret),
                "replay error leaked {secret}"
            );
        }

        let too_large = replay_conversation(
            Cursor::new(Vec::new()),
            MAX_CONVERSATION_FILE_BYTES + 1,
            "ses_11111111111111111111111111111111"
                .parse()
                .expect("session id is valid"),
            ConversationScanAccess::ReadOnly,
        );
        assert_eq!(
            too_large.unwrap_err(),
            ConversationReplayError::HistoryTooLarge,
            "the physical file cap maps to the wire HistoryTooLarge outcome"
        );
    }

    #[test]
    fn replay_maps_scanner_failures_to_typed_redacted_errors() {
        for (scanner_error, expected) in [
            (
                ConversationScanError::FileTooLarge,
                ConversationReplayError::HistoryTooLarge,
            ),
            (
                ConversationScanError::HistoryTooLarge,
                ConversationReplayError::HistoryTooLarge,
            ),
            (
                ConversationScanError::HeaderCorrupt {
                    code: ConversationCodecError::JsonSyntax,
                },
                ConversationReplayError::HeaderCorrupt,
            ),
            (
                ConversationScanError::UnsupportedFormatVersion,
                ConversationReplayError::UnsupportedFormatVersion,
            ),
            (
                ConversationScanError::MissingHeader,
                ConversationReplayError::MissingHeader,
            ),
            (
                ConversationScanError::LeaseMismatch,
                ConversationReplayError::LeaseMismatch,
            ),
            (
                ConversationScanError::InputChanged,
                ConversationReplayError::InputChanged,
            ),
            (
                ConversationScanError::InputUnavailable,
                ConversationReplayError::InputUnavailable,
            ),
            (
                ConversationScanError::CounterOverflow,
                ConversationReplayError::CounterOverflow,
            ),
            (
                ConversationScanError::InvariantViolation,
                ConversationReplayError::InvariantViolation,
            ),
        ] {
            assert_eq!(map_replay_scan_error(scanner_error), expected);
        }

        let empty = replay_conversation(
            Cursor::new(Vec::new()),
            0,
            "ses_11111111111111111111111111111111"
                .parse()
                .expect("session id is valid"),
            ConversationScanAccess::ReadOnly,
        );
        assert_eq!(empty.unwrap_err(), ConversationReplayError::MissingHeader);

        let malformed_header = replay_fixture(
            b"not json\n",
            "ses_11111111111111111111111111111111"
                .parse()
                .expect("session id is valid"),
        );
        assert_eq!(
            malformed_header.unwrap_err(),
            ConversationReplayError::HeaderCorrupt
        );
    }

    #[test]
    fn replay_cold_state_never_reconstructs_current_turn() {
        for case in CORRUPTION_CASES {
            let expected: Value =
                serde_json::from_str(case.expected).expect("expected sidecar is JSON");
            if expected["load"].as_str() == Some("fails") {
                continue;
            }
            let session_id = fixture_session(case.bytes);
            let view = replay_fixture(case.bytes, session_id)
                .unwrap_or_else(|error| panic!("{}: replay must load: {error}", case.name));
            assert_eq!(
                view.current_turn(),
                None,
                "{}: replay never reconstructs a current turn",
                case.name
            );
        }
    }
}
