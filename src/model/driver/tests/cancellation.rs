use super::*;

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_timeout_before_invocation_are_not_started() {
    let model = ScriptModel::new(descriptor(), vec![Behavior::Events(text_success("unused"))]);
    let driver = model.driver(&kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()));
    let (progress, _receiver) = progress_channel();

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = driver
        .run(
            request(),
            context(cancellation, Duration::from_secs(30)),
            &progress,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::NotStarted);
    assert_eq!(model.starts(), 0);

    let error = driver
        .run(
            request(),
            context(CancellationToken::new(), Duration::ZERO),
            &progress,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Timeout);
    assert_eq!(error.delivery(), DeliveryState::NotStarted);
    assert_eq!(model.starts(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_during_start_drops_future_and_reports_unknown() {
    let dropped = Arc::new(AtomicBool::new(false));
    let model = ScriptModel::new(
        descriptor(),
        vec![Behavior::PendingStart(Arc::clone(&dropped))],
    );
    let driver = model.driver(&kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()));
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
    model.wait_for_starts(1).await;
    cancellation.cancel();
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::Unknown);
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_during_start_drops_future_and_reports_unknown() {
    let dropped = Arc::new(AtomicBool::new(false));
    let model = ScriptModel::new(
        descriptor(),
        vec![Behavior::PendingStart(Arc::clone(&dropped))],
    );
    let config = KernelConfig {
        model_call_timeout: Duration::from_secs(5),
        ..kernel(RetryPolicy::new(1, Duration::ZERO).unwrap())
    };
    let driver = model.driver(&config);
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
    model.wait_for_starts(1).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Timeout);
    assert_eq!(error.delivery(), DeliveryState::Unknown);
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_during_stream_drops_stream_and_tracks_delivery() {
    let pending = StreamProbe::shared();
    let started = StreamProbe::shared();
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::ProbedStream {
                events: Vec::new(),
                terminal: StreamTerminal::Pending,
                probe: Arc::clone(&pending),
            },
            Behavior::ProbedStream {
                events: vec![Ok(ModelEvent::text_delta("seen").unwrap())],
                terminal: StreamTerminal::Pending,
                probe: Arc::clone(&started),
            },
        ],
    );
    let driver = Arc::new(model.driver(&kernel(RetryPolicy::new(1, Duration::ZERO).unwrap())));

    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let first_driver = Arc::clone(&driver);
    let (progress, _receiver) = progress_channel();
    let run = tokio::spawn(async move {
        first_driver
            .run(
                request(),
                context(task_cancel, Duration::from_secs(30)),
                &progress,
            )
            .await
    });
    pending.wait_for_terminal().await;
    cancellation.cancel();
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::Unknown);
    assert!(pending.dropped());

    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let (progress, mut progress_rx) = progress_channel();
    let run = tokio::spawn(async move {
        driver
            .run(
                request(),
                context(task_cancel, Duration::from_secs(30)),
                &progress,
            )
            .await
    });
    started.wait_for_terminal().await;
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ModelDriverProgress::TextDelta(_))
    ));
    cancellation.cancel();
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::Started);
    assert!(started.dropped());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_during_stream_drops_stream_and_reports_unknown() {
    let dropped = StreamProbe::shared();
    let model = ScriptModel::new(
        descriptor(),
        vec![Behavior::ProbedStream {
            events: Vec::new(),
            terminal: StreamTerminal::Pending,
            probe: Arc::clone(&dropped),
        }],
    );
    let config = KernelConfig {
        model_call_timeout: Duration::from_secs(5),
        ..kernel(RetryPolicy::new(1, Duration::ZERO).unwrap())
    };
    let driver = model.driver(&config);
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
    dropped.wait_for_terminal().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Timeout);
    assert_eq!(error.delivery(), DeliveryState::Unknown);
    assert!(dropped.dropped());
}

#[tokio::test(flavor = "current_thread")]
async fn start_and_stream_panics_become_unknown_nonretryable_errors() {
    for behavior in [Behavior::StartPanic, Behavior::FuturePanic] {
        let model = ScriptModel::new(descriptor(), vec![behavior]);
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
        assert_eq!(error.kind(), ModelErrorKind::Panicked);
        assert_eq!(error.delivery(), DeliveryState::Unknown);
        assert!(!error.retryable());
        assert_eq!(model.starts(), 1);
    }

    let dropped = StreamProbe::shared();
    let model = ScriptModel::new(
        descriptor(),
        vec![Behavior::ProbedStream {
            events: Vec::new(),
            terminal: StreamTerminal::Panic,
            probe: Arc::clone(&dropped),
        }],
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
    assert_eq!(error.kind(), ModelErrorKind::Panicked);
    assert_eq!(error.delivery(), DeliveryState::Unknown);
    assert!(!error.retryable());
    assert_eq!(model.starts(), 1);
    assert!(dropped.dropped());
}
