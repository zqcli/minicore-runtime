#[test]
fn conversation_surface_is_crate_private_and_not_root_exported() {
    let lib = include_str!("../src/lib.rs");
    let public_exports = lib
        .split_once("pub use session::{")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(exports, _)| exports)
        .unwrap_or("");
    for symbol in [
        "ConversationLog",
        "ConversationEntry",
        "NewConversationEntry",
        "ConversationError",
        "PromptConversationView",
        "ConversationSnapshot",
        "CompactionConversationView",
    ] {
        assert!(
            !public_exports.contains(symbol),
            "public conversation leak: {symbol}"
        );
    }
    let session = include_str!("../src/session/mod.rs");
    assert!(!session.contains("pub(crate) mod conversation;"));
    let storage = include_str!("../src/storage/mod.rs");
    assert!(storage.contains("pub(crate) mod conversation;"));
    assert!(storage.contains("mod compaction_visibility;"));
    assert!(!storage.contains("pub use conversation"));
    let conversation = include_str!("../src/storage/conversation.rs");
    let visibility = include_str!("../src/storage/compaction_visibility.rs");
    assert!(conversation.contains("pub(crate) use compaction::CompactionConversationView;"));
    assert!(visibility.contains("CompactionConversationView"));
    for removed in [
        "ConversationResolution",
        "PendingInteraction",
        "InteractionRequested",
        "InteractionResolved",
    ] {
        assert!(
            !conversation.contains(removed),
            "removed interaction surface remains: {removed}"
        );
    }
    for declaration in [
        "pub(crate) enum ConversationError",
        "pub(crate) enum ConversationHealth",
        "pub(crate) enum StoredTurnOutcome",
        "pub(crate) enum NewConversationEntry",
        "pub(crate) enum ConversationEntry",
        "pub(crate) struct ConversationLog",
        "pub(crate) struct ConversationSnapshot",
        "pub(crate) struct ConversationSummary",
        "pub(crate) struct PromptConversationView",
    ] {
        assert!(
            conversation.contains(declaration),
            "missing private declaration: {declaration}"
        );
    }
}

#[test]
fn conversation_owns_strict_jsonl_and_worker_boundaries_without_legacy_coupling() {
    for source in [
        include_str!("../src/storage/conversation.rs"),
        include_str!("../src/storage/conversation/codec.rs"),
        include_str!("../src/storage/conversation/compaction.rs"),
        include_str!("../src/storage/conversation/usage.rs"),
        include_str!("../src/storage/compaction_visibility.rs"),
        include_str!("../src/storage/store.rs"),
        include_str!("../src/storage/mod.rs"),
    ] {
        for forbidden in [
            "crate::wire",
            "crate::durable_state",
            "crate::conversation_storage",
            "crate::live_conversation",
            "crate::prompt",
            "crate::runtime",
            "crate::model_gateway",
            "Agent",
            "Fork",
            "generation",
            "allow(dead_code",
            "tokio::spawn",
            "spawn_blocking",
            "pub use conversation",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden conversation coupling: {forbidden}"
            );
        }
    }
    let conversation = include_str!("../src/storage/conversation.rs");
    let codec = include_str!("../src/storage/conversation/codec.rs");
    let compaction = include_str!("../src/storage/conversation/compaction.rs");
    let usage = include_str!("../src/storage/conversation/usage.rs");
    let combined = [conversation, codec, compaction, usage].join("\n");
    for required in [
        "serde(tag = \"type\", rename_all = \"snake_case\", deny_unknown_fields)",
        "deny_unknown_fields",
        "MAX_LINE_BYTES",
        "MAX_FILE_BYTES",
        "1_073_741_824",
        "MAX_COMPLETE_ENTRIES",
        "BufReader",
        "fill_buf",
        "assistant_from_response",
        "impl Serialize for ConversationEntry",
        "validate_shape().map_err(S::Error::custom)",
        "call_id",
        "result",
        "pending_restart_terminal",
        "ConversationCorrupt",
        "CorruptAt",
        "line",
        "offset",
        "CancelledByRestart",
        "cancelled by restart",
        "Notify",
        "SessionRegistration",
        "run_io",
        "CompactionConversationView",
        "compaction_view",
        "append_summary",
        "latest_terminal_seq",
        "IncompleteToolExchange",
        "Stale",
        "pub(crate) use compaction::CompactionConversationView",
        "NewConversationEntry::Summary",
    ] {
        assert!(
            combined.contains(required),
            "missing conversation contract: {required}"
        );
    }
    assert!(!combined.contains("read_to_end"));
    assert!(!combined.contains("with_capacity(MAX_FILE_BYTES"));
    let store = include_str!("../src/storage/store.rs");
    assert!(store.contains("pub(crate) fn run_io"));
    assert!(store.contains("pub(crate) fn conversation_path"));
    assert!(store.contains("pub(crate) async fn open_registration"));
    assert!(store.contains("ConversationCorrupt"));
    let prepare = conversation
        .split_once("fn reserve_append_slot(")
        .and_then(|(_, rest)| rest.split_once("fn encode_candidate("))
        .map(|(body, _)| body)
        .expect("reserve_append_slot must remain a distinct lock-order seam");
    let health = prepare
        .find("read_lock(&inner.state).health")
        .expect("prepare_append must check health");
    let lifecycle = prepare
        .find("let mut lifecycle = lock_mutex(&inner.lifecycle)")
        .expect("prepare_append must reserve lifecycle");
    let state_after_reservation = prepare
        .find("let state = read_lock(&inner.state)")
        .expect("prepare_append must read state after reservation");
    assert!(
        health < lifecycle,
        "health check must precede lifecycle lock"
    );
    assert!(
        lifecycle < state_after_reservation,
        "state read must follow reservation"
    );
    assert!(
        !prepare[lifecycle..state_after_reservation].contains("read_lock(&inner.state)"),
        "lifecycle reservation must not hold state lock"
    );
    let production_lines = conversation
        .find("#[cfg(test)]")
        .expect("focused tests must be after production code");
    assert!(conversation[..production_lines].lines().count() <= 1_500);
}
