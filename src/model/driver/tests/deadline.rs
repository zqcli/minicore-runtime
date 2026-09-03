use std::sync::atomic::AtomicU64;

use super::*;
use crate::time::DeadlineSource;

fn deadline_kernel(model_timeout: Duration, retry_policy: RetryPolicy) -> KernelConfig {
    KernelConfig {
        model_call_timeout: model_timeout,
        retry_policy,
        ..KernelConfig::default_checked().unwrap()
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn pending_stream_reports_exact_turn_or_port_deadline_source() {
    for (turn_after, model_timeout, expected) in [
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
        let model = ScriptModel::new(
            descriptor(),
            vec![Behavior::ProbedStream {
                events: Vec::new(),
                terminal: StreamTerminal::Pending,
                probe: Arc::clone(&probe),
            }],
        );
        let driver = model.driver(&deadline_kernel(
            model_timeout,
            RetryPolicy::new(1, Duration::ZERO).unwrap(),
        ));
        let (progress, _receiver) = progress_channel();
        let dropped = AtomicU64::new(0);
        let run = tokio::spawn(async move {
            driver
                .run_detailed(
                    request(),
                    context(CancellationToken::new(), turn_after),
                    &progress,
                    &dropped,
                )
                .await
        });
        probe.wait_for_terminal().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let failure = run.await.unwrap().unwrap_err();
        assert_eq!(failure.error().kind(), ModelErrorKind::Timeout);
        assert_eq!(failure.deadline_source(), Some(expected));
        assert!(probe.dropped());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn adapter_timeout_has_no_core_deadline_provenance() {
    let model = ScriptModel::new(
        descriptor(),
        vec![Behavior::StartError(test_error(ModelErrorKind::Timeout))],
    );
    let driver = model.driver(&deadline_kernel(
        Duration::from_secs(5),
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    ));
    let (progress, _receiver) = progress_channel();
    let failure = driver
        .run_detailed(
            request(),
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
            &AtomicU64::new(0),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.error().kind(), ModelErrorKind::Timeout);
    assert_eq!(failure.deadline_source(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn expired_turn_deadline_is_reported_before_model_invocation() {
    let model = ScriptModel::new(descriptor(), Vec::new());
    let driver = model.driver(&deadline_kernel(
        Duration::from_secs(30),
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    ));
    let mut call_context = context(CancellationToken::new(), Duration::from_secs(30));
    call_context.deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let (progress, _receiver) = progress_channel();
    let failure = driver
        .run_detailed(request(), call_context, &progress, &AtomicU64::new(0))
        .await
        .unwrap_err();
    assert_eq!(failure.error().kind(), ModelErrorKind::Timeout);
    assert_eq!(failure.deadline_source(), Some(DeadlineSource::Turn));
    assert_eq!(model.starts(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_panic_have_no_deadline_provenance() {
    let model = ScriptModel::new(descriptor(), Vec::new());
    let driver = model.driver(&deadline_kernel(
        Duration::from_secs(5),
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    ));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let (progress, _receiver) = progress_channel();
    let failure = driver
        .run_detailed(
            request(),
            context(cancellation, Duration::from_secs(30)),
            &progress,
            &AtomicU64::new(0),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.error().kind(), ModelErrorKind::Cancelled);
    assert_eq!(failure.deadline_source(), None);

    let model = ScriptModel::new(descriptor(), vec![Behavior::StartPanic]);
    let driver = model.driver(&deadline_kernel(
        Duration::from_secs(5),
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    ));
    let failure = driver
        .run_detailed(
            request(),
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
            &AtomicU64::new(0),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.error().kind(), ModelErrorKind::Panicked);
    assert_eq!(failure.deadline_source(), None);
}

async fn retry_timeout_source(turn_after: Duration, model_timeout: Duration) -> ModelDriverFailure {
    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::StartError(retryable_error(None)),
            Behavior::Events(text_success("unexpected")),
        ],
    );
    let driver = model.driver(&deadline_kernel(
        model_timeout,
        RetryPolicy::new(2, Duration::from_secs(5)).unwrap(),
    ));
    let (progress, _receiver) = progress_channel();
    let dropped = AtomicU64::new(0);
    let run = tokio::spawn(async move {
        driver
            .run_detailed(
                request(),
                context(CancellationToken::new(), turn_after),
                &progress,
                &dropped,
            )
            .await
    });
    model.wait_for_completions(1).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    let failure = run.await.unwrap().unwrap_err();
    assert_eq!(model.starts(), 1);
    failure
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retry_sleep_timeout_reports_turn_source() {
    let failure = retry_timeout_source(Duration::from_secs(9), Duration::from_secs(30)).await;
    assert_eq!(failure.error().kind(), ModelErrorKind::Timeout);
    assert_eq!(failure.deadline_source(), Some(DeadlineSource::Turn));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retry_sleep_timeout_reports_port_source() {
    let failure = retry_timeout_source(Duration::from_secs(30), Duration::from_secs(9)).await;
    assert_eq!(failure.error().kind(), ModelErrorKind::Timeout);
    assert_eq!(failure.deadline_source(), Some(DeadlineSource::Port));
}
