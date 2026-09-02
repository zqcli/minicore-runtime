# Test Inventory

Test targets for the v0.4 core. Every target is deterministic: notify /
oneshot / watch / `start_paused` + `advance` only, no long sleeps.

| Target | Owns | Scope |
| --- | --- | --- |
| `src/*` unit `tests` (incl. `src/agent_loop/runner/tests.rs`) | module | value validation, control state machine, runner slices |
| `tests/p1_v04_loop_dtos.rs` | integration | DTO construction / serialization for loop DTOs |
| `tests/p2_agent_loop.rs` | integration | the flexible agent-loop contract E2E |

## Contract Coverage (`tests/p2_agent_loop.rs`)

- Text loop: final `Stop` -> `Completed`, report `requests`/`appended`
  shape.
- Tool loop: sequential tool calls, `ToolResult` append, tool round limit,
  tool unavailable / invalid invocation / interrupted batch.
- Steer: accepted steers apply as `Steering` at the next request; queue
  full; `NotActive` after final; steer-vs-final race (final seal wins or
  steer keeps alive).
- Update: accepted config applies at the next request; invalid config
  rejected; update does not extend a final; revision commits only at a true
  issue boundary.
- Cancellation / deadline: `CancelReason::User`/`OwnerDropped`/`Deadline`;
  cancel-after-seal; late `wait`ers see `CompletionClosed`; pending steers
  dropped on cancel.
- Prompt: provider failure / panics / timeout as `Failed(Prompt)`; empty or
  over-limit prompts rejected for every provider; default provider summary
  truncation at `MAX_MODEL_MESSAGE_TEXT_BYTES`; invalid message via public
  enum variant.
- Events: best-effort stream, `dropped_before`, `Finished` envelope.
- Interactions: approval and tool-input flows, kind mismatch, deadline
  while waiting.

## Running

```text
cargo test --locked --all-targets --all-features
```

The full gate is `./scripts/check.sh` (fmt, tests, clippy `-D warnings`,
rustdoc `-D warnings`, docs/architecture checks, provider-gate harness) plus
`./scripts/check-msrv.sh` and `cargo metadata --locked`.