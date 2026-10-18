# AGENTS.md

Guidance for coding agents working in this repository. This file is
non-authority guidance: the current source and the pages indexed by
[docs/README.md](docs/README.md) define behavior. Humans should start with
[README.md](README.md).

## What This Project Is

`minicore-runtime` is a Rust library that runs one live agent loop: model
requests and tool batches, streaming, cancellation, interactions, steering,
and request-boundary config updates. It deliberately has no sessions,
persistence, transcripts, providers, workspaces, or compaction scheduler. Those
belong to hosts such as
[minicore-agent](https://github.com/zqcli/minicore-agent), which pins this
crate by Git revision. Do not add host concerns here. The architecture gate
rejects session-era types and provider, workspace, or storage dependencies.

## Repository Map

| Path | Responsibility |
|---|---|
| `src/agent_loop/` | `AgentLoop` and `LoopHandle`. `control.rs` holds the linearized update/steer/seal state, `event.rs` the best-effort stream, and `runner.rs` with `runner/{model,tools}.rs` the loop itself |
| `src/model/` | The `Model` seam, request/message/response types, provider replay. `driver.rs` and `driver/` handle retries, deadlines, and stream assembly |
| `src/tools/` | The `Tool` and `ToolPolicy` seams, `ToolSpec`, `ToolSet`, interactions, progress |
| `src/execution.rs`, `src/history.rs`, `src/prompt.rs` | `ExecutionConfig`/`UserInput`, `HistoryItem`, the `PromptProvider` seam |
| `src/limits.rs`, `src/value.rs`, `src/error.rs`, `src/port_call.rs` | Bounds, bounded values, diagnostics, timeout/panic-isolated port calls |
| `tests/*.rs`, `tests/support/` | Contract and integration targets with shared builders |
| `examples/` | `agent_loop.rs` and `memory_agent.rs`, compiled by the gates |
| `scripts/` | Quality gates; `acceptance_v04.json` is the v0.4 acceptance manifest |
| `provider-gate/` | Standalone opt-in provider evidence harness (Rust 1.88+) |

## Commands

```bash
./scripts/check.sh       # fmt, tests, Clippy -D warnings, rustdoc -D warnings, docs/architecture/acceptance gates
./scripts/check-msrv.sh  # tests on Rust 1.85.0
```

Focused runs: `cargo test --all-features --locked --test p2_agent_loop <name>` or
`cargo test --all-features --locked --lib model::driver`. Always pass
`--locked`. Run `./scripts/check-provider-gate.sh` only when `provider-gate/` or
its provider dependency changes.

## Code Rules

- Rust edition 2024, MSRV 1.85. Do not use std APIs stabilized later. `unsafe`
  is forbidden.
- The only production `tokio::spawn` is the runner task in
  `AgentLoop::start`. Do not use `tokio::sync::Mutex` or `parking_lot`. Keep
  the control `std::sync::Mutex` critical sections short, and drop replaced or
  discarded configs outside the lock.
- Events are best-effort: emit with `try_send`/`try_emit` and never branch
  correctness on delivery. The `LoopReport` is the only authoritative outcome.
- `ExecutionConfig` is immutable: no `set_*` mutators.
- Keep code lean. Validate once at construction or deserialization, and do not
  re-check invariants the types already guarantee. Add a dependency only with a
  clear need.
- Every public type change is an API decision. Record it in the README
  compatibility notes and `docs/release-v0.7.md` (or the current release
  notes), and update the affected page under `docs/contracts/`.

## Docs And Acceptance Gates

- Every non-archive Markdown file must be listed in `scripts/check_docs.py`,
  in `AUTHORITY` or `NON_AUTHORITY`. Adding, renaming, or deleting a page means
  editing that list and `docs/README.md`.
- All relative Markdown links and `#fragment` anchors are validated. Files
  under `docs/archive/` are exempt.
- `scripts/acceptance_v04.json` cites tests as `tests/<file>.rs::<test>`. Do not
  rename those files or functions. `docs/acceptance-v0.4.md` is generated: run
  `python3 scripts/check_acceptance.py --write` after editing the manifest.
- Keep `tests/README.md` test counts accurate when you add or remove tests.

## Tests

- Tests are deterministic. Synchronize with `Notify`, `oneshot`, `watch`, or
  `start_paused` with `advance`, not with sleeps. Bound any concurrency or
  stress test with an explicit timeout.
- Reuse the scripted model, tool, and policy builders in `tests/support/`.
  Driver unit tests live under `src/model/driver/tests/`.
- Use fake adapters only. Real providers belong to `provider-gate/`.

## Commits

- Subject format: `type(scope): summary`, for example
  `fix(loop): reject steer after cancellation`. The summary is lowercase and
  imperative. Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`,
  `build`, `ci`, `chore`. The scope is the module or area.
- One logical change per commit, with its tests and docs. Stage explicit paths.
  `scripts/check.sh` also runs `git diff --check`.
