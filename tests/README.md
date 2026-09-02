# Test Inventory

Test targets for the v0.4 core. Every test is deterministic: notify /
oneshot / watch / `start_paused` + `advance` only, no long sleeps.

## Integration (`tests/`)

| File | Tests | Scope |
| --- | --- | --- |
| `p1_dto.rs` | 9 | v0.1-era DTO contracts revalidated for the current surface |
| `p1_value.rs` | 4 | bounded value types and validation |
| `p1_v04_loop_dtos.rs` | 14 | loop history, `ExecutionConfig`, `PromptProvider`, `LoopReport` DTOs |
| `p2_agent_loop.rs` | 72 | the flexible agent-loop contract end to end |
| `p3_agent_loop_closeout.rs` | 14 | Phase 8 close-out: reasoning channels, event-free runs, multi-batch cancel, repeated answers, policy snapshot swaps, history limits, delta-only reports, malformed responses, loop-level retry, shared-resource isolation, steer-survives-prompt-failure |
| `tool_set_contract.rs` | 11 | `ToolSet` registration, spec validation, enabled subsets |
| `tool_policy_interaction_contract.rs` | 6 | policy decisions and interaction DTO contracts |
| `model_port_contract.rs` | 7 | model port wire / error contracts |
| `model_driver_contract.rs` | 3 | driver delivery-state contracts |
| `model_error_contract.rs` | 1 | model error construction and retry hints |

## Unit (`src/**` inline and driver)

- `src/agent_loop/control.rs` — control-layer CAS/seal unit tests
  (`finish_once`/`mark_cancel`/`begin_final`/revision monotonicity, 5 tests).
- `src/agent_loop/runner/tests.rs` — runner-slice unit tests (3).
- `src/model/driver/tests/` — delivery-aware driver: retry (8), assembly (6),
  cancellation (6), deadline (6), semantics (5), settlement (3),
  preflight/progress (3).

## Contract Coverage (`tests/p2_agent_loop.rs` + `tests/p3_agent_loop_closeout.rs`)

- Text and tool loops, sequential tools, max tool rounds.
- Steer / update request-boundary semantics, final-seal races, queue bounds.
- Cancellation and deadline paths, owner drop, multi-waiter shared report.
- Prompt: failures, panics, budget enforcement, default summary truncation.
- Events: best-effort stream, dropped counts, reasoning/text channels, no
  consumer, mid-close.
- Interactions: approval and tool-input flows, wrong id/kind, repeated
  answers, deadline while waiting.
- History: item/byte limits, delta-only reports, mixed model refs.
- Malformed model responses never enter Assistant history.
- Loop-level delivery retry; shared model/toolset cancel isolation.

## Acceptance

The V4-001..V4-070 acceptance matrix ([`docs/acceptance-v0.4.md`](../docs/acceptance-v0.4.md))
maps every row to concrete tests, gates, docs, or examples; it is generated
and validated by `scripts/check_acceptance.py`.

## Running

```text
cargo test --locked --all-targets --all-features
```

The full gate is `./scripts/check.sh` (fmt, tests, clippy `-D warnings`,
rustdoc `-D warnings`, docs/architecture/acceptance checks, provider-gate
harness) plus `./scripts/check-msrv.sh` and `cargo metadata --locked`.