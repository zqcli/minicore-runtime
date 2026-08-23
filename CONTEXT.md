# Current Implementation Context

> Transitional v0.2 checkpoint. P3-D has removed public registries and concrete
> model/tool adapters; the current public Port boundaries are
> described by the v0.3 README and architecture/module map.

## Checkpoint

This repository retains the v0.2 typed runtime core as migration evidence. The current authority is the source tree and the documents linked from [docs/README.md](docs/README.md). Pre-reset design material is historical and lives under `docs/archive/v2/pre-reset/`.

The crate is Rust 2024 with Rust 1.85 as its MSRV. The default build is offline. External model networking and the Tokio runtime handle are host responsibilities.

## Ownership Map

- `config`: checked `RuntimeConfig`, `SessionConfig`, `RetryPolicy`, paths, text, capacities, and bounds.
- `ids`: checked session, turn, interaction, and tool-call identifiers.
- `model`: direct streaming `Model` Port, checked descriptors/contexts/requests/events/errors, shared response DTOs, and private legacy batch lookup.
- `tools`: immutable `ToolSet`, async policy Port, interaction/tool DTOs, and private legacy runner scaffolding.
- `workspace`: one capability-backed root, relative-path validation, bounded file operations, directory enumeration, and workspace shutdown.
- `prompt`: private prompt assembly and compaction planning.
- `agent`: private turn runner, model/tool ordering, retries, cancellation, and compaction integration.
- `storage`: private durable session configuration, timestamps, root lock, store worker, conversation log, replay, repair, and transcript source.
- `session`: actor, mailbox, observation, terminal/lifecycle orchestration, snapshots, events, and public transcript projection.
- `runtime`: public orchestration and session residency manager.

The public root exposes canonical `config`, `error`, `event`, `ids`, `model`, `session`, `tools`, and typed Port modules. `agent`, `prompt`, `runtime`, `storage`, and `workspace` remain private. Storage workers, legacy lookup, actor commands, and prompt internals are not public extension seams.

## Core Invariants

- The transitional `Runtime` owns one storage root plus private legacy model/tool collections until P4/P5 replace it.
- One loaded session coordinates one storage-owned conversation log, one actor, one bounded command mailbox, one workspace, and one active turn at most.
- Session states are exactly `Idle`, `Running`, `WaitingForInput`, and `Closing`.
- Submit and answer use the same bounded mailbox. Cancellation is an out-of-band request and never waits behind normal work.
- A response to an interaction has one first-winner claim. The actor persists the interaction before resuming model work.
- A terminal outcome is persisted by the actor. Cancellation, denial, failure, and close are never represented as successful completion.
- Model tool-call indexes are ordered within each response round. Tool results are matched by checked call identifiers.
- Final sessions bind one `Arc<dyn Model>` directly; legacy lookup remains private only for old actor tests.
- Only explicitly retryable `NotStarted` model failures may use the configured logical retry policy.
- Compaction appends a summary at a checked boundary. It never rewrites source history or invents an incomplete tool exchange.
- Snapshots publish before event delivery. A lagged subscriber must resynchronize from a fresh snapshot/subscription baseline.
- `Runtime::shutdown()` starts one owner-tracked cleanup operation and returns its authoritative result. `Drop` may start it but cannot report the result.

## Public API

`Runtime::open(config, handle)` is asynchronous and returns `RuntimeError`. This is private migration implementation; final P4 bindings will carry one direct model and immutable tool set.

The session methods are:

- `create_session(SessionConfig) -> Result<SessionId, SessionError>`
- `load_session(SessionId) -> Result<(), SessionError>`
- `close_session(SessionId) -> Result<(), SessionError>`
- `delete_session(SessionId) -> Result<(), SessionError>`
- `list_sessions() -> Result<Vec<SessionSummary>, SessionError>`
- `submit(SessionId, String) -> Result<TurnId, SessionError>`
- `answer(SessionId, InteractionId, UserAnswer) -> Result<(), SessionError>`
- `cancel(SessionId) -> Result<(), SessionError>`
- `snapshot(SessionId) -> Result<SessionSnapshot, SessionError>`
- `subscribe(SessionId) -> Result<SessionEventStream, SessionError>`
- `transcript(SessionId, Option<u64>, usize) -> Result<TranscriptPage, SessionError>`
- `shutdown() -> Result<(), RuntimeError>`

`SessionError` is authoritative for session admission, lifecycle, interaction, observation, and transcript operations. It distinguishes not found, already loaded, busy, closing, interaction mismatch, invalid input, unavailable, and internal failure. `RuntimeError` is reserved for invalid runtime configuration, closing, and runtime internal failure.

## Persistence

A runtime data directory contains `runtime.lock` and a `sessions/` directory. Each session directory is named by its checked `ses_` identifier and contains exactly `session.json` and `conversation.jsonl`.

The store worker bootstraps the directory, owns the exclusive root lock, removes orphan temporary creation directories, and serializes filesystem work. Session creation writes `session.json` and an empty conversation file in a temporary directory, flushes/synchronizes them, and atomically renames the directory into the session namespace.

The session JSON format is version 2. It stores checked configuration and has a one-megabyte bounded serialized size. Conversation JSONL is append-only, one complete semantic entry per newline, with a one-megabyte line bound, a one-gigabyte file bound, and a one-million complete-entry bound. A final incomplete tail is repaired by truncation. A complete malformed or semantically inconsistent line reports located corruption and is not silently skipped.

Restart recovery appends failure ToolResult entries for unresolved calls and a `CancelledByRestart` terminal entry using the fixed text `cancelled by restart`. Existing source entries remain in the file. Transcript paging projects all six durable conversation variants, including non-model-visible interaction entries.

## Safety

Workspace access starts from one configured absolute root. File and directory tools use captured capability-relative paths, reject lexical escape and symlink escape, respect `ReadOnly` versus `ReadWrite`, and bound bytes, entries, names, and output. Workspace shutdown closes admission and joins its owner-tracked worker.

The process tool is deliberately different: it starts one direct child with structured arguments and a validated ambient host cwd. It never invokes a shell. `ProcessPolicy` controls enabled state, executable allowlist, inherited environment allowlist, timeout, stdout bound, and stderr bound. The child environment is cleared before explicitly permitted values are added. The runtime does not claim an OS process sandbox or process-tree cleanup guarantee.

Host Model adapters own external networking and cleanup. Core errors carry `NotStarted`, `Started`, or `Unknown` delivery state so the future ModelDriver cannot blindly retry an operation that may already have started.

## Verification

Deterministic gates are offline:

- `cargo fmt --all -- --check`
- `cargo test --all-targets --locked`
- `cargo clippy --all-targets --all-features --locked -- -D warnings`
- `cargo test --locked --manifest-path provider-gate/Cargo.toml --all-targets`
- `./scripts/check.sh`
- `./scripts/check-msrv.sh`
- `cargo test --locked --test v2_acceptance`

Root concrete model/live-network tests were removed in P3-D. The independent provider-gate package remains deterministic historical evidence; no network access is required by root tests.

## Intentional Limits

- There is no model registry or default adapter catalog.
- There is no shell command interface.
- There is no process-tree sandbox claim.
- There is no automatic migration from the historical Store V1 layout; migration is an explicit offline host operation.
- There is no compatibility wrapper for the removed public surface.
- There is no detached task ownership: blocking workers, actor joins, provider futures, and shutdown work remain owner-tracked.
- Current docs describe the v0.2 core. Historical design rationale remains available only for reference under the pre-reset archive.

## Evidence Map

- Public surface: `src/lib.rs`, `src/config.rs`, `src/error.rs`, `src/runtime/runtime_impl.rs`.
- Model contract: `src/model/model.rs`, `src/model/response.rs`, `src/model/types.rs`, and `tests/model_port_contract.rs`.
- Tool contract: `src/tools/tool.rs`, `src/tools/set.rs`, `src/tools/policy.rs`, and their focused contract tests.
- Storage contract: `src/storage/store.rs`, `src/storage/conversation.rs`, `src/storage/conversation/codec.rs`, and `src/session/transcript.rs`.
- Lifecycle contract: `src/session/actor.rs`, `src/session/command.rs`, and `src/runtime/session_manager.rs`.
- Acceptance contract: focused v0.3 Port tests now; owner/runtime acceptance remains deferred to P4/P5.
- Documentation validation: `scripts/check_docs.py` checks current authority plus selected non-pre-reset evidence.
