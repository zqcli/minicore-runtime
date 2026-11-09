# minicore-runtime Release Notes From 0.7.0

Versioned sections describe changes at that version. Current behavior is
defined by the contracts and the latest README compatibility notes.

## 0.10.0 (prepared locally)

### Application quotas and API migration

- Rename `BoundedText` to `Text`; `Text::new` and model delta constructors are
  infallible. Text serialization remains a plain JSON string. Remove quota
  constructors/constants and `ModelEvent::validate` rather than retaining
  aliases or Result/no-op compatibility shells.
- Remove `LoopLimits`/`LoopLimitsError` and `LoopOptions`'s `limits`,
  `max_tool_rounds` and `event_capacity`. Remove budget-only failure/error
  variants listed in the [migration notes](../README.md#0100).
- Remove application byte/node/count limits for text, reasoning, tool
  schemas/arguments/results, interactions, replay, tool calls, rounds, history
  and prompts. Preserve the full default summary prefix and content. Large
  successful tool output remains successful, not downgraded or truncated.
- Public events and internal model/tool progress use unbounded, non-awaiting
  channels. Remove full/drop paths and `LoopEventEnvelope::dropped_before`.
  Observation stays optional; no persistent event store, acknowledgement or
  replay mode is added. History callbacks and final reports remain authoritative.
- Retain protocol grammar and ID/name lengths, nonempty/control/type checks,
  valid tool associations, model capabilities/token limits, and timeout/retry
  rules. `Length` still means the model reached an output limit.
- Replace arbitrary JSON depth 32 with the locked serde_json default
  stack-safety boundary: 127 nested arrays/objects accepted, 128 rejected.
  Tests compare the guard directly with the parser, and accept payloads above
  the former byte/node limits. Enclosing wire objects also count in the parser.
- Historical acceptance test function names remain stable while their bodies
  and current summaries describe removed quotas honestly. The architecture
  gate mutation-tests unbounded/non-awaiting observations and a unit-return sink.
- Package version fields change to 0.10.0 without dependency updates. No tag
  or remote publication is implied; the integrating workstream runs final gates.

### Local verification

- The 13 migrated integration targets pass all 114 existing tests, with their
  function identities unchanged; focused Clippy also passes with warnings denied.
  The standalone Value guard is checked against the locked parser for both arrays
  and objects at 127/128 containers.
- Documentation, architecture and acceptance gates and their mutation self-tests
  pass. The acceptance matrix retains all 70 stable identities.
- Full integration with the source-unit, loop and new stress tests is checked
  by the integrating workstream before release. Provider evidence has no Runtime
  dependency and no affected caller, so its opt-in gate is not rerun for this
  migration. No live-provider result is claimed.

### Complete-history handoff and unfinished output

- Add the optional `LoopOptions.history` receiver and root-exported `LoopHistory`
  and `HistoryFuture`. It acknowledges only new complete records, in order, at
  response, tool-batch and final-unwind boundaries before continuation.
- Handoffs use the existing `LoopId` and offsets within the full final delta.
  Failure does not retry accepted ranges or completed tools; callback errors and
  panics produce `LoopFailureKind::History`. Cancellation waits for already-owned
  callback work. Events remain independent, best-effort observations.
- Add `LoopReport.partial` and serializable `model::ModelPartial` /
  `model::IncompleteToolCall`. Interrupted stream assembly moves its actual
  content into the report without relying on events. Existing complete parts
  are reused; unfinished calls retain raw arguments and positions without
  becoming executable. A complete response remains complete if tools later fail.
- Preserve complete `Unknown`-finish no-tool responses in history and handoff
  before returning the existing `InvalidModelResponse` outcome. Output and
  actual finish metadata are no longer discarded or mistaken for a partial.
  Aggregate usage overflow likewise retains the new complete response before
  returning the existing internal-failure outcome.
- `LoopReport.history_failure` preserves a callback diagnostic separately from
  the primary business outcome. Model/prompt/policy failures are not replaced by
  later saving failures, and structured model errors survive. Non-completed
  endings claim their outcome before awaited saving; an earlier cancellation
  still wins. A completed no-tool candidate can retain `Completed` with a
  history failure, which the host must treat as a blocked save boundary.

## 0.7.0

The one-loop host boundary, JSON representations and storage-independent design
remain unchanged. Public Rust API changes are listed in the
[README compatibility notes](../README.md#070).

### Changes

- Runtime normalizes model display text (CRLF/CR to LF, other controls except
  LF/TAB to a space), without expanding bytes. Assembly preserves CRLF continuity
  across text/reasoning deltas and ignores empty deltas. Tool text validation is
  consistent across input, progress and diagnostics.
- Large immutable text, reasoning, tool arguments and replay payloads share
  storage. Requests share message/tool slices and reuse the frozen tool catalog;
  history projection copies item handles rather than payload bytes.
- Model attempts receive child cancellation tokens cancelled on settlement,
  and reuse one pinned timeout. Interactions without a loop deadline wait until
  answered or cancelled, including beyond 24 hours.
- Finalization drops discarded configurations only after publishing the report.
  Destructor-panic regression tests preserve appended history and report identity.
- Cancellation immediately rejects later steering, updates and answers. Randomized
  multi-thread control races and a real runner panic verify completion identity.
- Ready model progress is drained fairly. Empty-stream EOF has Unknown delivery,
  and retry-backoff interruption preserves the previous model cause.
- `ToolSpec` fields are private with existing read-only accessors; registration and
  request construction trust checked specs instead of validating schemas again.
  `ToolSetError::InvalidSpec` is removed. Debug distinguishes replay-bearing assistants.
- Usage sums reported values independently per field; omitted values do not erase
  known counts. `LoopHandle::shutdown()` records host shutdown.
- `history::estimate_history_bytes` exposes the exact size estimate, with
  allocation-free counting of compact tool-argument JSON.
- Model contracts are split by responsibility. Integration fixtures are shared
  under `tests/support/`; test binaries and acceptance function names are retained.

## 0.8.0

- **Breaking:** `LoopLimits::max_history_items`, `max_history_bytes` and
  `max_prompt_messages` and `LoopStartError::HistoryTooLarge` are removed.
  History and prompt size are governed only by the host's token-based context
  compaction; per-item bounds are unchanged. See the
  [README compatibility notes](../README.md#080).

## 0.9.0

- **Breaking:** `AgentLoop::start` takes an `Arc<dyn LoopInputs>`; the host's
  `take` hands over user inputs before every request preparation and again
  after a prompt is prepared (a non-empty answer rebuilds the prompt without
  advancing `request_index`). `LoopHandle::steer`, `SteerError`,
  `LoopOptions::max_pending_steers` and the interaction-time steer refusal are
  removed, and a model answer without tool calls always ends the loop. See the
  [README compatibility notes](../README.md#090) and the
  [agent-loop contract](contracts/agent-loop.md#inputs).

## 0.9.1

- No API or behavior changes. Loop outcomes, `LoopInputs` pull semantics,
  request-boundary accounting and event bounds are those of 0.9.0. The version
  follows the 0.9.1 consistency work in `minicore-agent`; Runtime still has no
  session, storage, transcript or provider surface.
- `tests/host_observation_contract.rs` records the host-facing observation contracts
  the Agent relies on: late `watch_state` subscriptions see actionable
  interactions, a retained handle keeps the state sender open after `join`, a
  final state and report survive complete event loss, and `take` is not a
  request index.

## 0.9.2

- Documentation and test maintenance only: production code, APIs, dependency
  versions and the 0.9.0 loop behavior are unchanged.
- Consolidate the seven host-observation cases in one test target and document
  initial/terminal watch observations, retained sender lifetime, lossy terminal
  events and host-owned candidate input identities.
- Align README, coding-agent guidance and the architecture/module/test indexes
  with the current source; retain the earlier versioned compatibility record.

## Gates

The runtime gate is:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features
bash scripts/check-architecture.sh
```

Since 0.7.3 no minimum Rust version is tested; the gate runs on current stable.

The main suite has 307 tests: 81 unit and 226 integration tests in 14 binaries.
Two compile-fail doctests additionally protect the checked tool DTO boundaries.
The two examples compile as test targets. The acceptance checker preserves all
70 Passed rows and validates their source/test references. See the
[test inventory](../tests/README.md) for per-target counts.

Provider evidence is separate: `./scripts/check-provider-gate.sh` runs its own
fmt/test/clippy gate when that harness or its dependency changes.
It is no longer invoked by `scripts/check.sh`. Historical v0.4 release records
retain the gate and counts that applied at that release.

## Host Adaptation

Besides the Rust API updates in README, adapters should remove their own rejection
of CR/ESC model display text and let Runtime normalize it. Delta constructors
preserve raw text: adapters can chunk at the raw event byte limit on UTF-8
boundaries without expansion headroom or special CRLF handling. Hosts can replace
mirrored history-size accounting with the exported estimator and use handle
shutdown when closing a session. See the [adapter obligations](integration/host-boundary.md#adapter-obligations)
for cooperative futures, panic unwinding, dropped work and deadline ties.
