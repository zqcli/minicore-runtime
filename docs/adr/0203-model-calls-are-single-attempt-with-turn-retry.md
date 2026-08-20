# ADR 0203: Model Calls Are Single-Attempt With Turn Retry

状态：Accepted

日期：2026-08-20

## Context

Provider transports can fail before a request is sent, after a request may have been accepted, or after semantic output has started. Retrying at the transport layer would make the outcome ambiguous and could duplicate a remote operation.

## Decision

Each provider execution is one stateless full request and at most one HTTP attempt. The provider reports `DeliveryState` and a typed `ModelError`. The turn runner owns logical retries using `RetryPolicy`: one through four total attempts, base delay no greater than 30 seconds, bounded exponential delay, and an optional provider retry-after hint.

Only permitted transient errors with `NotSent` or `RejectedBeforeExecution` delivery may retry. `AcceptedNoOutput`, `Unknown`, and `OutputStarted` are conservative non-retryable outcomes. A retry-after value above 30 seconds disables retry. Credentials are resolved afresh within each provider attempt.

## Consequences

The model transport never silently replays a request. Provider adapters own protocol parsing and terminal proof; the turn runner owns retry timing and cancellation; the session actor owns final durable settlement. An unknown remote outcome becomes a truthful terminal error rather than an optimistic retry.

See [architecture](../architecture.md#provider-retry), [model module ownership](../modules/README.md#model), and [`src/agent/context.rs`](../../src/agent/context.rs).
