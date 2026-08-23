# Current ADR Index

Current ADRs explain the accepted final v0.3 ownership and execution boundaries. Source, [architecture](../architecture.md), and the [module map](../modules/README.md) remain the contract; ADRs record why those boundaries exist.

## Current

| ADR | Decision |
| --- | --- |
| [0200](0200-v0.2-core-reset-uses-typed-runtime.md) | Core exposes one typed SessionRuntime owner per loaded session and no multi-session facade |
| [0203](0203-model-calls-are-single-attempt-with-turn-retry.md) | each Model call is one attempt and only delivery-safe failures retry |

A new cross-cutting decision must add a current ADR or explicitly revise one of these decisions.

## Historical

Workspace/process authority and flat-v2 storage decisions are archived under [`docs/archive/v2/adr/`](../archive/v2/adr/). Pre-reset ADR prose remains under [`docs/archive/v2/pre-reset/adr/`](../archive/v2/pre-reset/adr/). Neither archive is current Core authority.
