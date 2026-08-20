# Development Plan

## Status

P0 through P8 are complete. The current v0.2 core, public surface, persistence behavior, provider/tool ownership, fixture archival, and static architecture gate are implemented. P9 is maintenance: documentation authority reset, dependency/lockfile convergence, and only explicitly authorized non-core follow-up.

## Completed Foundations

- [x] **P0 — baseline and contract:** repository baseline, scope, checked DTO direction, and deterministic acceptance inventory.
- [x] **P1 — public values:** checked IDs, configuration values, errors, events, model/session/tool DTOs, serde constructors, and redacted diagnostics.
- [x] **P2 — tool core:** immutable registry, policy decisions, interaction client, builtins, cancellation, duplicate/unknown handling, and bounded tool values.
- [x] **P3 — model core:** provider registry, gateway resolution, checked model requests/responses, bounded event sinks, delivery-aware errors, transport drains, SSE parsing, and retry hints.
- [x] **P4 — storage:** worker-owned root lock, atomic session create, bounded CRUD, exact session JSON, append-only conversation JSONL, replay, repair, usage, transcript, and degradation behavior.
- [x] **P5 — prompt and compaction:** terminal-aware prompt projection, serialized-byte estimation, current-turn preservation, stale-safe summary append, and context-overflow recovery.
- [x] **P6 — turn execution:** private prompt/turn runner, ordered model/tool rounds, interaction claim, cancellation linearization, truthful terminal settlement, and one session mailbox.
- [x] **P7 — runtime:** typed open/create/load/close/delete/list/submit/answer/cancel/snapshot/subscribe/transcript/shutdown APIs, residency admission, restart behavior, and explicit cleanup ownership.
- [x] **P8 — reset closure:** canonical source graph, public root surface, legacy production deletion, archived historical fixtures, current M12 ownership, and active AT-20 source audit.

## Current Maintenance Gates

Every change should preserve these gates:

1. `cargo fmt --all -- --check`.
2. Rust 1.85 all-target validation with locked dependencies.
3. All-target Clippy with warnings denied.
4. Offline provider-gate contract tests and Clippy.
5. Current Markdown links, fences, and ADR index checks.
6. Source architecture scan for removed owners and forbidden coupling.
7. Git diff/show checks and a clean working tree.
8. No default network access, live credential use, or detached owner-tracked work.

The two live provider smoke cases remain explicit opt-in and ignored. They are evidence harnesses, not deterministic default gates.

## P9 Scope

- [x] **P9-01 manifest cleanup:** remove empty crate features and unused direct dependencies without hand-editing the lockfile.
- [ ] **P9-02 documentation authority:** archive pre-reset prose, establish current source-accurate docs, and reduce the checker to current authority plus selected evidence.
- [ ] **P9-03 dependency convergence:** regenerate and review `Cargo.lock` remotely after manifest cleanup; make no version bump without a separate decision.
- [ ] **P9-04 optional non-core work:** only an independently specified host adapter, process hardening seam, or provider installation change may enter after a source owner and deterministic evidence exist.

## Non-Core Limits

The core does not install a provider by default, expose a shell command language, claim an OS process-tree sandbox, or automatically migrate historical storage. It does not provide a server, CLI, GUI, ambient credential lookup, generic schema ecosystem, or compatibility wrapper. These are separate host/product decisions and must not silently expand the Runtime contract.

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
- snapshot publication and event delivery remain ordered.

Before changing persistence, confirm:

- the exact serialized field order and version are documented;
- byte, entry, text, and page limits are tested at the boundary and plus one;
- complete corruption is distinguishable from a final partial tail;
- restart repair is idempotent and preserves source history;
- an offline migration story exists if the shape is incompatible.

Before changing a provider or builtin, confirm:

- the registry freezes descriptors and names before Runtime opens;
- credential and endpoint details remain host-owned and redacted;
- provider delivery state or tool execution state is retained until settlement;
- retry is owned at the turn boundary, not hidden inside transport;
- capability and process authority claims match what the operating system can enforce;
- deterministic offline evidence covers protocol, cancellation, bounds, and failure mapping.

## Remote Release Gate

The final P9 gate is expected to run on the approved Rust 1.85 toolchain and stable toolchain without changing source behavior. It must include current Markdown/ADR validation, format, locked all-target tests, Clippy, the provider-gate package, MSRV checks, and the active acceptance matrix. Live provider smokes remain explicit opt-in and are not part of the default release result.

Dependency lockfile regeneration is a separate review step after P9-01 manifest cleanup. It must be generated by Cargo, reviewed for direct/transitive changes, and committed separately from documentation or behavioral changes. No version bump is implied by the current plan.

The P9 documentation reset is complete only when the current authority has no stale pre-reset contract, the archive boundary is explicit, and the checker validates the new files.

The P9 dependency gate is complete only when a remote Cargo regeneration is reviewed, the lockfile matches the cleaned manifest, and the locked Rust 1.85/stable gates remain green.

The P9 release note must report the source graph, public root surface, persistence formats, current ADRs, and any intentionally deferred host work without reopening historical design prose.

The current documentation set intentionally stays small: one root introduction, one maintainer context, one architecture, one module map, two formats, four current ADRs, one migration guide, and one maintenance plan.

That small set is deliberate. A current page should name a source owner, a public boundary, a durable effect, and a verification gate. Historical explanation belongs in the archive rather than in a second competing current page.

## Future Host Work

A host may build a CLI, server, GUI, credential broker, provider catalog, offline migration utility, or process supervisor around this core. Those products must own their own configuration, lifecycle, security claims, and compatibility policy. They must not add ambient authority to the core by convenience or treat private actor fields as a public protocol.
