use super::*;

#[tokio::test(flavor = "current_thread")]
async fn shared_tool_driver_has_no_global_execution_lock() {
    let gate = Arc::new(Barrier::new(2));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let behavior = || ToolBehavior::Barrier {
        gate: Arc::clone(&gate),
        active: Arc::clone(&active),
        max_active: Arc::clone(&max_active),
    };
    let tool = ScriptTool::new("search", vec![behavior(), behavior()]);
    let policy = ScriptPolicy::new(vec![
        PolicyBehavior::Decision(ToolDecision::Allow),
        PolicyBehavior::Decision(ToolDecision::Allow),
    ]);
    let driver = Arc::new(driver(
        Arc::clone(&tool),
        Some(policy_port(&policy)),
        config(),
    ));

    let first_driver = Arc::clone(&driver);
    let first = tokio::spawn(async move {
        let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
        first_driver
            .run(
                invocation("search", 80, json!({})),
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });
    let second_driver = Arc::clone(&driver);
    let second = tokio::spawn(async move {
        let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
        second_driver
            .run(
                invocation("search", 81, json!({})),
                deadline_after(Duration::from_secs(30)),
                CancellationToken::new(),
                &suspensions,
                &progress,
            )
            .await
    });

    assert_eq!(
        first.await.unwrap().unwrap().outcome,
        ToolResultOutcome::Success
    );
    assert_eq!(
        second.await.unwrap().unwrap().outcome,
        ToolResultOutcome::Success
    );
    assert_eq!(tool.calls(), 2);
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
}
