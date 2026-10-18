# Test Inventory

Test targets for the v0.7.0 core. Every test is deterministic: notify /
oneshot / watch / `start_paused` + `advance` only, no long sleeps.

## Integration (`tests/`)

| File | Tests | Scope |
| --- | --- | --- |
| `p1_dto.rs` | 11 | v0.1-era DTO contracts revalidated for the current surface |
| `model_error_contract.rs` | 1 | model error construction and retry hints |
| `model_port_contract.rs` | 7 | model port wire / error contracts |
| `model_driver_contract.rs` | 3 | driver delivery-state contracts |
| `p1_value.rs` | 4 | bounded value types and validation |
| `p1_v04_loop_dtos.rs` | 16 | loop history, `ExecutionConfig`, `PromptProvider`, `LoopReport` DTOs |
| `p2_agent_loop.rs` | 112 | the flexible agent-loop contract end to end: output bounding, model error preservation, request issue accounting, and progress drop loss |
| `p3_agent_loop_closeout.rs` | 22 | Phase 8 close-out and correctness fixes: catalog/per-response call limits, timeout/retry bounds, reasoning channels, event-free runs, multi-batch cancel, repeated answers, policy snapshot swaps, history limits, delta-only reports, malformed responses, invalid no-tool finish, loop-level retry, shared-resource isolation, steer-survives-prompt-failure |
| `tool_policy_interaction_contract.rs` | 8 | policy decisions, interaction DTO contracts, and ToolInputRequest validation |
| `tool_set_contract.rs` | 11 | `ToolSet` registration, spec validation, enabled subsets |
| `provider_replay.rs` | 2 | replay bounds, serde and legacy wire compatibility |

## Unit (`src/**` inline and driver)

- 81 unit tests in `src/`:
  - `src/agent_loop/control.rs` (10 tests): control-layer CAS/seal, post-cancel rejection, 512 randomized multi-thread cancel/finish/steer/update races, revision monotonicity, and outside-lock config drop probes.
  - `src/agent_loop/runner/tests.rs` (7 tests): real runner panic with one published report, completion identity, wide counts, bounded settlement and post-publication config-destructor panics.
  - `src/model/driver/tests/` (46 tests): assembly (11), retry (8), deadline (6), preflight/progress (3), settlement (3), semantics (8), cancellation (7).
  - `src/tools/` (4 tests): input request validation (1) and progress channel semantics (3).
  - `src/time.rs` (4 tests): monotonic clock and timeout math.
  - `src/port_call.rs` (9 tests): port call execution and timeout isolation.

  - `src/usage.rs` (1 test): partial usage accumulates reported fields.

Total main-crate volume: **278 tests** (81 unit + 197 integration across 11 targets).
Two compile-fail doctests protect private `ToolSpec` and `ToolOutput` fields.
`tests/support/` shares scripted model/tool/policy builders, config/start/join helpers
and report extractors across separate binaries. Driver tests use the actual
`ModelDriverConfig` and `LoopLimits`, not mirrored config types.
The standalone provider harness has 25 tests and is not included in this runtime gate.

## Contract Coverage (`tests/p2_agent_loop.rs` + `tests/p3_agent_loop_closeout.rs`)

- Text and tool loops, sequential tools, max tool rounds.
- Tool catalog size is independent from per-response `ToolCall` limits; registration and start bounds enforced fail-fast.
- All tool outputs (`Success`, `Failed`, `Denied`) bounded by `max_tool_output_bytes`; oversized successes downgraded to failed with matching `ToolFinished` byte reporting.
- `ToolInputRequest` validated up front; malformed requests fail directly without entering `WaitingForInput`.
- Request-level structured `ModelError` preserved in `LoopFailure::model_error()` for `Model` and `InvalidModelResponse` failures without leaking diagnostics via `Debug`.
- Logical `LoopReport.requests` and state `request_index` aligned at the `RequestStarted` issue boundary; driver retries stay within a single logical request.
- Ready model progress is forwarded without internal starvation; best-effort
  queue loss is still merged into `LoopEventEnvelope.dropped_before`.
- EOF without any event has Unknown delivery; interrupted retry backoff preserves
  the previous model cause while loop deadlines still cancel the loop.
- Replaced pending configs drop outside the control mutex; finalization-discarded
  configs drop after publication, with host destructor panics isolated.
- Steer / update request-boundary semantics, final-seal races, queue bounds.
- Cancellation and deadline paths, owner drop, multi-waiter shared report.
- Prompt: failures, panics, budget enforcement, default summary truncation.
- Events: best-effort stream, dropped counts, reasoning/text channels, no
  consumer, mid-close.
- Interactions: approval and tool-input flows, wrong id/kind, repeated
  answers, deadline while waiting, and no fabricated 24-hour deadline.
- Non-expanding model text normalization at the raw byte limit, direct/constructed
  delta parity, split-CRLF continuity in canonical text and progress, and
  empty-delta suppression; per-attempt child-token
  cleanup; handle shutdown versus dropped-join cancellation.
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
rustdoc `-D warnings`, docs/architecture/acceptance checks) plus
`./scripts/check-msrv.sh`. Run `./scripts/check-provider-gate.sh` separately
when changing provider evidence or its dependency (Rust >= 1.88).
### v0.6.1 provider replay evidence

`provider_replay.rs` covers full-envelope byte/depth/node bounds, validated serde,
redacted Debug, legacy constructors/wire formats, and replay-only content.
Driver assembly/settlement tests cover single metadata delivery, duplicate and
post-Finish rejection, missing Finish, EOF/error/cancellation settlement, and no
progress forwarding. `p2_agent_loop.rs` covers runner/history/projection retention,
old history serde, exact history-byte accounting, and the 2 MiB absolute ceiling.
