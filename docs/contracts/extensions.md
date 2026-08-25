# Extension Contract

MiniCore extends only through typed Ports and immutable checked DTOs. It has no generic extension registry or privileged lifecycle escape hatch.

Primary source surfaces: [`src/bindings.rs`](../../src/bindings.rs), [`src/model/model.rs`](../../src/model/model.rs), [`src/tools/tool.rs`](../../src/tools/tool.rs), [`src/tools/policy.rs`](../../src/tools/policy.rs), [`src/context/provider.rs`](../../src/context/provider.rs), [`src/compaction/strategy.rs`](../../src/compaction/strategy.rs), and [`src/conversation/session_log.rs`](../../src/conversation/session_log.rs). Boundary evidence: the authoritative [`check_v03_architecture.py`](../../scripts/check_v03_architecture.py), and exact shared-Arc concurrency in [`session_runtime_shared_ports_evidence.rs`](../../tests/session_runtime_shared_ports_evidence.rs).

## Supported Seams

The supported extension seams are exactly:

- `Model`;
- `Tool` values collected into immutable `ToolSet`;
- `ToolPolicy`;
- `ContextProvider`;
- `CompactionStrategy`;
- `SessionLog`.

`SessionBindings` freezes one Model, one ToolSet, and optional policy/context/compaction adapters for one loaded Session. SessionLog is transferred separately because it is the exclusive mutable durability owner.

## Host Decorators And Composites

A Host may implement typed wrappers around a Port for metrics, tracing, caching, rate limiting, routing, authentication, or protocol adaptation, provided the wrapper preserves the Port contract. For example:

- a Model decorator may record safe latency and then delegate `start`;
- a Tool composite may route by the frozen `ToolName` through ToolSet registration;
- a ContextProvider may merge several Host sources and return one checked bundle;
- a SessionLog adapter may wrap an external database transaction/lease implementation.

These are ordinary Host types implementing the same trait. Core does not discover, install, order, configure, or unload them.

## Explicitly Unsupported

Core exposes no:

- `Hook` or universal before/after lifecycle callback;
- `Plugin`, plugin manager, dynamic-library ABI, or package loader;
- `ServiceLocator`, provider registry, model resolver, or dependency container;
- dedicated Subagent, AgentSpawner, parent/child agent graph, or remote-agent state machine;
- mutable runtime extension map;
- access to SessionActor internals, log ownership, task handles, or cancellation slots.

A remote agent is modeled as an ordinary Host Tool, usually an RPC Tool, with its own external lifecycle and durability.

## Concurrency And Sharing

`Model`, `Tool`, `ToolPolicy`, `ContextProvider`, and `CompactionStrategy` are `Send + Sync + 'static` and may be shared with `Arc` across SessionRuntime instances. Core adds no process-global serialization lock. An adapter that needs concurrency limits owns them internally or through a Host-level shared decorator.

`SessionLog` is different: it is `Send + 'static` and called through exclusive `&mut self`. One adapter instance belongs to one SessionRuntime owner. Repository leases and cross-process writer exclusion remain external.

Shared Ports must not share Session state accidentally. Every request carries exact identity and a call-scoped cancellation/deadline. Cancelling or shutting down one SessionRuntime must not cancel another runtime that uses the same shared adapter object.

## Authority Rule

Concrete authority is captured when the Host constructs an adapter. A filesystem Tool may capture a capability-limited workspace; a model adapter may capture credentials and an HTTP client; a ContextProvider may capture an index; a SessionLog may capture a database lease. Those capabilities never enter public Core DTOs or owner handles.

Adding a new public extension seam requires an explicit architectural decision, checked immutable request/response types, cancellation/deadline semantics, ownership and panic rules, deterministic tests, and an update to the architecture gate. Convenience alone is not sufficient.
