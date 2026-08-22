use std::fmt;
use std::sync::Arc;

use super::entry::{ConversationEntry, ConversationSeq};

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
}
