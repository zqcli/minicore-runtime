use std::fmt;
use std::sync::Arc;

use crate::config::{SemanticLimits, SessionSpec};
use crate::ids::TurnId;

use super::compaction_candidate::CompactionCandidate;
use super::entry::{ConversationEntry, ConversationSeq, SummaryEntry, TurnExecutionRecord};
use super::state::ConversationState;
use super::validator::ConversationValidationError;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PromptConversationProjection {
    selected_summary: Option<SummaryEntry>,
    entries: Arc<[ConversationEntry]>,
    head: ConversationSeq,
    active_turn_id: Option<TurnId>,
    active_turn_execution: Option<TurnExecutionRecord>,
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
            .field("active_turn_id", &self.active_turn_id)
            .field(
                "active_turn_model",
                &self
                    .active_turn_execution
                    .as_ref()
                    .map(|execution| &execution.model),
            )
            .field(
                "active_turn_max_tool_rounds",
                &self
                    .active_turn_execution
                    .as_ref()
                    .map(|execution| execution.max_tool_rounds),
            )
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

    pub(crate) const fn active_turn_id(&self) -> Option<TurnId> {
        self.active_turn_id
    }

    pub(crate) fn active_turn_execution(&self) -> Option<&TurnExecutionRecord> {
        self.active_turn_execution.as_ref()
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
        let state = self.validated_state(spec, limits)?;
        let selected_summary = state.projection().latest_summary().cloned();
        let active_turn_id = state.active_turn_id();
        let active_turn_execution = active_turn_id.and_then(|turn_id| {
            state
                .projection()
                .entries()
                .iter()
                .rev()
                .find_map(|entry| match entry {
                    ConversationEntry::UserMessage(entry) if entry.turn_id == turn_id => {
                        Some(entry.execution.clone())
                    }
                    _ => None,
                })
        });
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
            active_turn_id,
            active_turn_execution,
        })
    }

    pub(crate) fn validated_compaction_candidate(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<CompactionCandidate, ConversationValidationError> {
        Ok(self.validated_state(spec, limits)?.compaction_candidate())
    }

    fn validated_state(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<ConversationState, ConversationValidationError> {
        let state =
            ConversationState::new(spec.clone(), limits.clone())?.candidate(self.entries())?;
        if state.head() != self.head {
            return Err(ConversationValidationError::SequenceGap);
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests;
