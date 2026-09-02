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

`ToolSpec` (name, description, JSON schema) is registered into a `ToolSet`
and validated at `ExecutionConfig::new`. The runtime executes only the
tools the model asked for, in the order of the response, pinned to the
snapshot of the request that produced the batch.

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

- `Completed(output)` appends a `Success` tool result and the model
  continues.
- An ordinary tool error, timeout, or panic appends a `Failed` terminal
  result (the model continues); only cancellation, turn-deadline, or a
  broken interaction end the batch.
- `RequestInput(request)` stalls the call for a host interaction (see
  below); oversized tool output is a `Failed` terminal result.

## Interactions

- `PendingInteraction`/`InteractionKind` describe what is being asked; the
  loop publishes `WaitingForInput` state and an `InteractionRequested` event.
- The host resolves via `LoopHandle::answer` (`InteractionAnswer::Approval`
  or `InteractionAnswer::ToolInput`). The runtime enforces the interaction
  type contract; a mismatched answer is an `Internal` failure.
- While waiting, the loop deadline still wins; cancellation also ends the
  wait. One interaction slot is outstanding at a time.

## Progress

- Tools may emit `ToolProgress` through `ToolContext`; each emission is
  validated and forwarded as a best-effort `ToolProgress` event. Progress is
  observation only, never correctness.

## Snapshot Rule

A config `update` or `steer` arriving mid-batch does **not** affect the
current batch: the batch uses the snapshot of the request that produced it.
New config/steers apply from the next model request.