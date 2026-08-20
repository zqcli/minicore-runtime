use minicore_runtime::{
    ModelSelection, ProcessPolicy, ProgramPolicy, ProviderRegistry, RetryPolicy, RunCommandTool,
    Runtime, RuntimeConfig, SessionConfig, SessionError, SessionId, SessionSummary, ToolName,
    ToolRegistry, TranscriptEntry, TranscriptPage,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

async fn create_signature(
    runtime: &Runtime,
    config: SessionConfig,
) -> Result<SessionId, SessionError> {
    runtime.create_session(config).await
}

async fn list_signature(runtime: &Runtime) -> Result<Vec<SessionSummary>, SessionError> {
    runtime.list_sessions().await
}

#[test]
fn p7_public_runtime_surface_is_typed_and_redacted() {
    let source = include_str!("../src/runtime/runtime_impl.rs");
    let manager = include_str!("../src/runtime/session_manager.rs");
    let config = include_str!("../src/config.rs");
    let lib = include_str!("../src/lib.rs");
    let tools = include_str!("../src/tools/mod.rs");
    for text in [source, manager, config] {
        for forbidden in [
            "crate::wire",
            "crate::durable_state",
            "crate::conversation_storage",
            "crate::session_execution",
            "crate::session_ingress",
            "crate::session_residency",
            "tokio::spawn",
            "Handle::current",
            "allow(",
        ] {
            assert!(
                !text.contains(forbidden),
                "forbidden P7 coupling: {forbidden}"
            );
        }
    }
    assert!(source.contains("self.inner.runtime.spawn(actor.run())"));
    assert!(source.contains("JoinOnce<Result<(), RuntimeError>>"));
    assert!(source.contains("RetainedRuntimeOwners"));
    assert!(source.contains("process teardown"));
    assert!(source.contains("begin_shutdown"));
    assert!(source.contains("join_all"));
    assert!(!source.contains("shutdown_started"));
    assert!(!source.contains("shutdown_tx"));
    assert!(!source.contains("shutdown_rx"));
    assert!(!source.contains("managed.close().await"));
    assert!(source.lines().count() <= 1_200);
    assert!(manager.lines().count() <= 700);
    assert!(config.lines().count() <= 700);
    assert!(manager.contains("struct ManagerState"));
    assert!(manager.contains("state: Mutex<ManagerState>"));
    assert!(manager.contains("closing: bool"));
    assert!(manager.contains("BTreeMap<SessionId"));
    assert!(manager.contains("BTreeSet<SessionId"));
    assert!(!manager.contains("loaded: Mutex"));
    assert!(!manager.contains("loading: Mutex"));
    assert!(!manager.contains("close_started"));
    assert!(!manager.contains("watch::"));
    assert!(!manager.contains("SessionExecutor"));
    assert!(!manager.contains("SessionIngress"));
    assert!(!manager.contains("SessionResidency"));
    assert!(lib.contains("ProcessPolicy"));
    assert!(lib.contains("ProcessPolicyError"));
    assert!(lib.contains("ProgramPolicy"));
    assert!(lib.contains("RunCommandTool"));
    assert!(tools.contains("mod builtins;"));
    assert!(!tools.contains("P7_PROCESS_SURFACE"));

    let providers = ProviderRegistry::default();
    let tools = ToolRegistry::default();
    let retry = RetryPolicy::new(1, std::time::Duration::ZERO).unwrap();
    let config = RuntimeConfig::new(
        PathBuf::from("/tmp/minicore-p7-runtime"),
        providers,
        tools,
        "coding",
        retry,
    )
    .unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains("/tmp/minicore-p7-runtime"));
    assert!(!debug.contains("coding\""));

    let session = SessionConfig::new(
        PathBuf::from("/tmp/minicore-p7-workspace"),
        ModelSelection::new("provider".parse().unwrap(), "model".parse().unwrap()),
        "system",
        BTreeSet::<ToolName>::new(),
        1_000,
        999,
        4,
    )
    .unwrap();
    let _ = session;
    let _ = Runtime::open;
    let _ = ProcessPolicy::coding_agent_local;
    let _ = ProgramPolicy::any;
    let _ = RunCommandTool::new;
    let _ = create_signature;
    let _ = list_signature;
    let _ = TranscriptEntry::User {
        seq: 1,
        turn_id: minicore_runtime::TurnId::new().unwrap(),
        text: "safe".to_owned(),
    };
    let _ = TranscriptPage::new(Vec::new(), None);
}
