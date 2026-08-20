# Current ADR Index

Current ADRs explain the accepted v0.2 ownership and boundary decisions. Source and the current [architecture](../architecture.md), [module map](../modules/README.md), and [formats](../formats/session-json-v2.md) remain the contract; ADRs record why the boundary is shaped this way.

## Current

| ADR | Decision |
| --- | --- |
| [0200](0200-v0.2-core-reset-uses-typed-runtime.md) | v0.2 uses one typed Runtime and one owner-tracked session graph |
| [0201](0201-workspace-files-are-capability-relative-and-processes-are-ambient.md) | filesystem tools use one root capability while direct processes retain ambient host authority |
| [0202](0202-session-storage-is-flat-v2-and-append-only.md) | session storage is flat v2 JSON plus append-only conversation JSONL |
| [0203](0203-model-calls-are-single-attempt-with-turn-retry.md) | each provider call is one attempt and only safe turn failures retry |

## Current With Later Refinements

No later refinements are currently registered. A new cross-cutting decision must add a current ADR or explicitly revise one of the four decisions above.

## Historical / Superseded

The pre-reset ADR prose is preserved under [`docs/archive/v2/pre-reset/adr/`](../archive/v2/pre-reset/adr/). Existing v2 evidence ADRs remain under [`docs/archive/v2/adr/`](../archive/v2/adr/); neither archive is current authority or indexed as a current decision.
