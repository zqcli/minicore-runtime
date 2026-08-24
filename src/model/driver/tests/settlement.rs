use super::*;

#[tokio::test(flavor = "current_thread")]
async fn finish_then_stream_error_is_not_success_or_retryable_after_observation() {
    let dropped = StreamProbe::shared();
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::ProbedStream {
                events: vec![
                    Ok(ModelEvent::text_delta("complete-looking").unwrap()),
                    Ok(finish(ModelFinishReason::Stop)),
                    Err(retryable_error(None)),
                ],
                terminal: StreamTerminal::Eof,
                probe: Arc::clone(&dropped),
            },
            Behavior::Events(text_success("unexpected retry")),
        ],
    );
    let driver = model.driver(&kernel(RetryPolicy::new(4, Duration::ZERO).unwrap()));
    let (progress, _receiver) = progress_channel();
    let error = driver
        .run(
            request(),
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
        )
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ModelErrorKind::ProviderUnavailable);
    assert_eq!(error.delivery(), DeliveryState::Started);
    assert_eq!(error.retry_hint(), &RetryHint::Never);
    assert_eq!(model.starts(), 1);
    assert!(dropped.dropped());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn finish_then_pending_requires_eof_and_drops_on_cancel_or_observed_timeout() {
    let cancelled_probe = StreamProbe::shared();
    let cancelled_model = ScriptModel::new(
        descriptor(),
        vec![Behavior::ProbedStream {
            events: vec![
                Ok(ModelEvent::text_delta("complete-looking").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            terminal: StreamTerminal::Pending,
            probe: Arc::clone(&cancelled_probe),
        }],
    );
    let cancelled_driver =
        cancelled_model.driver(&kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()));
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let (progress, _receiver) = progress_channel();
    let run = tokio::spawn(async move {
        cancelled_driver
            .run(
                request(),
                context(task_cancellation, Duration::from_secs(30)),
                &progress,
            )
            .await
    });
    cancelled_probe.wait_for_terminal().await;
    assert!(!run.is_finished());
    assert!(!cancelled_probe.dropped());
    cancellation.cancel();
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::Started);
    assert!(cancelled_probe.dropped());

    let deadline_probe = StreamProbe::shared();
    let deadline_model = ScriptModel::new(
        descriptor(),
        vec![Behavior::ProbedStream {
            events: vec![
                Ok(ModelEvent::text_delta("complete-looking").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            terminal: StreamTerminal::Pending,
            probe: Arc::clone(&deadline_probe),
        }],
    );
    let config = KernelConfig {
        model_call_timeout: Duration::from_secs(5),
        ..kernel(RetryPolicy::new(1, Duration::ZERO).unwrap())
    };
    let deadline_driver = deadline_model.driver(&config);
    let (progress, _receiver) = progress_channel();
    let run = tokio::spawn(async move {
        deadline_driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
    });
    deadline_probe.wait_for_terminal().await;
    assert!(!run.is_finished());
    assert!(!deadline_probe.dropped());
    tokio::time::advance(Duration::from_secs(5)).await;
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Timeout);
    assert_eq!(error.delivery(), DeliveryState::Started);
    assert!(deadline_probe.dropped());
}

#[tokio::test(flavor = "current_thread")]
async fn event_then_stream_poll_panic_is_started_dropped_and_never_retried() {
    let dropped = StreamProbe::shared();
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::ProbedStream {
                events: vec![Ok(ModelEvent::text_delta("observed").unwrap())],
                terminal: StreamTerminal::Panic,
                probe: Arc::clone(&dropped),
            },
            Behavior::Events(text_success("unexpected retry")),
        ],
    );
    let driver = model.driver(&kernel(RetryPolicy::new(4, Duration::ZERO).unwrap()));
    let (progress, mut progress_rx) = progress_channel();
    let error = driver
        .run(
            request(),
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
        )
        .await
        .unwrap_err();

    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ModelDriverProgress::TextDelta(delta)) if delta.as_str() == "observed"
    ));
    assert_eq!(error.kind(), ModelErrorKind::Panicked);
    assert_eq!(error.delivery(), DeliveryState::Started);
    assert_eq!(error.retry_hint(), &RetryHint::Never);
    assert_eq!(model.starts(), 1);
    assert!(dropped.dropped());
}
