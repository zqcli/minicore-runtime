# Documentation Authority

> The pages below retain the v0.2 reset record as transitional migration
> evidence. The current v0.3 P4-A Port/bindings/state/event/TurnHandle seams are documented
> in the root README, `docs/architecture.md`, and `docs/modules/README.md`.

The transitional v0.2 documentation follows this order:

1. [Architecture](architecture.md) for dependency direction, owners, lifecycle, and cross-module invariants.
2. [Canonical module map](modules/README.md) for source ownership and public/private boundaries.
3. [Current formats](formats/session-json-v2.md) and [conversation JSONL v2](formats/conversation-jsonl-v2.md) for durable bytes and limits.
4. [Current ADR index](adr/README.md) for accepted decisions that explain why the current source has its boundaries.
5. [Final migration guide](migration-v0.1-v0.2.md) for the breaking public and persistence reset.
6. [v0.2 Core Reset release readiness](release-v0.2-core-reset.md) for the final source graph, public surface, persistence, dependency, verification, and deferred-host result.
7. [Development plan](development-plan.md) for completed foundations, maintenance gates, and non-core follow-up.

The [current implementation context](../CONTEXT.md) is a short checkpoint for maintainers. The [root README](../README.md) is the host-facing introduction and API example.

## Historical Material

- [archived pre-reset authority](archive/v2/pre-reset/README.md) contains the prose that described the repository before the v0.2 reset. Its internal links are intentionally historical and are not validated as current contracts.
- [archived v1 material](archive/v1/README.md) is retained for provenance only.
- [archived v2 fixtures](archive/v2/fixtures/wire-v1/README.md) remain byte-level evidence and are not default gates.
- [archived v2 ADR and review evidence](archive/v2/adr/README.md) is not current source authority.

When source and documentation disagree, source plus the current architecture, module map, formats, and ADR index win. Historical documents may explain a prior decision but may not define a current API or persistence contract.
