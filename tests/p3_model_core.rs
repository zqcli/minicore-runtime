use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use minicore_runtime::{
    AssistantPart, DeliveryState, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind,
    ModelEvent, ModelEventSink, ModelFinishReason, ModelFuture, ModelGateway, ModelLimits,
    ModelMessage, ModelProvider, ModelRequest, ModelResponse, ModelSelection, ProviderId,
    ProviderRegistryBuilder, ReasoningPreference, ToolSpec, Usage,
};
use tokio::sync::{Barrier, Notify};
use tokio_util::sync::CancellationToken;

fn provider_id(value: &str) -> ProviderId {
    value.parse().unwrap()
}

fn make_selection(provider: &str, model: &str) -> ModelSelection {
    ModelSelection::new(provider_id(provider), model.parse().unwrap())
}

fn descriptor(
    provider: &str,
    model: &str,
    api_model_name: &str,
    reasoning: &[ReasoningPreference],
) -> ModelDescriptor {
    ModelDescriptor::new(
        make_selection(provider, model),
        api_model_name,
        ModelLimits::new(Some(100), Some(20)).unwrap(),
        reasoning.iter().copied().collect(),
    )
    .unwrap()
}

fn response(text: &str) -> ModelResponse {
    ModelResponse::new(
        vec![AssistantPart::Text(text.to_owned())],
        ModelFinishReason::Stop,
        Usage::default(),
    )
    .unwrap()
}

fn request(
    selection: ModelSelection,
    limits: ModelLimits,
    reasoning: ReasoningPreference,
) -> ModelRequest {
    ModelRequest::new(
        selection,
        vec![ModelMessage::user("hello").unwrap()],
        Vec::<ToolSpec>::new(),
        limits,
        reasoning,
    )
    .unwrap()
}

#[derive(Clone)]
struct ScriptedProvider {
    id: ProviderId,
    models: Vec<ModelDescriptor>,
    id_calls: Arc<AtomicUsize>,
    models_calls: Arc<AtomicUsize>,
    generate_calls: Arc<AtomicUsize>,
    outcome: Result<ModelResponse, ModelError>,
    barrier: Option<Arc<Barrier>>,
}

impl ScriptedProvider {
    fn new(
        id: &str,
        models: Vec<ModelDescriptor>,
        outcome: Result<ModelResponse, ModelError>,
    ) -> Self {
        Self {
            id: provider_id(id),
            models,
            id_calls: Arc::new(AtomicUsize::new(0)),
            models_calls: Arc::new(AtomicUsize::new(0)),
            generate_calls: Arc::new(AtomicUsize::new(0)),
            outcome,
            barrier: None,
        }
    }

    fn concurrent(id: &str, models: Vec<ModelDescriptor>, barrier: Arc<Barrier>) -> Self {
        let mut provider = Self::new(id, models, Ok(response("concurrent")));
        provider.barrier = Some(barrier);
        provider
    }
}

impl ModelProvider for ScriptedProvider {
    fn id(&self) -> &ProviderId {
        self.id_calls.fetch_add(1, Ordering::SeqCst);
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        self.models_calls.fetch_add(1, Ordering::SeqCst);
        &self.models
    }

    fn generate<'a>(&'a self, _request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'a> {
        self.generate_calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self.outcome.clone();
        let barrier = self.barrier.clone();
        Box::pin(async move {
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            if ctx.cancellation().is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            ctx.publish(ModelEvent::TextDelta {
                delta: "provider event".to_owned(),
            });
            outcome
        })
    }
}

struct PanicIdProvider;

impl ModelProvider for PanicIdProvider {
    fn id(&self) -> &ProviderId {
        panic!("provider secret endpoint must be redacted")
    }

    fn models(&self) -> &[ModelDescriptor] {
        &[]
    }

    fn generate<'a>(&'a self, _request: ModelRequest, _ctx: ModelCallContext) -> ModelFuture<'a> {
        Box::pin(async { Err(ModelError::Internal) })
    }
}

struct PanicModelsProvider {
    id: ProviderId,
}

impl ModelProvider for PanicModelsProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        panic!("provider api model name must be redacted")
    }

    fn generate<'a>(&'a self, _request: ModelRequest, _ctx: ModelCallContext) -> ModelFuture<'a> {
        Box::pin(async { Err(ModelError::Internal) })
    }
}

struct LateEventProvider {
    id: ProviderId,
    models: Vec<ModelDescriptor>,
    held_sink: Arc<Mutex<Option<ModelEventSink>>>,
}

impl ModelProvider for LateEventProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        &self.models
    }

    fn generate<'a>(&'a self, _request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'a> {
        *self.held_sink.lock().unwrap() = Some(ctx.event_sink().clone());
        Box::pin(async { Ok(response("late event provider")) })
    }
}

struct PanicGenerateProvider {
    id: ProviderId,
    models: Vec<ModelDescriptor>,
}

impl ModelProvider for PanicGenerateProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        &self.models
    }

    fn generate<'a>(&'a self, _request: ModelRequest, _ctx: ModelCallContext) -> ModelFuture<'a> {
        Box::pin(async { panic!("scripted provider panic") })
    }
}

struct DropFlag(Arc<AtomicUsize>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct CancellableProvider {
    id: ProviderId,
    models: Vec<ModelDescriptor>,
    started: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}

impl ModelProvider for CancellableProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        &self.models
    }

    fn generate<'a>(&'a self, _request: ModelRequest, _ctx: ModelCallContext) -> ModelFuture<'a> {
        let started = Arc::clone(&self.started);
        let dropped = Arc::clone(&self.dropped);
        Box::pin(async move {
            let _drop_flag = DropFlag(dropped);
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(response("never returned"))
        })
    }
}

#[test]
fn registry_accepts_dynamic_providers_freezes_models_and_preserves_old_snapshots() {
    let selection_a = make_selection("scripted_a", "model_a");
    let provider = ScriptedProvider::new(
        "scripted_a",
        vec![
            descriptor(
                "scripted_a",
                "model_a",
                "wire-secret-a",
                &[ReasoningPreference::Auto, ReasoningPreference::Low],
            ),
            descriptor(
                "scripted_a",
                "model_a_2",
                "wire-secret-a-2",
                &[ReasoningPreference::Auto],
            ),
        ],
        Ok(response("a")),
    );
    let id_calls = Arc::clone(&provider.id_calls);
    let models_calls = Arc::clone(&provider.models_calls);
    let mut builder = ProviderRegistryBuilder::default();
    builder.register(provider).unwrap();
    assert_eq!(id_calls.load(Ordering::SeqCst), 1);
    assert_eq!(models_calls.load(Ordering::SeqCst), 1);
    let old_registry = builder.build();

    let provider_b = ScriptedProvider::new(
        "scripted_b",
        vec![descriptor(
            "scripted_b",
            "model_b",
            "wire-secret-b",
            &[ReasoningPreference::Auto],
        )],
        Ok(response("b")),
    );
    let mut newer_builder = ProviderRegistryBuilder::default();
    newer_builder.register(provider_b).unwrap();
    let new_registry = newer_builder.build();

    let old_gateway = ModelGateway::new(old_registry);
    let new_gateway = ModelGateway::new(new_registry);
    assert_eq!(
        old_gateway
            .resolve(&selection_a)
            .unwrap()
            .descriptor()
            .selection(),
        &selection_a
    );
    assert!(
        old_gateway
            .resolve(&make_selection("scripted_a", "model_a_2"))
            .is_ok()
    );
    assert_eq!(
        new_gateway
            .resolve(&make_selection("scripted_b", "model_b"))
            .unwrap()
            .descriptor()
            .selection(),
        &make_selection("scripted_b", "model_b")
    );
    assert_eq!(
        new_gateway.resolve(&selection_a).unwrap_err(),
        ModelError::Unavailable
    );

    let arc_provider = Arc::new(ScriptedProvider::new(
        "arc_provider",
        vec![descriptor(
            "arc_provider",
            "model",
            "wire-arc",
            &[ReasoningPreference::Auto],
        )],
        Ok(response("arc")),
    ));
    let arc_id_calls = Arc::clone(&arc_provider.id_calls);
    let mut arc_registry = ProviderRegistryBuilder::new();
    arc_registry.register(arc_provider).unwrap();
    assert_eq!(arc_id_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gateway_cannot_execute_a_model_from_a_foreign_registry_snapshot() {
    let foreign_selection = make_selection("foreign", "model");
    let foreign_provider = ScriptedProvider::new(
        "foreign",
        vec![descriptor(
            "foreign",
            "model",
            "wire-foreign",
            &[ReasoningPreference::Auto],
        )],
        Ok(response("foreign")),
    );
    let foreign_calls = Arc::clone(&foreign_provider.generate_calls);
    let mut foreign_builder = ProviderRegistryBuilder::default();
    foreign_builder.register(foreign_provider).unwrap();
    let foreign_gateway = ModelGateway::new(foreign_builder.build());
    let foreign_resolved = foreign_gateway.resolve(&foreign_selection).unwrap();
    assert_eq!(
        foreign_resolved.descriptor().selection(),
        &foreign_selection
    );

    let local_provider = ScriptedProvider::new(
        "local",
        vec![descriptor(
            "local",
            "model",
            "wire-local",
            &[ReasoningPreference::Auto],
        )],
        Ok(response("local")),
    );
    let mut local_builder = ProviderRegistryBuilder::default();
    local_builder.register(local_provider).unwrap();
    let local_gateway = ModelGateway::new(local_builder.build());
    let (sink, _events) = ModelEventSink::channel(4).unwrap();
    let result = local_gateway
        .generate(
            request(
                foreign_selection,
                ModelLimits::default(),
                ReasoningPreference::Auto,
            ),
            ModelCallContext::new(CancellationToken::new(), sink.clone()).unwrap(),
        )
        .await;
    assert_eq!(result, Err(ModelError::Unavailable));
    assert_eq!(foreign_calls.load(Ordering::SeqCst), 0);
    assert!(sink.is_closed());
}

#[test]
fn registry_rejects_duplicate_mismatch_empty_and_panicking_providers() {
    let duplicate_descriptor = descriptor(
        "duplicate",
        "model",
        "wire-name",
        &[ReasoningPreference::Auto],
    );
    let mut duplicate_models = ProviderRegistryBuilder::default();
    assert_eq!(
        duplicate_models
            .register(ScriptedProvider::new(
                "duplicate",
                vec![duplicate_descriptor.clone(), duplicate_descriptor],
                Ok(response("duplicate")),
            ))
            .unwrap_err(),
        ModelError::InvalidRequest
    );

    let mut duplicate_provider = ProviderRegistryBuilder::default();
    duplicate_provider
        .register(ScriptedProvider::new(
            "same_provider",
            vec![descriptor(
                "same_provider",
                "first",
                "wire-first",
                &[ReasoningPreference::Auto],
            )],
            Ok(response("first")),
        ))
        .unwrap();
    assert_eq!(
        duplicate_provider
            .register(ScriptedProvider::new(
                "same_provider",
                vec![descriptor(
                    "same_provider",
                    "second",
                    "wire-second",
                    &[ReasoningPreference::Auto],
                )],
                Ok(response("second")),
            ))
            .unwrap_err(),
        ModelError::InvalidRequest
    );

    let mut empty = ProviderRegistryBuilder::default();
    assert_eq!(
        empty
            .register(ScriptedProvider::new(
                "empty",
                Vec::new(),
                Ok(response("empty")),
            ))
            .unwrap_err(),
        ModelError::InvalidRequest
    );

    let mut mismatch = ProviderRegistryBuilder::default();
    assert_eq!(
        mismatch
            .register(ScriptedProvider::new(
                "owner",
                vec![descriptor(
                    "different_owner",
                    "model",
                    "wire-name",
                    &[ReasoningPreference::Auto],
                )],
                Ok(response("mismatch")),
            ))
            .unwrap_err(),
        ModelError::InvalidRequest
    );

    let mut panic_id = ProviderRegistryBuilder::default();
    assert_eq!(
        panic_id.register(PanicIdProvider).unwrap_err(),
        ModelError::Internal
    );
    let mut panic_models = ProviderRegistryBuilder::default();
    assert_eq!(
        panic_models
            .register(PanicModelsProvider {
                id: provider_id("panic_models"),
            })
            .unwrap_err(),
        ModelError::Internal
    );
}

#[tokio::test]
async fn event_sink_is_bounded_best_effort_and_context_is_checked() {
    assert!(ModelEventSink::channel(0).is_err());
    assert!(ModelEventSink::channel(1_025).is_err());
    let (sink, mut events) = ModelEventSink::channel(1).unwrap();
    assert!(!sink.publish(ModelEvent::TextDelta {
        delta: "x".repeat(65_537),
    }));
    assert!(!sink.publish(ModelEvent::TextDelta {
        delta: "unsafe\u{0001}".to_owned(),
    }));
    let cancellation = CancellationToken::new();
    let context = ModelCallContext::new(cancellation.clone(), sink.clone()).unwrap();
    assert!(!context.cancellation().is_cancelled());
    assert!(context.publish(ModelEvent::ReasoningDelta {
        delta: "first".to_owned(),
    }));
    assert!(!context.publish(ModelEvent::TextDelta {
        delta: "best effort drop".to_owned(),
    }));
    assert_eq!(
        events.recv().await.unwrap(),
        ModelEvent::ReasoningDelta {
            delta: "first".to_owned()
        }
    );
    cancellation.cancel();
    assert!(context.cancellation().is_cancelled());
    sink.close();
    assert!(sink.is_closed());
    assert!(!sink.publish(ModelEvent::TextDelta {
        delta: "late".to_owned(),
    }));
}

#[tokio::test]
async fn event_sink_close_and_publish_have_one_linearized_winner_and_drain_to_none() {
    for _ in 0..64 {
        let (sink, mut events) = ModelEventSink::channel(1).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let publish_sink = sink.clone();
        let close_sink = sink.clone();
        let publish_barrier = Arc::clone(&barrier);
        let close_barrier = Arc::clone(&barrier);
        let publish = tokio::spawn(async move {
            publish_barrier.wait().await;
            publish_sink.publish(ModelEvent::TextDelta {
                delta: "race".to_owned(),
            })
        });
        let close = tokio::spawn(async move {
            close_barrier.wait().await;
            close_sink.close();
        });
        barrier.wait().await;
        let published = publish.await.unwrap();
        close.await.unwrap();
        if published {
            assert_eq!(
                events.recv().await,
                Some(ModelEvent::TextDelta {
                    delta: "race".to_owned()
                })
            );
        }
        assert!(events.recv().await.is_none());
        assert!(!sink.publish(ModelEvent::TextDelta {
            delta: "late".to_owned(),
        }));
    }
}

#[test]
fn descriptor_and_model_errors_are_redacted_and_serde_checked() {
    let model = descriptor(
        "redacted",
        "model",
        "api-wire-name-secret",
        &[ReasoningPreference::Auto],
    );
    let debug = format!("{model:?}");
    assert!(!debug.contains("api-wire-name-secret"));
    assert!(debug.contains("redacted"));
    assert!(
        ModelDescriptor::new(
            make_selection("redacted", "model"),
            "bad\nname",
            ModelLimits::new(Some(100), Some(20)).unwrap(),
            [ReasoningPreference::Auto].into_iter().collect(),
        )
        .is_err()
    );
    assert!(
        ModelDescriptor::new(
            make_selection("redacted", "model"),
            "safe-name",
            ModelLimits::new(Some(100), Some(20)).unwrap(),
            BTreeSet::new(),
        )
        .is_err()
    );
    assert!(
        ModelDescriptor::new(
            make_selection("redacted", "model"),
            "bad\"name",
            ModelLimits::new(Some(100), Some(20)).unwrap(),
            [ReasoningPreference::Auto].into_iter().collect(),
        )
        .is_err()
    );

    let detailed = ModelError::with_delivery(
        ModelErrorKind::RateLimited,
        DeliveryState::RejectedBeforeExecution,
        Some(Duration::from_secs(2)),
    )
    .unwrap();
    assert_eq!(detailed.delivery(), DeliveryState::RejectedBeforeExecution);
    assert_eq!(detailed.retry_after(), Some(Duration::from_secs(2)));
    assert_eq!(
        serde_json::from_str::<ModelError>(&serde_json::to_string(&detailed).unwrap()).unwrap(),
        detailed
    );
    assert!(
        ModelError::detailed(
            ModelErrorKind::RateLimited,
            DeliveryState::NotSent,
            Some(Duration::from_secs(1)),
        )
        .is_err()
    );
    assert!(
        ModelError::detailed(
            ModelErrorKind::StreamInterrupted,
            DeliveryState::Unknown,
            None
        )
        .is_err()
    );
    assert!(
        ModelError::detailed(
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::OutputStarted,
            None,
        )
        .is_err()
    );
    assert!(
        ModelError::detailed(ModelErrorKind::AuthMissing, DeliveryState::Unknown, None).is_err()
    );
    assert!(
        ModelError::detailed(
            ModelErrorKind::AuthRejected,
            DeliveryState::OutputStarted,
            None,
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ModelError>(serde_json::json!({
            "type": "detailed",
            "data": {
                "kind": "stream_interrupted",
                "delivery": "unknown",
                "retry_after_ms": null
            }
        }))
        .is_err()
    );
    for kind in [
        ModelErrorKind::InvalidRequest,
        ModelErrorKind::ContextOverflow,
    ] {
        let encoded = serde_json::json!({
            "type": "detailed",
            "data": {
                "kind": serde_json::to_value(kind).unwrap(),
                "delivery": "rejected_before_execution",
                "retry_after_ms": null
            }
        });
        assert!(serde_json::from_value::<ModelError>(encoded).is_ok());
    }
    assert!(
        serde_json::from_value::<ModelError>(serde_json::json!({
            "type": "detailed",
            "data": {
                "kind": "auth_missing",
                "delivery": "rejected_before_execution",
                "retry_after_ms": null
            }
        }))
        .is_err()
    );

    let errors = [
        ModelError::InvalidRequest,
        ModelError::Unavailable,
        ModelError::ProviderUnavailable,
        ModelError::AuthMissing,
        ModelError::AuthRejected,
        ModelError::RateLimited,
        ModelError::QuotaExceeded,
        ModelError::ContextOverflow,
        ModelError::Timeout,
        ModelError::TransportUnavailable,
        ModelError::Cancelled,
        ModelError::InvalidResponse,
        ModelError::IncompleteResponse,
        ModelError::StreamInterrupted,
        ModelError::RequestOutcomeUnknown,
        ModelError::UnexpectedToolCall,
        ModelError::Internal,
    ];
    for error in errors {
        let encoded = serde_json::to_string(&error).unwrap();
        assert_eq!(serde_json::from_str::<ModelError>(&encoded).unwrap(), error);
        let debug = format!("{error:?}");
        assert!(!debug.contains("api-wire-name-secret"));
        assert!(!debug.contains("provider secret endpoint"));
    }
}

#[test]
fn detailed_model_errors_follow_the_complete_delivery_matrix() {
    let deliveries = [
        DeliveryState::NotSent,
        DeliveryState::RejectedBeforeExecution,
        DeliveryState::AcceptedNoOutput,
        DeliveryState::OutputStarted,
        DeliveryState::Unknown,
    ];
    let transient = [
        ModelErrorKind::ProviderUnavailable,
        ModelErrorKind::RateLimited,
        ModelErrorKind::Timeout,
        ModelErrorKind::TransportUnavailable,
    ];
    for kind in transient {
        for delivery in deliveries {
            let allowed = matches!(
                delivery,
                DeliveryState::NotSent | DeliveryState::RejectedBeforeExecution
            );
            assert_eq!(
                ModelError::detailed(kind, delivery, None).is_ok(),
                allowed,
                "unexpected transient matrix result for {kind:?} + {delivery:?}"
            );
            assert_eq!(
                ModelError::detailed(kind, delivery, Some(Duration::from_millis(1))).is_ok(),
                kind == ModelErrorKind::RateLimited
                    && delivery == DeliveryState::RejectedBeforeExecution,
                "unexpected retry matrix result for {kind:?} + {delivery:?}"
            );
        }
    }

    for (kind, required) in [
        (
            ModelErrorKind::QuotaExceeded,
            DeliveryState::RejectedBeforeExecution,
        ),
        (
            ModelErrorKind::AuthRejected,
            DeliveryState::RejectedBeforeExecution,
        ),
        (ModelErrorKind::AuthMissing, DeliveryState::NotSent),
        (
            ModelErrorKind::StreamInterrupted,
            DeliveryState::OutputStarted,
        ),
        (
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
        ),
    ] {
        for delivery in deliveries {
            assert_eq!(
                ModelError::detailed(kind, delivery, None).is_ok(),
                delivery == required,
                "unexpected fixed matrix result for {kind:?} + {delivery:?}"
            );
        }
    }
    for kind in [
        ModelErrorKind::InvalidRequest,
        ModelErrorKind::ContextOverflow,
    ] {
        for delivery in deliveries {
            assert_eq!(
                ModelError::detailed(kind, delivery, None).is_ok(),
                matches!(
                    delivery,
                    DeliveryState::NotSent | DeliveryState::RejectedBeforeExecution
                ),
                "unexpected local preflight matrix result for {kind:?} + {delivery:?}"
            );
        }
    }
    for delivery in deliveries {
        assert_eq!(
            ModelError::detailed(ModelErrorKind::Cancelled, delivery, None).is_ok(),
            matches!(delivery, DeliveryState::NotSent | DeliveryState::Unknown),
            "unexpected cancellation matrix result for {delivery:?}"
        );
    }
}

#[test]
fn simple_model_errors_have_conservative_default_delivery() {
    assert_eq!(ModelError::Cancelled.delivery(), DeliveryState::NotSent);
    assert_eq!(
        ModelError::RateLimited.delivery(),
        DeliveryState::RejectedBeforeExecution
    );
    assert_eq!(
        ModelError::StreamInterrupted.delivery(),
        DeliveryState::OutputStarted
    );
    assert_eq!(
        ModelError::RequestOutcomeUnknown.delivery(),
        DeliveryState::Unknown
    );
    for error in [
        ModelError::Unavailable,
        ModelError::AuthMissing,
        ModelError::InvalidRequest,
        ModelError::ContextOverflow,
    ] {
        assert_eq!(error.delivery(), DeliveryState::NotSent);
    }
    for error in [ModelError::AuthRejected, ModelError::QuotaExceeded] {
        assert_eq!(error.delivery(), DeliveryState::RejectedBeforeExecution);
    }
    for error in [
        ModelError::ProviderUnavailable,
        ModelError::Timeout,
        ModelError::TransportUnavailable,
        ModelError::InvalidResponse,
        ModelError::IncompleteResponse,
        ModelError::UnexpectedToolCall,
        ModelError::Internal,
    ] {
        assert_eq!(error.delivery(), DeliveryState::Unknown);
    }
}

#[tokio::test]
async fn gateway_resolves_validates_once_and_does_not_retry() {
    let selection = make_selection("gateway", "model");
    let provider = ScriptedProvider::new(
        "gateway",
        vec![descriptor(
            "gateway",
            "model",
            "wire-model",
            &[ReasoningPreference::Auto, ReasoningPreference::Low],
        )],
        Err(ModelError::RateLimited),
    );
    let generate_calls = Arc::clone(&provider.generate_calls);
    let mut builder = ProviderRegistryBuilder::default();
    builder.register(provider).unwrap();
    let gateway = ModelGateway::new(builder.build());
    let (sink, _events) = ModelEventSink::channel(4).unwrap();
    let context = ModelCallContext::new(CancellationToken::new(), sink).unwrap();
    let result = gateway
        .generate(
            request(
                selection,
                ModelLimits::new(Some(100), Some(20)).unwrap(),
                ReasoningPreference::Low,
            ),
            context,
        )
        .await;
    assert_eq!(result, Err(ModelError::RateLimited));
    assert_eq!(generate_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gateway_closes_late_event_sinks_and_converts_provider_panics() {
    let late_sink = Arc::new(Mutex::new(None));
    let late_selection = make_selection("late", "model");
    let late_provider = LateEventProvider {
        id: provider_id("late"),
        models: vec![descriptor(
            "late",
            "model",
            "wire-late",
            &[ReasoningPreference::Auto],
        )],
        held_sink: Arc::clone(&late_sink),
    };
    let mut late_builder = ProviderRegistryBuilder::default();
    late_builder.register(late_provider).unwrap();
    let late_gateway = ModelGateway::new(late_builder.build());
    let (sink, _events) = ModelEventSink::channel(4).unwrap();
    assert!(
        late_gateway
            .generate(
                request(
                    late_selection,
                    ModelLimits::default(),
                    ReasoningPreference::Auto,
                ),
                ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
            )
            .await
            .is_ok()
    );
    let held_sink = late_sink.lock().unwrap().take().unwrap();
    assert!(held_sink.is_closed());
    assert!(!held_sink.publish(ModelEvent::TextDelta {
        delta: "late event".to_owned(),
    }));

    let panic_selection = make_selection("panic_generate", "model");
    let mut panic_builder = ProviderRegistryBuilder::default();
    panic_builder
        .register(PanicGenerateProvider {
            id: provider_id("panic_generate"),
            models: vec![descriptor(
                "panic_generate",
                "model",
                "wire-panic",
                &[ReasoningPreference::Auto],
            )],
        })
        .unwrap();
    let panic_gateway = ModelGateway::new(panic_builder.build());
    let (panic_sink, _events) = ModelEventSink::channel(4).unwrap();
    let panic_result = panic_gateway
        .generate(
            request(
                panic_selection,
                ModelLimits::default(),
                ReasoningPreference::Auto,
            ),
            ModelCallContext::new(CancellationToken::new(), panic_sink.clone()).unwrap(),
        )
        .await;
    assert_eq!(panic_result, Err(ModelError::Internal));
    assert!(panic_sink.is_closed());
}

#[tokio::test]
async fn gateway_cancellation_drops_an_started_provider_future() {
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let selection = make_selection("cancellable", "model");
    let mut builder = ProviderRegistryBuilder::default();
    builder
        .register(CancellableProvider {
            id: provider_id("cancellable"),
            models: vec![descriptor(
                "cancellable",
                "model",
                "wire-cancellable",
                &[ReasoningPreference::Auto],
            )],
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        })
        .unwrap();
    let gateway = ModelGateway::new(builder.build());
    let cancellation = CancellationToken::new();
    let (sink, _events) = ModelEventSink::channel(4).unwrap();
    let context = ModelCallContext::new(cancellation.clone(), sink.clone()).unwrap();
    let started_wait = started.notified();
    let task_gateway = gateway.clone();
    let task = tokio::spawn(async move {
        task_gateway
            .generate(
                request(selection, ModelLimits::default(), ReasoningPreference::Auto),
                context,
            )
            .await
    });
    started_wait.await;
    cancellation.cancel();
    let cancellation_error = task.await.unwrap().unwrap_err();
    assert_eq!(cancellation_error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(cancellation_error.delivery(), DeliveryState::Unknown);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(sink.is_closed());
}

#[tokio::test]
async fn gateway_rejects_mismatch_limits_reasoning_and_pre_cancel_before_provider() {
    let selection = make_selection("validation", "model");
    let provider = ScriptedProvider::new(
        "validation",
        vec![descriptor(
            "validation",
            "model",
            "wire-model",
            &[ReasoningPreference::Disabled],
        )],
        Ok(response("should not run")),
    );
    let generate_calls = Arc::clone(&provider.generate_calls);
    let mut builder = ProviderRegistryBuilder::default();
    builder.register(provider).unwrap();
    let gateway = ModelGateway::new(builder.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();

    let mismatch = gateway
        .generate(
            request(
                make_selection("other", "model"),
                ModelLimits::default(),
                ReasoningPreference::Disabled,
            ),
            ModelCallContext::new(CancellationToken::new(), sink.clone()).unwrap(),
        )
        .await;
    assert_eq!(mismatch, Err(ModelError::Unavailable));

    let too_large = gateway
        .generate(
            request(
                selection.clone(),
                ModelLimits::new(Some(101), Some(20)).unwrap(),
                ReasoningPreference::Disabled,
            ),
            ModelCallContext::new(CancellationToken::new(), sink.clone()).unwrap(),
        )
        .await;
    assert_eq!(too_large, Err(ModelError::InvalidRequest));

    let output_too_large = gateway
        .generate(
            request(
                selection.clone(),
                ModelLimits::new(Some(100), Some(21)).unwrap(),
                ReasoningPreference::Disabled,
            ),
            ModelCallContext::new(CancellationToken::new(), sink.clone()).unwrap(),
        )
        .await;
    assert_eq!(output_too_large, Err(ModelError::InvalidRequest));

    let unsupported_reasoning = gateway
        .generate(
            request(
                selection.clone(),
                ModelLimits::default(),
                ReasoningPreference::High,
            ),
            ModelCallContext::new(CancellationToken::new(), sink.clone()).unwrap(),
        )
        .await;
    assert_eq!(unsupported_reasoning, Err(ModelError::InvalidRequest));

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = gateway
        .generate(
            request(
                selection,
                ModelLimits::default(),
                ReasoningPreference::Disabled,
            ),
            ModelCallContext::new(cancellation, sink).unwrap(),
        )
        .await;
    assert_eq!(cancelled, Err(ModelError::Cancelled));
    assert_eq!(generate_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn gateway_allows_concurrent_provider_calls_without_global_serialization() {
    let selection = make_selection("concurrent", "model");
    let barrier = Arc::new(Barrier::new(2));
    let provider = ScriptedProvider::concurrent(
        "concurrent",
        vec![descriptor(
            "concurrent",
            "model",
            "wire-model",
            &[ReasoningPreference::Auto],
        )],
        barrier,
    );
    let generate_calls = Arc::clone(&provider.generate_calls);
    let mut builder = ProviderRegistryBuilder::default();
    builder.register(provider).unwrap();
    let gateway = ModelGateway::new(builder.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let context = ModelCallContext::new(CancellationToken::new(), sink).unwrap();

    let first = gateway.generate(
        request(
            selection.clone(),
            ModelLimits::default(),
            ReasoningPreference::Auto,
        ),
        context.clone(),
    );
    let second = gateway.generate(
        request(selection, ModelLimits::default(), ReasoningPreference::Auto),
        context,
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(
        first.unwrap().parts()[0],
        AssistantPart::Text("concurrent".into())
    );
    assert_eq!(
        second.unwrap().parts()[0],
        AssistantPart::Text("concurrent".into())
    );
    assert_eq!(generate_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn p3_model_sources_stay_on_the_model_owned_dependency_boundary() {
    for source in [
        include_str!("../src/model/mod.rs"),
        include_str!("../src/model/types.rs"),
        include_str!("../src/model/provider.rs"),
        include_str!("../src/model/registry.rs"),
        include_str!("../src/model/gateway.rs"),
    ] {
        for forbidden in [
            "crate::prompt",
            "crate::session",
            "crate::runtime",
            "crate::wire",
            "crate::tools::",
            "crate::model_gateway",
            "provider_installation",
            "provider_transport",
            "openai",
            "anthropic",
            "tokio::spawn",
            "spawn_blocking",
            "allow(dead_code",
        ] {
            assert!(!source.contains(forbidden), "found forbidden {forbidden}");
        }
        assert!(!source.contains("::*"));
    }

    let lib = include_str!("../src/lib.rs");
    let types = include_str!("../src/model/types.rs");
    let provider = include_str!("../src/model/provider.rs");
    let registry = include_str!("../src/model/registry.rs");
    assert!(!types.contains("Serialize)]\npub struct ModelDescriptor"));
    assert!(!types.contains("Deserialize<'de> for ModelDescriptor"));
    assert!(!registry.contains("pub fn provider"));
    assert!(provider.contains("Mutex"));
    assert!(!provider.contains("AtomicBool"));
    assert!(lib.contains("pub use model_v2::{"));
    for required in [
        "ModelCallContext",
        "ModelDescriptor",
        "ModelErrorDetails",
        "ModelEventSink",
        "ModelGateway",
        "ModelProvider",
        "ProviderRegistry",
        "ProviderRegistryBuilder",
        "ResolvedModel",
    ] {
        assert!(lib.contains(required), "missing root export {required}");
    }
    assert!(!lib.contains("pub use model_v2::*"));
}
