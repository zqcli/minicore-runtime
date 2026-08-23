use super::*;

async fn answer_input(
    driver: ToolDriver,
    invocation: ToolInvocation,
    answer: InteractionAnswer,
) -> (Result<ToolDriverResult, SuspensionError>, InteractionKind) {
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation,
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    let suspension = suspension_rx.recv().await.unwrap();
    let kind = suspension.kind.clone();
    suspension.resume.send(Ok(answer)).unwrap();
    (run.await.unwrap(), kind)
}

#[tokio::test(flavor = "current_thread")]
async fn text_input_drops_future_escapes_json_and_rejects_controls() {
    let dropped = Arc::new(AtomicBool::new(false));
    let request = input_request(ToolInputAnswerKind::Text);
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            request.clone(),
            Arc::clone(&dropped),
        )],
    );
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let answer = InteractionAnswer::ToolInput(ToolInputAnswer::Text(
        BoundedText::new("say \"hi\" \\ path").unwrap(),
    ));
    let expected = invocation("search", 70, json!({"query": "rust"}));
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn({
        let expected = expected.clone();
        async move {
            driver
                .run(
                    expected,
                    deadline_after(Duration::from_secs(30)),
                    CancellationToken::new(),
                    &suspensions,
                    &progress,
                )
                .await
        }
    });
    let suspension = suspension_rx.recv().await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert!(
        matches!(&suspension.kind, InteractionKind::ToolInput(actual)
        if actual == &request)
    );
    suspension.resume.send(Ok(answer)).unwrap();
    let result = run.await.unwrap().unwrap();
    assert_outcome(
        &result,
        ToolResultOutcome::InputProvided,
        r#"{"answer":"say \"hi\" \\ path"}"#,
    );
    let control = ToolInputAnswer::Text(BoundedText::new("line\nbreak").unwrap());
    assert_eq!(
        control.encode_result(&request),
        Err(ToolValueError::InvalidAnswer)
    );
    assert_eq!(tool.calls(), 1);
    assert_eq!(tool.invocations(), vec![expected]);
}

#[tokio::test(flavor = "current_thread")]
async fn choice_input_includes_selected_text_without_reexecuting_tool() {
    let dropped = Arc::new(AtomicBool::new(false));
    let request = ToolInputRequest::new(
        "provide input",
        vec![
            BoundedText::new("alpha").unwrap(),
            BoundedText::new("b\"eta\\path").unwrap(),
        ],
        ToolInputAnswerKind::SingleChoice,
    )
    .unwrap();
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            request.clone(),
            Arc::clone(&dropped),
        )],
    );
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (result, kind) = answer_input(
        driver,
        invocation("search", 71, json!({})),
        InteractionAnswer::ToolInput(ToolInputAnswer::Choice { index: 1 }),
    )
    .await;

    assert!(dropped.load(Ordering::SeqCst));
    assert!(matches!(kind, InteractionKind::ToolInput(ref actual)
        if actual == &request));
    let result = result.unwrap();
    assert_outcome(
        &result,
        ToolResultOutcome::InputProvided,
        r#"{"choice_index":1,"choice":"b\"eta\\path"}"#,
    );
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_input_request_answer_and_encoded_output_bound_fail_safely() {
    let mut invalid_request = input_request(ToolInputAnswerKind::SingleChoice);
    invalid_request.choices.clear();
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            invalid_request,
            Arc::new(AtomicBool::new(false)),
        )],
    );
    let policy = allow_policy();
    let tool_driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let result = tool_driver
        .run(
            invocation("search", 72, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, ToolResultOutcome::Failed);
    assert!(suspension_rx.try_recv().is_err());

    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            input_request(ToolInputAnswerKind::SingleChoice),
            Arc::new(AtomicBool::new(false)),
        )],
    );
    let policy = allow_policy();
    let tool_driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (result, _) = answer_input(
        tool_driver,
        invocation("search", 73, json!({})),
        InteractionAnswer::ToolInput(ToolInputAnswer::Choice { index: 9 }),
    )
    .await;
    assert_eq!(result, Err(SuspensionError::InvalidState));
    assert_eq!(tool.calls(), 1);

    let encoded = r#"{"answer":"x"}"#;
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            input_request(ToolInputAnswerKind::Text),
            Arc::new(AtomicBool::new(false)),
        )],
    );
    let policy = allow_policy();
    let driver = driver(
        Arc::clone(&tool),
        Some(policy_port(&policy)),
        config_with(
            Duration::from_secs(5),
            Duration::from_secs(10),
            MAX_JSON_BYTES,
            encoded.len() - 1,
        ),
    );
    let (result, _) = answer_input(
        driver,
        invocation("search", 74, json!({})),
        InteractionAnswer::ToolInput(ToolInputAnswer::Text(BoundedText::new("x").unwrap())),
    )
    .await;
    let result = result.unwrap();
    assert_eq!(result.outcome, ToolResultOutcome::Failed);
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn encoded_input_output_succeeds_at_exact_semantic_boundary() {
    let encoded = r#"{"answer":"x"}"#;
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            input_request(ToolInputAnswerKind::Text),
            Arc::new(AtomicBool::new(false)),
        )],
    );
    let policy = allow_policy();
    let driver = driver(
        Arc::clone(&tool),
        Some(policy_port(&policy)),
        config_with(
            Duration::from_secs(5),
            Duration::from_secs(10),
            MAX_JSON_BYTES,
            encoded.len(),
        ),
    );
    let (result, _) = answer_input(
        driver,
        invocation("search", 75, json!({})),
        InteractionAnswer::ToolInput(ToolInputAnswer::Text(BoundedText::new("x").unwrap())),
    )
    .await;
    let result = result.unwrap();
    assert_outcome(&result, ToolResultOutcome::InputProvided, encoded);
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_and_deadline_while_waiting_for_tool_input_are_exact_suspension_errors() {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            input_request(ToolInputAnswerKind::Text),
            Arc::new(AtomicBool::new(false)),
        )],
    );
    let policy = allow_policy();
    let tool_driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        tool_driver
            .run(
                invocation("search", 82, json!({})),
                deadline_after(Duration::from_secs(30)),
                task_cancellation,
                &suspensions,
                &progress,
            )
            .await
    });
    let suspension = suspension_rx.recv().await.unwrap();
    cancellation.cancel();
    assert_eq!(run.await.unwrap(), Err(SuspensionError::Cancelled));
    assert!(
        suspension
            .resume
            .send(Ok(InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                BoundedText::new("late").unwrap(),
            ))))
            .is_err()
    );

    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::RequestInput(
            input_request(ToolInputAnswerKind::Text),
            Arc::new(AtomicBool::new(false)),
        )],
    );
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 83, json!({})),
                deadline_after(Duration::from_secs(4)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    let suspension = suspension_rx.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(run.await.unwrap(), Err(SuspensionError::DeadlineExceeded));
    assert!(
        suspension
            .resume
            .send(Ok(InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                BoundedText::new("late").unwrap(),
            ))))
            .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn progress_is_lossy_for_accepted_full_closed_and_invalid_values() {
    let valid =
        ToolProgress::new(Some(BoundedText::new("working").unwrap()), Some(1), Some(2)).unwrap();
    let invalid = ToolProgress {
        message: None,
        completed: Some(2),
        total: Some(1),
    };
    let tool = ScriptTool::new(
        "search",
        vec![
            ToolBehavior::Progress(vec![valid.clone()], ToolOutput::new("accepted").unwrap()),
            ToolBehavior::Progress(vec![valid.clone()], ToolOutput::new("full").unwrap()),
            ToolBehavior::Progress(vec![valid.clone()], ToolOutput::new("closed").unwrap()),
            ToolBehavior::Progress(vec![invalid], ToolOutput::new("invalid").unwrap()),
        ],
    );
    let policy = ScriptPolicy::new(vec![
        PolicyBehavior::Decision(ToolDecision::Allow),
        PolicyBehavior::Decision(ToolDecision::Allow),
        PolicyBehavior::Decision(ToolDecision::Allow),
        PolicyBehavior::Decision(ToolDecision::Allow),
    ]);
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, _suspension_rx) = mpsc::channel(1);

    let (progress, mut progress_rx) = mpsc::channel(2);
    let result = driver
        .run(
            invocation("search", 76, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, ToolResultOutcome::Success);
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ToolDriverProgress::Started { tool_call_id, tool_name })
            if tool_call_id == call_id(76) && tool_name.as_str() == "search"
    ));
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ToolDriverProgress::Update { tool_call_id, progress })
            if tool_call_id == call_id(76) && progress == valid
    ));

    progress
        .try_send(ToolDriverProgress::Update {
            tool_call_id: call_id(99),
            progress: valid.clone(),
        })
        .unwrap();
    let result = driver
        .run(
            invocation("search", 77, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_outcome(&result, ToolResultOutcome::Success, "full");
    assert_eq!(progress_rx.try_recv().unwrap().tool_call_id(), &call_id(99));

    let (closed, closed_rx) = mpsc::channel(1);
    drop(closed_rx);
    let result = driver
        .run(
            invocation("search", 78, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &closed,
        )
        .await
        .unwrap();
    assert_outcome(&result, ToolResultOutcome::Success, "closed");

    let (progress, mut progress_rx) = mpsc::channel(2);
    let result = driver
        .run(
            invocation("search", 79, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_outcome(&result, ToolResultOutcome::Success, "invalid");
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ToolDriverProgress::Started { tool_call_id, .. }) if tool_call_id == call_id(79)
    ));
    assert!(progress_rx.try_recv().is_err());
}
