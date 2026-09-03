# Release v0.4: Flexible Agent Loop

Final delivery report for the v0.4 "flex agent loop reset". This is the
authority record required by the v0.4 implementation spec §41. The v0.4
branch is a breaking reset: the runtime converged on one live `AgentLoop`
and dropped the v0.3 durable session-runtime surface.

## Timeline

- **Start HEAD**: `759902f` (pre-branch converged runtime).
- **v0.3 saved tag**: `v0.3.0-durable-session-runtime` (the durable
  session-runtime baseline was also preserved as commit `d7e96c4` on this
  branch).
- **v0.4 Phase 8 base HEAD**: `3edc71d` (`3edc71d3a95f135b79b5a06b6eea0b2781c12455`).
- **v0.4 correctness closeout code HEAD**: `a4cbf60` (`a4cbf60a4be04f492488ec702c01f97ec57a1e94`).
- **v0.4 final docs/acceptance freeze**: this commit — documentation and
  acceptance evidence freeze only (no runtime, test, or workflow changes).
  Its SHA lands when the parent push is recorded.
- **Branch**: `refactor/v0.4-flex-agent-loop`.

## Commit List

| Phase | Subject | SHA / State |
| --- | --- | --- |
| baseline | `chore: preserve the durable v0.3 runtime baseline` | `d7e96c4` |
| 1 | `feat(v0.4): introduce loop history and execution configuration` | `f854370` |
| 2 | `feat(v0.4): run one agent loop without session ownership` | `153be0e` |
| 3 | `feat(v0.4): apply execution updates at request boundaries` | `71784fc` |
| 4 | `feat(v0.4): steer active loops at request boundaries` | `55b9f73` |
| 5 | `refactor(v0.4): remove session storage and durable conversation ownership` | `0b73df3` |
| 6 | `refactor(v0.4): replace context and durable compaction with prompt providers` | `d9d8e27` |
| 7 | `refactor(v0.4): converge modules around the agent loop boundary` | `d8d9016` |
| 8 | `test(v0.4): close the flexible agent loop contract` | `3edc71d` |
| closeout-1 | `fix(model): separate tool catalog size from response call limits` | `f6a9e3e` |
| closeout-2 | `fix(tools): enforce loop output limits and validate input requests` | `9caae19` |
| closeout-3 | `fix(loop): preserve model errors and count issued requests` | `b4ca0fa` |
| closeout-4 | `fix(events): account for internal progress loss` | `04c3f22` |
| closeout-5 | `refactor(control): release pending configs outside the control lock` | `a4cbf60` |
| closeout-6 | `docs(v0.4): close correctness findings and refresh acceptance evidence` | final docs commit |

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
  `LoopLimits`, `BoundedText`. `LoopFailure` exposes `model_error()` to
  preserve structured request-level `ModelError` metadata without leaking
  diagnostics via `Debug`.
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
- Tool catalog size is independent from per-response `ToolCall` limits
  (`max_tool_calls_per_response`).
- `LoopOptions::validate` and `ModelDriver` enforce consistent upper bounds
  on model timeouts (<= 24h) and retry delays (<= 30s).
- All `ToolResultHistory` textual outputs are strictly bounded by
  `max_tool_output_bytes`; oversized successes are downgraded to failures.
- `ToolInputRequest` enforces unified validation across constructors,
  deserialization, and runner boundaries; malformed requests fail immediately
  without entering `WaitingForInput`.
- The delivery-aware `ModelDriver` still retries only delivery-safe failures
  (`MAX_MODEL_RETRY_ATTEMPTS`, driven per-loop by `LoopOptions`). Driver
  retries remain internal to a single logical request attempt.
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
  request; never keeps a final alive; latest wins; replaced or discarded
  configs are dropped outside the control lock.
- **steer** — accepted into process-local queue; applies as `Steering` user
  items at the next request boundary; bounded queue; rejected while waiting
  for input; discarded without appearing in `appended` if cancelled or shut
  down before reaching the boundary; the only thing that can extend a final.
- **interaction** — one pending slot; `answer` validates id and kind;
  invalid input requests fail fast; loop deadline and cancel still win while
  waiting.
- **event** — best-effort bounded single-consumer stream; `try_send` only;
  `dropped_before` counts event queue and internal model/tool progress queue
  loss; events are never authoritative.
- **cancel/final** — one `AtomicU8` CAS (`mark_cancel`/`finish_once`) plus a
  short `std::sync::Mutex` shared with update/steer/begin-final;
  exactly-once seal; `wait`/`join` return the same `Arc<LoopReport>` for the
  lifetime of the completion channel.
- **request counting** — `LoopReport.requests` and final `request_index` count
  at the real `RequestStarted` issue boundary; failed issued requests are
  counted; driver-internal retries remain one logical request; no automatic
  whole-loop retry.

## Size

Using raw line counts (`wc -l`, same口径 as the recorded baseline):

- Production + inline tests under `src/`: **32,072 → 10,150** raw lines
  (production-only ≈ 7,789 of those).
- `tests/`: **13,548 → 7,703** raw lines.
- `src/*.rs` files: 174+ → 44; production files 83+ → 34.

## Test Results (this commit, remote Linux)

Full gate chain green under `./scripts/check.sh` and `./scripts/check-msrv.sh`.
**271 tests pass across the repository**:
- **246 tests in the main crate** across 69 unit tests in `src/` and 177
  integration tests in `tests/` across 10 targets (`p1_dto.rs` (9),
  `model_error_contract.rs` (1), `model_port_contract.rs` (7),
  `model_driver_contract.rs` (3), `p1_value.rs` (4), `p1_v04_loop_dtos.rs` (14),
  `p2_agent_loop.rs` (103), `p3_agent_loop_closeout.rs` (17),
  `tool_policy_interaction_contract.rs` (8), `tool_set_contract.rs` (11)).
- **25 tests in `provider-gate`**.

All clippy `-D warnings` / rustdoc `-D warnings` / fmt / architecture checks
pass; all tests are deterministic with no long sleeps.

## CI Status

`.github/workflows/ci.yml` configures Linux (stable + Rust 1.85, full gate)
plus native macOS and Windows `cargo test` jobs. GitHub Actions
[run 33750189748](https://github.com/zqcli/minicore-runtime/actions/runs/33750189748)
passed all four jobs — macOS, Windows, Rust stable quality gate, and Rust
1.85.0 — executing code revision `a4cbf60` (`a4cbf60a4be04f492488ec702c01f97ec57a1e94`).
That run executed the complete code and test surface with all 271 tests green.

The final documentation and acceptance freeze commit touches only
documentation and generated acceptance metadata; it does not alter runtime,
test, or workflow code.

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