# Test Inventory

Test targets for the current one-loop core. Every test is deterministic: notify /
oneshot / watch / `start_paused` + `advance` only, no long sleeps.

## Integration (`tests/`)

| File | Tests | Scope |
| --- | --- | --- |
| `p1_dto.rs` | 11 | v0.1-era DTO contracts revalidated for the current surface |
| `model_error_contract.rs` | 1 | model error construction and retry hints |
| `model_port_contract.rs` | 7 | model port wire / error contracts |
| `model_driver_contract.rs` | 3 | driver delivery-state contracts |
| `p1_value.rs` | 4 | unrestricted text/JSON width and parser-aligned nesting validation |
| `p1_v04_loop_dtos.rs` | 16 | loop history, `ExecutionConfig`, `PromptProvider`, `LoopReport` DTOs |
| `p2_agent_loop.rs` | 112 | the flexible agent-loop contract end to end: unrestricted payloads, model error preservation, request issue accounting, and lossless connected observation bursts |
| `model_retry_state_contract.rs` | 14 | source-time retry waiting, actual calls and elapsed wait, interrupted/delayed re-polls, per-request timing and no-start accounting, late/absent/slow observation, settlement and next-request clearing, exact retry/deadline bounds, retained partials and failed next preparation |
| `host_observation_contract.rs` | 7 | late watch subscriptions (including after `join`), retained handles, delayed event draining, input identity and candidate request boundaries, driver retry and config-update boundary identity |
| `p3_agent_loop_closeout.rs` | 21 | Phase 8 close-out and correctness fixes: unrestricted catalog/per-response calls, timeout/retry bounds, reasoning channels, event-free runs, multi-batch cancel, repeated answers, policy snapshot swaps, delta-only reports, malformed responses, invalid no-tool finish, loop-level retry, shared-resource isolation, input-survives-prompt-failure |
| `tool_policy_interaction_contract.rs` | 8 | policy decisions, interaction DTO contracts, and ToolInputRequest validation |
| `tool_set_contract.rs` | 11 | `ToolSet` registration, spec validation, enabled subsets |
| `provider_replay.rs` | 2 | large replay payloads, stack safety, serde and legacy wire compatibility |
| `history_handoff_contract.rs` | 16 | ordered incremental handoffs, backpressure, failure, cancellation-safe saving, callback panic isolation and unsupported complete finish preservation, combined failures and first-outcome claims |
| `model_partial_contract.rs` | 7 | authoritative assembly on failure, EOF, cancellation and deadline, partial tool arguments, request identity and complete-response preservation |
| `unbounded_execution_contract.rs` | 3 | real-loop payload/count/progress stress, exact handoff/report/next-request retention, large interaction payloads, unfinished output with primary model and secondary history failures |

## Unit (`src/**` inline and driver)

- 81 unit tests in `src/`:
  - `src/agent_loop/control.rs` (10 tests): control-layer CAS/seal, post-cancel rejection, 512 randomized multi-thread cancel/finish/update races, revision monotonicity, and outside-lock config drop probes.
  - `src/agent_loop/runner/tests.rs` (7 tests): real runner panic with one published report, completion identity, wide counts, complete settlement and post-publication config-destructor panics.
  - `src/model/driver/tests/` (46 tests): assembly (11), retry (8), deadline (6), preflight/progress (3), settlement (3), semantics (8), cancellation (7).
  - `src/tools/` (4 tests): input request validation (1) and progress channel semantics (3).
  - `src/time.rs` (4 tests): monotonic clock and timeout math.
  - `src/port_call.rs` (9 tests): port call execution and timeout isolation.

  - `src/usage.rs` (1 test): partial usage accumulates reported fields.

Total main-crate volume: **324 tests** (81 unit + 243 integration across 16 targets).
Two compile-fail doctests protect private `ToolSpec` and `ToolOutput` fields.
`tests/support/` shares scripted model/tool/policy builders, config/start/join helpers
and report extractors across separate binaries. Driver tests use the actual
`ModelDriverConfig`, not mirrored config types.
The standalone provider harness has 25 tests and is not included in this runtime gate.

## Contract Coverage (`tests/p2_agent_loop.rs`, `tests/p3_agent_loop_closeout.rs`, and `tests/host_observation_contract.rs`)

- Text and tool loops and sequential tools, without a tool-round quota.
- Tool catalog and per-response call counts exceed former application thresholds;
  protocol grammar, duplicate IDs and model capability checks remain enforced.
- Complete tool outputs (`Success`, `Failed`, `Denied`) survive beyond former
  byte caps without downgrades or truncation; `ToolFinished` reports their actual bytes.
- `ToolInputRequest` validated up front; malformed requests fail directly without entering `WaitingForInput`.
- Request-level structured `ModelError` preserved in `LoopFailure::model_error()` for `Model` and `InvalidModelResponse` failures without leaking diagnostics via `Debug`.
- Logical `LoopReport.requests` and state `request_index` aligned at the `RequestStarted` issue boundary; driver retries stay within a single logical request.
- Ready model progress is forwarded without internal starvation. Public and
  internal progress queues retain connected-consumer bursts without capacity loss.
- EOF without any event has Unknown delivery; interrupted retry backoff preserves
  the previous model cause while loop deadlines still cancel the loop.
- Replaced pending configs drop outside the control mutex; finalization-discarded
  configs drop after publication, with host destructor panics isolated.
- Host input (`LoopInputs`) and update request-boundary semantics: A/F takes, stale-prompt rebuild, order, no-tool answers always end the loop, cancellation versus take.
- Cancellation and deadline paths, owner drop, multi-waiter shared report.
- Prompt: failures, panics, semantic validation and full default summary retention.
- Events: optional unbounded stream, reasoning/text channels, no consumer and
  mid-close. `host_observation_contract.rs` also covers draining 2,049 deltas and
  the terminal event after completion, late and post-`join` watch observations,
  and the take boundary a host needs for input identity.
- Interactions: approval and tool-input flows, wrong id/kind, repeated
  answers, deadline while waiting, and no fabricated 24-hour deadline.
- Non-expanding model text normalization beyond the former raw byte limit, direct/constructed
  delta parity, split-CRLF continuity in canonical text and progress, and
  empty-delta suppression; per-attempt child-token
  cleanup; handle shutdown versus dropped-join cancellation.
- History: delta-only reports, mixed model refs, no history or prompt size cap.
- Malformed model assembly never enters Assistant history; a complete response with an unsupported terminal reason remains complete while the loop fails.
- Loop-level delivery retry; shared model/toolset cancel isolation.

## Acceptance

The V4-001..V4-070 acceptance matrix ([`docs/acceptance-v0.4.md`](../docs/acceptance-v0.4.md))
maps every row to concrete tests, gates, docs, or examples; it is generated
and validated by `scripts/check_acceptance.py`. Historical acceptance test
function names are preserved; their bodies and summaries assert current
unrestricted behavior rather than obsolete quota failures.

## Running

```text
cargo test --locked --all-targets --all-features
```

The full gate is `./scripts/check.sh` (fmt, tests, clippy `-D warnings`,
rustdoc `-D warnings`, docs/architecture/acceptance checks). Run
`./scripts/check-provider-gate.sh` separately when changing provider evidence or
its dependency.
### Current provider replay evidence

`provider_replay.rs` covers payloads above former byte/node caps, the shared
JSON stack-safety guard, validated serde, redacted Debug, legacy wire formats
and replay-only content. `p1_value.rs` compares the exact 127/128 nested-container
boundary with the locked serde_json parser for both standalone arrays and objects.
Driver assembly/settlement tests cover single metadata delivery, duplicate and
post-Finish rejection, missing Finish, EOF/error/cancellation settlement, and no
progress forwarding. `p2_agent_loop.rs` covers runner/history/projection retention,
old history serde, and exact history-byte estimation.
