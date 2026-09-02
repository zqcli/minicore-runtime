# Model Contract

The runtime–model boundary around `model::Model`, `ModelRequest`, and the
delivery-aware `ModelDriver`.

## Seam

```rust
pub trait Model: Send + Sync + 'static {
    fn descriptor(&self) -> &ModelDescriptor;
    fn start<'a>(&'a self, request: ModelRequest, context: ModelCallContext)
        -> ModelStartFuture<'a>; // -> Result<ModelStream, ModelError>
}
```

- The host (or its adapter) implements `Model`; the runtime never does.
- `ModelDescriptor` (model ref, context window, supported reasoning, tool
  support) is validated at `ExecutionConfig::new`; a model that claims no
  reasoning or zero context window is rejected, and a tool set with any tool
  requires `supports_tools`.

## Request Semantics

- `ModelRequest` validates: non-empty messages, per-message text validity
  (single source: `MAX_MODEL_MESSAGE_TEXT_BYTES`), tool-exchange consistency
  (a `ToolCall` is balanced with a `ToolResult` carrying the same id), and
  a tool schema budget. Construction failure is a `Prompt`-path failure for
  the loop.
- `ModelCallContext { loop_id, request_index, cancellation, deadline }` is
  per request; the driver uses it for cancel/timeout/panic isolation.
- One `ModelRequest` maps to one streamed response (`ToolCall`s + finish
  reason). The driver performs delivery-aware retries (up to
  `MAX_MODEL_RETRY_ATTEMPTS`), stream assembly, and progress forwarding.

## Outcomes

- `ModelFinishReason::Stop` with no tool calls ends the loop (subject to the
  final-seal decision). `Length` / `ContentFiltered` / `Refused` map to the
  corresponding failure kinds; `Unknown`/`ToolCalls` mislabels are
  `InvalidModelResponse`.
- Streaming: deltas (`TextDelta`, `ReasoningDelta`) and per-tool-call
  argument deltas are forwarded as best-effort events; they carry no
  correctness weight.
- Model timeouts, retry exhaustion, panics, and malformed responses surface
  as `Failed(Model)` (or the specific mapping in the loop contract), never
  as a hang.

## Budgets

Request-level budgets come from `LoopLimits` (max tool calls per response,
max tool name/schema/arguments bytes, max model text/reasoning bytes) and
are enforced by the driver and the assembler. The per-message text ceiling
is the shared crate-private `MAX_MODEL_MESSAGE_TEXT_BYTES` constant used by
both `ModelMessage` validation and default prompt projection.