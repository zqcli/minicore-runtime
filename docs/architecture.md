# Current Architecture

This document describes the current v0.3 reset slice. The former v0.2 multi-session Runtime and SessionManager are deleted. The remaining old actor/command/provider/workspace/storage graph is gated at owning module declarations with `cfg(test)`: it compiles for legacy unit evidence, not in the production library and not as a compatibility facade.

## Public Spine

```text
Host
 ├── session::SessionRuntime { exactly one loaded Session }
 │   ├── session::SessionRuntimeOptions
 │   ├── storage::SessionLog { exactly one owned adapter }
 │   ├── session::SessionEventStream { taken once }
 │   └── root cancellation + one owner JoinHandle
 ├── session::SessionBindings
 │   ├── model::Model
 │   │   └── ModelStream { typed events }
 │   ├── tools::ToolSet ── Arc<dyn tools::Tool>
 │   │   └── ToolContext { cancellation, deadline, progress }
 │   ├── tools::ToolPolicy
 │   │   └── ToolPolicyRequest { invocation, spec, cancellation, deadline }
 │   ├── context::ContextProvider
 │   └── compaction::CompactionStrategy
 └── storage::SessionLog
```

The Host obtains one exclusive SessionLog and calls `SessionRuntime::create` or `load`. Managing `HashMap<SessionId, SessionRuntime>`, list/delete policy, writer leases, global rate limits, and shutdown-all belongs outside Core.

The public Tool seam is host-neutral. A Tool receives a checked `ToolInvocation` and a context containing only cancellation, deadline, and synchronous best-effort progress. Workspace roots, process capabilities, RPC clients, credentials, policy decisions, and runtime/session handles are not fields of `ToolContext`.

`ToolSetBuilder` is the only mutable phase. Registration captures a checked `ToolSpec`, validates its public fields, and records the first duplicate name, spec panic, or invalid-spec mutation; `build()` returns that error or a frozen immutable set. `specs_for` deterministically returns only registered enabled specs and omits unknown names; `SessionBindings::validate` rejects unknown enabled names and checks every frozen spec against semantic budgets. Cloned sets share the same `Arc` tool values and support concurrent execution. There is no public `ToolRegistry`, no default concrete adapter set, and no builtin/process implementation in `src/tools`.

## Session Bindings

`session::SessionBindings` is the immutable adapter bundle for one future loaded session. Its exact fields are one direct Model, one ToolSet, and optional ToolPolicy, ContextProvider, and CompactionStrategy values. It contains no Clock, runtime/task handle, log, store, workspace, owner, or metadata. Construction installs no defaults.

Validation is pure and does not invoke adapter futures. The only adapter call is `Model::descriptor`, cloned inside `catch_unwind`; a panic becomes the payload-free `ModelPanicked` error. Validation then checks limits/specs, descriptor integrity and compatibility, enabled tool support/policy/registration, all frozen ToolSpec semantic budgets, and compaction strategy presence. Disabled compaction permits a strategy without invoking it, and optional context is never invoked.

`model::driver` is the private P5-A execution module. `ModelDriver` binds one `Arc<dyn Model>` plus an immutable Kernel-derived snapshot of model timeout, retry policy, and semantic limits. The snapshot avoids a `model → config` dependency and preserves the singleton module DAG. `run_detailed` applies one shared effective deadline and retains whether the Turn or model-port timeout selected it through start, stream, and retry waits; the existing `run` wrapper erases only that internal provenance and preserves the ModelError contract. Adapter-returned Timeout has no Core provenance. Cancellation, panic isolation, delivery-safe retry, strict EOF/Finish/Usage/tool-call grammar, checked response construction, and bounded progress remain unchanged. Missing Usage becomes `Usage::default()`; every response still requires at least one assistant part. The driver imports no session, agent, storage, workspace, provider lookup, credential, HTTP, or tool-execution authority.

`agent::tool_driver` is the private P5-B tool-execution module. It binds one immutable ToolSet, the enabled ToolName set, an optional policy that is required whenever tools are enabled, and checked Kernel-derived policy/tool timeout and semantic input/output snapshots. Policy and Tool stages independently use the shared deadline selector. A Core-selected Turn deadline before or during either pending stage returns exact `Err(SuspensionError::DeadlineExceeded)` and produces no ordinary ToolResult; a configured policy timeout becomes generic Denied, and a configured Tool timeout becomes generic Failed. Adapter-returned policy/Tool errors, including Tool TimedOut, remain ordinary port failures with no Core deadline provenance. Child cancellation occurs before interrupted future Drop for both sources. Approval/input suspension waits remain bounded solely by the Turn deadline. Canonical input encoding, semantic output checks, Started/Update progress, and no-spawn/no-append authority remain unchanged.

`context::driver` is the private P5-C ContextProvider execution module. It binds zero or one provider, a checked context timeout, and immutable semantic limits. `provide_detailed` uses the shared deadline selector and reports driver-generated DeadlineExceeded with exact Turn or Port provenance; a provider-returned DeadlineExceeded has no Core provenance. The existing `provide` wrapper preserves the original ContextError-only contract. Construction/poll panic isolation, cancellation, same-future Drop, typed errors, and one final validate-and-sort remain single-sourced. No-provider sessions receive the validated empty bundle. There is no retry, fanout, spawn, or partial-success merge.

`ConversationView::validated_prompt_projection` is the only prompt-history proof seam. It constructs a fresh ConversationState from the full SessionSpec and SemanticLimits, validates the entire confirmed view as one candidate batch through ConversationValidator, requires the validator head to equal the view head, then derives the latest validated SummaryEntry, the canonical active UserMessage TurnExecutionRecord, and entries after the summary through boundary from the canonical PromptProjection. Sequence gaps, turn phases and identity, session-wide tool-call IDs, tool/result order, finish shape, terminal settlement, summary boundaries, and lower valid per-turn tool-round overrides therefore have exactly the durable validator semantics. The crate-private proof Debug reports only head, selected summary sequence/boundary, entry count, active Turn identity, model reference, and max tool rounds; it never reports User input or summary content.

`prompt::builder` is the private final P5-C prompt module. Its immutable constructor captures the full SessionSpec, exact already-selected frozen ToolSpecs, and semantic limits. The fixed kernel invariant contains protocol facts only. Prompt order is kernel invariant, optional session system prompt, sorted ProjectInstructions, RetrievedKnowledge, TurnContext blocks, then the conversation-owned validated projection. Context headers include fixed slot and ContextSourceId fields inside System messages. The selected SummaryEntry becomes a System message containing exactly its checked summary text, with no metadata prefix, and entries through its boundary are suppressed; later user/assistant/tool-result entries are mapped while summaries and terminals are omitted. PromptBuilder contains no second durable-history validator. It first constructs the exact checked ModelRequest, serializes that same request including limits and reasoning, rounds compact JSON bytes by four, and reserves max output tokens before reporting remaining context or returning the unchanged request. The builder invokes no provider, model, tool, log, workspace, store, or owner.

`ConversationView::validated_compaction_candidate` shares the same fresh ConversationState and whole-view candidate validation as the prompt proof. ConversationState constructs the immutable CompactionCandidate from all confirmed projection entries, the exact snapshot head, the validator-owned latest summary boundary, and the validator's sorted terminal-boundary set. An active current turn remains visible to a strategy but never becomes a completed boundary. Conversation owns this proof DTO physically; `compaction` publicly reexports it through the unchanged Port path without creating a reverse dependency.

`compaction::driver` is the private P5-D strategy-execution module. It binds zero or one immutable CompactionStrategy, a checked timeout, and the max summary byte snapshot. It now uses the shared Turn/Port deadline selector while preserving its existing CompactionError-only behavior, scoped child cancellation, panic isolation, and cancellation-before-Drop ordering. Detailed compaction deadline propagation is deferred to P5-E2, where the actor will consume the result and must compare current head to snapshot head before committing Summary. Proposal validation and the driver's no-commit/no-spawn authority are unchanged.

`agent::turn_context`, `agent::runner_protocol`, and `agent::runner` are the private P5-E1 ordinary Turn module. TurnRunnerRequest contains exact identity, full SessionSpec, the durable effective tool-round cap, immutable SessionBindings, one canonical active ConversationView, cancellation/deadline, checked Kernel snapshots, and bounded critical/progress senders. The runner consumes ContextDriverFailure and ModelDriverFailure directly: Timeout/DeadlineExceeded with `Some(Turn)` is BudgetExceeded; `Some(Port)` or adapter-origin `None` retains ContextFailed or ModelTimeout. It never infers provenance by reading the clock after an error. Absolute pre-loop, critical send/ack, and suspension deadlines remain BudgetExceeded. Exact prefix-extending acknowledgements, sequential tools, ToolStarted-before-suspension ordering, all-outcome conservative usage, Join fallback, bounded panic handling, and the no-spawn/no-log/no-compaction boundary remain unchanged.

## State, Event, And Turn Foundation

`session::SessionState` replaces the final heavy snapshot concept with one process-local current-state value. `SessionStatus` is exactly Idle, Running, WaitingForInput, or Closing. Validation centralizes active-turn/pending-interaction shape and prevents an active Turn from also being the latest durable terminal. `SessionHealth::Degraded` carries a checked `DiagnosticSummary`; its Debug reports message bytes rather than message content.

`session::SessionEventStream` owns one bounded mpsc receiver. It has no Clone, snapshot, subscribe, broadcast, cursor, revision, epoch, gap, or resync interface. The crate-private `InternalEventSink` uses `try_send`; full queues increment a saturating loss count, and a later event first attempts an `EventsDropped` marker. State and TurnHandle remain authoritative when best-effort events are lost.

`session::TurnHandle` contains only stable identities plus shared cancellation/completion state. One mutex orders cancel and completion, cancellation tokens carry only the exact-Turn signal, and Notify wakes all waiters after first-wins settlement. Successful wait results contain the confirmed durable `conversation::TurnTerminal` and Usage. Unknown/unavailable durability and actor termination remain typed diagnostic errors.

## SessionRuntime Ownership

`SessionRuntimeOptions` contains exactly a checked KernelConfig, immutable SessionBindings, and Tokio Handle. Construction enters the supplied Handle inside `catch_unwind`, constructs and drops a zero-duration sleep, and rejects a missing time driver without executing or blocking. The timer-enabled runtime must remain alive and actively driven during create, load, and shutdown. A non-Tokio caller is valid when that configured runtime is driven; a live but undriven current-thread runtime cannot provide progress.

Before owner spawn and before any await, OpenGuard attempts cleanup-watcher spawns on the configured Handle and `Handle::try_current()`. Each watcher captures the single-take payload, owner cancellation, and payload-claimed token. On first poll it panic-safely constructs and drops a zero-duration sleep in its executing runtime; without a time driver it exits before taking payload ownership. `run_open` signals claim immediately after its take. Watcher handles are retained and polled by mutable reference; after one returns a close result, loser handles may detach because the shared payload is empty and their tasks are finite under active runtime progress.

Create ordering is kernel → custom-limit spec → bindings → manifest → initialize/zero head → fresh instance → validated Idle/Healthy state and bounded events → ready. Load ordering is manifest/expected identity → bindings → identity-bound proof → paged replay/semantic validation → atomic restart repair → fresh instance → confirmed head/last terminal state → ready. No first snapshot event is emitted.

After ready, the idle owner task holds ConversationLog, kernel/spec/bindings lifetime evidence, state sender, InternalEventSink, and root cancellation. P4-B adds no ordinary command mailbox and no fake SessionHandle. Cancellation first publishes Closing to state, then closes ConversationLog, drops state/event senders, and returns a typed task exit.

Successful SessionRuntime retains the configured Handle. `shutdown(self)` first cancels, then enters that Handle only long enough to construct the timeout future; the enter guard is dropped before any await, so a non-Tokio executor can poll shutdown without a caller-runtime timer. Unexpected construction panic aborts and awaits the same owner task and returns ActorTerminated. Normal timeout still aborts and awaits that task before Timeout. Drop only cancels; if neither timer-capable runtime is actively driven, graceful close is not guaranteed.

Core catches the host-controlled panic boundaries: Model descriptor access, SessionLog future construction and polling, and the post-ready actor loop. These become typed failures and preserve their specified close attempts. Arbitrary Core allocation or invariant panics after ownership transfer are not a recoverable API error boundary and may skip graceful close, as may destruction of all timer-capable runtimes. Core deliberately does not add a shared-log worker solely to claim recovery from every possible internal panic.

## Checked DTOs

`ToolInvocation` accepts only bounded object-shaped JSON arguments and redacts arguments from `Debug`. `ToolSpec` exposes the exact public fields `name`, `description`, and `input_schema`, while its constructor and strict unknown-field deserializer enforce their bounds. Public `ToolOutput` contains only `content: BoundedText`, serializes as `{ "content": "..." }`, and never exposes a failure-status bit.

Input requests have bounded prompt/choice text and an explicit answer kind. Text answers reject empty, oversized, and control-character content. Choice answers reject non-object or extra-field wire shapes; their index is checked against the request before execution.

Progress is synchronous and nonblocking. `ToolProgressSink::emit` validates `completed <= total` and delegates to a private emitter using `try_send`-style semantics. Full, closed, invalid, and no-op sinks return `false` without waiting.

## Policy And Interaction

`ToolPolicy` is an asynchronous `Send + Sync + 'static` Port. It receives an owned `ToolPolicyRequest`, so the exact checked invocation and captured spec cross the seam without borrowing actor/session internals. Decisions are exactly `Allow`, bounded `Deny`, or `RequireApproval`; approval answers are exactly `AllowOnce` or `Deny`. Policy and approval `Debug` output reports safe identities, counts, risks, and byte lengths while redacting arguments, reasons, and prompts.

`session::PendingInteraction`, `InteractionKind`, and `InteractionAnswer` are process-local DTOs only. They validate answer-kind matching and delegate tool-input answer checks to the original checked request. They contain no serde representation, resume sender, callback, owner handle, Workspace, Store, or arbitrary continuation. The internal `TurnSuspension` owns the one-shot sender separately; TurnRunner now forwards that exact sender through RunnerEvent::Suspend, and the future P4-C actor will consume it exactly once without changing the public DTOs.

## Model Port

`model::Model` is the only public model execution Port. A loaded session binds one host-owned model directly; there is no registry, resolver, installation manager, endpoint policy, credential source, or concrete network adapter in the root crate. `ModelDescriptor` has exactly `model_ref`, `context_window`, `supported_reasoning`, and `supports_tools`. `ModelCallContext` carries exact session/instance/turn identity, a zero-based round, cancellation, and deadline without owner or capability handles.

`ModelRequest` contains only checked messages, tools, limits, and reasoning. `ModelStream` emits bounded typed `ModelEvent` values for text, reasoning, tool-call grammar, usage, and finish. Event `Debug` output reports only safe identities and byte counts. `ModelError` reports `DeliveryState::{NotStarted, Started, Unknown}`, retryability, and an optional retry-after hint; retryable/hint combinations are rejected unless delivery is `NotStarted`.

The old runner/context, batch Model gateway/provider lookup, Workspace, direct ConversationLog behavior, and old prompt/compaction implementation now live only under `agent::legacy`, `prompt::legacy`, and other `cfg(test)` migration modules. P5-A through P5-E1 now provide the final private drivers and ordinary TurnRunner. P4-C/P5-E2 replace the remaining legacy actor path with durable commit ownership, compaction integration, and terminal settlement.

## Legacy Boundary

The test-only actor/runner/storage path uses `LegacyTool`, `LegacyToolContext`, and `legacy_types` DTOs. `LegacyToolOutput` deliberately preserves the old `{ "text": ..., "is_error": ... }` JSON shape for legacy unit tests. Production `ModelMessage::Tool` uses only public `ToolOutput` plus `ToolResultOutcome`; the legacy conversion itself is `cfg(test)`.

The test-only old actor uses `legacy_state.rs`, `legacy_event.rs`, `legacy_event_stream.rs`, and `legacy_snapshot.rs`. Their modules are gated with actor, command, and old transcript at `session/mod.rs`. Broadcast, first-snapshot delivery, resync, old commands, and legacy terminal behavior are absent from the production SessionRuntime graph.

`agent/legacy_context.rs`, `agent/legacy_runner.rs`, `tools/legacy_context.rs`, `legacy_policy.rs`, `legacy_types.rs`, and `registry.rs` are staged private migration files. `legacy_policy.rs` retains the synchronous string-based decision flow only for the test-only old runner and is marked for P4-C/P6 deletion. Final `policy.rs` is the public typed Port and is part of the canonical Tool seam.

## Deferred Execution

The old top-level `runtime` directory, Runtime configuration/manager/error symbols, and legacy `SessionConfig` are physically removed. ToolRegistry, old actor/commands, legacy model lookup, workspace, and filesystem store remain test-only where P4-C/P5/P6/P7 unit evidence consumes them; SessionRuntime does not import or own them.

Concrete filesystem/process adapters and their tests were deleted in P3-B rather than replaced with defaults. Future concrete tools must be host-owned implementations of the public `Tool` Port, not reintroduced builtins or process policy modules.

P5-E1 now supplies the final ordinary TurnRunner without compaction. P4-C will add the final SessionHandle, state watch access, commands, transcript routing, actor-owned commit acknowledgements, and suspension state. P5-E2 will invoke CompactionDriver and add stale-head-checked Summary commits before final settlement. P4-B deliberately supports no submit path.

## Model Retry

The retry contract is implemented in ModelDriver: retry requires `retryable == true`, `delivery == NotStarted`, no semantic event in the attempt, remaining attempts, a valid delay no greater than 30 seconds, and remaining overall deadline. `Started` and `Unknown` are never retried. Any adapter error claiming NotStarted after an event is normalized to Started and stripped of retry metadata.

## Dependency Direction

Public Ports and SessionBindings do not import SessionRuntime, SessionHandle, Workspace, Store, registry lookup, direct I/O, or fanout. SessionRuntime depends only on checked config/bindings, ConversationLog, SessionLog, IDs, Tokio ownership primitives, state, and events. Owner/Port/DTO files remain below 500 lines, and the module DAG remains all singleton SCCs.
