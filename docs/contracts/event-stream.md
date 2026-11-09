# Event Stream Contract

Optional observation of a live loop. **Events are not authoritative.**

## Stream Shape

- `AgentLoop::take_events()` returns the single `LoopEventStream` for this
  loop (one consumer; calling it twice errors). Dropping the stream has no
  execution effect.
- Events: `Started`, `StateChanged`, `RequestStarted` (with the committed
  `ConfigRevision`), `OutputDelta`, `ToolStarted` / `ToolProgress` /
  `ToolFinished`, `InteractionRequested` / `InteractionResolved`, `Finished`.
- `LoopEventEnvelope` contains only `event`. The public stream and internal
  model/tool progress channels are unbounded and use non-awaiting sends.
  There is no `event_capacity`, full-queue rejection, lost-on-full path or
  `dropped_before` counter. A connected slow consumer can drain queued events
  after the loop finishes without capacity-driven loss.
- The internal event sink's `try_emit` returns unit; callers cannot branch on delivery.
  An absent or disconnected consumer has no business effect. This is an
  in-memory live queue, not persistent storage or a replay stream.

## Current-State Observation

- `LoopHandle::state()` reads the latest `LoopState`; `watch_state()` subscribes
  to the same retained value independently of event consumption. A late
  subscription can already contain an actionable `pending_interaction` or
  `Finished` even when the host has not consumed any events.
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

- Lifecycle events are not necessarily paired. Calls settled without execution
  after a batch interruption have no `ToolStarted` or `ToolFinished`. An
  unavailable tool name is rejected before execution and has no lifecycle
  events. An interrupted call may have `ToolStarted` without `ToolFinished`.
- Cancelled or timed-out interactions have no `InteractionResolved`. Consumers
  must use final state/report settlement rather than wait for a matching event.
- Observation and tool side effects are not atomic. Disconnection or process
  exit can prevent a host from observing work that ran; a missing lifecycle
  event does not prove a tool did not execute.
- There is no event acknowledgement, replay cursor, persistent event store or
  background event task. Deltas are progress signals, not authoritative history.
- `LoopHistory` acknowledges ordered complete records before continuation;
  `LoopReport` from `wait`/`join` contains the authoritative outcome and full
  appended delta, plus unfinished model assembly when present. Reconcile
  durable state from those boundaries, never from observation events alone.
