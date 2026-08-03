use std::collections::BTreeSet;
use std::fmt;
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
    use super::*;
    use crate::model_gateway::{
        ModelId, ModelReasoningSummary, ModelServiceClass, ProviderId, ProviderResponseMetadata,
    };
    use crate::tools::{ToolApprovalResolutionRef, UserQuestionFieldAnswer};
    use crate::wire::conversation_jsonl::ConversationLineCodec;

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
