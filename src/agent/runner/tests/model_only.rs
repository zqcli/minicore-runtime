use super::*;

#[tokio::test(flavor = "current_thread")]
async fn model_only_turn_uses_exact_context_prompt_commit_ack_and_finish_order() {
    let usage = Usage::new(3, 2, 1);
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events("answer", usage))],
    );
    let context = ScriptContext::new(vec![Ok(ContextBundle {
        blocks: vec![ContextBlock {
            source: "project".parse::<ContextSourceId>().unwrap(),
            slot: ContextSlot::ProjectInstructions,
            priority: 1,
            content: BoundedText::new("project context").unwrap(),
        }],
    })]);
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, mut progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(
            Arc::clone(&model),
            Some(Arc::clone(&context)),
            Vec::new(),
            None,
        ),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));

    let draft = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            assert_eq!(draft.turn_id, turn_id());
            assert_eq!(draft.text.as_ref().unwrap().as_str(), "answer");
            assert!(draft.tool_calls.is_empty());
            let acknowledgement = ack_assistant(&initial, &draft, &spec);
            reply.send(Ok(acknowledgement)).unwrap();
            draft
        }
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(draft.usage, usage);
    let outcome = joined_outcome(task).await;
    assert_eq!(outcome.usage(), usage);
    assert_eq!(outcome.diagnostic(), None);

    let context_requests = context.requests();
    assert_eq!(context_requests.len(), 1);
    assert_eq!(context_requests[0].session_id, session_id());
    assert_eq!(context_requests[0].instance_id, instance_id());
    assert_eq!(context_requests[0].turn_id, turn_id());
    assert_eq!(context_requests[0].model_round, 0);
    assert_eq!(context_requests[0].conversation, initial);
    assert!(context_requests[0].remaining_context_budget > 0);

    let model_requests = model.requests();
    assert_eq!(model_requests.len(), 1);
    let (model_request, call_context) = &model_requests[0];
    assert_eq!(call_context.session_id, session_id());
    assert_eq!(call_context.instance_id, instance_id());
    assert_eq!(call_context.turn_id, turn_id());
    assert_eq!(call_context.round, 0);
    assert!(matches!(
        model_request.messages()[0],
        ModelMessage::System(_)
    ));
    assert!(model_request.messages().iter().any(|message| {
        matches!(message, ModelMessage::System(text) if text.contains("project context"))
    }));
    assert!(matches!(
        model_request.messages().last(),
        Some(ModelMessage::User(text)) if text == "question"
    ));

    let observed_progress = std::iter::from_fn(|| progress_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(matches!(
        observed_progress.first(),
        Some(RunnerProgress::ModelStarted { model_round: 0 })
    ));
    assert!(
        observed_progress
            .iter()
            .any(|event| matches!(event, RunnerProgress::ModelProgress { model_round: 0, .. }))
    );
    assert!(matches!(
        observed_progress.last(),
        Some(RunnerProgress::ModelFinished {
            model_round: 0,
            usage: observed,
        }) if *observed == usage
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn progress_full_or_closed_never_controls_model_completion() {
    for close_progress in [false, true] {
        let model = ScriptModel::new(
            4_096,
            vec![ModelBehavior::Events(final_events(
                "answer",
                Usage::default(),
            ))],
        );
        let spec = session_spec(&[], 4);
        let initial = initial_conversation(&spec, 4);
        let kernel = KernelConfig::default_checked().unwrap();
        let (critical_tx, mut critical_rx) = mpsc::channel(4);
        let (progress_tx, progress_rx) = mpsc::channel(1);
        if !close_progress {
            progress_tx
                .try_send(RunnerProgress::ModelStarted { model_round: 99 })
                .unwrap();
        }
        if close_progress {
            drop(progress_rx);
        }
        let bindings = session_bindings(Arc::clone(&model), None, Vec::new(), None);
        let environment = SessionEnvironment::build(&kernel, &spec, &bindings).unwrap();
        let request = TurnRunnerRequest::new(
            TurnRunnerIdentity {
                session_id: session_id(),
                instance_id: instance_id(),
                turn_id: turn_id(),
            },
            environment,
            4,
            initial.clone(),
            TurnRunnerControl {
                cancellation: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(30),
                critical_tx,
                progress_tx,
            },
        )
        .unwrap();
        let task = tokio::spawn(run_turn(request));
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::CommitAssistant { draft, reply } => {
                reply
                    .send(Ok(ack_assistant(&initial, &draft, &spec)))
                    .unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
        assert!(matches!(
            joined_outcome(task).await,
            RunnerOutcome::Completed { .. }
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completed_runner_drops_its_request_owned_model_reference() {
    let dropped = Arc::new(AtomicBool::new(false));
    let model = ScriptModel::with_drop_probe(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
        Arc::clone(&dropped),
    );
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(Arc::clone(&model), None, Vec::new(), None),
        initial.clone(),
    );
    drop(model);
    let task = tokio::spawn(run_turn(request));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            reply
                .send(Ok(ack_assistant(&initial, &draft, &spec)))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::Completed { .. }
    ));
    assert!(dropped.load(Ordering::SeqCst));
}
