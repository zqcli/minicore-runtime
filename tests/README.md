## M12 Provider Gate

`provider-gate/tests/m12_rig_*.rs`在standalone stable-only package中驱动exact `rig-core = 0.40.0`和test-owned `127.0.0.1:0` HTTP servers。这些targets是独立历史协议证据，不安装或定义root crate的Model adapter。历史M12 mapping fixture已归档于`docs/archive/v2/fixtures/provider-gate-m12/error-mapping-v1.json`。

M12 tests必须保持offline和deterministic：不得使用external DNS/network、真实credential、ambient provider config、sleep、timeout-based absence proof、blind yield polling或unjoined server thread。Rig只存在于声明Rust 1.88并拥有独立lockfile的`provider-gate/` evidence package；root dependency/lockfile、production `src/`和public DTO不得出现Rig。`./scripts/check.sh`运行主crate与evidence package；`./scripts/check-msrv.sh`用真实Rust 1.85运行主crate全部targets。

## v0.3 Port And Foundation Test Migration

The v0.2 public Tool/Runtime/concrete-adapter integration targets were deleted as part of the v0.3 reset. Their source and behavior remain historical baseline evidence; they are not compatibility contracts.

- `tests/tool_set_contract.rs` is the focused P3-B replacement for the public `Tool`/`ToolSet` execution seam; `ToolSetBuilder` reports duplicate/spec-panic/invalid-spec failures from `build()` and `specs_for` omits unknown names.
- `tests/tool_policy_interaction_contract.rs` is the focused P3-C contract for the async `ToolPolicy` Port, checked approval decisions, process-local interaction DTOs, answer-kind validation, and redacted Debug surfaces.
- `tests/model_port_contract.rs` is the focused P3-D contract for direct `Model::start`, typed streams, checked descriptors/contexts/requests/events, delivery-safe errors, redaction, and shared concurrency.
- `tests/session_bindings_contract.rs` is the focused P3-E contract for exact immutable bindings, pure compatibility validation, descriptor panic isolation, frozen ToolSpec semantic limits, optional adapter non-invocation, and P4 load ordering.
- `tests/session_state_event_contract.rs` is the focused P4-A contract for lightweight state invariants, exact event/envelope summaries, diagnostic redaction, and the bounded single-consumer stream surface; private sink behavior remains in module unit tests.
- `tests/turn_handle_contract.rs` is the focused P4-A public contract for exact Turn outcomes, cancellation/completion ownership, safe wait errors, and forbidden owner couplings; concurrency races remain beside the private completion publisher.
- `tests/p2_workspace.rs` was deleted after its v0.2 baseline coverage was recorded; workspace remains private migration implementation, not a public v0.3 module.
- `tests/p1_dto.rs` covers checked public Tool DTOs, strict input answers, content-only output, and redacted debug surfaces.
- Private module tests cover bounded progress delivery and legacy `{text,is_error}` output recovery.
- The former root model registry, concrete adapter, transport, live-smoke, and Runtime-facade tests were deleted and retained only as v0.2 baseline history.
- SessionRuntime/actor acceptance and end-to-end replacement coverage is intentionally deferred to P4-B/P5; this revision does not claim complete replacement.

The deleted `tests/p3_model_core.rs`, `tests/p3_openai_provider.rs`, `tests/p3_anthropic_provider.rs`, and `tests/p3_transport_surface.rs` are not compatibility contracts. Independent protocol evidence remains only in `provider-gate/`.
