# Development Plan

## Status

P0 through P9 describe the historical v0.2 reset. The current v0.3 migration is in progress: P3-B through P4-B expose the final Port/bindings/state/event/TurnHandle foundations and the real single-session create/load/shutdown owner. P5-A ModelDriver and P5-B ToolDriver/suspension protocol are complete; P4-C/P5-C SessionHandle and actor/runner execution plus remaining workspace migration are still deferred.

## Completed Foundations

- [x] **P0 — baseline and contract:** repository baseline, scope, checked DTO direction, and deterministic acceptance inventory.
- [x] **P1 — public values:** checked IDs, configuration values, errors, events, model/session/tool DTOs, serde constructors, and redacted diagnostics.
- [x] **P2 — tool core:** historical private registry/policy/interaction implementation retained only for migration.
- [x] **P3 — model core:** provider registry, gateway resolution, checked model requests/responses, bounded event sinks, delivery-aware errors, transport drains, SSE parsing, and retry hints.
- [x] **P4 — storage:** worker-owned root lock, atomic session create, bounded CRUD, exact session JSON, append-only conversation JSONL, replay, repair, usage, transcript, and degradation behavior.
- [x] **P5 — prompt and compaction:** terminal-aware prompt projection, serialized-byte estimation, current-turn preservation, stale-safe summary append, and context-overflow recovery.
- [x] **P6 — turn execution:** private prompt/turn runner, ordered model/tool rounds, interaction claim, cancellation linearization, truthful terminal settlement, and one session mailbox.
- [x] **P7 — historical runtime:** the old public facade was removed; P4-B now physically deletes its remaining private multi-session owner implementation.
- [x] **P8 — reset closure:** historical v0.2 source graph and archive baseline.

- [x] **P3-B — Tool seam reset:** public `Tool`/`ToolSet`, checked invocation/context/progress/input/output DTOs, true legacy DTO split, concrete adapter deletion, and focused contract tests.
- [x] **P3-C — policy/approval seam:** async typed `ToolPolicy`, checked approval DTOs, and process-local interaction values; actor suspension/consumption wiring remains deferred to the owner migration.
- [x] **P3-D — direct Model Port:** host-neutral streaming `Model`, checked descriptors/contexts/events/errors/requests, private `Legacy*` runner lookup, concrete adapter/transport deletion, and focused contract coverage.
- [x] **P3-E — SessionBindings:** exact immutable Port bundle, pure spec/limits/model/tool/compaction validation, descriptor panic isolation, frozen ToolSpec semantic budgets, root export, and focused contract coverage.
- [x] **P4-A — state/event/TurnHandle foundation:** lightweight invariant-checked state, redacted diagnostics, exact event DTOs, bounded single-consumer lossy stream, exact-Turn cancellation/completion, and physical legacy observation split.
- [x] **P4-B — SessionRuntime owner lifecycle:** spawn-first OpenGuard, create/load validation and recovery, one log/state/event owner, typed open/shutdown errors, cancellation cleanup, one-shot events, deterministic shutdown, multi-owner concurrency, and deletion of the multi-session Runtime.
- [x] **P5-A — ModelDriver:** one direct Model, checked Kernel-derived configuration snapshot, strict stream assembler/tool grammar, panic conversion, effective deadline/cancellation, delivery-safe retry, lossy bounded progress, and deterministic private tests.
- [x] **P5-B — ToolDriver and suspension protocol:** frozen-spec policy evaluation, approval and ToolInput one-shot suspension, panic-safe execution, child cancellation, semantic input/output bounds, canonical input-answer encoding, lossy progress, and deterministic ownership tests.
- [ ] **P4-C/P5-C — commands and execution:** final SessionHandle/commands, turn runner, actor integration and durable commits, context/compaction, and terminal settlement through ModelDriver and ToolDriver.

## Current Maintenance Gates

Every change should preserve these gates:

1. `cargo fmt --all -- --check`.
2. Rust 1.85 all-target validation with locked dependencies.
3. All-target Clippy with warnings denied.
4. Offline provider-gate contract tests and Clippy.
5. Current Markdown links, fences, and ADR index checks.
6. `python3 scripts/check_architecture.py` for canonical paths, public surface, dependencies, production size, and an all-singleton module DAG.
7. Git diff/show checks and a clean working tree.
8. No default network access, live credential use, or detached owner-tracked work.

Historical live-network smoke cases are not root-crate targets. The standalone provider-gate package remains deterministic independent evidence.

## P9 Scope

- [x] **P9-01 manifest cleanup:** remove empty crate features and unused direct dependencies without hand-editing the lockfile.
- [x] **P9-02 documentation authority:** archive pre-reset prose, establish current source-accurate docs, and reduce the checker to current authority plus selected evidence.
- [x] **P9-03 automated quality gates:** enforce the canonical source graph, public root surface, direct dependency policy, production size/function limits, and an all-singleton module DAG through `scripts/check_architecture.py`.
- [x] **P9-04 dependency convergence:** regenerate and review `Cargo.lock` remotely with Rust 1.85 and stable Cargo; the subsequent v0.3 package bump updates only the root package entry.
- [x] **P9-05 scope closure:** admit no optional non-core work; host adapters, additional process hardening, provider installation policy, migration tooling, and package publication remain separate decisions with separate owners and evidence.

## Non-Core Limits

The core does not install a provider by default, expose a shell command language, claim an OS process-tree sandbox, or automatically migrate historical storage. It does not provide a server, CLI, GUI, ambient credential lookup, generic schema ecosystem, multi-session supervisor, repository, or compatibility wrapper. These are separate host/product decisions and must not silently expand the SessionRuntime contract.

A future change must identify its owner, public/private boundary, failure authority, cancellation owner, persistence effect, and deterministic test seam before implementation. Documentation changes should update the current authority first and leave historical material under the archive.

## Maintenance Review Questions

Before changing a public value, confirm:

- the constructor and deserializer reject the same invalid shapes;
- Debug, Display, Serialize, and error text do not disclose secrets or host paths;
- the owning module remains the only writer of the value's lifecycle;
- the public root export list changes only when the API contract changes;
- the current module map and an accepted ADR explain the new boundary.

Before changing a session operation, confirm:

- admission is bounded and has a typed rejection;
- the actor owns the accepted work after the caller future is dropped;
- cancellation has an identified linearization point;
- every durable barrier has a truthful completion result;
- close and shutdown join all owner-tracked work;
- authoritative state updates precede best-effort event delivery.

Before changing persistence, confirm:

- the exact serialized field order and version are documented;
- byte, entry, text, and page limits are tested at the boundary and plus one;
- complete corruption is distinguishable from a final partial tail;
- restart repair is idempotent and preserves source history;
- an offline migration story exists if the shape is incompatible.

Before changing a Model adapter or host tool, confirm:

- the bound descriptor and tool specs are checked before execution;
- network configuration remains outside the core and redacted by the host adapter;
- model delivery state or tool execution state is retained until settlement;
- retry is owned at the turn boundary, not hidden inside transport;
- capability and process authority claims match what the operating system can enforce;
- deterministic offline evidence covers protocol, cancellation, bounds, and failure mapping.

## Remote Release Gate

The final historical P9 gate ran on the approved Rust 1.85 toolchain and stable toolchain without changing source behavior. It included current Markdown/ADR validation, format, locked all-target tests, Clippy, the provider-gate package, MSRV checks, locked documentation generation, Cargo metadata resolution, and the active acceptance matrix. Native macOS and Windows jobs also passed in [GitHub Actions run 32434427759](https://github.com/zqcli/minicore-runtime/actions/runs/32434427759).

Dependency lockfile regeneration was committed separately after P9-01 manifest cleanup. Rust 1.85 and stable Cargo produced byte-identical lockfiles. Review confirmed a stable 199-package graph with 20 compatible patch-version updates, no package entry added or removed, and green locked gates. The current v0.3 package metadata is `0.3.0`; publication remains a separate decision.

The P9 documentation reset is complete: current authority has no stale pre-reset contract, the archive boundary is explicit, and the checker validates the current files.

The P9 dependency gate is complete: remote Cargo regeneration was reviewed, the lockfile matches the cleaned manifest, and the locked Rust 1.85/stable gates are green.

The [P9 release-readiness note](release-v0.2-core-reset.md) reports the source graph, public root surface, persistence formats, current ADRs, dependency convergence, verification result, and intentionally deferred host work without reopening historical design prose.

The current documentation set intentionally stays small: one root introduction, one maintainer context, one architecture, one module map, two formats, four current ADRs, one migration guide, one release-readiness note, and one maintenance plan.

That small set is deliberate. A current page should name a source owner, a public boundary, a durable effect, and a verification gate. Historical explanation belongs in the archive rather than in a second competing current page.

## Future Host Work

A host may build a CLI, server, GUI, credential broker, provider catalog, offline migration utility, or process supervisor around this core. Those products must own their own configuration, lifecycle, security claims, and compatibility policy. They must not add ambient authority to the core by convenience or treat private actor fields as a public protocol.
