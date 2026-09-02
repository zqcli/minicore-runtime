# History Contract

The host-facing conversation vocabulary passed in and returned by the loop.

## Vocabulary

`HistoryItem` is one of:

- `User(UserHistory { loop_id, kind, input })` — `kind` is `Prompt` (the
  input that started/extends the loop) or `Steering` (a host steer applied
  at a request boundary). `input: UserInput` is bounded text.
- `Assistant(AssistantHistory { loop_id, request_index, model, reasoning,
  content, finish_reason, usage })` — a complete, locally validated model
  response with typed `AssistantPart`s (text / reasoning / tool call).
- `ToolResult(ToolResultHistory { call_id, tool_name, outcome, output })` —
  the result of one tool call that ran in a loop.
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
  history, transform it, or discard it. Nothing in the runtime depends on
  that choice.

## Invariants

- Items must be constructible with the bounded value types (`BoundedText`,
  `UserInput`, `ToolOutput`); malformed values are rejected at construction
  or by the loop as `Prompt`-path failures.
- A `ToolResult` must agree with the tool-exchange the model actually
  produced; `ModelRequest` validates that every `ToolCall` is matched by a
  `ToolResult` with the same id.
- Host-provided history is trusted for semantics; the loop only enforces the
  message-budget and structural validation described in the
  [prompt contract](prompt.md).