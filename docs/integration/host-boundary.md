# Host Boundary

MiniCore Runtime owns execution for one loaded Session. The Host owns every collection, adapter, product policy, and ambient capability around it.

See the lifecycle and extension contracts: [`session-runtime-lifecycle.md`](../contracts/session-runtime-lifecycle.md) and [`extensions.md`](../contracts/extensions.md).

The failure-safe compiled integration shape is [`examples/session_runtime_lifecycle.rs`](../../examples/session_runtime_lifecycle.rs); README is checker-synchronized to its marked body. After load succeeds, that example captures operation errors, always shuts down, then always joins the event task before applying shutdown → event-task → operation error precedence.

## Loaded Session Collection

A Host may keep its own map:

```rust,ignore
use std::collections::HashMap;

use minicore_runtime::{SessionId, SessionRuntime};

struct LoadedSessions {
    runtimes: HashMap<SessionId, SessionRuntime>,
}
```

This is illustrative Host code, not a Core type. The Host decides duplicate-load policy, admission, eviction, tenancy, routing, and what to do when two independent instances use the same durable SessionId. Core provides no `Runtime`, `SessionManager`, `SessionRepository`, or shutdown-all facade.

## Repository And Writer Lease

Listing, creating repository metadata, opening an existing Session, deleting/archiving a Session, and acquiring a writer lease happen before `SessionRuntime::create` or `load`.

A typical Host flow is:

1. authorize the product request;
2. consult the Host repository;
3. acquire an exclusive writer lease;
4. open one concrete `Box<dyn SessionLog>` adapter;
5. build immutable SessionBindings;
6. create/load one SessionRuntime;
7. retain the runtime and lease until explicit shutdown completes;
8. release the lease and update repository metadata.

Cross-process exclusion belongs to the repository/adapter. `SessionLog` expected-head append detects conflicts but is not a repository lease service.

## Storage Adapter

The Host selects the persistence technology and format. Core defines only the SessionLog Port for manifest initialization/load, paged Conversation reads, atomic expected-head append, and close.

The adapter owns transaction boundaries, durability guarantees, migrations, backup, encryption, filesystem/database paths, corruption diagnostics, and known-versus-unknown outcome classification. No JSONL, filesystem, SQL, or cloud-store implementation ships in Core.

## Workspace And Capabilities

Core has no Workspace abstraction. A Host Tool or ContextProvider captures only the capability it needs when constructed:

```text
Host authorization
→ construct capability-limited Tool/ContextProvider
→ register Tool in ToolSet / bind ContextProvider
→ pass immutable SessionBindings to SessionRuntime
```

ToolContext carries cancellation, deadline, and progress only. Filesystem roots, process launch rights, RPC clients, secrets, repositories, and product services remain inside Host adapters.

MiniCore never starts a process on its own. A process-running Tool, if a product chooses to implement one, is a Host component and must define its own sandbox and process-tree cleanup claims.

## Shared Ports

A Host may share `Arc<dyn Model>`, `Arc<dyn ToolPolicy>`, `Arc<dyn ContextProvider>`, or `Arc<dyn CompactionStrategy>` across multiple SessionRuntime instances. ToolSet is immutable and cloneable. Shared adapters must use the request identity and child cancellation token rather than retaining mutable Session ownership.

Core does not place a global lock around shared Ports. Host-level decorators may provide concurrency limits, pools, budgets, or rate limits.

SessionLog is not shared this way: one mutable adapter instance is transferred to one owner.

## Global Limits

Global or tenant-wide scheduling remains outside Core, including:

- maximum loaded sessions;
- aggregate model/tool concurrency;
- request quotas and billing budgets;
- memory/disk pressure and idle eviction;
- credential rotation;
- provider failover policy;
- workspace conflict policy;
- shutdown ordering across many sessions.

Per-session `KernelConfig`, SessionSpec, and semantic limits do not claim to enforce a process-global policy.

## Shutdown-All

The Host implements shutdown-all by draining its own collection and explicitly awaiting each owner:

```rust,ignore
for (_, runtime) in loaded_sessions.runtimes.drain() {
    if let Err(error) = runtime.shutdown().await {
        record_shutdown_failure(error);
    }
}
```

A production Host chooses concurrency, deadline, retry/reporting, and failure aggregation policy. Dropping the map only sends cancellation; it is not a durability barrier.

## Product Boundary

Servers, CLIs, GUIs, authentication, audit logs, telemetry export, model/provider installation, tool catalogs, workspace selection, session listing, and release policy are Host/product concerns. They may use the typed Ports but must not depend on private actor protocol or treat EventStream as durable truth.
