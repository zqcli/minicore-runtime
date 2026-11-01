# Host Boundary

## The Deal

The host owns everything durable and stateful; the runtime executes exactly
one loop and returns control. Concretely:

- **The host supplies** prior `HistoryItem`s (`LoopRequest::history`), the
  fresh `UserInput`, and the immutable `ExecutionConfig` (model, reasoning,
  tools, optional policy, prompt provider).
- **The runtime supplies** `LoopReport` (outcome, `appended` in-memory items,
  `usage`, `requests`, `tool_rounds`, final `ConfigRevision`) and an optional
  best-effort `LoopEventStream`.
- **The host persists** whatever it wants: the report, selected appended
  items, or transcripts it built from streamed events. None of that happens
  in the runtime.

## One-Shot Contract

- `AgentLoop` runs once. `join()` waits for the run and returns
  `Arc<LoopReport>`; every handle `wait` returns the same report as
  long as the runner has published it. If the runner task is gone before
  publishing, waiters observe `LoopWaitError::CompletionClosed`.
- Dropping the `AgentLoop` cancels the loop best-effort without blocking.

## Events Are Best Effort

- `take_events()` hands the caller the single bounded stream for this loop.
- Every emission is a `try_send`; a full or closed queue never blocks the
  runner. `LoopEventEnvelope.dropped_before` counts what you missed.
- **Events are not authoritative.** Do not reconcile your durable transcript
  against the event stream, and never assume a Missing event means a tool
  never ran: tool side effects and the on-disk log are not atomic. If the
  host needs an atomic journal, it must own that transaction itself (for
  example by only recording outcomes that the report or tool results prove).

## Concurrency Notes

- Requires a Tokio context; `AgentLoop::start` spawns the single runner task.
- `handle.update` / `handle.cancel` are callable from any thread. They either
  linearize against the runner's mutex or fail fast with `NotActive`.
- `update` stages for the **next model request**; the current tool batch keeps
  the snapshot of the request that produced it.
- Inputs: the host implements `LoopInputs` and passes it to `AgentLoop::start`.
  The loop calls `take` on the runner task before every request preparation
  and again once a prompt is prepared; it must not block, await or do IO, and
  runs with no Runtime lock held. The host validates inputs where it accepts
  them and bounds its own queue. Handing inputs over is final: they are in
  `LoopReport::appended` for every outcome. A model answer without tool
  calls always ends the loop, so a host with queued inputs starts the next
  loop itself. Hosts that want a loop-scoped view of their queue create the
  `LoopInputs` before `start` and let it hand out entries only to that loop.
- Hosts that associate inputs with request indices wrap both the prompt and
  model on every config, including updates; neither state snapshots nor take
  counts identify that boundary. Candidate indices and issued requests are
  distinct. See the [input contract](../contracts/agent-loop.md#inputs).
- After cancellation, `update` and `answer` return `NotActive` even if
  finalization has not yet run.
- Interactions: when the loop is waiting for user input (`WaitingForInput`),
  `handle.answer` resolves the pending interaction; loop deadline still wins
  over waiting.

## Migration Impact

Old hosts that owned sessions via the v0.3 runtime lose session
open/load/save and durable conversation. See
[`docs/migrations/v0.3-to-v0.4.md`](../migrations/v0.3-to-v0.4.md) for the
concrete upgrade path, including what is intentionally not migrated.
## Adapter Obligations

- Futures and streams, including host tools, must cooperate with the executor
  and must not block its threads. Offload blocking work; runtime timeouts cannot
  preempt synchronous code. Tool calls run sequentially in response order.
- Enable unwinding (`panic = "unwind"`) for panic isolation. Abort-mode panics
  terminate the process and cannot produce a report.
- Futures/streams are dropped on cancellation or timeout. Make them cancellation
  safe, and arrange cleanup of any spawned work using the supplied child token.
- Tool deadlines win ties with ready results. Side effects may already have
  happened even when the report records failure; hosts own reconciliation and
  must not assume that retrying the whole loop is safe.
