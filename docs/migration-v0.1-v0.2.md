# MiniCore Runtime v0.1 → v0.2

**Status:** P8 public switch, legacy production deletion, fixture archival, and AT-20 closure complete; P9 documentation and dependency cleanup pending
**Change type:** breaking Core Reset; no v0.1 Wire/API compatibility promise

P1–P8 are complete in the v0.2 core. The public typed `Runtime` surface, explicit provider/tool registries, capability-relative file tools, structured `run_command`, durable transcript/compaction behavior, cancellation, and AT-01..AT-20 are active in `tests/v2_acceptance.rs`; the acceptance target has no ignored cases. P9 is reserved for documentation and dependency cleanup.

## Branch Authority

For this branch, `minicore-runtime-v0.2-refactor-spec.md` and this migration note are the target contract. They supersede the current V2 implementation descriptions in `README.md`, `docs/architecture.md`, `docs/modules/`, the development plan, and ADRs where those documents describe the pre-reset runtime. Those files remain baseline evidence until the later documentation phase rewrites or archives them; they are not additional v0.2 requirements.

The fixed baseline is commit `5088bc254548b3e80e87179898ebb7abbea52c7d` on `dev`. P0 does not change production code or the baseline behavior.

## Product Reset

v0.2 keeps one coding-agent loop: Session lifecycle, one active Turn per Session, streaming model calls, ordered model/tool rounds, `read_file`, `list_directory`, `write_file`, `run_command`, `ask_user`, cancellation, typed snapshots/events, append-only conversation recording, restart recovery, and compaction.

The default core removes Agent durable entities and revisions, Session CAS/publication machinery, Fork/Archive, queued Steer/FollowUp, runtime command/query routing, Wire V1, dynamic shared-resource/security recovery, skills/source ecosystems, and other platform management surfaces. Optional compatibility belongs outside the core in a separate adapter crate or migration program.

## Public Shape

The target library exposes typed `Runtime`, `RuntimeConfig`, `SessionConfig`, typed Session errors/events/snapshots, model provider registration, and a generic `ToolRegistry`. The target Session states are only `Idle`, `Running`, `WaitingForInput`, and `Closing`.

The old `MiniCoreRuntime::{dispatch, query, snapshot, subscribe, session_transcript}` transport surface and its Wire carriers are not v0.2 API. The replacement is direct typed operations: create/load/close/delete/list Session, submit, answer, cancel, snapshot, subscribe, transcript, and shutdown.

`AT-17` is tested through public consequences: after `close_session` returns, the old loaded handle is unusable, the active Turn has been cancelled or boundedly aborted, and the Session can be loaded again. Tests must not inspect `SessionManager` internals.

## Persistence

v0.2 uses `<data_dir>/sessions/<session-id>/session.json` and `conversation.jsonl`. `session.json` always stores the absolute `workspace_root` path; the alternative `workspace_key -> PathBuf` resolver mode is rejected for v0.2 and is not implemented in parallel. Store V1 generation/lease/publication data is not read on the Runtime hot path. Existing data requires a one-shot offline migration.

The JSONL contract is append-only, one complete semantic entry per newline, monotonic sequence numbers, tolerant final partial-line recovery, explicit middle-file corruption, no token-delta persistence, and summary append rather than history rewrite.

## Safety and Scope

Workspace file access remains root-relative and capability-relative, rejects absolute paths, traversal, NULs, and symlink escape, and keeps final-component no-follow write protection. `run_command` is structured `program + args`, never a default shell string; it is explicit opt-in, clears the environment by default, applies program/env/timeout/output limits, and does not claim to sandbox the child process.

The v0.2 acceptance matrix is AT-01..AT-20. AT-20 is an active static architecture gate, not a behavioral test: it verifies that the deleted legacy paths and structured legacy tokens are absent after the reset.

## Migration Policy

P0 records the baseline and installs the acceptance inventory only. P1–P7 built the new implementation beside the old one; P8 completed the public switch, legacy production deletion, fixture archival, and AT-20 closure. No compatibility shim should be added merely to preserve old tests. P9 covers documentation and dependency cleanup. Old tests are retained only when their scenario protects a v0.2 user-visible guarantee; otherwise the scenario is rewritten against the typed seams.
