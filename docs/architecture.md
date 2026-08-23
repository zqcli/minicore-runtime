# Current Architecture

This document describes the current v0.3 reset slice. The former v0.2 Runtime/session/provider graph remains compiled only as private migration scaffolding where later phases still need it; it is not a public compatibility facade.

## Public Spine

```text
Host
 ├── tools::ToolSet ── Arc<dyn tools::Tool>
 │       └── ToolContext { cancellation, deadline, progress }
 ├── tools::ToolPolicy
 │       └── ToolPolicyRequest { invocation, spec, cancellation, deadline }
 ├── context::ContextProvider
 ├── compaction::CompactionStrategy
 └── storage::SessionLog
```

The public Tool seam is host-neutral. A Tool receives a checked `ToolInvocation` and a context containing only cancellation, deadline, and synchronous best-effort progress. Workspace roots, process capabilities, RPC clients, credentials, policy decisions, and runtime/session handles are not fields of `ToolContext`.

`ToolSetBuilder` is the only mutable phase. Registration captures a checked `ToolSpec`, validates its public fields, and records the first duplicate name, spec panic, or invalid-spec mutation; `build()` returns that error or a frozen immutable set. `specs_for` deterministically returns only registered enabled specs and omits unknown names; SessionBindings validation owns unknown-enabled rejection in the next migration phase. Cloned sets share the same `Arc` tool values and support concurrent execution. There is no public `ToolRegistry`, no default concrete adapter set, and no builtin/process implementation in `src/tools`.

## Checked DTOs

`ToolInvocation` accepts only bounded object-shaped JSON arguments and redacts arguments from `Debug`. `ToolSpec` exposes the exact public fields `name`, `description`, and `input_schema`, while its constructor and strict unknown-field deserializer enforce their bounds. Public `ToolOutput` contains only `content: BoundedText`, serializes as `{ "content": "..." }`, and never exposes a failure-status bit.

Input requests have bounded prompt/choice text and an explicit answer kind. Text answers reject empty, oversized, and control-character content. Choice answers reject non-object or extra-field wire shapes; their index is checked against the request before execution.

Progress is synchronous and nonblocking. `ToolProgressSink::emit` validates `completed <= total` and delegates to a private emitter using `try_send`-style semantics. Full, closed, invalid, and no-op sinks return `false` without waiting.

## Policy And Interaction

`ToolPolicy` is an asynchronous `Send + Sync + 'static` Port. It receives an owned `ToolPolicyRequest`, so the exact checked invocation and captured spec cross the seam without borrowing actor/session internals. Decisions are exactly `Allow`, bounded `Deny`, or `RequireApproval`; approval answers are exactly `AllowOnce` or `Deny`. Policy and approval `Debug` output reports safe identities, counts, risks, and byte lengths while redacting arguments, reasons, and prompts.

`session::PendingInteraction`, `InteractionKind`, and `InteractionAnswer` are process-local DTOs only. They validate answer-kind matching and delegate tool-input answer checks to the original checked request. They contain no serde representation, resume sender, callback, owner handle, Workspace, Store, or arbitrary continuation. One-shot consumption remains a future actor responsibility rather than DTO state.

## Legacy Boundary

The old actor/runner/storage path uses private `LegacyTool`, `LegacyToolContext`, and `legacy_types` DTOs. `LegacyToolOutput` deliberately preserves the old `{ "text": ..., "is_error": ... }` JSON shape. Prompt-facing `ModelMessage::Tool` uses only public `ToolOutput` plus `ToolResultOutcome`; the crate-private conversion maps the legacy status bit before provider encoding, while physical conversation entries retain the legacy DTO.

`legacy_context.rs`, `legacy_policy.rs`, `legacy_types.rs`, and `registry.rs` are staged private migration files. `legacy_policy.rs` retains the synchronous string-based decision flow only for the old runner and is marked for P5/P6 deletion. Final `policy.rs` is the public typed Port and is part of the canonical Tool seam.

## Deferred Owners

The old `runtime` and `workspace` modules are private in `src/lib.rs`; `Runtime`, `RuntimeConfig`, `RuntimeConfigBuilder`, `SessionConfig`, `SessionSummary`, `RuntimeError`, `ToolRegistry`, and `ToolRegistryBuilder` are crate-private or removed from root/module exports. The old actor/session/storage/model/provider/workspace implementation remains only for migration work through P6.

Concrete filesystem/process adapters and their tests were deleted in P3-B rather than replaced with defaults. Future concrete tools must be host-owned implementations of the public `Tool` Port, not reintroduced builtins or process policy modules.

P4/P5 will introduce the SessionRuntime owner and its acceptance contract. The typed P3-C seam does not yet wire policy suspension or interaction consumption into the transitional actor/runner.

## Provider Retry

Provider retry behavior remains owned by the transitional model/agent implementation. P3-B does not change provider retry policy or expose a provider/runtime facade.

## Dependency Direction

Public Ports do not import Runtime, SessionHandle, Workspace, Store, Model, provider lookup, direct I/O, or fanout. The final Tool and ToolPolicy files are adapter-neutral and remain below the 500-line Port limit. Transitional private owners may still depend on the old graph until their scheduled phase, but no new public alias or wrapper may be added.
