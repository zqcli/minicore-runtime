# Canonical Module Map

This page maps the current source graph. It is intentionally narrower than the historical design archive: every listed owner is compiled by the public crate, and every unlisted implementation file is private or removed.

## Root Surface

`src/lib.rs` declares these public modules:

| Module | Surface |
| --- | --- |
| `config` | Checked runtime/session configuration and retry policy |
| `error` | Runtime/session errors and public error summaries |
| `event` | Session event-kind catalog |
| `ids` | Checked identifiers |
| `model` | Provider and model contracts |
| `runtime` | Runtime orchestration and session summaries |
| `session` | Session observation, outcomes, transcript DTOs, actor, store, and conversation |
| `tools` | Tool contracts, policy, interaction, process policy, and builtins |
| `workspace` | Capability-backed root and relative filesystem operations |

The root also reexports stable DTOs and operations. `agent` and `prompt` are private modules. There are no path-based compatibility declarations.

## Ownership and Dependencies

### `config`

- Source: [`src/config.rs`](../../src/config.rs)
- Owns `RuntimeConfig`, `RuntimeConfigBuilder`, `SessionConfig`, and checked bounds.
- Depends on model selection/registry, tool names/registry, stored session constructors, and session timestamps.
- Does not open files, providers, actors, or processes.

### `model`

- Sources: [`src/model/mod.rs`](../../src/model/mod.rs), [`gateway.rs`](../../src/model/gateway.rs), [`provider.rs`](../../src/model/provider.rs), [`registry.rs`](../../src/model/registry.rs), [`types.rs`](../../src/model/types.rs), [`transport.rs`](../../src/model/transport.rs), and [`providers/`](../../src/model/providers/).
- Owns `ModelGateway`, `ModelProvider`, `ProviderRegistry`, descriptors, selections, request/response values, opaque credentials, event sinks, delivery state, and provider errors.
- The registry freezes provider descriptors and resolves a selection only from its own immutable maps.
- Providers own protocol encoding, SSE parsing, terminal proof, usage normalization, endpoint policy, and provider-native error mapping.
- Transport owns bounded response drains, no-redirect/no-retry client policy, cancellation-aware reads, and retry-after parsing.
- Depends on `serde_json`, `reqwest`, `futures-util`, and Tokio cancellation primitives; it does not depend on session actors or storage.

### `tools`

- Sources: [`src/tools/mod.rs`](../../src/tools/mod.rs), [`registry.rs`](../../src/tools/registry.rs), [`types.rs`](../../src/tools/types.rs), [`policy.rs`](../../src/tools/policy.rs), [`context.rs`](../../src/tools/context.rs), [`process.rs`](../../src/tools/process.rs), and [`builtins/`](../../src/tools/builtins/).
- Owns the immutable tool registry, tool descriptions, tool result values, user-question values, interaction client, policy decisions, and process policy.
- Builtins own their schemas, argument validation, execution mapping, bounded output, and fixed result text.
- `ToolContext` supplies the current workspace, cancellation token, and interaction bridge. Tools do not own session state or provider selection.
- The registry is frozen before a runtime opens. Session configuration only selects names already present in that registry.

### `workspace`

- Sources: [`src/workspace/mod.rs`](../../src/workspace/mod.rs), [`path.rs`](../../src/workspace/path.rs), and [`root.rs`](../../src/workspace/root.rs).
- Owns one configured root capability, access mode, relative paths, directory-entry projection, bounded I/O, final-component checks, and owner-tracked shutdown.
- `Workspace::open` captures the single root capability. File and directory operations resolve relative to that capability; they do not re-open ambient paths.
- The workspace worker joins admitted blocking operations before shutdown completes. Production `SessionActor` ownership awaits `Workspace::shutdown()` during close; the `Workspace` Drop fallback may block synchronously and is not preferred. Explicit Runtime shutdown waits for all session actors and observes their workspace shutdowns.
- Depends on cap-std/cap-primitives for capability and no-follow operations, not on Runtime or session residency.

### `prompt`

- Sources: [`src/prompt/mod.rs`](../../src/prompt/mod.rs), [`builder.rs`](../../src/prompt/builder.rs), and [`compaction.rs`](../../src/prompt/compaction.rs).
- Private owner of prompt message assembly, coding instructions, serialized-byte estimation, compaction planning, and validated summaries.
- Consumes model/tool values and conversation projections. It does not write the conversation file or publish session events.

### `agent`

- Sources: [`src/agent/mod.rs`](../../src/agent/mod.rs), [`context.rs`](../../src/agent/context.rs), and [`runner.rs`](../../src/agent/runner.rs).
- Private owner of one model/tool turn, tool-round ordering, interaction requests, delivery-aware logical retry, cancellation, and compaction requests.
- It returns turn work/results to the session actor; it does not own terminal persistence.

### `session`

- Sources: [`src/session/mod.rs`](../../src/session/mod.rs), [`actor.rs`](../../src/session/actor.rs), [`command.rs`](../../src/session/command.rs), [`event.rs`](../../src/session/event.rs), [`event_stream.rs`](../../src/session/event_stream.rs), [`snapshot.rs`](../../src/session/snapshot.rs), [`state.rs`](../../src/session/state.rs), [`store.rs`](../../src/session/store.rs), [`conversation.rs`](../../src/session/conversation.rs), [`conversation_actor.rs`](../../src/session/conversation_actor.rs), [`conversation_codec.rs`](../../src/session/conversation_codec.rs), [`conversation_compaction.rs`](../../src/session/conversation_compaction.rs), [`conversation_usage.rs`](../../src/session/conversation_usage.rs), [`transcript.rs`](../../src/session/transcript.rs), and [`time.rs`](../../src/session/time.rs).
- The actor owns admission, mailbox ordering, terminal settlement, interaction persistence, snapshot publication, and close completion.
- The conversation log owns append ordering, replay, repair, prompt projection, compaction boundaries, usage aggregation, and transcript projection.
- The store worker owns the root lock, session namespace, atomic create, bounded CRUD, readiness, and shutdown result.
- Snapshot and event modules expose observation; the durable and actor modules remain crate-private.
- Session depends on model, tools, workspace, prompt, and agent internals. Those dependencies point inward to the session owner rather than creating peer actors.

### `runtime`

- Sources: [`src/runtime/mod.rs`](../../src/runtime/mod.rs), [`runtime_impl.rs`](../../src/runtime/runtime_impl.rs), and [`session_manager.rs`](../../src/runtime/session_manager.rs).
- Owns public lifecycle admission, loaded-session residency, create/load/close/delete/list orchestration, public method error mapping, and runtime shutdown ownership.
- It contains one manager state boundary for loaded, loading, and closing sessions. It does not duplicate the session actor mailbox.
- Runtime opens the session store and carries a model/tool/workspace configuration into prepared sessions; session actors own per-session work after admission.

## Public and Private Boundary

Public values are checked at construction and are safe to serialize or display. Provider credentials, model endpoint details, workspace roots in debug output, and internal actor/store details are redacted or private.

The public transcript contains `User`, `Assistant`, `ToolResult`, `Interaction`, `Summary`, and `Terminal` projections. The interaction entry is durable transcript evidence but is intentionally absent from model prompt messages.

The canonical graph has no public transport protocol module, no second runtime owner, no compatibility aliases, and no alternate source tree. New functionality should deepen an existing owner before adding a module.

## File Inventory

The compiled current graph is intentionally small:

```text
src/
├── agent/{context.rs,mod.rs,runner.rs}
├── config.rs
├── error.rs
├── event.rs
├── ids.rs
├── model/{gateway.rs,mod.rs,provider.rs,registry.rs,transport.rs,types.rs}
│   └── providers/{anthropic.rs,mod.rs,openai.rs}
├── prompt/{builder.rs,compaction.rs,mod.rs}
├── runtime/{mod.rs,runtime_impl.rs,session_manager.rs}
├── session/{actor.rs,command.rs,event.rs,event_stream.rs,mod.rs,snapshot.rs,state.rs,store.rs,time.rs,transcript.rs}
│   └── conversation{,_actor.rs,_codec.rs,_compaction.rs,_usage.rs}
├── tools/{context.rs,mod.rs,policy.rs,process.rs,registry.rs,types.rs}
│   └── builtins/{ask_user.rs,list_directory.rs,path_args.rs,read_file.rs,run_command.rs,write_file.rs}
└── workspace/{mod.rs,path.rs,root.rs}
```

The graph above names the current owners, not an archival compatibility layout. The `src/session` conversation files are private submodules of the session module. The `src/model/transport.rs` module is crate-private even though model provider types are public. The `src/prompt` and `src/agent` directories are private implementation modules declared from the crate root.

## Boundary Rules

- A public DTO must have a checked constructor or a checked deserializer before it reaches Runtime or a worker.
- A registry is mutable only during its builder phase; the value passed into Runtime is immutable.
- An owner may expose a narrow typed operation, but it must not expose its internal queue, worker handle, raw path, credential, or actor channel.
- A blocking operation must have a named owner and a join/settlement path. Dropping a caller cannot detach it.
- A public error must preserve the authoritative owner and must not leak secret or host-specific diagnostic material.
- A new event must be derived from a published snapshot fact and must remain bounded.
- A persistence change must specify field order, byte limits, replay behavior, repair behavior, and migration behavior before source changes.

## Test Seams

- `tests/p1_*` cover checked IDs, public DTOs, and root/module surface.
- `tests/p2_*` cover registry/policy behavior and capability-relative builtin/workspace behavior.
- `tests/p3_*` cover model gateway, provider protocol mapping, SSE, transport, delivery, and retry behavior.
- `tests/p4_*` cover conversation and store bytes, replay, repair, lock ownership, and degradation.
- `tests/p5_*` cover prompt/compaction boundaries and deterministic estimation.
- `tests/p6_*` cover turn runner and session actor ownership/cancellation.
- `tests/p7_*` cover Runtime lifecycle, restart, close, shutdown, process policy, and public surface.
- `tests/v2_acceptance.rs` covers the active end-to-end matrix and the source graph audit.
- `provider-gate/` is a separate stable-only evidence package for provider SDK behavior; it is not a production dependency.

A test that needs a private owner should use an existing crate-private seam or a focused module test. It should not resurrect a public transport or lifecycle type solely to reach an assertion.

## Change Checklist

When adding a module, record its owner and its dependency direction.

When adding a public type, add a checked constructor, a redacted Debug form where needed, and a surface test.

When adding a worker, record who starts it, who cancels it, who joins it, and which result is authoritative.

When adding a durable field, update both the source serializer and the current format document.

When adding a tool, specify its input schema, access boundary, cancellation result, output bound, and registry name.

When adding a provider behavior, keep endpoint, credential, protocol, delivery, and retry evidence in the model owner.
