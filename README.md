# MiniCore Runtime

MiniCore Runtime is an embeddable Rust 2024 core for typed, bounded session execution. The current v0.3 reset exposes host-neutral Model, Tool, policy, context, compaction, and storage Ports; the former v0.2 Runtime/configuration facade remains crate-private transitional implementation until the P6 migration boundary.

## Implemented Core

- Checked identifiers and bounded text/JSON values.
- Public `tools::Tool`, immutable `tools::ToolSet`, async `tools::ToolPolicy`, checked invocations, typed approval decisions, content-only outputs, input requests, cancellation, deadlines, and best-effort progress.
- `ToolSet` registration is explicit, deterministic, duplicate-safe, and panic-safe while freezing tool specs for shared cloned sets.
- Typed context and compaction Ports with immutable DTOs.
- Public direct `model::Model` streaming Port with checked descriptors, contexts, requests, events, delivery-aware errors, cancellation, and deadlines.
- Public `SessionBindings` freezes one direct Model, ToolSet, and optional policy/context/compaction adapters, then validates them purely against `SessionSpec` and `SemanticLimits`.
- Public process-local `SessionState`, bounded single-consumer `SessionEventStream`, and exact-turn `TurnHandle` foundations with redacted diagnostics and no snapshot/broadcast recovery protocol.
- Crate-private legacy runner/session/storage execution seam, including exact legacy tool-result wire preservation.
- Conversation, storage, and workspace implementations remain transitional internal slices while their v0.3 owners are migrated.
- No concrete builtin, process adapter, model network adapter, default tool set, or network service is installed by the public seams.

## Install and MSRV

The crate targets Rust `1.85` and edition 2024. The repository's pinned-toolchain and offline architecture gates are the authoritative checks for this transitional phase.

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

`ToolContext` contains only cancellation, deadline, and nonblocking progress. `ToolInvocation` validates object-shaped JSON arguments and redacts them from `Debug`. `ToolSet::specs_for` returns deterministic registered specs and omits unknown names; invalid public-field mutations are rejected during `build()`, while `SessionBindings::validate` rejects unknown enabled tools and enforces semantic ToolSpec budgets. `ToolOutput` serializes as `{ "content": "..." }`; `ModelMessage::Tool` carries its public `ToolResultOutcome`, while legacy `{ "text": "...", "is_error": ... }` values belong only to physical private storage.

`ToolPolicy` is an asynchronous host Port over an owned, checked `ToolPolicyRequest`. Decisions are exactly `Allow`, bounded `Deny`, or `RequireApproval`; approval answers are typed `AllowOnce`/`Deny`. Process-local `session::PendingInteraction` values pair approval and tool-input requests with matching typed answers without carrying resume senders, callbacks, owner handles, or durable state.

## Session Bindings

`SessionBindings::new` accepts exactly one `Arc<dyn Model>`, an immutable `ToolSet`, and optional `ToolPolicy`, `ContextProvider`, and `CompactionStrategy` adapters. It installs no default policy or adapter. `validate` is pure: it checks limits/specs, catches descriptor panics, matches model identity/reasoning/tool support, rejects missing enabled tools or policy, validates every frozen ToolSpec against semantic count/name/description/compact-schema budgets, and requires a strategy only when compaction is enabled. It never starts a Model, Tool, policy, context provider, or compaction future.

## Typed Model API

Each loaded session will bind one host-owned `Arc<dyn model::Model>` directly. `Model::start` receives a host-neutral checked `ModelRequest` and exact process-local `ModelCallContext`, then returns a typed `ModelStream`. Descriptors contain only a `ModelRef`, context window, supported reasoning set, and tool support. The core exposes no registry, resolver, endpoint, credential, or concrete network adapter.

Stream events are typed text/reasoning deltas, tool-call boundaries, usage, and finish events. `DeliveryState` is exactly `NotStarted`, `Started`, or `Unknown`; automatic retry is only meaningful when an error is explicitly retryable and delivery is `NotStarted`. Stream assembly, panic catching, cancellation polling, and retry ownership remain the P5 `ModelDriver` cutover.

## State, Events, And Turns

`SessionState` is the lightweight authoritative current-state DTO: four statuses, healthy/degraded health, exact active Turn and pending Interaction, confirmed conversation sequence, and the latest durable terminal outcome. Its validator rejects illegal status/turn/interaction combinations. It is process-local and has no serde representation.

`SessionEventStream` is one bounded Tokio mpsc receiver and is not Clone. Internal publication is synchronous best effort: queue overflow drops the current event, counts losses, and attempts an `EventsDropped` marker before a later ordinary event. Events contain bounded deltas and summaries, never raw tool output, arguments, answers, or adapter errors.

`TurnHandle` is Clone + Send + Sync and controls one exact Turn. Cancellation and completion share one mutex linearization point; cancellation is first-request-only, completion is first-wins, multiple waiters receive the same durable outcome, and dropping handles does not cancel. The P4-A foundation is not yet wired into the transitional actor; P4-B owns that migration.

## Public Modules

| Module | Public responsibility |
| --- | --- |
| `config` | Checked kernel/session-spec DTOs and configuration errors; legacy Runtime configuration is private |
| `conversation` | Canonical v0.3 conversation entries, loading, transcript, and read-only views |
| `context` | Typed context provider Port and validated context bundles |
| `compaction` | Typed compaction strategy Port and immutable candidates/proposals |
| `error` | Public error summaries; legacy session errors remain private |
| `ids` | Checked session, instance, turn, interaction, tool-call, and context-source identifiers |
| `model` | Direct streaming `Model` Port, checked descriptor/context/request/events, and delivery-aware errors |
| `session` | Bindings, interactions, lightweight state, bounded events, and exact TurnHandle foundations; SessionRuntime ownership remains deferred |
| `storage` | `SessionLog` Port and storage DTOs |
| `tools` | `Tool`, immutable `ToolSet`, async `ToolPolicy`, approval, invocation/context/progress/output DTOs |

The old `runtime` and `workspace` modules, `RuntimeConfig`, `SessionConfig`, `SessionSummary`, `ToolRegistry`, synchronous legacy policy, and legacy model lookup are not public compatibility surfaces. `model/legacy_*`, `tools/legacy_*`, and `session/legacy_*` are private migration scaffolding scheduled for P4-B/P5/P6 deletion.

## Transitional Scope

The removed v0.2 Runtime, concrete builtin/process/model adapters, and their integration tests are baseline evidence for the reset, not current extension contracts. Focused P3-B through P4-A contracts cover Ports, bindings, state/events, and TurnHandle primitives. SessionRuntime/actor integration remains deferred to P4-B/P5; this revision does not claim that the full replacement is complete.

The former Runtime/model-network examples and root integration suites were removed with their public facades. The standalone `provider-gate/` package remains independent historical protocol evidence and does not establish a root-crate model adapter API.

## Testing

The deterministic offline checks are Python/docs/diff/architecture checks. Rust validation for this phase is intentionally run remotely by the project workflow; no local Rust build or test command is part of the current P4-A review.

```bash
python3 scripts/check_architecture.py
python3 scripts/check_v03_architecture.py --self-test
python3 scripts/check_docs.py
python3 -m py_compile scripts/check_architecture.py scripts/check_docs.py scripts/check_v03_architecture.py
git diff --check
```

## Breaking Change

This is a breaking v0.3 reset with no Runtime, ToolRegistry, builtin, process, or compatibility facade. Historical v0.2 design material remains under `docs/archive/v2/` and is not current public authority.
