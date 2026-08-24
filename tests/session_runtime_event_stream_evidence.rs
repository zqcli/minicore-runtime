pub mod support;

use futures_util::StreamExt;
use minicore_runtime::conversation::TurnTerminal;
use minicore_runtime::session::SessionEvent;
use minicore_runtime::{TurnOptions, UserInput};
use tokio_util::sync::CancellationToken;

use support::fake_session_log::FakeSessionLog;
use support::transcript_runtime::{create_runtime, session};

#[tokio::test(flavor = "current_thread")]
async fn session_event_stream_next_receives_events() {
    let (runtime, handle, _inspection, mut events) =
        create_runtime(session(94), FakeSessionLog::new()).await;
    let turn = handle
        .submit(UserInput::text("next").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let first = events
        .next()
        .await
        .expect("TurnStarted should be delivered");
    assert!(matches!(first.event, SessionEvent::TurnStarted { .. }));
    assert_eq!(turn.wait().await.unwrap().terminal, TurnTerminal::Completed);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn session_event_stream_next_can_be_cancelled_by_external_token() {
    let (runtime, _handle, _inspection, mut events) =
        create_runtime(session(95), FakeSessionLog::new()).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let selected = tokio::select! {
        event = events.next() => panic!("unexpected event: {event:?}"),
        _ = cancellation.cancelled() => "cancelled",
    };
    assert_eq!(selected, "cancelled");
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn session_event_stream_drains_after_shutdown_and_then_ends() {
    let (runtime, handle, _inspection, mut events) =
        create_runtime(session(96), FakeSessionLog::new()).await;
    let turn = handle
        .submit(UserInput::text("shutdown").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    assert_eq!(turn.wait().await.unwrap().terminal, TurnTerminal::Completed);
    runtime.shutdown().await.unwrap();

    let mut delivered = 0;
    while events.next().await.is_some() {
        delivered += 1;
    }
    assert!(delivered > 0);
    assert!(events.next().await.is_none());
}
