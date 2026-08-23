use super::*;

#[tokio::test(flavor = "current_thread")]
async fn multiple_tools_are_sequential_and_every_commit_ack_precedes_continuation() {
    let first_usage = Usage::new(3, 1, 0);
    let second_usage = Usage::new(5, 2, 1);
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(1, "search"), (2, "search")], first_usage)),
            ModelBehavior::Events(final_events("final", second_usage)),
        ],
    );
    let tool = ScriptTool::new(
        "search",
        vec![
            ToolBehavior::Complete(ToolOutput::new("first result").unwrap()),
            ToolBehavior::Complete(ToolOutput::new("second result").unwrap()),
        ],
    );
    let policy = ScriptPolicy::new(vec![ToolDecision::Allow, ToolDecision::Allow]);
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, mut progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(
            Arc::clone(&model),
            None,
            vec![Arc::clone(&tool)],
            Some(policy),
        ),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));
    let mut conversation = initial;

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            assert_eq!(tool.calls(), 0);
            assert_eq!(draft.tool_calls.len(), 2);
            let acknowledgement = ack_assistant(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitToolResult { draft, reply } => {
            assert_eq!(draft.tool_call_id, call_id(1));
            assert_eq!(draft.outcome, ToolResultOutcome::Success);
            assert_eq!(tool.calls(), 1);
            let before_ack = std::iter::from_fn(|| progress_rx.try_recv().ok()).collect::<Vec<_>>();
            assert!(before_ack.iter().any(|event| matches!(
                event,
                RunnerProgress::ToolStarted { tool_call_id, .. } if tool_call_id == &call_id(1)
            )));
            assert!(!before_ack.iter().any(|event| matches!(
                event,
                RunnerProgress::ToolFinished { tool_call_id, .. } if tool_call_id == &call_id(1)
            )));
            let acknowledgement = ack_tool(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitToolResult { draft, reply } => {
            assert_eq!(draft.tool_call_id, call_id(2));
            assert_eq!(tool.calls(), 2);
            let acknowledgement = ack_tool(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            assert_eq!(draft.text.as_ref().unwrap().as_str(), "final");
            assert!(draft.tool_calls.is_empty());
            let acknowledgement = ack_assistant(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::Completed { usage }
        }) if usage == Usage::new(8, 3, 1)
    ));
    assert_finished(task.await.unwrap());
    assert_eq!(conversation.head(), ConversationSeq::new(5));

    let trailing = std::iter::from_fn(|| progress_rx.try_recv().ok()).collect::<Vec<_>>();
    let finished = trailing
        .iter()
        .filter(|event| matches!(event, RunnerProgress::ToolFinished { .. }))
        .count();
    assert_eq!(finished, 2);
    assert!(trailing.iter().any(|event| matches!(
        event,
        RunnerProgress::ToolStarted { tool_call_id, .. } if tool_call_id == &call_id(2)
    )));
    let first_finished = trailing
        .iter()
        .position(|event| {
            matches!(
                event,
                RunnerProgress::ToolFinished { tool_call_id, .. } if tool_call_id == &call_id(1)
            )
        })
        .unwrap();
    let second_started = trailing
        .iter()
        .position(|event| {
            matches!(
                event,
                RunnerProgress::ToolStarted { tool_call_id, .. } if tool_call_id == &call_id(2)
            )
        })
        .unwrap();
    assert!(first_finished < second_started);
    assert_eq!(model.requests().len(), 2);
    assert!(
        model.requests()[1]
            .0
            .messages()
            .iter()
            .any(|message| matches!(
                message,
                ModelMessage::Tool { tool_call_id, .. } if tool_call_id == &call_id(2)
            ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tool_round_limit_commits_assistant_but_never_executes_over_budget_calls() {
    let first_usage = Usage::new(3, 2, 1);
    let second_usage = Usage::new(5, 4, 2);
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(3, "search")], first_usage)),
            ModelBehavior::Events(tool_events(&[(4, "search")], second_usage)),
        ],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("first").unwrap())],
    );
    let policy = ScriptPolicy::new(vec![ToolDecision::Allow]);
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 1);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        1,
        session_bindings(model, None, vec![Arc::clone(&tool)], Some(policy)),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));
    let mut conversation = initial;
    for expected in ["assistant", "tool", "assistant"] {
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::CommitAssistant { draft, reply } if expected == "assistant" => {
                let acknowledgement = ack_assistant(&conversation, &draft, &spec);
                conversation = acknowledgement.conversation.clone();
                reply.send(Ok(acknowledgement)).unwrap();
            }
            RunnerEvent::CommitToolResult { draft, reply } if expected == "tool" => {
                let acknowledgement = ack_tool(&conversation, &draft, &spec);
                conversation = acknowledgement.conversation.clone();
                reply.send(Ok(acknowledgement)).unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
    }
    assert_eq!(tool.calls(), 1);
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::BudgetExceeded { usage }
        }) if usage == Usage::new(8, 6, 3)
    ));
    assert_finished(task.await.unwrap());
    assert_eq!(tool.calls(), 1);
}
