# Development Plan

## Status

The final v0.3 source architecture and P6 cleanup validation are complete: the single-session owner, final Ports, private drivers, TurnRunner, actor-owned durability, and physical removal of the legacy/workspace/concrete-storage graph are present in code. The authoritative Python architecture scanner and current-document checker pass locally.

`Cargo.lock` was regenerated remotely from the cleaned manifest. The root lock now contains 37 package records; review found 39 removed records, no added records, and no retained-package version drift. The deleted dependencies are absent. Remote formatting and check commands passed, followed by the complete remote `scripts/check.sh` gate.

P8 documentation and Linux functional acceptance are complete. The nine runtime contracts, Host-boundary guide, v0.2-to-v0.3 migration guide, README lifecycle example, AT-K01 through AT-K90 evidence matrix, and v0.3 release note are current authority. The native macOS and Windows CI matrix passed in GitHub Actions [run 32705101762](https://github.com/zqcli/minicore-runtime/actions/runs/32705101762); release validation is complete and ready for publication.

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

The accurate status is: P6 source, lockfile, remote Rust/script gates, P8 documentation, Linux functional acceptance, and the native macOS and Windows CI matrix are complete; the repository is ready for publication.

## Completed P8 Documentation And Acceptance

- [x] Added nine normative contract pages under `docs/contracts/` for lifecycle, state, events, Conversation, SessionLog, Model, Tool/policy/interaction, cancellation, and extensions.
- [x] Added `docs/integration/host-boundary.md` covering Host collections, repository/lease ownership, storage/workspace capabilities, global limits, shared Ports, and shutdown-all.
- [x] Added `docs/migrations/v0.2-to-v0.3.md` with exact breaking API/ownership mappings and an external storage migration checklist.
- [x] Added the all-target `examples/session_runtime_lifecycle.rs`; README contains an exact synchronized copy whose failure path always shuts down and joins the event task before propagating errors.
- [x] Added canonical `scripts/acceptance_v03.json` and generated `docs/acceptance-v0.3.md` with one Passed row and one-or-more attributed evidence entries for every AT-K criterion (AT-K01 through AT-K90).
- [x] Added checker mutation evidence for unrelated Markdown mapping edits, stale unlisted current Markdown, and functions missing a Rust test attribute.
- [x] Added `docs/release-v0.3.md` with D-01 through D-15, breaking changes, evidence, dependency/lock review, known limitations, and upgrade guidance.
- [x] Recorded Linux functional acceptance, stable/MSRV commands, warnings-denied rustdoc, 285 root library tests, integration/provider suites, scanner output, and lock metrics.
- [x] Obtain passing native macOS and Windows CI jobs before publication ([run 32705101762](https://github.com/zqcli/minicore-runtime/actions/runs/32705101762)).

P8 documentation, functional acceptance, and native cross-platform CI are complete. The repository is a validated release candidate ready for publication.

## Current Authority Inventory

The current, non-archive documentation authority is exactly:

- `README.md`
- `CONTEXT.md`
- `docs/README.md`
- `docs/architecture.md`
- `docs/acceptance-v0.3.md`
- `docs/development-plan.md`
- `docs/release-v0.3.md`
- `docs/modules/README.md`
- `docs/contracts/session-runtime-lifecycle.md`
- `docs/contracts/session-state.md`
- `docs/contracts/event-stream.md`
- `docs/contracts/conversation.md`
- `docs/contracts/session-log.md`
- `docs/contracts/model.md`
- `docs/contracts/tool-policy-interaction.md`
- `docs/contracts/cancellation.md`
- `docs/contracts/extensions.md`
- `docs/integration/host-boundary.md`
- `docs/migrations/v0.2-to-v0.3.md`
- `docs/adr/README.md`
- `docs/adr/0200-v0.2-core-reset-uses-typed-runtime.md`
- `docs/adr/0203-model-calls-are-single-attempt-with-turn-retry.md`
- `tests/README.md`

The only non-archive Markdown allowed outside authority is the tracked root `minicore-runtime-v0.3-session-runtime-refactor-spec.md`. It records the implementation specification and review baseline without overriding current source or contract authority. The checker classifies it separately and rejects every missing authority file or other unlisted current Markdown file.

Core intentionally defines no concrete persistence-format specification because SessionLog adapters are Host-owned. Historical v0.1/v0.2 formats, migration, release, workspace, store, ADR, and review material remains under `docs/archive/v2/` and is not validated as current API documentation.

## Local Maintenance Gates

Local work for this refactor uses only non-Rust checks:

1. `python3 scripts/check_v03_architecture.py --self-test`.
2. `python3 scripts/check_v03_architecture.py`.
3. `python3 scripts/check_architecture.py` and `python3 -m scripts.check_architecture` compatibility entry points.
4. `python3 scripts/check_docs.py`.
5. Python syntax, source-contract, path, UTF-8, newline, width, and size checks.
6. `git diff --check` and verification that no unrelated files are staged or reverted.

The completed P6/P8 evidence is recorded in `docs/acceptance-v0.3.md` and `docs/release-v0.3.md`. Native macOS and Windows CI results from GitHub Actions [run 32705101762](https://github.com/zqcli/minicore-runtime/actions/runs/32705101762) are recorded without inference, completing release validation.

## Non-Core Limits

Core does not install providers, expose a shell command language, create process or filesystem adapters, own a workspace abstraction, select a persistence format, manage a session repository, migrate historical storage automatically, or supervise multiple loaded sessions. A Host may implement those capabilities behind the public Ports, but they must not silently expand SessionRuntime ownership or add ambient authority to Core.

A future change must identify its owner, public/private boundary, failure authority, cancellation owner, persistence effect, and deterministic test seam before implementation.

## Review Questions

Before changing a public value, verify that constructors and deserializers reject the same invalid shapes, diagnostics remain redacted, lifecycle mutation has one owner, and root exports change only with an explicit API decision.

Before changing a session operation, verify bounded admission, actor ownership after admission, the cancellation linearization point, truthful durable completion, owner-task joining, and state-before-event ordering.

Before changing persistence, verify expected-head semantics, corruption versus partial-tail classification, idempotent repair, close ownership, and an external migration story for incompatible formats.

Before changing a Model adapter or Host tool, verify checked descriptors/specs, external credential and network ownership, delivery/usage retention through settlement, turn-owned retry, accurate capability claims, and deterministic offline failure evidence.
