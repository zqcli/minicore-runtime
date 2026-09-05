# Current Implementation Context

v0.4 is the "flex agent loop reset": the runtime converged on a single
boundary — one live `AgentLoop`. Everything session-ish (session runtime,
session state/log, conversation ledger, durable compaction, storage, agent
definitions, workspace capability models, providers) was removed from the
production surface. The design intent is a thin execution core the host
composes; a multi-turn `MemoryAgent` is deliberately an example, not a
library type.

## Where the Core Stands

- `src/agent_loop/` owns the loop: `control` (linearized update/steer/seal
  state), `event` (best-effort stream), `handle` (host control surface),
  `state`, and `runner` — split into `runner.rs` (main loop / control /
  final), `runner/model.rs` (prompt preparation + model driver), and
  `runner/tools.rs` (policy, interaction, tool execution). One `tokio::spawn`
  per loop; no async mutex; all events are `try_send` best-effort.
- `src/execution.rs` holds `ExecutionConfig` + `UserInput`; `src/history.rs`
  the host-facing `HistoryItem` vocabulary; `src/prompt.rs` the
  `PromptProvider` seam with `DefaultPromptProvider`.
- `src/model`, `src/tools`, `src/limits`, `src/usage`, `src/interaction`,
  `src/ids`, `src/value`, `src/time`, `src/error`, `src/port_call` carry the
  typed seams and validation the loop builds on. No builder, no sessions.
- The public root re-exports only loop, execution, history, model, prompt,
  tools, interaction, ids, limits, value, and error types (see `src/lib.rs`).

## Engine Rules

- One runner task per loop; `wait`/`join` return the same `Arc<LoopReport>`.
- Completion is owned by the runner task; its drop surfaces as
  `LoopWaitError::CompletionClosed` to waiters.
- Update/steer and the final seal linearize on one short `std::sync::Mutex`;
  cancel/finish use a single `AtomicU8` single-CAS state machine; replaced or
  discarded `ExecutionConfig` instances are dropped outside the mutex.
- Revision commits and `LoopReport.requests` accounting happen at the real
  `RequestStarted` issue boundary; failed issued requests are counted while
  stale rebuilds are not; driver-internal retries remain one logical request.
- `LoopFailure::model_error()` preserves structured request-level `ModelError`
  for model/invalid-response failures; this does not indicate whole-loop auto-retry.
- Tool outputs in history are bounded by `max_tool_output_bytes`; invalid tool
  input requests fail fast without entering `WaitingForInput`.
- Internal model and tool progress queue drops are merged into `dropped_before`.
- Steer accepted means queued in the process-local queue for the next request
  boundary; cancelled/shutdown loops may discard queued steers without appending them.
- Tool catalog size is independent from per-response `ToolCall` limits.
- MSRV is 1.85; production code forbids `unsafe`; no async mutex, no
  `parking_lot`.
- Negative scope preserved: no session runtime, no session logs/manifests,
  no durable store, no whole-loop auto-retry, no ACK/replay event stream.

## Gates

`./scripts/check.sh` (main crate fmt/test all-targets all-features/clippy
`-D warnings`/rustdoc `-D warnings`, provider-gate fmt/test/clippy,
`check_docs.py`, `check_v04_architecture.py`, git diff/show checks) and
`./scripts/check-msrv.sh` must stay green. The v0.4 reset is complete with
the correctness closeout applied across five code commits (`1d1e880`, `e1c1e19`,
`1c4d385`, `e9760e0`, `8336b3c`). The current suite is 278 tests (253 in main
crate across 69 unit and 184 integration tests across 10 targets, plus 25 in
provider-gate). GitHub Actions CI run 33750189748 is the historical
code-closeout run and passed the then-current closeout suite.

## Authority

`README.md`, `tests/README.md`, `docs/README.md`, `docs/architecture.md`,
`docs/modules/README.md`, `docs/development-plan.md`, all files under
`docs/contracts/`, `docs/integration/host-boundary.md`, and
`docs/migrations/v0.3-to-v0.4.md` are the current authority set (see
`docs/README.md`). The tracked `minicore-runtime-v0.4-flex-agent-loop-reset-spec.md`
is a non-authority implementation record.