use std::sync::Arc;

use super::entry::{ConversationEntry, ConversationSeq, SummaryEntry};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromptProjection {
    entries: Arc<[ConversationEntry]>,
    head: ConversationSeq,
    latest_summary: Option<SummaryEntry>,
}

impl Default for PromptProjection {
    fn default() -> Self {
        Self {
            entries: Arc::from([]),
            head: ConversationSeq::ZERO,
            latest_summary: None,
        }
    }
}

impl PromptProjection {
    pub(crate) const fn head(&self) -> ConversationSeq {
        self.head
    }

    pub(crate) fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    pub(crate) fn entries_arc(&self) -> Arc<[ConversationEntry]> {
        Arc::clone(&self.entries)
    }

    pub(crate) fn latest_summary(&self) -> Option<&SummaryEntry> {
        self.latest_summary.as_ref()
    }

    pub(crate) fn latest_summary_through(&self) -> Option<ConversationSeq> {
        self.latest_summary.as_ref().map(|summary| summary.through)
    }

    pub(crate) fn appended(&self, entries: &[ConversationEntry]) -> Self {
        let mut retained = Vec::with_capacity(self.entries.len() + entries.len());
        retained.extend(self.entries.iter().cloned());
        retained.extend(entries.iter().cloned());
        let latest_summary = entries
            .iter()
            .filter_map(|entry| match entry {
                ConversationEntry::Summary(summary) => Some(summary.clone()),
                _ => None,
            })
            .next_back()
            .or_else(|| self.latest_summary.clone());
        let head = entries.last().map_or(self.head, ConversationEntry::seq);
        Self {
            entries: retained.into(),
            head,
            latest_summary,
        }
    }
}
