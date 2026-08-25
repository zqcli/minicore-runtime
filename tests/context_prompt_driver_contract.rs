#[test]
fn context_driver_is_private_single_provider_and_owner_neutral() {
    let module = include_str!("../src/context/mod.rs");
    assert!(module.contains("mod driver;"));
    assert!(module.contains("pub(crate) use driver::{ContextDriver, ContextDriverFailure};"));
    assert!(!module.contains("pub use driver"));

    let driver = include_str!("../src/context/driver.rs");
    for required in [
        "pub(crate) struct ContextDriver",
        "pub(crate) struct ContextDriverFailure",
        "provider: Option<Arc<dyn ContextProvider>>",
        "pub(crate) async fn provide(",
        "pub(crate) async fn provide_detailed(",
        "effective_deadline(request.deadline, self.context_timeout)",
        "deadline_source: Option<DeadlineSource>",
        "ContextDriverFailure::deadline(deadline.source())",
        "catch_unwind(AssertUnwindSafe(|| provider.provide(request)))",
        "ContextError::Cancelled",
        "ContextError::DeadlineExceeded",
        "ContextError::Internal",
    ] {
        assert!(
            driver.contains(required),
            "context driver misses {required}"
        );
    }
    assert!(driver.matches(".validate_and_sort(&self.limits)").count() >= 2);
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
    assert!(!driver.contains("fn effective_deadline("));
    assert!(driver.lines().count() < 500);
}

#[test]
fn shared_deadline_seam_is_private_checked_and_turn_conservative() {
    let time = include_str!("../src/time.rs");
    for required in [
        "pub(crate) enum DeadlineSource",
        "Turn,",
        "Port,",
        "pub(crate) struct EffectiveDeadline",
        "pub(crate) fn effective_deadline(",
        "now.checked_add(port_timeout).ok_or(DeadlineOverflow)?",
        "if turn_deadline <= port_deadline",
        "DeadlineSource::Turn",
        "DeadlineSource::Port",
    ] {
        assert!(time.contains(required), "deadline seam misses {required}");
    }
    assert!(!time.contains("pub enum DeadlineSource"));
    assert!(!time.contains("pub struct EffectiveDeadline"));
    let root = include_str!("../src/lib.rs");
    assert!(!root.contains("DeadlineSource"));
    assert!(!root.contains("EffectiveDeadline"));
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
        "pub(crate) fn remaining_context_budget_for(",
        "pub(crate) fn build(",
        "[minicore-context slot={slot} source={}]",
        "validated_prompt_projection(&self.spec, &self.limits)",
        "ModelMessage::system(summary.summary.as_str())",
        "ModelRequest::new(",
        "let request = self.request(messages, model_limits)?;",
        "serde_json::to_writer(&mut sink, request)",
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
    // Estimation must never materialise the serialized prompt: a counting sink
    // keeps the byte count exact without allocating a copy per round.
    assert!(!builder.contains("serde_json::to_vec"));
    assert!(builder.contains("serde_json::to_writer"));
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
        "pub(crate) fn validated_active_turn(",
        // A cached state is only reused when it proves the same head and the
        // same spec/limits; otherwise the validator is replayed in full.
        "state.head() == self.head && state.matches_configuration(spec, limits)",
        "ConversationState::new(spec.clone(), limits.clone())?",
        "state.head() != self.head",
        "state.projection().latest_summary().cloned()",
        "Some(entry.execution.clone())",
        "entry.seq() > through",
    ] {
        assert!(
            view.contains(required),
            "conversation proof misses {required}"
        );
    }
    assert!(!view.contains("active_turn_input"));
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
fn prompt_legacy_files_are_absent_and_public_surfaces_do_not_expand() {
    let root_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/prompt/legacy.rs",
        "src/prompt/legacy_builder.rs",
        "src/prompt/legacy_compaction.rs",
    ] {
        assert!(!root_dir.join(relative).exists());
    }
    let context = include_str!("../src/context/mod.rs");
    let root = include_str!("../src/lib.rs");
    assert!(!context.contains("pub use driver::ContextDriver"));
    for forbidden in [
        "ContextDriver",
        "ContextDriverFailure",
        "PromptBuilder",
        "PromptError",
        "KERNEL_INVARIANT",
        "PromptConversationProjection",
    ] {
        assert!(!root.contains(forbidden));
    }
}
