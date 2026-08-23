use super::*;
use crate::time::DeadlineSource;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn pending_provider_reports_exact_turn_or_port_deadline_source() {
    for (turn_after, port_timeout, expected) in [
        (
            Duration::from_secs(5),
            Duration::from_secs(30),
            DeadlineSource::Turn,
        ),
        (
            Duration::from_secs(30),
            Duration::from_secs(5),
            DeadlineSource::Port,
        ),
    ] {
        let probe = StreamProbe::shared();
        let provider = ScriptProvider::new(vec![ProviderBehavior::Pending(Arc::clone(&probe))]);
        let driver =
            ContextDriver::new(Some(provider_port(&provider)), port_timeout, limits()).unwrap();
        let run = tokio::spawn(async move {
            driver
                .provide_detailed(request(turn_after, CancellationToken::new()))
                .await
        });
        probe.wait_polled().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let failure = run.await.unwrap().unwrap_err();
        assert_eq!(failure.error(), ContextError::DeadlineExceeded);
        assert_eq!(failure.deadline_source(), Some(expected));
        assert!(probe.dropped.load(Ordering::SeqCst));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn provider_returned_deadline_has_no_core_deadline_provenance() {
    let provider = ScriptProvider::new(vec![ProviderBehavior::Error(
        ContextError::DeadlineExceeded,
    )]);
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let failure = driver
        .provide_detailed(request(Duration::from_secs(30), CancellationToken::new()))
        .await
        .unwrap_err();
    assert_eq!(failure.error(), ContextError::DeadlineExceeded);
    assert_eq!(failure.deadline_source(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn expired_turn_deadline_is_reported_before_provider_invocation() {
    let provider = ScriptProvider::new(Vec::new());
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(30),
        limits(),
    )
    .unwrap();
    let mut value = request(Duration::from_secs(30), CancellationToken::new());
    value.deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let failure = driver.provide_detailed(value).await.unwrap_err();
    assert_eq!(failure.error(), ContextError::DeadlineExceeded);
    assert_eq!(failure.deadline_source(), Some(DeadlineSource::Turn));
    assert_eq!(provider.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_panic_have_no_deadline_provenance() {
    let provider = ScriptProvider::new(Vec::new());
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let failure = driver
        .provide_detailed(request(Duration::from_secs(30), cancellation))
        .await
        .unwrap_err();
    assert_eq!(failure.error(), ContextError::Cancelled);
    assert_eq!(failure.deadline_source(), None);

    let provider = ScriptProvider::new(vec![ProviderBehavior::ConstructionPanic]);
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let failure = driver
        .provide_detailed(request(Duration::from_secs(30), CancellationToken::new()))
        .await
        .unwrap_err();
    assert_eq!(failure.error(), ContextError::Internal);
    assert_eq!(failure.deadline_source(), None);
}
