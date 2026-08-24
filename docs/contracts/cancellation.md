# Cancellation And Task Ownership Contract

Cancellation is hierarchical and identity-scoped. Tokens signal work; they do not replace durable settlement or explicit shutdown.

Sources: [`src/session/runtime.rs`](../../src/session/runtime.rs), [`src/session/turn_handle.rs`](../../src/session/turn_handle.rs), [`src/agent/turn_context.rs`](../../src/agent/turn_context.rs), and [`src/time.rs`](../../src/time.rs). Evidence spans [`turn_handle_contract.rs`](../../tests/turn_handle_contract.rs), [`session_runtime_open_cancellation_contract.rs`](../../tests/session_runtime_open_cancellation_contract.rs), focused driver cancellation tests, full-mailbox shutdown in [`session_runtime_lifecycle_evidence.rs`](../../tests/session_runtime_lifecycle_evidence.rs), and shared-Port cancellation isolation in [`session_runtime_shared_ports_evidence.rs`](../../tests/session_runtime_shared_ports_evidence.rs).

## Token Hierarchy

- The **root token** belongs to one SessionRuntime and is cancelled by explicit shutdown or runtime Drop.
- The **Turn token** belongs to one exact Turn and is cancelled by the first successful `TurnHandle::cancel`, root shutdown, or terminal durability failure.
- **Child Port tokens** are scoped to one Model, Tool, ToolPolicy, ContextProvider, or CompactionStrategy call. Drivers cancel the child before dropping a timed-out/cancelled Host future.
- The **interaction wait** uses the active Turn cancellation/deadline and one actor-owned resume sender.

Tokens are never shared across independent SessionRuntime owners merely because Ports are shared.

## TurnHandle

Cancellation and completion share one mutex linearization point. The first cancellation request returns `true`; repeated requests and requests after completion return `false`. Completion is first-wins and wakes every cloned waiter with the same result.

Dropping TurnHandle does not cancel. `TurnHandle` is `#[must_use]`; a Host may intentionally detach it, but should record that decision. Dropping SessionHandle does not cancel. Only root owner Drop sends root cancellation, and it performs no blocking cleanup.

## Deadline Provenance

Each call uses the earlier of:

- the absolute Turn deadline;
- the configured Port timeout.

Equal deadlines are conservatively classified as Turn deadlines. Core records the selected source before awaiting. It does not infer provenance later from an adapter-returned timeout/deadline error.

Turn deadline generally settles as `BudgetExceeded` or Turn control. A configured Port timeout retains the Port-specific failure taxonomy, such as model timeout, context failure, policy denial/failure, Tool failed result, or compaction failure.

## Port Rules

- **Model:** cancellation interrupts start, stream polling, and retry sleep; observed delivery controls retry eligibility.
- **ToolPolicy:** cancellation/Turn deadline prevents or interrupts policy evaluation and fails closed without Tool execution.
- **Tool:** child cancellation is fired before a pending Tool future is dropped; configured timeout may become a failed ToolResult.
- **ContextProvider:** cancellation and deadline drop the provider future and terminate the Turn with exact provenance.
- **CompactionStrategy:** cancellation is checked before candidate/strategy availability and again after candidate validation; no strategy call may begin after control wins.
- **Interaction:** cancellation consumes the pending sender once, clears public pending state, and lets runner Join drive durable settlement.

## Shutdown

Root cancellation is selected with biased priority ahead of critical traffic, command floods, and lossy progress. The actor enters Closing, rejects new work, cancels the active Turn, resolves any pending interaction, waits for runner exit, settles durably where possible, closes the log, and exits.

A full command queue cannot prevent shutdown because root cancellation is out of band. The explicit shutdown timeout aborts and awaits the same owner task, then aborts/waits tracked runner ownership. No Core-owned detached task is intentionally left alive.

## Panic

Defined Host-controlled panic boundaries include descriptor access, Port future construction/polling, SessionLog calls, and the post-ready actor loop. Panic is converted to typed failure where the boundary promises it. Actor panic cleanup retains runner ownership until join/abort and attempts one log close.

Core does not promise recovery from arbitrary allocation or invariant panics outside those boundaries.

## Drop Limits

Runtime Drop is cancel-only and best-effort. It must not use `mem::forget`, `block_on`, synchronous waiting, ownership-taking graceful cleanup, or a new detached task. `SessionRuntime` is `#[must_use]`, and explicit `shutdown(self).await` is the only durability barrier. Hosts must retain and drive a runtime long enough to await shutdown for every loaded SessionRuntime.
