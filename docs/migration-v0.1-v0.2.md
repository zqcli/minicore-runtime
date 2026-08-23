# v0.1 to v0.2 Migration

> Historical migration record. The v0.3 reset is breaking and does not add a
> compatibility wrapper. P3-B removes the public Runtime/ToolRegistry facade;
> P3-D removes model registries, concrete network adapters, and transport APIs;
> P4-B deletes the remaining private multi-session Runtime implementation.

## Status

This is the final breaking migration guide for the v0.2 core. P8 reset closure is complete: the current crate contains the typed Runtime graph, the historical pre-reset authority is archived, and the acceptance architecture gate is active. The typed `Runtime` replaces the removed platform command/query surface. There are no compatibility wrappers or root aliases for the removed surface.

P9 documentation status: complete. The lockfile was regenerated and reviewed remotely with Rust 1.85 and stable Cargo, the final deterministic gates passed, and the [release-readiness result](release-v0.2-core-reset.md) records the current contract and deferred host work.

## What Changes

The v0.2 core is a library runtime with one durable actor per loaded session and one active turn per loaded session. It is not a transport implementation and it does not preserve the v0.1 platform surface.

| v0.1 surface | v0.2 result |
| --- | --- |
| `MiniCoreRuntime::dispatch(CommandRequest)` | Removed. Use typed `Runtime` methods. |
| `MiniCoreRuntime::query(RuntimeQuery)` | Removed. Use `snapshot`, `list_sessions`, and `transcript`. |
| `MiniCoreRuntime::snapshot` / `subscribe` transport routes | Removed as transport routes. Use typed `Runtime::snapshot` and `Runtime::subscribe`. |
| `MiniCoreRuntime::session_transcript` | Replaced by typed `Runtime::transcript`. |
| Wire V1 DTOs and carriers | Removed from the core source graph. Historical fixtures are archived. |
| DurableState generations and Store V1 leases | Removed from the v2 persistence contract. The v2 store is flat and lock-owned. |
| Agent entities and revision management | Removed from the v2 core. A session selects one model and one workspace directly. |
| Fork and Archive lifecycle | Removed. Session lifecycle is create/load/close/delete. |
| Steer and FollowUp queues | Removed. A session accepts one submitted input at a time through its bounded mailbox. |
| Generic platform command/query ownership | Removed. Runtime methods and session actor commands are the current boundary. |
| Compatibility aliases and migration shims | Not provided by this crate. |

Historical prose for these surfaces is under [`docs/archive/v2/pre-reset/`](archive/v2/pre-reset/). It explains the previous design and is not a current contract.

## Current Runtime API

The current public methods have these exact call styles and result types:

| Method | Call style | Exact result |
| --- | --- | --- |
| `Runtime::open(config: RuntimeConfig, runtime: tokio::runtime::Handle)` | async | `Result<Runtime, RuntimeError>` |
| `Runtime::create_session(config: SessionConfig)` | async | `Result<SessionId, SessionError>` |
| `Runtime::load_session(session_id: SessionId)` | async | `Result<(), SessionError>` |
| `Runtime::close_session(session_id: SessionId)` | async | `Result<(), SessionError>` |
| `Runtime::delete_session(session_id: SessionId)` | async | `Result<(), SessionError>` |
| `Runtime::list_sessions()` | async | `Result<Vec<SessionSummary>, SessionError>` |
| `Runtime::submit(session_id: SessionId, input: String)` | async | `Result<TurnId, SessionError>` |
| `Runtime::answer(session_id: SessionId, interaction_id: InteractionId, answer: UserAnswer)` | async | `Result<(), SessionError>` |
| `Runtime::cancel(session_id: SessionId)` | sync | `Result<(), SessionError>` |
| `Runtime::snapshot(session_id: SessionId)` | sync | `Result<SessionSnapshot, SessionError>` |
| `Runtime::subscribe(session_id: SessionId)` | sync | `Result<SessionEventStream, SessionError>` |
| `Runtime::transcript(session_id: SessionId, after_seq: Option<u64>, limit: usize)` | async | `Result<TranscriptPage, SessionError>` |
| `Runtime::shutdown()` | async | `Result<(), RuntimeError>` |

`RetryPolicy::new(max_attempts, base_delay)` returns `Result<RetryPolicy, RetryPolicyError>`. Only a checked `RetryPolicy` is accepted by `RuntimeConfig`; invalid retry attempt counts or delays are rejected before `RuntimeConfig::new` is called. `ConfigError` describes invalid checked paths, text, capacities, and runtime/session bounds; it does not replace `RetryPolicyError`.

`RuntimeError` describes runtime configuration, closing, and runtime-internal failures. `SessionError` describes session existence, residency, busy/closing state, invalid input, interaction mismatch, unavailability, and session-internal failures.

The historical v0.2 registry-based construction example was removed in P3-D. Current v0.3 hosts bind one checked `Arc<dyn Model>` and an immutable `ToolSet` through `SessionBindings`; network adapter configuration is outside this crate.

Session configuration selects a workspace root, model selection, system prompt, enabled tool names, compaction trigger/target, and maximum tool rounds. The selected names must already exist in the immutable `ToolRegistry`.

## Persistence Migration

The v2 layout is:

```text
<data_dir>/runtime.lock
<data_dir>/sessions/<ses_...>/session.json
<data_dir>/sessions/<ses_...>/conversation.jsonl
```

`runtime.lock` is an exclusive runtime-root lock owned by the SessionStore worker. A session directory contains exactly `session.json` and `conversation.jsonl`. Creation is complete-or-invisible: the worker writes a temporary directory, synchronizes its files, and renames it into the namespace.

`session.json` is format version `2` and stores the current checked session configuration. `conversation.jsonl` is append-only v2 semantic history. There are no generations, current/previous heads, aliases, or publication markers in the v2 hot path. The exact fields and order are documented in [session-json-v2.md](formats/session-json-v2.md) and [conversation-jsonl-v2.md](formats/conversation-jsonl-v2.md).

The v2 core does not automatically read or transform the historical Store V1 layout. A host migrating existing data must perform an explicit offline migration:

1. Stop all writers and obtain a backup or immutable copy of the old data.
2. Read the old records with the old owner or a separately reviewed migration tool.
3. Select the sessions that can be represented by one v2 workspace root, model selection, prompt, tool set, compaction policy, and conversation.
4. Emit v2 `session.json` and `conversation.jsonl` through checked constructors or a separately reviewed offline writer.
5. Validate the emitted files by opening them with a v2 runtime before replacing the destination.
6. Keep the source data until the migrated runtime has been independently verified.

This is deliberately not an online compatibility mode. A malformed, ambiguous, or unsupported old record must be rejected or quarantined by the migration tool rather than guessed by the v2 runtime.

## Historical Configuration Note

The v0.2 registry and builtin examples are baseline history, not current v0.3 interfaces. P3-B replaced tool registration with immutable `ToolSet`; P3-D replaced model lookup with one directly bound `Arc<dyn Model>`. Concrete filesystem, process, and network adapters are host-owned and are not installed by the core.

## Security Behavior

Filesystem routes use one capability-backed workspace root. Configuration requires an absolute root without lexical dot components; tool paths are relative and cannot traverse outside the captured root. Symlink escape, invalid final targets, read-only writes, non-UTF-8 data, and bounds failures are rejected.

`run_command` is structured `program + args`, never a shell string. Its `cwd` is checked as a workspace-relative path before spawn, but the child receives an ambient host path and host process authority. The process policy controls executable allowlist, inherited environment keys, default/max timeout, and output limits. The environment is cleared before permitted values are added. The runtime does not claim a process sandbox or process-tree sandbox.

Model adapters must report safe typed errors. Delivery state is exactly `NotStarted`, `Started`, or `Unknown`; only explicitly retryable `NotStarted` failures are eligible for logical retry.

## Observation and Conversation Differences

`Runtime::snapshot` is the recovery baseline. `Runtime::subscribe` publishes that baseline before subsequent bounded events; a subscriber that falls behind must resynchronize. Session events describe state, model/tool progress, interactions, terminals, and closure without exposing private credentials or paths.

Conversation interaction entries are durable transcript facts but are not model-visible messages. The prompt projection contains user, assistant, and tool messages while preserving the interaction in the transcript. Compaction appends a summary boundary and retains source history; it does not delete the evidence used for transcript paging.

Restart recovery completes unresolved tool calls with the fixed error text `cancelled by restart` and appends a cancelled terminal outcome. A complete middle-file corruption is an error with a physical line/offset classification. Only a final incomplete tail is truncated and repaired.

## Migration Checklist

- [ ] Stop the old runtime and make a durable source backup.
- [ ] Bind one checked host `Arc<dyn Model>` per loaded session.
- [ ] Build an explicit immutable `ToolSet`; enable only the names it contains.
- [ ] Convert each workspace to one absolute root and review requested access.
- [ ] Convert session model/prompt/compaction/tool-round settings to `SessionConfig`.
- [ ] Convert or regenerate storage into the v2 flat layout offline.
- [ ] Replace transport command/query calls with typed Runtime calls.
- [ ] Replace old lifecycle/queue operations with submit, answer, cancel, close, and shutdown semantics.
- [ ] Observe a snapshot before consuming a subscription stream.
- [ ] Await `Runtime::shutdown()` before host runtime teardown.
- [ ] Keep external adapter evidence separate from deterministic core validation.

No compatibility wrapper is supplied. Downstream applications that need the previous surface must own a separate adapter and an explicit data migration program.

## Error and Lifecycle Mapping

The old caller should not translate every failure into a generic transport error. Use the typed v2 boundary:

| Situation | v2 result |
| --- | --- |
| invalid data root, prompt, capacity, or runtime/session bounds | `ConfigError` while building `RuntimeConfig` or `SessionConfig` |
| invalid retry attempt count or base delay | `RetryPolicyError` from `RetryPolicy::new` before constructing `RuntimeConfig` |
| data root lock already held or store worker cannot bootstrap | `RuntimeError` from `Runtime::open` |
| unknown session ID | `SessionError::NotFound` |
| session is loading/closing or another operation owns its boundary | `SessionError::Busy` or `SessionError::Closing` |
| the bound model does not match the session model reference | `SessionError::Unavailable` during create/load/admission |
| answer targets another interaction | `SessionError::InteractionMismatch` |
| model cancellation wins | a cancelled turn outcome and `SessionError` only for the caller boundary that was rejected |
| durable middle corruption or unrecoverable append failure | unavailable/internal session state rather than fabricated success |

`close_session` waits for the actor-owned completion and removes only the exact loaded owner. `delete_session` is a durable namespace operation and refuses a loaded session. `shutdown` closes admission across the Runtime, joins session and store owners, and releases the root lock; it is not equivalent to dropping a handle.

## Host Configuration Direction

Current v0.3 host construction is module-qualified and direct: build a checked `Model` implementation, immutable `ToolSet`, optional `ToolPolicy`/context/compaction adapters, and a `SessionLog`; `SessionBindings` validates the adapters now, while P4 will attach them to the loaded owner. No registry, resolver, builtin set, or network configuration enters the core interface.

## Tool Behavior Checklist

- Replace old generic tool command payloads with registered `ToolName` values in `SessionConfig`.
- Use `ReadFileTool`, `ListDirectoryTool`, and `WriteFileTool` for capability-relative filesystem work.
- Use `AskUserTool` when a turn may wait for a user answer; the answer is persisted before the turn resumes.
- Use `RunCommandTool` only when a host has explicitly selected its executable and environment policy.
- Treat a tool failure as a durable tool result, not as a successful model round.
- Do not pass absolute paths to file tools or shell strings to the process builtin.

## Data Verification

After an offline conversion, validate each target by opening the data directory with a v2 Runtime, loading the session, reading a snapshot, paging the transcript, and checking that any current turn is either absent or represented by a truthful terminal/recovery state. Compare the original source history separately; v2 summaries are projections and must not be used to discard source entries.

Keep the old data copy until:

1. every selected session opens and lists correctly;
2. session JSON round-trips with format version 2;
3. conversation replay either succeeds or reports a located complete corruption;
4. partial tails are repaired only at the end of a file;
5. model and tool bindings are intentionally reviewed;
6. shutdown releases the new root lock after the verification run.
