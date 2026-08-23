# ADR 0203: Model Calls Are Single-Attempt With Turn Retry

状态：Accepted

> Refined for v0.3 P3-D. The direct Model Port reports delivery truth; P5 will
> centralize stream consumption and retry in ModelDriver.

日期：2026-08-20

## Context

A host Model adapter can fail before work starts, after work starts, or when the delivery outcome is unknown. Retrying inside an adapter would make the outcome ambiguous and could duplicate a remote operation.

## Decision

Each `Model::start` call is one logical attempt. The adapter reports `DeliveryState` and a typed `ModelError`. The future P5 `ModelDriver` owns logical retries using `RetryPolicy`: one through four total attempts, base delay no greater than 30 seconds, bounded exponential delay, and an optional retry-after hint.

Automatic retry requires `retryable == true` and `delivery == NotStarted`. `Started` and `Unknown` are conservative non-retryable outcomes. A retry-after value above the remaining budget disables retry.

## Consequences

The core never silently replays a request. Host adapters own their external protocol and cleanup; `ModelDriver` owns stream assembly, panic conversion, retry timing, deadlines, and cancellation; the session actor owns final durable settlement. An unknown outcome becomes a truthful terminal error rather than an optimistic retry.

See [architecture](../architecture.md#model-retry), [model module ownership](../modules/README.md#model), and [`src/model/model.rs`](../../src/model/model.rs).
