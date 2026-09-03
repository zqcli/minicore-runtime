# MiniCore Runtime

MiniCore Runtime is a small Rust execution core for one live agent loop. It runs model/tool iterations, streaming, cancellation, interaction, steering, and request-boundary configuration updates. It does not own sessions, persistence, transcripts, providers, or workspaces.

## What the Runtime Does Not Own

An `AgentLoop` is a single, one-shot execution. The host owns everything around it:

- An `AgentLoop` runs exactly once; it cannot be resumed, loaded, or reopened.
- Sessions are owned by the host. The runtime has no session type, no session lifecycle, and no open/load/save surface.
- History is passed in by the host. The runtime reads `HistoryItem`s, appends its in-memory results to `LoopReport::appended`, and never persists anything.
- `LoopReport` is saved by the host. `wait`/`join` return the same `Arc<LoopReport>`: the report stays available through the loop's completion channel for as long as the owner or any handle lives, and every waiter receives the same `Arc`.
- Events are not authoritative. The event stream is a best-effort, bounded, single-consumer channel; dropped events are counted, never reconstructed. The report is the only authoritative outcome.
- Tool side effects are not atomic with host logging. A tool can run successfully (with side effects) even when the host never sees the corresponding streamed event or persisted log.
- `update` takes effect at the next model request. A config revision is committed only when a real model request is issued.
- The current tool batch uses the snapshot of the request that produced it. A config `update` or `steer` never retroactively changes an in-flight batch.
- `steer` takes effect at the next model request. A successful call means the steer was accepted into this process-local queue and is applied at the next request boundary; if the loop is cancelled, shut down, or the process exits before that boundary, the queued steer may be discarded and will not appear in `LoopReport::appended`. Steers queue up to `max_pending_steers` (`QueueFull`), are rejected if an interaction is pending (`WaitingForInput`) or after finalization (`NotActive`), and appear as `Steering` user items at the next request boundary.
- A config `update` does not keep the loop alive. It only reconfigures the next request; whether the loop continues is decided by the model's final response and pending steers.
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