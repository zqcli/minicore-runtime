# Development Plan

## Status

The final v0.3 source architecture and P6 cleanup validation are complete: the single-session owner, final Ports, private drivers, TurnRunner, actor-owned durability, and physical removal of the legacy/workspace/concrete-storage graph are present in code. The authoritative Python architecture scanner and current-document checker pass locally.

`Cargo.lock` was regenerated remotely from the cleaned manifest. The root lock now contains 37 package records; review found 39 removed records, no added records, and no retained-package version drift. The deleted dependencies are absent. Remote formatting and check commands passed, followed by the complete remote `scripts/check.sh` gate.

P8 user documentation is also pending. The repository does not yet contain the required Host-boundary guide, v0.2-to-v0.3 migration guide, complete SessionRuntime lifecycle example, or v0.3 release note.

## Implemented Source Milestones

- [x] Checked IDs, bounded values, configuration, manifests, errors, and host-neutral DTOs.
- [x] Canonical conversation validation, proof-gated load, replay/recovery, transcript, settlement drafts, and truthful append/close classification.
- [x] Final Model, Tool, ToolSet, ToolPolicy, ContextProvider, CompactionStrategy, SessionBindings, and SessionLog Ports.
- [x] Private ModelDriver, ToolDriver, ContextDriver, PromptBuilder, CompactionDriver, and TurnRunner execution graph.
- [x] Single-session SessionRuntime owner with SessionHandle, TurnHandle, bounded commands/events, interaction ownership, durable commits, settlement, panic supervision, and shutdown propagation.
- [x] Active-Turn-safe compaction, exact acknowledgement proofs, forced overflow recovery, and conservative usage settlement.
- [x] Physical deletion of the legacy agent/model/tool/session/prompt graph, workspace implementation, concrete storage implementations, obsolete tests, root aliases, and obsolete direct dependencies.
- [x] Authoritative v0.3 architecture scanner, compatibility delegate, fixture self-tests, current-doc checks, and final source-contract inventory.

## Completed P6 Validation

- [x] Regenerated `Cargo.lock` remotely from the cleaned `Cargo.toml` without hand-editing it.
- [x] Reviewed the root lock: 37 package records, 39 removed, 0 added, and 0 retained-package version drift.
- [x] Confirmed the deleted dependencies are absent from the regenerated lock.
- [x] Passed the supplied full remote formatting and check commands.
- [x] Passed remote `scripts/check.sh` under its current toolchain. The script covers root formatting, locked all-target tests, warnings-denied all-target/all-feature Clippy, provider-gate formatting/tests/Clippy, documentation checks, architecture scanner self-tests/full gate, and diff checks.
- [x] Observed 285 passing root library tests; the cleaned integration suites and provider-gate tests also passed. No aggregate test total is asserted here.

The accurate status is: P6 source, lockfile, and remote Rust/script gates complete. P8 user documentation and release acceptance are next.

## Pending P8 User Documentation

- [ ] Add `docs/integration/host-boundary.md` explaining that Hosts own multi-session maps, repositories, storage acquisition, workspace/process authority, credentials, and concrete adapters.
- [ ] Add `docs/migrations/v0.2-to-v0.3.md` mapping the removed Runtime/registry/store/workspace APIs to SessionRuntime, SessionHandle, SessionBindings, and pre-opened SessionLog adapters.
- [ ] Add a Host-facing create/load lifecycle example covering SessionRuntimeOptions, external Ports, state/watch, `take_events`, submit/wait, and explicit shutdown without referencing nonexistent adapters.
- [ ] Add `docs/release-v0.3.md` with breaking changes, validation results, dependency/code-size review, and the actual Rust/platform matrix.

P8 remains pending until these files exist and their examples and claims are checked against the final public API and completed remote evidence.

## Current Authority Inventory

The current, non-archive documentation authority is exactly:

- `README.md`
- `CONTEXT.md`
- `docs/README.md`
- `docs/architecture.md`
- `docs/development-plan.md`
- `docs/modules/README.md`
- `docs/adr/README.md`
- `docs/adr/0200-v0.2-core-reset-uses-typed-runtime.md`
- `docs/adr/0203-model-calls-are-single-attempt-with-turn-retry.md`
- `tests/README.md`

There are currently no authoritative format specifications, migration guide, Host-boundary guide, v0.3 release note, or complete runtime lifecycle example. Historical v0.1/v0.2 formats, migration, release, workspace, store, ADR, and review material remains under `docs/archive/v2/` and is not validated as current API documentation.

## Local Maintenance Gates

Local work for this refactor uses only non-Rust checks:

1. `python3 scripts/check_v03_architecture.py --self-test`.
2. `python3 scripts/check_v03_architecture.py`.
3. `python3 scripts/check_architecture.py` and `python3 -m scripts.check_architecture` compatibility entry points.
4. `python3 scripts/check_docs.py`.
5. Python syntax, source-contract, path, UTF-8, newline, width, and size checks.
6. `git diff --check` and verification that no unrelated files are staged or reverted.

The completed P6 remote evidence is recorded above. P8 release acceptance must document any additional release-note, example, rustdoc, or platform-matrix evidence when those checks are actually run.

## Non-Core Limits

Core does not install providers, expose a shell command language, create process or filesystem adapters, own a workspace abstraction, select a persistence format, manage a session repository, migrate historical storage automatically, or supervise multiple loaded sessions. A Host may implement those capabilities behind the public Ports, but they must not silently expand SessionRuntime ownership or add ambient authority to Core.

A future change must identify its owner, public/private boundary, failure authority, cancellation owner, persistence effect, and deterministic test seam before implementation.

## Review Questions

Before changing a public value, verify that constructors and deserializers reject the same invalid shapes, diagnostics remain redacted, lifecycle mutation has one owner, and root exports change only with an explicit API decision.

Before changing a session operation, verify bounded admission, actor ownership after admission, the cancellation linearization point, truthful durable completion, owner-task joining, and state-before-event ordering.

Before changing persistence, verify expected-head semantics, corruption versus partial-tail classification, idempotent repair, close ownership, and an external migration story for incompatible formats.

Before changing a Model adapter or Host tool, verify checked descriptors/specs, external credential and network ownership, delivery/usage retention through settlement, turn-owned retry, accurate capability claims, and deterministic offline failure evidence.
