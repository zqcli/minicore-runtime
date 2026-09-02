use serde::{Deserialize, Serialize};

use crate::config::UserInput;
use crate::ids::{LoopId, ToolCallId};
use crate::model::{AssistantPart, ModelFinishReason, ModelRef, ReasoningPreference, Usage};
use crate::tools::{ToolName, ToolOutput, ToolResultOutcome};
use crate::value::BoundedText;

/// Typed model-context history consumed and produced by one agent loop.
///
/// `HistoryItem` is neither a durable ledger nor a replay proof: the host owns
/// session history, persistence, and migration. An agent loop receives a base
/// history from the host and returns only its own incremental delta in
/// `LoopReport::appended`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HistoryItem {
    User(UserHistory),
    Assistant(AssistantHistory),
    ToolResult(ToolResultHistory),
    Summary(SummaryHistory),
}

/// Host input and runtime steers belonging to this loop.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UserHistory {
    pub loop_id: LoopId,
    pub kind: UserMessageKind,
    pub input: UserInput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserMessageKind {
    /// The initial user input that started the loop.
    Prompt,
    /// A `LoopHandle::steer` applied at a request boundary.
    Steering,
}

/// A complete, locally validated model response for one request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantHistory {
    pub loop_id: LoopId,
    pub request_index: u32,
    pub model: ModelRef,
    pub reasoning: ReasoningPreference,
    /// Reuses the existing typed assistant parts; no second part vocabulary.
    pub content: Vec<AssistantPart>,
    pub finish_reason: ModelFinishReason,
    pub usage: Usage,
}

/// The result of one tool call executed inside this loop.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultHistory {
    pub loop_id: LoopId,
    pub request_index: u32,
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub outcome: ToolResultOutcome,
    pub output: ToolOutput,
}

/// Host-managed summary without durable boundary semantics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SummaryHistory {
    pub content: BoundedText,
}

/// Borrowed projection of the host base history plus the current loop delta.
///
/// Views never copy or merge history; the runner hands one to the active
/// `PromptProvider` for each request.
#[derive(Clone, Copy, Debug)]
pub struct HistoryView<'a> {
    base: &'a [HistoryItem],
    appended: &'a [HistoryItem],
}

impl<'a> HistoryView<'a> {
    pub fn new(base: &'a [HistoryItem], appended: &'a [HistoryItem]) -> Self {
        Self { base, appended }
    }

    pub fn base(&self) -> &'a [HistoryItem] {
        self.base
    }

    pub fn appended(&self) -> &'a [HistoryItem] {
        self.appended
    }

    pub fn len(&self) -> usize {
        self.base.len() + self.appended.len()
    }

    pub fn is_empty(&self) -> bool {
        self.base.is_empty() && self.appended.is_empty()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &'a HistoryItem> {
        self.base.iter().chain(self.appended.iter())
    }
}
