use super::*;

#[tokio::test(flavor = "current_thread")]
async fn shared_context_driver_does_not_serialize_provider_calls() {
    let barrier = Arc::new(Barrier::new(2));
    let provider = ScriptProvider::new(vec![
        ProviderBehavior::Barrier(Arc::clone(&barrier), bundle(Vec::new())),
        ProviderBehavior::Barrier(Arc::clone(&barrier), bundle(Vec::new())),
    ]);
    let driver = Arc::new(
        ContextDriver::new(
            Some(provider_port(&provider)),
            Duration::from_secs(5),
            limits(),
        )
        .unwrap(),
    );
    let first_driver = Arc::clone(&driver);
    let first = tokio::spawn(async move {
        first_driver
            .provide(request(Duration::from_secs(30), CancellationToken::new()))
            .await
    });
    let second_driver = Arc::clone(&driver);
    let second = tokio::spawn(async move {
        second_driver
            .provide(request(Duration::from_secs(30), CancellationToken::new()))
            .await
    });
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    assert_eq!(provider.calls(), 2);
}
