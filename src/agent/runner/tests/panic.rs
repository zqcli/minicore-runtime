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
    let bindings = session_bindings(model, None, Vec::new(), None);
    let environment =
        SessionEnvironment::build(&KernelConfig::default_checked().unwrap(), &spec, &bindings)
            .unwrap();
    let request = TurnRunnerRequest::new(
        TurnRunnerIdentity {
            session_id: session_id(),
            instance_id: instance_id(),
            turn_id: panic_turn_id,
        },
        environment,
        4,
        conversation,
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
async fn panic_returns_panicked_without_a_finish_event() {
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
