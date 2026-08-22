use crate::config::{SemanticLimits, SessionSpec};

use super::entry::{ConversationEntry, ConversationSeq};
use super::projection::PromptProjection;
use super::validator::{ConversationValidationError, ConversationValidator};

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
        Ok(Self {
            head: validator.head(),
            validator,
            projection: self.projection.appended(entries),
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

    #[cfg(test)]
    pub(crate) fn set_head_for_test(&mut self, head: ConversationSeq) {
        self.head = head;
        self.validator.set_head_for_test(head);
    }
}
