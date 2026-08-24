# MiniCore Runtime v0.3 Release Note

**Status:** v0.3 release validation complete and ready for publication. Linux functional acceptance, authoritative remote Rust gates, and the complete native macOS and Windows CI matrix passed. This is a validated release candidate; no package or tag release has occurred.

## Breaking Reset

v0.3 replaces the v0.2 multi-session runtime with an embeddable single-session Agent Execution Kernel. There is no compatibility layer, deprecated alias, dual API, automatic store conversion, or retained test-only implementation bridge.

A Host now owns its Session collection, repositories, writer leases, concrete storage/model/tool/workspace capabilities, global scheduling, and shutdown-all policy. Core owns exactly one loaded Session per SessionRuntime.

See [Migrating from v0.2 to v0.3](migrations/v0.2-to-v0.3.md) and the [Host boundary](integration/host-boundary.md).

The event stream also has a direct breaking cutover: `SessionEventEnvelope` now exposes public
`dropped_before: u64`, every dropped count is attached to the next successfully delivered ordinary
event, and the `SessionEvent::EventsDropped` variant plus the internal marker-reservation error
are removed. A closed receiver returns `Closed` without increasing the accumulated count. There is
no compatibility marker or legacy event shape.

## Final Decisions

| ID | Decision | Explicitly excluded |
| --- | --- | --- |
| D-01 | Core runs exactly one loaded Session. | A Core multi-session Runtime, registry, or SessionManager. |
| D-02 | `SessionRuntime` is the unique owner. | Cloneable owners or multiple objects sharing shutdown responsibility. |
| D-03 | `SessionHandle` is the cloneable control handle. | Host access to actor channels, log ownership, task handles, or cancellation slots. |
| D-04 | `TurnHandle` represents one exact Turn. | A session-wide ambiguous cancel API as the primary control. |
| D-05 | The Host manages the Session collection. | Core list/create-metadata/delete repository APIs. |
| D-06 | Workspace authority is captured by external Tool or ContextProvider implementations. | A Core Workspace Port or `ToolContext.workspace`. |
| D-07 | Model, Tool, Policy, Context, Compaction, and SessionLog are injected through typed Ports. | Plugin managers, service locators, and dynamic-library ABIs. |
| D-08 | Bindings are immutable for one loaded Session lifetime. | Runtime hot plug/unplug or replacing the model/tool set while loaded. |
| D-09 | Core retains a lightweight `SessionState`. | Heavy snapshots and observation epoch/cursor/gap protocols. |
| D-10 | EventStream is bounded, single-consumer, and best-effort. | Multi-subscriber broadcast or event replay as the source of truth. |
| D-11 | A pending interaction exists only in current process memory. | Restoring arbitrary Tool futures or approval questions across restart. |
| D-12 | Restart restores only durable Conversation. | Continuation, background-job, or Tool-stack restoration. |
| D-13 | A remote agent is an ordinary RPC Tool. | A Core Subagent type, parent/child state machine, or Agent graph. |
| D-14 | Core provides no universal lifecycle hook. | `before_everything` or hooks that mutate private internal state. |
| D-15 | v0.3 is directly breaking. | A v0.2 dual-track API or old `Runtime` alias. |

## Public API And Lifecycle

The public owner/control surface is:

- non-Clone `SessionRuntime` and checked `SessionRuntimeOptions`;
- cloneable `SessionHandle` for bounded submit/answer/transcript commands and state watch;
- cloneable exact-Turn `TurnHandle` for cancellation and durable completion;
- process-local `SessionState`, one takeable `SessionEventStream`, and typed interactions;
- canonical durable Conversation entries and transcript pages.

Create initializes a checked v3 manifest and empty Conversation before readiness. Load validates manifest identity and immutable bindings, replays canonical pages, atomically repairs unfinished durable Turns, then returns a new SessionInstanceId.

`SessionSpec` and `SessionManifest` constructors and Serde deserialization enforce absolute structural safety bounds, while the host-configured `KernelConfig.limits` serves as the sole instance limit enforced during `SessionRuntime::create`, `SessionRuntime::load`, and `SessionBindings::validate`.

`SessionRuntime` and `TurnHandle` are `#[must_use]` types, so ignored owners/handles produce compiler warnings. Runtime Drop is cancellation-only and best-effort; `SessionRuntime::shutdown(self)` is the explicit durability barrier that cancels active work, settles where possible, closes the one SessionLog, and joins owner-tracked tasks. TurnHandle Drop does not cancel, and a Host may intentionally detach a handle while recording that decision.

`SessionEventStream` now also implements the additive standard `futures_util::Stream` interface, enabling `StreamExt::next()` and `tokio::select!` while preserving `recv`, `try_recv`, single-consumer ownership, and close semantics. Dropping the stream has no execution effect.

See the [SessionRuntime lifecycle contract](contracts/session-runtime-lifecycle.md), [state contract](contracts/session-state.md), [event contract](contracts/event-stream.md), and [cancellation contract](contracts/cancellation.md).

## Typed Ports

v0.3 supports extension only through:

- direct `model::Model`;
- `tools::Tool` values frozen in `ToolSet`;
- optional `ToolPolicy`;
- optional `ContextProvider`;
- optional `CompactionStrategy`;
- exclusive mutable `SessionLog`.

Core ships no provider registry, HTTP adapter, concrete model adapter, builtin/process/filesystem Tool, Workspace, JSONL/Store implementation, repository, plugin manager, service locator, lifecycle hook bus, or Subagent graph.

See the [Model contract](contracts/model.md), [Tool/policy/interaction contract](contracts/tool-policy-interaction.md), [SessionLog contract](contracts/session-log.md), and [extension contract](contracts/extensions.md).

## Durable Execution

Conversation contains exactly UserMessage, AssistantMessage, ToolResult, Summary, and TurnTerminal entries. The actor is the sole append authority. Runners send unsequenced drafts; ConversationLog assigns sequences/timestamps and advances confirmed memory only after an exact durable AppendReceipt.

ToolFinished and TurnFinished are best-effort events after durability. TurnHandle completion follows durable terminal settlement. Unknown/failed critical append outcomes degrade the Session and do not fabricate a terminal.

`SessionHandle::transcript` returns confirmed history without speculative entries. Transcript errors use explicit semantic classification: caller input errors and `Closed` logs leave health unaffected, transient `Unavailable` and `Internal` errors return `TranscriptUnavailable` while preserving `Healthy` state (`Unavailable` retryable), while storage consistency failures (`Conflict`, `Corrupt`, `UnknownOutcome`, page contract or projection mismatches) transition health to `Degraded`. Degraded health during an active turn cancels active work, suppresses fallback terminal appends, rejects pending interactions, and rejects subsequent `submit` and `answer` commands.

Restart restores durable Conversation only. It atomically appends cancelled results for unresolved calls followed by `CancelledByRestart`; pending approval, ToolInput, Model/Tool continuations, events, and task state are not restored.

See the [Conversation contract](contracts/conversation.md).

## Acceptance

All functional criteria in [AT-K01 through AT-K96](acceptance-v0.3.md) are **Passed on Linux**. `scripts/acceptance_v03.json` is the canonical reviewed traceability mapping; the generated Markdown and attributed evidence are checker-enforced, while the remote Rust gates—not the documentation checker—execute and validate the cited behavior.

Validation environment:

- remote Linux checkout: `/root/minicore-runtime-v03`;
- stable `rustc 1.98.0`, `cargo 1.98.0`, and `clippy 1.98.0`;
- full `scripts/check.sh` pass;
- 285 root library tests plus passing cleaned integration/provider-gate suites;
- MSRV `rustc 1.85.0` and `cargo 1.85.0` with `scripts/check-msrv.sh` passing;
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked` passing;
- authoritative architecture scanner passing with `production_files=143`;
- GitHub Actions [run 32755428283](https://github.com/zqcli/minicore-runtime/actions/runs/32755428283) passed for commit `815494dad38c34c585dfeda3c0845ccc7c1fb7d0` across all four jobs (Rust stable Clippy quality gate, Rust 1.85.0 MSRV, `macos-latest`, and `windows-latest` with MSVC), validating review fixes AT-K01 through AT-K96.

Cross-platform validation is complete across Linux, macOS, and Windows. No package or tag release has occurred; the repository is ready for publication.

## Dependency And Source Review

The final root manifest has 8 direct dependencies:

- `getrandom`;
- `serde`;
- `serde_json`;
- `thiserror`;
- `time`;
- `tokio`;
- `tokio-util`;
- `futures-util`.

After P6 cleanup, the root lock contains 37 package records. Relative to the pre-P6 lock, 39 records were removed, 0 added, and retained packages had 0 version drift. Removed capability/storage dependencies are absent.

## Code-Size Comparison

The reviewed comparison uses baseline commit `2fd7104`. “cfg(test)-excluded production LOC” is the production view used by the authoritative architecture scanner: files reached only through test-only modules are excluded, then `production_view` removes inline `cfg(test)` spans before counting. Raw lines and Rust-file counts include every `src/**/*.rs` file.

| Metric | Baseline | Current | Change |
| --- | ---: | ---: | ---: |
| cfg(test)-excluded production LOC | 15,483 | 14,251 | -1,232 |
| raw `src/**/*.rs` lines | 48,055 | 31,230 | -16,825 |
| `src` Rust files | 174 | 143 | -31 |
| files with production content | 83 | 77 | -6 |

The authoritative architecture gate separately prints `production_files=143`, meaning all physical Rust source files enumerated by that scanner. It must not be confused with the 77 files whose cfg(test)-excluded production view is nonempty. The gate also enforces canonical production paths, direct dependencies, public Port declarations, root exports, source-size limits, forbidden authority, and an all-singleton module DAG.

## Known Limitations

The following are intentional v0.3 boundaries from the final specification, not hidden incomplete Core features:

- pending Interaction recovery across restart;
- active Model/Tool continuation recovery;
- multi-observer consistency and Event replay;
- Host Session listing, idle eviction, and shutdown-all;
- cross-process writer-lease implementation;
- global model/tool scheduling;
- workspace write-conflict policy;
- concrete Store durability implementation;
- remote Agent orchestration;
- plugin ABI;
- per-Turn model override or hot model swap.

Hosts may implement these outside Core behind typed Ports and explicit product ownership. Reintroducing them into SessionRuntime requires a new architecture decision.

## Upgrade Guidance

1. Read the [migration guide](migrations/v0.2-to-v0.3.md).
2. Move multi-session collection/repository/lease ownership into the Host.
3. Implement/open concrete SessionLog and Model/Tool adapters externally.
4. Replace snapshots and broadcast assumptions with state watch, one event stream, Turn wait, and transcript.
5. Replace string/durable interactions with typed process-local answers and restart cancellation.
6. Use explicit shutdown for every loaded owner.
7. Run the complete acceptance and native CI matrix before publication.

The README contains a compile-shaped load/submit/watch/events/shutdown example using only the final public API.
