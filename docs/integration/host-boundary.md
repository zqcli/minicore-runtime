# Host Boundary

## The Deal

The host owns everything durable and session-scoped; the runtime executes exactly
one loop and returns control. Concretely:

- **The host supplies** prior `HistoryItem`s (`LoopRequest::history`), the
  fresh `UserInput`, and the immutable `ExecutionConfig` (model, reasoning,
  tools, optional policy, prompt provider).
- **The runtime supplies** `LoopReport` (outcome, `appended` in-memory items,
  optional model `partial`, optional `history_failure`, `usage`, `requests`,
  per-request `model_attempts` and `model_retry_wait_elapsed`,
  `tool_rounds`, final `ConfigRevision`), current
  `LoopState` through `state`/`watch_state`, and an optional best-effort
  `LoopEventStream`.
- **The host persists** complete records from the optional history callback
  and final report, plus unfinished model output from `report.partial`.
  Streamed events are observation only, never the source of authoritative
  history or partial output. Runtime performs no persistence itself.

## One-Shot Contract

- `AgentLoop` runs once. `join()` waits for the run and returns
  `Arc<LoopReport>`; every handle `wait` returns the same report as
  long as the runner has published it. If the runner task is gone before
  publishing, waiters observe `LoopWaitError::CompletionClosed`.
- Dropping the `AgentLoop` cancels the loop best-effort without blocking.

## Events Are Best Effort

- `take_events()` hands the caller the single unbounded stream for this loop.
- Public events and internal model/tool progress use non-awaiting sends. There
  is no capacity-driven loss, drop counter or persistent/replay event store.
  A disconnected observer never blocks the runner or changes its outcome.
- **Events are not authoritative.** Do not reconcile your durable transcript
  against the event stream, and never assume a Missing event means a tool
  never ran: tool side effects and the on-disk log are not atomic. If the
  host needs an atomic journal, it must own that transaction itself (for
  example by only recording outcomes that the report or tool results prove).
- Control observation comes from `state`/`watch_state`, independently of event
  delivery. Read the initial watch value, handle coalesced updates, and use
  `wait`/`join` rather than state-channel closure for completion. See the
  [current-state contract](../contracts/event-stream.md#current-state-observation).

## Model Retry Observation

Read `LoopState.model_retry` from `state` or the initial/current value of
`watch_state`. Keep the original `loop_id` and zero-based `request_index` when
projecting it into host execution state. The driver supplies the exact preceding
`ModelError`; the host can map it into its own error vocabulary without parsing
messages. Format `Waiting.retry_at: SystemTime` as UTC/RFC3339 for display.
Do not reschedule using that wall time: the driver's `retry_deadline` is the
monotonic execution target.

`Waiting` describes a planned attempt, while `Requesting` means a real model
start. Use `model_attempts`, not the retry attempt number or received event count,
for actual invocation accounting. It includes the initial call, and the final
report retains every request's count at `report.model_attempts[request_index]`.
Watch updates can coalesce; the report does not require seeing intermediate
states. An unissued preparation contributes no report entry.

Use `report.model_retry_wait_elapsed[request_index]` for actual accumulated
backoff time, not a sum of configured delays or time between received states.
Runtime measures each wait with its monotonic clock through settlement, so
cancellation includes the elapsed partial wait and delayed re-poll includes
scheduler delay. Time in prompt preparation, model execution, tools or saving
is excluded. Zero means no elapsed wait; a request with zero actual starts has
zero wait. Actual recovery attempts are the actual count minus the initial
call, using `saturating_sub(1)` for the no-start case. A positive duration alone
does not prove a recovery attempt started. Neither metric requires an event
consumer or a state subscription.

Clearing is a normal state update before history handoff, including failure and
cancellation. Apply `None` to remove a previous retry indicator even when the
host is still saving the request's output. No public retry event or separate
subscription is needed. See [retry state](../contracts/model.md#retry-state).

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
  them and owns its queue. Runtime imposes no input byte or queue-count quota.
  Handing inputs over is final: they are in
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
## Incremental History and Partial Output

Hosts may install `LoopOptions.history` without changing the start/input API.
Use the original loop ID and appended offset to accept each complete range
exactly once, then save outside owner-state locks before acknowledging it. Keep
accepted and saved offsets separate: an error does not unaccept a range, and
join still returns the entire delta. The callback future must retain ownership
until saving settles, including after cancellation or caller wait abandonment.
Inspect `report.history_failure` independently of `report.outcome`, preserving
business faults separately from persistence faults. Even `Completed` with a
history failure is blocked: it cannot authorize continuation or public completion
until the applicable saving is confirmed. Runtime does not discard or time out
owned saving, so the host owns its eventual successful or failed termination.
See [history handoff](../contracts/history.md#incremental-handoff).

Save `LoopReport.partial` separately from complete history when present. It is
assembled authoritative content, independent of subscriber availability, and
is never a complete assistant message or executable tool request. Preserve the
original loop/request association, and do not append it as a second response
to previously observed output. Complete responses remain complete when tools
later fail or cancel. See [unfinished model output](../contracts/model.md#unfinished-model-output).

## Adapter Obligations

- Futures and streams, including host tools, must cooperate with the executor
  and must not block its threads. Offload blocking work; runtime timeouts cannot
  preempt synchronous code. Tool calls run sequentially in response order.
- Enable unwinding (`panic = "unwind"`) for panic isolation. Abort-mode panics
  terminate the process and cannot produce a report.
- Model, prompt, policy and tool futures/streams are dropped on cancellation
  or timeout. Make them cancellation safe and arrange cleanup of spawned work
  using the supplied child token. History acknowledgements are different:
  Runtime awaits owned saving without dropping the callback on cancellation
  or timeout.
- Tool deadlines win ties with ready results. Side effects may already have
  happened even when the report records failure; hosts own reconciliation and
  must not assume that retrying the whole loop is safe.
