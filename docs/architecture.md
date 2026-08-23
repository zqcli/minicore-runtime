# Current Architecture

This document describes the current v0.3 reset slice. The former v0.2 Runtime/session/provider graph remains compiled only as private migration scaffolding where later phases still need it; it is not a public compatibility facade.

## Public Spine

```text
Host
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

The public Tool seam is host-neutral. A Tool receives a checked `ToolInvocation` and a context containing only cancellation, deadline, and synchronous best-effort progress. Workspace roots, process capabilities, RPC clients, credentials, policy decisions, and runtime/session handles are not fields of `ToolContext`.

`ToolSetBuilder` is the only mutable phase. Registration captures a checked `ToolSpec`, validates its public fields, and records the first duplicate name, spec panic, or invalid-spec mutation; `build()` returns that error or a frozen immutable set. `specs_for` deterministically returns only registered enabled specs and omits unknown names; `SessionBindings::validate` rejects unknown enabled names and checks every frozen spec against semantic budgets. Cloned sets share the same `Arc` tool values and support concurrent execution. There is no public `ToolRegistry`, no default concrete adapter set, and no builtin/process implementation in `src/tools`.

## Session Bindings

`session::SessionBindings` is the immutable adapter bundle for one future loaded session. Its exact fields are one direct Model, one ToolSet, and optional ToolPolicy, ContextProvider, and CompactionStrategy values. It contains no Clock, runtime/task handle, log, store, workspace, owner, or metadata. Construction installs no defaults.

Validation is pure and does not invoke adapter futures. The only adapter call is `Model::descriptor`, cloned inside `catch_unwind`; a panic becomes the payload-free `ModelPanicked` error. Validation then checks limits/specs, descriptor integrity and compatibility, enabled tool support/policy/registration, all frozen ToolSpec semantic budgets, and compaction strategy presence. Disabled compaction permits a strategy without invoking it, and optional context is never invoked.

## Checked DTOs

`ToolInvocation` accepts only bounded object-shaped JSON arguments and redacts arguments from `Debug`. `ToolSpec` exposes the exact public fields `name`, `description`, and `input_schema`, while its constructor and strict unknown-field deserializer enforce their bounds. Public `ToolOutput` contains only `content: BoundedText`, serializes as `{ "content": "..." }`, and never exposes a failure-status bit.

Input requests have bounded prompt/choice text and an explicit answer kind. Text answers reject empty, oversized, and control-character content. Choice answers reject non-object or extra-field wire shapes; their index is checked against the request before execution.

Progress is synchronous and nonblocking. `ToolProgressSink::emit` validates `completed <= total` and delegates to a private emitter using `try_send`-style semantics. Full, closed, invalid, and no-op sinks return `false` without waiting.

## Policy And Interaction

`ToolPolicy` is an asynchronous `Send + Sync + 'static` Port. It receives an owned `ToolPolicyRequest`, so the exact checked invocation and captured spec cross the seam without borrowing actor/session internals. Decisions are exactly `Allow`, bounded `Deny`, or `RequireApproval`; approval answers are exactly `AllowOnce` or `Deny`. Policy and approval `Debug` output reports safe identities, counts, risks, and byte lengths while redacting arguments, reasons, and prompts.

`session::PendingInteraction`, `InteractionKind`, and `InteractionAnswer` are process-local DTOs only. They validate answer-kind matching and delegate tool-input answer checks to the original checked request. They contain no serde representation, resume sender, callback, owner handle, Workspace, Store, or arbitrary continuation. One-shot consumption remains a future actor responsibility rather than DTO state.

## Model Port

`model::Model` is the only public model execution Port. A loaded session binds one host-owned model directly; there is no registry, resolver, installation manager, endpoint policy, credential source, or concrete network adapter in the root crate. `ModelDescriptor` has exactly `model_ref`, `context_window`, `supported_reasoning`, and `supports_tools`. `ModelCallContext` carries exact session/instance/turn identity, a zero-based round, cancellation, and deadline without owner or capability handles.

`ModelRequest` contains only checked messages, tools, limits, and reasoning. `ModelStream` emits bounded typed `ModelEvent` values for text, reasoning, tool-call grammar, usage, and finish. Event `Debug` output reports only safe identities and byte counts. `ModelError` reports `DeliveryState::{NotStarted, Started, Unknown}`, retryability, and an optional retry-after hint; retryable/hint combinations are rejected unless delivery is `NotStarted`.

The current private runner still consumes batch `ModelResponse` through `LegacyModelGateway`, `LegacyModelProvider`, and `LegacyProviderRegistry`. Those files preserve old actor tests without exposing provider lookup. P5 will replace them with an internal `ModelDriver` that assembles streams, catches start/stream poll panics as `Panicked`, applies cancellation/deadlines, and owns delivery-safe retry.

## Legacy Boundary

The old actor/runner/storage path uses private `LegacyTool`, `LegacyToolContext`, and `legacy_types` DTOs. `LegacyToolOutput` deliberately preserves the old `{ "text": ..., "is_error": ... }` JSON shape. Prompt-facing `ModelMessage::Tool` uses only public `ToolOutput` plus `ToolResultOutcome`; the crate-private conversion maps the legacy status bit before provider encoding, while physical conversation entries retain the legacy DTO.

`legacy_context.rs`, `legacy_policy.rs`, `legacy_types.rs`, and `registry.rs` are staged private migration files. `legacy_policy.rs` retains the synchronous string-based decision flow only for the old runner and is marked for P5/P6 deletion. Final `policy.rs` is the public typed Port and is part of the canonical Tool seam.

## Deferred Owners

The old `runtime` and `workspace` modules are private in `src/lib.rs`; `Runtime`, `RuntimeConfig`, `RuntimeConfigBuilder`, `SessionConfig`, `SessionSummary`, `RuntimeError`, `ToolRegistry`, and `ToolRegistryBuilder` are crate-private or removed from root/module exports. The old actor/session/storage/legacy-model/workspace implementation remains only for migration work through P6.

Concrete filesystem/process adapters and their tests were deleted in P3-B rather than replaced with defaults. Future concrete tools must be host-owned implementations of the public `Tool` Port, not reintroduced builtins or process policy modules.

P4/P5 will introduce the SessionRuntime owner and its acceptance contract. P4 must validate bindings against the loaded manifest before constructing `LoadCompatibilityValidated` and finishing replay. P3-E does not expose that proof or wire the transitional actor.

## Model Retry

The final retry contract is explicit: `retryable == true` and `delivery == NotStarted`. `Started` and `Unknown` are never automatically retried. The transitional runner retains its old retry loop until P5 moves start/stream consumption, panic catching, cancellation, deadlines, and assembly into `ModelDriver`.

## Dependency Direction

Public Ports and SessionBindings do not import Runtime, SessionHandle, Workspace, Store, registry lookup, direct I/O, or fanout. The final Model, Tool, ToolPolicy, and SessionBindings files remain below the 500-line limit. Transitional private owners may still depend on the old graph until their scheduled phase, but no new public alias or wrapper may be added.
