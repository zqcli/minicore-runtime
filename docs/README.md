# Documentation Authority

The final v0.3 Core is documented by the current source tree and these pages:

1. [Architecture](architecture.md) for owners, lifecycle, durability, and dependency direction.
2. [Canonical module map](modules/README.md) for physical source ownership and public/private boundaries.
3. [Current ADR index](adr/README.md) for accepted cross-cutting decisions.
4. [Development plan](development-plan.md) for completed phases and maintenance gates.
5. [Test inventory](../tests/README.md) for current contract and integration evidence.

The [current implementation context](../CONTEXT.md) is a short maintainer checkpoint. The [root README](../README.md) is the Host-facing introduction.

The final v0.3 architecture, regenerated lockfile, and remote P6 Rust/script gates are complete. P8 user documentation and release acceptance are next. In particular, there is not yet a current Host-boundary guide, v0.2-to-v0.3 migration guide, complete SessionRuntime lifecycle example, or v0.3 release note.

Core intentionally does not define a filesystem persistence format, workspace abstraction, provider installation format, or builtin/process adapter. Hosts implement those concerns behind the public Ports.

## Historical Material

All v0.1/v0.2 migration, persistence-format, workspace, store, release, review, and pre-reset documents are preserved under [`docs/archive/`](archive/) for provenance. Archive documents are not current API or architecture authority, and their internal source links are not validated against the final tree.

When historical prose and current authority differ, current source plus the architecture, module map, ADR index, development plan, and tests win.
