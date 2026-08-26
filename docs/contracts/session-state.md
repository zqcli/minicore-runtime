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

Ordinary Model, Context, policy, or Tool failures settle a Turn and do not degrade the Session. Degraded health records a durability or invariant problem that makes further execution unsafe:

- A known or unknown active append/critical-commit failure during turn execution; unknown outcomes additionally latch durability uncertainty.
- Storage consistency failures during transcript reading (`Conflict`, `Corrupt`, `UnknownOutcome`, page contract violation, or projection mismatch).

When a session degrades during an active turn:

- Authoritative `Degraded` health is published to the state watch channel before emitting `HealthChanged` and before replying to callers.
- The first observed failure diagnostic is preserved.
- The active turn's cancellation token is triggered.
- Pending interactions are rejected with `SuspensionError::Cancelled`.
- An `ActiveCommitFailure` is latched to suppress settlement terminal append attempts, preventing fabricated completions.
- Subsequent `submit` and `answer` commands are rejected.

Transient storage errors (`Unavailable` [retryable] or `Internal` [non-retryable]), caller errors (invalid cursor or limit), and `Closed` logs preserve `Healthy` session state when they arise from transcript operations. The same low-level `KnownFailure` class from an active append is a commit durability failure and degrades the Session.

Health and status are distinct. A Session may be `Closing` while degraded, and a durability failure does not invent a terminal entry.

## Watch Semantics

`SessionHandle::state()` clones the current watch value. `watch_state()` returns another receiver for the same authoritative state channel. The initial value exists before `SessionRuntime::create` or `load` returns ready, so callers never need an event to discover initial identity, health, status, or confirmed head.

The actor is the only state writer. Its private `ActorCoreState` is the sole owner of active Turn, health, closing/durability, last-terminal, and interaction-resolution facts; `SessionState` is derived from that core plus `ConversationLog::head()`. The watch sender is output-only and is never read for production decisions. For public transitions it updates the derived state before best-effort event delivery. A dropped `InteractionRequested`, `HealthChanged`, or `TurnFinished` event therefore does not erase the authoritative current state or durable result.

Watch is level-triggered current truth, not a lossless transition history. Consumers that need durable history use `SessionHandle::transcript`; consumers that need exact Turn completion use `TurnHandle::wait`.
