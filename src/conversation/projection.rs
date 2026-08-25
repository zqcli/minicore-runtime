use std::sync::Arc;

use super::entry::{ConversationEntry, SummaryEntry};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PromptProjection {
    entries: Arc<[ConversationEntry]>,
    latest_summary: Option<SummaryEntry>,
}

impl PromptProjection {
    pub(crate) fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    pub(crate) fn entries_arc(&self) -> Arc<[ConversationEntry]> {
        Arc::clone(&self.entries)
    }

    pub(crate) fn latest_summary(&self) -> Option<&SummaryEntry> {
        self.latest_summary.as_ref()
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
        Self {
            entries: retained.into(),
            latest_summary,
        }
    }
}
