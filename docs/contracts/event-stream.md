# Event Stream Contract

Best-effort observation of a live loop. **Events are not authoritative.**

## Stream Shape

- `AgentLoop::take_events()` returns the single `LoopEventStream` for this
  loop (one consumer; calling it twice errors). Dropping the stream has no
  execution effect.
- Events: `Started`, `StateChanged`, `RequestStarted` (with the committed
  `ConfigRevision`), `OutputDelta`, `ToolStarted` / `ToolProgress` /
  `ToolFinished`, `InteractionRequested` / `InteractionResolved`,
  `Finished`.
- The stream is bounded (`event_capacity`); every emission is a `try_send`
  that never blocks the runner. `LoopEventEnvelope.dropped_before` counts
  dropped events: this includes events dropped when the public event queue
  is full, as well as internal bursts dropped by the model driver and tool
  progress queues before reaching the loop sink. Drops on closed queues
  are not counted.

## Non-Authoritative Guarantees

- Lifecycle events are not paired even when no events are dropped. Calls settled
  without execution (remaining calls synthesized after a batch interruption or a
  tool-round limit) have no `ToolStarted` or `ToolFinished`. An unavailable tool
  name in model output is rejected before execution and has no tool lifecycle
  events. An in-flight interrupted call may have `ToolStarted` without `ToolFinished`.
- Cancelled or timed-out interactions have no `InteractionResolved`. Consumers
  must use final state/report settlement rather than wait for a matching event.

- Events can be dropped under load, so a missing `ToolStarted`/`ToolFinished`
  pair does **not** imply a tool did not execute. A tool may have side
  effects even if the host never logs the corresponding event (tool side
  effects and host logging are not atomic).
- Progress queues remain strictly best-effort observation channels; there is
  no event ACK, no replay buffer, and no background event task.
- The authoritative outcome is `LoopReport` (returned by `wait`/`join`).
  Reconcile durable state from the report and appended items, never from the
  event stream alone.
- Deltas (`OutputDelta`, `ToolProgress`) are progress signals, not
  transcripts; never assume they are complete or contiguous.