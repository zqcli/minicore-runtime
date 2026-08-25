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

#[derive(Clone)]
pub struct ConversationView {
    head: ConversationSeq,
    entries: Arc<[ConversationEntry]>,
    /// The already-validated state this view was projected from, when the view
    /// came from the owning `ConversationLog`. Present views are consumed
    /// without replaying the validator; absent views fall back to a full
    /// replay so externally constructed views keep the same guarantees.
    state: Option<Arc<ConversationState>>,
}

impl fmt::Debug for ConversationView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversationView")
            .field("head", &self.head)
            .field("entry_count", &self.entries.len())
            .field("validated", &self.state.is_some())
            .finish()
    }
}

/// Two views are equal when they carry the same confirmed conversation.
/// The cached validated state is provenance, not content, so it is excluded.
impl PartialEq for ConversationView {
    fn eq(&self, other: &Self) -> bool {
        self.head == other.head && self.entries == other.entries
    }
}

impl Eq for ConversationView {}

impl ConversationView {
    pub fn empty() -> Self {
        Self {
            head: ConversationSeq::ZERO,
            entries: Arc::<[ConversationEntry]>::from([]),
            state: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_confirmed(head: ConversationSeq, entries: Arc<[ConversationEntry]>) -> Self {
        Self {
            head,
            entries,
            state: None,
        }
    }

    /// Builds a view from the owning log's validated state. The state proves the
    /// entries already passed semantic validation at `head`, so consumers that
    /// supply a matching configuration need no replay.
    pub(crate) fn from_validated_state(state: &Arc<ConversationState>) -> Self {
        Self {
            head: state.head(),
            entries: state.projection().entries_arc(),
            state: Some(Arc::clone(state)),
        }
    }

    pub const fn head(&self) -> ConversationSeq {
        self.head
    }

    pub fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    /// Resolves the active turn and its durable execution record without
    /// materialising a prompt projection. Callers that only need to confirm
    /// turn identity use this instead of building the full projection.
    pub(crate) fn validated_active_turn(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<ActiveTurnProof, ConversationValidationError> {
        let state = self.validated_state(spec, limits)?;
        Ok(active_turn_proof(state.as_ref()))
    }

    pub(crate) fn validated_prompt_projection(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<PromptConversationProjection, ConversationValidationError> {
        let state = self.validated_state(spec, limits)?;
        let state = state.as_ref();
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

    pub(crate) fn validated_compaction_candidate(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<CompactionCandidate, ConversationValidationError> {
        Ok(self.validated_state(spec, limits)?.compaction_candidate())
    }

    /// Returns the validated state for `spec`/`limits`, reusing the state the
    /// view carries when it was validated against the same configuration and
    /// replaying the validator otherwise.
    fn validated_state(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<MaybeOwnedState<'_>, ConversationValidationError> {
        if let Some(state) = &self.state {
            if state.head() == self.head && state.matches_configuration(spec, limits) {
                return Ok(MaybeOwnedState::Validated(state));
            }
        }
        let state =
            ConversationState::new(spec.clone(), limits.clone())?.candidate(self.entries())?;
        if state.head() != self.head {
            return Err(ConversationValidationError::SequenceGap);
        }
        Ok(MaybeOwnedState::Replayed(Box::new(state)))
    }
}

pub(crate) struct ActiveTurnProof {
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) execution: Option<TurnExecutionRecord>,
}

fn active_turn_proof(state: &ConversationState) -> ActiveTurnProof {
    let turn_id = state.active_turn_id();
    let execution = turn_id.and_then(|turn_id| {
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
    ActiveTurnProof { turn_id, execution }
}

enum MaybeOwnedState<'a> {
    Validated(&'a ConversationState),
    Replayed(Box<ConversationState>),
}

impl MaybeOwnedState<'_> {
    fn as_ref(&self) -> &ConversationState {
        match self {
            Self::Validated(state) => state,
            Self::Replayed(state) => state,
        }
    }

    fn compaction_candidate(&self) -> CompactionCandidate {
        self.as_ref().compaction_candidate()
    }
}

#[cfg(test)]
mod tests;
