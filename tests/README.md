## M12 Provider Gate

`provider-gate/tests/m12_rig_*.rs`在standalone stable-only package中驱动exact `rig-core = 0.40.0`和test-owned `127.0.0.1:0` HTTP servers。这些targets是独立历史协议证据，不安装或定义root crate的Model adapter。历史M12 mapping fixture已归档于`docs/archive/v2/fixtures/provider-gate-m12/error-mapping-v1.json`。

M12 tests必须保持offline和deterministic：不得使用external DNS/network、真实credential、ambient provider config、sleep、timeout-based absence proof、blind yield polling或unjoined server thread。Rig只存在于声明Rust 1.88并拥有独立lockfile的`provider-gate/` evidence package；root dependency/lockfile、production `src/`和public DTO不得出现Rig。`./scripts/check.sh`运行主crate与evidence package；`./scripts/check-msrv.sh`用真实Rust 1.85运行主crate全部targets。

## v0.3 Port And Foundation Test Migration

The v0.2 public Tool/Runtime/concrete-adapter integration targets were deleted as part of the v0.3 reset. Their source and behavior remain historical baseline evidence; they are not compatibility contracts.

- `tests/tool_set_contract.rs` is the focused P3-B replacement for the public `Tool`/`ToolSet` execution seam; `ToolSetBuilder` reports duplicate/spec-panic/invalid-spec failures from `build()` and `specs_for` omits unknown names.
- `tests/tool_policy_interaction_contract.rs` is the focused P3-C contract for the async `ToolPolicy` Port, checked approval decisions, process-local interaction DTOs, answer-kind validation, and redacted Debug surfaces.
- `tests/model_port_contract.rs` is the focused P3-D contract for direct `Model::start`, typed streams, checked descriptors/contexts/requests/events, delivery-safe errors, redaction, and shared concurrency.
- `tests/model_driver_contract.rs` source-checks the private canonical P5-A driver role, forbidden dependencies, checked assembler ownership, file bounds, and unchanged public Model surface; behavioral coverage lives under `src/model/driver/tests/`.
- `tests/session_bindings_contract.rs` is the focused P3-E contract for exact immutable bindings, pure compatibility validation, descriptor panic isolation, frozen ToolSpec semantic limits, optional adapter non-invocation, and P4 load ordering.
- `tests/session_state_event_contract.rs` is the focused P4-A contract for lightweight state invariants, exact event/envelope summaries, diagnostic redaction, and the bounded single-consumer stream surface; private sink behavior remains in module unit tests.
- `tests/turn_handle_contract.rs` is the focused P4-A public contract for exact Turn outcomes, cancellation/completion ownership, safe wait errors, and forbidden owner couplings; concurrency races remain beside the private completion publisher.
- `tests/session_runtime_owner_contract.rs` is the focused P4-B owner contract for options, create/load ordering, replay/repair-before-ready, error/secondary-close preservation, one-shot events, explicit and Drop shutdown, timeout abort+await, and concurrent owner isolation.
- `tests/session_runtime_open_cancellation_contract.rs` proves pre-poll cleanup watchers survive caller cancellation before Close admission, plus cancellation during admitted close, manifest load, later replay pages, and recovery append. Private owner tests prove watcher installation precedes owner spawn, owner-spawn panic closes once, payload claim drains ready-path watchers, cleanup-task panic leaves duplicate ownership intact, and post-ready actor panic closes once before ActorTerminated.
- `tests/session_runtime_timer_contract.rs` proves no-time runtime rejection and full create/shutdown polling from a minimal non-Tokio executor while a timer-enabled configured runtime provides task and timeout progress; private watcher tests prove a no-time current fallback never claims the payload.
- `tests/p2_workspace.rs` was deleted after its v0.2 baseline coverage was recorded; workspace remains private migration implementation, not a public v0.3 module.
- `tests/p1_dto.rs` covers checked public Tool DTOs, strict input answers, content-only output, and redacted debug surfaces.
- Private module tests cover bounded progress delivery and legacy `{text,is_error}` output recovery.
- The former root model registry, concrete adapter, transport, live-smoke, and Runtime-facade tests were deleted and retained only as v0.2 baseline history.
- Final SessionHandle commands and turn/actor execution acceptance remain deferred to P4-C/P5; P4-B does not claim submit support.

The deleted `tests/p3_model_core.rs`, `tests/p3_openai_provider.rs`, `tests/p3_anthropic_provider.rs`, and `tests/p3_transport_surface.rs` are not compatibility contracts. Independent protocol evidence remains only in `provider-gate/`.
