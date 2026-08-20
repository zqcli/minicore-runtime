use super::{ConversationEntry, ConversationLog, ConversationSnapshot, StoredTurnOutcome};
use crate::ids::SessionId;
use crate::model::Usage;

pub(crate) const MAX_USER_TEXT_BYTES: usize = super::MAX_TEXT_BYTES;

pub(crate) fn validate_user_text(value: &str) -> Result<(), super::ConversationError> {
    super::validate_text(value, MAX_USER_TEXT_BYTES)
}

impl ConversationLog {
    pub(crate) fn session_id(&self) -> SessionId {
        self.inner.id
    }
}

impl ConversationSnapshot {
    pub(crate) fn entries(&self) -> &[std::sync::Arc<ConversationEntry>] {
        &self.entries
    }

    pub(crate) const fn max_seq(&self) -> u64 {
        self.max_seq
    }

    pub(crate) const fn health(&self) -> super::ConversationHealth {
        self.health
    }

    pub(crate) fn usage(&self) -> Usage {
        super::usage::usage_from_entries(&self.entries)
    }

    pub(crate) fn latest_terminal(&self) -> Option<(crate::ids::TurnId, StoredTurnOutcome)> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match entry.as_ref() {
                ConversationEntry::TurnTerminal {
                    turn_id, outcome, ..
                } => Some((*turn_id, *outcome)),
                _ => None,
            })
    }

    pub(crate) fn has_failed_terminal(&self) -> bool {
        self.entries.iter().rev().any(|entry| {
            matches!(
                entry.as_ref(),
                ConversationEntry::TurnTerminal {
                    outcome: StoredTurnOutcome::Failed,
                    ..
                }
            )
        })
    }
}
