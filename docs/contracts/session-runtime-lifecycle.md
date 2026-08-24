# SessionRuntime Lifecycle Contract

`SessionRuntime` is the unique, non-Clone owner of one created or loaded Session. A Host may own many runtimes, but Core never combines them under a manager or repository.

Source: [`src/session/runtime.rs`](../../src/session/runtime.rs), [`src/session/runtime_open.rs`](../../src/session/runtime_open.rs), and [`src/session/runtime_shutdown.rs`](../../src/session/runtime_shutdown.rs). Primary evidence: [`session_runtime_owner_contract.rs`](../../tests/session_runtime_owner_contract.rs), [`session_runtime_open_cancellation_contract.rs`](../../tests/session_runtime_open_cancellation_contract.rs), [`session_runtime_timer_contract.rs`](../../tests/session_runtime_timer_contract.rs), and [`session_runtime_lifecycle_evidence.rs`](../../tests/session_runtime_lifecycle_evidence.rs). The compiled Host workflow is [`examples/session_runtime_lifecycle.rs`](../../examples/session_runtime_lifecycle.rs).

## Owner And Handles

`SessionRuntime` exclusively owns:

- the root cancellation token;
- the actor `JoinHandle`;
- the one `Box<dyn SessionLog>` after open ownership transfers;
- the one takeable `SessionEventStream` receiver;
- runner-lifecycle supervision state;
- the configured Tokio `Handle` and shutdown timeout.

`SessionHandle` is Clone and contains only stable IDs, a bounded command sender, and a watch receiver. `TurnHandle` controls one exact Turn. Neither handle owns shutdown, the log, or actor tasks.

## Options

`SessionRuntimeOptions::new(kernel, bindings, task_runtime)` validates the `KernelConfig` and verifies that the supplied Tokio runtime has a time driver. The immutable options bind one `SessionBindings` bundle for the entire loaded lifetime.

The runtime progress precondition is part of the API contract: the configured Tokio runtime must remain alive, timer-enabled, and actively driven while `create`, `load`, or `shutdown` is in progress. Merely retaining a live current-thread runtime without driving it cannot advance owner, cleanup, Port, or timeout futures.

## Create Order

`SessionRuntime::create(session_id, spec, log, options)` performs this order before returning ready:

1. validate options/kernel;
2. spawn the owner task before awaiting readiness;
3. claim the open payload exactly once;
4. validate the supplied `SessionSpec` against semantic limits;
5. validate immutable `SessionBindings` against the spec;
6. construct and validate a v3 `SessionManifest`;
7. call `SessionLog::initialize` and require a zero head;
8. construct a fresh `SessionInstanceId`;
9. construct actor state/channels as `Idle` and `Healthy` at the confirmed head;
10. send the ready handshake containing the final handle and event stream;
11. enter the actor loop.

The ready result is not visible before manifest initialization and initial state construction complete.

## Load Order

`SessionRuntime::load(expected_session_id, log, options)` performs:

1. validate options/kernel and spawn the owner;
2. claim the payload;
3. load and validate the manifest, including the expected `SessionId`;
4. validate bindings against the durable spec;
5. create an identity-bound compatibility proof;
6. replay paged entries and validate the complete canonical Conversation;
7. append restart repair atomically when an unfinished Turn exists;
8. construct a fresh `SessionInstanceId`;
9. rehydrate confirmed head and last terminal state;
10. send ready and enter the actor loop.

A failed binding check occurs before replay. A failed or uncertain repair prevents actor readiness.

## OpenGuard

`OpenGuard` owns pre-ready cleanup. Before the first open await, cleanup watchers are installed on the configured and current Tokio runtimes. A watcher may take the still-unclaimed payload only after verifying a usable timer context. Payload claim is one-shot; loser watchers can only observe an empty slot.

Owner-spawn panic, caller cancellation, ready-channel loss, and owner failure converge on the same guarded ownership path. Once the owner claims the payload, it is responsible for close. Before claim, at most one cleanup watcher closes the raw log. Failed-open close is attempted once and is reported as secondary evidence without replacing the primary open error.

Evidence includes `dropped_and_cancelled_open_owners_close_without_orphans`, `pre_poll_caller_cancellation_is_closed_by_existing_watcher`, and `fallback_cleanup_survives_caller_abort_after_close_admission`.

## Drop And Shutdown

`SessionRuntime::drop` is cancel-only. It does not block, spawn, call `block_on`, take the owner task, or claim graceful durability. `SessionHandle` and `TurnHandle` Drop have no owner-lifecycle effect; dropping a TurnHandle does not cancel its Turn.

`SessionRuntime::shutdown(self)` is the explicit durability barrier:

1. cancel the root token out of band;
2. actor state moves to `Closing`;
3. any active Turn and pending interaction are cancelled;
4. the actor settles the Turn durably when possible;
5. the log is closed exactly once;
6. owner and runner tasks are joined or aborted-and-awaited on timeout;
7. state/event/command senders close before completion.

The timeout future is constructed while briefly entered into the configured runtime, then may be polled by another executor.

## Failure Priority

Open failures preserve a primary `SessionOpenErrorKind`: invalid configuration, invalid manifest, SessionId mismatch, binding mismatch, log failure, uncertain recovery, or actor-start failure. A close failure during failed open is secondary diagnostic evidence.

Shutdown distinguishes timeout, durability failure, log-close failure, and unexpected actor termination. If active settlement/commit durability has already failed, that durability fact remains primary even when the mandatory close also fails. Core emits no fabricated terminal or `TurnFinished` after a durability failure.

Post-ready actor panic supervision keeps the active runner owned until it is joined or aborted-and-awaited, resolves the Turn as runtime-terminated, and then attempts one close. See `post_ready_actor_panic_joins_pending_runner_before_close`.

The compiled lifecycle example captures take-events/submit/wait failures after owner acquisition, always awaits shutdown, always joins the event task after shutdown, and only then returns errors in shutdown, event-task, captured-operation precedence. README contains an exact checker-enforced copy.
