# v0.1 to v0.2 Migration

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

A host must install a real provider descriptor before creating a session that selects it:

```rust
let mut providers = ProviderRegistry::builder();
providers.register(OpenAiResponsesProvider::new_https(
    "https://api.openai.com/v1/responses",
    fixed_credential_source(&std::env::var("OPENAI_API_KEY")?)?,
    vec![descriptor],
)?)?;
let provider_registry = providers.build();

let mut tools = ToolRegistry::builder();
tools.register(ReadFileTool::new())?;
let config = RuntimeConfig::new(
    data_dir,
    provider_registry,
    tools.build(),
    "coding instructions",
    RetryPolicy::new(3, Duration::from_millis(250))?,
)?;
let runtime = Runtime::open(config, Handle::current()).await?;
```

A host may instead install `AnthropicMessagesProvider::new_https` with its required version string. Credentials are opaque values resolved inside provider attempts; they must not be placed in command-line arguments or persisted session configuration.

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

## Tool and Provider Configuration

The v2 host owns both registries. A minimal filesystem configuration is:

```rust
let mut tools = ToolRegistry::builder();
tools.register(ReadFileTool::new())?;
tools.register(ListDirectoryTool::new())?;
tools.register(WriteFileTool::new())?;
```

`ask_user` is a question/answer builtin. `read_file` returns one bounded UTF-8 text result from a relative path. `list_directory` returns sorted direct entries as compact JSON. `write_file` replaces one relative UTF-8 file and does not create directories. `run_command` is enabled only when the host registers `RunCommandTool` with an explicit `ProcessPolicy`.

The model registry freezes descriptors at build time. A descriptor binds a stable provider/model selection to an API model name, model limits, and supported reasoning preferences. The current direct providers are OpenAI Responses and Anthropic Messages. No provider is selected or installed implicitly.

## Security Behavior

Filesystem routes use one capability-backed workspace root. Configuration requires an absolute root without lexical dot components; tool paths are relative and cannot traverse outside the captured root. Symlink escape, invalid final targets, read-only writes, non-UTF-8 data, and bounds failures are rejected.

`run_command` is structured `program + args`, never a shell string. Its `cwd` is checked as a workspace-relative path before spawn, but the child receives an ambient host path and host process authority. The process policy controls executable allowlist, inherited environment keys, default/max timeout, and output limits. The environment is cleared before permitted values are added. The runtime does not claim a process sandbox or process-tree sandbox.

Provider credentials are checked opaque values and are redacted in debug/error surfaces. Provider transport disables redirects, automatic retries, proxies, and compression. Delivery state distinguishes pre-send/rejected failures from unknown or output-started outcomes, so logical retry is conservative.

## Observation and Conversation Differences

`Runtime::snapshot` is the recovery baseline. `Runtime::subscribe` publishes that baseline before subsequent bounded events; a subscriber that falls behind must resynchronize. Session events describe state, model/tool progress, interactions, terminals, and closure without exposing private credentials or paths.

Conversation interaction entries are durable transcript facts but are not model-visible messages. The prompt projection contains user, assistant, and tool messages while preserving the interaction in the transcript. Compaction appends a summary boundary and retains source history; it does not delete the evidence used for transcript paging.

Restart recovery completes unresolved tool calls with the fixed error text `cancelled by restart` and appends a cancelled terminal outcome. A complete middle-file corruption is an error with a physical line/offset classification. Only a final incomplete tail is truncated and repaired.

## Migration Checklist

- [ ] Stop the old runtime and make a durable source backup.
- [ ] Install concrete provider descriptors in a `ProviderRegistry`.
- [ ] Resolve credentials through a host-owned `CredentialSource`.
- [ ] Build an explicit `ToolRegistry`; enable only the names it contains.
- [ ] Convert each workspace to one absolute root and review requested access.
- [ ] Convert session model/prompt/compaction/tool-round settings to `SessionConfig`.
- [ ] Convert or regenerate storage into the v2 flat layout offline.
- [ ] Replace transport command/query calls with typed Runtime calls.
- [ ] Replace old lifecycle/queue operations with submit, answer, cancel, close, and shutdown semantics.
- [ ] Observe a snapshot before consuming a subscription stream.
- [ ] Await `Runtime::shutdown()` before host runtime teardown.
- [ ] Keep live provider smokes explicitly ignored unless release evidence is intended.

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
| selected provider/model is not registered | `SessionError::Unavailable` during create/load/admission |
| answer targets another interaction | `SessionError::InteractionMismatch` |
| model cancellation wins | a cancelled turn outcome and `SessionError` only for the caller boundary that was rejected |
| durable middle corruption or unrecoverable append failure | unavailable/internal session state rather than fabricated success |

`close_session` waits for the actor-owned completion and removes only the exact loaded owner. `delete_session` is a durable namespace operation and refuses a loaded session. `shutdown` closes admission across the Runtime, joins session and store owners, and releases the root lock; it is not equivalent to dropping a handle.

## Host Configuration Example

A host that previously sent a command envelope should now build values directly:

```rust
let mut provider_builder = ProviderRegistry::builder();
provider_builder.register(real_openai_provider)?;
let providers = provider_builder.build();

let mut tool_builder = ToolRegistry::builder();
tool_builder.register(ReadFileTool::new())?;
tool_builder.register(ListDirectoryTool::new())?;
tool_builder.register(WriteFileTool::new())?;
// Register RunCommandTool only with an explicit ProcessPolicy.
let tools = tool_builder.build();

let runtime = Runtime::open(
    RuntimeConfig::new(
        data_dir,
        providers,
        tools,
        coding_instructions,
        RetryPolicy::new(3, Duration::from_millis(250))?,
    )?,
    Handle::current(),
)
.await?;
```

The provider descriptor must bind the stable selection used by `SessionConfig`. The API model name, endpoint, credential source, reasoning support, and model limits remain provider installation details. The session persists only the stable provider/model selection, not the credential or endpoint.

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
5. provider and tool selections are intentionally reviewed;
6. shutdown releases the new root lock after the verification run.
