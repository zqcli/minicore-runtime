#[test]
fn p6_observation_surface_is_precise_and_keeps_owners_private() {
    let lib = include_str!("../src/lib.rs");
    assert!(!lib.contains("pub use session::{"));
    assert!(!lib.contains("SessionEventStream"));
    let session = include_str!("../src/session/mod.rs");
    assert!(session.contains("pub(crate) mod event_stream;"));
    assert!(!session.contains("pub use event_stream::SessionEventStream;"));
    assert!(!session.contains("pub use event_stream::SessionObservation;"));
    assert!(!session.contains("pub use event_stream::{"));
    let time = include_str!("../src/time.rs");
    let conversation = include_str!("../src/storage/conversation.rs");
    let usage = include_str!("../src/storage/conversation/usage.rs");
    let stream = include_str!("../src/session/event_stream.rs");
    for required in [
        "now_utc",
        "pub(crate) async fn usage",
        "MAX_EVENT_CAPACITY",
        "SessionObservation",
        "SessionEventStream",
        "pending_resync_snapshot",
        "watch::Sender<SessionSnapshot>",
        "broadcast::Sender<SessionEvent>",
        "publish_snapshot",
        "publish(",
        "subscribe",
        "ResyncRequired",
        "Closed",
    ] {
        let found = time.contains(required)
            || conversation.contains(required)
            || usage.contains(required)
            || stream.contains(required);
        assert!(found, "missing P6 contract: {required}");
    }
    for source in [time, conversation, usage, stream, session] {
        for forbidden in [
            "crate::wire",
            "crate::durable_state",
            "crate::conversation_storage",
            "crate::live_conversation",
            "crate::skills",
            "crate::model_gateway",
            "crate::runtime",
            "crate::session_execution",
            "crate::session_ingress",
            "crate::session_residency",
            "crate::runtime_task",
            "crate::turn_execution_context",
            "crate::http_transport",
            "crate::agent_session_lifecycle",
            "tokio::spawn",
            "spawn_blocking",
            "allow(dead_code",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden P6 coupling: {forbidden}"
            );
        }
    }
}
