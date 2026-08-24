# ADR 0203: Model Calls Are Single-Attempt With Turn Retry

状态：Accepted

> Refined for v0.3 P5-A. The direct Model Port reports delivery truth;
> ModelDriver now centralizes stream consumption and retry.

日期：2026-08-20

## Context

A host Model adapter can fail before work starts, after work starts, or when the delivery outcome is unknown. Retrying inside an adapter would make the outcome ambiguous and could duplicate a remote operation.

## Decision

Each `Model::start` call is one logical attempt. The adapter reports `DeliveryState` and a structured `ModelError` with an explicit `RetryHint`. The private `ModelDriver` owns logical retries using the checked Kernel-derived RetryPolicy snapshot: one through four total attempts, base delay no greater than 30 seconds, bounded exponential delay, and an optional retry-after hint.

Automatic retry requires `delivery == DeliveryState::NotStarted`, `retry_hint == RetryHint::Retryable { .. }`, no semantic event observed in the attempt, a remaining attempt, a valid delay, and remaining overall deadline. `Started` and `Unknown` are conservative non-retryable outcomes with `RetryHint::Never`. A retry-after value above 30 seconds or beyond the remaining budget disables retry. An adapter claiming NotStarted after an event is normalized to Started and never retried.

## Consequences

The core never silently replays a request. Host adapters own their external protocol and cleanup; `ModelDriver` owns strict stream assembly, panic conversion, retry timing, one shared effective deadline with exact Turn/Port provenance, cancellation, and lossy delta progress. TurnRunner consumes detailed failures: Core Turn timeout becomes budget exhaustion and configured or adapter Model timeout remains ModelTimeout. Compaction may recover only local Prompt ContextOverflow through one committed attempt; it does not reinterpret delivery truth or blindly replay a Model call. SessionActor owns final durable settlement. An unknown outcome becomes a truthful terminal error rather than an optimistic retry.

See [architecture](../architecture.md#model-retry), [execution module ownership](../modules/README.md#execution-modules), [`src/model/model.rs`](../../src/model/model.rs), and [`src/model/driver.rs`](../../src/model/driver.rs).
