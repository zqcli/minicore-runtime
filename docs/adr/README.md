# Current ADR Index

Current ADRs explain the accepted v0.4 execution boundaries. Source,
[architecture](../architecture.md), and the
[module map](../modules/README.md) remain the contract; ADRs record why those
boundaries exist.

## Current

| ADR | Decision |
| --- | --- |
| [0300](0300-v0.4-agent-loop-reset.md) | v0.4 converges the runtime on one live `AgentLoop`; host owns sessions, history, persistence, providers, workspaces |

A new cross-cutting decision must add a current ADR or explicitly revise this
decision.

## Historical

Session-era and pre-reset decisions (v0.1/v0.2/v0.3) are archived under
[`docs/archive/v2/adr/`](../archive/v2/adr/) and
[`docs/archive/v2/pre-reset/adr/`](../archive/v2/pre-reset/adr/). The
`provider-gate` harness keeps the standalone provider evidence. Neither
archive is current Core authority.