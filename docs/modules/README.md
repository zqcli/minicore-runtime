# Canonical Module Map

This page maps the final v0.3 source graph. Core contains host-neutral Ports, checked DTOs, private execution drivers, and one single-session owner. It contains no workspace, concrete storage, provider registry, builtin/process adapter, multi-session manager, or compatibility implementation.

## Root Surface

`src/lib.rs` exposes the public modules `compaction`, `config`, `context`, `conversation`, `error`, `ids`, `model`, `session`, `storage`, `tools`, and `value`. The private root modules are `agent`, `bindings`, `interaction`, `port_call`, `prompt`, and `time`.

`src/bindings.rs` and `src/interaction.rs` are neutral physical modules used to keep the module graph acyclic. Their public types are reexported through `session`; they are not duplicate public paths.

| Module | Responsibility |
| --- | --- |
| `config` | Kernel limits, retry policy, session specs, manifests, and checked inputs |
| `conversation` | Canonical entries, validation, replay/recovery, transcript, prompt/compaction proofs, settlement, and the physical SessionLog Port declaration |
| `context` | Public ContextProvider Port/DTOs, private ContextDriver, and crate-private ValidatedContextBundle |
| `compaction` | Public CompactionStrategy Port, candidates/proposals, and private CompactionDriver |
| `error` | Redacted diagnostics plus typed session/log/open/shutdown/turn errors |
| `ids` | Checked runtime, interaction, tool-call, and context-source identifiers |
| `model` | Direct streaming Model Port, checked DTOs, and private ModelDriver |
| `prompt` | Private deterministic PromptBuilder and crate-private PromptPlan |
| `agent` | Private TurnRunner, ToolDriver, commit/suspension protocol, and compaction recovery |
| `session` | SessionRuntime, SessionHandle, actor, commands, state/events, interactions, and TurnHandle |
| `storage` | Public reexport facade for the SessionLog Port and its DTOs; no adapter implementation |
| `tools` | Tool, ToolSet, ToolPolicy, invocation/context/input/progress/output DTOs |
| `port_call` | Crate-private bounded one-shot execution for Context, Compaction, ToolPolicy, and Tool ports |

## Ownership

The Host acquires exactly one `Box<dyn SessionLog>` and passes it to `SessionRuntime::create` or `load`. Core owns that adapter for the loaded lifetime. Listing sessions, selecting storage formats, writer leases, repository policy, and multi-session shutdown belong to the Host.

`SessionBindings` freezes one direct `Arc<dyn Model>`, one immutable `ToolSet`, and optional ToolPolicy, ContextProvider, and CompactionStrategy adapters. Validation is pure apart from panic-isolated descriptor inspection and invokes no adapter future. Create/load then constructs one checked `Arc<agent::SessionEnvironment>` containing the immutable SessionSpec/limits, model limits, enabled tools, and static Prompt/Context/Compaction/Model/Tool drivers for the loaded lifetime.

`SessionActor` is the sole durable mutation owner. Its private `ActorCoreState` owns active Turn state, health, closing/durability facts, the last durable terminal, and interaction-resolution identity. Public `SessionState` is derived from that core and `ConversationLog::head()`; the watch sender is output-only. The actor serializes commands, runner commit acknowledgements, interactions, transcript reads, settlement, health degradation, and close ordering. `SessionHandle` contains only stable IDs, a bounded command sender, and watch state.

The SessionLog trait is physically declared in `conversation/session_log.rs` because conversation validation and ConversationLog consume it. `storage` publicly reexports the Port so the external API remains `storage::SessionLog`. This placement removes the former `conversation ↔ storage` module cycle.

## Execution Modules

- `model::driver` owns stream grammar, usage settlement, retry truth, cancellation, deadline provenance, and panic isolation for one direct Model.
- `agent::tool_driver` owns frozen-spec policy decisions, approval/input suspension, Tool execution, output bounds, and lossy progress.
- `context::driver` owns zero-or-one provider execution, the single ContextBundle validation/canonical-sort seam, crate-private ValidatedContextBundle construction, cancellation, and deadline provenance.
- `prompt::builder` owns one-per-head conversation planning, deterministic prompt ordering, fixed/final serialized-request budgeting, and checked-context finalization.
- `compaction::driver` owns canonical candidate validation and one strategy call without commit authority; candidate validation and the test-only race pause remain outside the helper, which is entered directly and prechecks control before its closure resolves availability.
- `port_call` owns the shared cancellation/deadline/panic/drop protocol for the four bounded one-shot Port calls, including distinct invalid-effective-deadline reporting; it owns no retry, durability, interaction, or runner protocol policy.
- `agent::environment` freezes the checked static Session environment once at create/load; `agent::runner` owns ordinary model/tool rounds, compaction recovery, exact single-entry delta acknowledgements, conservative usage, and normal completion through the tracked runner join.
- `session::actor` owns durable append authority and terminal settlement.

## Source Inventory

```text
src/
├── agent/{mod.rs,environment.rs,runner.rs,runner/{compaction.rs,diagnostics.rs,support.rs,tests/...},runner_protocol.rs,tool_driver.rs,tool_driver/{support.rs,tests/...},turn_context.rs}
├── bindings.rs
├── compaction/{mod.rs,strategy.rs,driver.rs,driver/tests/...}
├── config/{kernel.rs,retry.rs,session.rs,session_spec.rs}
├── context/{mod.rs,provider.rs,driver.rs,driver/tests/...}
├── conversation/{compaction_candidate.rs,entry.rs,load.rs,log.rs,log/tests/...,projection.rs,recovery.rs,session_log.rs,settlement.rs,state.rs,transcript.rs,validator.rs,view.rs}
├── error/{operations.rs}
├── interaction.rs
├── model/{mod.rs,model.rs,response.rs,types.rs,driver.rs,driver/{assembler.rs,failure.rs,tests/...}}
├── port_call.rs
├── prompt/{mod.rs,builder.rs,builder/tests/...}
├── session/{mod.rs,actor.rs,actor/{commands.rs,lifecycle.rs,run.rs,runner.rs,settlement.rs,supervisor.rs,tests/...},command.rs,event.rs,event_stream.rs,handle.rs,runtime.rs,runtime_log.rs,runtime_open.rs,runtime_shutdown.rs,state.rs,turn_handle.rs}
├── storage/mod.rs
├── time.rs
├── tools/{mod.rs,context.rs,input.rs,policy.rs,progress.rs,set.rs,tool.rs,types.rs}
└── value.rs
```

## Tests

Focused source contracts protect each public Port and private driver boundary. Public integration tests cover create/load/shutdown, command admission, interactions/restart, forced event loss, compaction commits, Port failures, durability failures, transcript behavior, panic cleanup, shared-Port concurrency, and owner isolation. `port_call` unit tests directly cover construction/poll panic, Ok/Err result cleanup, precheck suppression, biased cancellation/deadline/result priority, invalid effective deadlines, and child cancellation before future drop; source contracts prove only Context/Compaction/ToolPolicy/Tool use it and Model/ConversationLog do not. `tests/final_architecture_contract.rs` plus `scripts/check_v03_architecture.py` protect physical absence of the removed graph and the exact root/dependency/module surface. `scripts/acceptance_v03.json` is the canonical reviewed AT traceability map, and the documentation checker requires its generated Markdown and attributed tests to remain synchronized.

The normative behavior pages are indexed from [Documentation Authority](../README.md): [SessionRuntime lifecycle](../contracts/session-runtime-lifecycle.md), [state](../contracts/session-state.md), [events](../contracts/event-stream.md), [Conversation](../contracts/conversation.md), [SessionLog](../contracts/session-log.md), [Model](../contracts/model.md), [Tool/policy/interaction](../contracts/tool-policy-interaction.md), [cancellation](../contracts/cancellation.md), and [extensions](../contracts/extensions.md). Host ownership is defined in the [Host boundary](../integration/host-boundary.md), and final evidence is recorded in the [acceptance matrix](../acceptance-v0.3.md).

## Boundary Rules

A public DTO has a checked constructor or strict deserializer. Public Debug implementations redact payloads. Ports do not import SessionRuntime, SessionHandle, direct I/O, registries, repositories, or fanout. Concrete adapters capture Host authority behind the public Port traits rather than receiving Core owner handles.
