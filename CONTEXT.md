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
  cancel/finish use a single `AtomicU8` single-CAS state machine.
- Revision commits happen only at a true issue boundary; a config update
  never extends a final; a pending config alone never keeps the loop alive.
- MSRV is 1.85; production code forbids `unsafe`; no async mutex, no
  `parking_lot`.

## Gates

`./scripts/check.sh` (main crate fmt/test all-targets all-features/clippy
`-D warnings`/rustdoc `-D warnings`, provider-gate fmt/test/clippy,
`check_docs.py`, `check_v04_architecture.py`, git diff/show checks) and
`./scripts/check-msrv.sh` must stay green. Phase 8 (test work and migration
docs close-out, acceptance/release) is the only remaining plan item.

## Authority

`README.md`, `tests/README.md`, `docs/README.md`, `docs/architecture.md`,
`docs/modules/README.md`, `docs/development-plan.md`, all files under
`docs/contracts/`, `docs/integration/host-boundary.md`, and
`docs/migrations/v0.3-to-v0.4.md` are the current authority set (see
`docs/README.md`). The tracked `minicore-runtime-v0.4-flex-agent-loop-reset-spec.md`
is a non-authority implementation record.