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

## M12 Provider Gate

`tests/m12_rig_*.rs` drives exact `rig-core = 0.40.0` against test-owned `127.0.0.1:0` HTTP servers. These targets cover OpenAI Responses and Anthropic Messages unary/stream contracts, terminal-vs-EOF evidence, cancellation, single-request behavior, typed error envelopes and response metadata allowlists. `tests/m12_provider_error_matrix.rs` consumes `docs/fixtures/provider-gate-m12/error-mapping-v1.json` and freezes delivery-safe retry/normalization rules.

M12 tests must remain offline and deterministic: no external DNS/network, real credential, ambient provider config, sleep, timeout-based absence proof, blind yield polling or unjoined server thread. Rig remains a dev-dependency and may not appear in production `src/` or public DTOs.
