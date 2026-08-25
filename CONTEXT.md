# Current Implementation Context

> Final v0.3 single-session release candidate after completed P6 validation, P8 documentation, Linux functional acceptance, and passing native macOS/Windows CI; ready for publication.

## Checkpoint

The crate is Rust 2024 with Rust 1.85 as its MSRV. Concrete storage acquisition, model networking, tools, workspaces, credentials, and management of multiple loaded sessions are Host responsibilities. Current authority is the source tree plus the documents linked from [docs/README.md](docs/README.md); v0.2 material is historical evidence.

## Ownership Map

- `session::SessionRuntime`: unique non-Clone owner of one created or loaded Session, root cancellation, event-stream take, actor JoinHandle, and shutdown barrier; `handle()` returns the cloneable command/watch facade.
- `session::SessionActor`: sole owner of ConversationLog mutation, bounded commands, state/event publication, one active runner, interaction resume sender, durable settlement, and close ordering.
- `session::SessionRuntimeOptions`: checked `KernelConfig`, immutable `SessionBindings`, and the Host-selected Tokio `Handle` used to spawn the whole open lifecycle.
- `conversation`: canonical entries, semantic validation, proof-gated load, paged replay, restart repair, append coordination, confirmed projection, canonical prompt-history and compaction-candidate proofs, transcript, and close classification.
- `storage::SessionLog`: the only public persistence Port. Host code acquires one exclusive adapter and passes ownership into `SessionRuntime::create/load`.
- `session`: public SessionHandle/bindings/interactions/state/events/TurnHandle plus the single-session owner and final command/runner/settlement actor.
- `model`, `tools`, `context`, `compaction`: host-neutral Ports and checked DTOs bound immutably for the loaded lifetime. Their private drivers share one crate-private deadline selector whose equal-deadline rule conservatively chooses the Turn source.
- `model::driver`: private P5-A execution module binding one direct Model, a checked Kernel-derived timeout/retry/semantic snapshot, strict stream assembly, and best-effort delta progress; no session/log/tool-execution authority.
- `agent::tool_driver` and `agent::runner_protocol`: private P5-B execution modules owning frozen-spec policy evaluation, typed approval/input suspension, panic-safe Tool execution, child cancellation, output bounds, and lossy progress. They never append, spawn, or own SessionRuntime/log authority.
- `context::driver` and `prompt::builder`: private P5-C modules owning one-provider context deadlines/panic isolation, the crate-private canonical `ValidatedContextBundle` seam, one-per-head `PromptPlan` construction/finalization, consumption of conversation-owned prompt proofs, deterministic mapping, stable context headers, exact frozen tools, and exact serialized-request output-reserved budgeting. ContextDriver is the sole production validator/constructor for the checked context bundle; PromptBuilder plans once and finalizes without reprojection. They invoke no model, tool, log, workspace, or owner.
- `compaction::driver`: private P5-D/P5-E2 module binding zero or one CompactionStrategy, a checked timeout/summary snapshot, cancellation/deadline-first preflight plus an identical post-candidate control check before boundary/strategy availability, completed-boundary-only proposal validation, scoped child cancellation, exact Turn/Port deadline provenance, and a stale-head proof. It has no conversation mutation, log, model, tool, context, workspace, or owner authority.
- `agent::runner`, `agent::turn_context`, and `agent::runner_protocol`: private P5-E1/P5-E2 Turn execution. They bind durable effective rounds, accept exact prefix-extending acknowledgements, apply cancellation-first Turn control before compaction availability/overflow decisions and after Context success, distinguish Core Turn deadlines from configured/adapter port timeouts without post-error provenance inference, build one initial PromptPlan per model round, allow at most one proactive attempt, and after a successful head change replan once without proactive retry. Forced ContextOverflow recovery also replans once and retries Context without proactive compaction. They emit stale-head Summary commit requests, retain one PromptPlan through provider/finalization, discard it when a compaction acknowledgement advances the head, preserve ordinary critical commit diagnostics, enqueue internal started progress before suspension, and retain conservative usage in every outcome and Join fallback. Public ToolStarted progress is lossy and has no ordering guarantee relative to critical suspension. They never append or own a log/runtime/workspace.
- Workspace, filesystem storage, concrete providers/tools, and multi-session management are Host responsibilities and have no Core implementation module.

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
→ ready handshake with SessionHandle
→ final command/runner/settlement actor loop
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
→ ready handshake with SessionHandle
→ final command/runner/settlement actor loop
```

`take_events` transfers the one bounded receiver exactly once. SessionHandle clones only IDs, the bounded command sender, and watch state; it owns no log, task, or shutdown authority.

## Cancellation And Shutdown

Before owner spawn and before any await, `OpenGuard` synchronously installs cleanup watchers on both the configured and current Tokio runtimes. Each captures only `SharedOpenPayload`, root cancellation, and a `payload_claimed` signal. Before entering the single-take cleanup path, a watcher panic-safely constructs and drops a zero-duration sleep in its current execution context; a no-time fallback exits `None` without taking the log. `run_open` signals claim immediately after its successful take. Owner spawn panic, pre-poll Join failure, ready-channel loss, and caller cancellation share this path.

`SessionRuntime::shutdown(self)` cancels out of band, waits for state `Closing`, active Turn resolution, log close, sender drops, and task completion. Active commit or settlement durability failure while Closing remains the primary `SessionShutdownError::Durability`; close is still attempted exactly once and any close failure is secondary internal evidence. If `shutdown_timeout` expires, shutdown aborts and awaits the same owner task before returning Timeout.

`SessionRuntimeOptions::new` synchronously validates that `task_runtime` has an enabled Tokio time driver by entering it and constructing then dropping a zero-duration sleep under `catch_unwind`. The runtime must remain timer-enabled, alive, and actively driven throughout create, load, and shutdown. Successful SessionRuntime retains the Handle; shutdown constructs its timeout under a short panic-isolated `enter()` scope, drops that scope before await, and can then be polled by a non-Tokio executor. Unexpected timeout-construction panic cancels, aborts, and awaits the same owner task before returning ActorTerminated.

Model descriptor access, SessionLog future construction and polling, and the post-ready actor loop are explicit host-controlled panic boundaries. During panic cleanup the active runner JoinHandle stays installed while awaited; if the outer shutdown timeout aborts cleanup, ActiveTurn Drop still owns and aborts the runner. Deterministic Drop-probe evidence checks the runner future is dropped before shutdown returns. Panic cleanup then resolves the Turn waiter and attempts one close.

## Conversation Memory And Commit Cost

The confirmed conversation is held entirely in memory for the loaded lifetime.
Compaction adds a Summary entry and shortens the prompt, but it does not drop
the entries it summarised: `SemanticLimits` bounds per-entry size, never the
total. A long-lived session therefore grows monotonically in resident memory,
and compaction lowers token cost without lowering RSS.

Commit cost grows with the same length. `ConversationState::candidate` builds a
non-destructive candidate so a failed durable append leaves the committed state
untouched; that two-phase rule requires copying the confirmed entries per
commit. Measured on the reference machine, this is roughly 0.4 microseconds per
entry per commit — about 400 microseconds per commit at 900 entries, and about
4 milliseconds at 10,000, incurred once per assistant message and once per tool
result.

Reads are not on this curve. A `ConversationView` produced by the log carries
the validated `ConversationState`, so prompt projection and turn-identity
proofs reuse it instead of replaying the validator; `SessionRuntime::view` is a
pointer clone rather than a copy.

Hosts that need bounded memory or flat commit cost should bound session length
and open a fresh Session, rather than relying on compaction to reclaim either.
Removing the growth inside Core requires either a persistent entry structure or
a single-phase commit, both of which are architecture decisions rather than
tuning.

## Error Boundary

`SessionOpenError` is a redacted struct with public `SessionOpenErrorKind`. It preserves invalid configuration, invalid manifest, SessionId mismatch, binding mismatch, typed log failure, recovery uncertainty, and owner-start failure. Failed-open close information is a secondary bounded diagnostic and never replaces the primary error.

`SessionShutdownError` is non-exhaustive and distinguishes Timeout, Durability, LogClose, and ActorTerminated. Diagnostics use bounded static text; raw adapter sources, paths, credentials, prompts, and response bodies are not retained.

## Cleanup Status

P4-C, P6, and P8 documentation/functional acceptance are complete: SessionRuntime exposes one cloneable SessionHandle backed by a bounded actor mailbox and watch state; the actor owns User/Assistant/Tool/Summary/terminal commits, interaction resume senders, transcript serialization, runner progress, settlement, and shutdown sequencing. No compatibility or legacy implementation graph remains. The lockfile was regenerated, remote Rust/doc gates passed, all AT-K functional rows passed on Linux, and GitHub Actions [run 32755428283](https://github.com/zqcli/minicore-runtime/actions/runs/32755428283) for commit `815494dad38c34c585dfeda3c0845ccc7c1fb7d0` passed across stable, MSRV, macOS, and Windows, validating review fixes AT-K01 through AT-K96. No package or tag release has occurred; release validation is complete and ready for publication.

## Verification

Local work for this phase uses Python architecture/docs/source checks and `git diff --check`. Remote evidence includes the regenerated 37-record lockfile, stable `scripts/check.sh`, MSRV `scripts/check-msrv.sh`, and warnings-denied rustdoc; the cleaned root library run reported 285 passing tests, with integration and provider-gate suites also passing. The complete evidence map is [docs/acceptance-v0.3.md](docs/acceptance-v0.3.md), and publication status is [docs/release-v0.3.md](docs/release-v0.3.md).
