# SessionState Contract

`SessionState` is the lightweight authoritative process-local view of one loaded Session. It is published through Tokio watch; it is not durable storage, a replay cursor, or a snapshot protocol.

Source: [`src/session/state.rs`](../../src/session/state.rs) and [`src/session/handle.rs`](../../src/session/handle.rs). Evidence: [`session_state_event_contract.rs`](../../tests/session_state_event_contract.rs), `initial_state_rehydrates_head_terminal_and_final_handle_identity` in [`src/session/actor/tests.rs`](../../src/session/actor/tests.rs), and the forced-loss/restart cases in [`session_runtime_restart_event_evidence.rs`](../../tests/session_runtime_restart_event_evidence.rs).

## Exact Fields

`SessionState` contains exactly:

- `session_id: SessionId`;
- `instance_id: SessionInstanceId`;
- `status: SessionStatus`;
- `health: SessionHealth`;
- `active_turn: Option<TurnId>`;
- `pending_interaction: Option<PendingInteraction>`;
- `conversation_seq: ConversationSeq`;
- `last_terminal: Option<TurnOutcome>`.

`conversation_seq` is the confirmed durable head known to Core. `last_terminal` is the latest confirmed durable terminal outcome, not the latest event delivered to a consumer.

## Four States

`SessionStatus` has exactly four variants:

| Status | Active Turn | Pending Interaction | Meaning |
| --- | --- | --- | --- |
| `Idle` | none | none | Ready to admit a Turn when healthy. |
| `Running` | exactly one | none | The active Turn is executing or settling. |
| `WaitingForInput` | exactly one | exactly one for that Turn | A typed approval or ToolInput answer is required. |
| `Closing` | optional | none | Root shutdown has started; no new work is admitted. |

`Closing` may retain an active Turn while cancellation and durable settlement complete. It may not retain a public pending interaction.

A state is invalid if its active Turn is already recorded as `last_terminal`. Waiting interaction identity must match the active Turn. The validator rejects every illegal combination rather than normalizing it.

## Health

`SessionHealth` is exactly:

- `Healthy`;
- `Degraded { diagnostic }`.

Ordinary Model, Context, policy, or Tool failures settle a Turn and do not degrade the Session. Degraded health records a durability or invariant problem that makes further submit unsafe, including an unknown append outcome or an active critical append failure. A degraded Session rejects submit with the retained redacted diagnostic.

Health and status are distinct. A Session may be `Closing` while degraded, and a durability failure does not invent a terminal entry.

## Watch Semantics

`SessionHandle::state()` clones the current watch value. `watch_state()` returns another receiver for the same authoritative state channel. The initial value exists before `SessionRuntime::create` or `load` returns ready, so callers never need an event to discover initial identity, health, status, or confirmed head.

The actor is the only state writer. For public transitions it updates state before best-effort event delivery. A dropped `InteractionRequested`, `HealthChanged`, or `TurnFinished` event therefore does not erase the authoritative current state or durable result.

Watch is level-triggered current truth, not a lossless transition history. Consumers that need durable history use `SessionHandle::transcript`; consumers that need exact Turn completion use `TurnHandle::wait`.
