# Model Port Contract

`model::Model` is the only public model-execution Port. A Session binds one Host-owned `Arc<dyn Model>` directly for its loaded lifetime; Core has no provider registry, resolver, endpoint, credential store, HTTP client, or concrete model adapter.

Source: [`src/model/model.rs`](../../src/model/model.rs), [`src/model/response.rs`](../../src/model/response.rs), and private [`src/model/driver.rs`](../../src/model/driver.rs). Evidence: [`model_port_contract.rs`](../../tests/model_port_contract.rs), [`model_driver_contract.rs`](../../tests/model_driver_contract.rs), focused tests under [`src/model/driver/tests/`](../../src/model/driver/tests/), and real-runtime ordinary failure/shared-Arc cases in [`session_runtime_context_failure_evidence.rs`](../../tests/session_runtime_context_failure_evidence.rs) and [`session_runtime_shared_ports_evidence.rs`](../../tests/session_runtime_shared_ports_evidence.rs).

## Direct Port

```rust
pub trait Model: Send + Sync + 'static {
    fn descriptor(&self) -> &ModelDescriptor;
    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a>;
}
```

`ModelStartFuture` is Send and returns a Send `ModelStream`. The Port may be shared across SessionRuntime instances; Core adds no global model lock.

## Descriptor

`ModelDescriptor` has exactly four host-neutral fields:

- `model_ref`;
- nonzero `context_window`;
- nonempty `supported_reasoning`;
- `supports_tools`.

Descriptor access is panic-isolated before execution. SessionBindings checks exact model identity, reasoning support, and tool support against the durable SessionSpec.

## Call Context

`ModelCallContext` carries exact `SessionId`, `SessionInstanceId`, `TurnId`, a zero-based round, one child cancellation token, and an absolute effective deadline. It carries no SessionHandle, log, workspace, credentials, endpoint, callback, or continuation.

`ModelRequest` contains only checked messages, frozen tools, token limits, and reasoning preference. Debug output redacts nested prompts, arguments, and model output.

## Stream Grammar

`ModelEvent` is typed:

- text and reasoning deltas;
- ToolCall start, argument delta, and matching end;
- usage;
- finish reason.

The private ModelDriver validates aggregate byte limits and the whole grammar. Tool calls retain first-start order, IDs are unique, arguments form bounded object JSON, starts/ends match, open calls cannot survive EOF, and finish shape must agree with content/tool calls. Malformed, partial, post-finish-error, panic, and interrupted streams never become AssistantMessage drafts.

Success requires a complete valid stream through EOF. Missing usage remains conservatively unknown/defaulted rather than invented.

## Delivery And Retry

`DeliveryState` is exactly `NotStarted`, `Started`, or `Unknown`. `RetryHint` is either `Never` or `Retryable { retry_after: Option<Duration> }`. `ModelError` is a structured type created exclusively through explicit constructors:

- `ModelError::not_started(kind, retry_after, diagnostic)`: sets `DeliveryState::NotStarted`, `RetryHint::Retryable`, and normalizes `diagnostic.retryable` to `true`;
- `ModelError::started(kind, diagnostic)`: sets `DeliveryState::Started`, `RetryHint::Never`, and normalizes `diagnostic.retryable` to `false`;
- `ModelError::unknown(kind, diagnostic)`: sets `DeliveryState::Unknown`, `RetryHint::Never`, and normalizes `diagnostic.retryable` to `false`;
- `ModelError::permanent(kind, delivery, diagnostic)`: preserves explicit delivery with `RetryHint::Never`, and normalizes `diagnostic.retryable` to `false`.

Wire deserialization strictly validates invariants with `#[serde(deny_unknown_fields)]`: deserializing `RetryHint::Retryable` requires `DeliveryState::NotStarted`, while `Started` or `Unknown` with `Retryable` is rejected. `RetryHint::Never` is accepted for all delivery states (including `NotStarted` as permanent). Deserialization normalizes `diagnostic.retryable` to match `retry_hint` (`true` for `Retryable`, `false` for `Never`).

ModelDriver retries only when all are true:

- delivery is explicitly `NotStarted`;
- `retry_hint` is `RetryHint::Retryable { .. }`;
- no semantic event was observed in that attempt;
- attempts remain;
- retry delay is valid and fits the effective deadline;
- cancellation has not fired.

`Started` and `Unknown` are never retried. If an adapter claims `NotStarted` after an event, Core normalizes the attempt to `Started` and `RetryHint::Never`. Retry sleep is cancellation- and deadline-aware.

## Deadline, Cancellation, And Panic

The effective deadline is the earlier of the absolute Turn deadline and configured model timeout; an equal deadline is attributed to the Turn. Driver failures preserve Turn-versus-Port deadline provenance instead of inferring it from an adapter error after the fact.

Cancellation before start reports not-started cancellation. Cancellation, timeout, or panic while polling start/stream drops the Host future/stream and reports conservative delivery. Start construction, future polling, stream polling, and descriptor inspection are panic-isolated at their defined boundaries.

## Host Cleanup Duties

A Host adapter must release request, connection, stream, and credential-broker resources when its future or stream is dropped. It must classify delivery honestly, avoid hidden retries, bound provider metadata, redact diagnostics, honor cancellation/deadline promptly, and translate provider protocol into the exact typed stream grammar. Provider-specific terminal quirks belong in the Host adapter, not in Core DTOs.
