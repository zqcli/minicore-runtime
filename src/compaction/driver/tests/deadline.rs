use super::*;
use crate::time::DeadlineSource;

#[tokio::test(flavor = "current_thread")]
async fn turn_control_precedes_target_candidate_and_strategy_availability() {
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let failure = driver(None, 64)
        .run_detailed(
            session_id(),
            turn_id(9),
            CompactionCandidate::empty(),
            0,
            Instant::now() + Duration::from_secs(30),
            cancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(failure.error(), CompactionError::Cancelled);
    assert_eq!(failure.deadline_source(), None);

    let strategy = ScriptStrategy::new(Vec::new());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let failure = driver(Some(strategy_port(&strategy)), 64)
        .run_detailed(
            session_id(),
            turn_id(9),
            CompactionCandidate::empty(),
            0,
            Instant::now() + Duration::from_secs(30),
            cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(failure.error(), CompactionError::Cancelled);
    assert_eq!(strategy.calls(), 0);

    let strategy = ScriptStrategy::new(Vec::new());
    let failure = driver(Some(strategy_port(&strategy)), 64)
        .run_detailed(
            session_id(),
            turn_id(9),
            CompactionCandidate::empty(),
            0,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.error(), CompactionError::DeadlineExceeded);
    assert_eq!(failure.deadline_source(), Some(DeadlineSource::Turn));
    assert_eq!(strategy.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_after_candidate_validation_wins_before_strategy_invocation() {
    let strategy = ScriptStrategy::new(Vec::new());
    let compact = driver(Some(strategy_port(&strategy)), 64);
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let race_turn = turn_id(88);
    let hook = install_candidate_control_hook(race_turn);
    let task = tokio::spawn(async move {
        compact
            .run_detailed(
                session_id(),
                race_turn,
                completed_candidate(),
                10,
                Instant::now() + Duration::from_secs(30),
                task_cancellation,
            )
            .await
    });
    hook.wait_reached().await;
    cancellation.cancel();
    hook.release().await;
    let failure = task.await.unwrap().unwrap_err();
    assert_eq!(failure.error(), CompactionError::Cancelled);
    assert_eq!(failure.deadline_source(), None);
    assert_eq!(strategy.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn adapter_deadline_error_has_no_core_deadline_source() {
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Error(
        CompactionError::DeadlineExceeded,
    )]);
    let failure = driver(Some(strategy_port(&strategy)), 64)
        .run_detailed(
            session_id(),
            turn_id(9),
            completed_candidate(),
            10,
            Instant::now() + Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.error(), CompactionError::DeadlineExceeded);
    assert_eq!(failure.deadline_source(), None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn configured_timeout_carries_port_deadline_source() {
    let probe = FutureProbe::shared();
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Pending(Arc::clone(&probe))]);
    let compact = driver(Some(strategy_port(&strategy)), 64);
    let task = tokio::spawn(async move {
        compact
            .run_detailed(
                session_id(),
                turn_id(9),
                completed_candidate(),
                10,
                Instant::now() + Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await
    });
    probe.wait_polled().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    let failure = task.await.unwrap().unwrap_err();
    assert_eq!(failure.error(), CompactionError::DeadlineExceeded);
    assert_eq!(failure.deadline_source(), Some(DeadlineSource::Port));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn earlier_turn_deadline_carries_turn_source() {
    let probe = FutureProbe::shared();
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Pending(Arc::clone(&probe))]);
    let compact = driver(Some(strategy_port(&strategy)), 64);
    let task = tokio::spawn(async move {
        compact
            .run_detailed(
                session_id(),
                turn_id(9),
                completed_candidate(),
                10,
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
    });
    probe.wait_polled().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    let failure = task.await.unwrap().unwrap_err();
    assert_eq!(failure.error(), CompactionError::DeadlineExceeded);
    assert_eq!(failure.deadline_source(), Some(DeadlineSource::Turn));
}
