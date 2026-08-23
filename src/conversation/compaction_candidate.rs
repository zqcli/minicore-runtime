use std::fmt;
use std::sync::Arc;

use super::entry::{ConversationEntry, ConversationSeq};

#[derive(Clone, Eq, PartialEq)]
pub struct CompactionCandidate {
    entries: Arc<[ConversationEntry]>,
    head: ConversationSeq,
    latest_summary_through: Option<ConversationSeq>,
    completed_boundaries: Arc<[ConversationSeq]>,
}

impl fmt::Debug for CompactionCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionCandidate")
            .field("entry_count", &self.entries.len())
            .field("head", &self.head)
            .field("latest_summary_through", &self.latest_summary_through)
            .field("completed_boundary_count", &self.completed_boundaries.len())
            .finish()
    }
}

impl CompactionCandidate {
    pub fn empty() -> Self {
        Self {
            entries: Arc::<[ConversationEntry]>::from([]),
            head: ConversationSeq::ZERO,
            latest_summary_through: None,
            completed_boundaries: Arc::<[ConversationSeq]>::from([]),
        }
    }

    pub(super) fn from_confirmed(
        entries: Arc<[ConversationEntry]>,
        head: ConversationSeq,
        latest_summary_through: Option<ConversationSeq>,
        completed_boundaries: Arc<[ConversationSeq]>,
    ) -> Self {
        Self {
            entries,
            head,
            latest_summary_through,
            completed_boundaries,
        }
    }

    #[cfg(test)]
    pub(crate) fn forge_for_test(
        entries: Arc<[ConversationEntry]>,
        head: ConversationSeq,
        latest_summary_through: Option<ConversationSeq>,
        completed_boundaries: Arc<[ConversationSeq]>,
    ) -> Self {
        Self::from_confirmed(entries, head, latest_summary_through, completed_boundaries)
    }

    pub fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    pub const fn head(&self) -> ConversationSeq {
        self.head
    }

    pub const fn latest_summary_through(&self) -> Option<ConversationSeq> {
        self.latest_summary_through
    }

    pub fn completed_boundaries(&self) -> &[ConversationSeq] {
        &self.completed_boundaries
    }
}
