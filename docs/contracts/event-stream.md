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
  the events you were too slow to observe.

## Non-Authoritative Guarantees

- Events can be dropped under load, so a missing `ToolStarted`/`ToolFinished`
  pair does **not** imply a tool did not execute. A tool may have side
  effects even if the host never logs the corresponding event (tool side
  effects and host logging are not atomic).
- The authoritative outcome is `LoopReport` (returned by `wait`/`join`).
  Reconcile durable state from the report and appended items, never from the
  event stream alone.
- Deltas (`OutputDelta`, `ToolProgress`) are progress signals, not
  transcripts; never assume they are complete or contiguous.