# Development Plan

## Completed Phases

These phases describe the v0.4 implementation history, not the current public
API. Later changes below remove its Runtime-owned steer queue and application quotas.

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

## 0.10.0 (prepared locally)

Application quotas are removed without compatibility modes: payload text/bytes,
JSON width/node counts, tool-call/round counts, history/prompts and observation
queue capacities. `Text` and model delta construction are infallible; default
summary projection retains all content. Public/internal observation queues are
unbounded and non-awaiting, with no lost-on-full behavior. Protocol grammar and
associations, actual model limits, timeout/retry behavior and parser-aligned
JSON stack safety remain. Current acceptance summaries retain historical test
identities. This local package preparation does not constitute publication.

Acknowledged complete-history handoffs and authoritative unfinished model output
are implemented, including independent history diagnostics and first-outcome
claims. See the [host handoff guide](integration/host-boundary.md#incremental-history-and-partial-output)
and [release notes](release-v0.7.md).

### Authoritative retry state follow-up

This separate work starts from quota closeout `cd59b6e` and does not rewrite or
publish its history. The first implementation adds synchronous driver-to-state
updates, source/monotonic retry timing and actual per-request invocation counts.
The existing retry policy, history acknowledgements, partial output and
independent primary/history failures remain unchanged.

Initial verification on Linux: `cargo test --all-features --locked --test
model_retry_state_contract` (3 passed), `cargo test --all-features --locked --lib
model::driver` (46 passed), and `./scripts/check.sh` (313 main-crate tests,
Clippy/rustdoc warnings denied, docs/architecture/acceptance and mutation
self-tests passed). Production implementation commit: `142412b`.

The follow-up contract suite now has 11 deterministic tests covering late state
reads, absent/slow observers, exact preceding errors through four total attempts,
clearing before success/failure/cancellation saving, tools and later preparation,
Retry-After/deadline bounds and partial-output preservation. An independent
checkout of `142412b` with these tests verifies three mutations go red:

- Remove the driver's `Finished` state handoff: the before-saving clear test
  times out instead of observing a clear state.
- Count `RetryWaiting.attempt` as already issued: cancellation during waiting
  reports two actual calls instead of one.
- Shorten the monotonic backoff by one second: the 30-second Retry-After test
  observes an early second model invocation.

Each mutation is restored before the next; all 11 tests pass after restoration.
The mutation targets are respectively
`failed_and_cancelled_started_retries_clear_before_history_handoff`,
`cancellation_during_wait_clears_before_saving_without_counting_planned_attempt`
and `retry_after_at_cap_uses_the_full_lower_bound`, run with
`cargo test --all-features --locked --test model_retry_state_contract <name> -- --exact`.

Final verification on Linux with Rust/Cargo 1.90.0:

- `./scripts/check.sh`: 321 tests (81 unit and 240 integration across 16 targets),
  fmt, Clippy and rustdoc with warnings denied, docs/architecture mutation
  self-tests and all 70 acceptance rows passed.
- `cargo test --all-features --locked --doc`: two compile-fail doctests passed.
- `cargo metadata --locked --format-version 1 --no-deps` and
  `cargo build --locked --release --examples`: passed.
- `cargo run --locked --release --example agent_loop` and
  `cargo run --locked --release --example memory_agent`: both examples executed
  successfully, including input/update and cancellation assertions.

No dependency or provider harness changed, so the opt-in provider gate was not
rerun. No live provider, macOS/Windows execution, Host integration, remote push
or tag is claimed by these Runtime checks. Watch coalescing is intentional;
consumers use the latest state and authoritative per-request report counts,
not an expectation of every intermediate transition.

### Actual retry-wait timing follow-up

The independent change based on `69faf91` adds
`LoopReport.model_retry_wait_elapsed: Arc<[Duration]>`, aligned with the existing
actual-attempt array. The driver measures Tokio monotonic elapsed immediately
around each retry-wait await and carries its sum through the private `Finished`
handoff, including waits interrupted by cancellation or a deadline. It does
not estimate time from a configured delay, wall clock or observer receipt.
No public state/event, retry policy, task, dependency, provider or host concern
is added. Existing failure causes, partial output and history barriers remain.

The 14-test retry-state target extends its existing fixtures to verify partial
wait cancellation, model/loop deadline crossing, multiple waits, delayed events
and no observation, exhausted retries, exact Retry-After, no-wait success and
failure, unstarted issued requests versus unissued preparations, per-request
reset and retained partials. Controlled model/tool/history delays are excluded
from the reported time. New delayed-repoll coverage records five and seven
seconds for two separate requests whose scheduled backoffs were each two
seconds.

Three restored timing mutations fail behavior assertions:

- Replace actual elapsed with scheduled delay: the delayed-repoll test reports
  `[2s, 2s]` instead of `[5s, 7s]`.
- Accumulate only after a successful wait: cancellation reports zero instead
  of the actual 750ms partial wait.
- Replace the running sum with the last wait: two waits report 4s instead of 6s
  with no event or state observer.

The mutation targets are respectively
`retry_wait_elapsed_counts_delayed_repolls_separately_for_each_request`,
`cancellation_during_wait_clears_before_saving_without_counting_planned_attempt`
and `no_observer_is_needed_for_retries_or_late_terminal_state`, run with
`cargo test --all-features --locked --test model_retry_state_contract <name> -- --exact`.
All 14 tests pass after all mutations are restored.

Final local verification on Linux with Rust/Cargo 1.90.0 and an exclusive target
folder for this checkout:

- `./scripts/check.sh`: all 324 main-crate tests pass (81 unit and 243 integration
  across 16 targets), along with fmt, Clippy/rustdoc with warnings denied,
  docs/architecture mutation self-tests, all 70 acceptance rows and diff checks.
- `cargo test --all-features --locked --doc`: two compile-fail doctests pass.
- `cargo test --all-targets --all-features --locked -- --list`: 324 unique tests;
  `tests/README.md` matches the per-target counts.
- `cargo test --all-features --locked --test model_retry_state_contract`: all
  14 tests pass on each of ten consecutive focused repeats.

These are fake-adapter Linux Runtime checks, not Host integration, live-provider
or macOS/Windows evidence. The provider harness and dependencies are unchanged;
no remote push or tag is performed by this local change.

### Historical post-handoff cleanup audit (before quota removal)

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
