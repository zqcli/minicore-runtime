#[test]
fn p3_transport_is_private_and_stays_outside_legacy_owner_boundaries() {
    let source = include_str!("../src/model/transport.rs");
    for forbidden in [
        "crate::wire",
        "crate::model_gateway",
        "crate::http_transport",
        "crate::prompt",
        "crate::session",
        "crate::runtime",
        "allow(dead_code",
        "tokio::spawn",
        "thread::sleep",
        "tokio::time::sleep",
        "tokio::time::timeout",
    ] {
        assert!(!source.contains(forbidden), "found forbidden {forbidden}");
    }
    assert!(!source.contains("pub mod transport"));
    assert!(!source.contains("pub use"));

    let model_mod = include_str!("../src/model/mod.rs");
    assert!(model_mod.contains("pub(crate) mod transport;"));
    assert!(!model_mod.contains("pub mod transport;"));
    assert!(!model_mod.contains("pub use transport"));

    let lib = include_str!("../src/lib.rs");
    assert!(!lib.contains("pub use model::transport"));
}
