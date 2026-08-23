use std::task::Poll;

use super::*;

fn panic_request(
    cancellation: CancellationToken,
    deadline: Instant,
    critical_capacity: usize,
) -> (TurnRunnerRequest, mpsc::Receiver<RunnerEvent>) {
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let panic_turn_id = next_scripted_turn_id();
    let mut entries = initial_conversation(&spec, 4).entries().to_vec();
    match &mut entries[0] {
        ConversationEntry::UserMessage(entry) => entry.turn_id = panic_turn_id,
        entry => panic!("unexpected entry: {entry:?}"),
    }
    let conversation = ConversationView::from_confirmed(ConversationSeq::new(1), entries.into());
    let (critical_tx, critical_rx) = mpsc::channel(critical_capacity);
    let (progress_tx, _progress_rx) = mpsc::channel(1);
    let request = TurnRunnerRequest::new(
        TurnRunnerIdentity {
            session_id: session_id(),
            instance_id: instance_id(),
            turn_id: panic_turn_id,
        },
        spec,
        4,
        session_bindings(model, None, Vec::new(), None),
        conversation,
        TurnRunnerKernel::from_kernel(&KernelConfig::default_checked().unwrap()).unwrap(),
        TurnRunnerControl {
            cancellation,
            deadline,
            critical_tx,
            progress_tx,
        },
    )
    .unwrap();
    script_turn_panic(panic_turn_id);
    (request, critical_rx)
}

#[tokio::test(flavor = "current_thread")]
async fn panic_delivers_internal_finish_with_default_usage_then_exits_panicked() {
    let (request, mut critical_rx) = panic_request(
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(30),
        4,
    );
    let mut run = Box::pin(run_turn(request));
    assert!(matches!(
        futures_util::poll!(run.as_mut()),
        Poll::Ready(TurnRunnerExit::Panicked)
    ));
    let outcome = match critical_rx.try_recv().unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(outcome.usage(), Usage::default());
    let diagnostic = outcome.diagnostic().unwrap();
    assert_eq!(diagnostic.code, crate::error::DiagnosticCode::Internal);
    assert_eq!(
        diagnostic.category,
        crate::error::DiagnosticCategory::Internal
    );
    assert!(!format!("{outcome:?}").contains("turn runner panicked"));
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn panic_finish_blocked_by_full_channel_is_cancellable_without_delayed_enqueue() {
    let cancellation = CancellationToken::new();
    let (request, mut critical_rx) = panic_request(
        cancellation.clone(),
        Instant::now() + Duration::from_secs(30),
        1,
    );
    request
        .critical_tx
        .try_send(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled {
                usage: Usage::default(),
            },
        })
        .unwrap();
    let mut run = Box::pin(run_turn(request));
    assert!(matches!(futures_util::poll!(run.as_mut()), Poll::Pending));
    cancellation.cancel();
    assert!(matches!(
        futures_util::poll!(run.as_mut()),
        Poll::Ready(TurnRunnerExit::Panicked)
    ));
    assert!(matches!(
        critical_rx.try_recv(),
        Ok(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled { .. }
        })
    ));
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn panic_finish_blocked_by_full_channel_respects_the_absolute_deadline() {
    let (request, mut critical_rx) = panic_request(
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
        1,
    );
    request
        .critical_tx
        .try_send(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled {
                usage: Usage::default(),
            },
        })
        .unwrap();
    let mut run = Box::pin(run_turn(request));
    assert!(matches!(futures_util::poll!(run.as_mut()), Poll::Pending));
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        futures_util::poll!(run.as_mut()),
        Poll::Ready(TurnRunnerExit::Panicked)
    ));
    assert!(critical_rx.try_recv().is_ok());
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn panic_with_closed_critical_channel_returns_panicked_without_retry() {
    let (request, critical_rx) = panic_request(
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(30),
        1,
    );
    drop(critical_rx);
    let mut run = Box::pin(run_turn(request));
    assert!(matches!(
        futures_util::poll!(run.as_mut()),
        Poll::Ready(TurnRunnerExit::Panicked)
    ));
}
