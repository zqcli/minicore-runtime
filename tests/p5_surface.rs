#[test]
fn p5_prompt_surface_is_private_and_uses_only_new_foundations() {
    let lib = include_str!("../src/lib.rs");
    assert!(lib.contains("#[path = \"prompt_v2/mod.rs\"]"));
    assert!(lib.contains("pub(crate) mod prompt_v2;"));
    assert!(!lib.contains("pub use prompt_v2"));
    assert!(!lib.contains("pub mod prompt_v2"));
    let public_exports = lib
        .split_once("pub use model_v2::{")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(exports, _)| exports)
        .unwrap_or("");
    assert!(!public_exports.contains("PromptBuilder"));
    assert!(!public_exports.contains("Compactor"));
    assert!(!public_exports.contains("Plan"));
    assert!(!public_exports.contains("ValidatedSummary"));

    let sources = [
        include_str!("../src/prompt_v2/mod.rs"),
        include_str!("../src/prompt_v2/builder.rs"),
        include_str!("../src/prompt_v2/compaction.rs"),
    ];
    let production_sources = sources
        .iter()
        .map(|source| source.split("#[cfg(test)]").next().unwrap_or(source))
        .collect::<Vec<_>>();
    for source in production_sources.iter() {
        for forbidden in [
            "crate::prompt",
            "crate::compaction",
            "crate::skills",
            "crate::wire",
            "crate::workspace",
            "crate::workspace_v2",
            "crate::live_conversation",
            "crate::conversation_storage",
            "crate::model_gateway",
            "crate::runtime",
            "source",
            "catalog",
            "provenance",
            "std::fs",
            "tokio::spawn",
            "spawn_blocking",
            "allow(dead_code",
            "pub use",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden P5 prompt coupling: {forbidden}"
            );
        }
    }
    let production = production_sources.join("\n");
    for required in [
        "PromptBuilder",
        "PromptBuildOptions",
        "ModelRequest",
        "serde_json::to_vec",
        "CompactionConfig",
        "CompactionConversationView",
        "ValidatedSummary",
        "append_validated_summary",
    ] {
        assert!(
            production.contains(required),
            "missing P5 contract: {required}"
        );
    }
    assert!(production.lines().count() <= 1_000);
}
