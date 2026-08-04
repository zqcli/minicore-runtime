use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use thiserror::Error;

use crate::live_conversation::ConversationRevision;
use crate::model_gateway::{
    ModelFinishReason, ModelResponseSummary, ModelUsage, ProviderResponseId,
    ProviderResponseMetadata,
};
use crate::prompt::ModelMessage;
use crate::wire::lexical::validate_safe_text;
use crate::wire::{EntryId, SessionId};

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
pub(crate) enum CompactionUnitKind {
    RollingSummary,
    UserMessage,
    AssistantMessage,
    ToolExchange,
}

#[derive(Clone)]
pub(crate) struct LiveCompactionSourceView {
    session_id: SessionId,
    revision: ConversationRevision,
    units: Arc<[LiveCompactionUnit]>,
}

#[derive(Clone)]
pub(crate) struct LiveCompactionUnit {
    first_entry_id: EntryId,
    kind: CompactionUnitKind,
    messages: Arc<[ModelMessage]>,
}

pub(crate) struct PreparedLiveCompactionUnit {
    kind: CompactionUnitKind,
    messages: Arc<[ModelMessage]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionSourceErrorReason {
    EmptyUnitMessages,
    DuplicateUnitOrigin,
    MisplacedRollingSummary,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct CompactionSourceError {
    reason: CompactionSourceErrorReason,
}

impl CompactionSourceError {
    const fn new(reason: CompactionSourceErrorReason) -> Self {
        Self { reason }
    }
}

impl fmt::Debug for CompactionSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionSourceError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for CompactionSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid live compaction source")
    }
}

impl Error for CompactionSourceError {}

impl PreparedLiveCompactionUnit {
    pub(crate) fn for_live_reducer(
        kind: CompactionUnitKind,
        messages: Arc<[ModelMessage]>,
    ) -> Result<Self, CompactionSourceError> {
        if messages.is_empty() {
            return Err(CompactionSourceError::new(
                CompactionSourceErrorReason::EmptyUnitMessages,
            ));
        }
        Ok(Self { kind, messages })
    }

    pub(crate) fn bind_origin(self, first_entry_id: EntryId) -> LiveCompactionUnit {
        LiveCompactionUnit {
            first_entry_id,
            kind: self.kind,
            messages: self.messages,
        }
    }
}

impl LiveCompactionUnit {
    pub(crate) const fn first_entry_id(&self) -> &EntryId {
        &self.first_entry_id
    }

    pub(crate) const fn kind(&self) -> CompactionUnitKind {
        self.kind
    }

    pub(crate) fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }
}

impl LiveCompactionSourceView {
    pub(crate) fn for_live_reducer(
        session_id: SessionId,
        revision: ConversationRevision,
        units: Arc<[LiveCompactionUnit]>,
    ) -> Result<Self, CompactionSourceError> {
        let mut origins = BTreeSet::new();
        for (index, unit) in units.iter().enumerate() {
            if unit.messages().is_empty() {
                return Err(CompactionSourceError::new(
                    CompactionSourceErrorReason::EmptyUnitMessages,
                ));
            }
            if !origins.insert(*unit.first_entry_id()) {
                return Err(CompactionSourceError::new(
                    CompactionSourceErrorReason::DuplicateUnitOrigin,
                ));
            }
            if unit.kind() == CompactionUnitKind::RollingSummary && index != 0 {
                return Err(CompactionSourceError::new(
                    CompactionSourceErrorReason::MisplacedRollingSummary,
                ));
            }
        }
        Ok(Self {
            session_id,
            revision,
            units,
        })
    }

    pub(crate) const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[cfg(test)]
    pub(crate) const fn revision(&self) -> &ConversationRevision {
        &self.revision
    }

    pub(crate) fn units(&self) -> &[LiveCompactionUnit] {
        &self.units
    }

    pub(crate) fn has_same_stable_identity(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.revision == other.revision
            && self.units.len() == other.units.len()
            && self
                .units
                .iter()
                .zip(other.units.iter())
                .all(|(left, right)| {
                    left.first_entry_id == right.first_entry_id && left.kind == right.kind
                })
    }
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

    pub(crate) fn reconstruct(
        summary: impl AsRef<str>,
        first_kept_entry_id: Option<EntryId>,
        model_call: Option<StoredCompactionModelCall>,
    ) -> Result<Self, CompactionValueError> {
        Self::new(summary, first_kept_entry_id, model_call)
    }

    /// Constructs an otherwise ordinary stored fact with a deliberately unchecked summary so
    /// the M4 replacement seam can prove its own pre-reducer validation boundary.
    #[cfg(test)]
    pub(crate) fn with_unchecked_summary_for_m4_test(
        summary: Arc<str>,
        first_kept_entry_id: Option<EntryId>,
    ) -> Self {
        Self {
            summary,
            first_kept_entry_id,
            model_call: None,
        }
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[allow(dead_code, reason = "consumed by Conversation codec/replay in M3/M5")]
    pub const fn first_kept_entry_id(&self) -> Option<EntryId> {
        self.first_kept_entry_id
    }

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

pub(crate) struct CompactionReplacement {
    stored: StoredCompaction,
    rolling_summary: ModelMessage,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionReplacementErrorReason {
    InvalidRollingSummary,
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct CompactionReplacementError {
    reason: CompactionReplacementErrorReason,
}

#[cfg(test)]
impl CompactionReplacementError {
    const fn invalid_rolling_summary() -> Self {
        Self {
            reason: CompactionReplacementErrorReason::InvalidRollingSummary,
        }
    }
}

#[cfg(test)]
impl fmt::Debug for CompactionReplacementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionReplacementError")
            .field("reason", &self.reason)
            .finish()
    }
}

#[cfg(test)]
impl fmt::Display for CompactionReplacementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid compaction replacement")
    }
}

#[cfg(test)]
impl Error for CompactionReplacementError {}

impl CompactionReplacement {
    #[cfg(test)]
    pub(crate) fn for_m4_test(
        stored: StoredCompaction,
    ) -> Result<Self, CompactionReplacementError> {
        let rolling_summary = ModelMessage::rolling_summary(stored.summary.clone())
            .map_err(|_| CompactionReplacementError::invalid_rolling_summary())?;
        Ok(Self {
            stored,
            rolling_summary,
        })
    }

    pub(crate) fn into_parts(self) -> (StoredCompaction, ModelMessage) {
        (self.stored, self.rolling_summary)
    }
}

impl fmt::Debug for CompactionReplacement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CompactionReplacement(<redacted>)")
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

    pub const fn model(&self) -> &ModelResponseSummary {
        &self.model
    }

    pub const fn response_id(&self) -> Option<&ProviderResponseId> {
        self.response_id.as_ref()
    }

    pub const fn usage(&self) -> Option<&ModelUsage> {
        self.usage.as_ref()
    }

    pub const fn finish_reason(&self) -> ModelFinishReason {
        self.finish_reason
    }

    pub const fn requested_max_output_tokens(&self) -> NonZeroU32 {
        self.requested_max_output_tokens
    }

    pub const fn logical_retry_count(&self) -> u8 {
        self.logical_retry_count
    }

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
    use crate::prompt::ModelMessageRef;

    fn entry_id(value: &str) -> EntryId {
        value.parse().expect("test entry IDs are valid")
    }

    fn session_id(value: &str) -> SessionId {
        value.parse().expect("test session IDs are valid")
    }

    fn model_message(text: &str) -> ModelMessage {
        ModelMessage::unstamped_user_text(Arc::from(text)).expect("test model messages are valid")
    }

    fn unit(first_entry_id: EntryId, kind: CompactionUnitKind, text: &str) -> LiveCompactionUnit {
        PreparedLiveCompactionUnit::for_live_reducer(kind, Arc::from([model_message(text)]))
            .expect("test unit is valid")
            .bind_origin(first_entry_id)
    }

    fn source(session_id: SessionId, units: Arc<[LiveCompactionUnit]>) -> LiveCompactionSourceView {
        source_at_revision(session_id, ConversationRevision::default(), units)
    }

    fn source_at_revision(
        session_id: SessionId,
        revision: ConversationRevision,
        units: Arc<[LiveCompactionUnit]>,
    ) -> LiveCompactionSourceView {
        LiveCompactionSourceView::for_live_reducer(session_id, revision, units)
            .expect("test source is valid")
    }

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
    fn stored_compaction_reconstructs_with_or_without_model_call() {
        let marker: EntryId = "ent_11111111111111111111111111111111".parse().unwrap();
        let automatic = StoredCompaction::reconstruct(
            "summary",
            Some(marker),
            Some(model_call(ModelFinishReason::Stop, 1).unwrap()),
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
        let stored = StoredCompaction::reconstruct(
            "SECRET-SUMMARY",
            None,
            Some(model_call(ModelFinishReason::Stop, 0).unwrap()),
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

    #[test]
    fn live_compaction_unit_and_source_factories_validate_structural_invariants() {
        let empty: Arc<[ModelMessage]> = Arc::from([]);
        let Err(error) =
            PreparedLiveCompactionUnit::for_live_reducer(CompactionUnitKind::UserMessage, empty)
        else {
            panic!("empty unit messages unexpectedly succeeded");
        };
        assert_eq!(error.reason, CompactionSourceErrorReason::EmptyUnitMessages);

        let duplicate = entry_id("ent_11111111111111111111111111111111");
        let Err(error) = LiveCompactionSourceView::for_live_reducer(
            session_id("ses_11111111111111111111111111111111"),
            ConversationRevision::default(),
            Arc::from([
                unit(duplicate, CompactionUnitKind::UserMessage, "SECRET-FIRST"),
                unit(
                    duplicate,
                    CompactionUnitKind::AssistantMessage,
                    "SECRET-SECOND",
                ),
            ]),
        ) else {
            panic!("duplicate unit origin unexpectedly succeeded");
        };
        assert_eq!(
            error.reason,
            CompactionSourceErrorReason::DuplicateUnitOrigin
        );
        for output in [format!("{error:?}"), error.to_string()] {
            assert!(!output.contains("SECRET-FIRST"));
            assert!(!output.contains("SECRET-SECOND"));
            assert!(!output.contains(&duplicate.to_string()));
        }
        assert!(std::error::Error::source(&error).is_none());

        let Err(error) = LiveCompactionSourceView::for_live_reducer(
            session_id("ses_11111111111111111111111111111111"),
            ConversationRevision::default(),
            Arc::from([
                unit(
                    entry_id("ent_22222222222222222222222222222222"),
                    CompactionUnitKind::UserMessage,
                    "ordinary",
                ),
                unit(
                    entry_id("ent_33333333333333333333333333333333"),
                    CompactionUnitKind::RollingSummary,
                    "summary",
                ),
            ]),
        ) else {
            panic!("misplaced rolling summary unexpectedly succeeded");
        };
        assert_eq!(
            error.reason,
            CompactionSourceErrorReason::MisplacedRollingSummary
        );
    }

    #[test]
    fn live_compaction_source_factory_rejects_forged_empty_unit_messages() {
        let forged_origin = entry_id("ent_11111111111111111111111111111111");
        let forged = LiveCompactionUnit {
            first_entry_id: forged_origin,
            kind: CompactionUnitKind::UserMessage,
            messages: Arc::from([]),
        };

        let Err(error) = LiveCompactionSourceView::for_live_reducer(
            session_id("ses_11111111111111111111111111111111"),
            ConversationRevision::default(),
            Arc::from([forged]),
        ) else {
            panic!("forged empty unit messages unexpectedly succeeded");
        };

        assert_eq!(error.reason, CompactionSourceErrorReason::EmptyUnitMessages);
        for output in [format!("{error:?}"), error.to_string()] {
            assert!(!output.contains(&forged_origin.to_string()));
        }
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn stable_identity_uses_session_revision_and_unit_origins_not_message_values() {
        let session = session_id("ses_11111111111111111111111111111111");
        let first = entry_id("ent_11111111111111111111111111111111");
        let second = entry_id("ent_22222222222222222222222222222222");
        let third = entry_id("ent_33333333333333333333333333333333");
        let original = source(
            session,
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );
        let changed_messages = source(
            session,
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "different user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "different assistant",
                ),
            ]),
        );
        let other_revision = source_at_revision(
            session,
            ConversationRevision::default().checked_next().unwrap(),
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );
        let fewer_units = source(
            session,
            Arc::from([unit(
                first,
                CompactionUnitKind::UserMessage,
                "original user",
            )]),
        );
        let reordered_units = source(
            session,
            Arc::from([
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
                unit(first, CompactionUnitKind::UserMessage, "original user"),
            ]),
        );
        let changed_first_entry_id = source(
            session,
            Arc::from([
                unit(third, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );
        let changed_kind = source(
            session,
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::ToolExchange,
                    "original assistant",
                ),
            ]),
        );
        let other_session = source(
            session_id("ses_22222222222222222222222222222222"),
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );

        assert!(original.has_same_stable_identity(&changed_messages));
        assert!(!original.has_same_stable_identity(&other_revision));
        assert!(!original.has_same_stable_identity(&fewer_units));
        assert!(!original.has_same_stable_identity(&reordered_units));
        assert!(!original.has_same_stable_identity(&changed_first_entry_id));
        assert!(!original.has_same_stable_identity(&changed_kind));
        assert!(!original.has_same_stable_identity(&other_session));
        assert_eq!(original.session_id(), &session);
        assert_eq!(original.revision(), &ConversationRevision::default());
        assert_eq!(original.units()[0].first_entry_id(), &first);
        assert_eq!(original.units()[0].kind(), CompactionUnitKind::UserMessage);
    }

    #[test]
    fn compaction_source_clone_and_origin_binding_preserve_arc_backed_values() {
        let messages: Arc<[ModelMessage]> = Arc::from([model_message("arc preserved")]);
        let prepared = PreparedLiveCompactionUnit::for_live_reducer(
            CompactionUnitKind::UserMessage,
            messages.clone(),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&prepared.messages, &messages));

        let unit = prepared.bind_origin(entry_id("ent_11111111111111111111111111111111"));
        assert!(Arc::ptr_eq(&unit.messages, &messages));
        assert!(std::ptr::eq(&unit.messages()[0], &messages[0]));
        let unit_clone = unit.clone();
        assert!(Arc::ptr_eq(&unit.messages, &unit_clone.messages));
        assert!(std::ptr::eq(&unit.messages()[0], &unit_clone.messages()[0],));

        let source = source(
            session_id("ses_11111111111111111111111111111111"),
            Arc::from([unit]),
        );
        let clone = source.clone();
        assert!(Arc::ptr_eq(&source.units, &clone.units));
        assert!(std::ptr::eq(
            &source.units()[0].messages()[0],
            &clone.units()[0].messages()[0],
        ));
    }

    #[test]
    fn m4_replacement_is_consuming_and_redacts_summary_validation_details() {
        let marker = entry_id("ent_11111111111111111111111111111111");
        let stored = StoredCompaction::reconstruct(
            "SECRET-ROLLING-SUMMARY",
            Some(marker),
            Some(model_call(ModelFinishReason::Stop, 0).unwrap()),
        )
        .unwrap();
        let replacement = CompactionReplacement::for_m4_test(stored.clone()).unwrap();
        let debug = format!("{replacement:?}");
        assert_eq!(debug, "CompactionReplacement(<redacted>)");
        assert!(!debug.contains("SECRET-ROLLING-SUMMARY"));
        assert!(!debug.contains(&marker.to_string()));
        assert!(!debug.contains("SECRET-RESPONSE-ID"));
        assert!(!debug.contains("SECRET-PROVIDER-REQUEST-ID"));

        let (returned_stored, rolling_summary) = replacement.into_parts();
        assert_eq!(returned_stored, stored);
        assert!(Arc::ptr_eq(&returned_stored.summary, &stored.summary));
        let ModelMessageRef::User { content } = rolling_summary.as_ref() else {
            panic!("replacement did not materialize a user-role rolling summary");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "SECRET-ROLLING-SUMMARY");
        assert_eq!(
            content[0].as_text().as_ptr(),
            returned_stored.summary().as_ptr()
        );

        let too_long_summary: Arc<str> = format!(
            "SECRET-TOO-LONG-{}",
            "x".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES + 1 - "SECRET-TOO-LONG-".len())
        )
        .into();
        assert_eq!(
            too_long_summary.len(),
            MAX_STORED_COMPACTION_SUMMARY_BYTES + 1
        );
        let forged_summaries: [(&str, Arc<str>); 3] = [
            ("EmptyText", Arc::from("")),
            ("UnsafeText", Arc::from("SECRET-UNSAFE\r\nSUMMARY")),
            ("TextTooLong", too_long_summary),
        ];

        for (case, summary) in forged_summaries {
            let Err(error) = CompactionReplacement::for_m4_test(StoredCompaction {
                summary,
                first_kept_entry_id: Some(marker),
                model_call: Some(model_call(ModelFinishReason::Stop, 0).unwrap()),
            }) else {
                panic!("{case} rolling summary unexpectedly succeeded");
            };
            assert_eq!(
                error.reason,
                CompactionReplacementErrorReason::InvalidRollingSummary,
                "{case} rolling summary mapped to the wrong error"
            );
            assert_eq!(
                format!("{error:?}"),
                "CompactionReplacementError { reason: InvalidRollingSummary }",
                "{case} rolling summary debug output leaked validation details"
            );
            assert_eq!(
                error.to_string(),
                "invalid compaction replacement",
                "{case} rolling summary display output leaked validation details"
            );
            assert!(
                std::error::Error::source(&error).is_none(),
                "{case} rolling summary unexpectedly retained a source error"
            );
        }
    }
}
