use std::fmt;
use std::sync::Arc;

use crate::config::{SemanticLimits, SessionSpec};

use super::entry::{ConversationEntry, ConversationSeq, SummaryEntry};
use super::state::ConversationState;
use super::validator::ConversationValidationError;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PromptConversationProjection {
    selected_summary: Option<SummaryEntry>,
    entries: Arc<[ConversationEntry]>,
    head: ConversationSeq,
}

impl fmt::Debug for PromptConversationProjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptConversationProjection")
            .field("head", &self.head)
            .field(
                "selected_summary_seq",
                &self.selected_summary.as_ref().map(|summary| summary.seq),
            )
            .field(
                "selected_summary_through",
                &self
                    .selected_summary
                    .as_ref()
                    .map(|summary| summary.through),
            )
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

impl PromptConversationProjection {
    pub(crate) fn selected_summary(&self) -> Option<&SummaryEntry> {
        self.selected_summary.as_ref()
    }

    pub(crate) fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ConversationView {
    head: ConversationSeq,
    entries: Arc<[ConversationEntry]>,
}

impl fmt::Debug for ConversationView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversationView")
            .field("head", &self.head)
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

impl ConversationView {
    pub fn empty() -> Self {
        Self {
            head: ConversationSeq::ZERO,
            entries: Arc::<[ConversationEntry]>::from([]),
        }
    }

    pub(crate) fn from_confirmed(head: ConversationSeq, entries: Arc<[ConversationEntry]>) -> Self {
        Self { head, entries }
    }

    pub const fn head(&self) -> ConversationSeq {
        self.head
    }

    pub fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    pub(crate) fn validated_prompt_projection(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<PromptConversationProjection, ConversationValidationError> {
        let state =
            ConversationState::new(spec.clone(), limits.clone())?.candidate(self.entries())?;
        if state.head() != self.head {
            return Err(ConversationValidationError::SequenceGap);
        }
        let selected_summary = state.projection().latest_summary().cloned();
        let through = selected_summary.as_ref().map(|summary| summary.through);
        let entries = state
            .projection()
            .entries()
            .iter()
            .filter(|entry| through.is_none_or(|through| entry.seq() > through))
            .cloned()
            .collect::<Vec<_>>();
        Ok(PromptConversationProjection {
            selected_summary,
            entries: entries.into(),
            head: state.head(),
        })
    }
}

#[cfg(test)]
mod tests;
