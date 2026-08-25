use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::config::CompactionConfig;
use crate::conversation::{
    AssistantMessageEntry, ToolResultEntry, TurnExecutionRecord, TurnTerminal, TurnTerminalEntry,
    UserInputRecord, UserMessageEntry,
};
use crate::ids::TurnId;
use crate::model::{ModelFinishReason, ModelRef, ReasoningPreference, ToolCall, Usage};
use crate::time::Timestamp;
use crate::tools::ToolResultOutcome;
use crate::value::BoundedText;

#[cfg(test)]
mod compaction;

fn timestamp() -> Timestamp {
    "2026-08-19T12:34:56.789Z".parse().unwrap()
}

fn spec() -> SessionSpec {
    SessionSpec::new(
        "model:v1".parse::<ModelRef>().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        ["read_file", "write_file"]
            .into_iter()
            .map(|name| name.parse().unwrap())
            .collect::<BTreeSet<_>>(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

fn view(entries: Vec<ConversationEntry>) -> ConversationView {
    let head = entries
        .last()
        .map(ConversationEntry::seq)
        .unwrap_or(ConversationSeq::ZERO);
    ConversationView::from_confirmed(head, Arc::from(entries))
}

#[test]
fn validated_provenance_requires_state_head_spec_and_limits_match() {
    let spec = spec();
    let limits = SemanticLimits::default();
    let turn = TurnId::new().unwrap();
    let trusted =
        ConversationView::from_validated_entries(&spec, &limits, Arc::from(vec![user(1, turn, 4)]))
            .unwrap();
    assert!(trusted.is_validated_for(&spec, &limits));

    let external =
        ConversationView::from_confirmed(trusted.head(), trusted.entries().to_vec().into());
    assert!(!external.is_validated_for(&spec, &limits));

    let mut wrong_head = trusted.clone();
    wrong_head.head = ConversationSeq::new(2);
    assert!(!wrong_head.is_validated_for(&spec, &limits));

    let mut wrong_spec = spec.clone();
    wrong_spec.model = "other:v1".parse().unwrap();
    assert!(!trusted.is_validated_for(&wrong_spec, &limits));

    let mut wrong_limits = limits.clone();
    wrong_limits.max_tool_rounds -= 1;
    assert!(!trusted.is_validated_for(&spec, &wrong_limits));
}

fn user(seq: u64, turn_id: TurnId, max_tool_rounds: u16) -> ConversationEntry {
    ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id,
        input: UserInputRecord::new(BoundedText::new("question").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            "model:v1".parse().unwrap(),
            ReasoningPreference::Auto,
            max_tool_rounds,
        )
        .unwrap(),
        created_at: timestamp(),
    })
}

fn call(id: &str, name: &str, index: u32) -> ToolCall {
    ToolCall::new(id.parse().unwrap(), name.parse().unwrap(), json!({}), index).unwrap()
}

fn assistant(
    seq: u64,
    turn_id: TurnId,
    text: Option<&str>,
    calls: Vec<ToolCall>,
    finish_reason: ModelFinishReason,
) -> ConversationEntry {
    ConversationEntry::AssistantMessage(AssistantMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id,
        model: "model:v1".parse().unwrap(),
        text: text.map(|value| BoundedText::new(value).unwrap()),
        reasoning: None,
        tool_calls: calls,
        usage: Usage::default(),
        finish_reason,
        created_at: timestamp(),
    })
}

fn result(seq: u64, turn_id: TurnId, id: &str, name: &str) -> ConversationEntry {
    ConversationEntry::ToolResult(ToolResultEntry {
        seq: ConversationSeq::new(seq),
        turn_id,
        tool_call_id: id.parse().unwrap(),
        tool_name: name.parse().unwrap(),
        outcome: ToolResultOutcome::Success,
        content: BoundedText::new("result").unwrap(),
        created_at: timestamp(),
    })
}

fn terminal(seq: u64, turn_id: TurnId) -> ConversationEntry {
    ConversationEntry::TurnTerminal(TurnTerminalEntry {
        seq: ConversationSeq::new(seq),
        turn_id,
        terminal: TurnTerminal::Completed,
        usage: Usage::default(),
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

fn assert_error(entries: Vec<ConversationEntry>, expected: ConversationValidationError) {
    assert_eq!(
        view(entries)
            .validated_prompt_projection(&spec(), &SemanticLimits::default())
            .unwrap_err(),
        expected
    );
}

#[test]
fn projection_selects_latest_summary_and_accepts_lower_active_turn_round_limit() {
    let first = TurnId::new().unwrap();
    let current = TurnId::new().unwrap();
    let conversation = view(vec![
        user(1, first, 4),
        assistant(
            2,
            first,
            Some("answer"),
            Vec::new(),
            ModelFinishReason::Stop,
        ),
        terminal(3, first),
        summary(4, 3, "sensitive summary content"),
        user(5, current, 1),
    ]);
    let projection = conversation
        .validated_prompt_projection(&spec(), &SemanticLimits::default())
        .unwrap();
    assert_eq!(projection.head, ConversationSeq::new(5));
    let active = conversation
        .validated_active_turn(&spec(), &SemanticLimits::default())
        .unwrap();
    assert_eq!(active.turn_id, Some(current));
    assert_eq!(active.execution.unwrap().max_tool_rounds, 1);
    assert_eq!(
        projection.selected_summary().unwrap().seq,
        ConversationSeq::new(4)
    );
    assert_eq!(
        projection
            .entries()
            .iter()
            .map(ConversationEntry::seq)
            .collect::<Vec<_>>(),
        [ConversationSeq::new(4), ConversationSeq::new(5)]
    );
    // Debug stays redacted: counts and sequence numbers only, never content.
    let debug = format!("{projection:?}");
    assert!(debug.contains("entry_count: 2"));
    assert!(debug.contains("selected_summary_seq: Some(ConversationSeq(4))"));
    assert!(!debug.contains("sensitive summary content"));
    assert!(!debug.contains("question"));
}

#[test]
fn projection_reuses_exact_sequence_turn_tool_and_finish_errors() {
    let turn = TurnId::new().unwrap();
    assert_error(
        vec![user(2, turn, 4)],
        ConversationValidationError::SequenceGap,
    );
    assert_error(
        vec![
            user(1, turn, 4),
            assistant(3, turn, Some("gap"), Vec::new(), ModelFinishReason::Stop),
        ],
        ConversationValidationError::SequenceGap,
    );
    assert_error(
        vec![
            user(1, turn, 4),
            assistant(
                2,
                TurnId::new().unwrap(),
                Some("wrong turn"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
        ],
        ConversationValidationError::TurnMismatch,
    );
    let mut wrong_model = user(1, turn, 4);
    if let ConversationEntry::UserMessage(entry) = &mut wrong_model {
        entry.execution.model = "other:v1".parse().unwrap();
    }
    assert_error(
        vec![wrong_model],
        ConversationValidationError::ModelMismatch,
    );
    let mut wrong_reasoning = user(1, turn, 4);
    if let ConversationEntry::UserMessage(entry) = &mut wrong_reasoning {
        entry.execution.reasoning = ReasoningPreference::High;
    }
    assert_error(
        vec![wrong_reasoning],
        ConversationValidationError::ReasoningMismatch,
    );
    assert_error(
        vec![
            user(1, turn, 4),
            assistant(
                2,
                turn,
                None,
                vec![call("call-a", "missing", 0)],
                ModelFinishReason::ToolCalls,
            ),
        ],
        ConversationValidationError::ToolNotEnabled,
    );
    assert_error(
        vec![
            user(1, turn, 4),
            assistant(
                2,
                turn,
                None,
                vec![call("call-a", "read_file", 1)],
                ModelFinishReason::ToolCalls,
            ),
        ],
        ConversationValidationError::InvalidToolCallOrder,
    );
    assert_error(
        vec![
            user(1, turn, 4),
            assistant(
                2,
                turn,
                None,
                vec![call("call-a", "read_file", 0)],
                ModelFinishReason::Stop,
            ),
        ],
        ConversationValidationError::InvalidAssistantShape,
    );
}

#[test]
fn projection_reuses_tool_result_session_id_terminal_and_summary_errors() {
    let first = TurnId::new().unwrap();
    assert_error(
        vec![
            user(1, first, 4),
            assistant(
                2,
                first,
                None,
                vec![
                    call("call-a", "read_file", 0),
                    call("call-b", "write_file", 1),
                ],
                ModelFinishReason::ToolCalls,
            ),
            result(3, first, "call-b", "write_file"),
        ],
        ConversationValidationError::ToolResultMismatch,
    );

    let second = TurnId::new().unwrap();
    assert_error(
        vec![
            user(1, first, 4),
            assistant(
                2,
                first,
                None,
                vec![call("call-a", "read_file", 0)],
                ModelFinishReason::ToolCalls,
            ),
            result(3, first, "call-a", "read_file"),
            assistant(4, first, Some("done"), Vec::new(), ModelFinishReason::Stop),
            terminal(5, first),
            user(6, second, 4),
            assistant(
                7,
                second,
                None,
                vec![call("call-a", "read_file", 0)],
                ModelFinishReason::ToolCalls,
            ),
        ],
        ConversationValidationError::DuplicateToolCallId,
    );
    assert_error(
        vec![user(1, first, 4), terminal(2, first)],
        ConversationValidationError::MissingFinalAssistant,
    );

    let completed = vec![
        user(1, first, 4),
        assistant(2, first, Some("done"), Vec::new(), ModelFinishReason::Stop),
        terminal(3, first),
    ];
    let mut invalid_boundary = completed.clone();
    invalid_boundary.push(summary(4, 1, "bad boundary"));
    assert_error(
        invalid_boundary,
        ConversationValidationError::SummaryInvalidBoundary,
    );
    let mut not_advanced = completed;
    not_advanced.push(summary(4, 3, "first"));
    not_advanced.push(summary(5, 3, "same boundary"));
    assert_error(
        not_advanced,
        ConversationValidationError::SummaryNotAdvanced,
    );
}

#[test]
fn projection_requires_the_validated_head_to_match_the_view_head() {
    let turn = TurnId::new().unwrap();
    let conversation = ConversationView::from_confirmed(
        ConversationSeq::new(2),
        Arc::from(vec![user(1, turn, 1)]),
    );
    assert_eq!(
        conversation
            .validated_prompt_projection(&spec(), &SemanticLimits::default())
            .unwrap_err(),
        ConversationValidationError::SequenceGap
    );
}
