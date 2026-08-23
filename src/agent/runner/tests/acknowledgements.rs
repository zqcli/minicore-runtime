use super::*;

#[tokio::test(flavor = "current_thread")]
async fn assistant_ack_cannot_replace_an_earlier_canonical_user_entry() {
    let usage = Usage::new(3, 2, 1);
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(tool_events(&[(21, "search")], usage))],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("unused").unwrap())],
    );
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(
            Arc::clone(&model),
            None,
            vec![Arc::clone(&tool)],
            Some(ScriptPolicy::new(vec![ToolDecision::Allow])),
        ),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let acknowledgement = ack_assistant(&initial, &draft, &spec);
            let mut entries = acknowledgement.conversation.entries().to_vec();
            match &mut entries[0] {
                ConversationEntry::UserMessage(entry) => {
                    entry.input = UserInputRecord::new(
                        BoundedText::new("forged earlier user input").unwrap(),
                    )
                    .unwrap();
                }
                entry => panic!("unexpected entry: {entry:?}"),
            }
            let conversation =
                ConversationView::from_confirmed(acknowledgement.head, entries.into());
            conversation
                .validated_prompt_projection(&spec, &SemanticLimits::default())
                .unwrap();
            reply
                .send(Ok(CommitAck {
                    head: acknowledgement.head,
                    conversation,
                }))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }

    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(outcome.usage(), usage);
    assert_eq!(
        outcome.diagnostic().unwrap().code,
        crate::error::DiagnosticCode::SessionBusy
    );
    assert_eq!(
        task.await.unwrap(),
        TurnRunnerExit::Finished {
            outcome: outcome.clone(),
        }
    );
    assert_eq!(tool.calls(), 0);
    assert_eq!(model.requests().len(), 1);
    assert!(critical_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn tool_ack_cannot_replace_an_earlier_canonical_assistant_entry() {
    let usage = Usage::new(5, 3, 1);
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(22, "search")], usage)),
            ModelBehavior::Events(final_events("must not run", Usage::default())),
        ],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("result").unwrap())],
    );
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(
            Arc::clone(&model),
            None,
            vec![Arc::clone(&tool)],
            Some(ScriptPolicy::new(vec![ToolDecision::Allow])),
        ),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));
    let mut conversation = initial;

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let acknowledgement = ack_assistant(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitToolResult { draft, reply } => {
            let acknowledgement = ack_tool(&conversation, &draft, &spec);
            let mut entries = acknowledgement.conversation.entries().to_vec();
            match &mut entries[1] {
                ConversationEntry::AssistantMessage(entry) => {
                    entry.usage = Usage::new(99, 98, 97);
                }
                entry => panic!("unexpected entry: {entry:?}"),
            }
            let replacement =
                ConversationView::from_confirmed(acknowledgement.head, entries.into());
            replacement
                .validated_prompt_projection(&spec, &SemanticLimits::default())
                .unwrap();
            reply
                .send(Ok(CommitAck {
                    head: acknowledgement.head,
                    conversation: replacement,
                }))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }

    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(outcome.usage(), usage);
    assert_eq!(
        outcome.diagnostic().unwrap().code,
        crate::error::DiagnosticCode::SessionBusy
    );
    assert_eq!(
        task.await.unwrap(),
        TurnRunnerExit::Finished {
            outcome: outcome.clone(),
        }
    );
    assert_eq!(tool.calls(), 1);
    assert_eq!(model.requests().len(), 1);
    assert!(critical_rx.try_recv().is_err());
}
