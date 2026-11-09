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
2. A-step: pull the latest config candidate and call `LoopInputs::take`
   (the first request included); apply both to the working state. Taken
   inputs join the working history as `Steering` user items, in order.
3. Publish `Starting` once for this preparation attempt, then prepare under
   cancel/timeout/panic isolation. Stale rebuilds and requests after tool
   batches each have their own preparation state. Empty prompts fail as
   `Prompt` regardless of provider; prompt size is otherwise unbounded.
4. F-step: take again. If a newer config or a non-empty `take` landed while
   preparing, discard the stale prompt and rebuild; `request_index` does not
   advance and no model request is issued for the stale snapshot.
5. Issue boundary: build the `ModelRequest`, commit the candidate revision,
   stamp `RequestStarted` with it, increment the `requests` count and record
   `last_issued_request_index`, then run the model. If the issued model
   request fails, it is counted in `LoopReport.requests` and the final
   `LoopState.request_index` matches the last issued `RequestStarted`.
   Driver-internal retries execute within that single logical request; the
   runtime does not perform automatic whole-loop retries.
6. On a response with tool calls, execute the tool batch pinned to the
   snapshot of the producing request; then loop back.
7. On a response without tool calls, close accepting and end. The host's
   inputs are not consulted: a no-tool answer always ends the loop.

## Configuration Updates

- `LoopHandle::update(ExecutionConfig) -> Result<ConfigRevision, UpdateError>`.
  Accepted while the loop is active and not finalizing (`NotActive` after cancellation or sealing), and
  invalid configs are rejected without effect.
- An update takes effect at the **next model request**; the revision is
  committed only at the true issue boundary. A `Prompt`/timeout/cancel in
  between keeps the last issued revision.
- A pending config update **does not** keep the loop alive: a final response
  closes accepting and seals even if an update is queued.
- Displaced configurations (from a replacing `update` or discarded during
  `begin_final`/`finish_once`) are dropped outside the `control` mutex,
  ensuring external capabilities and providers do not execute drop logic
  while holding the lock.

## Inputs

- `AgentLoop::start(request, options, inputs)` takes an `Arc<dyn LoopInputs>`
  implemented by the host. `LoopInputs::take(&self) -> Vec<UserInput>` returns
  the inputs to append before the next model request, in order, and hands them
  over for good. The host owns the queue and validates each input (non-empty,
  within `max_user_input_bytes`); the Runtime trusts `take`.
- `take` runs synchronously on the runner task with no Runtime lock held. It
  must not block, await or do IO. The loop calls it at the A-step and the
  F-step only: never during a model call, a tool batch or an interaction wait.
- Each taken input becomes its own `Steering` user item, in order, and is in
  `LoopReport::appended` for every outcome, cancellation included. Inputs the
  loop never took stay with the host.
- A response without tool calls always ends the loop; `take` is not called
  after it. Cancellation still decides the outcome when it lands while `take`
  returns inputs: the loop ends `Cancelled` and keeps the handed-over inputs
  in the report.
- `take` carries no request identity: it can precede the handle, observe the
  previous request's `RunningTools` state, or repeat during prompt rebuilds.
  A host that records input boundaries observes `PromptRequest::request_index`
  and `ModelCallContext::request_index` through wrappers on every config,
  including replacements passed to `LoopHandle::update`. At `take`, a newer
  prepared index wins; otherwise use the index after the last model call, or
  zero before the first preparation. The Runtime guarantees these call sites;
  the host owns the association.
- This candidate index does not prove issuance. Use `LoopReport.requests` and
  `RequestStarted` for that: cancellation or preparation failure can leave
  taken inputs in `appended` without issuing their request.

## Complete Record Boundaries

An optional `LoopOptions.history` receiver acknowledges each new history range
before tools or the next model request run. The existing start signature and
input handoff do not change. See [incremental handoff](history.md#incremental-handoff)
for offsets, cancellation-safe saving and failure ownership.

## Final Report

- `LoopReport` carries `outcome`, `appended` (only in-memory items this loop
  produced), `usage`, `requests`, `tool_rounds`, and the final
  `ConfigRevision`. `tool_rounds` is a `u64` count of entered tool batches,
  not a saturated `u16` budget counter. `LoopOptions.max_tool_rounds` remains
  `u16`: zero (the default) disables that optional limit; positive values
  retain the existing finite-budget validation.
- `appended` is the loop's history delta. The host decides whether and where
  to persist it and the report's outcome and counters. Nothing is durable in
  the runtime.
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

`update`, `answer`, and the final seal linearize on one short mutex;
cancel/finish use a single-CAS `AtomicU8` state machine. All event
emissions are best-effort `try_send`s and never block the loop. Waiting for
input or events never affects loop correctness.
Usage totals sum each reported field independently; a field is `None` only if
no request reported it. Missing usage does not erase known counts.
