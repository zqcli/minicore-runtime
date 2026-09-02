# Module Map

Physical source ownership for the v0.4 core. Public re-exports are listed at
the crate root (`src/lib.rs`); every other module is reached through them.

| Source path | Owns | Public boundary |
| --- | --- | --- |
| `src/agent_loop/mod.rs` | `AgentLoop`, `LoopRequest`, `LoopOptions`, `LoopReport`, errors | Re-exported at root |
| `src/agent_loop/handle.rs` | `LoopHandle` (steer/update/cancel/state/answer) | Re-exported at root |
| `src/agent_loop/control.rs` | linearized update/steer/begin-final state, interaction slots | crate-private |
| `src/agent_loop/event.rs` | `LoopEvent(Envelope/Stream)`, best-effort sink | Re-exported at root |
| `src/agent_loop/state.rs` | `LoopState`, `LoopStatus` | Re-exported at root |
| `src/agent_loop/runner.rs` | main loop, control, final, finish reporting | crate-private |
| `src/agent_loop/runner/model.rs` | prompt preparation + model driver (private impl slice) | crate-private |
| `src/agent_loop/runner/tools.rs` | policy, interaction, tool execution (private impl slice) | crate-private |
| `src/execution.rs` | `ExecutionConfig`, `ConfigRevision`, `UserInput` | Re-exported at root |
| `src/history.rs` | `HistoryItem`, `HistoryView`, per-item structs | Re-exported at root |
| `src/prompt.rs` | `PromptProvider` seam, `DefaultPromptProvider` | `prompt` module |
| `src/model/` | `Model` seam, `ModelDriver`, typed request/response | `model` module |
| `src/tools/` | `Tool` seam, `ToolSet`, policies, interactions | `tools` module |
| `src/limits.rs` | `LoopLimits` | Re-exported at root |
| `src/usage.rs` | `Usage` / `UsageAccumulator` | `Usage` re-exported |
| `src/interaction.rs` | `PendingInteraction`, `InteractionAnswer`/`Kind` | Re-exported at root |
| `src/ids.rs` | `LoopId`, `InteractionId`, `ToolCallId` | Re-exported at root |
| `src/value.rs` | `BoundedText`, byte budgets | `value` module |
| `src/time.rs` | deadline sources | crate-private |
| `src/error.rs` | `DiagnosticCategory/Code/Summary` | Re-exported at root |
| `src/port_call.rs` | per-port cancel/timeout/panic isolation | crate-private |

## Test Ownership

- `src/*/tests.rs` / `src/agent_loop/runner/tests.rs` — unit tests inside
  the owning module.
- `tests/p1_v04_loop_dtos.rs` — DTO/serialization contract evidence.
- `tests/p2_agent_loop.rs` — end-to-end loop contract evidence (text, tools,
  steer/update/cancel, deadlines, report semantics).
- `tests/p2_*` follow the same pattern for further contract domains, if
  added; integration test files live directly in `tests/`.

## Boundaries

- `agent_loop` is the only module that spawns tasks (one per loop) and
  touches `tokio::sync::watch`/`mpsc` for loop plumbing.
- No module owns a session, a store, or a provider implementation.
- Runner implementation slices (`runner/model.rs`, `runner/tools.rs`) are
  private to `agent_loop::runner` and exist only to keep physical files
  focused; they are not a layering boundary.