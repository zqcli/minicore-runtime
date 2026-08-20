use serde::Serialize;

use crate::error::{PublicErrorCode, PublicErrorSummary};
use crate::ids::{InteractionId, ToolCallId, TurnId};
use crate::tools::ToolName;

use super::snapshot::TurnOutcome;
use crate::storage::conversation::{
    ConversationEntry, ConversationError, ConversationLog, ConversationSnapshot, StoredTurnOutcome,
};

const MAX_PAGE_SIZE: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TranscriptToolCall {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum TranscriptEntry {
    User {
        seq: u64,
        turn_id: TurnId,
        text: String,
    },
    Assistant {
        seq: u64,
        turn_id: TurnId,
        text: Option<String>,
        tool_calls: Vec<TranscriptToolCall>,
    },
    ToolResult {
        seq: u64,
        turn_id: TurnId,
        call_id: ToolCallId,
        text: String,
        is_error: bool,
    },
    Interaction {
        seq: u64,
        turn_id: TurnId,
        interaction_id: InteractionId,
        question: String,
        answer: String,
    },
    Summary {
        seq: u64,
        through_seq: u64,
        text: String,
    },
    Terminal {
        seq: u64,
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TranscriptPage {
    entries: Vec<TranscriptEntry>,
    next_after_seq: Option<u64>,
}

impl TranscriptPage {
    pub fn new(entries: Vec<TranscriptEntry>, next_after_seq: Option<u64>) -> Self {
        Self {
            entries,
            next_after_seq,
        }
    }

    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub const fn next_after_seq(&self) -> Option<u64> {
        self.next_after_seq
    }
}

impl ConversationLog {
    pub(crate) async fn transcript(
        &self,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<TranscriptPage, ConversationError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(ConversationError::InvalidPage);
        }
        let snapshot = self.snapshot().await;
        Ok(snapshot.transcript_page(after_seq, limit))
    }
}

impl ConversationSnapshot {
    pub(crate) fn transcript_page(&self, after_seq: Option<u64>, limit: usize) -> TranscriptPage {
        let start = after_seq.map_or(0, |after| {
            self.entries()
                .iter()
                .position(|entry| entry.seq() > after)
                .unwrap_or(self.entries().len())
        });
        let end = start.saturating_add(limit).min(self.entries().len());
        let entries = self.entries()[start..end]
            .iter()
            .map(|entry| entry.transcript_projection())
            .collect();
        let next_after_seq = (end < self.entries().len()).then(|| self.entries()[end - 1].seq());
        TranscriptPage::new(entries, next_after_seq)
    }
}

fn terminal_outcome(outcome: StoredTurnOutcome) -> TurnOutcome {
    match outcome {
        StoredTurnOutcome::Completed => TurnOutcome::Completed,
        StoredTurnOutcome::Cancelled | StoredTurnOutcome::CancelledByRestart => {
            TurnOutcome::Cancelled
        }
        StoredTurnOutcome::Failed => TurnOutcome::Failed {
            error: PublicErrorSummary::with_retryable(PublicErrorCode::Internal, false),
        },
    }
}

impl ConversationEntry {
    pub(crate) fn transcript_projection(&self) -> TranscriptEntry {
        match self {
            Self::User {
                seq, turn_id, text, ..
            } => TranscriptEntry::User {
                seq: *seq,
                turn_id: *turn_id,
                text: text.clone(),
            },
            Self::Assistant {
                seq,
                turn_id,
                text,
                tool_calls,
                ..
            } => TranscriptEntry::Assistant {
                seq: *seq,
                turn_id: *turn_id,
                text: text.clone(),
                tool_calls: tool_calls
                    .iter()
                    .map(|call| TranscriptToolCall {
                        call_id: call.tool_call_id().clone(),
                        tool_name: call.name().clone(),
                    })
                    .collect(),
            },
            Self::ToolResult {
                seq,
                turn_id,
                call_id,
                result,
                ..
            } => TranscriptEntry::ToolResult {
                seq: *seq,
                turn_id: *turn_id,
                call_id: call_id.clone(),
                text: result.text().to_owned(),
                is_error: result.is_error(),
            },
            Self::Interaction {
                seq,
                turn_id,
                interaction_id,
                question,
                answer,
                ..
            } => TranscriptEntry::Interaction {
                seq: *seq,
                turn_id: *turn_id,
                interaction_id: *interaction_id,
                question: question.question().to_owned(),
                answer: answer.text().to_owned(),
            },
            Self::Summary {
                seq,
                through_seq,
                text,
                ..
            } => TranscriptEntry::Summary {
                seq: *seq,
                through_seq: *through_seq,
                text: text.clone(),
            },
            Self::TurnTerminal {
                seq,
                turn_id,
                outcome,
                ..
            } => TranscriptEntry::Terminal {
                seq: *seq,
                turn_id: *turn_id,
                outcome: terminal_outcome(*outcome),
            },
        }
    }
}

const _: () = {
    let _ = std::mem::size_of::<TranscriptEntry>();
    let _ = std::mem::size_of::<TranscriptPage>();
    let _ = std::mem::size_of::<TranscriptToolCall>();
    let _ = TranscriptPage::new;
};
