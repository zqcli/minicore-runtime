use super::*;

#[tokio::test(flavor = "current_thread")]
async fn no_provider_returns_empty_validated_bundle() {
    let driver = ContextDriver::new(None, Duration::from_secs(5), limits()).unwrap();
    let result = driver
        .provide(request(Duration::from_secs(30), CancellationToken::new()))
        .await
        .unwrap();
    assert!(result.blocks.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn pre_cancel_and_expired_deadline_do_not_call_provider() {
    let provider = ScriptProvider::new(Vec::new());
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        driver
            .provide(request(Duration::from_secs(30), cancellation))
            .await,
        Err(ContextError::Cancelled)
    );
    let mut expired = request(Duration::from_secs(30), CancellationToken::new());
    expired.deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    assert_eq!(
        driver.provide(expired).await,
        Err(ContextError::DeadlineExceeded)
    );
    assert_eq!(provider.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn construction_poll_panic_and_typed_error_are_preserved_or_isolated() {
    for (behavior, expected) in [
        (ProviderBehavior::ConstructionPanic, ContextError::Internal),
        (ProviderBehavior::FuturePanic, ContextError::Internal),
        (
            ProviderBehavior::Error(ContextError::Unavailable),
            ContextError::Unavailable,
        ),
    ] {
        let provider = ScriptProvider::new(vec![behavior]);
        let driver = ContextDriver::new(
            Some(provider_port(&provider)),
            Duration::from_secs(5),
            limits(),
        )
        .unwrap();
        assert_eq!(
            driver
                .provide(request(Duration::from_secs(30), CancellationToken::new(),))
                .await,
            Err(expected)
        );
        assert_eq!(provider.calls(), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_during_provider_drops_future() {
    let probe = StreamProbe::shared();
    let provider = ScriptProvider::new(vec![ProviderBehavior::Pending(Arc::clone(&probe))]);
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let run = tokio::spawn(async move {
        driver
            .provide(request(Duration::from_secs(30), task_cancellation))
            .await
    });
    probe.wait_polled().await;
    cancellation.cancel();
    assert_eq!(run.await.unwrap(), Err(ContextError::Cancelled));
    assert!(probe.dropped.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_during_provider_drops_future() {
    let probe = StreamProbe::shared();
    let provider = ScriptProvider::new(vec![ProviderBehavior::Pending(Arc::clone(&probe))]);
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let run = tokio::spawn(async move {
        driver
            .provide(request(Duration::from_secs(30), CancellationToken::new()))
            .await
    });
    probe.wait_polled().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(run.await.unwrap(), Err(ContextError::DeadlineExceeded));
    assert!(probe.dropped.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn shorter_request_deadline_is_observed_exactly() {
    let provider = ScriptProvider::new(vec![ProviderBehavior::Bundle(bundle(Vec::new()))]);
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let expected = Instant::now() + Duration::from_secs(2);
    let mut value = request(Duration::from_secs(30), CancellationToken::new());
    value.deadline = expected;
    assert!(driver.provide(value).await.is_ok());
    assert_eq!(provider.requests()[0].deadline, expected);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shorter_configured_timeout_is_observed_exactly() {
    let provider = ScriptProvider::new(vec![ProviderBehavior::Bundle(bundle(Vec::new()))]);
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(2),
        limits(),
    )
    .unwrap();
    let expected = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(2))
        .unwrap()
        .into_std();
    assert!(
        driver
            .provide(request(Duration::from_secs(30), CancellationToken::new(),))
            .await
            .is_ok()
    );
    assert_eq!(provider.requests()[0].deadline, expected);
}
