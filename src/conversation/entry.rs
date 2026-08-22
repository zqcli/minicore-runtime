use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::DiagnosticSummary;
use crate::ids::{ToolCallId, TurnId};
use crate::model::{ModelFinishReason, ModelRef, ReasoningPreference, ToolCall, Usage};
use crate::time::Timestamp;
use crate::tools::{ToolName, ToolResultOutcome};
use crate::value::BoundedText;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConversationSeq(u64);

impl ConversationSeq {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum UserInputRecordError {
    #[error("conversation user input must not be empty")]
    Empty,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserInputRecord {
    pub text: BoundedText,
}

impl UserInputRecord {
    pub fn new(text: BoundedText) -> Result<Self, UserInputRecordError> {
        if text.is_empty() {
            return Err(UserInputRecordError::Empty);
        }
        Ok(Self { text })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserInputRecordWire {
    text: BoundedText,
}

impl<'de> Deserialize<'de> for UserInputRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = UserInputRecordWire::deserialize(deserializer)?;
        Self::new(value.text).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TurnExecutionRecordError {
    #[error("conversation turn execution must allow at least one tool round")]
    InvalidMaxToolRounds,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnExecutionRecord {
    pub model: ModelRef,
    pub reasoning: ReasoningPreference,
    pub max_tool_rounds: u16,
}

impl TurnExecutionRecord {
    pub fn new(
        model: ModelRef,
        reasoning: ReasoningPreference,
        max_tool_rounds: u16,
    ) -> Result<Self, TurnExecutionRecordError> {
        if max_tool_rounds == 0 {
            return Err(TurnExecutionRecordError::InvalidMaxToolRounds);
        }
        Ok(Self {
            model,
            reasoning,
            max_tool_rounds,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnExecutionRecordWire {
    model: ModelRef,
    reasoning: ReasoningPreference,
    max_tool_rounds: u16,
}

impl<'de> Deserialize<'de> for TurnExecutionRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = TurnExecutionRecordWire::deserialize(deserializer)?;
        Self::new(value.model, value.reasoning, value.max_tool_rounds)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserMessageEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub input: UserInputRecord,
    pub execution: TurnExecutionRecord,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantMessageEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub model: ModelRef,
    pub text: Option<BoundedText>,
    pub reasoning: Option<BoundedText>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: ModelFinishReason,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub outcome: ToolResultOutcome,
    pub content: BoundedText,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryEntry {
    pub seq: ConversationSeq,
    pub through: ConversationSeq,
    pub summary: BoundedText,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminal {
    Completed,
    Failed { diagnostic: DiagnosticSummary },
    CancelledByUser,
    CancelledByShutdown,
    CancelledByRestart,
    BudgetExceeded,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedTurnTerminalWire {
    diagnostic: DiagnosticSummary,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TurnTerminalWire {
    Completed,
    Failed(FailedTurnTerminalWire),
    CancelledByUser,
    CancelledByShutdown,
    CancelledByRestart,
    BudgetExceeded,
}

impl<'de> Deserialize<'de> for TurnTerminal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match TurnTerminalWire::deserialize(deserializer)? {
            TurnTerminalWire::Completed => Self::Completed,
            TurnTerminalWire::Failed(value) => Self::Failed {
                diagnostic: value.diagnostic,
            },
            TurnTerminalWire::CancelledByUser => Self::CancelledByUser,
            TurnTerminalWire::CancelledByShutdown => Self::CancelledByShutdown,
            TurnTerminalWire::CancelledByRestart => Self::CancelledByRestart,
            TurnTerminalWire::BudgetExceeded => Self::BudgetExceeded,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnTerminalEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub terminal: TurnTerminal,
    pub usage: Usage,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationEntry {
    UserMessage(UserMessageEntry),
    AssistantMessage(AssistantMessageEntry),
    ToolResult(ToolResultEntry),
    Summary(SummaryEntry),
    TurnTerminal(TurnTerminalEntry),
}

impl ConversationEntry {
    pub const fn seq(&self) -> ConversationSeq {
        match self {
            Self::UserMessage(entry) => entry.seq,
            Self::AssistantMessage(entry) => entry.seq,
            Self::ToolResult(entry) => entry.seq,
            Self::Summary(entry) => entry.seq,
            Self::TurnTerminal(entry) => entry.seq,
        }
    }

    pub const fn turn_id(&self) -> Option<TurnId> {
        match self {
            Self::UserMessage(entry) => Some(entry.turn_id),
            Self::AssistantMessage(entry) => Some(entry.turn_id),
            Self::ToolResult(entry) => Some(entry.turn_id),
            Self::Summary(_) => None,
            Self::TurnTerminal(entry) => Some(entry.turn_id),
        }
    }
}
