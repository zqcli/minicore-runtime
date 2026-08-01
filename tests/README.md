# Test Tiers

Default tests must be deterministic, bounded, offline and runnable through `cargo test --all-targets`.

Generated boundary tests that may create 1 MiB/1 GiB files or 1,000,001 entries belong only in `tests/heavy_boundaries.rs`. Cargo does not build that target unless the `heavy-tests` feature is selected:

```rust
#[test]
fn generated_boundary_case() {
    // Stream to a temporary file; never allocate the whole artifact in memory.
}
```

Run gates:

```bash
./scripts/check.sh       # default PR gate
./scripts/check-msrv.sh  # Rust 1.85 check/test
./scripts/check-heavy.sh # explicit generated boundary gate
```

`check-msrv.sh` requires rustup and the `1.85.0` toolchain (`rustup toolchain install 1.85.0 --profile minimal`). Heavy tests must clean temporary artifacts on success and failure and must not require network, credentials or ambient home configuration.
