use crate::config::{SemanticLimits, SessionSpec};
use crate::ids::TurnId;

use super::compaction_candidate::CompactionCandidate;
use super::entry::{ConversationEntry, ConversationSeq};
use super::projection::PromptProjection;
use super::validator::{ConversationValidationError, ConversationValidator, PendingToolCall};

#[derive(Clone, Debug)]
pub(crate) struct ConversationState {
    validator: ConversationValidator,
    projection: PromptProjection,
}

impl ConversationState {
    pub(crate) fn new(
        spec: SessionSpec,
        limits: SemanticLimits,
    ) -> Result<Self, ConversationValidationError> {
        Ok(Self {
            validator: ConversationValidator::new(spec, limits)?,
            projection: PromptProjection::default(),
        })
    }

    pub(crate) fn candidate(
        &self,
        entries: &[ConversationEntry],
    ) -> Result<Self, ConversationValidationError> {
        Ok(Self {
            validator: self.validator.validate_batch(entries)?,
            projection: self.projection.appended(entries),
        })
    }

    pub(crate) fn commit(&mut self, candidate: Self) {
        *self = candidate;
    }

    pub(crate) fn matches_configuration(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> bool {
        self.validator.matches_configuration(spec, limits)
    }

    pub(crate) const fn head(&self) -> ConversationSeq {
        self.validator.head()
    }

    pub(crate) fn projection(&self) -> &PromptProjection {
        &self.projection
    }

    pub(crate) fn compaction_candidate(&self) -> CompactionCandidate {
        let entries = self.projection.entries().to_vec().into();
        let completed_boundaries = self
            .validator
            .terminal_boundaries()
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .into();
        CompactionCandidate::from_confirmed(
            entries,
            self.head(),
            self.validator.latest_summary_through(),
            completed_boundaries,
        )
    }

    pub(crate) const fn active_turn_id(&self) -> Option<TurnId> {
        self.validator.active_turn_id()
    }

    pub(crate) fn unresolved_tool_calls(&self) -> &[PendingToolCall] {
        self.validator.unresolved_tool_calls()
    }

    pub(crate) const fn max_tool_output_bytes(&self) -> usize {
        self.validator.max_tool_output_bytes()
    }

    pub(crate) fn contains_seq(&self, seq: ConversationSeq) -> bool {
        self.projection
            .entries()
            .iter()
            .any(|entry| entry.seq() == seq)
    }

    pub(crate) fn matches_confirmed_page(
        &self,
        after: Option<ConversationSeq>,
        entries: &[ConversationEntry],
    ) -> bool {
        let confirmed = self.projection.entries();
        entries.iter().enumerate().all(|(index, entry)| {
            let expected = after
                .and_then(|cursor| cursor.get().checked_add(index as u64 + 1))
                .unwrap_or(index as u64 + 1);
            entry.seq().get() == expected
                && confirmed.get(expected.saturating_sub(1) as usize) == Some(entry)
        })
    }

    #[cfg(test)]
    pub(crate) fn set_head_for_test(&mut self, head: ConversationSeq) {
        self.validator.set_head_for_test(head);
    }
}
