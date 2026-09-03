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
  `Arc<LoopReport>`; a second join or `wait` also returns the same report as
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
- `handle.steer` / `handle.update` / `handle.cancel` are callable from any
  thread. They either linearize against the runner's mutex or fail fast with
  `NotActive` / `QueueFull` / `WaitingForInput`.
- `update` and `steer` stage for the **next model request**; the current tool
  batch keeps the snapshot of the request that produced it. A successful call
  means the steer was accepted into this process-local queue and is applied at
  the next request boundary; if the loop is cancelled, shut down, or the
  process exits before that boundary, the queued steer may be discarded and
  will not appear in `LoopReport::appended`. Steers are bounded by
  `max_pending_steers` (`QueueFull`) and rejected on `WaitingForInput` or
  `NotActive`.
- Interactions: when the loop is waiting for user input (`WaitingForInput`),
  `handle.answer` resolves the pending interaction; loop deadline still wins
  over waiting.

## Migration Impact

Old hosts that owned sessions via the v0.3 runtime lose session
open/load/save and durable conversation. See
[`docs/migrations/v0.3-to-v0.4.md`](../migrations/v0.3-to-v0.4.md) for the
concrete upgrade path, including what is intentionally not migrated.