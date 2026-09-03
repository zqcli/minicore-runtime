# Agent Loop Contract

Normative contract for the one-shot `AgentLoop`. This is the master loop
contract; the focused contracts ([model](model.md), [event-stream](event-stream.md),
[cancellation](cancellation.md), [tool-policy-interaction](tool-policy-interaction.md),
[history](history.md), [prompt](prompt.md)) refine specific slices.

## Lifecycle

- An `AgentLoop` runs **exactly once**. `start` accepts a `LoopRequest`
  (host history + fresh `UserInput` + `ExecutionConfig`) and spawns a single
  runner task. There is no resume, reload, reopen, or save.
- `start` validates options and configuration up front and rejects with
  `LoopStartError` / `ExecutionConfigError` before any task runs.
- `join()` returns `Result<Arc<LoopReport>, LoopJoinError>`; `wait()` on the
  control handle returns the same `Arc<LoopReport>`. Both `wait` and `join`
  see the same published report; a runner that exits before publishing closes
  the completion channel (`LoopWaitError::CompletionClosed`).
- Dropping the `AgentLoop` owner cancels the loop best-effort without
  blocking (`CancelReason::OwnerDropped`).

## Request Loop

The runner loops:

1. Cancel / deadline pre-checks.
2. A-step: atomically pull the latest config candidate and every accepted
   steer (`take_boundary`), apply them to the working state.
3. Prompt preparation under cancel/timeout/panic isolation. Empty prompts or
   prompts over `max_prompt_messages` fail as `Prompt` regardless of
   provider.
4. F-step: if newer config/steers landed while preparing, discard the stale
   prompt and rebuild; `request_index` does not advance and no model request
   is issued for the stale snapshot.
5. Issue boundary: build the `ModelRequest`, commit the candidate revision,
   stamp `RequestStarted` with it, increment the `requests` count and record
   `last_issued_request_index`, then run the model. If the issued model
   request fails, it is counted in `LoopReport.requests` and the final
   `LoopState.request_index` matches the last issued `RequestStarted`.
   Driver-internal retries execute within that single logical request; the
   runtime does not perform automatic whole-loop retries.
6. On a response with tool calls, execute the tool batch pinned to the
   snapshot of the producing request; then loop back.
7. On a no-tool-call `Stop`, run the final-seal decision (below) and end.

## Configuration Updates

- `LoopHandle::update(ExecutionConfig) -> Result<ConfigRevision, UpdateError>`.
  Accepted while the loop is not finalizing (`NotActive` otherwise), and
  invalid configs are rejected without effect.
- An update takes effect at the **next model request**; the revision is
  committed only at the true issue boundary. A `Prompt`/timeout/cancel in
  between keeps the last issued revision.
- A pending config update **does not** keep the loop alive: a final response
  with no pending steers seals even if an update is queued.
- Displaced configurations (from a replacing `update` or discarded during
  `begin_final`/`finish_once`) are dropped outside the `control` mutex,
  ensuring external capabilities and providers do not execute drop logic
  while holding the lock.

## Steering

- `LoopHandle::steer(UserInput) -> Result<(), SteerError>`. A successful call
  means the steer was accepted into this process-local queue. It is applied at
  the next request boundary. If the loop is cancelled, shut down, or the
  process exits before that boundary, the queued steer may be discarded and
  will not appear in `LoopReport::appended`.
  Steers queue up to `max_pending_steers` (`QueueFull`), are rejected if an
  interaction is pending (`WaitingForInput`), or once the loop has finalized
  (`NotActive`), and are applied as `Steering` user items at the **next model request**.
- A final response with pending steers keeps the loop alive and advances
  `request_index`; the steers (not the pending config) are what extend the
  final.

## Final Report

- `LoopReport` carries `outcome`, `appended` (only in-memory items this loop
  produced), `usage`, `requests`, `tool_rounds`, and the final
  `ConfigRevision`.
- `appended` is the only host-worthy record; the host decides whether and
  where to persist it. Nothing is durable in the runtime.
- A failed inner port (model/prompt/policy/tool) maps to a `LoopOutcome:
  Failed(LoopFailure { kind, diagnostic })`; cancellation maps to
  `Cancelled(CancelReason)`.
- For `Model` and `InvalidModelResponse` failures, `LoopFailure::model_error()`
  preserves the original structured request-level `ModelError` (kind, delivery
  outcome, and retry hint). Non-model failures return `None`. `LoopFailure`'s
  derived `Debug` representation (via `DiagnosticSummary` and `ModelError`)
  omits the diagnostic message to avoid leaking prompt or payload details.
  Preserving `ModelError` provides diagnostic fidelity for host decisions
  and does not imply that the loop can or should be automatically retried.

## Determinism of Control

`update`, `steer`, and the final seal linearize on one short mutex;
cancel/finish use a single-CAS `AtomicU8` state machine. All event
emissions are best-effort `try_send`s and never block the loop. Waiting for
input or events never affects loop correctness.