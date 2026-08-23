#[test]
fn final_prompt_surface_is_private_and_legacy_prompt_is_test_only() {
    let lib = include_str!("../src/lib.rs");
    assert!(lib.contains("mod prompt;"));
    assert!(!lib.contains("pub mod prompt"));
    assert!(!lib.contains("pub use prompt"));

    let module = include_str!("../src/prompt/mod.rs");
    let builder = include_str!("../src/prompt/builder.rs");
    let legacy = include_str!("../src/prompt/legacy.rs");
    assert!(module.contains("mod builder;"));
    assert!(module.contains("#[cfg(test)]\nmod legacy;"));
    assert!(
        module.contains("pub(crate) use builder::{KERNEL_INVARIANT, PromptBuilder, PromptError}")
    );
    assert!(legacy.starts_with("#![cfg(test)]"));
    assert!(legacy.contains("#[path = \"legacy_builder.rs\"]\nmod builder;"));
    assert!(legacy.contains("#[path = \"legacy_compaction.rs\"]\nmod compaction;"));

    for required in [
        "pub(crate) const KERNEL_INVARIANT",
        "pub(crate) struct PromptBuilder",
        "pub(crate) enum PromptError",
        "pub(crate) fn remaining_context_budget(",
        "pub(crate) fn build(",
        "ConversationView",
        "ContextBundle",
        "validated_prompt_projection",
        "ModelRequest::new(",
        "estimate_request",
        "serde_json::to_vec(request)",
    ] {
        assert!(
            builder.contains(required),
            "missing final prompt role: {required}"
        );
    }
    for forbidden in [
        "SessionRuntime",
        "SessionHandle",
        "ContextProvider",
        "ConversationLog",
        "SessionLog",
        "Workspace",
        "Store",
        "std::fs",
        "tokio::spawn",
        "spawn_blocking",
        "allow(",
        "expect(",
    ] {
        assert!(
            !builder.contains(forbidden),
            "forbidden prompt coupling: {forbidden}"
        );
    }
    for removed in ["EstimatedPrompt", "validate_view", "[minicore-summary"] {
        assert!(
            !builder.contains(removed),
            "removed prompt logic remains: {removed}"
        );
    }
    assert!(builder.lines().count() < 500);
}
