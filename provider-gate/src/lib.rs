//! Stable-only evidence harness for the M12 Rig provider gate.
//!
//! This package pins `rig-core =0.40.0`, declares the Rust 1.88 language
//! floor required by that dependency's source, and hosts the
//! seven `tests/m12_rig_*.rs` integration targets against test-owned
//! `127.0.0.1:0` HTTP servers. It is intentionally not a member of the main
//! `minicore-runtime` workspace: Rig must never enter the production
//! baseline, and the MSRV (Rust 1.85) gate never builds this package.
//!
//! All gate code lives in the integration tests; this lib target exists so
//! `cargo test --all-targets` exercises the package as a normal crate.
