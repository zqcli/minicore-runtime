#[test]
fn canonical_agent_module_is_private_unconditional_and_legacy_is_test_only() {
    let root = include_str!("../src/lib.rs");
    assert!(root.starts_with("mod agent;"));
    assert!(!root.contains("pub mod agent;"));
    assert!(!root.contains("pub use agent::"));

    let module = include_str!("../src/agent/mod.rs");
    assert!(module.contains("mod runner_protocol;"));
    assert!(module.contains("mod tool_driver;"));
    assert!(module.contains("#[cfg(test)]\nmod legacy;"));
    assert!(module.contains("pub(crate) use runner_protocol::{"));
    assert!(module.contains("pub(crate) use tool_driver::{"));

    let legacy = include_str!("../src/agent/legacy.rs");
    assert!(legacy.contains("#[path = \"legacy_context.rs\"]\nmod context;"));
    assert!(legacy.contains("#[path = \"legacy_runner.rs\"]\nmod runner;"));
}

#[test]
fn runner_protocol_is_exact_redacted_and_continuation_free() {
    let protocol = include_str!("../src/agent/runner_protocol.rs");
    for required in [
        "pub(crate) struct TurnSuspension",
        "pub(crate) turn_id: TurnId",
        "pub(crate) tool_call_id: ToolCallId",
        "pub(crate) tool_name: ToolName",
        "pub(crate) kind: InteractionKind",
        "pub(crate) resume: oneshot::Sender<Result<InteractionAnswer, SuspensionError>>",
        "suspension: TurnSuspension",
        "Cancelled",
        "DeadlineExceeded",
        "StaleTurn",
        "InvalidState",
        "RuntimeClosed",
    ] {
        assert!(protocol.contains(required), "protocol misses {required}");
    }
    let suspension = protocol
        .split_once("pub(crate) struct TurnSuspension {")
        .and_then(|(_, tail)| tail.split_once("}\n\nimpl fmt::Debug"))
        .map(|(body, _)| body)
        .unwrap();
    assert_eq!(
        suspension
            .lines()
            .filter(|line| line.trim_start().starts_with("pub(crate)"))
            .count(),
        5
    );
    let errors = protocol
        .split_once("pub(crate) enum SuspensionError {")
        .and_then(|(_, tail)| tail.split_once("}\n\nimpl SuspensionError"))
        .map(|(body, _)| body)
        .unwrap();
    assert_eq!(
        errors
            .lines()
            .filter(|line| line.trim().ends_with(','))
            .count(),
        5
    );
    let debug = protocol
        .split_once("impl fmt::Debug for TurnSuspension")
        .and_then(|(_, tail)| tail.split_once("#[derive"))
        .map(|(body, _)| body)
        .unwrap();
    assert!(!debug.contains(".field(\"resume\""));
    assert!(!protocol.contains("take_resume_for_actor"));
    assert!(!protocol.contains("take_commit_reply_for_actor"));
    let actor = include_str!("../src/session/actor/runner.rs");
    assert!(actor.contains("let TurnSuspension {"));
    for forbidden in [
        "serde",
        "Serialize",
        "Deserialize",
        "callback",
        "closure",
        "Any",
        "SessionHandle",
        "SessionRuntime",
        "SessionLog",
        "ConversationLog",
        "ToolFuture",
        "ModelFuture",
        "Workspace",
        "Store",
    ] {
        assert!(
            !protocol.contains(forbidden),
            "protocol contains {forbidden}"
        );
    }
    assert!(protocol.lines().count() < 500);
}

#[test]
fn tool_driver_owns_only_policy_tool_suspension_and_progress_execution() {
    let driver = include_str!("../src/agent/tool_driver.rs");
    let support = include_str!("../src/agent/tool_driver/support.rs");
    let implementation = format!("{driver}\n{support}");
    for required in [
        "pub(crate) struct ToolDriver",
        "pub(crate) struct ToolDriverConfig",
        "pub(crate) enum ToolDriverProgress",
        "Started {",
        "Update {",
        "pub(crate) struct ToolDriverResult",
        "pub(crate) async fn run(",
        ") -> Result<ToolDriverResult, SuspensionError>",
        "effective_deadline(turn_deadline, self.config.policy_timeout)",
        "effective_deadline(turn_deadline, self.config.tool_timeout)",
        "DeadlineSource::Turn => PolicyResolution::DeadlineExceeded",
        "DeadlineSource::Port => PolicyResolution::Denied",
        "DeadlineSource::Turn => ExecutionResolution::DeadlineExceeded",
        "DeadlineSource::Port => ExecutionResolution::Failed",
        "catch_unwind(AssertUnwindSafe(|| policy.decide(request)))",
        "catch_unwind(AssertUnwindSafe(|| tool.execute(invocation, context)))",
        "child.cancel();",
        "suspensions.send(suspension)",
        ".try_send(ToolDriverProgress",
        "ToolResultOutcome::InputProvided",
        "answer.encode_result(&request)",
        "validate_json_size(invocation.arguments()",
        "self.tools.frozen_spec(invocation.tool_name())",
    ] {
        assert!(
            implementation.contains(required),
            "tool driver misses {required}"
        );
    }
    assert!(!driver.contains("tool.spec()"));
    assert!(!driver.contains("fn suspension_failure"));
    assert!(!driver.contains("fn effective_deadline("));
    for forbidden in [
        "SessionHandle",
        "SessionRuntime",
        "SessionLog",
        "ConversationLog",
        "SessionManager",
        "SessionBindings",
        "SessionSpec",
        "crate::config",
        "Workspace",
        "Store",
        "serde",
        "callback",
        "closure",
        "Any",
        "Hook",
        "tokio::spawn",
        ".append(",
        "retry",
    ] {
        assert!(
            !implementation.contains(forbidden),
            "tool driver contains {forbidden}"
        );
    }
    assert!(driver.lines().count() < 500);
    assert!(support.lines().count() < 500);
}

#[test]
fn narrow_tool_capabilities_are_crate_private_and_public_surface_is_unchanged() {
    let set = include_str!("../src/tools/set.rs");
    assert!(set.contains("pub(crate) fn frozen_spec("));
    assert!(!set.contains("pub fn frozen_spec("));

    let progress = include_str!("../src/tools/mod.rs");
    assert!(progress.contains("pub(crate) use progress::ToolProgressEmitter;"));
    assert!(!progress.contains("pub use progress::ToolProgressEmitter;"));

    let input = include_str!("../src/tools/input.rs");
    assert!(input.contains("struct CanonicalTextResult<'a>"));
    assert!(input.contains("answer: &'a str"));
    assert!(input.contains("struct CanonicalChoiceResult<'a>"));
    assert!(input.contains("choice_index: usize,\n    choice: &'a str"));
    assert!(input.contains("pub(crate) fn encode_result("));
    assert!(input.contains("self.validate(request)?;"));
    assert!(!input.contains("pub fn encode_result("));
    assert!(input.contains("#[serde(tag = \"kind\", content = \"data\""));

    let root = include_str!("../src/lib.rs");
    for forbidden in [
        "ToolDriver",
        "ToolDriverResult",
        "ToolDriverProgress",
        "TurnSuspension",
        "SuspensionError",
    ] {
        assert!(!root.contains(forbidden));
    }
}
