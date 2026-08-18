use minicore_runtime::{InteractionId, SessionEventKind, SessionStatus, TurnId};

const P1_SOURCES: &[&str] = &[
    include_str!("../src/ids.rs"),
    include_str!("../src/error.rs"),
    include_str!("../src/event.rs"),
    include_str!("../src/model/mod.rs"),
    include_str!("../src/model/types.rs"),
    include_str!("../src/tools/mod.rs"),
    include_str!("../src/tools/types.rs"),
    include_str!("../src/session/mod.rs"),
    include_str!("../src/session/state.rs"),
    include_str!("../src/session/snapshot.rs"),
    include_str!("../src/session/event.rs"),
];

#[test]
fn p1_sources_have_no_legacy_imports_or_new_dead_code_suppression() {
    for source in P1_SOURCES {
        assert!(!source.contains("crate::wire"));
        assert!(!source.contains("crate::model_gateway"));
        assert!(!source.contains("crate::session_execution"));
        assert!(!source.contains("crate::session_residency"));
        assert!(!source.contains("crate::tools::"));
        assert!(!source.contains("crate::ids::"));
        assert!(!source.contains("crate::error::"));
        assert!(!source.contains("crate::model::"));
        assert!(!source.contains("crate::session::"));
        assert!(!source.contains("allow(dead_code"));
        assert!(!source.contains("allow(\n    dead_code"));
        assert!(!source.contains("::*"));
    }
}

#[test]
fn every_new_module_is_a_private_alias_with_explicit_root_exports() {
    let lib = include_str!("../src/lib.rs");
    for (alias, path) in [
        ("ids_v2", "ids.rs"),
        ("error_v2", "error.rs"),
        ("event_v2", "event.rs"),
        ("model_v2", "model/mod.rs"),
        ("session_v2", "session/mod.rs"),
        ("tools_v2", "tools/mod.rs"),
    ] {
        let declaration = format!("#[path = \"{path}\"]\npub(crate) mod {alias};");
        assert!(lib.contains(&declaration), "missing private alias {alias}");
    }
    for public_module in [
        "pub mod ids;",
        "pub mod error;",
        "pub mod event;",
        "pub mod model;",
        "pub mod session;",
    ] {
        assert!(
            !lib.contains(public_module),
            "public P1 module leaked: {public_module}"
        );
    }
    assert!(lib.contains("#[path = \"tools.rs\"]\npub mod tools;"));
    assert!(lib.contains("pub use event_v2::SessionEventKind;"));
    assert!(!include_str!("../src/model/mod.rs").contains("pub mod types"));
    assert!(!include_str!("../src/session/mod.rs").contains("pub mod snapshot"));
    assert!(!include_str!("../src/tools/mod.rs").contains("pub mod types"));
    assert!(!lib.contains("pub use ids_v2::*"));
    assert!(!lib.contains("pub use error_v2::*"));
    assert!(!lib.contains("pub use event_v2::*"));
    assert!(!lib.contains("pub use model_v2::*"));
    assert!(!lib.contains("pub use session_v2::*"));
    assert!(!lib.contains("pub use tools_v2::*"));
}

#[test]
fn root_event_is_a_leaf_and_session_event_kind_is_the_public_event_catalog() {
    let root_event = include_str!("../src/event.rs");
    assert!(root_event.contains("enum SessionEventKind"));
    assert!(!root_event.contains("SessionEvent {"));
    assert!(!root_event.contains("use crate::"));
    assert!(!root_event.contains("EventDelivery"));
    let model_types = include_str!("../src/model/types.rs");
    assert!(!model_types.contains("Completed { response"));
    assert!(!model_types.contains("ToolCall { call"));
    assert!(
        P1_SOURCES
            .iter()
            .any(|source| source.contains("enum SessionEvent"))
    );
    assert_eq!(SessionEventKind::Snapshot, SessionEventKind::Snapshot);
}

#[test]
fn session_status_has_exactly_the_four_p1_states() {
    let turn_id = TurnId::new().unwrap();
    let interaction_id = InteractionId::new().unwrap();
    let states = [
        SessionStatus::Idle,
        SessionStatus::Running { turn_id },
        SessionStatus::WaitingForInput {
            turn_id,
            interaction_id,
        },
        SessionStatus::Closing,
    ];
    assert!(matches!(states[0], SessionStatus::Idle));
    assert!(matches!(states[1], SessionStatus::Running { .. }));
    assert!(matches!(states[2], SessionStatus::WaitingForInput { .. }));
    assert!(matches!(states[3], SessionStatus::Closing));
}
