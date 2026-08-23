use super::*;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn not_started_errors_retry_with_exact_attempts_and_delays() {
    let policy = RetryPolicy::new(3, Duration::from_secs(2)).unwrap();
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::StartError(retryable_error(None)),
            Behavior::StartError(retryable_error(Some(Duration::from_secs(5)))),
            Behavior::Events(text_success("done")),
        ],
    );
    let driver = model.driver(&kernel(policy));
    let (progress, _receiver) = progress_channel();
    let run = tokio::spawn(async move {
        driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
    });

    model.wait_for_completions(1).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(model.starts(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    model.wait_for_completions(2).await;
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(model.starts(), 2);
    tokio::time::advance(Duration::from_secs(1)).await;
    model.wait_for_starts(3).await;
    assert_text(&run.await.unwrap().unwrap(), "done");
    assert_eq!(model.starts(), 3);
    assert!(model.requests().iter().all(|value| value == &request()));
}

#[tokio::test(flavor = "current_thread")]
async fn excessive_retry_after_started_unknown_and_nonretryable_do_not_retry() {
    let cases = vec![
        ModelError::detailed(
            ModelErrorKind::ProviderUnavailable,
            DeliveryState::NotStarted,
            true,
            Some(Duration::from_secs(31)),
        )
        .unwrap(),
        generated_error(ModelErrorKind::ProviderUnavailable, DeliveryState::Started),
        generated_error(ModelErrorKind::ProviderUnavailable, DeliveryState::Unknown),
        ModelError::AuthRejected,
    ];
    for error in cases {
        let expected = error.kind();
        let model = ScriptModel::new(
            descriptor(),
            vec![
                Behavior::StartError(error),
                Behavior::Events(text_success("unexpected")),
            ],
        );
        let driver = model.driver(&kernel(RetryPolicy::new(4, Duration::ZERO).unwrap()));
        let (progress, _receiver) = progress_channel();
        let actual = driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
            .unwrap_err();
        assert_eq!(actual.kind(), expected);
        assert_eq!(model.starts(), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn semantic_event_blocks_retry_and_normalizes_lying_not_started_error() {
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::Events(vec![
                Ok(ModelEvent::text_delta("partial").unwrap()),
                Err(retryable_error(None)),
            ]),
            Behavior::Events(text_success("unexpected")),
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
        Ok(ModelDriverProgress::TextDelta(delta)) if delta.as_str() == "partial"
    ));
    assert!(progress_rx.try_recv().is_err());
    assert_eq!(error.kind(), ModelErrorKind::ProviderUnavailable);
    assert_eq!(error.delivery(), DeliveryState::Started);
    assert!(!error.retryable());
    assert_eq!(error.retry_after(), None);
    assert_eq!(model.starts(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn no_event_stream_error_can_retry_but_retry_must_fit_overall_deadline() {
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::Events(vec![Err(retryable_error(None))]),
            Behavior::Events(text_success("retried")),
        ],
    );
    let driver = model.driver(&kernel(RetryPolicy::new(2, Duration::ZERO).unwrap()));
    let (progress, _receiver) = progress_channel();
    let response = driver
        .run(
            request(),
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
        )
        .await
        .unwrap();
    assert_text(&response, "retried");
    assert_eq!(model.starts(), 2);

    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::StartError(retryable_error(None)),
            Behavior::Events(text_success("unexpected")),
        ],
    );
    let driver = model.driver(&kernel(
        RetryPolicy::new(2, Duration::from_secs(10)).unwrap(),
    ));
    let error = driver
        .run(
            request(),
            context(CancellationToken::new(), Duration::from_secs(5)),
            &progress,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::ProviderUnavailable);
    assert_eq!(model.starts(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn retryable_event_free_stream_is_dropped_before_next_attempt_starts() {
    let dropped = StreamProbe::shared();
    let confirmed = Arc::new(AtomicBool::new(false));
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::ProbedStream {
                events: vec![Err(retryable_error(None))],
                terminal: StreamTerminal::Eof,
                probe: Arc::clone(&dropped),
            },
            Behavior::RequireDropThenEvents {
                dropped: Arc::clone(&dropped),
                confirmed: Arc::clone(&confirmed),
                events: text_success("retried"),
            },
        ],
    );
    let driver = model.driver(&kernel(RetryPolicy::new(2, Duration::ZERO).unwrap()));
    let (progress, _receiver) = progress_channel();
    let response = driver
        .run(
            request(),
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
        )
        .await
        .unwrap();

    assert_text(&response, "retried");
    assert!(dropped.dropped());
    assert!(confirmed.load(Ordering::SeqCst));
    assert_eq!(model.starts(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn overall_deadline_can_expire_during_retry_sleep() {
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::StartError(retryable_error(None)),
            Behavior::Events(text_success("unexpected")),
        ],
    );
    let driver = model.driver(&kernel(
        RetryPolicy::new(2, Duration::from_secs(5)).unwrap(),
    ));
    let (progress, _receiver) = progress_channel();
    let run = tokio::spawn(async move {
        driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(9)),
                &progress,
            )
            .await
    });
    model.wait_for_completions(1).await;
    // The 5-second retry fits initially; advancing 10 seconds crosses the 9-second deadline.
    tokio::time::advance(Duration::from_secs(10)).await;
    let error = run.await.unwrap().unwrap_err();

    assert_eq!(error.kind(), ModelErrorKind::Timeout);
    assert_eq!(error.delivery(), DeliveryState::NotStarted);
    assert_eq!(model.starts(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancellation_interrupts_retry_sleep_without_another_attempt() {
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::StartError(retryable_error(None)),
            Behavior::Events(text_success("unexpected")),
        ],
    );
    let driver = model.driver(&kernel(
        RetryPolicy::new(2, Duration::from_secs(10)).unwrap(),
    ));
    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let (progress, _receiver) = progress_channel();
    let run = tokio::spawn(async move {
        driver
            .run(
                request(),
                context(task_cancel, Duration::from_secs(30)),
                &progress,
            )
            .await
    });
    model.wait_for_completions(1).await;
    cancellation.cancel();
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::NotStarted);
    assert_eq!(model.starts(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn one_driver_allows_true_concurrent_model_calls() {
    let barrier = Arc::new(Barrier::new(2));
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::Barrier(Arc::clone(&barrier), text_success("first")),
            Behavior::Barrier(Arc::clone(&barrier), text_success("second")),
        ],
    );
    let driver = Arc::new(model.driver(&kernel(RetryPolicy::new(1, Duration::ZERO).unwrap())));
    let (first_progress, _first_rx) = progress_channel();
    let (second_progress, _second_rx) = progress_channel();
    let first_driver = Arc::clone(&driver);
    let second_driver = Arc::clone(&driver);
    let first = tokio::spawn(async move {
        first_driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &first_progress,
            )
            .await
    });
    let second = tokio::spawn(async move {
        second_driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &second_progress,
            )
            .await
    });
    model.wait_for_starts(2).await;
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert_eq!(model.starts(), 2);
    assert!(matches!(first.parts(), [AssistantPart::Text(_)]));
    assert!(matches!(second.parts(), [AssistantPart::Text(_)]));
}
