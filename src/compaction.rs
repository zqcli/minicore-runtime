use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use thiserror::Error;

use crate::model_gateway::{
    ModelFinishReason, ModelResponseSummary, ModelUsage, ProviderResponseId,
    ProviderResponseMetadata,
};
use crate::wire::EntryId;
use crate::wire::lexical::validate_safe_text;

pub(crate) const MAX_STORED_COMPACTION_SUMMARY_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum CompactionValueError {
    #[error("stored compaction summary is empty, unsafe, or exceeds its byte limit")]
    Summary,
    #[error("stored compaction finish reason is not portable")]
    FinishReason,
    #[error("stored compaction logical retry count exceeds its limit")]
    LogicalRetryCount,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[allow(dead_code, reason = "consumed by LiveConversation reducer in M4")]
pub(crate) enum CompactionUnitKind {
    RollingSummary,
    UserMessage,
    AssistantMessage,
    ToolExchange,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredCompaction {
    summary: Arc<str>,
    first_kept_entry_id: Option<EntryId>,
    model_call: Option<StoredCompactionModelCall>,
}

impl StoredCompaction {
    fn new(
        summary: impl AsRef<str>,
        first_kept_entry_id: Option<EntryId>,
        model_call: Option<StoredCompactionModelCall>,
    ) -> Result<Self, CompactionValueError> {
        let summary = summary.as_ref();
        validate_safe_text(summary, MAX_STORED_COMPACTION_SUMMARY_BYTES, false)
            .map_err(|_| CompactionValueError::Summary)?;
        Ok(Self {
            summary: summary.into(),
            first_kept_entry_id,
            model_call,
        })
    }

    #[allow(
        dead_code,
        reason = "constructed by Compaction summary validation in M10"
    )]
    fn from_automatic(
        summary: impl AsRef<str>,
        first_kept_entry_id: Option<EntryId>,
        model_call: StoredCompactionModelCall,
    ) -> Result<Self, CompactionValueError> {
        Self::new(summary, first_kept_entry_id, Some(model_call))
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        summary: impl AsRef<str>,
        first_kept_entry_id: Option<EntryId>,
        model_call: Option<StoredCompactionModelCall>,
    ) -> Result<Self, CompactionValueError> {
        Self::new(summary, first_kept_entry_id, model_call)
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[allow(dead_code, reason = "consumed by Conversation codec/replay in M3/M5")]
    pub const fn first_kept_entry_id(&self) -> Option<EntryId> {
        self.first_kept_entry_id
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn model_call(&self) -> Option<&StoredCompactionModelCall> {
        self.model_call.as_ref()
    }
}

impl fmt::Debug for StoredCompaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCompaction")
            .field("summary_bytes", &self.summary.len())
            .field(
                "has_first_kept_entry_id",
                &self.first_kept_entry_id.is_some(),
            )
            .field("has_model_call", &self.model_call.is_some())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredCompactionModelCall {
    model: ModelResponseSummary,
    response_id: Option<ProviderResponseId>,
    usage: Option<ModelUsage>,
    finish_reason: ModelFinishReason,
    requested_max_output_tokens: NonZeroU32,
    logical_retry_count: u8,
    metadata: ProviderResponseMetadata,
}

impl StoredCompactionModelCall {
    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the frozen Compaction provenance shape"
    )]
    fn new(
        model: ModelResponseSummary,
        response_id: Option<ProviderResponseId>,
        usage: Option<ModelUsage>,
        finish_reason: ModelFinishReason,
        requested_max_output_tokens: NonZeroU32,
        logical_retry_count: u8,
        metadata: ProviderResponseMetadata,
    ) -> Result<Self, CompactionValueError> {
        if !matches!(
            finish_reason,
            ModelFinishReason::Stop | ModelFinishReason::Unknown
        ) {
            return Err(CompactionValueError::FinishReason);
        }
        if logical_retry_count > 1 {
            return Err(CompactionValueError::LogicalRetryCount);
        }
        Ok(Self {
            model,
            response_id,
            usage,
            finish_reason,
            requested_max_output_tokens,
            logical_retry_count,
            metadata,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the frozen Compaction provenance shape"
    )]
    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        model: ModelResponseSummary,
        response_id: Option<ProviderResponseId>,
        usage: Option<ModelUsage>,
        finish_reason: ModelFinishReason,
        requested_max_output_tokens: NonZeroU32,
        logical_retry_count: u8,
        metadata: ProviderResponseMetadata,
    ) -> Result<Self, CompactionValueError> {
        Self::new(
            model,
            response_id,
            usage,
            finish_reason,
            requested_max_output_tokens,
            logical_retry_count,
            metadata,
        )
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn model(&self) -> &ModelResponseSummary {
        &self.model
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn response_id(&self) -> Option<&ProviderResponseId> {
        self.response_id.as_ref()
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn usage(&self) -> Option<&ModelUsage> {
        self.usage.as_ref()
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn finish_reason(&self) -> ModelFinishReason {
        self.finish_reason
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn requested_max_output_tokens(&self) -> NonZeroU32 {
        self.requested_max_output_tokens
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn logical_retry_count(&self) -> u8 {
        self.logical_retry_count
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub const fn metadata(&self) -> &ProviderResponseMetadata {
        &self.metadata
    }
}

impl fmt::Debug for StoredCompactionModelCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCompactionModelCall")
            .field("model", &self.model)
            .field("has_response_id", &self.response_id.is_some())
            .field("has_usage", &self.usage.is_some())
            .field("finish_reason", &self.finish_reason)
            .field(
                "requested_max_output_tokens",
                &self.requested_max_output_tokens,
            )
            .field("logical_retry_count", &self.logical_retry_count)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_gateway::{ModelReasoningSummary, ModelServiceClass};

    fn model_call(
        finish_reason: ModelFinishReason,
        logical_retry_count: u8,
    ) -> Result<StoredCompactionModelCall, CompactionValueError> {
        StoredCompactionModelCall::new(
            ModelResponseSummary::reconstruct(
                "openai".parse().unwrap(),
                "gpt-5-mini".parse().unwrap(),
                ModelReasoningSummary::Disabled,
                ModelServiceClass::Standard,
            ),
            Some("SECRET-RESPONSE-ID".parse().unwrap()),
            Some(ModelUsage::reconstruct(
                Some(1_000),
                Some(100),
                None,
                None,
                None,
                Some(1_100),
                None,
            )),
            finish_reason,
            NonZeroU32::new(512).unwrap(),
            logical_retry_count,
            ProviderResponseMetadata::reconstruct(
                Some("SECRET-PROVIDER-REQUEST-ID".parse().unwrap()),
                Some("SECRET-FINISH".parse().unwrap()),
                Some("SECRET-SERVICE-TIER".parse().unwrap()),
            ),
        )
    }

    #[test]
    fn stored_summary_is_safe_and_bounded_by_utf8_bytes() {
        let stored = StoredCompaction::reconstruct("a\nb", None, None).unwrap();
        assert_eq!(stored.summary(), "a\nb");
        assert!(StoredCompaction::reconstruct("", None, None).is_err());
        assert!(StoredCompaction::reconstruct("bad\u{001b}", None, None).is_err());
        assert!(StoredCompaction::reconstruct("bad\r\ntext", None, None).is_err());
        assert!(StoredCompaction::reconstruct("bad\rtext", None, None).is_err());
        assert!(
            StoredCompaction::reconstruct(
                "x".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES),
                None,
                None,
            )
            .is_ok()
        );
        assert!(
            StoredCompaction::reconstruct(
                "x".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES + 1),
                None,
                None,
            )
            .is_err()
        );
        assert!(
            StoredCompaction::reconstruct(
                "é".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES / 2),
                None,
                None,
            )
            .is_ok()
        );
        assert!(
            StoredCompaction::reconstruct(
                "é".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES / 2 + 1),
                None,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn stored_model_call_accepts_only_portable_finish_and_retry_facts() {
        assert!(model_call(ModelFinishReason::Stop, 0).is_ok());
        assert!(model_call(ModelFinishReason::Unknown, 1).is_ok());
        for finish_reason in [
            ModelFinishReason::ToolCalls,
            ModelFinishReason::Length,
            ModelFinishReason::ContentFiltered,
            ModelFinishReason::Refused,
        ] {
            assert_eq!(
                model_call(finish_reason, 0),
                Err(CompactionValueError::FinishReason)
            );
        }
        assert_eq!(
            model_call(ModelFinishReason::Stop, 2),
            Err(CompactionValueError::LogicalRetryCount)
        );
    }

    #[test]
    fn automatic_and_replay_construction_have_distinct_provenance_rules() {
        let marker: EntryId = "ent_11111111111111111111111111111111".parse().unwrap();
        let automatic = StoredCompaction::from_automatic(
            "summary",
            Some(marker),
            model_call(ModelFinishReason::Stop, 1).unwrap(),
        )
        .unwrap();
        assert_eq!(automatic.first_kept_entry_id(), Some(marker));
        assert!(automatic.model_call().is_some());
        assert_eq!(
            automatic
                .model_call()
                .unwrap()
                .requested_max_output_tokens()
                .get(),
            512
        );

        let replayed = StoredCompaction::reconstruct("summary", Some(marker), None).unwrap();
        assert_eq!(replayed.first_kept_entry_id(), Some(marker));
        assert!(replayed.model_call().is_none());
    }

    #[test]
    fn compaction_debug_does_not_expose_summary_or_provider_ids() {
        let stored = StoredCompaction::from_automatic(
            "SECRET-SUMMARY",
            None,
            model_call(ModelFinishReason::Stop, 0).unwrap(),
        )
        .unwrap();
        let debug = format!("{stored:?} {:?}", stored.model_call().unwrap());
        assert!(!debug.contains("SECRET-SUMMARY"));
        assert!(!debug.contains("SECRET-RESPONSE-ID"));
        assert!(!debug.contains("SECRET-PROVIDER-REQUEST-ID"));
        assert!(!debug.contains("SECRET-FINISH"));
        assert!(!debug.contains("SECRET-SERVICE-TIER"));
    }

    #[test]
    fn stable_unit_kinds_are_closed_and_distinct() {
        let kinds = [
            CompactionUnitKind::RollingSummary,
            CompactionUnitKind::UserMessage,
            CompactionUnitKind::AssistantMessage,
            CompactionUnitKind::ToolExchange,
        ];
        assert_eq!(kinds.len(), 4);
        for (index, kind) in kinds.iter().enumerate() {
            assert!(!kinds[..index].contains(kind));
        }
    }
}
