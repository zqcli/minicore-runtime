use std::path::Path;

#[test]
fn canonical_driver_is_private_bounded_and_adapter_neutral() {
    let module = include_str!("../src/model/mod.rs");
    assert!(module.contains("mod driver;"));
    assert!(module.contains("pub(crate) use driver::{"));
    assert!(!module.contains("pub mod driver;"));
    assert!(!module.contains("pub use driver"));

    let driver = include_str!("../src/model/driver.rs");
    let failure = include_str!("../src/model/driver/failure.rs");
    let implementation = format!("{driver}\n{failure}");
    for required in [
        "pub(crate) struct ModelDriver",
        "pub(crate) struct ModelDriverFailure",
        "pub(crate) enum ModelDriverProgress",
        "model: Arc<dyn Model>",
        "model_call_timeout: Duration",
        "retry_policy: RetryPolicySnapshot",
        "limits: SemanticLimitsSnapshot",
        "pub(crate) fn new(",
        "pub(crate) fn from_validated(",
        "pub(crate) async fn run(",
        "pub(crate) async fn run_detailed(",
        "effective_deadline(context.deadline, self.model_call_timeout)",
        "deadline_source: Option<DeadlineSource>",
        "try_send(progress_event)",
        "AssertUnwindSafe(start).catch_unwind()",
        "AssertUnwindSafe(stream.next()).catch_unwind()",
        "evaluate_retry(",
        "failure.error.delivery() != DeliveryState::NotStarted",
        "RetryHint::Retryable { retry_after }",
    ] {
        assert!(
            implementation.contains(required),
            "driver misses {required}"
        );
    }
    assert!(!driver.contains("validate_capabilities"));
    assert!(!driver.contains("run_port_call"));
    assert!(driver.contains("self.descriptor.validate().map_err(|_| invalid())?;"));
    assert!(driver.contains("!self.descriptor.supports_reasoning(request.reasoning())"));
    assert!(driver.contains("!request.tools().is_empty() && !self.descriptor.supports_tools"));
    assert!(!driver.contains("fn effective_deadline("));
    for forbidden in [
        "SessionHandle",
        "SessionRuntime",
        "ConversationLog",
        "SessionLog",
        "Workspace",
        "Registry",
        "Resolver",
        "Credential",
        "reqwest",
        "std::fs",
        "tokio::fs",
        "ToolContext",
        "ToolInvocation",
        "crate::config",
        "Callback",
        "Hook",
        "ServiceLocator",
        "tokio::spawn",
    ] {
        assert!(
            !implementation.contains(forbidden),
            "driver contains {forbidden}"
        );
    }
    let lines = driver.lines().count();
    assert!(lines < 500, "canonical driver has {lines} lines");
    assert!(failure.lines().count() < 500);
    let progress = driver
        .split_once("pub(crate) enum ModelDriverProgress {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    assert!(progress.contains("TextDelta(BoundedText)"));
    assert!(progress.contains("ReasoningDelta(BoundedText)"));
    assert_eq!(
        progress
            .lines()
            .filter(|line| line.contains("Delta"))
            .count(),
        2
    );
    assert!(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/model/driver.rs")
            .is_file()
    );
}

#[test]
fn public_model_port_remains_free_of_driver_implementation() {
    let port = include_str!("../src/model/model.rs");
    for forbidden in [
        "ModelDriver",
        "catch_unwind",
        "AssertUnwindSafe",
        "tokio::select!",
        "RetryPolicy",
    ] {
        assert!(!port.contains(forbidden));
    }

    let root = include_str!("../src/lib.rs");
    assert!(!root.contains("ModelDriver"));
    assert!(!root.contains("ModelDriverFailure"));
    assert!(!root.contains("ModelDriverProgress"));
}

#[test]
fn assembler_uses_checked_dtos_and_centralized_json_validation() {
    let assembler = include_str!("../src/model/driver/assembler.rs");
    for required in [
        "validate_json_size(&arguments, maximum)",
        "ToolCall::new(",
        "ReasoningContent::new(",
        "ModelResponse::new(",
        "ModelErrorKind::IncompleteResponse",
        "ModelErrorKind::UnexpectedToolCall",
        "ModelFinishReason::ToolCalls | ModelFinishReason::Unknown",
        "!has_tools && reason == ModelFinishReason::ToolCalls",
    ] {
        assert!(assembler.contains(required), "assembler misses {required}");
    }
    for forbidden in ["unsafe", "serde_json::from_slice", "unwrap_unchecked"] {
        assert!(!assembler.contains(forbidden));
    }
    let lines = assembler.lines().count();
    assert!(lines < 500, "assembler has {lines} lines");
}
