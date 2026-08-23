use super::*;

#[tokio::test(flavor = "current_thread")]
async fn policy_construction_poll_error_and_invalid_decisions_are_safe_denials() {
    let invalid_reason = ToolDecision::Deny {
        reason: BoundedText::new("x".repeat(crate::tools::MAX_TOOL_POLICY_TEXT_BYTES + 1)).unwrap(),
    };
    let invalid_approval = ToolDecision::RequireApproval {
        request: ApprovalRequest {
            prompt: BoundedText::new("x".repeat(crate::tools::MAX_TOOL_POLICY_TEXT_BYTES + 1))
                .unwrap(),
            risk: ApprovalRisk::High,
        },
    };
    let behaviors = vec![
        PolicyBehavior::ConstructionPanic,
        PolicyBehavior::FuturePanic,
        PolicyBehavior::Error(ToolPolicyError::Failed),
        PolicyBehavior::Error(ToolPolicyError::Cancelled),
        PolicyBehavior::Decision(invalid_reason),
        PolicyBehavior::Decision(invalid_approval),
    ];

    for (index, behavior) in behaviors.into_iter().enumerate() {
        let tool = ScriptTool::new(
            "search",
            vec![ToolBehavior::Complete(
                ToolOutput::new("unexpected").unwrap(),
            )],
        );
        let policy = ScriptPolicy::new(vec![behavior]);
        let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
        let (suspensions, mut suspension_rx, progress, _progress_rx) = channels();
        let result = driver
            .run(
                invocation("search", 20 + index as u8, json!({})),
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
            .unwrap();

        assert_outcome(&result, ToolResultOutcome::Denied, "tool denied");
        assert_eq!(tool.calls(), 0);
        assert!(suspension_rx.try_recv().is_err());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_policy_call_is_cancelled_without_invocation() {
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(
            ToolOutput::new("unexpected").unwrap(),
        )],
    );
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let result = driver
        .run(
            invocation("search", 30, json!({})),
            deadline_after(Duration::from_secs(30)),
            cancellation,
            &suspensions,
            &progress,
        )
        .await
        .unwrap();

    assert_eq!(result.outcome, ToolResultOutcome::Cancelled);
    assert_eq!(policy.calls(), 0);
    assert_eq!(tool.calls(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn expired_policy_deadline_denies_without_calling_policy() {
    let tool = ScriptTool::new("search", Vec::new());
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let result = driver
        .run(
            invocation("search", 31, json!({})),
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();

    assert_outcome(&result, ToolResultOutcome::Denied, "tool denied");
    assert_eq!(policy.calls(), 0);
    assert_eq!(tool.calls(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn policy_timeout_cancels_child_before_drop_and_denies() {
    let probe = OperationProbe::shared();
    let tool = ScriptTool::new("search", Vec::new());
    let policy = ScriptPolicy::new(vec![PolicyBehavior::Pending(Arc::clone(&probe))]);
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 32, json!({})),
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    probe.wait_polled().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    let result = run.await.unwrap().unwrap();

    assert_outcome(&result, ToolResultOutcome::Denied, "tool denied");
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
    assert_eq!(tool.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn turn_cancellation_during_policy_cancels_child_before_drop() {
    let probe = OperationProbe::shared();
    let tool = ScriptTool::new("search", Vec::new());
    let policy = ScriptPolicy::new(vec![PolicyBehavior::Pending(Arc::clone(&probe))]);
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let run = tokio::spawn(async move {
        driver
            .run(
                invocation("search", 33, json!({})),
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
    assert_eq!(tool.calls(), 0);
}
