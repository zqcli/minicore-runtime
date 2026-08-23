# Canonical Module Map

This page maps the current v0.3 source graph. The v0.2 Runtime, concrete tool adapters, and public policy facade are transitional implementation history, not public extension points.

## Root Surface

`src/lib.rs` keeps the checked DTO and Port modules public. The `runtime` module and old Runtime/configuration facade are private. `agent`, `prompt`, `storage` implementation details, and the legacy tool execution modules are crate-private or private.

| Module | Current responsibility |
| --- | --- |
| `config` | Kernel limits, session specs, manifests, retry policy, and checked configuration values |
| `conversation` | Canonical conversation entries, proof-gated loading, replay/recovery, transcript, and read-only views |
| `context` | Typed `ContextProvider` Port and validated context DTOs |
| `compaction` | Typed `CompactionStrategy` Port and immutable candidates/proposals |
| `error` | Checked public error summaries and internal session errors |
| `event` | Stable event-kind values |
| `ids` | Checked identifiers, including `ContextSourceId` |
| `model` | Transitional model/provider implementation used by the current internal runner |
| `session` | Transitional actor/session implementation; its old observation facade is not a v0.3 public contract |
| `storage` | `SessionLog` Port and private durable store implementation |
| `tools` | Final public `Tool`/`ToolSet` execution seam plus private legacy runner scaffolding |

## Tools

The final P3-B seam is `tools::Tool`, `tools::ToolSet`, `tools::ToolInvocation`, `tools::ToolContext`, `tools::ToolOutput`, `tools::ToolInputRequest`, and progress DTOs. `ToolContext` contains only cancellation, deadline, and a synchronous nonblocking progress sink. Workspace, process, RPC, credential, and policy authorities remain outside the public Tool context.

`ToolSet` is mutable only during `ToolSetBuilder` registration. Registration returns the builder and records the first duplicate, spec-panic, or invalid-spec error; `build()` emits that error or freezes the tool and spec maps. `specs_for` deterministically omits unknown enabled names; SessionBindings validation owns unknown-enabled rejection in the next migration phase. Cloned sets share immutable `Arc` state and can dispatch the same `Arc<dyn Tool>` concurrently. There is no public ToolRegistry facade and no default concrete tool set.

`src/tools/legacy_context.rs` and `src/tools/legacy_types.rs` are private staged migration scaffolding for the old actor/runner/storage path. They are scheduled for P3-C/P6 deletion or replacement. `ToolPolicy` is also intentionally crate-private in this revision; the typed policy/approval actor flow belongs to P3-C.

Legacy physical storage persists separately as `LegacyToolOutput { text, is_error }`. Public `ToolOutput` is content-only and pairs with `ToolResultOutcome` in `ModelMessage::Tool`; the crate-private prompt conversion maps legacy status before provider encoding, while conversation storage retains the old DTO until that path is deleted.

## Model

The current model/provider implementation remains transitional and private-by-ownership even though the `model` module is retained for migration evidence. Provider adapters own their wire encoding and bounded transport behavior; P3-B does not treat them as a Runtime facade.

## Workspace

Workspace capability ownership remains a private transitional implementation detail. `workspace` is not a public root module, and the public Tool seam does not pass a workspace or process handle through `ToolContext`.

## Other Owners

- `config` owns checked kernel/session-spec values. `RuntimeConfig`, `RuntimeConfigBuilder`, and `SessionConfig` remain crate-private transitional values.
- `model` owns the current internal provider gateway and protocol adapters. Provider integration tests are transitional evidence and do not establish a Runtime facade.
- `conversation` owns confirmed state, durable append coordination, replay/recovery, transcript projection, and proof-gated load completion.
- `storage` owns the current private filesystem store and exposes only the `SessionLog` Port to future v0.3 owners.
- `session` and `runtime` retain old actor/lifecycle implementation only so the migration can proceed in later phases; neither is the P3-B public extension seam.

## File Inventory

```text
src/
├── config.rs
├── conversation/{entry.rs,load.rs,log.rs,projection.rs,recovery.rs,state.rs,transcript.rs,validator.rs,view.rs}
├── context/{mod.rs,provider.rs}
├── compaction/{mod.rs,strategy.rs}
├── model/{gateway.rs,mod.rs,provider.rs,registry.rs,transport.rs,types.rs}
├── session/{actor.rs,command.rs,event.rs,event_stream.rs,mod.rs,snapshot.rs,state.rs,transcript.rs}
├── storage/{mod.rs,session_log.rs,...private implementation files}
└── tools/{context.rs,input.rs,legacy_context.rs,legacy_types.rs,mod.rs,policy.rs,progress.rs,registry.rs,set.rs,tool.rs,types.rs}
```

Concrete `src/tools/builtins/**` and `src/tools/process.rs` adapters were deleted in P3-B. They must not return as canonical files or as a default registration path.

## Test Migration

- `tests/tool_set_contract.rs` is the focused P3-B replacement for removed v0.2 Tool/Registry/concrete-adapter integration tests.
- `tests/p1_dto.rs` covers the final checked Tool DTOs and input-answer validation.
- Private `src/tools/progress.rs` tests cover synchronous bounded `try_send` behavior.
- Private `src/tools/legacy_types.rs` tests prove legacy failure output wire round-trip.
- The removed v0.2 Runtime/provider smoke and Runtime surface tests are baseline evidence, not a complete replacement claim.
- SessionRuntime acceptance coverage is deferred to P4/P5.

## Boundary Rules

A public DTO has a checked constructor or strict deserializer. Public Debug implementations redact payloads. A Port does not depend on Workspace, SessionHandle, Runtime, Store, Model, provider lookup, direct I/O, or fanout. Transitional owners may use private legacy seams until their scheduled migration phase, but they cannot be reintroduced through public aliases.
