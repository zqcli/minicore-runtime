# MiniCore Runtime

[![CI](https://github.com/zqcli/minicore-runtime/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/zqcli/minicore-runtime/actions/workflows/ci.yml)

MiniCore Runtime is a small Rust execution core for one live agent loop. Give
it host-owned history, one user input, a model, and a tool set. It runs model
requests and tool batches until the model finishes, streams progress, accepts
steering, configuration updates, cancellation, and interaction answers while
running, and returns one authoritative `LoopReport`. Sessions, persistence,
transcripts, providers, and workspaces stay with the host.
[MiniCore Agent](https://github.com/zqcli/minicore-agent) is the reference
host.

| | |
|---|---|
| Version | 0.7.3 |
| Rust | edition 2024, current stable |
| Async runtime | Tokio (the loop spawns one task) |
| Production dependencies | serde, serde_json, tokio, tokio-util, futures-util, thiserror, getrandom |
| License | MIT OR Apache-2.0 |

## Features

- **One-shot loop.** An `AgentLoop` runs exactly once. `join` and every
  handle's `wait` return the same `Arc<LoopReport>`.
- **Live control.** `LoopHandle` steers or updates the next request, answers
  approval and tool-input interactions, and cancels or shuts down the loop.
- **Pluggable seams.** Models, tools, tool policies, and prompt projection are
  traits (`Model`, `Tool`, `ToolPolicy`, `PromptProvider`). No provider SDK is
  bundled.
- **Model driver.** Bounded retries that honor provider retry hints, request and
  turn deadlines, and text normalization that never grows a delta.
- **Bounded by construction.** Limits cover messages, tool calls per response,
  tool output bytes, history size, and the event queue. Oversized tool output
  becomes a failed tool result instead of corrupting history.
- **Best-effort events.** A bounded stream reports dropped events in
  `dropped_before` and never blocks the loop.

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
use minicore_runtime::{AgentLoop, ExecutionConfig, LoopOptions, LoopRequest, UserInput};

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

let mut agent = AgentLoop::start(request, LoopOptions::default_checked()?)?;
let events = agent.take_events()?; // optional best-effort stream
let report = agent.join().await?; // Arc<LoopReport>; the host keeps it
```

Controlling a running loop (before `join`):

```rust
let handle = agent.handle();
handle.steer(UserInput::text("focus on the time")?)?; // applied at the next request
let revision = handle.update(new_config)?;            // applied at the next request
handle.cancel();                                       // or handle.shutdown()
```

Two runnable examples use fake adapters:

```bash
cargo run --example agent_loop    # text, tools, events, steer + update, cancel
cargo run --example memory_agent  # a multi-turn host built from one-shot loops
```

## Host Boundary

The Runtime executes one loop. The host owns everything around it:

- An `AgentLoop` runs exactly once; it cannot be resumed, loaded, or reopened.
- Sessions are owned by the host. There is no session type, lifecycle, or
  open/load/save surface.
- History is passed in by the host. The Runtime reads `HistoryItem`s, returns
  its new items in `LoopReport::appended`, and never persists anything.
- `LoopReport` is saved by the host. `wait` and `join` return the same
  `Arc<LoopReport>` for as long as the owner or any handle lives.
- Events are not authoritative. The stream is bounded and single-consumer;
  lost events, including internal model and tool progress, are counted in
  `dropped_before` and never reconstructed.
- Tool side effects are not atomic with host logging. A tool can succeed even
  when the host never sees its event or persists its result.
- `update` takes effect at the next model request. A revision is committed only
  when a real request is issued. The current tool batch always uses the
  snapshot of the request that produced it.
- `steer` takes effect at the next model request. An accepted steer is queued in
  memory and discarded if the loop ends any other way than `Completed` before
  that boundary. Steers fail with `InvalidInput` beyond `max_user_input_bytes`,
  `QueueFull`, `WaitingForInput` while an interaction is pending, or `NotActive`
  after cancellation or completion.
- An `update` does not keep the loop alive. The model's final response and any
  pending steers decide whether another request runs.
- `LoopReport.requests` counts issued requests. Driver retries stay inside one
  logical request, and the Runtime never retries a whole loop.
- Summaries, compaction timing, and persistence are host policies implemented
  through `PromptProvider`; the Runtime has no compaction scheduler.
- The Runtime needs a Tokio context: call `AgentLoop::start` inside a Tokio
  runtime.

## Public Surface

The crate root exports the loop types: `AgentLoop`, `LoopHandle`,
`LoopRequest`, `LoopOptions`, `LoopReport`, `LoopState`, `LoopEvent`,
`LoopEventStream`, outcomes, and their error types. Everything else lives in
focused modules:

| Module | Contents |
|---|---|
| `execution` | `ExecutionConfig`, `ConfigRevision`, `UserInput` |
| `history` | `HistoryItem` (user, assistant, tool result, summary), `HistoryView`, `estimate_history_bytes` |
| `model` | The `Model` trait, `ModelRequest`/`ModelMessage`, events, errors, provider replay |
| `prompt` | The `PromptProvider` trait and `DefaultPromptProvider` |
| `tools` | The `Tool` trait, `ToolSpec`, `ToolSet`, policies, interactions, progress |
| `limits`, `interaction`, `value`, `error` | Bounds, interaction DTOs, bounded text, diagnostics |

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
- `history::estimate_history_bytes` exposes the exact start-admission estimate.
  Configs discarded by finalization are dropped after report publication.

### 0.6.1

- Provider replay is bounded opaque metadata carried by
  `ModelEvent::ProviderReplay`, `ModelResponse`, `AssistantHistory`, and
  `ModelMessage::AssistantWithReplay`. Existing constructors and legacy JSON
  remain valid; history struct literals must initialize `provider_replay`.
- A replay envelope is at most 256 KiB of compact JSON, with depth 32 and 16,384
  payload nodes. The Runtime neither interprets nor truncates it. The host's
  history byte limit includes the envelope.
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
[0.7.0 release notes](docs/release-v0.7.md).

## License

Licensed under `MIT OR Apache-2.0`, as declared in `Cargo.toml`.
