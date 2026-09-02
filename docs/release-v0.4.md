# Release v0.4: Flexible Agent Loop

Final delivery report for the v0.4 "flex agent loop reset". This is the
authority record required by the v0.4 implementation spec §41. The v0.4
branch is a breaking reset: the runtime converged on one live `AgentLoop`
and dropped the v0.3 durable session-runtime surface.

## Timeline

- **Start HEAD**: `7e85eaa` (pre-branch converged runtime).
- **v0.3 saved tag**: `v0.3.0-durable-session-runtime` (the durable
  session-runtime baseline was also preserved as commit `1492865` on this
  branch).
- **v0.4 final HEAD**: this commit — `test(v0.4): close the flexible agent
  loop contract` (Phase 8). Its SHA lands when the parent push is recorded.
- **Branch**: `refactor/v0.4-flex-agent-loop`.

## Commit List

| Phase | Subject | SHA / State |
| --- | --- | --- |
| baseline | `chore: preserve the durable v0.3 runtime baseline` | `1492865` |
| 1 | `feat(v0.4): introduce loop history and execution configuration` | `4c165bc` |
| 2 | `feat(v0.4): run one agent loop without session ownership` | `f4d79c0` |
| 3 | `feat(v0.4): apply execution updates at request boundaries` | `95821fa` |
| 4 | `feat(v0.4): steer active loops at request boundaries` | `4cef1d1` |
| 5 | `refactor(v0.4): remove session storage and durable conversation ownership` | `d717944` |
| 6 | `refactor(v0.4): replace context and durable compaction with prompt providers` | `20e536a` |
| 7 | `refactor(v0.4): converge modules around the agent loop boundary` | `ca93c58` |
| 8 | `test(v0.4): close the flexible agent loop contract` | this commit (final HEAD) |

## Deleted

- **Code**: `src/session`, `src/storage`, `src/conversation`, `src/agent`,
  `src/compaction`, `src/context`, the old prompt/context pipeline,
  `src/config/`, `src/bindings.rs`, `src/error/operations.rs`, and orphan
  `src/config.rs`; `examples/session_runtime_lifecycle.rs`.
- **Tests**: ~35 session/durable/old-API test files and `tests/support`.
- **Docs**: v0.3 architecture authority (the session-runtime-lifecycle/
  session-state/session-log/conversation/extensions contracts), the older
  v0.2 migration guide,
  the v0.3 ADRs 0200/0203, `docs/acceptance-v0.3.md`,
  `docs/release-v0.3.md`, and the root v0.3 implementation spec.
- **Gates**: `check_v03_architecture*.py`, `acceptance_v03.json`,
  `generate_acceptance_v03.py`.

None of the deleted history was re-archived: the `v0.3.0-durable-session-runtime`
tag and `docs/archive/` hold every v0.3-era artifact.

## Added

- **Code**: `src/prompt.rs` (`PromptProvider` seam + `DefaultPromptProvider`),
  `src/execution.rs` (`ExecutionConfig`, `ConfigRevision`, `UserInput`),
  `src/history.rs` (`HistoryItem`/`HistoryView`), runner slices
  `src/agent_loop/runner/{model,tools}.rs`, and the control-layer unit tests.
- **Tests**: `tests/p1_v04_loop_dtos.rs`, `tests/p2_agent_loop.rs`,
  `tests/p3_agent_loop_closeout.rs`, `tests/README.md`.
- **Examples**: `examples/agent_loop.rs`, `examples/memory_agent.rs`.
- **Docs**: `docs/migrations/v0.3-to-v0.4.md`, `docs/adr/0300-v0.4-agent-loop-reset.md`,
  `docs/contracts/{agent-loop,history,prompt}.md`, `docs/acceptance-v0.4.md`,
  and this report.
- **Gates**: `scripts/check_v04_architecture.py`, `scripts/check-architecture.sh`,
  `scripts/acceptance_v04.json`, `scripts/check_acceptance.py`.

## Public API Changes

- **Added (root)**: `AgentLoop`, `LoopHandle`, `LoopRequest`, `LoopOptions`,
  `LoopReport`, `LoopState`/`LoopStatus`, `LoopEvent(Stream/Envelope)`,
  `UpdateError`/`SteerError`/`LoopStartError`/`LoopWaitError`/`LoopJoinError`,
  `CancelReason`, `LoopOutcome(Summary)`, `LoopFailure(Kind)`,
  `ExecutionConfig`, `ConfigRevision`, `UserInput`, `HistoryItem`/`HistoryView`
  and the per-item structs, `InteractionAnswer`/`Kind`/`PendingInteraction`,
  `LoopLimits`, `BoundedText`.
- **Deleted**: every session-era public type (session runtime/state/log,
  manifest, conversation, durable store, agent definitions, bindings), the
  old adapter surface, and all v0.3 aliases. No compatibility layer remains.

## Model / Tool API Changes

- `Model` and `Tool` seams are retained: `Model::descriptor`/`start`
  (per-request `ModelRequest` + `ModelCallContext`), and `Tool::spec`/
  `execute` with `ToolExecutionOutcome`. `ToolSet`, `ToolSpec`, `ToolPolicy`,
  and interactions keep their contracts (see
  [`docs/contracts/model.md`](contracts/model.md) and
  [`docs/contracts/tool-policy-interaction.md`](contracts/tool-policy-interaction.md)).
- The delivery-aware `ModelDriver` still retries only delivery-safe failures
  (`MAX_MODEL_RETRY_ATTEMPTS`, driven per-loop by `LoopOptions`).
- `PromptProvider` is the new context/compaction boundary; the built-in
  `DefaultPromptProvider` projects host history strictly (see
  [`docs/contracts/prompt.md`](contracts/prompt.md)).

## History Wire

`HistoryItem` is a typed, serde-round-trippable vocabulary (`User` /
`Assistant` / `ToolResult` / `Summary` with typed tags and per-item
validation). The host owns history: it passes an `Arc<[HistoryItem]>` into
`LoopRequest` and receives the in-memory delta in `LoopReport::appended`.
Nothing in the runtime persists or merges history (see
[`docs/contracts/history.md`](contracts/history.md) and
[`tests/p1_v04_loop_dtos.rs`](../tests/p1_v04_loop_dtos.rs)).

## Control Semantics

- **update** — full atomic `ExecutionConfig` replacement; committed revision
  only at a true model-request issue boundary; takes effect at the next
  request; never keeps a final alive; latest wins.
- **steer** — appends `Steering` user items at the next request boundary;
  bounded queue; rejected while waiting for input; the only thing that can
  extend a final.
- **interaction** — one pending slot; `answer` validates id and kind;
  loop deadline and cancel still win while waiting.
- **event** — best-effort bounded single-consumer stream; `try_send` only;
  `dropped_before` counts loss; events are never authoritative.
- **cancel/final** — one `AtomicU8` CAS (`mark_cancel`/`finish_once`) plus a
  short `std::sync::Mutex` shared with update/steer/begin-final;
  exactly-once seal; `wait`/`join` return the same `Arc<LoopReport>` for the
  lifetime of the completion channel.

## Size

Using raw line counts (`wc -l`, same口径 as the recorded baseline):

- Production + inline tests under `src/`: **32,072 → 10,150** raw lines
  (production-only ≈ 7,789 of those).
- `tests/`: **13,548 → 7,703** raw lines.
- `src/*.rs` files: 174+ → 44; production files 83+ → 34.

## Test Results (this commit, remote Linux)

Full gate chain green under `/root/minicore-runtime-v04-build/phase8`:
`./scripts/check.sh`, `./scripts/check-msrv.sh`, `cargo metadata --locked`,
plus a live run of both examples. **203 tests pass across 13 test targets**
(unit tests in `src/` plus the integration suites in `tests/`), all clippy
`-D warnings` / rustdoc `-D warnings` / fmt checks pass. The new close-out
suite adds 14 deterministic integration tests
(`tests/p3_agent_loop_closeout.rs`) and 5 control-layer unit tests
(`src/agent_loop/control.rs`) with zero sleeps.

## CI Status

`.github/workflows/ci.yml` configures Linux (stable + Rust 1.85, full gate)
plus native macOS and Windows `cargo test` jobs. A pre-completion run against
this branch's work passed all four jobs — [run 33676282786](https://github.com/zqcli/minicore-runtime/actions/runs/33676282786),
macOS / Windows / Rust stable / Rust 1.85 all success, running SHA
`019eff1` (`019eff172fc5b66cfeef5acec78f0c62fec19867`). That run executed the
full code and test surface.

The final complete amendment changes only acceptance/release metadata (the
manifest phase and V4-063 row); it does not touch runtime or test code. The
parent will re-run the same workflow against the final SHA, and the precise
terminal run is reported in the final delivery answer rather than assumed
here.

## Acceptance

[`docs/acceptance-v0.4.md`](acceptance-v0.4.md) maps V4-001..V4-070 to
concrete tests, gates, docs, or examples: **70 rows, all Passed**. Every row
is validated by `scripts/check_acceptance.py` (ids continuous, no dup,
evidence paths and test functions with `#[test]`/`#[tokio::test]` attributes
resolve, markdown in sync, and in the `complete` phase every row must be
`Passed`).

## Known Limits

- Old session logs are **not migrated**: `SessionLog`/`SessionManifest`/
  conversation JSONL records are gone and are not re-created; hosts that
  need a durable transcript own the write path.
- `minicore-agent` is **not yet migrated** to the v0.4 runtime surface.
- Tool side effects and host logs are **not atomic**: events are
  best-effort, so a tool can run with side effects even when the host never
  logs the corresponding event.
- A current loop is **not recoverable**: no open/load/resume; a crashed or
  cancelled loop ends and the host starts a new one from its own history.