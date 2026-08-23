#[test]
fn canonical_modules_keep_only_the_current_root_facade() {
    let lib = include_str!("../src/lib.rs");
    for declaration in [
        "pub mod config;",
        "pub mod conversation;",
        "pub mod error;",
        "pub mod ids;",
        "pub mod model;",
        "pub mod session;",
        "pub mod storage;",
        "pub mod tools;",
        "mod workspace;",
    ] {
        assert!(
            lib.contains(declaration),
            "missing canonical module: {declaration}"
        );
    }
    assert!(!lib.contains("mod runtime;"));
    assert!(lib.contains("mod agent;"));
    assert!(lib.contains("mod prompt;"));
    assert!(lib.contains("pub mod compaction;"));
    assert!(lib.contains("pub mod context;"));
    assert!(!lib.contains("pub mod event;"));
    assert!(!lib.contains("SessionEventKind"));
    assert!(!lib.contains("pub mod runtime;"));
    assert!(!lib.contains("pub mod workspace;"));
    assert!(!lib.contains("pub use workspace"));
    assert!(!include_str!("../src/workspace/mod.rs").contains("pub use "));
    for removed in [
        "RuntimeConfig",
        "RuntimeConfigBuilder",
        "SessionConfig",
        "SessionSummary",
    ] {
        assert!(
            !lib.contains(removed),
            "legacy root export remains: {removed}"
        );
    }
    let compact = lib.split_whitespace().collect::<Vec<_>>().join("");
    let session_exports = compact
        .split_once("pubusesession::{")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(exports, _)| exports)
        .unwrap();
    let session_exports = session_exports.split(',').collect::<Vec<_>>();
    assert!(!session_exports.contains(&"Runtime"));
    assert!(session_exports.contains(&"SessionHandle"));
    assert!(session_exports.contains(&"SessionRuntime"));
    assert!(session_exports.contains(&"SessionRuntimeOptions"));
    assert!(!lib.contains("pub use runtime"));
    assert!(!include_str!("../src/tools/mod.rs").contains("pub use registry"));
}

#[test]
fn model_and_tool_errors_keep_the_public_legacy_split() {
    let model = include_str!("../src/model/types.rs");
    let public_model = model
        .split("pub enum ModelMessage")
        .nth(1)
        .and_then(|tail| tail.split("#[derive(Deserialize)]").next())
        .expect("public ModelMessage definition");
    assert!(public_model.contains("output: ToolOutput"));
    assert!(public_model.contains("outcome: ToolResultOutcome"));
    assert!(!public_model.contains("LegacyToolOutput"));

    let tool_errors = include_str!("../src/tools/types.rs");
    for legacy_variant in [
        "UnknownTool",
        "DuplicateTool",
        "InteractionClosed",
        "InteractionBusy",
        "InvalidInteraction",
    ] {
        assert!(!tool_errors.contains(legacy_variant));
    }
    assert!(include_str!("../src/tools/legacy_types.rs").contains("enum LegacyToolError"));
}

#[test]
fn source_has_no_legacy_imports_or_new_dead_code_suppression() {
    for source in [
        include_str!("../src/lib.rs"),
        include_str!("../src/tools/tool.rs"),
        include_str!("../src/tools/set.rs"),
        include_str!("../src/tools/context.rs"),
        include_str!("../src/tools/input.rs"),
    ] {
        for forbidden in [
            "crate::wire",
            "crate::runtime",
            "allow(dead_code",
            "allow(\n    dead_code",
            "::*",
            "ToolRegistry",
            "LegacyTool",
            "Workspace",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden final surface token: {forbidden}"
            );
        }
    }
}
