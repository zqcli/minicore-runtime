use minicore_runtime::config::{Timestamp, TimestampError};

#[test]
fn p4_store_types_are_crate_private_and_not_root_reexports() {
    let lib = include_str!("../src/lib.rs");
    let public_exports = lib
        .split_once("pub use session::{")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(exports, _)| exports)
        .unwrap_or("");
    for symbol in [
        "SessionStore",
        "SessionRegistration",
        "StoreError",
        "StoredCompactionConfig",
        "StoredExecutionConfig",
        "StoredModelConfig",
        "StoredSessionConfig",
        "Timestamp",
        "TimestampError",
    ] {
        assert!(
            !public_exports.contains(symbol),
            "P4 symbol leaked through public root/lib.rs: {symbol}"
        );
    }

    let session = include_str!("../src/session/mod.rs");
    assert!(!session.contains("pub(crate) mod store;"));
    assert!(!session.contains("pub(crate) mod time;"));
    let storage = include_str!("../src/storage/mod.rs");
    assert!(storage.contains("pub(crate) mod store;"));
    assert!(!storage.contains("pub(crate) mod time;"));
    assert!(!storage.contains("pub use store::{"));
    assert!(!storage.contains("pub use time::{"));

    let config = include_str!("../src/config.rs");
    assert!(config.contains("pub use crate::time::{Timestamp, TimestampError};"));
    let _: Result<Timestamp, TimestampError> = "2026-08-19T12:34:56.789Z".parse();
    let config_exports = lib
        .split_once("pub use config::{")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(exports, _)| exports)
        .unwrap_or("");
    assert!(!config_exports.contains("Timestamp"));

    let store = include_str!("../src/storage/store.rs");
    for declaration in [
        "pub(crate) enum StoreError",
        "pub(crate) struct SessionRegistration",
        "pub(crate) struct SessionStore",
        "pub(crate) struct StoredCompactionConfig",
        "pub(crate) struct StoredExecutionConfig",
        "pub(crate) struct StoredModelConfig",
        "pub(crate) struct StoredSessionConfig",
    ] {
        assert!(
            store.contains(declaration),
            "missing private declaration: {declaration}"
        );
    }
    let time = include_str!("../src/time.rs");
    assert!(time.contains("pub enum TimestampError"));
    assert!(time.contains("pub struct Timestamp"));
}

#[test]
fn p4_store_stays_on_the_new_boundary_and_keeps_bootstrap_guards() {
    for source in [
        include_str!("../src/time.rs"),
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
            "WorkspaceAccess",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden P4 coupling: {forbidden}"
            );
        }
    }
    let store = include_str!("../src/storage/store.rs");
    for required in [
        "fn start(",
        "root: PathBuf",
        "let (worker, readiness) = WorkerOwner::start(root)?;",
        "handle: Some(handle)",
        "runtime.lock",
        "remove_orphan_temps",
        "ready.send(Ok(sessions.clone()))",
        "let (sessions, lock_file)",
        "drop(lock_file)",
        "conversation.jsonl",
        "sync_channel(WORKER_QUEUE_CAPACITY)",
        "sender.try_send(job)",
        "take((MAX_SESSION_JSON_BYTES + 1) as u64)",
        "StoreError::CleanupFailed",
        "struct SessionRegistration",
        "remove_temp_path(&entry.path())?",
    ] {
        assert!(
            store.contains(required),
            "missing P4 contract source: {required}"
        );
    }
    assert!(!store.contains("metadata.len() > MAX_SESSION_JSON_BYTES"));
    assert!(!store.contains("RootLock"));
    assert!(!store.contains("workspace_access"));
    assert!(!store.contains("max_output_tokens"));
    assert!(!store.contains("reasoning"));
    assert!(store.contains("std::thread::Builder::new"));
    assert!(store.contains("handle.join()"));
}
