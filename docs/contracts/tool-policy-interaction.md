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
that checked value at registration. `ExecutionConfig` checks model compatibility,
and loop start/update applies per-tool budgets.

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

- All tool outputs recorded into `ToolResultHistory` (`Success`, `Failed`,
  `Denied`) are strictly bounded by `max_tool_output_bytes`.
- `Completed(output)` appends a `Success` tool result and the model
  continues. If completed tool output exceeds `max_tool_output_bytes`, it is
  downgraded to a `Failed` result with diagnostic explanation rather than
  corrupting history.
- An ordinary tool error, timeout, or panic appends a `Failed` terminal
  result (the model continues); only cancellation, turn-deadline, or a
  broken interaction end the batch. Bounding limits apply uniformly across
  all failure and denial textual outputs.
- `ToolFinished` event output byte counts strictly match the bounded bytes
  stored in `ToolResultHistory`.
- `RequestInput(request)` stalls the call for a host interaction (see
  below).

## Interactions

- `PendingInteraction`/`InteractionKind` describe what is being asked; the
  loop publishes `WaitingForInput` state and an `InteractionRequested` event.
- `ToolInputRequest` enforces invariant validation (`validate(&self)`)
  shared across its constructors, deserialization, and the port runner
  boundary. If a tool returns `RequestInput(request)` with a malformed
  request (empty prompt, empty SingleChoice choices, >32 choices, or
  oversized choice text), the runtime rejects it immediately as an ordinary
  tool failure (`Failed`) without generating an `InteractionId`, emitting
  `InteractionRequested`, or entering `WaitingForInput`.
- The host resolves via `LoopHandle::answer` (`InteractionAnswer::Approval`
  or `InteractionAnswer::ToolInput`). The runtime enforces the interaction
  type contract; a mismatched answer returns `AnswerError::WrongInteraction`
  to the caller and leaves the interaction waiting.
- While waiting, the loop deadline still wins; cancellation also ends the
  wait without an `InteractionResolved` event. After cancellation, `answer`
  returns `NotActive`. One interaction slot is outstanding at a time.

## Progress

- Tools may emit `ToolProgress` through `ToolContext`; each emission is
  validated and forwarded as a best-effort `ToolProgress` event. Progress is
  observation only, never correctness.

## Snapshot Rule

A config `update` or `steer` arriving mid-batch does **not** affect the
current batch: the batch uses the snapshot of the request that produced it.
New config/steers apply from the next model request.
Tool text, including input prompts, choices, answers, progress messages and
diagnostics, allows LF and TAB but rejects other control characters.
