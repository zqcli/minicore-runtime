# MiniCore Runtime

MiniCore Runtime is a small Rust execution core for one live agent loop. It runs model/tool iterations, streaming, cancellation, interaction, steering, and request-boundary configuration updates. It does not own sessions, persistence, transcripts, providers, or workspaces.

## 0.7.0 Compatibility Notes

- `ToolSpec` fields are private. Construct with `ToolSpec::new` (or deserialize)
  and read through `name()`, `description()` and `input_schema()`. The now-impossible
  `ToolSetError::InvalidSpec` variant is removed. Serialized tool specs are unchanged.
- `steer`, `update` and `answer` return `NotActive` after cancellation, without
  waiting for finalization. EOF before any model event reports `Unknown` delivery;
  interrupted retry backoff preserves the previous model error as its cause.

- `AssistantPart::Text`, `ModelMessage::System` and `ModelMessage::User` now
  carry `Arc<str>` instead of `String`. Use `.into()` for enum construction and
  `.as_ref()` for string comparisons. Constructors and JSON formats are unchanged.
- `BoundedText`, reasoning payloads, tool-call arguments and provider replay
  share immutable storage. `ModelRequest` clones share message/tool slices;
  `ModelRequest::new` accepts a tool `Vec` or `Arc<[ToolSpec]>`.
  `ToolCall::arguments()` and `ProviderReplay::payload()` are no longer `const`.
  Redundant `ToolCall::validate()` is removed: constructors and deserialization
  already guarantee valid arguments.
- `DiagnosticSummary::new` now returns `Result<_, ValueError>` and its `message`
  field is private (read through `message()`). Diagnostics and tool progress
  reject controls other than LF/TAB; `ToolProgressError::InvalidText` and
  `ValueError::InvalidText` are new variants. Input prompts/answers now allow LF/TAB.
- Model text normalizes CRLF/CR to LF and other controls except LF/TAB to a
  single space, never increasing byte length. Delta constructors retain raw text
  and cap its bytes; assembly handles split CRLF independently for text/reasoning,
  producing matching canonical text and progress. Hosts may chunk at any UTF-8
  boundary up to `MAX_MODEL_EVENT_TEXT_BYTES`, without expansion headroom or CRLF
  workarounds. Empty deltas are ignored without breaking CRLF continuity.
- `LoopStatus`, `CancelReason`, and `LoopOutcome` are non-exhaustive: downstream
  matches need a wildcard arm. Usage sums known values per field instead of
  losing them when another request omits that field.
- `LoopHandle::shutdown()` records shutdown cancellation. Interactions without a
  loop deadline wait indefinitely. Dropping a `join()` future cancels its loop.
- `history::estimate_history_bytes` exposes the exact start-admission estimate.
  Configs discarded by finalization are dropped only after report publication.

## 0.6.1 Compatibility Notes

- Provider replay is bounded opaque metadata carried by `ModelEvent::ProviderReplay`,
  `ModelResponse`, `AssistantHistory`, and `ModelMessage::AssistantWithReplay`.
  Existing assistant tuple construction, constructors, and legacy JSON remain valid.
  History struct literals must initialize the new optional `provider_replay` field.
- A replay envelope is at most 256 KiB of compact JSON, with depth 32 and 16,384
  payload nodes. Runtime does not interpret its provider format or truncate it.
  Host history's 2 MiB ceiling includes the full replay envelope byte count.
- Replay-only assistant content is accepted only with validated metadata; legacy
  empty constructors remain invalid. Replay never creates display text/tool calls.
  Its payload is accessible to the host and serialized in history, but redacted
  from Debug and never forwarded as a loop output event.

## 0.6.0 Compatibility Notes

- `LoopReport.tool_rounds` is now `u64`. Hosts that expose this count through JSON or storage must preserve the wider integer; clients limited to `u16` cannot read values above 65,535.
- `LoopOptions.max_tool_rounds` remains `u16`, with zero (the default) meaning unlimited. Cancellation, deadlines, and other resource limits still apply.
- Each actual prompt preparation emits one `Starting` state, including preparation after tools and stale-prompt rebuilds; the redundant initial event is removed.
- There is no Runtime compaction scheduler. Post-turn summaries and bounded request-level context recovery are Agent policies built on the existing generic interfaces.
- MSRV remains Rust 1.85. Historical v0.4 acceptance and release records below are preserved; they are not new 0.6.0 verification claims.

## What the Runtime Does Not Own

An `AgentLoop` is a single, one-shot execution. The host owns everything around it:

- An `AgentLoop` runs exactly once; it cannot be resumed, loaded, or reopened.
- Sessions are owned by the host. The runtime has no session type, no session lifecycle, and no open/load/save surface.
- History is passed in by the host. The runtime reads `HistoryItem`s, appends its in-memory results to `LoopReport::appended`, and never persists anything.
- `LoopReport` is saved by the host. `wait`/`join` return the same `Arc<LoopReport>`: the report stays available through the loop's completion channel for as long as the owner or any handle lives, and every waiter receives the same `Arc`.
- Events are not authoritative. The event stream is a best-effort, bounded, single-consumer channel; dropped events (including internal model/tool progress loss) are counted in `dropped_before`, never reconstructed. The report is the only authoritative outcome.
- Tool side effects are not atomic with host logging. A tool can run successfully (with side effects) even when the host never sees the corresponding streamed event or persisted log.
- All tool outputs in history are bounded by `max_tool_output_bytes`. Oversized tool outputs are downgraded to failure results rather than corrupting history.
- Tool input requests are strictly validated at the port boundary; invalid requests fail immediately as ordinary tool errors without entering `WaitingForInput`.
- `update` takes effect at the next model request. A config revision is committed only when a real model request is issued; replaced or discarded configurations are dropped outside the control lock.
- The current tool batch uses the snapshot of the request that produced it. A config `update` or `steer` never retroactively changes an in-flight batch.
- `steer` takes effect at the next model request. A successful call means the steer was accepted into this process-local queue and is applied at the next request boundary; on any non-`Completed` ending or process exit before that boundary, the queued steer is discarded and will not appear in `LoopReport::appended`. Steers queue up to `max_pending_steers` (`QueueFull`), are rejected if an interaction is pending (`WaitingForInput`) or after cancellation/finalization (`NotActive`), and appear as `Steering` user items at the next request boundary.
- A config `update` does not keep the loop alive. It only reconfigures the next request; whether the loop continues is decided by the model's final response and pending steers.
- `LoopReport.requests` and final `request_index` count issued requests at the `RequestStarted` boundary. Failed issued requests are counted; prompt rebuilds and driver-internal retries do not increment logical request count. The runtime never automatically retries an entire loop.
- `LoopReport.tool_rounds` counts tool batches with `u64` precision. The optional `LoopOptions.max_tool_rounds` remains `u16`; zero (the default) means no tool-round limit, not a limit on statistics.
- `PromptProvider` projects host-owned context at each request boundary. Summary generation, compaction timing, and persistence are host policies, not Runtime scheduling responsibilities.
- `LoopFailure::model_error()` preserves structured `ModelError` metadata for model and invalid-response failures without leaking diagnostics via `Debug`.
- Tool catalog size is independent from per-response `ToolCall` limits (`max_tool_calls_per_response`).
- The Runtime needs a Tokio context. `AgentLoop::start` spawns its single runner task; call it from inside a Tokio runtime.

## Public Surface

The crate exports the loop types at the root and keeps the rest under focused modules:

- `AgentLoop`, `LoopHandle`, `LoopRequest`, `LoopOptions`, `LoopReport`, `LoopState`, `LoopEvent(Stream)`, plus `UpdateError` / `SteerError` / cancel reasons and failure kinds.
- `execution`: `ExecutionConfig`, `ConfigRevision`, `UserInput`.
- `history`: `HistoryItem` (user / assistant / tool result / summary) and `HistoryView`.
- `model`: the `Model` seam, `ModelRequest` / `ModelMessage`, typed responses.
- `prompt`: the `PromptProvider` seam and `DefaultPromptProvider`.
- `tools`: the `Tool` seam, `ToolSet`, policies, interactions, progress.
- `limits`, `interaction`, `ids`, `value`, `error`.

## API Sketch

A simple text loop:

```rust
let config = ExecutionConfig::new(
    model,                         // Arc<dyn Model>
    ReasoningPreference::Auto,
    ToolSet::default(),            // or ToolSet::builder().register(tool).build()?
    None,                          // Optional Arc<dyn ToolPolicy>
    Arc::new(DefaultPromptProvider::new(None)),
)?;                                // ExecutionConfigError

let request = LoopRequest::new(
    Arc::from(Vec::<HistoryItem>::new()), // host-owned prior history
    UserInput::text("hello")?,
    config,
);

let mut agent = AgentLoop::start(request, LoopOptions::default_checked()?)?;
let events = agent.take_events()?; // optional best-effort stream
let report = agent.join().await?; // Arc<LoopReport>; host keeps it
```

Steering, updating, and cancelling a live loop:

```rust
let handle = agent.handle().clone();
handle.steer(UserInput::text("focus on the time")?)?;   // next request
let revision = handle.update(new_config)?;              // next request
handle.cancel();                                         // best-effort
```

## Examples

Compiled, runnable examples live in [`examples/`](examples):

- [`examples/agent_loop.rs`](examples/agent_loop.rs): a simple text loop, a
  tool loop, live event streaming, a deterministic steer+update demo that
  runs a second request under the updated config (proved via `requests == 2`
  and the second `RequestStarted` revision), and a separate held loop that is
  cancelled, all against fake adapters.
- [`examples/memory_agent.rs`](examples/memory_agent.rs): a host-owned `MemoryAgent` that builds a multi-turn conversation from repeated one-shot loops.

## Documentation

For the full picture see [`docs/README.md`](docs/README.md): architecture, module map, contracts, host boundary, and the v0.3-to-v0.4 migration. The tracked [v0.4 implementation spec](minicore-runtime-v0.4-flex-agent-loop-reset-spec.md) is a non-authority record of the reset work.