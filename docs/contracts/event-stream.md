# SessionEventStream Contract

`SessionEventStream` is a bounded, single-consumer, best-effort stream for live presentation. It is intentionally non-authoritative.

Source: [`src/session/event.rs`](../../src/session/event.rs) and [`src/session/event_stream.rs`](../../src/session/event_stream.rs). Evidence: [`session_state_event_contract.rs`](../../tests/session_state_event_contract.rs), the private event-sink tests in `src/session/event_stream.rs`, forced public loss in [`session_runtime_restart_event_evidence.rs`](../../tests/session_runtime_restart_event_evidence.rs), and [`event_summary_structure_contract.rs`](../../tests/event_summary_structure_contract.rs).

## Envelope And Identity

Every item is a `SessionEventEnvelope` containing:

- `session_id`;
- `instance_id`;
- `dropped_before`, the saturating count of events lost immediately before this item;
- one `SessionEvent`.

`dropped_before` is zero when no event was lost since the previous successful delivery. It is
informational only; it is not a replay cursor or a durability guarantee.

Turn-scoped variants carry their exact `TurnId`; Tool variants also carry `ToolCallId` and safe `ToolName` identity. `InteractionRequested` carries a checked `PendingInteraction`. The instance ID prevents events from an older load from being mistaken for a newer runtime of the same durable Session.

## Variants

The event variants are exactly:

- `TurnStarted`;
- `ModelStarted`;
- `OutputDelta` for text or reasoning;
- `ModelFinished` with round usage;
- `ToolStarted`;
- `ToolProgress`;
- `ToolFinished` with outcome and content byte count only;
- `InteractionRequested`;
- `InteractionResolved` with a typed summary;
- `HealthChanged`;
- `TurnFinished` with the durable `TurnOutcome`.

Events do not expose full tool output, tool arguments, interaction answers, raw adapter errors, credentials, or host paths.

## Bounded Single Consumer

`SessionRuntime::take_events()` transfers the only receiver. A second call returns `EventStreamTakenError::AlreadyTaken`. `SessionEventStream` is not Clone and offers `recv`, `try_recv`, and the standard `futures_util::Stream` interface for `StreamExt::next()`.

Dropping the stream has no execution effect: it only closes the receiver, while the actor continues without backpressure.

Internal publication uses bounded `try_send` semantics. A slow, absent, full, or closed event consumer never blocks actor progress, Port execution, durable append, Turn completion, or shutdown.

## Dropped Semantics

For every ordinary event, the sink makes one nonblocking `try_send` attempt for that actual event,
with the current accumulated loss count in `dropped_before`. A successful send clears the counter.

When the queue is full, the current event is lost, the counter becomes the previous count plus one
with `u64::saturating_add`, and the actor still receives success. The next event that fits carries
the exact accumulated count, so a capacity-one queue cannot be starved by a separate marker. If the
receiver is closed, `try_emit` returns `Closed` and leaves the accumulated count unchanged; it does
not keep growing after the stream has gone away.

The count reports events lost since the previous successfully enqueued ordinary event. It is not a
sequence cursor and does not make the stream replayable.

## Ordering Guarantees

Ordering applies only to events that are successfully enqueued:

- durable User append precedes `TurnStarted`;
- WaitingForInput state is published before `InteractionRequested`;
- durable ToolResult append precedes `ToolFinished`;
- durable terminal append and authoritative state/TurnHandle completion precede `TurnFinished`;
- a successfully delivered event carries the loss count accumulated before it.

Loss can remove any best-effort event, including interaction and terminal notifications. It cannot reorder durable Conversation entries or change state.

ToolDriver enqueues its internal started-progress signal before it sends the critical suspension request. That is not a public `ToolStarted` ordering promise: progress is lossy and lower priority, so the progress item may be dropped or processed after the critical suspension. A Host must use `SessionState::WaitingForInput` and `pending_interaction` as the actionable truth and must not wait for `ToolStarted` before answering an interaction.

## Source Of Truth

Use:

- `SessionHandle::watch_state` for current state and pending interaction;
- `TurnHandle::wait` for exact Turn completion;
- `SessionHandle::transcript` for confirmed durable history.

Never use EventStream replay, absence, or a `dropped_before` count as a durability fact.
