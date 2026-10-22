# Current Implementation Context

The current package is v0.7.4. It retains the one-loop boundary from the
v0.4 reset. Tool-batch statistics are now `u64` end to end within Runtime;
the optional legacy `u16` round budget still defaults to zero (unlimited).
Each real prompt preparation publishes one `Starting` event. Session-owned
post-turn/manual summaries and emergency recovery remain Agent policies,
not new Runtime responsibilities. See the root README compatibility notes.

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
  typed seams and validation the loop builds on. No sessions or durable state.
- The public root re-exports only loop, execution, history, model, prompt,
  tools, interaction, ids, limits, value, and error types (see `src/lib.rs`).

## Engine Rules

- One runner task per loop; `wait`/`join` return the same `Arc<LoopReport>`.
- Completion is owned by the runner task; its drop surfaces as
  `LoopWaitError::CompletionClosed` to waiters.
- Update/steer and the final seal linearize on one short `std::sync::Mutex`;
  cancel/finish use a single `AtomicU8` single-CAS state machine; replaced or
  discarded `ExecutionConfig` instances are dropped outside the mutex. Finalization
  returns discarded configs to the runner for panic-isolated drop after publication.
- Revision commits and `LoopReport.requests` accounting happen at the real
  `RequestStarted` issue boundary; failed issued requests are counted while
  stale rebuilds are not; driver-internal retries remain one logical request.
- `LoopFailure::model_error()` preserves structured request-level `ModelError`
  for model/invalid-response failures; this does not indicate whole-loop auto-retry.
- Tool outputs in history are bounded by `max_tool_output_bytes`; invalid tool
  input requests fail fast without entering `WaitingForInput`.
- Internal model and tool progress queue drops are merged into `dropped_before`.
- Steer accepted means queued in the process-local queue for the next request
  boundary; every non-`Completed` ending discards unapplied queued steers.
- Tool catalog size is independent from per-response `ToolCall` limits.
- Built with current stable Rust, no declared minimum; production code forbids `unsafe`; no async mutex, no
  `parking_lot`.
- Negative scope preserved: no session runtime, no session logs/manifests,
  no durable store, no whole-loop auto-retry, no ACK/replay event stream.

## Gates

`./scripts/check.sh` (main crate fmt/test all-targets all-features/clippy
`-D warnings`/rustdoc `-D warnings`, `check_docs.py`, `check_v04_architecture.py`,
acceptance and git diff/show checks) must stay green.
The standalone provider evidence is opt-in through `./scripts/check-provider-gate.sh`;
run it when changing that harness or its provider dependency,
not on every runtime change. The v0.4 reset is complete with
the correctness closeout applied across five code commits (`f6a9e3e`, `9caae19`,
`b4ca0fa`, `04c3f22`, `a4cbf60`). The historical v0.4 closeout suite was 278 tests (253 in main
crate across 69 unit and 184 integration tests across 10 targets, plus 25 in
provider-gate). GitHub Actions CI run 33750189748 is the historical
code-closeout run and passed the then-current closeout suite.

## Authority

`README.md`, `tests/README.md`, `docs/README.md`, `docs/architecture.md`,
`docs/modules/README.md`, `docs/development-plan.md`, all files under
`docs/contracts/`, `docs/integration/host-boundary.md`, and
`docs/migrations/v0.3-to-v0.4.md` are the current authority set (see
`docs/README.md`). `AGENTS.md` is non-authority guidance for coding agents.
## v0.7.0 Runtime Refactor

Model output text is normalized by Runtime, empty deltas are ignored, and model
attempt child tokens are cancelled at settlement. Requests, history payloads,
and frozen tool catalogs share immutable storage. Usage sums known counts per field.
Interactions have no fabricated deadline; handles expose shutdown cancellation.
The model modules are split by responsibility, and integration tests share fixtures
without merging binaries or changing acceptance names. See
[release notes](docs/release-v0.7.md) and README compatibility notes.

## v0.6.1 Opaque Replay Transport

The existing response/history/prompt path carries bounded `ProviderReplay` metadata
without provider logic or a second state owner. It is non-display and redacted in
Debug; only host API/explicit serde expose its payload. Canonical parts remain the
execution authority. The legacy assistant tuple and old JSON are retained, with
`AssistantWithReplay` for metadata-bearing requests. See the model contract for
bounds, stream ordering, and the narrow replay-only empty-content case.
