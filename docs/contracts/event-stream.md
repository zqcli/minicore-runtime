# SessionEventStream Contract

`SessionEventStream` is a bounded, single-consumer, best-effort stream for live presentation. It is intentionally non-authoritative.

Source: [`src/session/event.rs`](../../src/session/event.rs) and [`src/session/event_stream.rs`](../../src/session/event_stream.rs). Evidence: [`session_state_event_contract.rs`](../../tests/session_state_event_contract.rs), the private event-sink tests in `src/session/event_stream.rs`, forced public loss in [`session_runtime_restart_event_evidence.rs`](../../tests/session_runtime_restart_event_evidence.rs), and [`event_summary_structure_contract.rs`](../../tests/event_summary_structure_contract.rs).

## Envelope And Identity

Every item is a `SessionEventEnvelope` containing:

- `session_id`;
- `instance_id`;
- one `SessionEvent`.

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
- `TurnFinished` with the durable `TurnOutcome`;
- `EventsDropped` with a cumulative count.

Events do not expose full tool output, tool arguments, interaction answers, raw adapter errors, credentials, or host paths.

## Bounded Single Consumer

`SessionRuntime::take_events()` transfers the only receiver. A second call returns `EventStreamTakenError::AlreadyTaken`. `SessionEventStream` is not Clone and offers `recv` and `try_recv`.

Internal publication uses bounded `try_send` semantics. A slow, absent, full, or closed event consumer never blocks actor progress, Port execution, durable append, Turn completion, or shutdown.

## Dropped Semantics

When an ordinary event cannot be enqueued because the queue is full, the sink increments a saturating drop counter and returns success to the actor. Before a later ordinary event, it first attempts to enqueue `EventsDropped { count }`.

If the marker attempt is also full, the current ordinary event is dropped too and the counter increases. When space becomes available, the marker precedes the next successfully enqueued ordinary event. `EventsDropped` is reserved to the internal sink; producers cannot inject their own marker.

The count reports events lost since the previous successfully enqueued marker. It is not a sequence cursor and does not make the stream replayable.

## Ordering Guarantees

Ordering applies only to events that are successfully enqueued:

- durable User append precedes `TurnStarted`;
- WaitingForInput state is published before `InteractionRequested`;
- durable ToolResult append precedes `ToolFinished`;
- durable terminal append and authoritative state/TurnHandle completion precede `TurnFinished`;
- a pending drop marker precedes the next ordinary event.

Loss can remove any best-effort event, including interaction and terminal notifications. It cannot reorder durable Conversation entries or change state.

ToolDriver enqueues its internal started-progress signal before it sends the critical suspension request. That is not a public `ToolStarted` ordering promise: progress is lossy and lower priority, so the progress item may be dropped or processed after the critical suspension. A Host must use `SessionState::WaitingForInput` and `pending_interaction` as the actionable truth and must not wait for `ToolStarted` before answering an interaction.

## Source Of Truth

Use:

- `SessionHandle::watch_state` for current state and pending interaction;
- `TurnHandle::wait` for exact Turn completion;
- `SessionHandle::transcript` for confirmed durable history.

Never use EventStream replay, absence, or an `EventsDropped` count as a durability fact.
