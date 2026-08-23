#[test]
fn context_driver_is_private_single_provider_and_owner_neutral() {
    let module = include_str!("../src/context/mod.rs");
    assert!(module.contains("mod driver;"));
    assert!(module.contains("pub(crate) use driver::ContextDriver;"));
    assert!(!module.contains("pub use driver"));

    let driver = include_str!("../src/context/driver.rs");
    for required in [
        "pub(crate) struct ContextDriver",
        "provider: Option<Arc<dyn ContextProvider>>",
        "pub(crate) async fn provide(",
        "effective_deadline(request.deadline, self.context_timeout)",
        "catch_unwind(AssertUnwindSafe(|| provider.provide(request)))",
        "bundle.validate_and_sort(&self.limits)",
        "ContextError::Cancelled",
        "ContextError::DeadlineExceeded",
        "ContextError::Internal",
    ] {
        assert!(
            driver.contains(required),
            "context driver misses {required}"
        );
    }
    for forbidden in [
        "SessionRuntime",
        "SessionHandle",
        "SessionLog",
        "ConversationLog",
        "Workspace",
        "Store",
        "tokio::spawn",
        "join_all",
        "retry",
        "allow(",
        "expect(",
    ] {
        assert!(
            !driver.contains(forbidden),
            "context driver contains {forbidden}"
        );
    }
    assert!(driver.lines().count() < 500);
}

#[test]
fn final_prompt_builder_is_private_deterministic_and_dependency_narrow() {
    let root = include_str!("../src/lib.rs");
    assert!(root.contains("mod prompt;"));
    assert!(!root.contains("pub mod prompt;"));
    assert!(!root.contains("pub use prompt"));

    let builder = include_str!("../src/prompt/builder.rs");
    for required in [
        "pub(crate) const KERNEL_INVARIANT",
        "pub(crate) struct PromptBuilder",
        "pub(crate) enum PromptError",
        "pub(crate) fn remaining_context_budget(",
        "pub(crate) fn build(",
        "[minicore-context slot={slot} source={}]",
        "validated_prompt_projection(&self.spec, &self.limits)",
        "ModelMessage::system(summary.summary.as_str())",
        "ModelRequest::new(",
        "let request = self.request(messages, model_limits)?;",
        "serde_json::to_vec(request)",
    ] {
        assert!(
            builder.contains(required),
            "prompt builder misses {required}"
        );
    }
    for forbidden in [
        "SessionRuntime",
        "SessionHandle",
        "ContextProvider",
        "ConversationLog",
        "SessionLog",
        "ToolSet",
        "dyn Model",
        "Workspace",
        "Store",
        "std::fs",
        "tokio::spawn",
        "allow(",
        "expect(",
    ] {
        assert!(
            !builder.contains(forbidden),
            "prompt builder contains {forbidden}"
        );
    }
    for removed in [
        "EstimatedPrompt",
        "validate_view",
        "[minicore-summary",
        "validate_json_size(call.arguments()",
        "entry.execution.max_tool_rounds",
    ] {
        assert!(
            !builder.contains(removed),
            "prompt builder retains {removed}"
        );
    }
    assert_eq!(
        builder
            .matches("let request = self.request(messages, model_limits)?;")
            .count(),
        2
    );
    assert_eq!(builder.matches("ModelRequest::new(").count(), 1);
    assert!(builder.lines().count() < 500);
}

#[test]
fn conversation_owns_the_canonical_prompt_projection_proof() {
    let module = include_str!("../src/conversation/mod.rs");
    let view = include_str!("../src/conversation/view.rs");
    assert!(module.contains("pub(crate) use view::PromptConversationProjection;"));
    for required in [
        "pub(crate) struct PromptConversationProjection",
        "pub(crate) fn validated_prompt_projection(",
        "ConversationState::new(spec.clone(), limits.clone())?",
        ".candidate(self.entries())?",
        "state.head() != self.head",
        "state.projection().latest_summary().cloned()",
        "entry.seq() > through",
    ] {
        assert!(
            view.contains(required),
            "conversation proof misses {required}"
        );
    }
    for forbidden in [
        "pub struct PromptConversationProjection",
        "pub use view::PromptConversationProjection",
    ] {
        assert!(!module.contains(forbidden));
        assert!(!view.contains(forbidden));
    }
    assert!(view.lines().count() < 500);
}

#[test]
fn legacy_prompt_files_are_test_only_and_public_surfaces_do_not_expand() {
    for source in [
        include_str!("../src/prompt/legacy.rs"),
        include_str!("../src/prompt/legacy_builder.rs"),
        include_str!("../src/prompt/legacy_compaction.rs"),
    ] {
        assert!(source.starts_with("#![cfg(test)]"));
    }
    let context = include_str!("../src/context/mod.rs");
    let root = include_str!("../src/lib.rs");
    assert!(!context.contains("pub use driver::ContextDriver"));
    for forbidden in [
        "ContextDriver",
        "PromptBuilder",
        "PromptError",
        "KERNEL_INVARIANT",
        "PromptConversationProjection",
    ] {
        assert!(!root.contains(forbidden));
    }
}
