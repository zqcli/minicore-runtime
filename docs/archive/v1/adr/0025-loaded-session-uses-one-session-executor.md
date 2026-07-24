# ADR 0025: One SessionExecutor Owns Each Loaded Session

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

Status: Accepted
Date: 2026-07-16

## Context

A loaded Session must continue processing Steer, Cancel, Interaction resolution, FollowUp, snapshot and shutdown requests while Context construction, Model calls or Tool execution are waiting on external I/O. Waiting for an entire Agent run inside the Session state owner would stop request processing and can deadlock approval flows. Allowing Model or Tool tasks to modify Session state directly would distribute SessionWriter, projection and terminal ordering across multiple owners.

ADR 0021 solved the original responsiveness problem with a mandatory per-session actor plus a separate RunTask. Later decisions changed the surrounding architecture:

- SessionStorage now uses by-entry `SessionWriter::append(...)` and trusted projection updates;
- ToolService is Runtime-owned and each Turn pins an immutable ToolSet;
- Session execution, not Driver, owns Turn/Item/Interaction lifecycle and terminal state;
- AgentLoop is a private logic implementation that must not own I/O or storage;
- one Runtime must support multiple loaded Sessions running concurrently.

The design still needs one authoritative owner, but it no longer needs to require two execution owners or expose actor-specific terminology.

## Decision

1. Each loaded Session has exactly one `SessionExecutor` that owns its execution state, SessionWriter, committed projections, current Turn, AgentLoop, request queues and terminal processing.
2. Callers use a cloneable `SessionExecutionHandle`. External commands, Tool execution control requests and Workspace authorization revocation notifications enter one bounded FIFO `SessionRequestQueue`; callers cannot borrow or lock the Executor state.
3. Context construction, UserMessage composition, Model calls and Tool execution run as cancellable asynchronous operations. They return `OperationResult` values containing `SessionId`, `TurnId`, `execution_version` and `OperationType`.
4. Only SessionExecutor may apply an OperationResult to Session state. Results for an old Turn or execution version are ignored, except that Tool operations which may already have caused side effects must still have their outcomes confirmed and stored.
5. AgentLoop is private and contains only model/tool conversation logic. It does not call SessionWriter, PromptService, ToolService, SkillService or Runtime event publication.
6. Tool execution uses a private `ToolExecutionControl` interface to request approval and record execution start through SessionExecutor. Tool tasks do not own Session state or a second writer.
7. Every durable action follows `SessionWriter.append -> apply trusted projections -> perform dependent action`. Dependent actions include host notification, waiter completion, Tool side effect and the next Model call.
8. FollowUp uses a bounded process-local FIFO for MVP. It starts a new Turn after the current Turn is terminal and captures a new TurnExecutionContext. It is not crash-safe.
9. Progress events use a separate bounded publisher and may be merged or dropped. Durable final events are derived from committed entries and cannot be lost with progress output.
10. A MiniCoreRuntime may run multiple SessionExecutors concurrently. Each Session allows at most one Starting or Running Turn. Shared Model and Tool resources use explicit concurrency limits and canonical resource locks.
11. The implementation may use Tokio tasks, local tasks or equivalent scheduling. A private monolithic SDK adapter task is allowed only when required by Rig; it returns OperationResult and cannot own SessionWriter, projections, queues or terminal state.

## Consequences

- Session control remains responsive during Model, Tool and approval waits.
- Session mutations and Turn terminal results have one deterministic owner.
- Multi-session background execution does not require a global current Session.
- Request capacity, FollowUp capacity and progress capacity are explicit and bounded.
- Testing can drive one SessionExecutor with requests and synthetic operation results without a real provider or Tool executor.
- SessionWriter append may be awaited by SessionExecutor because it is a short operation; blocking filesystem work may be handled inside the storage implementation without creating another semantic owner.
- Public Runtime protocol remains separate and will route all operations by SessionId.

## Supersedes And Amends

- Supersedes the mandatory two-owner implementation shape in [ADR 0021](0021-session-runtime-separates-actor-control-from-run-execution.md).
- Retains ADR 0021's single authoritative owner, responsive control processing, no mutable state borrowed across external I/O, and separate progress processing principles.
- Supersedes the session-scoped Tools ownership in [ADR 0011](0011-tools-are-session-scoped-subsystem.md): ToolService is Runtime-owned and active Turn execution uses a pinned ToolSet.
- Uses the by-entry storage contract from [ADR 0024](0024-session-storage-uses-by-entry-jsonl.md).

## Rejected Alternatives

### SessionExecutor waits for the complete Agent run

This stops request processing during Model, Tool and approval waits.

### Shared `Arc<Mutex<SessionExecutor>>`

Holding the lock across I/O can deadlock; releasing it distributes state transitions and storage ordering across callers.

### Mandatory RunTask with Session ownership

This creates a second owner for AgentLoop, queues, SessionWriter or terminal state. A private SDK adapter may exist, but it only performs asynchronous work and returns a typed result.

### One Executor for the whole Runtime

This would serialize unrelated Sessions and allow one slow Session to delay all others.

### One Executor per sub-operation

This increases queue and shutdown complexity without benefit because one Session has only one active Turn.
