use super::*;

#[tokio::test(flavor = "current_thread")]
async fn tool_construction_poll_and_typed_errors_are_safe_failures() {
    let behaviors = vec![
        ToolBehavior::ConstructionPanic,
        ToolBehavior::FuturePanic,
        ToolBehavior::Error(ToolError::Failed),
        ToolBehavior::Error(ToolError::Cancelled),
        ToolBehavior::Error(ToolError::TimedOut),
    ];
    for (index, behavior) in behaviors.into_iter().enumerate() {
        let tool = ScriptTool::new("search", vec![behavior]);
        let policy = allow_policy();
        let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
        let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
        let result = driver
            .run(
                invocation("search", 60 + index as u8, json!({})),
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
            .unwrap();

        assert_eq!(result.outcome, ToolResultOutcome::Failed);
        assert_eq!(tool.calls(), 1);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn configured_tool_timeout_cancels_child_before_dropping_future() {
    let probe = OperationProbe::shared();
    let tool = ScriptTool::new("search", vec![ToolBehavior::Pending(Arc::clone(&probe))]);
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 64, json!({})),
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    probe.wait_polled().await;
    tokio::time::advance(Duration::from_secs(11)).await;
    let result = run.await.unwrap().unwrap();

    assert_eq!(result.outcome, ToolResultOutcome::Failed);
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn earlier_turn_deadline_during_tool_is_exact_control_failure() {
    let probe = OperationProbe::shared();
    let tool = ScriptTool::new("search", vec![ToolBehavior::Pending(Arc::clone(&probe))]);
    let policy = allow_policy();
    let driver = driver(
        Arc::clone(&tool),
        Some(policy_port(&policy)),
        config_with(
            Duration::from_secs(30),
            Duration::from_secs(30),
            MAX_JSON_BYTES,
            BoundedText::MAX_BYTES,
        ),
    );
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 69, json!({})),
                deadline_after(Duration::from_secs(4)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    probe.wait_polled().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(run.await.unwrap(), Err(SuspensionError::DeadlineExceeded));
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn turn_cancellation_during_tool_cancels_child_before_drop() {
    let probe = OperationProbe::shared();
    let tool = ScriptTool::new("search", vec![ToolBehavior::Pending(Arc::clone(&probe))]);
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 65, json!({})),
                deadline_after(Duration::from_secs(30)),
                task_cancellation,
                &suspensions,
                &progress,
            )
            .await
    });
    probe.wait_polled().await;
    cancellation.cancel();
    let result = run.await.unwrap().unwrap();

    assert_eq!(result.outcome, ToolResultOutcome::Cancelled);
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn completed_output_enforces_exact_session_output_limit() {
    let tool = ScriptTool::new(
        "search",
        vec![
            ToolBehavior::Complete(ToolOutput::new("abc").unwrap()),
            ToolBehavior::Complete(ToolOutput::new("abcd").unwrap()),
        ],
    );
    let policy = ScriptPolicy::new(vec![
        PolicyBehavior::Decision(ToolDecision::Allow),
        PolicyBehavior::Decision(ToolDecision::Allow),
    ]);
    let driver = driver(
        Arc::clone(&tool),
        Some(policy_port(&policy)),
        config_with(
            Duration::from_secs(5),
            Duration::from_secs(10),
            MAX_JSON_BYTES,
            3,
        ),
    );
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let exact = driver
        .run(
            invocation("search", 66, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_outcome(&exact, ToolResultOutcome::Success, "abc");

    let oversized = driver
        .run(
            invocation("search", 67, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(oversized.outcome, ToolResultOutcome::Failed);
    assert!(oversized.output.content().byte_len() <= 3);
    assert_eq!(tool.calls(), 2);
}
