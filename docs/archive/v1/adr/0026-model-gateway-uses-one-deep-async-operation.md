# ADR 0026: ModelGateway Uses One Deep Async Operation

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

Status: Accepted
Date: 2026-07-16

## Context

MiniCore needs one provider-neutral model invocation seam that can support multiple providers, streaming, cancellation, authentication refresh, concurrency limits, usage normalization, retries, prompt caching and provider continuation without allowing provider details to spread into Session execution.

Earlier ADRs established that PromptSet is the only producer of model-visible context, SessionExecutor is the only owner of loaded Session execution state, and `ModelGateway.generate_model_turn(...)` is the model invocation name. The remaining question was whether ModelGateway should expose:

- one complete asynchronous operation;
- a public prepare/execute lifecycle;
- a long-lived provider Turn session;
- or a raw async provider stream.

Research of pi, Codex, Grok Build and Rig 0.40.0 shows that provider connections and streams vary substantially. Codex benefits from private WebSocket and continuation state, while pi and Rig expose typed response streams. None of those provider lifecycle shapes need to become MiniCore Session state.

## Decision

1. `MiniCoreRuntime` owns one shared `ModelGateway`.
2. ModelGateway exposes two crate-private operations to Session execution:
   - `resolve_for_turn(...) -> TurnModelSnapshot`;
   - `generate_model_turn(ModelCallRequest, ProgressEventPublisher<ModelProgressEvent>, CancellationToken) -> Result<ModelCallResult, ModelCallError>`.
3. `TurnModelSnapshot` pins one exact model definition, capability projection, effective limits and generation policy for the active Turn.
4. `ModelCallRequest.input` is an `AssembledModelContext` produced by PromptSet. ModelGateway may encode it but may not change visibility, order, Tool membership or model-visible content.
5. Provider connections, provider sessions, raw streams, authentication, retry attempts, transport fallback, prompt cache and continuation state remain private ModelGateway implementation details.
6. Streaming progress is bounded, process-local and non-authoritative. One finalized result or typed error is the only terminal value returned to SessionExecutor.
7. Provider-internal retry reuses the exact immutable request. Transparent retry requires adapter proof that the request was not sent, a provider response that explicitly proves model execution did not begin, or provider-supported idempotent replay/exact resume. Delivery outcome unknown and post-delta interruption return distinct typed errors instead of blind replay.
8. Transport fallback may switch protocol transport for the same exact model and semantics, such as WebSocket to HTTP. ModelGateway does not silently substitute another provider or model inside an active Turn.
9. SessionExecutor owns logical retry after validating the unchanged Turn identity, execution version, conversation checkpoint, TurnExecutionContext, purpose, output contract, effective max-output limit and assembled-context fingerprint.
10. Provider-reported usage on a successful response is normalized and stored with the corresponding assistant or compaction fact. Failed-attempt usage remains ModelGateway-internal telemetry in the MVP and does not create a synthetic assistant or ModelAttempt entity.
11. Provider adapters are a private internal seam. Production uses a Rig adapter, which may use Rig generic or provider-specific public types when needed to preserve terminal semantics; tests use deterministic and scripted fake adapters. Direct first-party provider HTTP clients require a follow-up decision.
12. ModelGateway and its adapters never own SessionWriter, conversation projections, AgentLoop, Tool execution or Turn terminal processing.

## Consequences

- SessionExecutor has a small model invocation interface and does not learn provider lifecycle details.
- Provider SDK churn, custom base URLs, auth refresh, rate limits, usage extraction and error mapping have strong locality inside ModelGateway.
- Codex-style WebSocket sessions and provider continuation can be implemented privately without becoming recovery state.
- Prompt cache and continuation remain optimizations over the complete logical request rather than a second conversation source.
- Cross-model substitution must occur before Turn admission through an explicit Session model choice or configuration update.
- A provider stream interrupted after partial output returns a typed error; partial output remains observer data and is discarded.
- Rig limitations, including missing generic finish reason and raw `additional_params`, are isolated in `model_gateway/provider/rig.rs`.

## Supersedes And Amends

- Refines [ADR 0009](0009-model-gateway-wraps-rig-providers.md): Rig remains private, and its streaming/provider lifecycle does not define the ModelGateway interface.
- Amends [ADR 0014](0014-model-gateway-spine-precedes-driver-integration.md): the accepted operation name is `generate_model_turn`, and the spine is now the complete deep-operation design rather than `call_model` plus caller-visible provider state.
- Implements the model invocation decision in [ADR 0023](0023-driver-starts-from-one-committed-conversation-seed.md).
- Preserves [ADR 0025](0025-loaded-session-uses-one-session-executor.md): model calls are asynchronous RunningOperations and only SessionExecutor applies their results.

## Rejected Alternatives

### Public prepare/execute lifecycle

A `PreparedModelTurn` would require callers to understand staleness, configuration timing and dropped prepared values. The same planning can remain private inside `generate_model_turn`.

### Public provider Turn session

A long-lived `ModelTurnSession` would add a second Turn-scoped lifecycle to SessionExecutor and risk treating connection or continuation state as authoritative. Provider sessions remain private optimizations.

### Return the raw provider stream

This would move provider event ordering, retry, finish detection, usage extraction and protocol errors into Session execution. ModelGateway consumes the raw stream and returns normalized progress plus one terminal value.

### Driver or AgentLoop owns provider calls

This would distribute authentication, provider mapping and retry rules into the reasoning adapter and weaken SessionExecutor ownership.

### Transparent cross-model fallback

Changing provider/model identity changes capabilities, limits, cost and semantics. It violates the exact model pinned by TurnExecutionContext.

### Durable ModelAttempt entities

Provider attempts are execution details. Persisting them would expand the domain and recovery model without improving conversation correctness.
