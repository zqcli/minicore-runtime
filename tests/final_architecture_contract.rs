use std::path::Path;

#[test]
fn removed_implementation_graph_is_physically_absent() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/agent/legacy.rs",
        "src/agent/legacy_context.rs",
        "src/agent/legacy_runner.rs",
        "src/model/legacy_gateway.rs",
        "src/model/legacy_provider.rs",
        "src/model/legacy_registry.rs",
        "src/prompt/legacy.rs",
        "src/prompt/legacy_builder.rs",
        "src/prompt/legacy_compaction.rs",
        "src/session/legacy_actor.rs",
        "src/session/legacy_command.rs",
        "src/session/legacy_event.rs",
        "src/session/legacy_event_stream.rs",
        "src/session/legacy_snapshot.rs",
        "src/session/legacy_state.rs",
        "src/session/transcript.rs",
        "src/tools/legacy_context.rs",
        "src/tools/legacy_policy.rs",
        "src/tools/legacy_types.rs",
        "src/tools/registry.rs",
        "src/workspace",
        "src/storage/conversation.rs",
        "src/storage/conversation",
        "src/storage/store.rs",
        "src/storage/compaction_visibility.rs",
    ] {
        assert!(
            !root.join(relative).exists(),
            "removed path remains: {relative}"
        );
    }
}

#[test]
fn final_root_storage_and_dependency_surfaces_are_exact() {
    let root = include_str!("../src/lib.rs");
    let compact = root.split_whitespace().collect::<String>();
    assert!(!compact.contains("modworkspace;"));
    assert!(!compact.contains("pubuseerror::{PublicErrorCode,PublicErrorSummary};"));
    for removed in [
        "ConfigError,",
        "RetryPolicyError,",
        "IdError,",
        "ToolCallIdError,",
    ] {
        assert!(
            !compact.contains(removed),
            "root error export remains: {removed}"
        );
    }

    let storage = include_str!("../src/storage/mod.rs");
    assert!(!storage.contains("mod session_log;"));
    assert!(storage.contains("pub use crate::conversation::session_log::{"));
    assert!(!storage.contains("conversation;"));
    assert!(!storage.contains("store;"));

    let manifest = include_str!("../Cargo.toml");
    for removed in ["cap-std", "cap-primitives", "fs4"] {
        assert!(
            !manifest.contains(removed),
            "removed dependency remains: {removed}"
        );
    }
}
