# MiniCore Runtime

[![CI](https://github.com/zqcli/minicore-runtime/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/zqcli/minicore-runtime/actions/workflows/ci.yml)

MiniCore Runtime is a small Rust execution core for one live agent loop. Give
it host-owned history, one user input, a model, and a tool set. It runs model
requests and tool batches until the model finishes, streams progress, takes
the host's inputs at request boundaries, accepts configuration updates,
cancellation, and interaction answers while running, and returns one authoritative `LoopReport`. Sessions, persistence,
transcripts, providers, and workspaces stay with the host.
[MiniCore Agent](https://github.com/zqcli/minicore-agent) is the reference
host.

| | |
|---|---|
| Version | 0.10.0 |
| Rust | edition 2024, current stable |
| Async runtime | Tokio (the loop spawns one task) |
| Production dependencies | serde, serde_json, tokio, tokio-util, futures-util, thiserror, getrandom |
| License | MIT OR Apache-2.0 |

## Features

- **One-shot loop.** An `AgentLoop` runs exactly once. `join` and every
  handle's `wait` return the same `Arc<LoopReport>`.
- **Live control.** The host's `LoopInputs` hands over user inputs before each
  request, and `LoopHandle` updates the next request, answers approval and
  tool-input interactions, and cancels or shuts down the loop.
- **Pluggable seams.** Models, tools, tool policies, and prompt projection are
  traits (`Model`, `Tool`, `ToolPolicy`, `PromptProvider`). No provider SDK is
  bundled.
- **Model driver.** Bounded retries that honor provider retry hints, request and
  turn deadlines, and text normalization that never grows a delta.
- **No application quotas.** Text, tool payloads and counts, rounds, history
  and prompts are not capped or truncated. Protocol validity, actual model
  token/capability limits, and JSON parser stack safety remain enforced.
- **Optional events.** Unbounded, non-awaiting observation channels never drop
  progress because a queue is full. A disconnected consumer cannot affect execution.
- **Current-state observation.** `LoopHandle::state` and `watch_state` expose
  the latest loop state and pending interaction independently of event consumption.
  The watch coalesces updates; `wait`/`join` establish the final outcome.

## Installation

The crate is not published to crates.io. Depend on it by Git revision:

```toml
[dependencies]
minicore-runtime = { git = "https://github.com/zqcli/minicore-runtime", rev = "<commit>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Usage

A text loop:

```rust
use std::sync::Arc;
use minicore_runtime::history::HistoryItem;
use minicore_runtime::model::ReasoningPreference;
use minicore_runtime::prompt::DefaultPromptProvider;
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    AgentLoop, ExecutionConfig, LoopInputs, LoopOptions, LoopRequest, UserInput,
};

struct NoInputs; // a real host returns the inputs it accepted since the last call
impl LoopInputs for NoInputs {
    fn take(&self) -> Vec<UserInput> {
        Vec::new()
    }
}

let config = ExecutionConfig::new(
    model,                         // Arc<dyn Model>
    ReasoningPreference::Auto,
    ToolSet::default(),            // or a set assembled with ToolSet::builder()
    None,                          // optional Arc<dyn ToolPolicy>
    Arc::new(DefaultPromptProvider::new(None)),
)?;

let request = LoopRequest::new(
    Arc::from(Vec::<HistoryItem>::new()), // host-owned prior history
    UserInput::text("hello")?,
    config,
);

let mut agent = AgentLoop::start(request, LoopOptions::default_checked()?, Arc::new(NoInputs))?;
let events = agent.take_events()?; // optional best-effort stream
let report = agent.join().await?; // Arc<LoopReport>; the host keeps it
```

Controlling a running loop (before `join`):

```rust
let handle = agent.handle();
let revision = handle.update(new_config)?; // applied at the next request
handle.cancel();                           // or handle.shutdown()
```

Inputs reach the running loop through the `LoopInputs` the host passed to
`start`: the loop calls `take` before every request preparation (the first
included), and the host returns the inputs it accepted since the last call.

Two runnable examples use fake adapters:

```bash
cargo run --example agent_loop    # text, tools, events, input + update, cancel
cargo run --example memory_agent  # a multi-turn host built from one-shot loops
```

## Host Boundary

The Runtime executes one loop. The host owns everything around it:

- An `AgentLoop` runs exactly once; it cannot be resumed, loaded, or reopened.
- Sessions are owned by the host. There is no session type, lifecycle, or
  open/load/save surface.
- History is passed in by the host. The Runtime reads `HistoryItem`s, returns
  its new items in `LoopReport::appended`, and never persists anything. An optional
  `LoopHistory` receiver acknowledges incremental complete records while it runs.
- `LoopReport` is saved by the host. `wait` and `join` return the same
  `Arc<LoopReport>` for as long as the owner or any handle lives.
- Events are not authoritative. The single-consumer stream and internal model
  and tool progress queues are unbounded and non-awaiting. There is no queue
  capacity, lost-on-full path, drop counter, event acknowledgement or replay store.
  An absent or disconnected consumer has no business effect.
- Read a new `watch_state` receiver's initial value before waiting for changes;
  it may already contain a pending interaction or the final state. Retained
  handles keep the state channel open after `join`, so channel closure is not
  a completion signal. See the [observation contract](docs/contracts/event-stream.md#current-state-observation).
- Tool side effects are not atomic with host logging. A tool can succeed even
  when the host never sees its event or persists its result.
- `update` takes effect at the next model request. A revision is committed only
  when a real request is issued. The current tool batch always uses the
  snapshot of the request that produced it.
- Inputs come from the host's `LoopInputs::take`, called synchronously on the
  runner task with no Runtime lock held, so it must not block, await or do IO.
  The host owns the queue and validates each input (non-empty, within
  `max_user_input_bytes`) where it accepts it; the Runtime does not re-check.
  Returning inputs from `take` hands them over for good: they join the loop's
  history as `UserMessageKind::Steering` items in order and appear in
  `LoopReport::appended` for every outcome, cancellation included. Inputs the
  loop never took stay with the host.
- `take` is called at two points: before every request preparation, the first
  request included, and again once a prompt is prepared. A non-empty second
  call discards that prompt and rebuilds it without advancing `request_index`.
  It is never called during a model call or a tool batch, nor while the loop
  waits for an interaction answer.
- A model answer without tool calls always ends the loop, and `take` is not
  called after it. A host that wants to continue starts a new loop.
- `take` has no request-identity argument. Hosts that associate accepted inputs
  with candidate request indices observe both prompt preparation and model
  issuance on every installed config, including updates; state snapshots and
  take counts cannot supply that identity. See the [input contract](docs/contracts/agent-loop.md#inputs).
- An `update` does not keep the loop alive.
- `LoopReport.requests` counts issued requests. Driver retries stay inside one
  logical request, and the Runtime never retries a whole loop.
- Summaries, compaction timing, and persistence are host policies implemented
  through `PromptProvider`; the Runtime has no compaction scheduler.
- The Runtime needs a Tokio context: call `AgentLoop::start` inside a Tokio
  runtime.

## Public Surface

The crate root exports the loop types: `AgentLoop`, `LoopHandle`,
`LoopRequest`, `LoopOptions`, `LoopInputs`, `LoopReport`, `LoopState`, `LoopEvent`,
`LoopEventStream`, outcomes, and their error types. Everything else lives in
focused modules:

| Module | Contents |
|---|---|
| `execution` | `ExecutionConfig`, `ConfigRevision`, `UserInput` |
| `history` | `HistoryItem` (user, assistant, tool result, summary), `HistoryView`, `estimate_history_bytes` |
| `model` | The `Model` trait, `ModelRequest`/`ModelMessage`, events, errors, provider replay |
| `prompt` | The `PromptProvider` trait and `DefaultPromptProvider` |
| `tools` | The `Tool` trait, `ToolSpec`, `ToolSet`, policies, interactions, progress |
| `interaction`, `value`, `error` | Interaction DTOs, shared `Text`, JSON stack safety, diagnostics |

Generate the API docs with `cargo doc --no-deps --open`.

## Development

```bash
./scripts/check.sh       # fmt, tests, Clippy, rustdoc, docs and architecture gates
```

`check.sh` needs `python3` and `rg`. CI runs it on Ubuntu with stable Rust and
the tests on macOS and Windows; no older toolchain is tested.
`./scripts/check-provider-gate.sh` runs the standalone provider evidence harness
in `provider-gate/`; run it only when that harness or its provider dependency
changes. See [tests/README.md](tests/README.md)
for the test inventory and [AGENTS.md](AGENTS.md) for coding-agent guidance.

## Compatibility Notes

Versioned sections describe that version; the latest notes and current contracts
define the present API.

### 0.10.0

- **Breaking:** `BoundedText` becomes `Text`, with infallible `Text::new` and
  the same plain-string wire format. Remove `new_with_max_bytes` and obsolete
  byte/node constants. Model delta constructors are also infallible; remove
  their `?`/`unwrap` calls. `ModelEvent::validate` is removed.
- **Breaking:** remove `LoopLimits`, `LoopLimitsError`, the `limits` module,
  and `LoopOptions::{limits, max_tool_rounds, event_capacity}`. Remove
  `LoopEventEnvelope::dropped_before`, budget-only `LoopFailureKind::MaxToolRounds`,
  `LoopStartError::InvalidConfig`, `UpdateError::InvalidConfig`,
  `ExecutionConfigError::InvalidTools`, `DiagnosticCode::TurnBudgetExceeded`,
  and `ModelValueError::InvalidEvent` match arms. No alias, no-op option,
  unlimited sentinel or compatibility mode replaces these APIs.
- Valid text/reasoning, tool schemas/arguments/results, interaction text/choices,
  replay payloads and tool-call/round counts have no application quota.
  `DefaultPromptProvider` preserves the entire summary prefix and content.
  Public and internal progress channels use non-awaiting unbounded sends;
  observation never determines the outcome, and there is no full/drop path.
- Keep symbolic ID/name grammar and lengths, nonempty/control/type/association
  checks, actual `ModelLimits`, and existing model timeout/retry rules.
  `ModelFinishReason::Length` still reports `OutputLimit`. The shared
  `validate_json_nesting` guard accepts at most `MAX_JSON_NESTING = 127`
  nested arrays/objects, matching locked serde_json's default stack guard;
  it does not cap JSON byte size or width/node count. Serialized enclosing
  DTO objects also count toward the parser's depth when reading a whole wire value.
- The version is prepared locally; these notes do not imply a tag, push or
  published release.

- `LoopOptions.history` optionally installs `Arc<dyn LoopHistory>`; the default
  remains `None`. `LoopHistory::append` returns `HistoryFuture` and acknowledges
  ordered incremental records before tools, the next request, or final unwind.
  `AgentLoop::start` and `LoopInputs` retain their existing signatures.
- Callbacks receive the original `LoopId`, an offset into `LoopReport.appended`,
  and a borrowed slice of only new items. Callbacks are never retried, even after
  failure, and are not dropped by cancellation. Hosts track accepted records
  separately from successful saving and reconcile the full final report by offset.
- `LoopReport.partial` is an optional serializable `model::ModelPartial` from
  the actual failed/cancelled model assembly, not the observation stream. It
  preserves the original `request_index`, canonical `AssistantPart`s, unfinished
  tool argument fragments, and any observed usage/replay metadata. Add `partial`
  and `history_failure` when constructing report literals.
- `model::IncompleteToolCall` retains the original part/call indices, call ID,
  tool name and raw argument text; no missing arguments or executable calls are
  invented. Partial content is not a `HistoryItem` or automatic replay input.
- A fully assembled response with unsupported no-tool `Unknown` finish still
  fails the loop, but now remains complete history with its actual finish reason
  and is handed off before termination; it is never reclassified as partial.
  Aggregate usage overflow also retains the already complete response.
- `LoopReport.history_failure` retains a callback error or panic independently
  of the primary outcome. Known business failures keep their diagnostic and
  structured model error; prior cancellation remains cancellation. A no-tool
  completed candidate may have `Completed` plus a history failure, so hosts must
  check this field before continuation or claiming saved completion.
- A save failure that prevents otherwise-required tools or the next request
  uses `LoopFailureKind::History`; it never becomes a model retry or tool
  business error. Known non-completed endings are claimed before awaiting
  saving, so later cancellation cannot replace them; completed candidates stay
  cancellable while saving. Hosts using `LoopOptions` struct literals must add
  `history`; cloning options shares the receiver without requiring `Debug`.

### 0.9.2

- Documentation and test maintenance only; production code, public APIs,
  dependencies and the 0.9.0 loop semantics are unchanged.
- Document the distinction between current-state watch observation, lossy
  events and authoritative completion; retain host-owned input-boundary
  association rather than adding a Runtime identity API.
- Align the current architecture, module and test indexes with the source.
  The seven host-observation cases now share one contract test target.

### 0.9.1

- No API or behavior changes. The version follows the 0.9.1 consistency work in
  `minicore-agent`, which keeps 0.9.0 loop semantics. Runtime still gains no
  session, storage, or provider surface.

### 0.9.0

- **Breaking:** `AgentLoop::start` takes a third argument, `Arc<dyn LoopInputs>`.
  `LoopInputs::take(&self) -> Vec<UserInput>` is implemented by the host and
  called before every request preparation and again after a prompt is
  prepared; a host with nothing to hand over passes an implementation that
  returns an empty `Vec`.
- **Breaking:** `LoopHandle::steer`, `SteerError`, `LoopOptions::max_pending_steers`
  and the interaction-time steer refusal are removed, along with the
  `max_pending_steers` validation. Inputs are no longer pushed into the loop;
  the host queues them and the loop pulls them.
- **Breaking:** a model answer without tool calls always ends the loop. A steer
  no longer extends a final answer; a host that has more input starts another
  loop.
- Handed-over inputs are appended as `UserMessageKind::Steering` items in the
  order returned, one item per input, and stay in `LoopReport::appended` for
  every outcome. `update`, `answer`, `cancel` and `shutdown` are unchanged.

### 0.8.0

- **Breaking:** `LoopLimits::max_history_items`, `max_history_bytes` and
  `max_prompt_messages` are removed, with `LoopStartError::HistoryTooLarge`.
  `AgentLoop::start` no longer rejects large history, and a request may carry
  any number of messages. Per-item bounds are unchanged. Hosts that construct
  `LoopLimits` by struct literal or read these fields must drop them; a host
  that matches `LoopStartError` exhaustively must drop the arm.
- `history::estimate_history_bytes` stays as a host-side size estimate; the loop
  no longer uses it.

### 0.7.4

- `LoopLimits::max_history_bytes` accepts up to 32 MiB, so a host can size
  history to a large model context. The default stays 2 MiB.

### 0.7.3

- No API or behavior changes. The version follows `minicore-agent` 0.7.3; there
  is no 0.7.2.
- No minimum Rust version is declared or tested any more: build with current
  stable. Edition 2024 still needs Rust 1.85 or newer.
- `getrandom` 0.4, and every dependency updated to its latest release.

### 0.7.1

- No API or behavior changes. CI uploads release builds of the examples for
  Linux, macOS, and Windows as workflow artifacts.

### 0.7.0

- `ToolSpec` fields are private. Construct one with `ToolSpec::new` (or
  deserialize it) and read it through `name()`, `description()`, and
  `input_schema()`. `ToolSetError::InvalidSpec` is removed. Serialized tool
  specs are unchanged.
- `steer`, `update`, and `answer` return `NotActive` after cancellation, without
  waiting for finalization. EOF before any model event reports `Unknown`
  delivery. Interrupted retry backoff keeps the previous model error as its
  cause.
- `AssistantPart::Text`, `ModelMessage::System`, and `ModelMessage::User` carry
  `Arc<str>` instead of `String`. Use `.into()` to construct and `.as_ref()` to
  compare. Constructors and JSON formats are unchanged.
- `BoundedText`, reasoning payloads, tool-call arguments, and provider replay
  share immutable storage. `ModelRequest` clones share message and tool slices;
  `ModelRequest::new` accepts a tool `Vec` or `Arc<[ToolSpec]>`.
  `ToolCall::arguments()` and `ProviderReplay::payload()` are no longer `const`,
  and the redundant `ToolCall::validate()` is removed.
- `DiagnosticSummary::new` returns `Result<_, ValueError>`, and its `message`
  field is private (read it through `message()`). Diagnostics and tool progress
  reject controls other than LF and TAB, through the new
  `ToolProgressError::InvalidText` and `ValueError::InvalidText` variants.
  Input prompts and answers now allow LF and TAB.
- Model text normalizes CRLF and CR to LF, and other controls except LF and TAB
  to one space, never increasing byte length. Assembly handles CRLF split across
  deltas separately for text and reasoning. Hosts may chunk at any UTF-8
  boundary up to `MAX_MODEL_EVENT_TEXT_BYTES`. Empty deltas are ignored.
- `LoopStatus`, `CancelReason`, and `LoopOutcome` are `#[non_exhaustive]`;
  downstream matches need a wildcard arm. Usage sums known values per field.
- `LoopHandle::shutdown()` records shutdown cancellation. Interactions without
  a loop deadline wait indefinitely. Dropping a `join()` future cancels its loop.
- `history::estimate_history_bytes` exposes the exact history size estimate.
  Configs discarded by finalization are dropped after report publication.

### 0.6.1

- Provider replay is bounded opaque metadata carried by
  `ModelEvent::ProviderReplay`, `ModelResponse`, `AssistantHistory`, and
  `ModelMessage::AssistantWithReplay`. Existing constructors and legacy JSON
  remain valid; history struct literals must initialize `provider_replay`.
- A replay envelope is at most 256 KiB of compact JSON, with depth 32 and 16,384
  payload nodes. The Runtime neither interprets nor truncates it. Hosts should
  count the envelope in their own history size estimate.
- Replay never creates display text or tool calls. Its payload is visible to the
  host and serialized in history, but redacted from `Debug` and never emitted as
  an output event.

### 0.6.0

- `LoopReport.tool_rounds` is `u64`. Hosts exposing it through JSON or storage
  must keep the wider integer.
- `LoopOptions.max_tool_rounds` stays `u16`; zero (the default) means unlimited.
- Each real prompt preparation emits one `Starting` state.

## Documentation

[docs/README.md](docs/README.md) indexes the authority set:
[architecture](docs/architecture.md), the [module map](docs/modules/README.md),
the [agent-loop contract](docs/contracts/agent-loop.md) and its focused
contracts, the [host boundary](docs/integration/host-boundary.md), the
[v0.3-to-v0.4 migration](docs/migrations/v0.3-to-v0.4.md), and the
[release notes from 0.7.0 onward](docs/release-v0.7.md).

## License

Licensed under `MIT OR Apache-2.0`, as declared in `Cargo.toml`.
