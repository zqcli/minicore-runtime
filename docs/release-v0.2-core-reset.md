# v0.2 Core Reset Release Readiness

> Historical v0.2 release record. Its Runtime, registry, and v0.2 public-surface
> descriptions are transitional evidence; the breaking v0.3 P4-A surface is
> authoritative in the root README and current architecture/module map.

This note records the implementation and verification milestone for the MiniCore Runtime v0.2 Core Reset from baseline `5088bc254548b3e80e87179898ebb7abbea52c7d`. It records the historical source and durable contracts; it does not reopen the archived pre-reset design.

The milestone name is v0.2 Core Reset. At that historical milestone the crate package metadata was `0.1.0`; the current breaking v0.3 package is version `0.3.0`.

## Source Graph

The current transitional production crate contains 86 Rust source files and approximately 17.8k production lines. Its canonical top-level owners are `agent`, `compaction`, `config`, `context`, `conversation`, `error`, `ids`, `model`, `prompt`, `runtime`, `session`, `storage`, `time`, `tools`, `value`, and `workspace`. The production module dependency graph is a DAG: every module SCC is a singleton, with no accepted multi-module cycle.

The architecture gate requires the exact canonical file graph, rejects legacy source paths and migration aliases, checks owner-crossing imports, limits production file and function size, and freezes the direct dependency set. The current direct dependencies are `cap-primitives`, `cap-std`, `fs4`, `futures-util`, `getrandom`, `serde`, `serde_json`, `thiserror`, `time`, `tokio`, and `tokio-util`; the root `reqwest` dependency was removed with concrete model adapters in P3-D.

## Public Surface

The historical v0.2 root modules included `config`, `error`, `event`, `ids`, `model`, `runtime`, `session`, `tools`, and `workspace`. In the current v0.3 slice, `runtime` and `workspace` are private migration modules; root convenience reexports are limited to the current checked DTOs, Ports, identifiers, errors, and storage contract declared in [`src/lib.rs`](../src/lib.rs).

The host-facing entry point is `Runtime::open(config, tokio::runtime::Handle)`. Session operations use typed values and errors for create, load, close, delete, list, submit, answer, cancel, snapshot, subscribe, transcript, and shutdown. No v0.1 Wire/API compatibility wrapper is compiled.

## Persistence Formats

The runtime data directory contains one root lock and a `sessions/` namespace. Each session directory contains exactly `session.json` and `conversation.jsonl`.

- [`session.json` format v2](formats/session-json-v2.md) stores checked session configuration with fixed field order and bounded size.
- [`conversation.jsonl` v2](formats/conversation-jsonl-v2.md) is append-only, bounded, and replayed as checked semantic entries. Only a final incomplete tail is repaired; complete corruption is located and rejected.
- Restart repair appends explicit failed tool results and a `CancelledByRestart` terminal instead of rewriting history.
- Compaction appends a stale-safe summary projection and never removes source conversation records.

There is no automatic reader or in-place converter for the historical Store V1 layout. Migration is an explicit offline host operation.

## Current Decisions

The accepted current ADR set is:

- [ADR 0200](adr/0200-v0.2-core-reset-uses-typed-runtime.md): the reset uses the typed Runtime and one actor per loaded session.
- [ADR 0201](adr/0201-workspace-files-are-capability-relative-and-processes-are-ambient.md): file authority is capability-relative while direct child processes retain bounded ambient host authority.
- [ADR 0202](adr/0202-session-storage-is-flat-v2-and-append-only.md): session storage is flat v2 plus append-only conversation JSONL.
- [ADR 0203](adr/0203-model-calls-are-single-attempt-with-turn-retry.md): providers make one attempt and the turn owner applies delivery-safe logical retry.

Historical ADRs, fixtures, and pre-reset reviews remain under [`docs/archive/`](archive/) and are not current authority.

## Dependency Convergence

P9 removed unused direct dependencies and regenerated `Cargo.lock` remotely with both Rust 1.85 Cargo and stable Cargo. Both generators produced byte-identical lockfiles. The graph remains 199 package entries; regeneration accepted 20 compatible patch-version updates and added or removed no package entry. Locked stable and Rust 1.85 gates pass with the regenerated graph.

## Verification Result

The final deterministic result includes:

- stable format, locked all-target tests, all-feature Clippy with warnings denied, provider-gate tests and Clippy, current documentation checks, and the architecture gate through `scripts/check.sh`;
- Rust 1.85 locked check and all-target tests through `scripts/check-msrv.sh`;
- locked documentation generation and Cargo metadata resolution;
- [GitHub Actions run 32434427759](https://github.com/zqcli/minicore-runtime/actions/runs/32434427759): Ubuntu Rust 1.85 and stable gates plus native macOS and Windows all-target/provider-gate tests all passed;
- all 20 acceptance cases plus the acceptance inventory: 21 passed, 0 failed, 0 ignored;
- 148 library tests on the Rust 1.85 gate;
- live-provider harness source validation, with the two real-network provider smokes still explicitly ignored by default.

No credential or live network call is required by the deterministic release result.

## Deferred Host Work

The core intentionally does not include a CLI, server, GUI, provider catalog, ambient credential lookup, shell command language, process-tree sandbox, automatic Store V1 migration, or compatibility wrapper. Provider installation, endpoint and credential policy, process supervision beyond the direct child, package version publication, and product-specific host adapters remain separate decisions with separate owners and evidence.
