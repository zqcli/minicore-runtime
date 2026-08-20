use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tokio::sync::Notify;

use super::store::{SessionRegistration, SessionStore, StoreError};
use super::time::Timestamp;
use crate::ids::{InteractionId, SessionId, ToolCallId, TurnId};
use crate::model::{AssistantPart, ModelMessage, ModelResponse, ReasoningContent, ToolCall, Usage};
use crate::tools::{ToolOutput, UserAnswer, UserQuestion};

#[path = "conversation_actor.rs"]
mod actor_support;
pub(crate) use actor_support::validate_user_text;
#[path = "conversation_codec.rs"]
mod codec;
#[path = "conversation_compaction.rs"]
mod compaction;
#[path = "conversation_usage.rs"]
mod usage;
pub(crate) use compaction::CompactionConversationView;

const MAX_TEXT_BYTES: usize = 262_144;
const MAX_SUMMARY_BYTES: usize = 65_536;
const MAX_LINE_BYTES: usize = 1_048_576;
const MAX_FILE_BYTES: usize = 1_073_741_824;
const MAX_COMPLETE_ENTRIES: usize = 1_000_000;
const RESTART_CANCELLED_TEXT: &str = "cancelled by restart";

// Keep this crate-private foundation type-checked before the SessionActor slice consumes it.
const _: () = {
    let _ = MAX_TEXT_BYTES;
    let _ = MAX_SUMMARY_BYTES;
    let _ = MAX_LINE_BYTES;
    let _ = MAX_FILE_BYTES;
    let _ = MAX_COMPLETE_ENTRIES;
    let _ = RESTART_CANCELLED_TEXT;
    let _ = std::mem::size_of::<ConversationError>();
    let _ = std::mem::size_of::<ConversationHealth>();
    let _ = std::mem::size_of::<StoredTurnOutcome>();
    let _ = std::mem::size_of::<NewConversationEntry>();
    let _ = std::mem::size_of::<ConversationEntry>();
    let _ = std::mem::size_of::<ConversationState>();
    let _ = std::mem::size_of::<ConversationSnapshot>();
    let _ = std::mem::size_of::<ConversationSummary>();
    let _ = std::mem::size_of::<PromptConversationView>();
    let _ = std::mem::size_of::<CompactionConversationView>();
    let _ = std::mem::size_of::<ConversationInner>();
    let _ = std::mem::size_of::<ConversationLifecycle>();
    let _ = std::mem::size_of::<ConversationLog>();
    let _ = ConversationState::empty;
    let _ = ConversationState::apply;
    let _ = ConversationState::snapshot;
    let _ = ConversationState::prompt_view;
    let _ = NewConversationEntry::assistant_from_response;
    let _ = NewConversationEntry::validate;
    let _ = NewConversationEntry::into_entry;
    let _ = ConversationEntry::validate_shape;
    let _ = ConversationEntry::seq;
    let _ = ConversationLog::open;
    let _ = ConversationLog::append;
    let _ = ConversationLog::snapshot;
    let _ = ConversationLog::usage;
    let _ = ConversationLog::prompt_view;
    let _ = ConversationLog::compaction_view;
    let _ = ConversationLog::append_summary;
    let _ = CompactionConversationView::latest_summary;
    let _ = CompactionConversationView::completed_messages;
    let _ = CompactionConversationView::current_turn_messages;
    let _ = CompactionConversationView::through_seq;
    let _ = CompactionConversationView::snapshot_seq;
    let _ = ConversationLog::close;
    let _ = ConversationLog::wait_idle;
    let _ = ConversationSnapshot::entries;
    let _ = ConversationSnapshot::max_seq;
    let _ = ConversationSnapshot::health;
    let _ = ConversationSummary::text;
    let _ = PromptConversationView::messages;
    let _ = PromptConversationView::latest_summary;
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ConversationError {
    #[error("conversation entry is invalid")]
    InvalidEntry,
    #[error("conversation data is corrupt")]
    Corrupt,
    #[error("conversation data is corrupt")]
    CorruptAt { line: u64, offset: u64 },
    #[error("conversation data is too large")]
    TooLarge,
    #[error("conversation operation is busy")]
    Busy,
    #[error("conversation is closing")]
    Closing,
    #[error("conversation I/O failed")]
    Io,
    #[error("conversation worker failed")]
    WorkerFailed,
    #[error("conversation session was not found")]
    NotFound,
    #[error("conversation page size is invalid")]
    InvalidPage,
    #[error("conversation is degraded")]
    Degraded,
    #[error("conversation compaction state is incomplete")]
    IncompleteToolExchange,
    #[error("conversation snapshot is stale")]
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConversationHealth {
    Healthy,
    Degraded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredTurnOutcome {
    Completed,
    Failed,
    Cancelled,
    CancelledByRestart,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum NewConversationEntry {
    User {
        turn_id: TurnId,
        timestamp: Timestamp,
        text: String,
    },
    Assistant {
        turn_id: TurnId,
        timestamp: Timestamp,
        text: Option<String>,
        reasoning: Option<ReasoningContent>,
        tool_calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
    ToolResult {
        turn_id: TurnId,
        timestamp: Timestamp,
        call_id: ToolCallId,
        result: ToolOutput,
    },
    Interaction {
        turn_id: TurnId,
        timestamp: Timestamp,
        interaction_id: InteractionId,
        question: UserQuestion,
        answer: UserAnswer,
    },
    Summary {
        timestamp: Timestamp,
        through_seq: u64,
        text: String,
    },
    TurnTerminal {
        turn_id: TurnId,
        timestamp: Timestamp,
        outcome: StoredTurnOutcome,
    },
}

impl fmt::Debug for NewConversationEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NewConversationEntry(<redacted>)")
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum NewConversationEntryWire {
    User {
        turn_id: TurnId,
        timestamp: Timestamp,
        text: String,
    },
    Assistant {
        turn_id: TurnId,
        timestamp: Timestamp,
        text: Option<String>,
        reasoning: Option<ReasoningContent>,
        tool_calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
    ToolResult {
        turn_id: TurnId,
        timestamp: Timestamp,
        call_id: ToolCallId,
        result: ToolOutput,
    },
    Interaction {
        turn_id: TurnId,
        timestamp: Timestamp,
        interaction_id: InteractionId,
        question: UserQuestion,
        answer: UserAnswer,
    },
    Summary {
        timestamp: Timestamp,
        through_seq: u64,
        text: String,
    },
    TurnTerminal {
        turn_id: TurnId,
        timestamp: Timestamp,
        outcome: StoredTurnOutcome,
    },
}

impl<'de> Deserialize<'de> for NewConversationEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = match NewConversationEntryWire::deserialize(deserializer)? {
            NewConversationEntryWire::User {
                turn_id,
                timestamp,
                text,
            } => Self::User {
                turn_id,
                timestamp,
                text,
            },
            NewConversationEntryWire::Assistant {
                turn_id,
                timestamp,
                text,
                reasoning,
                tool_calls,
                usage,
            } => Self::Assistant {
                turn_id,
                timestamp,
                text,
                reasoning,
                tool_calls,
                usage,
            },
            NewConversationEntryWire::ToolResult {
                turn_id,
                timestamp,
                call_id,
                result,
            } => Self::ToolResult {
                turn_id,
                timestamp,
                call_id,
                result,
            },
            NewConversationEntryWire::Interaction {
                turn_id,
                timestamp,
                interaction_id,
                question,
                answer,
            } => Self::Interaction {
                turn_id,
                timestamp,
                interaction_id,
                question,
                answer,
            },
            NewConversationEntryWire::Summary {
                timestamp,
                through_seq,
                text,
            } => Self::Summary {
                timestamp,
                through_seq,
                text,
            },
            NewConversationEntryWire::TurnTerminal {
                turn_id,
                timestamp,
                outcome,
            } => Self::TurnTerminal {
                turn_id,
                timestamp,
                outcome,
            },
        };
        value.validate().map_err(D::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ConversationEntry {
    User {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        text: String,
    },
    Assistant {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        text: Option<String>,
        reasoning: Option<ReasoningContent>,
        tool_calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
    ToolResult {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        call_id: ToolCallId,
        result: ToolOutput,
    },
    Interaction {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        interaction_id: InteractionId,
        question: UserQuestion,
        answer: UserAnswer,
    },
    Summary {
        seq: u64,
        timestamp: Timestamp,
        through_seq: u64,
        text: String,
    },
    TurnTerminal {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        outcome: StoredTurnOutcome,
    },
}

impl fmt::Debug for ConversationEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConversationEntry(<redacted>)")
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ConversationEntryWire {
    User {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        text: String,
    },
    Assistant {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        text: Option<String>,
        reasoning: Option<ReasoningContent>,
        tool_calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
    ToolResult {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        call_id: ToolCallId,
        result: ToolOutput,
    },
    Interaction {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        interaction_id: InteractionId,
        question: UserQuestion,
        answer: UserAnswer,
    },
    Summary {
        seq: u64,
        timestamp: Timestamp,
        through_seq: u64,
        text: String,
    },
    TurnTerminal {
        seq: u64,
        turn_id: TurnId,
        timestamp: Timestamp,
        outcome: StoredTurnOutcome,
    },
}

impl<'de> Deserialize<'de> for ConversationEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = match ConversationEntryWire::deserialize(deserializer)? {
            ConversationEntryWire::User {
                seq,
                turn_id,
                timestamp,
                text,
            } => Self::User {
                seq,
                turn_id,
                timestamp,
                text,
            },
            ConversationEntryWire::Assistant {
                seq,
                turn_id,
                timestamp,
                text,
                reasoning,
                tool_calls,
                usage,
            } => Self::Assistant {
                seq,
                turn_id,
                timestamp,
                text,
                reasoning,
                tool_calls,
                usage,
            },
            ConversationEntryWire::ToolResult {
                seq,
                turn_id,
                timestamp,
                call_id,
                result,
            } => Self::ToolResult {
                seq,
                turn_id,
                timestamp,
                call_id,
                result,
            },
            ConversationEntryWire::Interaction {
                seq,
                turn_id,
                timestamp,
                interaction_id,
                question,
                answer,
            } => Self::Interaction {
                seq,
                turn_id,
                timestamp,
                interaction_id,
                question,
                answer,
            },
            ConversationEntryWire::Summary {
                seq,
                timestamp,
                through_seq,
                text,
            } => Self::Summary {
                seq,
                timestamp,
                through_seq,
                text,
            },
            ConversationEntryWire::TurnTerminal {
                seq,
                turn_id,
                timestamp,
                outcome,
            } => Self::TurnTerminal {
                seq,
                turn_id,
                timestamp,
                outcome,
            },
        };
        value.validate_shape().map_err(D::Error::custom)?;
        Ok(value)
    }
}

impl NewConversationEntry {
    pub(crate) fn assistant_from_response(
        turn_id: TurnId,
        timestamp: Timestamp,
        response: &ModelResponse,
    ) -> Result<Self, ConversationError> {
        let mut text = None;
        let mut reasoning = None;
        let mut tool_calls = Vec::new();
        for part in response.parts() {
            match part {
                AssistantPart::Text(value) => {
                    text.get_or_insert_with(String::new).push_str(value);
                }
                AssistantPart::Reasoning(value) => {
                    if reasoning.replace(value.clone()).is_some() {
                        return Err(ConversationError::InvalidEntry);
                    }
                }
                AssistantPart::ToolCall(value) => tool_calls.push(value.clone()),
            }
        }
        let entry = Self::Assistant {
            turn_id,
            timestamp,
            text,
            reasoning,
            tool_calls,
            usage: response.usage().cloned(),
        };
        entry.validate()?;
        Ok(entry)
    }

    fn validate(&self) -> Result<(), ConversationError> {
        match self {
            Self::User { text, .. } => validate_text(text, MAX_TEXT_BYTES),
            Self::Assistant {
                text,
                reasoning,
                tool_calls,
                ..
            } => validate_assistant(text.as_deref(), reasoning.as_ref(), tool_calls),
            Self::ToolResult { result, .. } => {
                if result.text() == RESTART_CANCELLED_TEXT {
                    Err(ConversationError::InvalidEntry)
                } else {
                    Ok(())
                }
            }
            Self::Interaction {
                interaction_id,
                question,
                ..
            } => {
                if question.interaction_id() == *interaction_id {
                    Ok(())
                } else {
                    Err(ConversationError::InvalidEntry)
                }
            }
            Self::Summary {
                through_seq, text, ..
            } => {
                if *through_seq == u64::MAX {
                    return Err(ConversationError::InvalidEntry);
                }
                validate_text(text, MAX_SUMMARY_BYTES)
            }
            Self::TurnTerminal { outcome, .. } => {
                if *outcome == StoredTurnOutcome::CancelledByRestart {
                    Err(ConversationError::InvalidEntry)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn into_entry(self, seq: u64) -> Result<ConversationEntry, ConversationError> {
        if seq == 0 {
            return Err(ConversationError::InvalidEntry);
        }
        self.validate()?;
        Ok(match self {
            Self::User {
                turn_id,
                timestamp,
                text,
            } => ConversationEntry::User {
                seq,
                turn_id,
                timestamp,
                text,
            },
            Self::Assistant {
                turn_id,
                timestamp,
                text,
                reasoning,
                tool_calls,
                usage,
            } => ConversationEntry::Assistant {
                seq,
                turn_id,
                timestamp,
                text,
                reasoning,
                tool_calls,
                usage,
            },
            Self::ToolResult {
                turn_id,
                timestamp,
                call_id,
                result,
            } => ConversationEntry::ToolResult {
                seq,
                turn_id,
                timestamp,
                call_id,
                result,
            },
            Self::Interaction {
                turn_id,
                timestamp,
                interaction_id,
                question,
                answer,
            } => ConversationEntry::Interaction {
                seq,
                turn_id,
                timestamp,
                interaction_id,
                question,
                answer,
            },
            Self::Summary {
                timestamp,
                through_seq,
                text,
            } => ConversationEntry::Summary {
                seq,
                timestamp,
                through_seq,
                text,
            },
            Self::TurnTerminal {
                turn_id,
                timestamp,
                outcome,
            } => ConversationEntry::TurnTerminal {
                seq,
                turn_id,
                timestamp,
                outcome,
            },
        })
    }
}

impl ConversationEntry {
    fn validate_shape(&self) -> Result<(), ConversationError> {
        if self.seq() == 0 {
            return Err(ConversationError::InvalidEntry);
        }
        match self {
            Self::User { text, .. } => validate_text(text, MAX_TEXT_BYTES),
            Self::Assistant {
                text,
                reasoning,
                tool_calls,
                ..
            } => validate_assistant(text.as_deref(), reasoning.as_ref(), tool_calls),
            Self::ToolResult { result, .. } => {
                if result.text() == RESTART_CANCELLED_TEXT && !result.is_error() {
                    Err(ConversationError::InvalidEntry)
                } else {
                    Ok(())
                }
            }
            Self::Interaction {
                interaction_id,
                question,
                ..
            } => {
                if question.interaction_id() == *interaction_id {
                    Ok(())
                } else {
                    Err(ConversationError::InvalidEntry)
                }
            }
            Self::TurnTerminal { .. } => Ok(()),
            Self::Summary {
                seq,
                through_seq,
                text,
                ..
            } => {
                if through_seq >= seq {
                    return Err(ConversationError::InvalidEntry);
                }
                validate_text(text, MAX_SUMMARY_BYTES)
            }
        }
    }

    pub(crate) fn seq(&self) -> u64 {
        match self {
            Self::User { seq, .. }
            | Self::Assistant { seq, .. }
            | Self::ToolResult { seq, .. }
            | Self::Interaction { seq, .. }
            | Self::Summary { seq, .. }
            | Self::TurnTerminal { seq, .. } => *seq,
        }
    }
}

fn validate_assistant(
    text: Option<&str>,
    reasoning: Option<&ReasoningContent>,
    tool_calls: &[ToolCall],
) -> Result<(), ConversationError> {
    if let Some(text) = text {
        validate_text(text, MAX_TEXT_BYTES)?;
    }
    if let Some(reasoning) = reasoning {
        if reasoning.text().is_none()
            && reasoning.summary().is_none()
            && reasoning.encrypted().is_none()
            && reasoning.signature().is_none()
        {
            return Err(ConversationError::InvalidEntry);
        }
    }
    let mut ids = BTreeSet::new();
    let mut expected_index = 0_u32;
    for call in tool_calls {
        call.validate()
            .map_err(|_| ConversationError::InvalidEntry)?;
        if call.call_index() != expected_index || !ids.insert(call.tool_call_id().clone()) {
            return Err(ConversationError::InvalidEntry);
        }
        expected_index = expected_index
            .checked_add(1)
            .ok_or(ConversationError::InvalidEntry)?;
    }
    if text.is_none() && reasoning.is_none() && tool_calls.is_empty() {
        return Err(ConversationError::InvalidEntry);
    }
    Ok(())
}

fn validate_text(value: &str, maximum: usize) -> Result<(), ConversationError> {
    if value.is_empty()
        || value.len() > maximum
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        Err(ConversationError::InvalidEntry)
    } else {
        Ok(())
    }
}

#[derive(Clone)]
struct OutstandingTools {
    turn_id: TurnId,
    timestamp: Timestamp,
    calls: Vec<ToolCall>,
    resolved: BTreeSet<ToolCallId>,
}

#[derive(Clone)]
struct PendingRestartTerminal {
    turn_id: TurnId,
    timestamp: Timestamp,
}

#[derive(Clone)]
struct SummaryBoundary {
    through_seq: u64,
    text: String,
}

#[derive(Clone)]
struct ConversationState {
    entries: Vec<Arc<ConversationEntry>>,
    max_seq: u64,
    latest_terminal_seq: Option<u64>,
    complete_entries: usize,
    outstanding_tools: Option<OutstandingTools>,
    pending_restart_terminal: Option<PendingRestartTerminal>,
    terminal_turns: BTreeSet<TurnId>,
    boundaries: BTreeSet<u64>,
    latest_summary: Option<SummaryBoundary>,
    health: ConversationHealth,
    partial_tail: bool,
}

impl ConversationState {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            max_seq: 0,
            latest_terminal_seq: None,
            complete_entries: 0,
            outstanding_tools: None,
            pending_restart_terminal: None,
            terminal_turns: BTreeSet::new(),
            boundaries: BTreeSet::from([0]),
            latest_summary: None,
            health: ConversationHealth::Healthy,
            partial_tail: false,
        }
    }

    fn apply(&mut self, entry: Arc<ConversationEntry>) -> Result<(), ConversationError> {
        entry.validate_shape()?;
        if self.complete_entries >= MAX_COMPLETE_ENTRIES {
            return Err(ConversationError::TooLarge);
        }
        if (self.entries.is_empty() && entry.seq() != 1)
            || (!self.entries.is_empty() && entry.seq() <= self.max_seq)
        {
            return Err(ConversationError::Corrupt);
        }

        let mut outstanding_tools = self.outstanding_tools.clone();
        let mut pending_restart_terminal = self.pending_restart_terminal.clone();
        let mut terminal_turns = self.terminal_turns.clone();
        let mut latest_summary = self.latest_summary.clone();
        let mut latest_terminal_seq = self.latest_terminal_seq;
        if let Some(pending) = pending_restart_terminal.as_ref() {
            match entry.as_ref() {
                ConversationEntry::ToolResult {
                    turn_id, result, ..
                } if *turn_id == pending.turn_id
                    && result.is_error()
                    && result.text() == RESTART_CANCELLED_TEXT
                    && outstanding_tools.is_some() => {}
                ConversationEntry::TurnTerminal {
                    turn_id,
                    outcome: StoredTurnOutcome::CancelledByRestart,
                    ..
                } if *turn_id == pending.turn_id && outstanding_tools.is_none() => {}
                _ => return Err(ConversationError::Corrupt),
            }
        }

        match entry.as_ref() {
            ConversationEntry::User { turn_id, .. }
            | ConversationEntry::Assistant { turn_id, .. }
            | ConversationEntry::ToolResult { turn_id, .. }
            | ConversationEntry::Interaction { turn_id, .. }
            | ConversationEntry::TurnTerminal { turn_id, .. }
                if terminal_turns.contains(turn_id) =>
            {
                return Err(ConversationError::Corrupt);
            }
            _ => {}
        }

        match entry.as_ref() {
            ConversationEntry::User { .. } => {
                if outstanding_tools.is_some() || pending_restart_terminal.is_some() {
                    return Err(ConversationError::Corrupt);
                }
            }
            ConversationEntry::Assistant {
                turn_id,
                timestamp,
                tool_calls,
                ..
            } => {
                if outstanding_tools.is_some() || pending_restart_terminal.is_some() {
                    return Err(ConversationError::Corrupt);
                }
                if !tool_calls.is_empty() {
                    outstanding_tools = Some(OutstandingTools {
                        turn_id: *turn_id,
                        timestamp: timestamp.clone(),
                        calls: tool_calls.clone(),
                        resolved: BTreeSet::new(),
                    });
                }
            }
            ConversationEntry::ToolResult {
                turn_id,
                call_id,
                result,
                ..
            } => {
                let Some(mut exchange) = outstanding_tools.take() else {
                    return Err(ConversationError::Corrupt);
                };
                if exchange.turn_id != *turn_id
                    || !exchange
                        .calls
                        .iter()
                        .any(|call| call.tool_call_id() == call_id)
                    || !exchange.resolved.insert(call_id.clone())
                {
                    return Err(ConversationError::Corrupt);
                }
                if result.text() == RESTART_CANCELLED_TEXT {
                    if !result.is_error() {
                        return Err(ConversationError::Corrupt);
                    }
                    pending_restart_terminal.get_or_insert(PendingRestartTerminal {
                        turn_id: exchange.turn_id,
                        timestamp: exchange.timestamp.clone(),
                    });
                } else if pending_restart_terminal.is_some() {
                    return Err(ConversationError::Corrupt);
                }
                if exchange.resolved.len() != exchange.calls.len() {
                    outstanding_tools = Some(exchange);
                }
            }
            ConversationEntry::Interaction { turn_id, .. } => {
                if pending_restart_terminal.is_some()
                    || outstanding_tools
                        .as_ref()
                        .is_some_and(|exchange| exchange.turn_id != *turn_id)
                {
                    return Err(ConversationError::Corrupt);
                }
            }
            ConversationEntry::Summary {
                through_seq, text, ..
            } => {
                if outstanding_tools.is_some()
                    || pending_restart_terminal.is_some()
                    || (*through_seq != 0 && !self.boundaries.contains(through_seq))
                    || latest_summary
                        .as_ref()
                        .is_some_and(|summary| *through_seq <= summary.through_seq)
                {
                    return Err(ConversationError::Corrupt);
                }
                latest_summary = Some(SummaryBoundary {
                    through_seq: *through_seq,
                    text: text.clone(),
                });
            }
            ConversationEntry::TurnTerminal {
                turn_id, outcome, ..
            } => {
                if *outcome == StoredTurnOutcome::CancelledByRestart {
                    if pending_restart_terminal
                        .as_ref()
                        .is_none_or(|pending| pending.turn_id != *turn_id)
                        || outstanding_tools.is_some()
                    {
                        return Err(ConversationError::Corrupt);
                    }
                    pending_restart_terminal = None;
                } else if outstanding_tools.is_some() || pending_restart_terminal.is_some() {
                    return Err(ConversationError::Corrupt);
                }
                if !terminal_turns.insert(*turn_id) {
                    return Err(ConversationError::Corrupt);
                }
                latest_terminal_seq = Some(entry.seq());
            }
        }

        self.max_seq = entry.seq();
        self.latest_terminal_seq = latest_terminal_seq;
        self.complete_entries += 1;
        self.entries.push(entry);
        self.outstanding_tools = outstanding_tools;
        self.pending_restart_terminal = pending_restart_terminal;
        self.terminal_turns = terminal_turns;
        self.latest_summary = latest_summary;
        if self.outstanding_tools.is_none() && self.pending_restart_terminal.is_none() {
            self.boundaries.insert(self.max_seq);
        }
        Ok(())
    }

    fn snapshot(&self) -> ConversationSnapshot {
        ConversationSnapshot {
            entries: Arc::from(self.entries.clone().into_boxed_slice()),
            max_seq: self.max_seq,
            health: self.health,
        }
    }

    fn prompt_view(&self) -> Result<PromptConversationView, ConversationError> {
        let through_seq = self
            .latest_summary
            .as_ref()
            .map_or(0, |summary| summary.through_seq);
        let mut messages = Vec::new();
        let mut pending_calls = Vec::<ToolCallId>::new();
        let mut pending_results = BTreeMap::<ToolCallId, ToolOutput>::new();
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.seq() > through_seq)
        {
            match entry.as_ref() {
                ConversationEntry::User { text, .. } => {
                    messages.push(
                        ModelMessage::user(text.clone()).map_err(|_| ConversationError::Corrupt)?,
                    );
                }
                ConversationEntry::Assistant {
                    text,
                    reasoning,
                    tool_calls,
                    ..
                } => {
                    if !pending_calls.is_empty() {
                        return Err(ConversationError::Corrupt);
                    }
                    let mut parts = Vec::new();
                    if let Some(reasoning) = reasoning {
                        parts.push(AssistantPart::Reasoning(reasoning.clone()));
                    }
                    if let Some(text) = text {
                        parts.push(AssistantPart::Text(text.clone()));
                    }
                    parts.extend(tool_calls.iter().cloned().map(AssistantPart::ToolCall));
                    messages.push(
                        ModelMessage::assistant(parts).map_err(|_| ConversationError::Corrupt)?,
                    );
                    pending_calls = tool_calls
                        .iter()
                        .map(|call| call.tool_call_id().clone())
                        .collect();
                }
                ConversationEntry::ToolResult {
                    call_id, result, ..
                } => {
                    pending_results.insert(call_id.clone(), result.clone());
                    while let Some(call_id) = pending_calls.first().cloned() {
                        let Some(output) = pending_results.remove(&call_id) else {
                            break;
                        };
                        messages.push(
                            ModelMessage::tool(call_id, output)
                                .map_err(|_| ConversationError::Corrupt)?,
                        );
                        pending_calls.remove(0);
                    }
                }
                ConversationEntry::Interaction { .. }
                | ConversationEntry::Summary { .. }
                | ConversationEntry::TurnTerminal { .. } => {}
            }
        }
        if !pending_calls.is_empty() || !pending_results.is_empty() {
            return Err(ConversationError::Corrupt);
        }
        Ok(PromptConversationView {
            messages: Arc::from(messages.into_boxed_slice()),
            latest_summary: self
                .latest_summary
                .as_ref()
                .map(|summary| ConversationSummary {
                    through_seq: summary.through_seq,
                    text: summary.text.clone(),
                }),
        })
    }
}

#[derive(Clone)]
pub(crate) struct ConversationSnapshot {
    entries: Arc<[Arc<ConversationEntry>]>,
    max_seq: u64,
    health: ConversationHealth,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConversationSummary {
    through_seq: u64,
    text: String,
}

impl ConversationSummary {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

pub(crate) struct PromptConversationView {
    messages: Arc<[ModelMessage]>,
    latest_summary: Option<ConversationSummary>,
}

impl PromptConversationView {
    pub(crate) fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    pub(crate) fn latest_summary(&self) -> Option<&ConversationSummary> {
        self.latest_summary.as_ref()
    }
}

struct ConversationInner {
    id: SessionId,
    store: SessionStore,
    path: PathBuf,
    state: RwLock<ConversationState>,
    lifecycle: Mutex<ConversationLifecycle>,
    notify: Notify,
    registration: Mutex<Option<SessionRegistration>>,
}

struct ConversationLifecycle {
    closing: bool,
    busy: bool,
}

pub(crate) struct ConversationLog {
    inner: Arc<ConversationInner>,
}

impl ConversationLog {
    pub(crate) async fn open(
        store: &SessionStore,
        id: SessionId,
    ) -> Result<Self, ConversationError> {
        let registration = store
            .open_registration(id)
            .await
            .map_err(ConversationError::from)?;
        let path = store.conversation_path(id);
        let job = store.run_io(move || open_sync(path, registration));
        let (state, registration) = SessionStore::await_io(job)
            .await
            .map_err(ConversationError::from)?;
        Ok(Self {
            inner: Arc::new(ConversationInner {
                id,
                store: store.clone(),
                path: store.conversation_path(id),
                state: RwLock::new(state),
                lifecycle: Mutex::new(ConversationLifecycle {
                    closing: false,
                    busy: false,
                }),
                notify: Notify::new(),
                registration: Mutex::new(Some(registration)),
            }),
        })
    }

    pub(crate) async fn append(
        &self,
        entry: NewConversationEntry,
    ) -> Result<u64, ConversationError> {
        let (reservation, candidate, line, projected) = prepare_append(&self.inner, entry)?;
        compaction::submit_append(&self.inner, reservation, candidate, line, projected).await
    }

    pub(crate) async fn snapshot(&self) -> ConversationSnapshot {
        read_lock(&self.inner.state).snapshot()
    }

    pub(crate) async fn prompt_view(&self) -> Result<PromptConversationView, ConversationError> {
        read_lock(&self.inner.state).prompt_view()
    }

    pub(crate) async fn wait_idle(&self) {
        loop {
            let notified = self.inner.notify.notified();
            let busy = lock_mutex(&self.inner.lifecycle).busy;
            if !busy {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn close(&self) -> Result<(), ConversationError> {
        request_close(&self.inner);
        self.wait_idle().await;
        release_registration(&self.inner);
        Ok(())
    }
}

impl Drop for ConversationLog {
    fn drop(&mut self) {
        request_close(&self.inner);
    }
}

struct AppendJobState {
    started: AtomicBool,
    admitted: AtomicBool,
    finished: AtomicBool,
}

struct AppendSettlement {
    inner: Arc<ConversationInner>,
    projected: ConversationState,
    seq: u64,
    job_state: Arc<AppendJobState>,
}

impl AppendSettlement {
    fn settle(self, write_result: Result<(), StoreError>) -> Result<u64, StoreError> {
        let success = write_result.is_ok();
        {
            let mut state = write_lock(&self.inner.state);
            if success {
                *state = self.projected.clone();
            } else {
                state.health = ConversationHealth::Degraded;
            }
        }
        self.job_state.finished.store(true, Ordering::Release);
        clear_busy(&self.inner);
        write_result.map(|()| self.seq)
    }
}

impl Drop for AppendSettlement {
    fn drop(&mut self) {
        if self.job_state.finished.load(Ordering::Acquire)
            || (!self.job_state.started.load(Ordering::Acquire)
                && !self.job_state.admitted.load(Ordering::Acquire))
        {
            return;
        }
        let mut state = write_lock(&self.inner.state);
        state.health = ConversationHealth::Degraded;
        drop(state);
        self.job_state.finished.store(true, Ordering::Release);
        clear_busy(&self.inner);
    }
}

struct BusyReservation {
    inner: Arc<ConversationInner>,
    active: bool,
}

impl BusyReservation {
    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for BusyReservation {
    fn drop(&mut self) {
        if self.active {
            clear_busy(&self.inner);
        }
    }
}

fn prepare_append(
    inner: &Arc<ConversationInner>,
    entry: NewConversationEntry,
) -> Result<
    (
        BusyReservation,
        Arc<ConversationEntry>,
        Vec<u8>,
        ConversationState,
    ),
    ConversationError,
> {
    if matches!(&entry, NewConversationEntry::Summary { .. }) {
        return Err(ConversationError::InvalidEntry);
    }
    let (reservation, state) = reserve_append_slot(inner)?;
    let candidate = Arc::new(
        entry.into_entry(
            state
                .max_seq
                .checked_add(1)
                .ok_or(ConversationError::Corrupt)?,
        )?,
    );
    let mut projected = state.clone();
    projected.apply(Arc::clone(&candidate))?;
    let line = encode_candidate(&candidate)?;
    Ok((reservation, candidate, line, projected))
}

fn reserve_append_slot(
    inner: &Arc<ConversationInner>,
) -> Result<(BusyReservation, ConversationState), ConversationError> {
    if read_lock(&inner.state).health == ConversationHealth::Degraded {
        return Err(ConversationError::Degraded);
    }
    let reservation = {
        let mut lifecycle = lock_mutex(&inner.lifecycle);
        if lifecycle.closing {
            return Err(ConversationError::Closing);
        }
        if lifecycle.busy {
            return Err(ConversationError::Busy);
        }
        lifecycle.busy = true;
        BusyReservation {
            inner: Arc::clone(inner),
            active: true,
        }
    };
    let state = read_lock(&inner.state);
    if state.health == ConversationHealth::Degraded {
        return Err(ConversationError::Degraded);
    }
    let snapshot = state.clone();
    drop(state);
    Ok((reservation, snapshot))
}

fn encode_candidate(candidate: &ConversationEntry) -> Result<Vec<u8>, ConversationError> {
    let mut line = serde_json::to_vec(candidate).map_err(|_| ConversationError::InvalidEntry)?;
    line.push(b'\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(ConversationError::TooLarge);
    }
    Ok(line)
}

fn request_close(inner: &Arc<ConversationInner>) {
    let release = {
        let mut lifecycle = lock_mutex(&inner.lifecycle);
        lifecycle.closing = true;
        !lifecycle.busy
    };
    if release {
        release_registration(inner);
    }
}

fn clear_busy(inner: &Arc<ConversationInner>) {
    let release = {
        let mut lifecycle = lock_mutex(&inner.lifecycle);
        lifecycle.busy = false;
        lifecycle.closing
    };
    inner.notify.notify_waiters();
    if release {
        release_registration(inner);
    }
}

fn release_registration(inner: &Arc<ConversationInner>) {
    let registration = lock_mutex(&inner.registration).take();
    drop(registration);
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_mutex<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_sync(
    path: PathBuf,
    registration: SessionRegistration,
) -> Result<(ConversationState, SessionRegistration), StoreError> {
    let state = codec::read_replay_file(&path)?;
    let mut projected = state.clone();
    let mut repair_entries = Vec::new();
    let repair_line = state.complete_entries as u64 + 1;
    let repair_offset = codec::file_len(&path)?;
    if let Some(exchange) = state.outstanding_tools.clone() {
        for call in exchange
            .calls
            .iter()
            .filter(|call| !exchange.resolved.contains(call.tool_call_id()))
        {
            let result = ToolOutput::failure(RESTART_CANCELLED_TEXT).map_err(|_| {
                StoreError::ConversationCorrupt {
                    line: repair_line,
                    offset: repair_offset,
                }
            })?;
            let entry = Arc::new(ConversationEntry::ToolResult {
                seq: projected
                    .max_seq
                    .checked_add(1)
                    .ok_or(StoreError::ConversationCorrupt {
                        line: repair_line,
                        offset: repair_offset,
                    })?,
                turn_id: exchange.turn_id,
                timestamp: exchange.timestamp.clone(),
                call_id: call.tool_call_id().clone(),
                result,
            });
            projected
                .apply(Arc::clone(&entry))
                .map_err(|error| codec::map_state_error(error, repair_line, repair_offset))?;
            repair_entries.push(entry);
        }
    }
    let terminal = state.pending_restart_terminal.clone().or_else(|| {
        state
            .outstanding_tools
            .as_ref()
            .map(|exchange| PendingRestartTerminal {
                turn_id: exchange.turn_id,
                timestamp: exchange.timestamp.clone(),
            })
    });
    if let Some(terminal) = terminal {
        let entry = Arc::new(ConversationEntry::TurnTerminal {
            seq: projected
                .max_seq
                .checked_add(1)
                .ok_or(StoreError::ConversationCorrupt {
                    line: repair_line,
                    offset: repair_offset,
                })?,
            turn_id: terminal.turn_id,
            timestamp: terminal.timestamp,
            outcome: StoredTurnOutcome::CancelledByRestart,
        });
        projected
            .apply(Arc::clone(&entry))
            .map_err(|error| codec::map_state_error(error, repair_line, repair_offset))?;
        repair_entries.push(entry);
    }
    if !repair_entries.is_empty() {
        let mut bytes = Vec::new();
        for entry in &repair_entries {
            bytes.extend(codec::encode_line(entry, repair_line, repair_offset)?);
        }
        codec::append_bytes_sync(&path, &bytes)?;
        return Ok((projected, registration));
    }
    Ok((state, registration))
}

#[cfg(test)]
pub(crate) use tests::wait_until_busy_for_test;

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::future::Future;
    use std::sync::mpsc::channel;
    use std::task::{Context, Poll};

    use serde_json::json;

    use super::*;
    use crate::model::{ModelFinishReason, ModelMessage, ModelSelection, ReasoningContent, Usage};
    use crate::session::store::{
        StoredCompactionConfig, StoredExecutionConfig, StoredModelConfig, StoredSessionConfig,
    };
    use crate::tools::ToolName;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "minicore-p4-conversation-{}",
            SessionId::new().unwrap()
        ))
    }

    fn timestamp() -> Timestamp {
        "2026-08-19T12:34:56.789Z".parse().unwrap()
    }

    fn config(id: SessionId) -> StoredSessionConfig {
        let model = StoredModelConfig::new(ModelSelection::new(
            "anthropic".parse().unwrap(),
            "claude".parse().unwrap(),
        ));
        let execution = StoredExecutionConfig::new(
            BTreeSet::<ToolName>::new(),
            StoredCompactionConfig::new(100, 50).unwrap(),
            4,
        )
        .unwrap();
        StoredSessionConfig::new(
            id,
            timestamp(),
            timestamp(),
            PathBuf::from("/tmp/workspace"),
            model,
            "system".to_owned(),
            execution,
        )
        .unwrap()
    }

    pub(crate) async fn wait_until_busy_for_test(log: &ConversationLog) {
        loop {
            if lock_mutex(&log.inner.lifecycle).busy {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    async fn opened() -> (SessionStore, ConversationLog, PathBuf, SessionId) {
        let root = root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        store.create(&config(id)).await.unwrap();
        let log = ConversationLog::open(&store, id).await.unwrap();
        (store, log, root, id)
    }

    async fn cleanup(store: &SessionStore, log: &ConversationLog, root: PathBuf) {
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    async fn expect_open_error(bytes: Vec<u8>, expected: ConversationError) {
        let root = root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        store.create(&config(id)).await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        fs::write(&file, bytes).unwrap();
        let original = fs::read(&file).unwrap();
        let actual = match ConversationLog::open(&store, id).await {
            Err(error) => error,
            Ok(_) => panic!("conversation unexpectedly opened"),
        };
        assert_eq!(actual, expected);
        assert_eq!(fs::read(&file).unwrap(), original);
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    async fn expect_sparse_file_error(expected: ConversationError) {
        let root = root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        store.create(&config(id)).await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let sparse = OpenOptions::new().write(true).open(&file).unwrap();
        sparse
            .set_len((MAX_FILE_BYTES as u64).checked_add(1).unwrap())
            .unwrap();
        assert!(matches!(
            ConversationLog::open(&store, id).await,
            Err(error) if error == expected
        ));
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn call(index: u32, id: &str) -> ToolCall {
        ToolCall::new(
            ToolCallId::new(id).unwrap(),
            "read_file".parse().unwrap(),
            json!({"path": "file.txt"}),
            index,
        )
        .unwrap()
    }

    fn response(parts: Vec<AssistantPart>, usage: Option<Usage>) -> ModelResponse {
        ModelResponse::new(
            parts,
            if usage.is_some() {
                ModelFinishReason::Stop
            } else {
                ModelFinishReason::ToolCalls
            },
            usage,
        )
        .unwrap()
    }

    fn assistant(turn_id: TurnId, response: ModelResponse) -> NewConversationEntry {
        NewConversationEntry::assistant_from_response(turn_id, timestamp(), &response).unwrap()
    }

    fn user(turn_id: TurnId, text: &str) -> NewConversationEntry {
        NewConversationEntry::User {
            turn_id,
            timestamp: timestamp(),
            text: text.to_owned(),
        }
    }

    fn terminal(turn_id: TurnId, outcome: StoredTurnOutcome) -> NewConversationEntry {
        NewConversationEntry::TurnTerminal {
            turn_id,
            timestamp: timestamp(),
            outcome,
        }
    }

    fn encoded(entry: &ConversationEntry) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(entry).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn entries_are_checked_compact_snake_case_and_round_trip() {
        let turn_id = TurnId::new().unwrap();
        let entry = ConversationEntry::User {
            seq: 1,
            turn_id,
            timestamp: timestamp(),
            text: "hello".to_owned(),
        };
        let json_text = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            json_text,
            format!(
                "{{\"type\":\"user\",\"seq\":1,\"turn_id\":\"{}\",\"timestamp\":\"2026-08-19T12:34:56.789Z\",\"text\":\"hello\"}}",
                turn_id
            )
        );
        assert_eq!(
            serde_json::from_str::<ConversationEntry>(&json_text).unwrap(),
            entry
        );
        let mut unknown = serde_json::to_value(&entry).unwrap();
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ConversationEntry>(unknown).is_err());
        assert!(
            serde_json::from_value::<ConversationEntry>(json!({
                "type": "user",
                "seq": 0,
                "turn_id": turn_id,
                "timestamp": "2026-08-19T12:34:56.789Z",
                "text": "hello"
            }))
            .is_err()
        );

        let reasoning =
            ReasoningContent::new(Some("thinking".to_owned()), None, None, None, None).unwrap();
        let assistant = assistant(
            turn_id,
            response(
                vec![
                    AssistantPart::Reasoning(reasoning),
                    AssistantPart::Text("done".to_owned()),
                ],
                Some(Usage::new(1, 2, 3)),
            ),
        )
        .into_entry(2)
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<ConversationEntry>(&serde_json::to_vec(&assistant).unwrap())
                .unwrap(),
            assistant
        );
    }

    #[test]
    fn assistant_response_is_flattened_into_the_exact_persisted_shape() {
        let turn_id = TurnId::new().unwrap();
        let reasoning =
            ReasoningContent::new(Some("thinking".to_owned()), None, None, None, None).unwrap();
        let assistant_response = response(
            vec![
                AssistantPart::Text("first".to_owned()),
                AssistantPart::Reasoning(reasoning.clone()),
                AssistantPart::Text("second".to_owned()),
                AssistantPart::ToolCall(call(0, "call-a")),
            ],
            Some(Usage::new(1, 2, 3)),
        );
        let new = NewConversationEntry::assistant_from_response(
            turn_id,
            timestamp(),
            &assistant_response,
        )
        .unwrap();
        let entry = new.into_entry(1).unwrap();
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["type"], "assistant");
        assert_eq!(value["text"], "firstsecond");
        assert_eq!(value["reasoning"], serde_json::to_value(reasoning).unwrap());
        assert_eq!(value["tool_calls"].as_array().unwrap().len(), 1);
        assert!(value.get("response").is_none());
        assert!(value.get("finish_reason").is_none());
        assert!(value.get("provider").is_none());
        assert_eq!(value["usage"]["input_tokens"], 1);

        let duplicate_reasoning = response(
            vec![
                AssistantPart::Reasoning(
                    ReasoningContent::new(Some("one".to_owned()), None, None, None, None).unwrap(),
                ),
                AssistantPart::Reasoning(
                    ReasoningContent::new(Some("two".to_owned()), None, None, None, None).unwrap(),
                ),
            ],
            None,
        );
        assert!(
            NewConversationEntry::assistant_from_response(
                turn_id,
                timestamp(),
                &duplicate_reasoning
            )
            .is_err()
        );
    }

    #[test]
    fn tool_result_and_interaction_use_exact_persisted_field_names() {
        let turn_id = TurnId::new().unwrap();
        let call_id = ToolCallId::new("call-a").unwrap();
        let tool = NewConversationEntry::ToolResult {
            turn_id,
            timestamp: timestamp(),
            call_id: call_id.clone(),
            result: ToolOutput::success("ok").unwrap(),
        }
        .into_entry(1)
        .unwrap();
        let tool_value = serde_json::to_value(tool).unwrap();
        assert!(tool_value.get("call_id").is_some());
        assert!(tool_value.get("result").is_some());
        assert!(tool_value.get("tool_call_id").is_none());
        assert!(tool_value.get("output").is_none());

        let interaction_id = InteractionId::new().unwrap();
        let interaction = NewConversationEntry::Interaction {
            turn_id,
            timestamp: timestamp(),
            interaction_id,
            question: UserQuestion::new(interaction_id, "Choose one", None).unwrap(),
            answer: UserAnswer::new("one").unwrap(),
        }
        .into_entry(2)
        .unwrap();
        let interaction_value = serde_json::to_value(interaction).unwrap();
        assert_eq!(interaction_value["type"], "interaction");
        assert!(interaction_value.get("resolution").is_none());
        assert!(interaction_value.get("question").is_some());
        assert!(interaction_value.get("answer").is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn append_reload_page_and_prompt_view_are_stateless() {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        assert_eq!(log.append(user(turn_id, "hello")).await.unwrap(), 1);
        assert_eq!(
            log.append(assistant(
                turn_id,
                response(vec![AssistantPart::Text("answer".to_owned())], None),
            ))
            .await
            .unwrap(),
            2
        );
        assert_eq!(
            log.append(terminal(turn_id, StoredTurnOutcome::Completed))
                .await
                .unwrap(),
            3
        );
        assert!(matches!(
            log.transcript(None, 0).await,
            Err(ConversationError::InvalidPage)
        ));
        assert!(matches!(
            log.transcript(None, 201).await,
            Err(ConversationError::InvalidPage)
        ));
        let page = log.transcript(None, 2).await.unwrap();
        assert_eq!(page.entries().len(), 2);
        assert_eq!(page.next_after_seq(), Some(2));
        let next = log.transcript(page.next_after_seq(), 2).await.unwrap();
        assert_eq!(next.entries().len(), 1);
        assert_eq!(next.next_after_seq(), None);
        let prompt = log.prompt_view().await.unwrap();
        assert!(prompt.latest_summary().is_none());
        assert_eq!(
            prompt.messages(),
            &[
                ModelMessage::user("hello").unwrap(),
                ModelMessage::assistant(vec![AssistantPart::Text("answer".to_owned())]).unwrap()
            ]
        );
        assert_eq!(log.snapshot().await.max_seq(), 3);
        log.close().await.unwrap();
        store.shutdown().await.unwrap();

        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        assert_eq!(reopened.snapshot().await.max_seq(), 3);
        assert_eq!(
            reopened.snapshot().await.health(),
            ConversationHealth::Healthy
        );
        cleanup(&reopened_store, &reopened, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_view_without_terminal_keeps_all_messages_current() {
        let (store, log, root, _id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        log.append(user(turn_id, "hello")).await.unwrap();
        log.append(assistant(
            turn_id,
            response(vec![AssistantPart::Text("answer".to_owned())], None),
        ))
        .await
        .unwrap();
        let view = log.compaction_view().await.unwrap();
        assert!(view.latest_summary().is_none());
        assert!(view.completed_messages().is_empty());
        assert_eq!(view.through_seq(), None);
        assert_eq!(view.snapshot_seq(), 2);
        assert_eq!(
            view.current_turn_messages(),
            &[
                ModelMessage::user("hello").unwrap(),
                ModelMessage::assistant(vec![AssistantPart::Text("answer".to_owned())]).unwrap(),
            ]
        );
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_view_splits_terminal_history_and_respects_existing_summary() {
        let (store, log, root, _id) = opened().await;
        let first_turn = TurnId::new().unwrap();
        log.append(user(first_turn, "first")).await.unwrap();
        log.append(assistant(
            first_turn,
            response(vec![AssistantPart::Text("done".to_owned())], None),
        ))
        .await
        .unwrap();
        log.append(terminal(first_turn, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        let first_view = log.compaction_view().await.unwrap();
        assert_eq!(first_view.through_seq(), Some(3));
        assert_eq!(first_view.completed_messages().len(), 2);
        assert!(first_view.current_turn_messages().is_empty());

        log.append_summary(3, 3, timestamp(), "old summary".to_owned())
            .await
            .unwrap();
        let second_turn = TurnId::new().unwrap();
        log.append(user(second_turn, "second")).await.unwrap();
        log.append(assistant(
            second_turn,
            response(vec![AssistantPart::Text("later".to_owned())], None),
        ))
        .await
        .unwrap();
        log.append(terminal(second_turn, StoredTurnOutcome::Failed))
            .await
            .unwrap();
        let view = log.compaction_view().await.unwrap();
        assert_eq!(view.latest_summary().unwrap().text(), "old summary");
        assert_eq!(view.latest_summary().unwrap().through_seq, 3);
        assert_eq!(view.through_seq(), Some(7));
        assert_eq!(view.snapshot_seq(), 7);
        assert_eq!(view.completed_messages().len(), 2);
        assert!(view.current_turn_messages().is_empty());
        assert_eq!(
            view.completed_messages(),
            &[
                ModelMessage::user("second").unwrap(),
                ModelMessage::assistant(vec![AssistantPart::Text("later".to_owned())]).unwrap(),
            ]
        );
        let current_turn = TurnId::new().unwrap();
        log.append(user(current_turn, "current")).await.unwrap();
        log.append(assistant(
            current_turn,
            response(vec![AssistantPart::Text("still running".to_owned())], None),
        ))
        .await
        .unwrap();
        let split_view = log.compaction_view().await.unwrap();
        assert_eq!(split_view.through_seq(), Some(7));
        assert_eq!(split_view.snapshot_seq(), 9);
        assert_eq!(split_view.completed_messages().len(), 2);
        assert_eq!(
            split_view.current_turn_messages(),
            &[
                ModelMessage::user("current").unwrap(),
                ModelMessage::assistant(vec![AssistantPart::Text("still running".to_owned())])
                    .unwrap(),
            ]
        );
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_view_refuses_incomplete_tool_exchange() {
        let (store, log, root, _id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        log.append(assistant(
            turn_id,
            response(vec![AssistantPart::ToolCall(call(0, "pending"))], None),
        ))
        .await
        .unwrap();
        assert!(matches!(
            log.compaction_view().await,
            Err(ConversationError::IncompleteToolExchange)
        ));
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn generic_summary_append_is_rejected_without_changing_current_turn() {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        log.append(user(turn_id, "current input")).await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let before_bytes = fs::read(&file).unwrap();
        let before = log.snapshot().await;
        assert_eq!(
            log.append(NewConversationEntry::Summary {
                timestamp: timestamp(),
                through_seq: 0,
                text: "not through append_summary".to_owned(),
            })
            .await,
            Err(ConversationError::InvalidEntry)
        );
        assert_eq!(fs::read(&file).unwrap(), before_bytes);
        let after = log.snapshot().await;
        assert_eq!(after.max_seq(), before.max_seq());
        assert_eq!(after.entries(), before.entries());
        assert_eq!(after.health(), ConversationHealth::Healthy);
        let view = log.compaction_view().await.unwrap();
        assert!(view.completed_messages().is_empty());
        assert_eq!(view.through_seq(), None);
        assert_eq!(view.snapshot_seq(), 1);
        assert_eq!(
            view.current_turn_messages(),
            &[ModelMessage::user("current input").unwrap()]
        );
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_view_orders_tool_results_by_call_index() {
        let (store, log, root, _id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let first = call(0, "first");
        let second = call(1, "second");
        log.append(assistant(
            turn_id,
            response(
                vec![
                    AssistantPart::ToolCall(first.clone()),
                    AssistantPart::ToolCall(second.clone()),
                ],
                None,
            ),
        ))
        .await
        .unwrap();
        log.append(NewConversationEntry::ToolResult {
            turn_id,
            timestamp: timestamp(),
            call_id: second.tool_call_id().clone(),
            result: ToolOutput::success("second").unwrap(),
        })
        .await
        .unwrap();
        log.append(NewConversationEntry::ToolResult {
            turn_id,
            timestamp: timestamp(),
            call_id: first.tool_call_id().clone(),
            result: ToolOutput::success("first").unwrap(),
        })
        .await
        .unwrap();
        log.append(terminal(turn_id, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        let messages = log
            .compaction_view()
            .await
            .unwrap()
            .completed_messages()
            .to_vec();
        assert!(matches!(
            &messages[1],
            ModelMessage::Tool { tool_call_id, .. } if tool_call_id == first.tool_call_id()
        ));
        assert!(matches!(
            &messages[2],
            ModelMessage::Tool { tool_call_id, .. } if tool_call_id == second.tool_call_id()
        ));
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn summary_append_is_stale_safe_and_append_only() {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        log.append(user(turn_id, "input")).await.unwrap();
        log.append(assistant(
            turn_id,
            response(vec![AssistantPart::Text("output".to_owned())], None),
        ))
        .await
        .unwrap();
        log.append(terminal(turn_id, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let before = fs::read(&file).unwrap();
        assert_eq!(
            log.append_summary(2, 3, timestamp(), "summary".to_owned())
                .await,
            Err(ConversationError::Stale)
        );
        assert_eq!(fs::read(&file).unwrap(), before);
        assert_eq!(log.snapshot().await.health(), ConversationHealth::Healthy);
        assert_eq!(
            log.append_summary(3, 2, timestamp(), "summary".to_owned())
                .await,
            Err(ConversationError::Stale)
        );
        assert_eq!(fs::read(&file).unwrap(), before);
        assert_eq!(
            log.append_summary(3, 3, timestamp(), "summary".to_owned())
                .await,
            Ok(4)
        );
        let after = fs::read(&file).unwrap();
        assert!(after.starts_with(&before));
        assert!(after.len() > before.len());
        assert_eq!(log.transcript(None, 10).await.unwrap().entries().len(), 4);
        let prompt = log.prompt_view().await.unwrap();
        assert_eq!(prompt.latest_summary().unwrap().text(), "summary");
        assert!(prompt.messages().is_empty());
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_results_are_ordered_and_restart_repairs_are_durable() {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let first = call(0, "call-a");
        let second = call(1, "call-b");
        log.append(user(turn_id, "read both")).await.unwrap();
        log.append(assistant(
            turn_id,
            response(
                vec![
                    AssistantPart::ToolCall(first.clone()),
                    AssistantPart::ToolCall(second.clone()),
                ],
                None,
            ),
        ))
        .await
        .unwrap();
        log.append(NewConversationEntry::ToolResult {
            turn_id,
            timestamp: timestamp(),
            call_id: second.tool_call_id().clone(),
            result: ToolOutput::success("second").unwrap(),
        })
        .await
        .unwrap();
        log.append(NewConversationEntry::ToolResult {
            turn_id,
            timestamp: timestamp(),
            call_id: first.tool_call_id().clone(),
            result: ToolOutput::success("first").unwrap(),
        })
        .await
        .unwrap();
        let prompt = log.prompt_view().await.unwrap();
        assert!(matches!(
            &prompt.messages()[2],
            ModelMessage::Tool { tool_call_id, .. } if tool_call_id == first.tool_call_id()
        ));
        assert!(matches!(
            &prompt.messages()[3],
            ModelMessage::Tool { tool_call_id, .. } if tool_call_id == second.tool_call_id()
        ));
        assert!(
            log.append(NewConversationEntry::ToolResult {
                turn_id,
                timestamp: timestamp(),
                call_id: first.tool_call_id().clone(),
                result: ToolOutput::success("late").unwrap(),
            })
            .await
            .is_err()
        );
        log.close().await.unwrap();
        store.shutdown().await.unwrap();

        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        assert_eq!(reopened.snapshot().await.max_seq(), 4);
        cleanup(&reopened_store, &reopened, root).await;

        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let repair_call = call(0, "repair");
        log.append(assistant(
            turn_id,
            response(vec![AssistantPart::ToolCall(repair_call.clone())], None),
        ))
        .await
        .unwrap();
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let before = fs::read(&file).unwrap();
        assert!(before.ends_with(b"\n"));

        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let repaired = ConversationLog::open(&reopened_store, id).await.unwrap();
        let snapshot = repaired.snapshot().await;
        assert!(snapshot.entries().iter().any(|entry| matches!(entry.as_ref(), ConversationEntry::ToolResult { result, .. } if result.text() == "cancelled by restart" && result.is_error())));
        assert!(snapshot.entries().iter().any(|entry| matches!(
            entry.as_ref(),
            ConversationEntry::TurnTerminal {
                outcome: StoredTurnOutcome::CancelledByRestart,
                ..
            }
        )));
        let bytes = fs::read(&file).unwrap();
        assert!(bytes.len() > before.len());
        assert!(String::from_utf8_lossy(&bytes).contains("cancelled by restart"));
        cleanup(&reopened_store, &repaired, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_repair_resumes_missing_lines_and_rejects_unrelated_tail() {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let first = call(0, "call-a");
        let second = call(1, "call-b");
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let assistant_entry = assistant(
            turn_id,
            response(
                vec![
                    AssistantPart::ToolCall(first.clone()),
                    AssistantPart::ToolCall(second.clone()),
                ],
                None,
            ),
        )
        .into_entry(1)
        .unwrap();
        let first_repair = ConversationEntry::ToolResult {
            seq: 2,
            turn_id,
            timestamp: timestamp(),
            call_id: first.tool_call_id().clone(),
            result: ToolOutput::failure(RESTART_CANCELLED_TEXT).unwrap(),
        };
        fs::write(
            &file,
            [
                encoded(&assistant_entry),
                encoded(&first_repair),
                b"{\"type\":\"tool_result\"".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let repaired = ConversationLog::open(&reopened_store, id).await.unwrap();
        let snapshot = repaired.snapshot().await;
        assert_eq!(snapshot.max_seq(), 4);
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.as_ref(), ConversationEntry::ToolResult { result, .. } if result.text() == RESTART_CANCELLED_TEXT))
                .count(),
            2
        );
        assert!(matches!(
            snapshot.entries().last().unwrap().as_ref(),
            ConversationEntry::TurnTerminal {
                outcome: StoredTurnOutcome::CancelledByRestart,
                ..
            }
        ));
        cleanup(&reopened_store, &repaired, root).await;

        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let first = call(0, "call-a");
        let second = call(1, "call-b");
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let assistant_entry = assistant(
            turn_id,
            response(
                vec![
                    AssistantPart::ToolCall(first.clone()),
                    AssistantPart::ToolCall(second.clone()),
                ],
                None,
            ),
        )
        .into_entry(1)
        .unwrap();
        let first_repair = ConversationEntry::ToolResult {
            seq: 2,
            turn_id,
            timestamp: timestamp(),
            call_id: first.tool_call_id().clone(),
            result: ToolOutput::failure(RESTART_CANCELLED_TEXT).unwrap(),
        };
        let second_repair = ConversationEntry::ToolResult {
            seq: 3,
            turn_id,
            timestamp: timestamp(),
            call_id: second.tool_call_id().clone(),
            result: ToolOutput::failure(RESTART_CANCELLED_TEXT).unwrap(),
        };
        fs::write(
            &file,
            [
                encoded(&assistant_entry),
                encoded(&first_repair),
                encoded(&second_repair),
            ]
            .concat(),
        )
        .unwrap();
        let before = fs::read(&file).unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let repaired = ConversationLog::open(&reopened_store, id).await.unwrap();
        assert_eq!(repaired.snapshot().await.max_seq(), 4);
        let terminal = encoded(&ConversationEntry::TurnTerminal {
            seq: 4,
            turn_id,
            timestamp: timestamp(),
            outcome: StoredTurnOutcome::CancelledByRestart,
        });
        assert_eq!(
            fs::read(&file).unwrap().len(),
            before.len() + terminal.len()
        );
        cleanup(&reopened_store, &repaired, root).await;

        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let first = call(0, "call-a");
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let assistant_entry = assistant(
            turn_id,
            response(vec![AssistantPart::ToolCall(first.clone())], None),
        )
        .into_entry(1)
        .unwrap();
        let first_repair = ConversationEntry::ToolResult {
            seq: 2,
            turn_id,
            timestamp: timestamp(),
            call_id: first.tool_call_id().clone(),
            result: ToolOutput::failure(RESTART_CANCELLED_TEXT).unwrap(),
        };
        let unrelated = ConversationEntry::User {
            seq: 3,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "unrelated".to_owned(),
        };
        let assistant_line = encoded(&assistant_entry);
        let repair_line = encoded(&first_repair);
        let unrelated_line = encoded(&unrelated);
        fs::write(
            &file,
            [
                assistant_line.as_slice(),
                repair_line.as_slice(),
                unrelated_line.as_slice(),
            ]
            .concat(),
        )
        .unwrap();
        let original = fs::read(&file).unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        assert!(matches!(
            ConversationLog::open(&reopened_store, id).await,
            Err(ConversationError::CorruptAt { line: 3, offset })
                if offset == (assistant_line.len() + repair_line.len()) as u64
        ));
        assert_eq!(fs::read(&file).unwrap(), original);
        reopened_store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interactions_terminals_and_summary_boundaries_are_checked() {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let interaction_id = InteractionId::new().unwrap();
        let question = UserQuestion::new(interaction_id, "Choose one", None).unwrap();
        log.append(user(turn_id, "question")).await.unwrap();
        log.append(NewConversationEntry::Interaction {
            turn_id,
            timestamp: timestamp(),
            interaction_id,
            question: question.clone(),
            answer: UserAnswer::new("one").unwrap(),
        })
        .await
        .unwrap();
        log.append(assistant(
            turn_id,
            response(vec![AssistantPart::Text("done".to_owned())], None),
        ))
        .await
        .unwrap();
        let prompt_before_summary = log.prompt_view().await.unwrap();
        assert_eq!(
            prompt_before_summary.messages(),
            &[
                ModelMessage::user("question").unwrap(),
                ModelMessage::assistant(vec![AssistantPart::Text("done".to_owned())]).unwrap(),
            ]
        );
        log.append(terminal(turn_id, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        assert!(
            log.append(terminal(turn_id, StoredTurnOutcome::Failed))
                .await
                .is_err()
        );

        log.append_summary(4, 4, timestamp(), "summary".to_owned())
            .await
            .unwrap();
        assert!(
            log.append_summary(5, 4, timestamp(), "duplicate boundary".to_owned())
                .await
                .is_err()
        );
        let turn_two = TurnId::new().unwrap();
        log.append(user(turn_two, "after")).await.unwrap();
        let prompt = log.prompt_view().await.unwrap();
        assert_eq!(prompt.latest_summary().unwrap().text(), "summary");
        assert_eq!(prompt.latest_summary().unwrap().through_seq, 4);
        assert_eq!(prompt.messages(), &[ModelMessage::user("after").unwrap()]);
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        assert!(reopened.snapshot().await.entries().iter().any(|entry| {
            matches!(entry.as_ref(), ConversationEntry::Interaction { answer, .. } if answer.text() == "one")
        }));
        assert_eq!(
            reopened.prompt_view().await.unwrap().messages(),
            &[ModelMessage::user("after").unwrap()]
        );
        cleanup(&reopened_store, &reopened, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partial_tail_crlf_and_complete_corruption_follow_strict_replay() {
        let (store, log, root, id) = opened().await;
        let line = encoded(&ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "one".to_owned(),
        });
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        fs::write(
            &file,
            [line.clone(), b"{\"type\":\"user\"}".to_vec()].concat(),
        )
        .unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        assert_eq!(fs::read(&file).unwrap(), line);
        cleanup(&reopened_store, &reopened, root).await;

        let (store, log, root, id) = opened().await;
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let entry = ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "crlf".to_owned(),
        };
        let mut crlf = serde_json::to_vec(&entry).unwrap();
        crlf.extend_from_slice(b"\r\n");
        fs::write(&file, crlf).unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        assert_eq!(reopened.snapshot().await.entries().len(), 1);
        cleanup(&reopened_store, &reopened, root).await;

        let (store, log, root, id) = opened().await;
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let first = encoded(&ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "first".to_owned(),
        });
        fs::write(
            &file,
            [
                first.as_slice(),
                b"{\"type\":\"user\"}\n".as_slice(),
                b"{\"type\":\"user\"".as_slice(),
            ]
            .concat(),
        )
        .unwrap();
        let original = fs::read(&file).unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        assert!(matches!(
            ConversationLog::open(&reopened_store, id).await,
            Err(ConversationError::CorruptAt { line: 2, offset })
                if offset == first.len() as u64
        ));
        assert_eq!(fs::read(&file).unwrap(), original);
        reopened_store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_repairs_tool_exchange_after_interaction_and_keeps_interaction_non_model_visible()
     {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let interaction_id = InteractionId::new().unwrap();
        let question = UserQuestion::new(interaction_id, "Allow tool?", None).unwrap();
        log.append(user(turn_id, "question")).await.unwrap();
        log.append(assistant(
            turn_id,
            response(vec![AssistantPart::ToolCall(call(0, "call-restart"))], None),
        ))
        .await
        .unwrap();
        log.append(NewConversationEntry::Interaction {
            turn_id,
            timestamp: timestamp(),
            interaction_id,
            question,
            answer: UserAnswer::new("no").unwrap(),
        })
        .await
        .unwrap();
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        let snapshot = reopened.snapshot().await;
        assert_eq!(snapshot.entries().len(), 5);
        assert!(snapshot.entries().iter().any(|entry| {
            matches!(entry.as_ref(), ConversationEntry::Interaction { interaction_id: current, .. } if *current == interaction_id)
        }));
        assert!(snapshot.entries().iter().any(|entry| {
            matches!(entry.as_ref(), ConversationEntry::ToolResult { result, .. } if result.text() == RESTART_CANCELLED_TEXT)
        }));
        assert!(snapshot.entries().iter().any(|entry| {
            matches!(
                entry.as_ref(),
                ConversationEntry::TurnTerminal {
                    outcome: StoredTurnOutcome::CancelledByRestart,
                    ..
                }
            )
        }));
        let prompt = reopened.prompt_view().await.unwrap();
        assert_eq!(prompt.messages().len(), 3);
        assert!(matches!(prompt.messages()[0], ModelMessage::User(_)));
        assert!(matches!(prompt.messages()[1], ModelMessage::Assistant(_)));
        assert!(matches!(prompt.messages()[2], ModelMessage::Tool { .. }));
        cleanup(&reopened_store, &reopened, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replay_reports_physical_line_and_offset_for_complete_corruption() {
        let first = encoded(&ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "first".to_owned(),
        });
        let malformed_with_partial_tail = [
            first.as_slice(),
            b"{\"type\":\"user\"}\n".as_slice(),
            b"{\"type\":\"user\"".as_slice(),
        ]
        .concat();
        expect_open_error(
            malformed_with_partial_tail,
            ConversationError::CorruptAt {
                line: 2,
                offset: first.len() as u64,
            },
        )
        .await;

        expect_open_error(
            [first.as_slice(), b"\xff\n"].concat(),
            ConversationError::CorruptAt {
                line: 2,
                offset: first.len() as u64,
            },
        )
        .await;

        let first_seq_two = encoded(&ConversationEntry::User {
            seq: 2,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "first".to_owned(),
        });
        expect_open_error(
            first_seq_two,
            ConversationError::CorruptAt { line: 1, offset: 0 },
        )
        .await;

        let first_seq_one = encoded(&ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "first".to_owned(),
        });
        let third_seq = encoded(&ConversationEntry::User {
            seq: 3,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "third".to_owned(),
        });
        let decreasing = encoded(&ConversationEntry::User {
            seq: 2,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "decreasing".to_owned(),
        });
        expect_open_error(
            [
                first_seq_one.as_slice(),
                third_seq.as_slice(),
                decreasing.as_slice(),
            ]
            .concat(),
            ConversationError::CorruptAt {
                line: 3,
                offset: (first_seq_one.len() + third_seq.len()) as u64,
            },
        )
        .await;

        let orphan = ConversationEntry::ToolResult {
            seq: 2,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            call_id: ToolCallId::new("orphan").unwrap(),
            result: ToolOutput::success("orphan").unwrap(),
        };
        let orphan_line = encoded(&orphan);
        expect_open_error(
            [first.as_slice(), orphan_line.as_slice()].concat(),
            ConversationError::CorruptAt {
                line: 2,
                offset: first.len() as u64,
            },
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_seq_one_is_required_but_later_gaps_are_allowed() {
        let root = root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        store.create(&config(id)).await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let first = encoded(&ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "first".to_owned(),
        });
        let gap = encoded(&ConversationEntry::User {
            seq: 3,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "gap".to_owned(),
        });
        fs::write(&file, [first.as_slice(), gap.as_slice()].concat()).unwrap();
        let log = ConversationLog::open(&store, id).await.unwrap();
        assert_eq!(log.snapshot().await.max_seq(), 3);
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_replay_rejects_utf8_line_seq_and_file_bounds_without_rewrite() {
        expect_open_error(
            [vec![b'x'; MAX_LINE_BYTES], b"\n".to_vec()].concat(),
            ConversationError::TooLarge,
        )
        .await;
        expect_open_error(
            vec![0xff, b'\n'],
            ConversationError::CorruptAt { line: 1, offset: 0 },
        )
        .await;

        let first = encoded(&ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "first".to_owned(),
        });
        let duplicate = encoded(&ConversationEntry::User {
            seq: 1,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "duplicate".to_owned(),
        });
        expect_open_error(
            [first.as_slice(), duplicate.as_slice()].concat(),
            ConversationError::CorruptAt {
                line: 2,
                offset: first.len() as u64,
            },
        )
        .await;

        let second = encoded(&ConversationEntry::User {
            seq: 2,
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "second".to_owned(),
        });
        expect_open_error(
            [second.as_slice(), first.as_slice()].concat(),
            ConversationError::CorruptAt { line: 1, offset: 0 },
        )
        .await;

        expect_sparse_file_error(ConversationError::TooLarge).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reasoning_and_usage_survive_conversation_reload() {
        let (store, log, root, id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        let reasoning =
            ReasoningContent::new(Some("private reasoning".to_owned()), None, None, None, None)
                .unwrap();
        let response = response(
            vec![
                AssistantPart::Reasoning(reasoning.clone()),
                AssistantPart::Text("answer".to_owned()),
            ],
            Some(Usage::new(4, 5, 6)),
        );
        log.append(assistant(turn_id, response.clone()))
            .await
            .unwrap();
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let bytes = fs::read_to_string(&file).unwrap();
        assert!(bytes.contains("reasoning"));
        assert!(bytes.contains("input_tokens"));
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        let snapshot = reopened.snapshot().await;
        assert_eq!(
            reopened.prompt_view().await.unwrap().messages(),
            &[ModelMessage::assistant(vec![
                AssistantPart::Reasoning(reasoning),
                AssistantPart::Text("answer".to_owned()),
            ])
            .unwrap()]
        );
        assert!(matches!(
            snapshot.entries()[0].as_ref(),
            ConversationEntry::Assistant {
                text: Some(actual),
                reasoning: Some(_),
                tool_calls,
                usage: Some(_),
                ..
            } if actual == "answer" && tool_calls.is_empty()
        ));
        cleanup(&reopened_store, &reopened, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn usage_aggregates_all_fields_and_survives_reload() {
        let (store, log, root, id) = opened().await;
        let first = Usage::from_optional(Some(1), Some(2), Some(3))
            .with_cache_read_tokens(Some(4))
            .with_cache_write_tokens(Some(5))
            .with_provider_total_tokens(Some(6));
        let second = Usage::from_optional(Some(10), Some(20), Some(30))
            .with_cache_read_tokens(Some(40))
            .with_cache_write_tokens(Some(50))
            .with_provider_total_tokens(Some(60));
        let first_turn = TurnId::new().unwrap();
        log.append(assistant(
            first_turn,
            response(vec![AssistantPart::Text("one".to_owned())], Some(first)),
        ))
        .await
        .unwrap();
        log.append(terminal(first_turn, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        let second_turn = TurnId::new().unwrap();
        log.append(assistant(
            second_turn,
            response(vec![AssistantPart::Text("two".to_owned())], Some(second)),
        ))
        .await
        .unwrap();
        log.append(terminal(second_turn, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        let expected = Usage::from_optional(Some(11), Some(22), Some(33))
            .with_cache_read_tokens(Some(44))
            .with_cache_write_tokens(Some(55))
            .with_provider_total_tokens(Some(66));
        assert_eq!(log.usage().await, expected);
        assert_eq!(log.snapshot().await.usage(), expected);
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        let reopened_store = SessionStore::open(root.clone()).await.unwrap();
        let reopened = ConversationLog::open(&reopened_store, id).await.unwrap();
        assert_eq!(reopened.usage().await, expected);
        assert_eq!(reopened.snapshot().await.usage(), expected);
        cleanup(&reopened_store, &reopened, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn usage_is_conservative_for_missing_fields_and_whole_usage() {
        let (store, log, root, _id) = opened().await;
        let partial = Usage::from_optional(Some(1), None, Some(3)).with_cache_read_tokens(Some(4));
        let partial_turn = TurnId::new().unwrap();
        log.append(assistant(
            partial_turn,
            response(
                vec![AssistantPart::Text("partial".to_owned())],
                Some(partial),
            ),
        ))
        .await
        .unwrap();
        log.append(terminal(partial_turn, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        let aggregate = log.usage().await;
        assert_eq!(aggregate.input_tokens(), Some(1));
        assert_eq!(aggregate.output_tokens(), None);
        assert_eq!(aggregate.reasoning_tokens(), Some(3));
        assert_eq!(aggregate.cache_read_tokens(), Some(4));
        assert_eq!(aggregate.cache_write_tokens(), None);
        assert_eq!(aggregate.provider_total_tokens(), None);

        let missing_turn = TurnId::new().unwrap();
        log.append(assistant(
            missing_turn,
            response(vec![AssistantPart::Text("missing".to_owned())], None),
        ))
        .await
        .unwrap();
        assert_eq!(log.usage().await, Usage::default());
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn usage_field_overflow_is_conservative_per_field() {
        let (store, log, root, _id) = opened().await;
        let first_turn = TurnId::new().unwrap();
        log.append(assistant(
            first_turn,
            response(
                vec![AssistantPart::Text("overflow".to_owned())],
                Some(Usage::from_optional(Some(u64::MAX), Some(2), Some(3))),
            ),
        ))
        .await
        .unwrap();
        let second_turn = TurnId::new().unwrap();
        log.append(assistant(
            second_turn,
            response(
                vec![AssistantPart::Text("second".to_owned())],
                Some(Usage::from_optional(Some(1), Some(4), Some(5))),
            ),
        ))
        .await
        .unwrap();
        let aggregate = log.usage().await;
        assert_eq!(aggregate.input_tokens(), None);
        assert_eq!(aggregate.output_tokens(), Some(6));
        assert_eq!(aggregate.reasoning_tokens(), Some(8));
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_idle_drains_admitted_append_without_setting_closing() {
        let (store, log, root, _id) = opened().await;
        let (started_sender, started_receiver) = channel();
        let (release_sender, release_receiver) = channel();
        let blocker = store.run_io(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        started_receiver.recv().unwrap();
        let turn_id = TurnId::new().unwrap();
        let mut append = Box::pin(log.append(user(turn_id, "pending")));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(append.as_mut().poll(&mut context), Poll::Pending));
        let mut idle = Box::pin(log.wait_idle());
        assert!(matches!(idle.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(
            log.append(user(TurnId::new().unwrap(), "rejected while busy"))
                .await,
            Err(ConversationError::Busy)
        );
        release_sender.send(()).unwrap();
        SessionStore::await_io(blocker).await.unwrap();
        assert_eq!(append.await.unwrap(), 1);
        idle.await;
        assert_eq!(
            log.append(user(TurnId::new().unwrap(), "after"))
                .await
                .unwrap(),
            2
        );
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn append_drop_is_settled_and_close_drains_without_timeouts() {
        let (store, log, root, _id) = opened().await;
        let (started_sender, started_receiver) = channel();
        let (release_sender, release_receiver) = channel();
        let blocker = store.run_io(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        started_receiver.recv().unwrap();
        let turn_id = TurnId::new().unwrap();
        let mut append = Box::pin(log.append(user(turn_id, "dropped")));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(append.as_mut().poll(&mut context), Poll::Pending));
        drop(append);
        let barrier = store.run_io(|| Ok::<_, crate::session::store::StoreError>(()));
        release_sender.send(()).unwrap();
        SessionStore::await_io(blocker).await.unwrap();
        SessionStore::await_io(barrier).await.unwrap();
        assert_eq!(log.snapshot().await.max_seq(), 1);

        let (started_sender, started_receiver) = channel();
        let (release_sender, release_receiver) = channel();
        let blocker = store.run_io(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        started_receiver.recv().unwrap();
        let append = log.append(user(TurnId::new().unwrap(), "drain"));
        tokio::pin!(append);
        let close = log.close();
        tokio::pin!(close);
        let pending_append = std::future::poll_fn(|cx| match append.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("append completed before blocker release"),
        });
        pending_append.await;
        let close_pending = std::future::poll_fn(|cx| match close.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(result) => panic!("close completed before drain: {result:?}"),
        });
        close_pending.await;
        release_sender.send(()).unwrap();
        SessionStore::await_io(blocker).await.unwrap();
        assert_eq!(append.await.unwrap(), 2);
        assert!(close.await.is_ok());
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_summary_future_is_settled_by_the_worker() {
        let (store, log, root, _id) = opened().await;
        let turn_id = TurnId::new().unwrap();
        log.append(user(turn_id, "input")).await.unwrap();
        log.append(assistant(
            turn_id,
            response(vec![AssistantPart::Text("output".to_owned())], None),
        ))
        .await
        .unwrap();
        log.append(terminal(turn_id, StoredTurnOutcome::Completed))
            .await
            .unwrap();
        let (started_sender, started_receiver) = channel();
        let (release_sender, release_receiver) = channel();
        let blocker = store.run_io(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        started_receiver.recv().unwrap();
        let mut summary = Box::pin(log.append_summary(3, 3, timestamp(), "summary".to_owned()));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(summary.as_mut().poll(&mut context), Poll::Pending));
        drop(summary);
        let barrier = store.run_io(|| Ok::<_, crate::session::store::StoreError>(()));
        release_sender.send(()).unwrap();
        SessionStore::await_io(blocker).await.unwrap();
        SessionStore::await_io(barrier).await.unwrap();
        assert_eq!(log.snapshot().await.max_seq(), 4);
        assert_eq!(
            log.prompt_view()
                .await
                .unwrap()
                .latest_summary()
                .unwrap()
                .text(),
            "summary"
        );
        cleanup(&store, &log, root).await;
    }

    #[test]
    fn bounds_and_errors_are_redacted_and_surface_stays_private() {
        assert!(ConversationError::Corrupt.to_string().len() < 64);
        assert!(!format!("{:?}", ConversationError::Corrupt).contains("/tmp"));
        let located = ConversationError::CorruptAt {
            line: 2,
            offset: 10,
        };
        assert!(matches!(
            located,
            ConversationError::CorruptAt {
                line: 2,
                offset: 10
            }
        ));
        assert_eq!(located.to_string(), "conversation data is corrupt");
        assert!(!located.to_string().contains("10"));
        let secret = NewConversationEntry::User {
            turn_id: TurnId::new().unwrap(),
            timestamp: timestamp(),
            text: "SECRET-CONVERSATION".to_owned(),
        };
        assert!(!format!("{secret:?}").contains("SECRET-CONVERSATION"));
        assert!(
            serde_json::to_string(&ConversationEntry::Summary {
                seq: 1,
                timestamp: timestamp(),
                through_seq: 0,
                text: "x".repeat(65_537),
            })
            .is_err()
        );
        assert!(
            NewConversationEntry::Summary {
                timestamp: timestamp(),
                through_seq: 0,
                text: "x".repeat(65_537),
            }
            .into_entry(1)
            .is_err()
        );
    }
}
