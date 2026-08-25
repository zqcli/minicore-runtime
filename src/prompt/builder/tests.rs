use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::config::CompactionConfig;
use crate::context::{
    ContextBlock, ContextBundle, ContextDriver, ContextSlot, ValidatedContextBundle,
};
use crate::conversation::{
    AssistantMessageEntry, ConversationSeq, SummaryEntry, ToolResultEntry, TurnExecutionRecord,
    TurnTerminal, TurnTerminalEntry, UserInputRecord, UserMessageEntry,
};
use crate::ids::{ContextSourceId, ToolCallId, TurnId};
use crate::model::{ModelFinishReason, ModelRef, ReasoningPreference, ToolCall, Usage};
use crate::time::Timestamp;
use crate::tools::{ToolName, ToolResultOutcome};
use crate::value::BoundedText;

#[cfg(test)]
mod ordering;
#[cfg(test)]
mod projection;
#[cfg(test)]
mod tools;
#[cfg(test)]
mod validation_budget;

fn timestamp() -> Timestamp {
    "2026-08-19T12:34:56.789Z".parse().unwrap()
}

fn turn_id(value: u8) -> TurnId {
    format!("trn_{value:032}").parse().unwrap()
}

fn call_id(value: u8) -> ToolCallId {
    format!("call_{value:032}").parse().unwrap()
}

fn model_ref() -> ModelRef {
    "host:prompt".parse().unwrap()
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec::new(
        name.parse::<ToolName>().unwrap(),
        "tool description",
        json!({"type": "object"}),
    )
    .unwrap()
}

fn session_spec(system_prompt: &str, tools: &[&str]) -> SessionSpec {
    SessionSpec::new(
        model_ref(),
        ReasoningPreference::High,
        BoundedText::new(system_prompt).unwrap(),
        tools.iter().map(|name| name.parse().unwrap()).collect(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

fn builder(system_prompt: &str, tool_names: &[&str]) -> PromptBuilder {
    let spec = session_spec(system_prompt, tool_names);
    let tools = tool_names.iter().map(|name| tool_spec(name)).collect();
    PromptBuilder::new(&spec, tools, SemanticLimits::default()).unwrap()
}

fn view(entries: Vec<ConversationEntry>) -> ConversationView {
    let head = entries
        .last()
        .map(ConversationEntry::seq)
        .unwrap_or(ConversationSeq::ZERO);
    ConversationView::from_confirmed(head, Arc::from(entries))
}

fn view_with_head(head: u64, entries: Vec<ConversationEntry>) -> ConversationView {
    ConversationView::from_confirmed(ConversationSeq::new(head), Arc::from(entries))
}

fn user(seq: u64, turn: u8, text: &str) -> ConversationEntry {
    user_with_rounds(seq, turn, text, 4)
}

fn user_with_rounds(seq: u64, turn: u8, text: &str, max_tool_rounds: u16) -> ConversationEntry {
    ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(turn),
        input: UserInputRecord::new(BoundedText::new(text).unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            model_ref(),
            ReasoningPreference::High,
            max_tool_rounds,
        )
        .unwrap(),
        created_at: timestamp(),
    })
}

fn assistant(
    seq: u64,
    turn: u8,
    reasoning: Option<&str>,
    text: Option<&str>,
    tool_calls: Vec<ToolCall>,
) -> ConversationEntry {
    ConversationEntry::AssistantMessage(AssistantMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(turn),
        model: model_ref(),
        text: text.map(|text| BoundedText::new(text).unwrap()),
        reasoning: reasoning.map(|text| BoundedText::new(text).unwrap()),
        tool_calls,
        usage: Usage::default(),
        finish_reason: ModelFinishReason::Unknown,
        created_at: timestamp(),
    })
}

fn call(index: u32, id: u8, name: &str) -> ToolCall {
    ToolCall::new(
        call_id(id),
        name.parse().unwrap(),
        json!({"index": index}),
        index,
    )
    .unwrap()
}

fn tool_result(
    seq: u64,
    turn: u8,
    id: u8,
    name: &str,
    outcome: ToolResultOutcome,
    content: &str,
) -> ConversationEntry {
    ConversationEntry::ToolResult(ToolResultEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(turn),
        tool_call_id: call_id(id),
        tool_name: name.parse().unwrap(),
        outcome,
        content: BoundedText::new(content).unwrap(),
        created_at: timestamp(),
    })
}

fn summary(seq: u64, through: u64, text: &str) -> ConversationEntry {
    ConversationEntry::Summary(SummaryEntry {
        seq: ConversationSeq::new(seq),
        through: ConversationSeq::new(through),
        summary: BoundedText::new(text).unwrap(),
        created_at: timestamp(),
    })
}

fn terminal(seq: u64, turn: u8) -> ConversationEntry {
    ConversationEntry::TurnTerminal(TurnTerminalEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(turn),
        terminal: TurnTerminal::Completed,
        usage: Usage::default(),
        created_at: timestamp(),
    })
}

fn context_block(source: &str, slot: ContextSlot, priority: i16, content: &str) -> ContextBlock {
    ContextBlock {
        source: source.parse::<ContextSourceId>().unwrap(),
        slot,
        priority,
        content: BoundedText::new(content).unwrap(),
    }
}

fn checked_context(blocks: Vec<ContextBlock>) -> ValidatedContextBundle {
    ContextDriver::validated_for_tests(ContextBundle { blocks }, &SemanticLimits::default())
        .unwrap()
}

fn empty_context() -> ValidatedContextBundle {
    checked_context(Vec::new())
}

fn finish(
    builder: &PromptBuilder,
    conversation: &ConversationView,
    context: &ValidatedContextBundle,
    model_limits: ModelLimits,
) -> Result<ModelRequest, PromptError> {
    builder.plan(conversation, model_limits)?.finish(context)
}

#[test]
fn plan_projects_once_and_finish_reuses_the_projection() {
    let builder = builder("session rules", &[]);
    let conversation = view(vec![user(1, 1, "question")]);
    let plan = builder.plan(&conversation, ModelLimits::default()).unwrap();
    assert_eq!(builder.projection_calls(), 1);
    let request = plan.finish(&empty_context()).unwrap();
    assert_eq!(builder.projection_calls(), 1);
    assert_eq!(
        request.messages()[0],
        ModelMessage::system(KERNEL_INVARIANT).unwrap()
    );
}
