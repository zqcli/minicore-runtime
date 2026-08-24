# MiniCore Runtime

MiniCore Runtime is an embeddable single-session Agent Execution Kernel. One `SessionRuntime` owns exactly one loaded Session. A Host manages multiple `SessionRuntime` instances and all concrete storage, model, tool, workspace, and product capabilities. The crate exposes host-neutral typed Ports and one durable Conversation owner; it does not provide a multi-session manager or concrete adapter catalog.

## Implemented Core

- Checked identifiers and bounded text/JSON values.
- Public `tools::Tool`, immutable `tools::ToolSet`, async `tools::ToolPolicy`, checked invocations, typed approval decisions, content-only outputs, input requests, cancellation, deadlines, and best-effort progress.
- `ToolSet` registration is explicit, deterministic, duplicate-safe, and panic-safe while freezing tool specs for shared cloned sets.
- Typed context and compaction Ports with immutable DTOs.
- Public direct `model::Model` streaming Port with checked descriptors, contexts, requests, events, delivery-aware errors, cancellation, and deadlines.
- Crate-private `ModelDriver` with strict stream assembly, panic isolation, overall deadlines, cancellation, delivery-safe retry, lossy delta progress, and checked tool-call grammar.
- Crate-private `ToolDriver` and one-shot suspension protocol with frozen-spec policy decisions, approval/input interactions, panic-safe execution, child cancellation, bounded outputs, and lossy progress.
- Crate-private `ContextDriver` plus the final deterministic `PromptBuilder`, with one-provider deadline/panic isolation, canonical context sorting, validator-proved latest-summary conversation projection, exact frozen tools, stable context headers, and exact serialized-request output-reserved budgeting.
- Crate-private `CompactionDriver` with conversation-owned canonical candidates, completed-boundary-only proposals, strategy deadline/panic isolation, scoped child cancellation, bounded summaries, exact Turn-versus-Port deadline provenance, and stale-head proof results.
- Crate-private P5-E1/P5-E2 `TurnRunner` context/model/tool/compaction loop with durable rounds, cancellation-first control checks, exact prefix acknowledgements, proactive best-effort compaction, one-shot forced overflow recovery, stale-head Summary commit requests using the ordinary critical commit taxonomy, internal started-progress enqueue before suspension, sequential tools, and conservative usage on every outcome. Lossy public `ToolStarted` delivery has no ordering guarantee relative to a critical interaction suspension.
- Public `SessionBindings` freezes one direct Model, ToolSet, and optional policy/context/compaction adapters, then validates them purely against `SessionSpec` and `SemanticLimits`.
- Public process-local `SessionState`, bounded single-consumer `SessionEventStream`, and exact-turn `TurnHandle` foundations with redacted diagnostics and no snapshot/broadcast recovery protocol.
- Public non-Clone `SessionRuntime` create/load/take-events/handle/shutdown lifecycle with spawn-first OpenGuard cancellation, proof-gated replay/recovery, one durable log owner, and typed open/shutdown failures.
- Public cloneable `SessionHandle` with bounded submit/answer/transcript commands and watch state; SessionActor owns runner acknowledgements, durable unresolved-tool suspension proof, first-wins active commit failure latches, settlement, and shutdown durability propagation.
- No concrete builtin, process adapter, model network adapter, default tool set, or network service is installed by the public seams.

## Install and MSRV

The crate targets Rust `1.85` and edition 2024. The v0.3 implementation, regenerated lockfile, remote Rust gates, P8 documentation, Linux functional acceptance, and native macOS and Windows CI matrix ([run 32705101762](https://github.com/zqcli/minicore-runtime/actions/runs/32705101762)) are complete and ready for publication.

The host owns tool capabilities. A host implementation captures workspace, process, RPC, or other authority inside its `Tool` implementation rather than receiving those capabilities through `ToolContext`.

## Typed Tool API

A host registers typed tools in an immutable `ToolSet`. Registration returns the builder; the first duplicate, spec-panic, or invalid-spec error is retained and `build()` returns it, otherwise `build()` freezes the set as a checked result.

```rust,no_run
use std::time::{Duration, Instant};

use minicore_runtime::tools::{Tool, ToolContext, ToolSet};
use tokio_util::sync::CancellationToken;

fn install_tools(host_tool: impl Tool + 'static) -> Result<ToolSet, Box<dyn std::error::Error>> {
    let mut builder = ToolSet::builder();
    builder.register(host_tool);
    let tools = builder.build()?;
    let _context = ToolContext {
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
        progress: Default::default(),
    };
    Ok(tools)
}
```

## Host Integration

A Host owns the collection of loaded sessions and all adapter acquisition. Multiple owners may share one Tokio runtime and shared `Arc` Ports while retaining independent state, cancellation, logs, and shutdown:

```rust,ignore
use std::collections::HashMap;

use minicore_runtime::{SessionId, SessionRuntime};

type LoadedSessions = HashMap<SessionId, SessionRuntime>;
```

Listing, deletion, writer leases, global limits, idle eviction, and shutdown-all belong to that Host collection. Core receives one already opened `Box<dyn SessionLog>` and one immutable `SessionBindings` bundle per loaded Session.

`SessionSpec` and `SessionManifest` constructors and deserialization enforce absolute structural bounds, while `SessionRuntime::create` and `SessionRuntime::load` enforce the Host-configured `KernelConfig.limits`.

The complete load/observe/submit/shutdown shape uses only current public API and Host-supplied placeholders:

```rust,no_run
use std::error::Error;

use minicore_runtime::{
    KernelConfig, SessionBindings, SessionEventEnvelope, SessionId, SessionLog, SessionRuntime,
    SessionRuntimeOptions, TurnOptions, TurnOutcome, UserInput,
};

pub fn render_event(_envelope: SessionEventEnvelope) {}

pub async fn run_loaded_session(
    session_id: SessionId,
    opened_log: Box<dyn SessionLog>,
    bindings: SessionBindings,
) -> Result<TurnOutcome, Box<dyn Error>> {
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked()?,
        bindings,
        tokio::runtime::Handle::current(),
    )?;
    let mut session = SessionRuntime::load(session_id, opened_log, options).await?;

    let events_result = session.take_events();
    let handle = session.handle();
    let state_watch = handle.watch_state();
    let _initial_state = state_watch.borrow().clone();

    let (event_task, turn_result) = match events_result {
        Ok(mut events) => {
            let event_task = tokio::spawn(async move {
                while let Some(envelope) = events.recv().await {
                    render_event(envelope);
                }
            });
            let turn_result = match UserInput::text("Inspect the repository") {
                Ok(input) => match handle.submit(input, TurnOptions::default()).await {
                    Ok(turn) => turn
                        .wait()
                        .await
                        .map_err(|error| Box::new(error) as Box<dyn Error>),
                    Err(error) => Err(Box::new(error) as Box<dyn Error>),
                },
                Err(error) => Err(Box::new(error) as Box<dyn Error>),
            };
            (Some(event_task), turn_result)
        }
        Err(error) => (None, Err(Box::new(error) as Box<dyn Error>)),
    };

    let shutdown_result = session.shutdown().await;
    let event_result = match event_task {
        Some(task) => task
            .await
            .map_err(|error| Box::new(error) as Box<dyn Error>),
        None => Ok(()),
    };

    shutdown_result.map_err(|error| Box::new(error) as Box<dyn Error>)?;
    event_result?;
    turn_result
}
```

After owner acquisition, the example captures event/submit/wait failures instead of returning early, always awaits `shutdown`, and then always joins the event task after shutdown closes the actor-owned stream. Error precedence is shutdown first, event-task join second, and the captured event/submit/wait result last. Runtime Drop sends cancellation but is not the durability barrier.

The injectable Ports are direct `Model`, `Tool`/`ToolSet`, `ToolPolicy`, `ContextProvider`, `CompactionStrategy`, and `SessionLog`. Host decorators or composites may implement these traits; Core has no plugin manager, service locator, provider registry, or lifecycle hook bus.

`SessionRuntime::load` validates the manifest and bindings, replays the canonical Conversation, and repairs an unfinished Turn before readiness by atomically appending cancelled results for unresolved calls followed by `CancelledByRestart`. It does not restore Model/Tool continuations, approval prompts, ToolInput waits, event cursors, or actor tasks.

`ToolContext` contains only cancellation, deadline, and nonblocking progress. `ToolInvocation` validates object-shaped JSON arguments and redacts them from `Debug`. `ToolSet::specs_for` returns deterministic registered specs and omits unknown names; invalid public-field mutations are rejected during `build()`, while `SessionBindings::validate` rejects unknown enabled tools and enforces semantic ToolSpec budgets. `ToolOutput` serializes as `{ "content": "..." }`; `ModelMessage::Tool` carries its public `ToolResultOutcome`.

`ToolPolicy` is an asynchronous host Port over an owned, checked `ToolPolicyRequest`. Decisions are exactly `Allow`, bounded `Deny`, or `RequireApproval`; approval answers are typed `AllowOnce`/`Deny`. Process-local `session::PendingInteraction` values pair approval and tool-input requests with matching typed answers without carrying resume senders, callbacks, owner handles, or durable state.

## Session Bindings

`SessionBindings::new` accepts exactly one `Arc<dyn Model>`, an immutable `ToolSet`, and optional `ToolPolicy`, `ContextProvider`, and `CompactionStrategy` adapters. It installs no default policy or adapter. `validate` is pure: it checks limits/specs, catches descriptor panics, matches model identity/reasoning/tool support, rejects missing enabled tools or policy, validates every frozen ToolSpec against semantic count/name/description/compact-schema budgets, and requires a strategy only when compaction is enabled. It never starts a Model, Tool, policy, context provider, or compaction future.

## Typed Model API

Each loaded session will bind one host-owned `Arc<dyn model::Model>` directly. `Model::start` receives a host-neutral checked `ModelRequest` and exact process-local `ModelCallContext`, then returns a typed `ModelStream`. Descriptors contain only a `ModelRef`, context window, supported reasoning set, and tool support. The core exposes no registry, resolver, endpoint, credential, or concrete network adapter.

Stream events are typed text/reasoning deltas, tool-call boundaries, usage, and finish events. `DeliveryState` is exactly `NotStarted`, `Started`, or `Unknown`; `ModelError` is a structured type with explicit constructors (`not_started`, `started`, `unknown`, `permanent`) and `RetryHint::{Never, Retryable}`. Automatic retry occurs strictly when delivery is `NotStarted` and `retry_hint` is `Retryable`. Stream assembly, panic catching, cancellation polling, and retry ownership remain the P5 `ModelDriver` cutover.

## State, Events, And Turns

`SessionState` is the lightweight authoritative current-state DTO: four statuses, healthy/degraded health, exact active Turn and pending Interaction, confirmed conversation sequence, and the latest durable terminal outcome. Its validator rejects illegal status/turn/interaction combinations. It is process-local and has no serde representation.

`SessionEventStream` is one bounded Tokio mpsc receiver and is not Clone. Internal publication is synchronous best effort: every event carries the accumulated `dropped_before` count, queue overflow drops only the current event and saturating-counts it, and a closed receiver returns without growing the count. Events contain bounded deltas and summaries, never raw tool output, arguments, answers, or adapter errors.

`TurnHandle` is Clone + Send + Sync and controls one exact Turn. Cancellation and completion share one mutex linearization point; cancellation is first-request-only, completion is first-wins, multiple waiters receive the same durable outcome, and dropping handles does not cancel.

`SessionHandle::transcript` returns paginated committed history. Invalid cursors or limits return `SessionError::InvalidInput` without altering health. Transient storage errors (`Unavailable` [retryable] or `Internal` [non-retryable]) return `TranscriptUnavailable` while preserving `Healthy` state; a `Closed` log returns `SessionError::Closed`. Storage consistency violations (`Conflict`, `Corrupt`, `UnknownOutcome`, page contract violations, or projection mismatches) transition health to `Degraded`, cancel any active turn without appending a fabricated terminal, and reject subsequent `submit` and `answer` commands.

## Session Runtime

`SessionRuntimeOptions` fixes one checked `KernelConfig`, immutable `SessionBindings`, and Host-selected Tokio Handle. Construction synchronously and panic-safely enters that Handle and creates then drops a zero-duration sleep, rejecting runtimes without an enabled time driver. The runtime must remain alive and actively driven while create, load, or shutdown is in progress; a live but undriven current-thread runtime cannot advance Tokio work. A non-Tokio caller is supported when the configured runtime is timer-enabled and actively driven.

Before spawning the owner or awaiting anything, OpenGuard installs cleanup watchers on the configured and current Tokio runtimes. A watcher first verifies that its executing runtime has a time driver; a no-time fallback exits without taking the payload. `run_open` signals claim immediately after taking the payload. Cancellation before claim lets at most one timer-capable watcher close the raw log. After the first successful close result, remaining JoinHandles may be detached: those loser tasks can only observe an empty payload and are finite when their runtimes are actively driven. Successful SessionRuntime retains the configured Handle, and shutdown constructs its timeout while synchronously entered into that Handle before polling it from any executor.

Core isolates host-controlled panic boundaries: Model descriptor access, SessionLog future construction/polling, and the post-ready actor loop. Those paths return typed failures and retain their defined close behavior. Arbitrary Core allocation or invariant panics after ownership transfer are not a recoverable API error boundary and may skip graceful close, as may destruction of every runtime capable of driving cleanup. Core does not claim that every possible panic is converted into a close-complete error.

P4-C adds the final cloneable `SessionHandle`, bounded submit/answer/transcript commands, state watch access, actor-owned runner acknowledgements and interactions, durable terminal settlement, and `SessionRuntime::handle()`. The non-Clone SessionRuntime remains the shutdown/log/task owner; SessionHandle Drop has no lifecycle effect.

The private integration test `post_ready_actor_panic_joins_pending_runner_before_close` creates a ready SessionRuntime, returns a TurnHandle, waits until the Model future is pending, triggers the keyed active-Turn actor panic, and proves runner/Model Drop precedes the sole log close. It also proves RuntimeTerminated Turn completion, legal event/state channel closure, and ActorTerminated shutdown without timeout or detached work.

## Public Modules

| Module | Public responsibility |
| --- | --- |
| `config` | Checked kernel/session-spec DTOs and configuration errors |
| `conversation` | Canonical v0.3 conversation entries, loading, transcript, and read-only views |
| `context` | Typed context provider Port and validated context bundles |
| `compaction` | Typed compaction strategy Port and immutable candidates/proposals |
| `error` | Redacted diagnostics and typed session/log/open/shutdown/turn errors |
| `ids` | Checked session, instance, turn, interaction, tool-call, and context-source identifiers |
| `model` | Direct streaming `Model` Port, checked descriptor/context/request/events, and delivery-aware errors |
| `session` | Single-session owner lifecycle, bindings, interactions, lightweight state, bounded events, and exact TurnHandle foundations |
| `storage` | `SessionLog` Port and storage DTOs |
| `tools` | `Tool`, immutable `ToolSet`, async `ToolPolicy`, approval, invocation/context/progress/output DTOs |

The old top-level `runtime` module, manager graph, legacy execution graph, workspace implementation, filesystem store, provider lookup, and concrete tool/model adapters are physically absent. Hosts own adapter acquisition and any multi-session repository or supervisor.

## Non-Goals

Core does not list/delete Sessions, acquire writer leases, choose a persistence format, own a Workspace, start processes, install model providers or builtin tools, manage credentials, enforce process-global scheduling, replay events, restore in-memory interactions after restart, orchestrate remote agents, load plugins, or publish a multi-session shutdown API. These are deliberate Host/product boundaries, not hidden adapters.

## Breaking Scope

The removed v0.2 Runtime, concrete builtin/process/model adapters, and their integration tests are baseline evidence, not compatibility contracts. Focused P3-B through P5-E2/P4-C contracts cover Ports, bindings, state/events, SessionHandle/TurnHandle, create/load recovery, commands, runner integration, interactions, transcript, settlement, and shutdown.

The former Runtime/model-network examples and root integration suites were removed with their public facades. The standalone `provider-gate/` package remains independent historical protocol evidence and does not establish a root-crate model adapter API.

## Testing

The deterministic offline checks are Python/docs/diff/architecture checks. Rust validation is run remotely by the project workflow.

```bash
python3 scripts/check_architecture.py
python3 scripts/check_v03_architecture.py --self-test
python3 scripts/check_docs.py
python3 -m py_compile scripts/check_architecture.py scripts/check_docs.py scripts/check_v03_architecture.py
git diff --check
```

## Breaking Change

This is a breaking v0.3 reset with no Runtime, ToolRegistry, builtin, process, or compatibility facade. Historical v0.2 design material remains under `docs/archive/v2/` and is not current public authority.

Upgrade and release-candidate evidence: [v0.2-to-v0.3 migration](docs/migrations/v0.2-to-v0.3.md), [AT-K acceptance matrix](docs/acceptance-v0.3.md), and [v0.3 release note](docs/release-v0.3.md).
