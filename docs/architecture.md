# Architecture

## Positioning

MiniCore Runtime is a small Rust execution core for **one live agent loop**.
It runs model/tool iterations, streaming, cancellation, interaction,
host-supplied inputs, and request-boundary configuration updates. It does not own
sessions, persistence, transcripts, providers, or workspaces.

The single architectural invariant: **there is one boundary — the running
`AgentLoop` — and everything durable or session-scoped lives on the host side of
it.** The runtime consumes host-provided history and a host-built execution
configuration, and it exposes current state, a final report and best-effort
events. The host decides what to persist, where, and when.

```
        host history            ExecutionConfig
             |                        |
             v                        v
        LoopRequest --------------> AgentLoop
                                      |  one tokio task
        LoopReport  <---------------- |  (runner.rs + model.rs + tools.rs)
        event stream <--------------- |
                 (best-effort)
```

## Ownership

- **Host owns sessions.** There is no session type anywhere in the crate.
  `AgentLoop::start` allocates a fresh `LoopId` and runs once.
- **Host owns history.** `LoopRequest::history: Arc<[HistoryItem]>` is a
  shared immutable input. The loop appends only in-memory results, exposed as
  `LoopReport::appended`.
- **Host owns persistence.** Nothing in the runtime writes a file, a store,
  or a log. The complete `LoopReport`, including outcome, counters and
  appended items, is authoritative; its persistence is entirely the host's call.
- **Host owns user inputs.** The host implements `LoopInputs` and passes it to
  `AgentLoop::start`; the loop pulls inputs at request boundaries and the
  host's queue, ordering and validation stay on its side.
- **Host owns providers and workspaces.** `Model`, `Tool`, `ToolPolicy` and
  `PromptProvider` are host- or host-adapter-supplied seams.

## Module Map

- `agent_loop/` — the loop: `AgentLoop`, `LoopHandle`, `LoopInputs`,
  `LoopRequest`, `LoopOptions`, control linearization, best-effort events, state, and the
  runner (`runner.rs`, `runner/model.rs`, `runner/tools.rs`).
- `execution.rs` — immutable `ExecutionConfig` (model, reasoning, tools,
  policy, prompt provider) and `UserInput`.
- `history.rs` — host-facing `HistoryItem` vocabulary and borrowed
  `HistoryView`.
- `prompt.rs` — `PromptProvider` seam and `DefaultPromptProvider`.
- `model/` — `Model` seam, typed `ModelRequest`/`ModelMessage`/responses,
  and the delivery-aware `ModelDriver`.
- `tools/` — `Tool` seam, `ToolSet`, `ToolPolicy`, interactions, progress.
- `usage.rs`, `interaction.rs`, `ids.rs`, `value.rs`,
  `time.rs`, `error.rs`, `port_call.rs` — validation, accounting, id and
  value types, and per-port isolation.

## Runner Shape

- One `tokio::spawn` per loop; the completion sender lives in that task.
- `runner.rs` keeps the main loop, control/final handling, and finish
  reporting; `runner/model.rs` holds prompt preparation and the model
  driver; `runner/tools.rs` holds batch settlement, policy, interaction, and tool execution.
- Every control touch is a short critical section. Public and internal progress
  queues are unbounded; sends do not await, and no full/drop path exists. The
  optional public sink returns unit and ignores disconnection.
- `LoopControl` retains the latest `LoopState` in a watch channel shared by
  `state` and `watch_state`; this is independent of the optional event stream.
  Completion has a separate runner-owned sender and returns the final report.

## Validation Boundary

Runtime imposes no application limits on payload bytes/nodes, response/tool
counts, rounds, history, prompts or observation queue sizes, and performs no
quota-driven truncation. Typed protocol grammar, associations, real model
capabilities/token limits and timeout/retry behavior remain unchanged.
JSON nesting follows the locked serde_json parser's default stack-safety guard:
127 arrays/objects are accepted, 128 rejected. Width and node count are unrestricted.
No session, storage, queue policy or tool-parallelism mode is added.

## Dependency Direction

`agent_loop` depends on `model`, `tools`, `prompt`, `history`,
`execution`, `usage`, `interaction`, `ids`, `value`, `time`,
`error`, `port_call`. None of those modules depends on `agent_loop`. Every
seam points outward: the runtime defines traits, hosts implement adapters.

## Quality Gates

`./scripts/check.sh` covers the runtime, documentation and architecture on
current stable Rust. The standalone provider harness has no production
dependency edge: use `./scripts/check-provider-gate.sh` when changing provider
evidence or its dependency.
It is intentionally outside the runtime gate.

## What v0.3 Had That v0.4 Removed

Session runtime, session manifest/state/log, conversation ledger and durable
compaction, storage, agent definitions, workspace capability models,
provider gates in the production baseline, and the old
session-owned history model. All of that is either gone or, where evidence
was still valued, preserved in the standalone `provider-gate` harness and
the archived docs. See [the migration](migrations/v0.3-to-v0.4.md).
