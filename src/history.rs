use std::sync::Arc;

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

/// Turn-local history: the host-owned base plus the current loop's in-memory
/// delta. The delta becomes `LoopReport::appended` when the loop ends.
pub(crate) struct WorkingHistory {
    base: Arc<[HistoryItem]>,
    appended: Vec<HistoryItem>,
}

impl WorkingHistory {
    pub(crate) fn new(base: Arc<[HistoryItem]>) -> Self {
        Self {
            base,
            appended: Vec::new(),
        }
    }

    pub(crate) fn view(&self) -> HistoryView<'_> {
        HistoryView::new(&self.base, &self.appended)
    }

    pub(crate) fn append_user(&mut self, item: UserHistory) {
        self.appended.push(HistoryItem::User(item));
    }

    pub(crate) fn append_assistant(&mut self, item: AssistantHistory) {
        self.appended.push(HistoryItem::Assistant(item));
    }

    pub(crate) fn append_tool_result(&mut self, item: ToolResultHistory) {
        self.appended.push(HistoryItem::ToolResult(item));
    }

    pub(crate) fn into_appended(self) -> Arc<[HistoryItem]> {
        self.appended.into()
    }
}

/// Estimates the total text footprint of a host history for the resource
/// ceiling checked at `AgentLoop::start`. This is a conservative upper bound,
/// not a serialized-size guarantee.
pub(crate) fn estimate_history_bytes(items: &[HistoryItem]) -> usize {
    items.iter().map(estimate_item_bytes).sum()
}

fn estimate_item_bytes(item: &HistoryItem) -> usize {
    match item {
        HistoryItem::User(user) => user.input.as_text().len(),
        HistoryItem::Assistant(assistant) => {
            assistant.content.iter().map(estimate_part_bytes).sum()
        }
        HistoryItem::ToolResult(result) => {
            result.call_id.as_str().len()
                + result.tool_name.as_str().len()
                + result.output.content().as_str().len()
        }
        HistoryItem::Summary(summary) => summary.content.as_str().len(),
    }
}

fn estimate_part_bytes(part: &AssistantPart) -> usize {
    match part {
        AssistantPart::Text(text) => text.len(),
        AssistantPart::Reasoning(reasoning) => reasoning
            .text()
            .map_or(0, str::len)
            .saturating_add(reasoning.summary().map_or(0, str::len))
            .saturating_add(reasoning.encrypted().map_or(0, str::len))
            .saturating_add(reasoning.signature().map_or(0, str::len)),
        AssistantPart::ToolCall(call) => {
            call.name().as_str().len() + call.arguments().to_string().len()
        }
    }
}
