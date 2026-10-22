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
- `ReasoningPreference` is ordered as `Auto`, `Disabled`, `Low`, `Medium`,
  `High`, `XHigh`, `Max`, `Ultra`, and its serde wire values are respectively
  `auto`, `disabled`, `low`, `medium`, `high`, `xhigh`, `max`, and `ultra`.

## Request Semantics

- Public `ModelRequest::new` validates non-empty messages, per-message text
  validity (`MAX_MODEL_MESSAGE_TEXT_BYTES`), tool-exchange consistency (a
  `ToolCall` is balanced with a `ToolResult` carrying the same id), and tool
  catalog order. `ToolSpec` constructors/deserialization already validate schemas.
  The runner uses a crate-private path that reuses the frozen `ToolSet` catalog
  without revalidating it; loop start/update checks catalog budgets. Host-produced prompt
  messages are still validated on every request; failures take the `Prompt` path.
- `ModelCallContext { loop_id, request_index, cancellation, deadline }` is
  per request; the driver uses it for cancel/timeout/panic isolation.
- One `ModelRequest` maps to one streamed response (`ToolCall`s + finish
  reason). The driver performs delivery-aware retries (up to
  `MAX_MODEL_RETRY_ATTEMPTS`), stream assembly, and progress forwarding.
  Driver retries are internal to a single logical request attempt and do not
  advance the loop's request count.

## Outcomes

Runtime owns model-text normalization: CRLF and lone CR become LF; every other
control character except LF/TAB becomes one ASCII space. Normalization never
increases UTF-8 byte length. Assistant text and reasoning text/summary constructors
apply this policy to their whole payloads.

Model delta constructors preserve raw text and enforce `MAX_MODEL_EVENT_TEXT_BYTES`
on its raw byte length; direct event variants receive the same size check at the
adapter boundary. Hosts can chunk raw text at any UTF-8 character boundary up to
that limit, without reserving expansion space or avoiding CRLF boundaries.

Assembly normalizes text and reasoning independently across events. A trailing CR
is emitted immediately as LF; a leading LF in the next non-empty delta of that
same channel is dropped. Empty deltas and events in another channel leave this
state unchanged. Canonical text equals normalization of the concatenated raw
text, regardless of chunk boundaries. Forwarded progress uses those same
normalized deltas, omitting empty ones. Per-round text/reasoning budgets count
normalized bytes, not raw bytes.

Tool-argument deltas use the same character replacements before JSON assembly,
but are never forwarded as progress; arguments must still form a bounded JSON
object. Opaque replay, encrypted reasoning and signatures retain their own
validation rules.

Each model attempt receives a child cancellation token, cancelled on every
attempt ending, including successful EOF, failure, timeout and dropped futures.
The driver waits for EOF after Finish; a pending stream remains subject to its
deadline, and any event after Finish is invalid. EOF before any event is
`IncompleteResponse` with `Unknown` delivery; EOF after an event but without
Finish has `Started` delivery. Neither is retryable.

If cancellation or a timeout interrupts retry backoff, the driver retains the
previous model error (kind, diagnostic, delivery and retry hint) as the cause.
The runtime still honors loop cancellation and loop-deadline provenance; a
model-only timeout during backoff reports `Failed(Model)` with that prior cause.

- `ModelFinishReason::Stop` with no tool calls ends the loop (subject to the
  final-seal decision). `Length` / `ContentFiltered` / `Refused` map to the
  corresponding failure kinds; `Unknown`/`ToolCalls` mislabels are
  `InvalidModelResponse`.
- Streaming: the driver yields after progress and the runner drains ready
  progress before polling the driver again, so back-to-back deltas do not
  starve forwarding. `TextDelta` and `ReasoningDelta` remain best-effort
  events; a full host event queue may still drop them and they carry no correctness
  weight. Tool-argument deltas are assembled
  privately and are never forwarded as progress.
- Model timeouts, retry exhaustion, start/poll panics, and malformed responses surface
  as `Failed(Model)` or `Failed(InvalidModelResponse)` (or the specific
  mapping in the loop contract), never as a hang. In both model and
  invalid-response failure paths, the runtime preserves the structured
  `ModelError` accessible via `LoopFailure::model_error()`. A retry hint in
  `ModelError` describes per-request delivery recoverability and must not be
  interpreted as an instruction for the runtime to automatically retry the
  entire loop.

## Budgets

Request-level budgets come from `LoopLimits` (max tool calls per response,
max tool name/schema/arguments bytes, max model text/reasoning bytes) and
are enforced by the driver and the assembler. Crucially,
`max_tool_calls_per_response` limits only the number of tool calls permitted
in a single model response; it does not restrict the tool catalog size in
`ModelRequest::tools` (`max_tool_schema_bytes` is a per-tool bound, not a total
catalog bound). For N registered tools, compact schema payloads total at most
N × `max_tool_schema_bytes` (at most 64 KiB per tool); the runtime does not cap N,
so hosts own catalog-count limits. `LoopOptions::validate` fail-fast enforces upper
bounds on model timeout (<= 24h) and retry base delay (<= 30s) aligned with
`ModelDriver` constants. The per-message text ceiling
is the shared crate-private `MAX_MODEL_MESSAGE_TEXT_BYTES` constant used by
both `ModelMessage` validation and default prompt projection.

## Opaque provider replay (v0.6.1)

`ProviderReplay::new(format, payload)` validates a provider-owned JSON envelope:
format is 1–128 symbolic ASCII bytes, compact serialized envelope at most 256 KiB,
payload depth at most 32 (root zero), and at most 16,384 nodes. Deserialization
runs the same validation. Runtime never interprets or truncates provider content.
`byte_len()` includes all envelope keys and punctuation, and history accounting
includes these bytes within the host-history byte limit (2 MiB by default).

Adapters emit at most one `ModelEvent::ProviderReplay { replay }` before `Finish`.
Duplicate replay, post-Finish events, missing Finish, cancellation, and incomplete
streams retain normal assembly/settlement failure semantics. Metadata is never
forwarded as progress or a display event. Only successfully settled responses enter
history. The runner and `DefaultPromptProvider` retain it verbatim through
`ModelResponse`, `AssistantHistory`, and `ModelMessage::AssistantWithReplay`.

Canonical parts remain the only text/tool authority. Validated replay can accompany
an empty canonical part list, while `ModelResponse::new([])` and
`ModelMessage::assistant([])` still reject empty content. This permits opaque
provider-only output without manufacturing text, tools, or new loop policies.
Provider-format meaning and cross-provider compatibility belong to host adapters.

Old assistant message content arrays and absent history/response metadata still
round-trip unchanged. Replay-bearing messages use an assistant content object
with `parts` and `provider_replay`. Debug shows only replay byte counts; host API
access and explicit serde serialization intentionally retain the full payload.
