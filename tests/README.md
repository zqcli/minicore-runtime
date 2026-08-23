## M12 Provider Gate

`provider-gate/tests/m12_rig_*.rs`在standalone stable-only package中驱动exact `rig-core = 0.40.0`和test-owned `127.0.0.1:0` HTTP servers。这些targets覆盖OpenAI Responses与Anthropic Messages unary/stream contracts、terminal-vs-EOF evidence、cancellation、single-request behavior、typed error envelopes与response metadata allowlists。历史M12 mapping fixture已归档于`docs/archive/v2/fixtures/provider-gate-m12/error-mapping-v1.json`；当前生产 provider matrix由P3 provider suites与AT-13拥有。

M12 tests必须保持offline和deterministic：不得使用external DNS/network、真实credential、ambient provider config、sleep、timeout-based absence proof、blind yield polling或unjoined server thread。Rig只存在于声明Rust 1.88并拥有独立lockfile的`provider-gate/` evidence package；root dependency/lockfile、production `src/`和public DTO不得出现Rig。`./scripts/check.sh`运行主crate与evidence package；`./scripts/check-msrv.sh`用真实Rust 1.85运行主crate全部targets。

## P3-B Test Migration

The v0.2 public Tool/Runtime/concrete-adapter integration targets were deleted as part of the v0.3 reset. Their source and behavior remain historical baseline evidence; they are not compatibility contracts.

- `tests/tool_set_contract.rs` is the focused P3-B replacement for the public `Tool`/`ToolSet` execution seam; `ToolSetBuilder` reports duplicate/spec-panic/invalid-spec failures from `build()` and `specs_for` omits unknown names.
- `tests/p2_workspace.rs` was deleted after its v0.2 baseline coverage was recorded; workspace remains private migration implementation, not a public v0.3 module.
- `tests/p1_dto.rs` covers checked public Tool DTOs, strict input answers, content-only output, and redacted debug surfaces.
- Private module tests cover bounded progress delivery and legacy `{text,is_error}` output recovery.
- The former `tests/m14_live_provider_smoke.rs` and `tests/p7_runtime_surface.rs` Runtime/provider-facade tests were removed with the public Runtime facade.
- SessionRuntime acceptance and end-to-end replacement coverage is intentionally deferred to P4/P5; this revision does not claim complete replacement.

Provider protocol suites under `tests/p3_*_provider.rs` remain transitional model-owner evidence. They do not imply that the removed Runtime or ToolRegistry facade is public.
