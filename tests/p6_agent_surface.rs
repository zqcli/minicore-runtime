#[test]
fn p6_agent_runner_surface_is_private_and_legacy_isolated() {
    let lib = include_str!("../src/lib.rs");
    assert_eq!(lib.matches("#[path = \"agent_v2/mod.rs\"]").count(), 1);
    assert!(lib.contains("#[path = \"agent_v2/mod.rs\"]\npub(crate) mod agent_v2;"));
    assert!(!lib.contains("pub mod agent_v2"));
    assert!(!lib.contains("pub use agent_v2"));
    assert!(!lib.contains("pub use agent_v2::"));

    let module = include_str!("../src/agent_v2/mod.rs");
    let context = include_str!("../src/agent_v2/context.rs");
    assert!(context.contains("type TimestampSource = fn()"));
    assert!(!context.contains("dyn Fn"));
    assert!(context.contains("!(1..=64).contains"));
    assert!(module.contains("pub(crate) use context::{"));
    assert!(module.contains("pub(crate) use runner::{"));
    let runner = include_str!("../src/agent_v2/runner.rs");
    for required in [
        "TimestampSource",
        "RetryPolicy",
        "TurnContext",
        "RunnerEvent",
        "RunnerEventSink",
        "RunnerEventSendError",
        "MAX_RUNNER_EVENT_CAPACITY",
        "TurnContextDependencies",
        "run_turn",
        "TurnTaskResult",
        "TurnFailure",
        "plan_after_context_overflow",
        "ToolStarted",
        "ToolFinished",
    ] {
        assert!(
            module.contains(required) || context.contains(required) || runner.contains(required),
            "missing P6 agent contract: {required}"
        );
    }
    for source in [module, context, runner] {
        for forbidden in [
            "crate::agent_session_lifecycle",
            "crate::conversation_storage",
            "crate::durable_state",
            "crate::live_conversation",
            "crate::session_execution",
            "crate::session_ingress",
            "crate::session_residency",
            "crate::runtime",
            "crate::runtime_task",
            "crate::wire",
            "crate::prompt::",
            "crate::compaction::",
            "crate::model_gateway",
            "crate::runtime_interface",
            "crate::turn_execution_context",
            "crate::workspace::",
            "crate::tools::ToolSet",
            "Agent",
            "Fork",
            "SessionActor",
            "Mailbox",
            "tokio::spawn",
            "spawn_blocking",
            "allow(",
            "allow(dead_code",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden P6 agent coupling: {forbidden}"
            );
        }
    }
}
