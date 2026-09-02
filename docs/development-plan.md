# Development Plan

## Completed Phases

- **Phase 1** `feat(v0.4): introduce loop history and execution configuration` — `HistoryItem`/`HistoryView` and `ExecutionConfig` as the host boundary inputs.
- **Phase 2** `feat(v0.4): run one agent loop without session ownership` — one-shot `AgentLoop`, runner-owned completion, single-CAS cancel/final state machine, `MAX_MODEL_RETRY_ATTEMPTS=4`.
- **Phase 3** `feat(v0.4): apply execution updates at request boundaries` — `LoopHandle::update`, update/seal linearization, genuine issue-boundary revision commit.
- **Phase 4** `feat(v0.4): steer active loops at request boundaries` — `LoopHandle::steer`, pending steer queue, final-seal race handling.
- **Phase 5** `refactor(v0.4): remove session storage and durable conversation ownership` — deleted `session`, `storage`, `conversation`, `agent`, `compaction`, `context`, old `prompt`/`config`; migrated `UserInput`/`Usage`; 181 files, net −36k lines.
- **Phase 6** `refactor(v0.4): replace context and durable compaction with prompt providers` — `PromptProvider` seam + `DefaultPromptProvider`, runner-side `max_prompt_messages` enforcement, shared `MAX_MODEL_MESSAGE_TEXT_BYTES`.
- **Phase 7** `refactor(v0.4): converge modules around the agent loop boundary` — runner split (`runner/model.rs`, `runner/tools.rs`), v0.4 documentation and examples, architecture-gate rewrite, dead code and dependency cleanup.
- **Phase 8** `test(v0.4): close the flexible agent loop contract` — contract close-out (reasoning channels, event-free and multi-batch cancel semantics, policy snapshot swaps, history limits and delta-only reports, malformed-response rejection, loop-level retry, shared-resource isolation, control-layer CAS tests), the V4-001..V4-070 [acceptance matrix](acceptance-v0.4.md) with its gate, and the [release documentation](release-v0.4.md).

## Remaining

No planned phases remain. Maintenance keeps the gates in the next section
and the acceptance matrix at `scripts/acceptance_v04.json` in sync.

## Maintenance Gates

Every phase ships green against:

- `cargo fmt --check`
- `cargo test --locked --all-targets --all-features`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features`
- `./scripts/check-msrv.sh` (Rust 1.85)
- `cargo metadata --locked`
- `./scripts/check.sh` (includes docs/architecture gates and provider-gate harness)

Provider-gate remains a standalone evidence harness and is exercised by
`check.sh` without modifying its source.