use super::*;

#[tokio::test(flavor = "current_thread")]
async fn shared_driver_does_not_serialize_strategy_calls() {
    let barrier = Arc::new(Barrier::new(2));
    let strategy = ScriptStrategy::new(vec![
        StrategyBehavior::Barrier(Arc::clone(&barrier), proposal(3, "first")),
        StrategyBehavior::Barrier(Arc::clone(&barrier), proposal(3, "second")),
    ]);
    let compact = Arc::new(driver(Some(strategy_port(&strategy)), 64));
    let first_driver = Arc::clone(&compact);
    let first = tokio::spawn(async move {
        run(
            &first_driver,
            completed_candidate(),
            10,
            Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await
    });
    let second_driver = Arc::clone(&compact);
    let second = tokio::spawn(async move {
        run(
            &second_driver,
            completed_candidate(),
            10,
            Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await
    });
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    assert_eq!(strategy.calls(), 2);
}
