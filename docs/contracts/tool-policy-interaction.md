# Tool, Policy, and Interaction Contract

How one tool call is decided, executed, and (when needed) stalled on the
host.

## Seam

```rust
pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> &ToolSpec;
    fn execute<'a>(&'a self, invocation: ToolInvocation, context: ToolContext)
        -> ToolFuture<'a>; // -> Result<ToolExecutionOutcome, ToolError>
}
```

`ToolSpec` has private fields and read-only accessors for its name, description
and JSON schema. Constructors and deserialization validate it; `ToolSet` freezes
that checked value at registration. `ExecutionConfig` checks model compatibility.
Descriptions, schemas and arguments have no application byte/node quotas; names,
JSON-object shape and parser stack-safety validation remain enforced.

The runtime executes tools sequentially in response order, pinned to the snapshot
of the request that produced the batch. Host tools must not block executor threads:
use asynchronous work or offload blocking operations, since cancellation and
deadlines cannot preempt synchronous code.

## Policy

- An optional `ToolPolicy` decides each call: allow, deny (with reason), or
  require host approval. Without a policy, tools run directly.
- Policy results are validated; an invalid decision is a `Policy` failure.
- An unavailable, panicking, or port-timed-out policy **fails closed**
  (`Denied`) rather than ending the loop. A turn deadline, by contrast, ends
  the loop.
- Denied calls still append a `ToolResultHistory` with `Denied` outcome so
  the model can continue.

## Execution Outcomes

- Tool outputs recorded into `ToolResultHistory` (`Success`, `Failed`,
  `Denied`) retain their full validated text without byte caps or truncation.
- `Completed(output)` appends a `Success` tool result and the model continues;
  a large result is not downgraded or replaced because of its size.
- An ordinary tool error, timeout, or panic appends a `Failed` terminal
  result (the model continues); only cancellation, turn-deadline, or a
  broken interaction end the batch. Failure and denial messages retain their
  complete text too.
- `ToolFinished` output byte counts match the full bytes in `ToolResultHistory`.
- `RequestInput(request)` stalls the call for a host interaction (see
  below).

## Interactions

- `PendingInteraction`/`InteractionKind` describe what is being asked; the
  loop publishes `WaitingForInput` state and an `InteractionRequested` event.
- `ToolInputRequest` enforces invariant validation (`validate(&self)`)
  shared across its constructors, deserialization, and the port runner
  boundary. If a tool returns `RequestInput(request)` with a malformed
  request (empty prompt, empty SingleChoice choices, or forbidden controls), the runtime rejects it as an ordinary
  tool failure (`Failed`) without generating an `InteractionId`, emitting
  `InteractionRequested`, or entering `WaitingForInput`.
- The host resolves via `LoopHandle::answer` (`InteractionAnswer::Approval`
  or `InteractionAnswer::ToolInput`). The runtime enforces the interaction
  type and choice-index contract without prompt/choice/answer byte or count
  quotas; a mismatched answer returns `AnswerError::WrongInteraction`
  to the caller and leaves the interaction waiting.
- While waiting, the loop deadline still wins; cancellation also ends the
  wait without an `InteractionResolved` event. After cancellation, `answer`
  returns `NotActive`. One interaction slot is outstanding at a time.

## Progress

- Tools may emit `ToolProgress` through `ToolContext`; each emission is
  validated and forwarded through unbounded, non-awaiting progress queues.
  There is no full/drop path. Progress is observation only, never correctness;
  an absent or disconnected observer does not change tool execution.

## Snapshot Rule

A config `update` or a host input arriving mid-batch does **not** affect the
current batch: the batch uses the snapshot of the request that produced it.
`LoopInputs::take` is not called inside a batch or while an interaction waits;
the new config and inputs apply from the next model request.
Tool text, including input prompts, choices, answers, progress messages and
diagnostics, allows LF and TAB but rejects other control characters.
