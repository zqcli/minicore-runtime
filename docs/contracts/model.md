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
  validity, tool-exchange consistency (a
  `ToolCall` is balanced with a `ToolResult` carrying the same id), and tool
  catalog order. `ToolSpec` constructors/deserialization already validate schemas.
  The runner uses a crate-private path that reuses the frozen `ToolSet` catalog
  without revalidating it. There is no catalog-count, schema-byte or message-byte
  quota. Host-produced prompt messages are still validated on every request;
  failures take the `Prompt` path.
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

Model delta constructors are infallible and preserve raw text without a byte
quota. Direct event variants have the same semantics. Hosts can chunk raw text
at any UTF-8 character boundary without reserving normalization space or
avoiding CRLF boundaries.

Assembly normalizes text and reasoning independently across events. A trailing CR
is emitted immediately as LF; a leading LF in the next non-empty delta of that
same channel is dropped. Empty deltas and events in another channel leave this
state unchanged. Canonical text equals normalization of the concatenated raw
text, regardless of chunk boundaries. Forwarded progress uses those same
normalized deltas, omitting empty ones. Neither deltas nor assembled
text/reasoning have an application byte quota.

Tool-argument deltas use the same character replacements before JSON assembly,
but are never forwarded as progress; arguments must still form a valid JSON
object within the parser's stack-safety guard. Opaque replay, encrypted reasoning
and signatures retain their own validation rules.

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
  `InvalidModelResponse`. A fully assembled `Unknown` no-tool response is
  retained in complete history and handed off with its actual finish reason
  before that failure outcome; it is not partial. A `ToolCalls` label without
  calls fails assembly and does not create complete history.
- Streaming: the driver yields after progress and the runner drains ready
  progress before polling the driver again, so back-to-back deltas do not
  starve forwarding. `TextDelta` and `ReasoningDelta` use unbounded,
  non-awaiting queues with no capacity-driven loss. They remain optional
  observations and carry no correctness weight. Tool-argument deltas are
  assembled privately and are never forwarded as progress.
- Model timeouts, retry exhaustion, start/poll panics, and malformed responses surface
  as `Failed(Model)` or `Failed(InvalidModelResponse)` (or the specific
  mapping in the loop contract), never as a hang. In both model and
  invalid-response failure paths, the runtime preserves the structured
  `ModelError` accessible via `LoopFailure::model_error()`. A retry hint in
  `ModelError` describes per-request delivery recoverability and must not be
  interpreted as an instruction for the runtime to automatically retry the
  entire loop.

## Retry State

The driver is the sole retry authority. A private synchronous handoff publishes
actual attempts and retry scheduling directly into `LoopState` before emitting
its normal optional `StateChanged` observation. This handoff does not use the
output-progress queue or a public callback. Events can be absent, disconnected
or undrained without changing the state, attempt count or execution.

`LoopState.model_retry` is `None` outside an active retry. `ModelRetryProgress`
retains the enclosing state's original loop and request association without
repeating their IDs. Its `attempt` includes the initial request, starts at two,
and never exceeds `max_attempts`; `error` is the preceding attempt's exact
structured `ModelError`, not a new synthesized failure.

- `ModelRetryPhase::Waiting` means that attempt is scheduled, not issued.
  `retry_at: SystemTime` is sampled at the source for a host's UTC/RFC3339 display.
  `retry_deadline: tokio::time::Instant` is the actual monotonic sleep target.
  No wall-clock adjustment or client countdown affects execution.
- `Requesting` means `Model::start` has actually been invoked, including a call
  whose synchronous start panics. `LoopState.model_attempts` increments only
  then and includes the initial call. It remains unchanged during backoff.
- The driver synchronously clears retry state when the model request settles,
  including success, failure, timeout and cancellation, before any history
  acknowledgement, tool execution or next request preparation. A normal state
  update publishes that clearing. The actual count remains through tool work
  and resets at the next preparation; final state uses the last issued request.
- `LoopReport.model_attempts` retains a count per issued logical request in
  zero-based index order, independently of watch coalescing. A logical request
  stopped by a preflight check before `Model::start` has count zero. Internal
  retries do not increment `LoopReport.requests` or change `request_index`.

The existing safe-retry predicate, total-attempt range 1–4 (default 2), capped
exponential delay, Retry-After lower bound, overall deadline and cancellation
rules are unchanged. Retry-After above 30 seconds or a delay that does not fit
strictly before the deadline stops retrying without scheduling a shorter wait.
There is no new event kind, retry object, acknowledgement, polling loop or host
retry policy. Watches retain current state rather than replay every transition.

## Limits and Stack Safety

`ModelLimits` retains actual context-window and output-token limits, and model
capability validation remains enforced. `ModelFinishReason::Length` still means
`OutputLimit`; removing application quotas does not change a provider's limits.
`LoopOptions::validate` retains model timeout (<= 24h), retry attempts (1–4),
retry base delay (<= 30s), nonzero port timeouts and future loop-deadline rules.

There is no `LoopLimits`, text/reasoning byte cap, per-response tool-call quota,
schema/argument byte or node cap, or round budget. Typed tool names and IDs keep
their protocol grammar and lengths. Duplicate IDs, invalid ordering, malformed
JSON and invalid tool associations remain errors.

`validate_json_nesting` accepts 127 nested arrays/objects and rejects 128, the
locked serde_json parser's default stack-safety boundary. Width and node count
are unrestricted. Deserializing an entire DTO also counts its enclosing objects
toward the parser's guard; the shared value guard checks the payload itself.

## Opaque provider replay (v0.6.1)

`ProviderReplay::new(format, payload)` validates a provider-owned JSON envelope:
format is 1–128 symbolic ASCII bytes and payload nesting follows the shared
JSON stack-safety guard. Envelope size and payload node count have no application
quota. Deserialization runs the same validation. Runtime never interprets or
truncates provider content.
`byte_len()` includes all envelope keys and punctuation, and history accounting
includes these bytes in its size estimate; the runtime does not cap total
history size.

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

## Unfinished Model Output

`LoopReport.partial: Option<ModelPartial>` carries output from the one model
request that failed or was cancelled before producing a complete response. The
assembly state, not `LoopEvent` or driver progress, supplies this data on error,
EOF without a valid response, timeout, cancellation and isolated stream-poll
panic. No output means `None`; a usage-only event does not manufacture a reply.
A response requires the existing valid Finish and clean EOF contract.

`ModelPartial` is serializable and contains:

- `request_index`: the original zero-based request within this loop, unchanged
  by driver retries, with loop association provided by the enclosing report.
- `content`: existing `AssistantPart` text, reasoning and actually validated
  complete tool calls, in original canonical order excluding unfinished slots.
- `incomplete_tool_calls`: `IncompleteToolCall` values retaining `part_index`
  (zero-based original canonical position), `call_index`, `tool_call_id`,
  `tool_name` and assembled `arguments_text`. Insert these at their part indices
  among `content` to recover the canonical ordering. Their raw arguments are
  never completed, parsed speculatively or turned into executable calls.
- `usage` and `provider_replay`: only the optional values actually observed for
  this unfinished request; complete-response report usage remains unchanged.

The model port already supplies a validated full tool name on `ToolCallStart`;
this change does not invent a new name-streaming port. Text and reasoning keep
existing assembly normalization without application byte quotas. Debug output redacts argument text.
No second error or loop identity is stored in a partial. A subsequent tool-stage
failure/cancellation retains the original complete assistant history and finish
reason with `partial: None`. Partial content is never appended as `HistoryItem`
or automatically fed into a future model request, including any complete tool
call parts inside the unfinished response. Persistence and replay adaptation
remain separate host responsibilities.
