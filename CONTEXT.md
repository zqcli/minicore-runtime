# Current Implementation Context

> P4-B checkpoint of the breaking v0.3 single-session runtime refactor. The
> multi-session Runtime has been deleted. `SessionRuntime` now owns create/load,
> durable log lifetime, root cancellation, events, and deterministic shutdown;
> final SessionHandle/commands and turn execution remain P4-C/P5 work.

## Checkpoint

The crate is Rust 2024 with Rust 1.85 as its MSRV. Concrete storage acquisition, model networking, tools, workspaces, credentials, and management of multiple loaded sessions are Host responsibilities. Current authority is the source tree plus the documents linked from [docs/README.md](docs/README.md); v0.2 material is historical evidence.

## Ownership Map

- `session::SessionRuntime`: unique owner of one created or loaded Session, one `ConversationLog`, one root cancellation domain, one state sender, one bounded event sink, and one actor JoinHandle.
- `session::SessionRuntimeOptions`: checked `KernelConfig`, immutable `SessionBindings`, and the Host-selected Tokio `Handle` used to spawn the whole open lifecycle.
- `conversation`: canonical entries, semantic validation, proof-gated load, paged replay, restart repair, append coordination, confirmed projection, transcript, and close classification.
- `storage::SessionLog`: the only public persistence Port. Host code acquires one exclusive adapter and passes ownership into `SessionRuntime::create/load`.
- `session`: public bindings/interactions/state/events/TurnHandle plus the P4-B owner. Old actor/command/observation files remain private P4-C/P5 scaffolding only.
- `model`, `tools`, `context`, `compaction`: host-neutral Ports and checked DTOs bound immutably for the loaded lifetime.
- `agent`, `prompt`, `workspace`, the old filesystem store, and legacy actor/model/tool observation graph: `cfg(test)` migration evidence only; none is present in the production library graph or owned by SessionRuntime.

The public root exposes `compaction`, `config`, `context`, `conversation`, `error`, `ids`, `model`, `session`, `storage`, `tools`, and `value`. There is no top-level `runtime` module, multi-session manager, loaded-session map, repository, or shutdown-all owner.

## SessionRuntime Contract

`SessionRuntimeOptions::new(kernel, bindings, task_runtime)` validates the kernel before any log is accepted. Its Debug reports only capacities, timeouts, tool count, and optional-adapter presence.

`SessionRuntime::create(session_id, spec, log, options)` spawns the complete open lifecycle on the supplied Tokio Handle, then performs:

```text
kernel validation
→ custom-limit SessionSpec validation
→ SessionBindings compatibility validation
→ SessionManifest construction/validation
→ SessionLog initialize with zero-head proof
→ new SessionInstanceId
→ Idle + Healthy state and bounded event channels
→ ready handshake
→ idle owner loop
```

`SessionRuntime::load(expected_session_id, log, options)` performs:

```text
load/validate manifest and expected SessionId
→ bindings validation
→ pending-load compatibility proof
→ paged replay and semantic validation
→ atomic restart repair when needed
→ new SessionInstanceId
→ confirmed-head/last-terminal state rehydration
→ ready handshake
→ idle owner loop
```

No SessionHandle, command sender, fake submit path, or first-snapshot event is exposed in P4-B. `take_events` transfers the one bounded receiver exactly once.

## Cancellation And Shutdown

Before owner spawn and before any await, `OpenGuard` synchronously installs cleanup watchers on both the configured and current Tokio runtimes. Each captures only `SharedOpenPayload`, root cancellation, and a `payload_claimed` signal. Before entering the single-take cleanup path, a watcher panic-safely constructs and drops a zero-duration sleep in its current execution context; a no-time fallback exits `None` without taking the log. `run_open` signals claim immediately after its successful take. Owner spawn panic, pre-poll Join failure, ready-channel loss, and caller cancellation share this path.

`SessionRuntime::shutdown(self)` cancels out of band, waits for state `Closing`, log close, sender drops, and task completion. If `shutdown_timeout` expires, it aborts and awaits that same JoinHandle before returning `SessionShutdownError::Timeout`. Known close failure, unknown durability, timeout, and task termination remain distinct typed errors.

`SessionRuntimeOptions::new` synchronously validates that `task_runtime` has an enabled Tokio time driver by entering it and constructing then dropping a zero-duration sleep under `catch_unwind`. The runtime must remain timer-enabled, alive, and actively driven throughout create, load, and shutdown. Successful SessionRuntime retains the Handle; shutdown constructs its timeout under a short panic-isolated `enter()` scope, drops that scope before await, and can then be polled by a non-Tokio executor. Unexpected timeout-construction panic cancels, aborts, and awaits the same owner task before returning ActorTerminated.

Model descriptor access, SessionLog future construction and polling, and the post-ready actor loop are explicit host-controlled panic boundaries. They are caught and mapped to typed failures; actor-loop panic attempts one bounded close. Arbitrary Core allocation or invariant panics after ownership transfer are not a recoverable API boundary and may skip graceful close, just like destruction of all runtimes capable of driving cleanup. This is an intentional engineering boundary, not a claim that any panic closes safely.

## Error Boundary

`SessionOpenError` is a redacted struct with public `SessionOpenErrorKind`. It preserves invalid configuration, invalid manifest, SessionId mismatch, binding mismatch, typed log failure, recovery uncertainty, and owner-start failure. Failed-open close information is a secondary bounded diagnostic and never replaces the primary error.

`SessionShutdownError` is non-exhaustive and distinguishes Timeout, Durability, LogClose, and ActorTerminated. Diagnostics use bounded static text; raw adapter sources, paths, credentials, prompts, and response bodies are not retained.

## Deferred Work

P4-C will replace the private legacy actor/command scaffolding with the final cloneable SessionHandle, state watch access, bounded submit/answer commands, and transcript routing. P5 will wire TurnHandle completion, ModelDriver, tools, policy/interactions, context, compaction, and terminal settlement. The current owner is deliberately idle: it proves lifecycle and durable ownership without fake execution.

## Verification

Local work for this phase uses Python architecture/docs/source checks and `git diff --check`. Rust build, tests, Clippy, rustdoc, and rustfmt validation are run remotely by the project workflow. The focused owner evidence is `tests/session_runtime_owner_contract.rs`, private runtime/actor unit tests, and the deterministic `FakeSessionLog` operation-admission controls.
