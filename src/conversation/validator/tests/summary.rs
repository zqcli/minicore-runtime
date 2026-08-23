use super::*;

fn completed_history(active: TurnId) -> Vec<ConversationEntry> {
    let prior = TurnId::new().unwrap();
    vec![
        user(1, prior),
        assistant(
            2,
            prior,
            Some("prior answer"),
            Vec::new(),
            ModelFinishReason::Stop,
        ),
        terminal(3, prior),
        user(4, active),
    ]
}

#[test]
fn summary_during_awaiting_assistant_preserves_the_active_turn() {
    let active = TurnId::new().unwrap();
    let mut entries = completed_history(active);
    entries.push(summary(5, 3));
    let state = validator().validate_batch(&entries).unwrap();
    assert_eq!(state.active_turn_id(), Some(active));
    assert!(state.unresolved_tool_calls().is_empty());
    assert_eq!(
        state.latest_summary_through(),
        Some(ConversationSeq::new(3))
    );
    assert!(
        state
            .validate_batch(&[
                assistant(
                    6,
                    active,
                    Some("current answer"),
                    Vec::new(),
                    ModelFinishReason::Stop,
                ),
                terminal(7, active),
            ])
            .is_ok()
    );
}

#[test]
fn summary_during_tool_results_preserves_pending_calls_and_continuation() {
    let active = TurnId::new().unwrap();
    let mut entries = completed_history(active);
    entries.extend([
        assistant(
            5,
            active,
            None,
            vec![call("call-active", "read_file", 0, json!({}))],
            ModelFinishReason::ToolCalls,
        ),
        summary(6, 3),
    ]);
    let state = validator().validate_batch(&entries).unwrap();
    assert_eq!(state.active_turn_id(), Some(active));
    assert_eq!(state.unresolved_tool_calls().len(), 1);
    assert_eq!(
        state.unresolved_tool_calls()[0].tool_call_id.as_str(),
        "call-active"
    );
    let after_result = state
        .validate_batch(&[result(7, active, "call-active", "read_file")])
        .unwrap();
    assert_error(
        after_result.validate_batch(&[assistant(
            8,
            active,
            None,
            vec![call("call-active", "read_file", 0, json!({}))],
            ModelFinishReason::ToolCalls,
        )]),
        ConversationValidationError::DuplicateToolCallId,
    );
    assert!(
        state
            .validate_batch(&[
                result(7, active, "call-active", "read_file"),
                assistant(
                    8,
                    active,
                    Some("after tool"),
                    Vec::new(),
                    ModelFinishReason::Stop,
                ),
                terminal(9, active),
            ])
            .is_ok()
    );
}

#[test]
fn summary_during_final_assistant_preserves_terminal_eligibility() {
    let active = TurnId::new().unwrap();
    let mut entries = completed_history(active);
    entries.extend([
        assistant(
            5,
            active,
            Some("current answer"),
            Vec::new(),
            ModelFinishReason::Stop,
        ),
        summary(6, 3),
    ]);
    let state = validator().validate_batch(&entries).unwrap();
    assert_eq!(state.active_turn_id(), Some(active));
    assert!(state.validate_batch(&[terminal(7, active)]).is_ok());
}

#[test]
fn active_turn_summary_cannot_cross_a_nonterminal_or_fake_boundary() {
    let active = TurnId::new().unwrap();
    let state = validator()
        .validate_batch(&completed_history(active))
        .unwrap();
    assert_error(
        state.validate_batch(&[summary(5, 4)]),
        ConversationValidationError::SummaryInvalidBoundary,
    );
    assert_error(
        state.validate_batch(&[summary(5, 2)]),
        ConversationValidationError::SummaryInvalidBoundary,
    );
    assert_eq!(state.active_turn_id(), Some(active));
    assert_eq!(state.head(), ConversationSeq::new(4));
}
