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
- A drop count is attached only to a later delivered event. Losing the final
  events can therefore leave no visible gap notification before stream EOF.

## Current-State Observation

- `LoopHandle::state()` reads the latest `LoopState`; `watch_state()` subscribes
  to the same retained value independently of the event queue. A late
  subscription can already contain an actionable `pending_interaction` or
  `Finished` even when its corresponding event was lost.
- Read the initial watch value before awaiting `changed()`. Clone the value
  from `borrow_and_update()` and release the borrow before awaiting or
  answering an interaction. Watch updates coalesce; they are not a history of
  transitions or a replacement event replay stream.
- The state sender lives with the shared control held by handles. A retained
  handle keeps it open after `AgentLoop::join()`, including for subscriptions
  created after completion. Do not wait for state-channel closure to decide
  that the loop ended; use `wait`/`join` for the authoritative outcome.
- `LoopState.request_index` is the current observation, not an identity handed
  to `LoopInputs::take`. It can still describe the previous tool request at
  the next input handoff; see the [input contract](agent-loop.md#inputs).

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
