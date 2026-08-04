use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;
use std::num::NonZeroU32;
use std::sync::Arc;

use thiserror::Error;

use crate::agent_session_lifecycle::AgentRevisionRef;
use crate::compaction::StoredCompaction;
use crate::model_gateway::{
    ModelFinishReason, ModelResponseSummary, ModelUsage, ProviderResponseId,
    ProviderResponseMetadata, ReasoningContent,
};
use crate::prompt::CanonicalUserMessage;
use crate::tools::{
    ToolAbandonReason, ToolApprovalRequestView, ToolApprovalResolution, ToolCallId, ToolName,
    ToolOutcomeSource, ToolResultContent, ToolResultDisposition, UserQuestionAnswer,
    UserQuestionRequest,
};
use crate::turn_item_interaction::{
    AssistantDisposition, InteractionCancelReason, UserMessageSource,
};
use crate::wire::conversation_jsonl::ConversationCodecError;
use crate::wire::conversation_jsonl_scanner::{
    ConversationJsonlScanner, ConversationLineCanonicality, ConversationLineFault,
    ConversationScanAccess, ConversationScanError, ConversationScanEvent,
    MAX_CONVERSATION_ENTRY_RECORDS,
};
use crate::wire::{
    BoundedJsonObject, EntryId, InteractionResolutionKey, ItemId, RequestId,
    SessionDefinitionRevision, SessionId, Timestamp, TurnId,
};

#[path = "live_conversation.rs"]
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

/// Opaque evidence that Conversation Storage opened one physical conversation file for exclusive
/// writable use.
///
/// This is deliberately only a file-binding capability: it records the Session identity and the
/// physical length observed while opening the file. It does not acquire, hold, or emulate an OS
/// lock. M5.0 will add the owning storage path that can create this proof after it has acquired a
/// real lease.
pub(crate) struct ExclusiveWritableConversationLease {
    session_id: SessionId,
    declared_file_bytes: u64,
}

impl ExclusiveWritableConversationLease {
    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn declared_file_bytes(&self) -> u64 {
        self.declared_file_bytes
    }

    #[cfg(test)]
    pub(crate) const fn for_scanner_test(session_id: SessionId, declared_file_bytes: u64) -> Self {
        Self {
            session_id,
            declared_file_bytes,
        }
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

    // The production scanner applies the same fixed V1 complete-entry cap before yielding an
    // Event. The explicit bound keeps this identity set bounded if scanner internals evolve.
    let maximum_recovery_entry_ids = usize::try_from(MAX_CONVERSATION_ENTRY_RECORDS)
        .map_err(|_| UnpublishedConversationRecoveryError::TooLarge)?;
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
                if seen_entry_ids.len() >= maximum_recovery_entry_ids {
                    return Err(UnpublishedConversationRecoveryError::TooLarge);
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
    use std::io::{self, Cursor, Read};

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
}
