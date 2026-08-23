use minicore_runtime::conversation::TurnTerminal;
use minicore_runtime::error::{
    DiagnosticCategory, DiagnosticCode, DiagnosticSummary, TurnWaitError,
};
use minicore_runtime::model::Usage;
use minicore_runtime::session::{TurnHandle, TurnOutcome};
use minicore_runtime::{BoundedText, TurnId};

fn turn_id() -> TurnId {
    "trn_00000000000000000000000000000001".parse().unwrap()
}

fn diagnostic() -> DiagnosticSummary {
    DiagnosticSummary::new(
        DiagnosticCode::RuntimeTerminated,
        DiagnosticCategory::Internal,
        BoundedText::new("turn-wait-diagnostic-secret").unwrap(),
        false,
    )
}

#[test]
fn public_turn_surface_is_exact_clone_send_sync_and_process_local() {
    fn assert_clone_send_sync<T: Clone + Send + Sync + 'static>() {}
    assert_clone_send_sync::<TurnHandle>();

    let outcome = TurnOutcome {
        turn_id: turn_id(),
        terminal: TurnTerminal::Completed,
        usage: Usage::new(1, 2, 3),
    };
    assert_eq!(outcome.turn_id, turn_id());
    assert_eq!(outcome.terminal, TurnTerminal::Completed);

    let source = include_str!("../src/session/turn_handle.rs");
    let fields = source
        .split_once("pub struct TurnOutcome")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    assert_eq!(
        fields
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub "))
            .map(|line| line.split(':').next().unwrap())
            .collect::<Vec<_>>(),
        vec!["turn_id", "terminal", "usage"]
    );
    for required in [
        "pub fn session_id(&self)",
        "pub fn instance_id(&self)",
        "pub fn turn_id(&self)",
        "pub fn cancel(&self) -> bool",
        "pub fn is_finished(&self) -> bool",
        "pub async fn wait(&self) -> Result<TurnOutcome, TurnWaitError>",
        "pub(crate) struct TurnCompletion",
        "pub(crate) fn finish",
        "pub(crate) fn durability_unknown",
        "pub(crate) fn durability_unavailable",
        "pub(crate) fn runtime_terminated",
    ] {
        assert!(
            source.contains(required),
            "missing TurnHandle contract: {required}"
        );
    }
    for forbidden in [
        "impl Drop for TurnHandle",
        "SessionHandle",
        "SessionActor",
        "SessionLog",
        "ConversationLog",
        "mpsc::Sender",
        "oneshot",
        "Serialize",
        "Deserialize",
        "serde",
    ] {
        assert!(!source.contains(forbidden), "TurnHandle leaked {forbidden}");
    }
    assert_eq!(source.matches("Mutex<TurnCompletionState>").count(), 1);
    assert!(source.contains("CancellationToken"));
    assert!(source.contains("Notify"));
}

#[test]
fn wait_errors_are_exact_payload_safe_and_static_display() {
    let errors = [
        TurnWaitError::DurabilityUnknown(diagnostic()),
        TurnWaitError::DurabilityUnavailable(diagnostic()),
        TurnWaitError::RuntimeTerminated(diagnostic()),
    ];
    for error in errors {
        let debug = format!("{error:?}");
        let display = error.to_string();
        assert!(!debug.contains("turn-wait-diagnostic-secret"));
        assert!(!display.contains("turn-wait-diagnostic-secret"));
        assert!(debug.contains("message_bytes"));
    }
    let source = include_str!("../src/error.rs");
    assert!(source.contains(
        "#[non_exhaustive]\n#[derive(Clone, Debug, Eq, Error, PartialEq)]\npub enum TurnWaitError"
    ));
    for variant in [
        "DurabilityUnknown(DiagnosticSummary)",
        "DurabilityUnavailable(DiagnosticSummary)",
        "RuntimeTerminated(DiagnosticSummary)",
    ] {
        assert!(source.contains(variant));
    }
}

#[test]
fn completion_and_cancellation_race_tests_live_with_the_private_publisher() {
    let source = include_str!("../src/session/turn_handle.rs");
    for required in [
        "clones_wait_for_one_first_wins_completion",
        "cancellation_and_completion_share_one_linearization_point",
        "repeated_cancel_and_drop_do_not_create_new_cancellation",
        "internal_error_publishers_are_first_wins_and_redacted",
        "Barrier::new(3)",
        "notify_waiters()",
        "notified.as_mut().enable()",
    ] {
        assert!(source.contains(required));
    }
    let module = include_str!("../src/session/mod.rs");
    assert!(module.contains("pub use turn_handle::{TurnHandle, TurnOutcome};"));
    let root = include_str!("../src/lib.rs");
    assert!(root.contains("TurnHandle, TurnOutcome"));
}
