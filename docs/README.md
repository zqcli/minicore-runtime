# Documentation Authority

The v0.4 core is documented by the current source tree and these pages:

1. [Architecture](architecture.md) for the one-loop boundary, ownership, and dependency direction.
2. [Canonical module map](modules/README.md) for physical source ownership and public/private boundaries.
3. [Runtime contracts](contracts/agent-loop.md) for the one-shot loop, plus the focused contracts: [model](contracts/model.md), [event-stream](contracts/event-stream.md), [cancellation](contracts/cancellation.md), [tool-policy-interaction](contracts/tool-policy-interaction.md), [history](contracts/history.md), and [prompt](contracts/prompt.md).
4. [Host boundary](integration/host-boundary.md) and the [v0.3-to-v0.4 migration](migrations/v0.3-to-v0.4.md) for integration ownership and upgrade work.
5. [Current ADR index](adr/README.md) for accepted cross-cutting decisions.
6. [Development plan](development-plan.md) for completed phases and maintenance gates.
7. [Test inventory](../tests/README.md) for current contract and integration evidence.

The [current implementation context](../CONTEXT.md) is a short maintainer
checkpoint. The [root README](../README.md) is the Host-facing introduction
and the only place that states the v0.4 positioning in prose.

The exhaustive authority set is `README.md`, `CONTEXT.md`, `tests/README.md`,
this index, `architecture.md`, `modules/README.md`, `development-plan.md`,
`adr/README.md` and the current ADR, all files under `docs/contracts/`,
`integration/host-boundary.md`, and `migrations/v0.3-to-v0.4.md`. The checker
compares that set to every non-archive Markdown file.

The tracked root `minicore-runtime-v0.4-flex-agent-loop-reset-spec.md` is the
sole explicitly allowed non-authority current Markdown file. It records the
implementation spec and review baseline; it does not override current source,
contracts, migration, or release status. The checker classifies it separately
from the current authority set. No other unlisted current Markdown is
permitted.

See `migrations/v0.3-to-v0.4.md` for what changed and what was removed; the
v0.3 acceptance and release records are preserved by the v0.3 tag and are not
part of the current tree.