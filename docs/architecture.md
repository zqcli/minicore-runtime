# v0.2 Architecture

## Purpose

MiniCore Runtime is a typed, embeddable core for a durable coding session. The architecture favors deep owners over a broad protocol layer:

```text
host configuration
    -> Runtime
        -> SessionManager / SessionStore
            -> zero or more loaded SessionActors
                -> one Turn runner per active Session
                    -> PromptBuilder -> ModelGateway -> ToolRegistry/Workspace
                        -> ConversationLog -> Snapshot/Event/Transcript
```

The source of truth for this map is [`src/lib.rs`](../src/lib.rs), the [canonical module map](modules/README.md), and the current [format documents](formats/session-json-v2.md). Historical pre-reset material is not an implementation dependency.

## Dependency Direction

The crate has one public facade and a small number of owner boundaries:

1. `config` validates host input and constructs checked values. It does not perform I/O.
2. `model` owns provider descriptors, credential resolution, request/response values, transport, and model selection resolution.
3. `tools` owns tool descriptions, policy decisions, interaction requests, process policy, and builtin execution contracts.
4. `workspace` owns one capability-backed root and all filesystem operations relative to that root.
5. `prompt` owns prompt assembly and compaction planning. It is private.
6. `agent` owns one turn's model/tool loop. It is private and returns work to the session owner.
7. `session` owns durable conversation state, the actor mailbox, terminal settlement, observation, and per-session resources.
8. `runtime` owns public lifecycle admission, loaded-session residency, and runtime cleanup.

The model registry does not reach into sessions. Tools receive a `ToolContext` rather than a Runtime handle. Workspace does not know about providers or conversation state. Prompt assembly consumes already-owned values. The session actor is the integration owner; it does not create peer actors for each subsystem.

## Runtime Ownership

`Runtime::open(config, handle)` opens the store before publishing a `Runtime`. The store worker creates the data directory, acquires `runtime.lock`, prepares `sessions/`, removes orphan temporary create directories, and reports a typed readiness result. The Runtime retains the store, a model gateway built from the immutable provider registry, and the session manager.

`RuntimeConfig` contains:

- absolute data directory;
- immutable `ProviderRegistry`;
- immutable `ToolRegistry`;
- non-empty checked coding instructions;
- shutdown timeout from 1 ms through 300 seconds;
- bounded event, command, and runner-event capacities;
- checked `RetryPolicy`.

`SessionConfig` contains:

- one absolute workspace root without lexical dot components;
- one `ModelSelection`;
- checked system prompt;
- a sorted set of enabled tool names;
- compaction trigger and target tokens;
- maximum tool rounds.

The host must register a provider descriptor for the selected model before `create_session`. The Runtime does not infer a provider from an endpoint or model string.

## Session Residency

`SessionManager` maintains one admission boundary for loaded, loading, and closing sessions. Create commits a durable session before preparing a loaded actor. Load reserves the session ID, reads and validates stored configuration, opens the workspace and conversation, prepares dependencies, then publishes one managed session. A failed preparation does not erase a durable create.

Close requests the actor's owner-tracked close completion, removes the exact managed session, and retains the durable session for later load. Delete first excludes a loaded session, then removes its validated durable directory. List reads durable summaries and marks which sessions are loaded without opening every session actor.

There is no second mailbox for a subsystem. Runtime admission is short and synchronous around manager state; the actor owns asynchronous session work after the manager publishes it.

## Session State Machine

The public session status is exactly:

```text
Idle -> Running -> Idle
Idle -> WaitingForInput -> Running -> Idle
Idle/Running/WaitingForInput -> Closing
```

`Idle` accepts input. `Running` has an active model/tool turn. `WaitingForInput` has one claimed question presentation awaiting an answer. `Closing` rejects new work while accepted work and cleanup settle.

A public `SessionSnapshot` contains exactly these fields: `session_id`, `status`, `active_turn`, `pending_question`, `usage`, `last_error`, `last_terminal`, and `conversation_seq`. Its constructor validates the legal combinations of status, active turn, and pending question.

## Submit Flow

1. The host calls `Runtime::submit(session_id, input)`.
2. Runtime resolves the exact loaded managed session or returns `SessionError`.
3. The session handle sends one bounded submit command to the actor mailbox.
4. The actor checks closing, readiness, interaction state, and turn admission.
5. The actor persists the user entry before the turn is publicly resumed.
6. The actor publishes the resulting snapshot and starts one owner future for the turn runner.
7. The runner builds a prompt from the conversation projection and captured turn resources.
8. Model output is streamed through a bounded model event sink and reduced into one checked response.
9. Tool calls are validated in round order, admitted through the configured policy, executed with the captured workspace/cancellation context, and appended as durable results.
10. The runner either requests the next model round, requests an interaction, requests compaction, or returns a truthful terminal result.
11. The actor owns terminal append, projection refresh, status publication, and follow-on cleanup.

A dropped caller is a dropped waiter, not a cancellation of owner-tracked actor work. A closed mailbox or actor failure maps to a session error rather than an apparent success.

## Model Flow

`ModelGateway` resolves a `ModelSelection` only through its immutable registry. Each provider descriptor carries its API model name, limits, and supported reasoning preferences. The provider receives a complete stateless request: system/user/assistant/tool messages and the current tool specifications are encoded for every attempt.

Credentials are represented by redacted `ProviderCredential` values. A provider resolves its `CredentialSource` inside the attempt future. A missing credential is `AuthMissing` with `NotSent` delivery. Endpoint policy is explicit: production constructors use `HttpsOnly`; loopback constructors are for offline contract tests.

OpenAI Responses and Anthropic Messages own their protocol rules. The transport client disables redirects, automatic retry, proxy discovery, and decompression. Response bodies and event streams are drained through bounds and cancellation. A successful stream requires protocol-specific terminal evidence; early EOF is not manufactured into success.

## Provider Retry

`RetryPolicy::new` accepts 1 through 4 total attempts and a base delay no greater than 30 seconds. Exponential delay is bounded at 30 seconds and may be raised by a provider retry-after value up to that same limit.

A model failure is retry-safe only when the provider reports a pre-execution delivery state (`NotSent` or `RejectedBeforeExecution`) and the error kind is a permitted transient. `AcceptedNoOutput`, `Unknown`, and `OutputStarted` are conservative, non-retryable outcomes because the remote operation may have executed or produced semantic output. A retry-after value above 30 seconds disables the retry rather than being silently clipped into a retry.

The model gateway has no local call permit and no detached retry task. The turn runner owns the retry loop and the session actor owns its cancellation and terminal settlement.

## Tool Flow

The `ToolRegistryBuilder` registers each tool once and freezes sorted immutable `ToolSpec` values. A session enables a set of names; unknown or unavailable names fail configuration or turn admission. The tool policy sees a checked `ToolRequest` and returns an allow, deny, or question decision without owning the tool future.

A tool receives `ToolContext` containing:

- the captured `Workspace`;
- the current cancellation token;
- the interaction bridge for question-based tools;
- the tool's checked request context.

The tool future returns a bounded `ToolOutput` with text and `is_error`. A panic, malformed result, cancellation, policy denial, or owner failure is converted into a truthful durable result. The actor does not treat a failed tool as a model success.

### Filesystem Builtins

`read_file`, `list_directory`, and `write_file` use one captured workspace capability. They accept only checked relative paths. Reads and writes are bounded to 256 KiB. Directory listing is direct-only, sorted, and bounded to 1,000 entries, 4 KiB per name, and 256 KiB total retained name bytes. Write is full replacement, does not create directories, and respects `ReadOnly` access.

### Question Builtin

`ask_user` produces one checked `UserQuestion` with safe UTF-8 question text and optional choices. The public DTO bounds are question/answer text `1..=8192` bytes, optional choices `1..=32`, and each choice `1..=1024` bytes. The DTO text validator permits newline and tab; the interaction context may apply the stricter all-control-character rejection before persistence. The actor publishes `WaitingForInput`. `InteractionRequest::claim_response` is the first-winner linearization point. After the claim, the actor appends the interaction and only then resumes normal Running publication. A late or duplicate answer is rejected without a second durable interaction.

### Process Builtin

`run_command` accepts `program`, `args`, optional relative `cwd`, optional `timeout_ms`, and an `env` object. It requires an explicit enabled `ProcessPolicy` and program/environment allowlists. It clears the child environment, passes only permitted inherited or requested values, captures bounded stdout/stderr, and uses a direct child process. It never parses a shell string. Timeout and cancellation kill/wait the direct child within the cleanup bound, but the runtime does not claim a process-tree sandbox.

## Cancellation and Close

Cancellation is requested through an out-of-band `CancelSlot`, not through the bounded submit/answer mailbox. The slot verifies the current turn while holding its short mutex, extracts pending interaction state, and signals the exact cancellation token. It never holds a mutex across an await.

The actor maps cancellation according to the point at which it wins: before admission, during model delivery, during a tool, while waiting for input, or after durable terminal work. Accepted interaction answers are not retroactively rejected by a later cancellation; persistence-before-resume prevents a misleading host retry.

Close synchronously signals admission cancellation, waits for accepted work, joins owner-tracked task handles, waits for conversation I/O idle, makes one truthful terminal append attempt when needed, closes the conversation and workspace, and publishes a shared close result. Cleanup errors cannot replace an earlier authoritative terminal error.

## Observation

Snapshots are the recovery baseline. `subscribe` returns a bounded stream whose first observable state is a snapshot; event delivery follows publication. The event stream can report lag and require a fresh subscription/resync rather than inventing missing intermediate state. Closure publishes exactly one `Closed` event before EOF.

Model deltas, tool execution, interaction waiting, terminal results, and closure are projected into typed session events. Usage and terminal history are available through the snapshot. Event payloads are bounded and redact secret/host-specific values. A subscriber that drops its receiver does not cancel the owning actor.

## Persistence and Recovery

The store owns `runtime.lock` and the sessions namespace. Conversation append is serialized through the store worker, flushed/synchronized, and reflected in the in-memory state only after the durable barrier. `ConversationLog` owns the semantic append and replay contract; `SessionActor` owns when a user, assistant, tool, interaction, summary, or terminal entry is allowed to exist.

Replay validates line size, file size, UTF-8, JSON shape, positive sequence order, tool relations, interaction identity, terminal boundaries, and summary boundaries. A final partial line is repaired by truncation to the last complete newline. A complete middle failure returns located corruption. Store health can degrade on local recording failure; a terminal append failure makes the live session unavailable rather than claiming completion.

Prompt projection retains source conversation. A summary changes the model-visible prefix through `through_seq`; it does not delete transcript entries. Transcript pages expose durable `Interaction` entries even though those entries are not sent to the model.

## Error Boundaries

- Configuration errors belong to `ConfigError` and occur before Runtime open or session create.
- Runtime open/shutdown failures belong to `RuntimeError`.
- Session admission, residency, interaction, observation, and transcript failures belong to `SessionError`.
- Model protocol and delivery failures remain `ModelError` until the turn runner maps them to a truthful terminal outcome.
- Tool argument, policy, workspace, process, and cancellation failures remain owned by the tool path until the actor persists a result.
- Store/conversation corruption, bounds, lock, worker, and cleanup failures remain owned by persistence until Runtime maps availability.

No layer turns an unknown remote outcome into a safe retry, a failed durable append into success, or an unavailable workspace into an ambient path access.

## Deliberate Limits

The core has no default provider, no shell language, no process-tree sandbox claim, no automatic historical storage migration, and no server or CLI. It has one actor per loaded session, one bounded mailbox, one conversation log, one workspace owner, and one Runtime shutdown owner. Future host features must preserve these ownership boundaries or introduce a separate documented owner and evidence contract.

## Startup and Shutdown Timeline

Open starts with checked configuration, then store bootstrap, then model/tool ownership publication. No session actor exists until a durable session is prepared. This ordering makes a failed root lock or invalid provider/tool setup visible before session admission.

Create first commits the durable session description and empty conversation, then prepares a workspace and conversation owner. Load starts from durable configuration and performs the same checked preparation against existing files. The manager publishes a managed session only after preparation succeeds.

Close first blocks new admission, then lets accepted turn and interaction work settle, waits for conversation I/O, performs the final terminal/close sequence, closes workspace ownership, and removes the exact manager entry. Runtime shutdown applies the same sequence to all known sessions and then shuts down the store worker.

The final barriers are deliberately ordered: actor close, conversation idle/terminal attempt, conversation close, workspace close, session manager removal, store worker shutdown, root-lock release, and Runtime shutdown result. A caller that drops a waiter cannot reorder those owner steps.

## Detailed Owner Contracts

### Configuration to admission

`RuntimeConfig::new` checks the data root, coding instructions, capacities, timeout, and retry policy before any worker is started. `SessionConfig::new` checks the workspace root, system prompt, enabled-name count, compaction ordering, and round count before a durable object is created. This keeps invalid host input outside the actor state machine.

The conversion from `SessionConfig` to `StoredSessionConfig` happens once, with a single timestamp used for creation and update. The stored configuration contains no provider credential, endpoint URL, tool implementation, workspace capability handle, or live actor state.

### Actor work ownership

The actor receives a bounded command enum. The command future may wait for a runner, an interaction append, a conversation barrier, or a close completion, but it never delegates ownership to an untracked task. The runner is a future held by the actor's owner-tracked turn state. A provider future is held by the runner; a blocking builtin job is held by its workspace or process owner.

The actor's normal cycle is:

```text
admit -> persist user -> publish Running -> run turn
     -> persist assistant/tool/interaction evidence
     -> publish waiting or terminal state
     -> settle close/queue/cancellation boundaries
```

A state publication is derived from durable/projection facts, not from a hopeful command receipt. For example, `WaitingForInput` is published only after the question request is represented by the actor's interaction state, and a terminal outcome is published only after its append attempt has a truthful result.

### Prompt and compaction ownership

The prompt builder receives system instructions, the current user input, completed conversation messages, tool specifications, and the current turn messages. It estimates tokens from deterministic serialized bytes, not provider-specific hidden counters. Exact context equality is allowed; overflow is handled as a typed model/compaction path.

Compaction requires a completed boundary and a valid summary plan. It preserves the current turn, appends a summary through a stale-safe conversation operation, and replans if another append advanced the snapshot. The source conversation remains replayable and the transcript remains complete even when the next prompt starts after a summary boundary.

### Workspace capability ownership

`Workspace::open` validates the configured root before opening a capability. The root worker owns all blocking operations and exposes asynchronous methods that retain the worker job until its result or cancellation cleanup is settled. A dropped result receiver does not detach a running filesystem job.

Relative-path parsing rejects empty paths where a file target is required, dot components, absolute forms, NULs, and platform-specific prefixes. Directory listing allows an empty path as the root and returns only direct entries. Write operations use final-component no-follow behavior and never silently widen a read-only workspace.

Production session ownership awaits `Workspace::shutdown()` during actor close. The `Workspace` Drop fallback may block synchronously and is not preferred. Explicit `Runtime::shutdown()` waits for all known session actors, so its result observes their workspace shutdowns as well as the store worker and root-lock release.

### Interaction ownership

The tool policy may return a question presentation, but the session actor owns its lifetime. The interaction client exposes a one-shot response claim. Only the first matching answer can transition the request to claimed; receiver closure, cancellation, and actor close are separate outcomes. The actor persists `Interaction` after the claim and before the next model admission.

An interaction is intentionally split between three views:

- tool view: the question, choices, answer, and claim state;
- session view: `WaitingForInput`, pending interaction identity, and close/cancel behavior;
- transcript view: the durable question/answer entry;

Only the appropriate view crosses each boundary. The interaction is not inserted into the model message sequence.

### Store and conversation barriers

The store worker has a bootstrap barrier, a bounded job queue, a final worker-exit result, and an explicit root-lock release step. The owner joins the exact worker thread and preserves sticky worker failure. A successful store shutdown therefore proves more than channel closure: the lock is released and the worker result is known.

Conversation append reserves a sequence under the conversation owner, encodes one candidate, submits one physical append job, and only then applies the candidate to the live projection. `wait_idle` is used by close paths before the final terminal append attempt. A degraded recording path remains visible to the session and prevents a later operation from claiming healthy durability.

### Public observation ownership

The snapshot contains current semantic state; events are notifications about a published state transition. The event stream has a bounded broadcast side and a snapshot/watch side. A lagged receiver receives a resynchronization signal rather than a fabricated sequence of missed events. `Closed` is owner-controlled and is followed by EOF exactly once.

Runtime-level and session-level projections use stable redacted summaries. They do not include workspace absolute paths, credentials, provider endpoint details, raw request bodies, or arbitrary tool arguments beyond bounded model-visible result text.

### Public error ownership

An invalid public value is rejected by its constructor and maps to `InvalidInput` or a configuration error before it reaches a worker. A missing loaded session maps to `NotFound`; an admitted concurrent operation maps to `Busy`; lifecycle shutdown maps to `Closing`; an unavailable workspace/model/tool dependency maps to `Unavailable`; cancellation maps to `Cancelled` when it wins the relevant boundary.

The error code is not a replacement for the owning typed error. The model layer retains delivery and retry information, the tool layer retains policy/workspace/process detail, and the persistence layer retains corruption/worker/cleanup detail until the public boundary intentionally redacts it.
