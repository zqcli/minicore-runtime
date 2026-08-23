# Canonical Module Map

This page maps the current v0.3 source graph. The v0.2 multi-session Runtime is deleted; concrete adapters and the remaining old actor/command/provider/workspace files are migration history, not public extension points.

## Root Surface

`src/lib.rs` keeps the checked DTO, Port, and single-session owner modules public. There is no top-level `runtime` module. `agent`, `prompt`, workspace, old storage implementations, actor/commands/observation/transcript, and legacy model/tool execution modules are `cfg(test)` at their owning declarations and absent from production compilation.

| Module | Current responsibility |
| --- | --- |
| `config` | Kernel limits, session specs, manifests, retry policy, and checked configuration values |
| `conversation` | Canonical conversation entries, proof-gated loading, replay/recovery, transcript, and read-only views |
| `context` | Typed `ContextProvider` Port and validated context DTOs |
| `compaction` | Typed `CompactionStrategy` Port and immutable candidates/proposals |
| `error` | Redacted diagnostics plus typed log/open/shutdown/turn errors |
| `ids` | Checked identifiers, including `ContextSourceId` |
| `model` | Final direct streaming `Model` Port and shared checked DTOs plus private legacy runner lookup |
| `session` | Public single-session owner, bindings/interactions/state/events/TurnHandle foundation, plus explicitly private legacy execution scaffolding |
| `storage` | `SessionLog` Port and private durable store implementation |
| `tools` | Final public `Tool`/`ToolSet` execution seam plus private legacy runner scaffolding |

## Tools

The final execution seam is `tools::Tool`, `tools::ToolSet`, `tools::ToolInvocation`, `tools::ToolContext`, `tools::ToolOutput`, `tools::ToolInputRequest`, and progress DTOs. P3-C adds the independent async `tools::ToolPolicy` Port with owned requests and typed approval decisions. `ToolContext` contains only cancellation, deadline, and a synchronous nonblocking progress sink. Workspace, process, RPC, credential, and policy authorities remain outside the public Tool context.

`ToolSet` is mutable only during `ToolSetBuilder` registration. Registration returns the builder and records the first duplicate, spec-panic, or invalid-spec error; `build()` emits that error or freezes the tool and spec maps. `specs_for` deterministically omits unknown enabled names; `SessionBindings::validate` rejects them and validates the full frozen spec snapshot. Cloned sets share immutable `Arc` state and can dispatch the same `Arc<dyn Tool>` concurrently. There is no public ToolRegistry facade and no default concrete tool set.

`src/tools/legacy_context.rs`, `src/tools/legacy_policy.rs`, and `src/tools/legacy_types.rs` are private staged migration scaffolding for the old actor/runner/storage path. They are scheduled for P5/P6 deletion or replacement. The final `src/tools/policy.rs` is canonical; only actor suspension/consumption wiring remains deferred.

`src/session/interaction.rs` owns the single public process-local `PendingInteraction`/`InteractionKind`/`InteractionAnswer` vocabulary. It validates typed answer matching without owning one-shot consumption, callbacks, or durable state.

`src/session/bindings.rs` owns the public immutable adapter bundle and payload-free `SessionBindingError`. Its pure validation checks `SessionSpec`/`SemanticLimits`, catches Model descriptor panic, checks direct model compatibility, enabled tools/policy, every frozen ToolSpec budget, and compaction strategy presence. It never invokes Model start, Tool execution, policy, context, or compaction futures and does not own Clock, task, log, store, workspace, or lifecycle state.

`src/session/state.rs`, `event.rs`, `event_stream.rs`, and `turn_handle.rs` own the final P4-A foundation. State is process-local and invariant-checked; EventStream is one bounded mpsc receiver with crate-private lossy publication; TurnHandle provides exact cancellation and first-wins durable completion. None contains actor, log, Workspace, snapshot recovery, broadcast, or serde capabilities.

`src/session/runtime.rs` owns the exact public P4-B surface and OpenGuard/JoinHandle discipline. `runtime_open.rs` sequences create/load, compatibility proof, replay/repair, ready, and failed-open cleanup. `runtime_actor.rs` owns the idle log/state/event lifetime and close barrier. `runtime_log.rs` adds only cancellation-at-operation-entry and delegates all timeout/panic/close classification to ConversationLog helpers. Every file is below 500 lines.

`src/error/operations.rs` owns SessionLogError, struct-plus-kind SessionOpenError, and non-exhaustive SessionShutdownError. Public diagnostics contain bounded static messages; SessionOpenError preserves primary log/identity distinctions and records failed-open close failure only as a secondary diagnostic.

`src/session/legacy_state.rs`, `legacy_event.rs`, `legacy_event_stream.rs`, and `legacy_snapshot.rs` preserve old observation tests under explicit `Legacy*` names. `actor.rs` and `command.rs` are marked P4-C/P5 deletion scaffolding. None is reached by SessionRuntime or publicly reexported.

Legacy physical storage persists separately as `LegacyToolOutput { text, is_error }`. Public `ToolOutput` is content-only and pairs with `ToolResultOutcome` in `ModelMessage::Tool`; the crate-private prompt conversion maps legacy status before provider encoding, while conversation storage retains the old DTO until that path is deleted.

## Model

`src/model/model.rs` owns the direct `Model` Port, checked descriptor, process-local call context, and exact start/stream aliases. `src/model/response.rs` owns delivery-aware errors and the bounded typed stream grammar. `ModelRequest` is host-neutral: messages, tools, limits, and reasoning only.

`src/model/legacy_gateway.rs`, `legacy_provider.rs`, and `legacy_registry.rs` are crate-private P5/P6 scaffolding for the old batch runner. Their identities are explicitly `Legacy*`; no public alias exposes lookup or provider concepts. Concrete OpenAI/Anthropic adapters, HTTP transport, root provider tests, and the root `reqwest` dependency were deleted in P3-D. The independent `provider-gate/` package remains separate evidence.

## Workspace

Workspace capability ownership remains a private transitional implementation detail. `workspace` is not a public root module, and the public Tool seam does not pass a workspace or process handle through `ToolContext`.

## Other Owners

- `config` owns checked kernel/session-spec values. RuntimeConfig/Builder and legacy SessionConfig are deleted.
- `model` owns the final direct Port, shared DTOs, and private P5-A `ModelDriver`; legacy lookup remains test-only until P5-B replaces the old runner path.
- `conversation` owns confirmed state, durable append coordination, replay/recovery, transcript projection, and proof-gated load completion.
- `storage` owns the current private filesystem store and exposes only the `SessionLog` Port to future v0.3 owners.
- `session` owns the final single-session lifecycle plus bindings/state/event/TurnHandle primitives; SessionHandle/commands remain P4-C and execution remains P5.

## File Inventory

```text
src/
├── config.rs
├── conversation/{entry.rs,load.rs,log.rs,projection.rs,recovery.rs,state.rs,transcript.rs,validator.rs,view.rs}
├── context/{mod.rs,provider.rs}
├── compaction/{mod.rs,strategy.rs}
├── model/{driver.rs,driver/assembler.rs,legacy_gateway.rs,legacy_provider.rs,legacy_registry.rs,mod.rs,model.rs,response.rs,types.rs}
├── error/{operations.rs}
├── session/{bindings.rs,event.rs,event_stream.rs,interaction.rs,runtime.rs,runtime_open.rs,runtime_actor.rs,runtime_log.rs,state.rs,turn_handle.rs,...legacy files}
├── storage/{mod.rs,session_log.rs,...private implementation files}
└── tools/{context.rs,input.rs,legacy_context.rs,legacy_policy.rs,legacy_types.rs,mod.rs,policy.rs,progress.rs,registry.rs,set.rs,tool.rs,types.rs}
```

Concrete `src/tools/builtins/**` and `src/tools/process.rs` adapters were deleted in P3-B. They must not return as canonical files or as a default registration path.

## Test Migration

- `tests/tool_set_contract.rs` is the focused P3-B replacement for removed v0.2 Tool/Registry/concrete-adapter integration tests.
- `tests/tool_policy_interaction_contract.rs` covers the final async policy Port, checked approvals, process-local interactions, matching answers, and source boundaries.
- `tests/model_port_contract.rs` covers the direct Model trait/start/stream contract, descriptor/context/request neutrality, event grammar, error delivery/retry invariants, redaction, and shared concurrency.
- `tests/model_driver_contract.rs` protects the private driver role and public isolation; private driver tests cover assembly, malformed streams, limits, panic/cancellation/deadline behavior, retry, progress loss, drop probes, and concurrency.
- `tests/session_bindings_contract.rs` covers exact bindings shape, pure validation, descriptor panic isolation, compatibility failures, frozen ToolSpec limits, optional adapter non-invocation, and P4 load ordering.
- `tests/session_state_event_contract.rs` covers exact state/event shapes, legal and illegal state matrices, diagnostic/event redaction, envelopes, and the single-consumer stream source contract; sink behavior is tested in `event_stream.rs`.
- `tests/turn_handle_contract.rs` covers the public exact-Turn surface and safe wait errors; mutex/cancellation/completion races are tested in `turn_handle.rs` beside the private publisher.
- `tests/session_runtime_owner_contract.rs` covers options, create/load order, repair-before-ready, error preservation, one-shot events, explicit/Drop shutdown, open cancellation, timeout abort+await, stopped task runtimes, and independent concurrent owners using FakeSessionLog.
- `tests/p1_dto.rs` covers the final checked Tool DTOs and input-answer validation.
- Private `src/tools/progress.rs` tests cover synchronous bounded `try_send` behavior.
- Private `src/tools/legacy_types.rs` tests prove legacy failure output wire round-trip.
- Removed v0.2 model registry, transport, concrete adapter, Runtime smoke, and Runtime surface tests are baseline evidence, not compatibility contracts.
- SessionHandle command/actor execution acceptance remains deferred to P4-C/P5.

## Boundary Rules

A public DTO has a checked constructor or strict deserializer. Public Debug implementations redact payloads. A Port does not depend on Workspace, SessionHandle, Runtime, Store, Model, provider lookup, direct I/O, or fanout. Transitional owners may use private legacy seams until their scheduled migration phase, but they cannot be reintroduced through public aliases.
