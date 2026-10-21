# minicore-runtime 0.7.0

The one-loop host boundary, JSON representations and storage-independent design
remain unchanged. Public Rust API changes are listed in the
[README compatibility notes](../README.md#070).

## Changes

- Runtime normalizes model display text (CRLF/CR to LF, other controls except
  LF/TAB to a space), without expanding bytes. Assembly preserves CRLF continuity
  across text/reasoning deltas and ignores empty deltas. Tool text validation is
  consistent across input, progress and diagnostics.
- Large immutable text, reasoning, tool arguments and replay payloads share
  storage. Requests share message/tool slices and reuse the frozen tool catalog;
  history projection copies item handles rather than payload bytes.
- Model attempts receive child cancellation tokens cancelled on settlement,
  and reuse one pinned timeout. Interactions without a loop deadline wait until
  answered or cancelled, including beyond 24 hours.
- Finalization drops discarded configurations only after publishing the report.
  Destructor-panic regression tests preserve appended history and report identity.
- Cancellation immediately rejects later steering, updates and answers. Randomized
  multi-thread control races and a real runner panic verify completion identity.
- Ready model progress is drained fairly. Empty-stream EOF has Unknown delivery,
  and retry-backoff interruption preserves the previous model cause.
- `ToolSpec` fields are private with existing read-only accessors; registration and
  request construction trust checked specs instead of validating schemas again.
  `ToolSetError::InvalidSpec` is removed. Debug distinguishes replay-bearing assistants.
- Usage sums reported values independently per field; omitted values do not erase
  known counts. `LoopHandle::shutdown()` records host shutdown.
- `history::estimate_history_bytes` exposes the exact admission estimate, with
  allocation-free counting of compact tool-argument JSON.
- Model contracts are split by responsibility. Integration fixtures are shared
  under `tests/support/`; test binaries and acceptance function names are retained.

## Gates

The runtime gate is:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features
bash scripts/check-architecture.sh
```

Since 0.7.3 no minimum Rust version is tested; the gate runs on current stable.

The main suite has 278 tests: 81 unit and 197 integration tests in 11 binaries.
Two compile-fail doctests additionally protect the checked tool DTO boundaries.
The two examples compile as test targets. The acceptance checker preserves all
70 Passed rows and validates their source/test references. See the
[test inventory](../tests/README.md) for per-target counts.

Provider evidence is separate: `./scripts/check-provider-gate.sh` runs its own
fmt/test/clippy gate when that harness or its dependency changes.
It is no longer invoked by `scripts/check.sh`. Historical v0.4 release records
retain the gate and counts that applied at that release.

## Host Adaptation

Besides the Rust API updates in README, adapters should remove their own rejection
of CR/ESC model display text and let Runtime normalize it. Delta constructors
preserve raw text: adapters can chunk at the raw event byte limit on UTF-8
boundaries without expansion headroom or special CRLF handling. Hosts can replace
mirrored history-size accounting with the exported estimator and use handle
shutdown when closing a session. See the [adapter obligations](integration/host-boundary.md#adapter-obligations)
for cooperative futures, panic unwinding, dropped work and deadline ties.
