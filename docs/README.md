# Documentation Authority

The final v0.3 Core is documented by the current source tree and these pages:

1. [Architecture](architecture.md) for owners, lifecycle, durability, and dependency direction.
2. [Canonical module map](modules/README.md) for physical source ownership and public/private boundaries.
3. [Runtime contracts](contracts/session-runtime-lifecycle.md) for the nine normative lifecycle, state, event, conversation, log, model, tool/interaction, cancellation, and extension contracts.
4. [Host boundary](integration/host-boundary.md) and [v0.2-to-v0.3 migration](migrations/v0.2-to-v0.3.md) for integration ownership and upgrade work.
5. [Acceptance matrix](acceptance-v0.3.md), generated from [`scripts/acceptance_v03.json`](../scripts/acceptance_v03.json), and [v0.3 release note](release-v0.3.md) for reviewed AT traceability, validation environment, limitations, and publication status.
6. [Current ADR index](adr/README.md) for accepted cross-cutting decisions.
7. [Development plan](development-plan.md) for completed phases and maintenance gates.
8. [Test inventory](../tests/README.md) for current contract and integration evidence.

The [current implementation context](../CONTEXT.md) is a short maintainer checkpoint. The [root README](../README.md) is the Host-facing introduction.

The exhaustive authority set is `README.md`, `CONTEXT.md`, `tests/README.md`, this index, `architecture.md`, `modules/README.md`, `development-plan.md`, `acceptance-v0.3.md`, `release-v0.3.md`, both current ADRs plus their index, all nine files under `docs/contracts/`, `integration/host-boundary.md`, and `migrations/v0.2-to-v0.3.md`. The checker compares that set to every non-archive Markdown file.

The root `minicore-runtime-v0.3-session-runtime-refactor-spec.md` is the sole explicitly allowed non-authority current Markdown file. It is untracked implementation input and **MUST NOT be committed**. The checker permits its presence or absence and does not inspect Git tracking; when present, it does not override source, contracts, acceptance mapping, migration, or release status. No other unlisted current Markdown is permitted.

The final v0.3 architecture, regenerated lockfile, remote Rust gates, P8 documentation, and Linux functional acceptance are complete. Native macOS and Windows jobs are configured but were not executed in this session, so release publication remains blocked on external CI.

Core intentionally does not define a filesystem persistence format, workspace abstraction, provider installation format, or builtin/process adapter. Hosts implement those concerns behind the public Ports.

## Contract Set

- [SessionRuntime lifecycle](contracts/session-runtime-lifecycle.md)
- [Session state](contracts/session-state.md)
- [Event stream](contracts/event-stream.md)
- [Conversation](contracts/conversation.md)
- [SessionLog](contracts/session-log.md)
- [Model](contracts/model.md)
- [Tool, policy, and interaction](contracts/tool-policy-interaction.md)
- [Cancellation and task ownership](contracts/cancellation.md)
- [Extensions](contracts/extensions.md)

## Historical Material

All v0.1/v0.2 migration, persistence-format, workspace, store, release, review, and pre-reset documents are preserved under [`docs/archive/`](archive/) for provenance. Archive documents are not current API or architecture authority, and their internal source links are not validated against the final tree.

When historical prose and current authority differ, current source plus the architecture, module map, ADR index, development plan, and tests win.
