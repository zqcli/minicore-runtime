use serde::{Deserialize, Serialize};

use super::session_log::ConversationPage;

use super::entry::{ConversationEntry, ConversationSeq};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptPage {
    pub entries: Vec<ConversationEntry>,
    pub next_after: Option<ConversationSeq>,
    pub observed_head: ConversationSeq,
    pub complete: bool,
}

pub(crate) fn valid_page_contract(
    page: &ConversationPage,
    after: Option<ConversationSeq>,
    limit: usize,
) -> bool {
    if page.entries.len() > limit || page.next_after.is_some() && page.entries.is_empty() {
        return false;
    }
    if page.entries.is_empty() {
        return page.next_after.is_none()
            && page.observed_head == after.unwrap_or(ConversationSeq::ZERO);
    }
    let mut expected = after.unwrap_or(ConversationSeq::ZERO).next();
    for entry in &page.entries {
        if Some(entry.seq()) != expected {
            return false;
        }
        expected = entry.seq().next();
    }
    let Some(last) = page.entries.last() else {
        return page.next_after.is_none();
    };
    match page.next_after {
        Some(next_after) => next_after == last.seq() && last.seq() != page.observed_head,
        None => last.seq() == page.observed_head,
    }
}
