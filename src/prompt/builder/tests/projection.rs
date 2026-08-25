use super::*;

#[test]
fn latest_summary_replaces_through_history_and_keeps_current_open_turn() {
    let conversation = view(vec![
        user(1, 1, "old question"),
        assistant(2, 1, None, Some("old answer"), Vec::new()),
        terminal(3, 1),
        summary(4, 3, "first summary"),
        user(5, 2, "middle question"),
        assistant(6, 2, None, Some("middle answer"), Vec::new()),
        terminal(7, 2),
        summary(8, 7, "latest summary"),
        user(9, 3, "current question"),
    ]);
    let request = finish(
        &builder("", &[]),
        &conversation,
        &empty_context(),
        ModelLimits::default(),
    )
    .unwrap();
    assert_eq!(
        request.messages(),
        &[
            ModelMessage::system(KERNEL_INVARIANT).unwrap(),
            ModelMessage::system("latest summary").unwrap(),
            ModelMessage::user("current question").unwrap(),
        ]
    );
}

#[test]
fn no_summary_projects_all_entries_in_canonical_part_and_tool_result_order() {
    let outcomes = [
        ToolResultOutcome::Success,
        ToolResultOutcome::Failed,
        ToolResultOutcome::Denied,
        ToolResultOutcome::Cancelled,
        ToolResultOutcome::InputProvided,
    ];
    let calls = (0..5)
        .map(|index| call(index, 10 + index as u8, "search"))
        .collect::<Vec<_>>();
    let mut entries = vec![
        user(1, 1, "question"),
        assistant(2, 1, Some("reasoning"), Some("answer"), calls.clone()),
    ];
    for (offset, outcome) in outcomes.into_iter().enumerate() {
        entries.push(tool_result(
            3 + offset as u64,
            1,
            10 + offset as u8,
            "search",
            outcome,
            &format!("output-{offset}"),
        ));
    }
    entries.push(assistant(8, 1, None, Some("final answer"), Vec::new()));
    entries.push(terminal(9, 1));
    let request = finish(
        &builder("", &["search"]),
        &view(entries),
        &empty_context(),
        ModelLimits::default(),
    )
    .unwrap();

    let expected_parts = {
        let mut parts = vec![
            AssistantPart::Reasoning(
                ReasoningContent::new(Some("reasoning".to_owned()), None, None, None).unwrap(),
            ),
            AssistantPart::Text("answer".to_owned()),
        ];
        parts.extend(calls.into_iter().map(AssistantPart::ToolCall));
        parts
    };
    assert_eq!(
        request.messages()[0],
        ModelMessage::system(KERNEL_INVARIANT).unwrap()
    );
    assert_eq!(
        request.messages()[1],
        ModelMessage::user("question").unwrap()
    );
    assert_eq!(
        request.messages()[2],
        ModelMessage::assistant(expected_parts).unwrap()
    );
    assert_eq!(request.messages().len(), 9);
    for (index, outcome) in [
        ToolResultOutcome::Success,
        ToolResultOutcome::Failed,
        ToolResultOutcome::Denied,
        ToolResultOutcome::Cancelled,
        ToolResultOutcome::InputProvided,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            request.messages()[3 + index],
            ModelMessage::tool_with_outcome(
                call_id(10 + index as u8),
                ToolOutput::new(format!("output-{index}")).unwrap(),
                outcome,
            )
            .unwrap()
        );
    }
    assert_eq!(
        request.messages()[8],
        ModelMessage::assistant(vec![AssistantPart::Text("final answer".to_owned())]).unwrap()
    );
}

#[test]
fn history_and_current_active_turn_accept_lower_per_turn_round_limits() {
    let conversation = view(vec![
        user_with_rounds(1, 1, "history", 1),
        assistant(2, 1, None, Some("done"), Vec::new()),
        terminal(3, 1),
        user_with_rounds(4, 2, "current", 1),
    ]);
    let request = finish(
        &builder("", &[]),
        &conversation,
        &empty_context(),
        ModelLimits::default(),
    )
    .unwrap();
    assert_eq!(
        request.messages(),
        &[
            ModelMessage::system(KERNEL_INVARIANT).unwrap(),
            ModelMessage::user("history").unwrap(),
            ModelMessage::assistant(vec![AssistantPart::Text("done".to_owned())]).unwrap(),
            ModelMessage::user("current").unwrap(),
        ]
    );
}

#[test]
fn maximum_valid_summary_is_projected_without_metadata_prefix() {
    let text = "s".repeat(BoundedText::MAX_BYTES);
    let conversation = view(vec![
        user(1, 1, "question"),
        assistant(2, 1, None, Some("done"), Vec::new()),
        terminal(3, 1),
        summary(4, 3, &text),
    ]);
    let request = finish(
        &builder("", &[]),
        &conversation,
        &empty_context(),
        ModelLimits::default(),
    )
    .unwrap();
    assert_eq!(request.messages()[1], ModelMessage::system(text).unwrap());
}
