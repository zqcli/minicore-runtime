use super::*;

fn approval_policy(dropped: Option<Arc<AtomicBool>>) -> Arc<ScriptPolicy> {
    let decision = ToolDecision::require_approval(approval_request()).unwrap();
    match dropped {
        Some(dropped) => {
            ScriptPolicy::new(vec![PolicyBehavior::DecisionWithDrop(decision, dropped)])
        }
        None => ScriptPolicy::new(vec![PolicyBehavior::Decision(decision)]),
    }
}

fn dummy_suspension() -> TurnSuspension {
    let (resume, _receiver) = tokio::sync::oneshot::channel();
    TurnSuspension {
        turn_id: turn_id(),
        tool_call_id: call_id(90),
        tool_name: "search".parse().unwrap(),
        kind: InteractionKind::Approval(approval_request()),
        resume,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn approval_suspends_exact_identity_then_allow_once_executes_exactly_once() {
    let policy_dropped = Arc::new(AtomicBool::new(false));
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("approved").unwrap())],
    );
    let policy = approval_policy(Some(Arc::clone(&policy_dropped)));
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let expected = invocation("search", 40, json!({"query": "rust"}));
    let (suspensions, mut suspension_rx, progress, mut progress_rx) = channels();
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

    assert!(policy_dropped.load(Ordering::SeqCst));
    assert_eq!(suspension.turn_id, expected.turn_id());
    assert_eq!(&suspension.tool_call_id, expected.tool_call_id());
    assert_eq!(&suspension.tool_name, expected.tool_name());
    assert!(matches!(&suspension.kind, InteractionKind::Approval(_)));
    assert!(!format!("{suspension:?}").contains("approve operation"));
    suspension
        .resume
        .send(Ok(InteractionAnswer::Approval(ApprovalDecision::AllowOnce)))
        .unwrap();
    let result = run.await.unwrap().unwrap();

    assert_outcome(&result, ToolResultOutcome::Success, "approved");
    assert_eq!(tool.calls(), 1);
    assert_eq!(tool.invocations(), vec![expected]);
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ToolDriverProgress::Started { tool_call_id, tool_name })
            if tool_call_id == call_id(40) && tool_name.as_str() == "search"
    ));
    assert!(progress_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn approval_deny_and_wrong_answer_never_execute_tool() {
    for (answer, expected) in [
        (
            InteractionAnswer::Approval(ApprovalDecision::Deny),
            Ok(ToolResultOutcome::Denied),
        ),
        (
            InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                BoundedText::new("wrong kind").unwrap(),
            )),
            Err(SuspensionError::InvalidState),
        ),
    ] {
        let tool = ScriptTool::new(
            "search",
            vec![ToolBehavior::Complete(
                ToolOutput::new("unexpected").unwrap(),
            )],
        );
        let policy = approval_policy(None);
        let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
        let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
        let run = tokio::spawn(async move {
            driver
                .run(
                    invocation("search", 41, json!({})),
                    deadline_after(Duration::from_secs(30)),
                    CancellationToken::new(),
                    &suspensions,
                    &progress,
                )
                .await
        });
        let suspension = suspension_rx.recv().await.unwrap();
        suspension.resume.send(Ok(answer.clone())).unwrap();
        let result = run.await.unwrap();
        assert_eq!(result.map(|result| result.outcome), expected);
        assert_eq!(tool.calls(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn closed_suspension_sender_and_typed_resume_errors_settle_safely() {
    let tool = ScriptTool::new("search", Vec::new());
    let policy = approval_policy(None);
    let tool_driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, suspension_rx, progress, _progress_rx) = channels();
    drop(suspension_rx);
    let result = tool_driver
        .run(
            invocation("search", 42, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await;
    assert_eq!(result, Err(SuspensionError::RuntimeClosed));

    for (index, error) in [
        (0, SuspensionError::Cancelled),
        (1, SuspensionError::DeadlineExceeded),
        (2, SuspensionError::StaleTurn),
        (3, SuspensionError::InvalidState),
        (4, SuspensionError::RuntimeClosed),
    ] {
        let policy = approval_policy(None);
        let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
        let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
        let run = tokio::spawn(async move {
            driver
                .run(
                    invocation("search", 43 + index, json!({})),
                    deadline_after(Duration::from_secs(30)),
                    CancellationToken::new(),
                    &suspensions,
                    &progress,
                )
                .await
        });
        let suspension = suspension_rx.recv().await.unwrap();
        suspension.resume.send(Err(error)).unwrap();
        assert_eq!(run.await.unwrap(), Err(error));
    }
    assert_eq!(tool.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn actor_dropping_resume_sender_returns_exact_runtime_closed() {
    let tool = ScriptTool::new("search", Vec::new());
    let policy = approval_policy(None);
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 48, json!({})),
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    let suspension = suspension_rx.recv().await.unwrap();
    drop(suspension.resume);

    assert_eq!(run.await.unwrap(), Err(SuspensionError::RuntimeClosed));
    assert_eq!(tool.calls(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_and_deadline_interrupt_blocked_critical_suspension_send() {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let tool = ScriptTool::new("search", Vec::new());
    let policy = approval_policy(None);
    let tool_driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx) = mpsc::channel(1);
    suspensions.try_send(dummy_suspension()).unwrap();
    let (progress, _progress_rx) = mpsc::channel(1);
    let run = tokio::spawn(async move {
        tool_driver
            .run(
                invocation("search", 50, json!({})),
                deadline_after(Duration::from_secs(30)),
                task_cancellation,
                &suspensions,
                &progress,
            )
            .await
    });
    policy.wait_completions(1).await;
    assert!(!run.is_finished());
    cancellation.cancel();
    assert_eq!(run.await.unwrap(), Err(SuspensionError::Cancelled));
    assert_eq!(suspension_rx.try_recv().unwrap().tool_call_id, call_id(90));
    assert!(matches!(
        suspension_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));

    let policy = approval_policy(None);
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx) = mpsc::channel(1);
    suspensions.try_send(dummy_suspension()).unwrap();
    let (progress, _progress_rx) = mpsc::channel(1);
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 51, json!({})),
                deadline_after(Duration::from_secs(4)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    policy.wait_completions(1).await;
    assert!(!run.is_finished());
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(run.await.unwrap(), Err(SuspensionError::DeadlineExceeded));
    assert_eq!(suspension_rx.try_recv().unwrap().tool_call_id, call_id(90));
    assert!(matches!(
        suspension_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_and_deadline_interrupt_waiting_for_approval_answer() {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let tool = ScriptTool::new("search", Vec::new());
    let policy = approval_policy(None);
    let tool_driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        tool_driver
            .run(
                invocation("search", 52, json!({})),
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
            .send(Ok(
                InteractionAnswer::Approval(ApprovalDecision::AllowOnce,)
            ))
            .is_err()
    );

    let policy = approval_policy(None);
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 53, json!({})),
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
            .send(Ok(
                InteractionAnswer::Approval(ApprovalDecision::AllowOnce,)
            ))
            .is_err()
    );
}
