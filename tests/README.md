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

`provider-gate/tests/m12_rig_*.rs`在standalone stable-only package中驱动exact `rig-core = 0.40.0`和test-owned `127.0.0.1:0` HTTP servers。这些targets覆盖OpenAI Responses与Anthropic Messages unary/stream contracts、terminal-vs-EOF evidence、cancellation、single-request behavior、typed error envelopes与response metadata allowlists。`tests/m12_provider_error_matrix.rs`留在主crate，消费`docs/fixtures/provider-gate-m12/error-mapping-v1.json`并在Rust 1.85下冻结delivery-safe retry/normalization rules。

M12 tests必须保持offline和deterministic：不得使用external DNS/network、真实credential、ambient provider config、sleep、timeout-based absence proof、blind yield polling或unjoined server thread。Rig只存在于声明Rust 1.88并拥有独立lockfile的`provider-gate/` evidence package；root dependency/lockfile、production `src/`和public DTO不得出现Rig。`./scripts/check.sh`运行主crate与evidence package；`./scripts/check-msrv.sh`用真实Rust 1.85运行主crate全部targets。
