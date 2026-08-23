# MiniCore Runtime

MiniCore Runtime is an embeddable Rust 2024 core for typed, bounded session execution. The current v0.3 reset exposes the host-neutral tool execution seam first; the former v0.2 Runtime/configuration facade remains crate-private transitional implementation until the P6 migration boundary.

## Implemented Core

- Checked identifiers and bounded text/JSON values.
- Public `tools::Tool`, immutable `tools::ToolSet`, async `tools::ToolPolicy`, checked invocations, typed approval decisions, content-only outputs, input requests, cancellation, deadlines, and best-effort progress.
- `ToolSet` registration is explicit, deterministic, duplicate-safe, and panic-safe while freezing tool specs for shared cloned sets.
- Typed context and compaction Ports with immutable DTOs.
- Crate-private legacy runner/session/storage execution seam, including exact legacy tool-result wire preservation.
- Model/provider, conversation, storage, and workspace implementations remain transitional internal slices while their v0.3 owners are migrated.
- No concrete builtin, process adapter, default tool set, provider credential, or network service is installed by the public Tool seam.

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

`ToolContext` contains only cancellation, deadline, and nonblocking progress. `ToolInvocation` validates object-shaped JSON arguments and redacts them from `Debug`. `ToolSet::specs_for` returns deterministic registered specs and omits unknown names; invalid public-field mutations are rejected during `build()`; SessionBindings validation owns unknown-enabled rejection in the next migration phase. `ToolOutput` serializes as `{ "content": "..." }`; `ModelMessage::Tool` carries its public `ToolResultOutcome`, while legacy `{ "text": "...", "is_error": ... }` values belong only to physical private storage.

`ToolPolicy` is an asynchronous host Port over an owned, checked `ToolPolicyRequest`. Decisions are exactly `Allow`, bounded `Deny`, or `RequireApproval`; approval answers are typed `AllowOnce`/`Deny`. Process-local `session::PendingInteraction` values pair approval and tool-input requests with matching typed answers without carrying resume senders, callbacks, owner handles, or durable state.

## Public Modules

| Module | Public responsibility |
| --- | --- |
| `config` | Checked kernel/session-spec DTOs and configuration errors; legacy Runtime configuration is private |
| `conversation` | Canonical v0.3 conversation entries, loading, transcript, and read-only views |
| `context` | Typed context provider Port and validated context bundles |
| `compaction` | Typed compaction strategy Port and immutable candidates/proposals |
| `error` | Public error summaries; legacy session errors remain private |
| `event` | Stable event-kind values |
| `ids` | Checked session, instance, turn, interaction, tool-call, and context-source identifiers |
| `model` | Transitional model/provider implementation surface pending later owner migration |
| `storage` | `SessionLog` Port and storage DTOs |
| `tools` | `Tool`, immutable `ToolSet`, async `ToolPolicy`, approval, invocation/context/progress/output DTOs |

The old `runtime` and `workspace` modules, `RuntimeConfig`, `SessionConfig`, `SessionSummary`, `ToolRegistry`, and synchronous legacy policy are not public compatibility surfaces. `legacy_context`, `legacy_policy`, and `legacy_types` are private migration scaffolding scheduled for P5/P6 deletion or replacement.

## Transitional Scope

The removed v0.2 Runtime, concrete builtin/process, and public Tool integration tests are baseline evidence for the reset, not current extension contracts. `tests/tool_set_contract.rs` and `tests/tool_policy_interaction_contract.rs` are the focused P3-B/P3-C replacements. SessionRuntime acceptance coverage is intentionally deferred to P4/P5; this revision does not claim that the full replacement is complete.

The former Runtime/provider example and live Runtime smoke harness were removed with the public facade. Provider protocol suites remain separate transitional evidence and do not establish a public Runtime API.

## Testing

The deterministic offline checks are Python/docs/diff/architecture checks. Rust validation for this phase is intentionally run remotely by the project workflow; no local Rust build or test command is part of the current P3-C review.

```bash
python3 scripts/check_architecture.py
python3 scripts/check_v03_architecture.py --self-test
python3 scripts/check_docs.py
python3 -m py_compile scripts/check_architecture.py scripts/check_docs.py scripts/check_v03_architecture.py
git diff --check
```

## Breaking Change

This is a breaking v0.3 reset with no Runtime, ToolRegistry, builtin, process, or compatibility facade. Historical v0.2 design material remains under `docs/archive/v2/` and is not current public authority.
