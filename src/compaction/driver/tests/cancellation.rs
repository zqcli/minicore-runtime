use super::*;

#[tokio::test(flavor = "current_thread")]
async fn pre_cancel_and_expired_deadline_do_not_call_strategy() {
    let strategy = ScriptStrategy::new(Vec::new());
    let compact = driver(Some(strategy_port(&strategy)), 64);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        run(
            &compact,
            completed_candidate(),
            10,
            Duration::from_secs(30),
            cancellation,
        )
        .await,
        Err(CompactionError::Cancelled)
    );
    assert_eq!(
        compact
            .run(
                session_id(),
                turn_id(9),
                completed_candidate(),
                10,
                Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
                CancellationToken::new(),
            )
            .await,
        Err(CompactionError::DeadlineExceeded)
    );
    assert_eq!(strategy.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn construction_poll_panics_and_typed_errors_are_isolated_or_preserved() {
    for (behavior, expected) in [
        (
            StrategyBehavior::ConstructionPanic,
            CompactionError::Internal,
        ),
        (StrategyBehavior::FuturePanic, CompactionError::Internal),
    ] {
        let strategy = ScriptStrategy::new(vec![behavior]);
        assert_eq!(
            run(
                &driver(Some(strategy_port(&strategy)), 64),
                completed_candidate(),
                10,
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await,
            Err(expected)
        );
        assert_eq!(strategy.calls(), 1);
        assert!(strategy.requests()[0].cancellation.is_cancelled());
    }

    for error in [
        CompactionError::InvalidRequest,
        CompactionError::Unavailable,
        CompactionError::Cancelled,
        CompactionError::DeadlineExceeded,
        CompactionError::Internal,
    ] {
        let strategy = ScriptStrategy::new(vec![StrategyBehavior::Error(error)]);
        assert_eq!(
            run(
                &driver(Some(strategy_port(&strategy)), 64),
                completed_candidate(),
                10,
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await,
            Err(error)
        );
        assert!(strategy.requests()[0].cancellation.is_cancelled());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_during_strategy_cancels_child_before_future_drop() {
    let probe = FutureProbe::shared();
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Pending(Arc::clone(&probe))]);
    let compact = driver(Some(strategy_port(&strategy)), 64);
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        run(
            &compact,
            completed_candidate(),
            10,
            Duration::from_secs(30),
            task_cancellation,
        )
        .await
    });
    probe.wait_polled().await;
    cancellation.cancel();
    assert_eq!(task.await.unwrap(), Err(CompactionError::Cancelled));
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_cancels_child_before_future_drop() {
    let probe = FutureProbe::shared();
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Pending(Arc::clone(&probe))]);
    let compact = driver(Some(strategy_port(&strategy)), 64);
    let task = tokio::spawn(async move {
        run(
            &compact,
            completed_candidate(),
            10,
            Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await
    });
    probe.wait_polled().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(task.await.unwrap(), Err(CompactionError::DeadlineExceeded));
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
}
