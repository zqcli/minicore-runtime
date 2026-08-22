use crate::config::{SemanticLimits, SessionSpec};
use crate::ids::TurnId;

use super::entry::{ConversationEntry, ConversationSeq};
use super::projection::PromptProjection;
use super::validator::{ConversationValidationError, ConversationValidator, PendingToolCall};

#[derive(Clone, Debug)]
pub(crate) struct ConversationState {
    validator: ConversationValidator,
    projection: PromptProjection,
    head: ConversationSeq,
}

impl ConversationState {
    pub(crate) fn new(
        spec: SessionSpec,
        limits: SemanticLimits,
    ) -> Result<Self, ConversationValidationError> {
        let validator = ConversationValidator::new(spec, limits)?;
        Ok(Self {
            head: validator.head(),
            validator,
            projection: PromptProjection::default(),
        })
    }

    pub(crate) fn candidate(
        &self,
        entries: &[ConversationEntry],
    ) -> Result<Self, ConversationValidationError> {
        let validator = self.validator.validate_batch(entries)?;
        let projection = self.projection.appended(entries);
        debug_assert_eq!(validator.head(), projection.head());
        debug_assert_eq!(
            validator.latest_summary_through(),
            projection.latest_summary().map(|summary| summary.through)
        );
        debug_assert_eq!(
            validator.latest_summary_through(),
            projection.latest_summary_through()
        );
        debug_assert!(validator.terminal_boundaries().iter().all(|boundary| {
            projection
                .entries()
                .iter()
                .any(|entry| entry.seq() == *boundary)
        }));
        Ok(Self {
            head: validator.head(),
            validator,
            projection,
        })
    }

    pub(crate) fn commit(&mut self, candidate: Self) {
        *self = candidate;
    }

    pub(crate) const fn head(&self) -> ConversationSeq {
        self.head
    }

    pub(crate) fn projection(&self) -> &PromptProjection {
        &self.projection
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
        self.head = head;
        self.validator.set_head_for_test(head);
    }
}
