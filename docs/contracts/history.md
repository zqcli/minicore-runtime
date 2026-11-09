# History Contract

The host-facing conversation vocabulary passed in and returned by the loop.

## Vocabulary

`HistoryItem` is one of:

- `User(UserHistory { loop_id, kind, input })` — `kind` is `Prompt` (the
  input that started the loop) or `Steering` (an input taken from
  `LoopInputs` at a request boundary). `input: UserInput` is bounded text.
- `Assistant(AssistantHistory { loop_id, request_index, model, reasoning,
  content, provider_replay, finish_reason, usage })` — a complete, locally
  validated model response with typed `AssistantPart`s (text / reasoning /
  tool call) and optional opaque replay metadata. Replay is not display text;
  see the [model contract](model.md).
- `ToolResult(ToolResultHistory { loop_id, request_index, call_id, tool_name,
  outcome, output })` — an executed call's result or a synthesized settlement
  for a call that did not run, such as a denied or cancelled call.
- `Summary(SummaryHistory { content })` — a host-authored compression
  marker with no durable boundary semantics.

## Ownership and Projection

- The host owns history. It passes an `Arc<[HistoryItem]>` in
  `LoopRequest::history` and receives `LoopReport::appended`, the in-memory
  items the loop produced. The runtime never persists history and never
  merges host history.
- `HistoryView::new(base, appended)` borrows the host base plus the current
  loop delta without copying; at most one view is active per request and it
  is handed to the prompt provider.
- The host decides what to keep: append `report.appended` to its own
  history, transform it, or discard it. An installed incremental callback must
  acknowledge its boundary before execution continues.

## Invariants

- Items must be constructible with the bounded value types (`BoundedText`,
  `UserInput`, `ToolOutput`); malformed values are rejected at construction
  or by the loop as `Prompt`-path failures.
- A `ToolResult` must agree with the tool-exchange the model actually
  produced; `ModelRequest` validates that every `ToolCall` is matched by a
  `ToolResult` with the same id.
- Host-provided history is trusted for semantics; the loop only enforces the
  structural validation described in the [prompt contract](prompt.md).
- The loop caps neither the number nor the total size of history items; the
  host bounds history, for example by token-based compaction.
  `estimate_history_bytes` is a host-side size estimate only.

## Incremental Handoff

`LoopOptions.history` optionally supplies a `LoopHistory` receiver. Its `append`
method receives the original `LoopId`, an offset within this loop's `appended`,
and a borrowed slice of only new ordered `HistoryItem`s. Base history is never
redelivered. No additional task, queue, identifier, or observation subscription
is involved; the callback future runs on the existing runner task.

Nonempty ranges are handed off after a complete response, after a settled tool
batch (including actual failure/cancellation results), and at final unwind for
otherwise unhanded consumed inputs or settlements. The response acknowledgement
precedes tools or a normal completion candidate. The tool-batch acknowledgement
precedes the next model request. Nothing is handed off once per output token.

`Ok(())` means the receiver accepted the range and completed applicable saving.
The receiver must own accepted work and keep its accepted position separately
from saved position, including when saving fails. Runtime awaits the callback
without cancelling or timing out that future, even if the loop is cancelled,
its owner is dropped, or its deadline passes. Hosts must complete or fail their
own saving; a callback that never settles prevents report publication.

A failed or panicking callback stops subsequent model/tool work with a `History`
failure, subject to the existing cancel/seal race. It is never retried, and no
completed tool is replayed. The final report still includes every complete item.
The host reconciles any missing tail using its accepted offset, not its saved
offset; it must not append an accepted but unsaved range twice. Callback arguments
remain valid only for the call, so the receiver copies or shares needed items
before retaining them beyond it. With no callback, join-only behavior is unchanged.
