use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::config::{CompactionConfig, SemanticLimits, SessionSpec};
use crate::conversation::entry::TurnTerminal;
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::model::{ModelRef, ReasoningPreference, ToolCall, Usage};
use crate::tools::ToolResultOutcome;

#[cfg(test)]
mod summary;

fn timestamp() -> crate::config::Timestamp {
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

fn validator() -> ConversationValidator {
    ConversationValidator::new(spec(), SemanticLimits::default()).unwrap()
}

fn assert_error(
    result: Result<ConversationValidator, ConversationValidationError>,
    expected: ConversationValidationError,
) {
    assert_eq!(result.err(), Some(expected));
}

fn user(seq: u64, turn_id: TurnId) -> ConversationEntry {
    ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id,
        input: super::super::UserInputRecord::new(BoundedText::new("hello").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            "model:v1".parse().unwrap(),
            ReasoningPreference::Auto,
            4,
        )
        .unwrap(),
        created_at: timestamp(),
    })
}

fn call(id: &str, name: &str, index: u32, arguments: serde_json::Value) -> ToolCall {
    ToolCall::new(id.parse().unwrap(), name.parse().unwrap(), arguments, index).unwrap()
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
        content: BoundedText::new("ok").unwrap(),
        created_at: timestamp(),
    })
}

fn terminal_with(seq: u64, turn_id: TurnId, terminal: TurnTerminal) -> ConversationEntry {
    ConversationEntry::TurnTerminal(TurnTerminalEntry {
        seq: ConversationSeq::new(seq),
        turn_id,
        terminal,
        usage: Usage::default(),
        created_at: timestamp(),
    })
}

fn terminal(seq: u64, turn_id: TurnId) -> ConversationEntry {
    terminal_with(seq, turn_id, TurnTerminal::Completed)
}

fn summary(seq: u64, through: u64) -> ConversationEntry {
    ConversationEntry::Summary(SummaryEntry {
        seq: ConversationSeq::new(seq),
        through: ConversationSeq::new(through),
        summary: BoundedText::new("summary").unwrap(),
        created_at: timestamp(),
    })
}

#[test]
fn valid_replay_tracks_turn_tools_terminal_summary_and_next_user() {
    let turn_id = TurnId::new().unwrap();
    let next_turn = TurnId::new().unwrap();
    let entries = vec![
        user(1, turn_id),
        assistant(
            2,
            turn_id,
            None,
            vec![
                call("call-a", "read_file", 0, json!({"path": "a"})),
                call("call-b", "write_file", 1, json!({"path": "b"})),
            ],
            ModelFinishReason::ToolCalls,
        ),
        result(3, turn_id, "call-a", "read_file"),
        result(4, turn_id, "call-b", "write_file"),
        assistant(
            5,
            turn_id,
            Some("done"),
            Vec::new(),
            ModelFinishReason::Stop,
        ),
        terminal(6, turn_id),
        summary(7, 6),
        user(8, next_turn),
    ];
    let state = validator().validate_batch(&entries).unwrap();
    assert_eq!(state.head(), ConversationSeq::new(8));
    assert_eq!(state.active_turn_id(), Some(next_turn));
    assert!(state.unresolved_tool_calls().is_empty());
    assert!(
        state
            .terminal_boundaries()
            .contains(&ConversationSeq::new(6))
    );
    assert_eq!(
        state.latest_summary_through(),
        Some(ConversationSeq::new(6))
    );
}

#[test]
fn pending_tools_preserve_call_order_and_batch_failure_rolls_back() {
    let turn_id = TurnId::new().unwrap();
    let base = validator()
        .validate_batch(&[
            user(1, turn_id),
            assistant(
                2,
                turn_id,
                None,
                vec![
                    call("call-a", "read_file", 0, json!({})),
                    call("call-b", "write_file", 1, json!({})),
                ],
                ModelFinishReason::ToolCalls,
            ),
        ])
        .unwrap();
    let pending = base.unresolved_tool_calls();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].tool_call_id.as_str(), "call-a");
    assert_eq!(pending[1].tool_call_id.as_str(), "call-b");
    assert_error(
        base.validate_batch(&[result(3, turn_id, "call-b", "write_file")]),
        ConversationValidationError::ToolResultMismatch,
    );
    assert_error(
        base.validate_batch(&[result(3, TurnId::new().unwrap(), "call-a", "read_file")]),
        ConversationValidationError::TurnMismatch,
    );
    assert_error(
        base.validate_batch(&[
            result(3, turn_id, "call-a", "read_file"),
            result(4, turn_id, "call-a", "read_file"),
        ]),
        ConversationValidationError::ToolResultMismatch,
    );
    assert_eq!(base.head(), ConversationSeq::new(2));
    assert_eq!(base.unresolved_tool_calls().len(), 2);
    assert_eq!(
        base.unresolved_tool_calls()[0].tool_call_id.as_str(),
        "call-a"
    );
    assert_eq!(
        base.unresolved_tool_calls()[1].tool_call_id.as_str(),
        "call-b"
    );
    let recovered = base
        .validate_batch(&[result(3, turn_id, "call-a", "read_file")])
        .unwrap();
    assert_eq!(
        recovered.unresolved_tool_calls()[0].tool_call_id.as_str(),
        "call-b"
    );
    let resolved = recovered
        .validate_batch(&[result(4, turn_id, "call-b", "write_file")])
        .unwrap();
    assert!(resolved.unresolved_tool_calls().is_empty());
}

#[test]
fn turn_model_reasoning_round_and_user_limits_are_checked() {
    let turn_id = TurnId::new().unwrap();
    let invalid_limits = SemanticLimits {
        max_user_input_bytes: 0,
        ..SemanticLimits::default()
    };
    assert_eq!(
        ConversationValidator::new(spec(), invalid_limits).unwrap_err(),
        ConversationValidationError::InvalidLimits
    );
    let mut invalid_spec = spec();
    invalid_spec.max_tool_rounds = 0;
    assert_eq!(
        ConversationValidator::new(invalid_spec, SemanticLimits::default()).unwrap_err(),
        ConversationValidationError::InvalidSpec
    );
    assert_error(
        validator().validate_batch(&[user(1, turn_id), user(2, turn_id)]),
        ConversationValidationError::ActiveTurnExists,
    );
    let mut wrong_model = user(1, turn_id);
    if let ConversationEntry::UserMessage(entry) = &mut wrong_model {
        entry.execution.model = "other:v1".parse().unwrap();
    }
    assert_error(
        validator().validate_batch(&[wrong_model]),
        ConversationValidationError::ModelMismatch,
    );

    let mut wrong_reasoning = user(1, turn_id);
    if let ConversationEntry::UserMessage(entry) = &mut wrong_reasoning {
        entry.execution.reasoning = ReasoningPreference::High;
    }
    assert_error(
        validator().validate_batch(&[wrong_reasoning]),
        ConversationValidationError::ReasoningMismatch,
    );

    let mut zero_rounds = user(1, turn_id);
    if let ConversationEntry::UserMessage(entry) = &mut zero_rounds {
        entry.execution.max_tool_rounds = 0;
    }
    assert_error(
        validator().validate_batch(&[zero_rounds]),
        ConversationValidationError::InvalidToolRounds,
    );

    let limits = SemanticLimits {
        max_user_input_bytes: 3,
        ..SemanticLimits::default()
    };
    let small = ConversationValidator::new(spec(), limits).unwrap();
    assert_error(
        small.validate_batch(&[user(1, turn_id)]),
        ConversationValidationError::InvalidUserInput,
    );
}

#[test]
fn assistant_shape_tool_enablement_json_and_text_limits_are_checked() {
    let turn_id = TurnId::new().unwrap();
    let limits = SemanticLimits {
        max_model_text_bytes_per_round: 3,
        max_model_reasoning_bytes_per_round: 3,
        max_tool_input_bytes: 10,
        ..SemanticLimits::default()
    };
    let limited = ConversationValidator::new(spec(), limits).unwrap();
    assert_error(
        limited.validate_batch(&[
            user(1, turn_id),
            assistant(
                2,
                turn_id,
                Some("toolong"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
        ]),
        ConversationValidationError::InvalidAssistantContent,
    );

    let no_content = assistant(2, turn_id, None, Vec::new(), ModelFinishReason::Stop);
    assert_error(
        validator().validate_batch(&[user(1, turn_id), no_content]),
        ConversationValidationError::InvalidAssistantContent,
    );
    let missing_calls = assistant(
        2,
        turn_id,
        Some("text"),
        Vec::new(),
        ModelFinishReason::ToolCalls,
    );
    assert_error(
        validator().validate_batch(&[user(1, turn_id), missing_calls]),
        ConversationValidationError::InvalidAssistantShape,
    );
    let wrong_finish = assistant(
        2,
        turn_id,
        None,
        vec![call("call-a", "read_file", 0, json!({}))],
        ModelFinishReason::Stop,
    );
    assert_error(
        validator().validate_batch(&[user(1, turn_id), wrong_finish]),
        ConversationValidationError::InvalidAssistantShape,
    );
    let unknown_with_calls = assistant(
        2,
        turn_id,
        None,
        vec![call("call-a", "read_file", 0, json!({}))],
        ModelFinishReason::Unknown,
    );
    assert!(
        validator()
            .validate_batch(&[user(1, turn_id), unknown_with_calls])
            .is_ok()
    );
    let disabled = assistant(
        2,
        turn_id,
        None,
        vec![call("call-a", "missing", 0, json!({}))],
        ModelFinishReason::ToolCalls,
    );
    assert_error(
        validator().validate_batch(&[user(1, turn_id), disabled]),
        ConversationValidationError::ToolNotEnabled,
    );
    let wrong_order = assistant(
        2,
        turn_id,
        None,
        vec![call("call-a", "read_file", 1, json!({}))],
        ModelFinishReason::ToolCalls,
    );
    assert_error(
        validator().validate_batch(&[user(1, turn_id), wrong_order]),
        ConversationValidationError::InvalidToolCallOrder,
    );
    let too_large_json = assistant(
        2,
        turn_id,
        None,
        vec![call("call-a", "read_file", 0, json!({"long": "value"}))],
        ModelFinishReason::ToolCalls,
    );
    assert_error(
        limited.validate_batch(&[user(1, turn_id), too_large_json]),
        ConversationValidationError::ToolInputTooLarge,
    );
    let mut too_long_reasoning =
        assistant(2, turn_id, Some("ok"), Vec::new(), ModelFinishReason::Stop);
    if let ConversationEntry::AssistantMessage(entry) = &mut too_long_reasoning {
        entry.reasoning = Some(BoundedText::new("long").unwrap());
    }
    assert_error(
        limited.validate_batch(&[user(1, turn_id), too_long_reasoning]),
        ConversationValidationError::InvalidAssistantContent,
    );
    let short_name_limits = SemanticLimits {
        max_tool_name_bytes: 3,
        ..SemanticLimits::default()
    };
    let short_names = ConversationValidator::new(
        SessionSpec::new(
            "model:v1".parse().unwrap(),
            ReasoningPreference::Auto,
            BoundedText::new("system").unwrap(),
            BTreeSet::new(),
            4,
            CompactionConfig::Disabled,
        )
        .unwrap(),
        short_name_limits,
    )
    .unwrap();
    let long_name = assistant(
        2,
        turn_id,
        None,
        vec![call("call-a", "read_file", 0, json!({}))],
        ModelFinishReason::ToolCalls,
    );
    assert_error(
        short_names.validate_batch(&[user(1, turn_id), long_name]),
        ConversationValidationError::ToolNameTooLong,
    );
    let max_count_limits = SemanticLimits {
        max_tool_count: 1,
        ..SemanticLimits::default()
    };
    let mut one_enabled_spec = spec();
    one_enabled_spec.enabled_tools = ["read_file".parse().unwrap()].into_iter().collect();
    let max_count = ConversationValidator::new(one_enabled_spec, max_count_limits).unwrap();
    let two_calls = assistant(
        2,
        turn_id,
        None,
        vec![
            call("call-a", "read_file", 0, json!({})),
            call("call-b", "read_file", 1, json!({})),
        ],
        ModelFinishReason::ToolCalls,
    );
    assert_error(
        max_count.validate_batch(&[user(1, turn_id), two_calls]),
        ConversationValidationError::InvalidToolCall,
    );
}

#[test]
fn terminal_phase_requires_final_assistant_only_for_completed() {
    let turn_id = TurnId::new().unwrap();
    assert_error(
        validator().validate_batch(&[user(1, turn_id), terminal(2, turn_id)]),
        ConversationValidationError::MissingFinalAssistant,
    );
    assert_error(
        validator().validate_batch(&[
            user(1, turn_id),
            assistant(
                2,
                turn_id,
                None,
                vec![call("call-a", "read_file", 0, json!({}))],
                ModelFinishReason::ToolCalls,
            ),
            result(3, turn_id, "call-a", "read_file"),
            terminal(4, turn_id),
        ]),
        ConversationValidationError::MissingFinalAssistant,
    );
    let failed = terminal_with(
        2,
        turn_id,
        TurnTerminal::Failed {
            diagnostic: DiagnosticSummary::new(
                DiagnosticCode::Internal,
                DiagnosticCategory::Internal,
                BoundedText::new("failed").unwrap(),
                false,
            ),
        },
    );
    assert!(
        validator()
            .validate_batch(&[user(1, turn_id), failed])
            .is_ok()
    );
    assert!(
        validator()
            .validate_batch(&[
                user(1, turn_id),
                terminal_with(2, turn_id, TurnTerminal::CancelledByUser),
            ])
            .is_ok()
    );

    for finish_reason in [ModelFinishReason::Length, ModelFinishReason::Refused] {
        let state = validator()
            .validate_batch(&[
                user(1, turn_id),
                assistant(2, turn_id, Some("done"), Vec::new(), finish_reason),
            ])
            .unwrap();
        assert!(state.validate_batch(&[terminal(3, turn_id)]).is_ok());
    }

    let mut reasoning_only = assistant(2, turn_id, None, Vec::new(), ModelFinishReason::Stop);
    if let ConversationEntry::AssistantMessage(entry) = &mut reasoning_only {
        entry.reasoning = Some(BoundedText::new("thinking").unwrap());
    }
    assert!(
        validator()
            .validate_batch(&[user(1, turn_id), reasoning_only, terminal(3, turn_id)])
            .is_ok()
    );

    let final_assistant = assistant(
        2,
        turn_id,
        Some("done"),
        Vec::new(),
        ModelFinishReason::Stop,
    );
    assert_error(
        validator().validate_batch(&[
            user(1, turn_id),
            final_assistant,
            assistant(
                3,
                turn_id,
                Some("again"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
        ]),
        ConversationValidationError::InvalidPhase,
    );
}

#[test]
fn duplicate_ids_results_terminal_and_incomplete_exchange_are_rejected() {
    let turn_id = TurnId::new().unwrap();
    let duplicate_calls = vec![
        user(1, turn_id),
        assistant(
            2,
            turn_id,
            None,
            vec![
                call("call-a", "read_file", 0, json!({})),
                call("call-a", "write_file", 1, json!({})),
            ],
            ModelFinishReason::ToolCalls,
        ),
    ];
    assert_error(
        validator().validate_batch(&duplicate_calls),
        ConversationValidationError::DuplicateToolCallId,
    );

    let incomplete = vec![
        user(1, turn_id),
        assistant(
            2,
            turn_id,
            None,
            vec![call("call-a", "read_file", 0, json!({}))],
            ModelFinishReason::ToolCalls,
        ),
        assistant(3, turn_id, Some("bad"), Vec::new(), ModelFinishReason::Stop),
    ];
    assert_error(
        validator().validate_batch(&incomplete),
        ConversationValidationError::IncompleteToolExchange,
    );
    let duplicate_result = vec![
        user(1, turn_id),
        assistant(
            2,
            turn_id,
            None,
            vec![call("call-a", "read_file", 0, json!({}))],
            ModelFinishReason::ToolCalls,
        ),
        result(3, turn_id, "call-a", "read_file"),
        result(4, turn_id, "call-a", "read_file"),
    ];
    assert_error(
        validator().validate_batch(&duplicate_result),
        ConversationValidationError::ToolResultWithoutPending,
    );
    let blocked_terminals = [
        TurnTerminal::Completed,
        TurnTerminal::Failed {
            diagnostic: DiagnosticSummary::new(
                DiagnosticCode::Internal,
                DiagnosticCategory::Internal,
                BoundedText::new("failed").unwrap(),
                false,
            ),
        },
        TurnTerminal::CancelledByUser,
        TurnTerminal::CancelledByShutdown,
        TurnTerminal::CancelledByRestart,
        TurnTerminal::BudgetExceeded,
    ];
    for terminal in blocked_terminals {
        assert_error(
            validator().validate_batch(&[
                user(1, turn_id),
                assistant(
                    2,
                    turn_id,
                    None,
                    vec![call("call-a", "read_file", 0, json!({}))],
                    ModelFinishReason::ToolCalls,
                ),
                terminal_with(3, turn_id, terminal),
            ]),
            ConversationValidationError::TerminalWithPendingTools,
        );
    }
    let terminal_state = validator()
        .validate_batch(&[
            user(1, turn_id),
            assistant(
                2,
                turn_id,
                Some("done"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
            terminal(3, turn_id),
        ])
        .unwrap();
    assert_error(
        terminal_state.validate_batch(&[terminal(4, turn_id)]),
        ConversationValidationError::TerminalWithoutActiveTurn,
    );
    assert_error(
        validator().validate_batch(&[
            user(1, turn_id),
            assistant(
                2,
                turn_id,
                Some("done"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
            terminal(3, TurnId::new().unwrap()),
        ]),
        ConversationValidationError::TerminalTurnMismatch,
    );
    let small_output_limits = SemanticLimits {
        max_tool_output_bytes: 1,
        ..SemanticLimits::default()
    };
    let small_output = ConversationValidator::new(spec(), small_output_limits).unwrap();
    assert_error(
        small_output.validate_batch(&[
            user(1, turn_id),
            assistant(
                2,
                turn_id,
                None,
                vec![call("call-a", "read_file", 0, json!({}))],
                ModelFinishReason::ToolCalls,
            ),
            result(3, turn_id, "call-a", "read_file"),
        ]),
        ConversationValidationError::ToolOutputTooLarge,
    );

    let first_turn = TurnId::new().unwrap();
    let second_turn = TurnId::new().unwrap();
    let duplicate_across_turns = [
        user(1, first_turn),
        assistant(
            2,
            first_turn,
            None,
            vec![call("call-a", "read_file", 0, json!({}))],
            ModelFinishReason::ToolCalls,
        ),
        result(3, first_turn, "call-a", "read_file"),
        assistant(
            4,
            first_turn,
            Some("done"),
            Vec::new(),
            ModelFinishReason::Stop,
        ),
        terminal(5, first_turn),
        user(6, second_turn),
        assistant(
            7,
            second_turn,
            None,
            vec![call("call-a", "read_file", 0, json!({}))],
            ModelFinishReason::ToolCalls,
        ),
    ];
    assert_error(
        validator().validate_batch(&duplicate_across_turns),
        ConversationValidationError::DuplicateToolCallId,
    );
}

#[test]
fn sequence_and_summary_boundaries_are_checked_without_mutating_state() {
    let turn_id = TurnId::new().unwrap();
    let base = validator();
    assert_error(
        base.validate_batch(&[user(2, turn_id)]),
        ConversationValidationError::SequenceGap,
    );
    assert_eq!(base.head(), ConversationSeq::ZERO);
    assert_eq!(ConversationSeq::new(u64::MAX).next(), None);
    let mut overflow = validator();
    overflow.head = ConversationSeq::new(u64::MAX);
    assert_error(
        overflow.validate_batch(&[user(u64::MAX, turn_id)]),
        ConversationValidationError::SequenceOverflow,
    );

    let no_terminal = base.validate_batch(&[summary(1, 1)]);
    assert_error(
        no_terminal,
        ConversationValidationError::SummaryInvalidBoundary,
    );
    let after_terminal = validator()
        .validate_batch(&[
            user(1, turn_id),
            assistant(
                2,
                turn_id,
                Some("done"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
            terminal(3, turn_id),
        ])
        .unwrap();
    assert_error(
        after_terminal.validate_batch(&[summary(4, 1)]),
        ConversationValidationError::SummaryInvalidBoundary,
    );
    let mut empty_summary = summary(4, 3);
    if let ConversationEntry::Summary(entry) = &mut empty_summary {
        entry.summary = BoundedText::new("").unwrap();
    }
    assert_error(
        after_terminal.validate_batch(&[empty_summary]),
        ConversationValidationError::InvalidSummary,
    );
    let later_turn = TurnId::new().unwrap();
    let before_summary = after_terminal
        .validate_batch(&[
            user(4, later_turn),
            assistant(
                5,
                later_turn,
                Some("later"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
            terminal(6, later_turn),
        ])
        .unwrap();
    let summarized = before_summary.validate_batch(&[summary(7, 3)]).unwrap();
    assert_eq!(
        summarized.latest_summary_through(),
        Some(ConversationSeq::new(3))
    );
    let later_summary = summarized.validate_batch(&[summary(8, 6)]).unwrap();
    assert_eq!(
        later_summary.latest_summary_through(),
        Some(ConversationSeq::new(6))
    );
    assert_error(
        later_summary.validate_batch(&[summary(9, 3)]),
        ConversationValidationError::SummaryNotAdvanced,
    );
    assert_error(
        later_summary.validate_batch(&[summary(9, 6)]),
        ConversationValidationError::SummaryNotAdvanced,
    );
}
