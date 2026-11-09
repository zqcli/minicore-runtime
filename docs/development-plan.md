# Development Plan

## Completed Phases

These phases describe the v0.4 implementation history, not the current public
API. Later changes below remove its Runtime-owned steer queue and history limits.

- **Phase 1** `feat(v0.4): introduce loop history and execution configuration` — `HistoryItem`/`HistoryView` and `ExecutionConfig` as the host boundary inputs.
- **Phase 2** `feat(v0.4): run one agent loop without session ownership` — one-shot `AgentLoop`, runner-owned completion, single-CAS cancel/final state machine, `MAX_MODEL_RETRY_ATTEMPTS=4`.
- **Phase 3** `feat(v0.4): apply execution updates at request boundaries` — `LoopHandle::update`, update/seal linearization, genuine issue-boundary revision commit.
- **Phase 4** `feat(v0.4): steer active loops at request boundaries` — `LoopHandle::steer`, pending steer queue, final-seal race handling.
- **Phase 5** `refactor(v0.4): remove session storage and durable conversation ownership` — deleted `session`, `storage`, `conversation`, `agent`, `compaction`, `context`, old `prompt`/`config`; migrated `UserInput`/`Usage`; 181 files, net −36k lines.
- **Phase 6** `refactor(v0.4): replace context and durable compaction with prompt providers` — `PromptProvider` seam + `DefaultPromptProvider`, runner-side `max_prompt_messages` enforcement, shared `MAX_MODEL_MESSAGE_TEXT_BYTES`.
- **Phase 7** `refactor(v0.4): converge modules around the agent loop boundary` — runner split (`runner/model.rs`, `runner/tools.rs`), v0.4 documentation and examples, architecture-gate rewrite, dead code and dependency cleanup.
- **Phase 8** `test(v0.4): close the flexible agent loop contract` — contract close-out (reasoning channels, event-free and multi-batch cancel semantics, policy snapshot swaps, history limits and delta-only reports, malformed-response rejection, loop-level retry, shared-resource isolation, control-layer CAS tests), the V4-001..V4-070 [acceptance matrix](acceptance-v0.4.md) with its gate, and the [release documentation](release-v0.4.md).

## v0.7.0

Completed runtime text normalization, shared request/history payloads, per-attempt
cancellation, unbounded interaction waits without a loop deadline, post-publication
config disposal, usage accounting, shutdown handles, model module separation and
shared test fixtures. See [release notes](release-v0.7.md) for compatibility and gates.

## v0.8.0–v0.9.2

- 0.8.0 removed Runtime whole-history and prompt-message count limits; hosts
  own context sizing and compaction while per-item limits remain.
- 0.9.0 replaced `LoopHandle::steer` with host-owned `LoopInputs` pulled at the
  A/F preparation boundaries. A no-tool response ends the loop; remaining
  input belongs to a successor loop the host starts.
- 0.9.1–0.9.2 preserve that production code and API. Host-observation tests
  cover late watches, terminal event loss and input-boundary identity; current
  documentation distinguishes those contracts from historical release work.

## Unreleased 0.10.0

Acknowledged complete-history handoffs and authoritative unfinished model output
are implemented, including independent history diagnostics and first-outcome
claims. See the [host handoff guide](integration/host-boundary.md#incremental-history-and-partial-output)
and [release notes](release-v0.7.md).

### Post-handoff cleanup audit

The 2026-11-09 audit compared `3fd5ecd..49a6e8b` and checked production, test
and example uses. No production function was made obsolete by those changes.
The cleanup removes the old user-history collection and redundant no-tool-result
scan from the unsupported-finish regression: its exact `[User, Assistant]`
match now checks both properties without allocation. The acceptance-linked test
name is unchanged. Current host guidance no longer suggests deriving durable
records from events or dropping history acknowledgements on cancellation.

The following paths remain required:

- `history: None` and full `LoopReport.appended` preserve join-only hosts and
  let callback users reconcile by accepted position, not saved position.
- The final tail handoff covers prompt/model failure, usage overflow and
  unhanded inputs or settlements; earlier handoffs cannot replace it.
- `begin_final`, pending-config disposal and the update gate retain normal
  completion ordering and post-publication destruction. Non-completed claims
  preserve the primary outcome while owned saving settles.
- Driver failure mapping still preserves structured model errors and deadline
  provenance after moving unfinished assembly into `LoopReport.partial`.
- The no-tool `ToolCalls` rejection already existed at both assembler and
  runner boundaries in `3fd5ecd`; it is not newly obsolete handoff code. The
  adjacent `Unknown` failure remains reachable and must retain complete history.

Verification on Linux with Rust/Cargo 1.90.0:

- `./scripts/check.sh`: 307 tests, Clippy/rustdoc warning checks, docs and
  architecture mutation self-tests, and 70 acceptance rows passed.
- Focused `history_handoff_contract`, `model_partial_contract`,
  `host_observation_contract` and `p3_agent_loop_closeout` targets: 51 passed.
- `cargo test --all-features --locked --doc`: 2 passed.
- `cargo metadata --locked --format-version 1 --no-deps` and
  `cargo build --locked --release --examples`: passed.

Provider evidence and dependencies are unchanged, so the opt-in provider gate
was not run. macOS and Windows CI were not run locally.

## Remaining

No planned phases remain. Maintenance keeps the gates in the next section
and the acceptance matrix at `scripts/acceptance_v04.json` in sync.

## Maintenance Gates

Every phase ships green against:

- `cargo fmt --check`
- `cargo test --locked --all-targets --all-features`
- `cargo clippy --locked --all-targets --all-features -- -D warnings`
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features`
- `cargo metadata --locked`
- `./scripts/check.sh` (includes docs/architecture/acceptance gates)

Provider-gate is independent: `./scripts/check-provider-gate.sh` runs its fmt,
tests and clippy. Run it when changing provider evidence or upgrading its
dependency; it is not part of the main runtime gate.
