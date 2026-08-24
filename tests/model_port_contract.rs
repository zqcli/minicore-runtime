use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use minicore_runtime::ids::{SessionId, SessionInstanceId, ToolCallId, TurnId};
use minicore_runtime::model::{
    MAX_MODEL_EVENT_TEXT_BYTES, Model, ModelCallContext, ModelDescriptor, ModelEvent,
    ModelFinishReason, ModelLimits, ModelMessage, ModelRef, ModelRequest, ModelStartFuture,
    ModelStream, ModelValueError, ReasoningPreference, Usage,
};
use minicore_runtime::tools::{ToolName, ToolSpec};
use minicore_runtime::value::BoundedText;
use serde_json::json;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

fn session_id() -> SessionId {
    "ses_00000000000000000000000000000001".parse().unwrap()
}
fn instance_id() -> SessionInstanceId {
    "ins_00000000000000000000000000000001".parse().unwrap()
}
fn turn_id() -> TurnId {
    "trn_00000000000000000000000000000001".parse().unwrap()
}
fn call_id() -> ToolCallId {
    "call_00000000000000000000000000000001".parse().unwrap()
}

fn descriptor() -> ModelDescriptor {
    ModelDescriptor::new(
        "host:model-v1".parse::<ModelRef>().unwrap(),
        128_000,
        BTreeSet::from([
            ReasoningPreference::Auto,
            ReasoningPreference::Disabled,
            ReasoningPreference::High,
        ]),
        true,
    )
    .unwrap()
}

fn request() -> ModelRequest {
    ModelRequest::new(
        vec![ModelMessage::user("private request text").unwrap()],
        vec![
            ToolSpec::new(
                "search".parse().unwrap(),
                "private tool description",
                json!({"type": "object", "private": "schema"}),
            )
            .unwrap(),
        ],
        ModelLimits::new(Some(120_000), Some(4_096)).unwrap(),
        ReasoningPreference::High,
    )
    .unwrap()
}

fn context(cancellation: CancellationToken, deadline: Instant) -> ModelCallContext {
    ModelCallContext::new(
        session_id(),
        instance_id(),
        turn_id(),
        0,
        cancellation,
        deadline,
    )
}

fn event_name(event: &ModelEvent) -> &'static str {
    match event {
        ModelEvent::TextDelta { .. } => "text_delta",
        ModelEvent::ReasoningDelta { .. } => "reasoning_delta",
        ModelEvent::ToolCallStart { .. } => "tool_call_start",
        ModelEvent::ToolCallArgumentsDelta { .. } => "tool_call_arguments_delta",
        ModelEvent::ToolCallEnd { .. } => "tool_call_end",
        ModelEvent::Usage { .. } => "usage",
        ModelEvent::Finish { .. } => "finish",
    }
}

struct ConcurrentModel {
    descriptor: ModelDescriptor,
    barrier: Arc<Barrier>,
    starts: Arc<AtomicUsize>,
}

impl Model for ConcurrentModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        assert_eq!(request.reasoning(), ReasoningPreference::High);
        assert_eq!(context.session_id, session_id());
        assert_eq!(context.instance_id, instance_id());
        assert_eq!(context.turn_id, turn_id());
        assert_eq!(context.round, 0);
        let barrier = Arc::clone(&self.barrier);
        let starts = Arc::clone(&self.starts);
        Box::pin(async move {
            starts.fetch_add(1, Ordering::SeqCst);
            barrier.wait().await;
            let events = vec![
                Ok(ModelEvent::text_delta("done").unwrap()),
                Ok(ModelEvent::Usage {
                    usage: Usage::new(2, 1, 0),
                }),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                }),
            ];
            Ok(Box::pin(stream::iter(events)) as ModelStream)
        })
    }
}

#[tokio::test]
async fn model_trait_start_future_and_stream_are_send_and_shareable() {
    fn assert_model<T: Model + Send + Sync + 'static>() {}
    fn assert_start_send<'a>(future: ModelStartFuture<'a>) -> ModelStartFuture<'a> {
        future
    }
    fn assert_stream_send(stream: ModelStream) -> ModelStream {
        stream
    }

    assert_model::<ConcurrentModel>();
    let starts = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(ConcurrentModel {
        descriptor: descriptor(),
        barrier: Arc::new(Barrier::new(2)),
        starts: Arc::clone(&starts),
    });
    assert_eq!(model.descriptor(), &descriptor());
    let deadline = Instant::now() + Duration::from_secs(30);
    let ctx1 = context(CancellationToken::new(), deadline);
    let ctx2 = context(CancellationToken::new(), deadline);
    let first = assert_start_send(model.start(request(), ctx1));
    let second = assert_start_send(model.start(request(), ctx2));
    let (first, second) = tokio::join!(first, second);
    let mut first = assert_stream_send(first.unwrap());
    let mut second = assert_stream_send(second.unwrap());
    assert!(matches!(
        first.next().await,
        Some(Ok(ModelEvent::TextDelta { .. }))
    ));
    assert!(matches!(
        second.next().await,
        Some(Ok(ModelEvent::TextDelta { .. }))
    ));
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}

#[test]
fn descriptor_has_exact_host_neutral_fields_and_checked_invariants() {
    let descriptor = descriptor();
    assert_eq!(descriptor.model_ref.as_str(), "host:model-v1");
    assert_eq!(descriptor.context_window, 128_000);
    assert!(
        descriptor
            .supported_reasoning
            .contains(&ReasoningPreference::High)
    );
    assert!(descriptor.supports_tools);
    assert!(
        ModelDescriptor::new(
            "host:model-v1".parse().unwrap(),
            0,
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
        )
        .is_err()
    );
    assert!(
        ModelDescriptor::new("host:model-v1".parse().unwrap(), 1, BTreeSet::new(), false,).is_err()
    );
    let mut invalid = descriptor.clone();
    invalid.context_window = 0;
    assert_eq!(invalid.validate(), Err(ModelValueError::InvalidDescriptor));
    let debug = format!("{descriptor:?}");
    for forbidden in ["credential", "endpoint", "api-key", "https://"] {
        assert!(!debug.contains(forbidden));
    }

    let source = include_str!("../src/model/model.rs");
    let descriptor_source = source
        .split_once("pub struct ModelDescriptor")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    assert_eq!(
        descriptor_source
            .lines()
            .filter(|line| line.trim_start().starts_with("pub "))
            .count(),
        4
    );
    for required in [
        "pub model_ref: ModelRef",
        "pub context_window: u64",
        "pub supported_reasoning: BTreeSet<ReasoningPreference>",
        "pub supports_tools: bool",
    ] {
        assert!(source.contains(required));
    }
    for forbidden in ["Credential", "Endpoint", "ProviderId", "ModelSelection"] {
        assert!(!source.contains(forbidden));
    }
}

#[test]
fn call_context_is_zero_based_exact_and_owner_neutral() {
    let cancellation = CancellationToken::new();
    let deadline = Instant::now() + Duration::from_secs(9);
    let context = context(cancellation.clone(), deadline);
    assert_eq!(context.session_id, session_id());
    assert_eq!(context.instance_id, instance_id());
    assert_eq!(context.turn_id, turn_id());
    assert_eq!(context.round, 0);
    assert_eq!(context.deadline, deadline);
    cancellation.cancel();
    assert!(context.cancellation.is_cancelled());
    let debug = format!("{context:?}");
    assert!(debug.contains("round: 0"));

    let source = include_str!("../src/model/model.rs");
    assert!(source.contains("Zero-based model call round"));
    let context_source = source
        .split_once("pub struct ModelCallContext")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    assert_eq!(
        context_source
            .lines()
            .filter(|line| line.trim_start().starts_with("pub "))
            .count(),
        6
    );
    for forbidden in ["SessionHandle", "Workspace", "Store", "ToolSet", "Runtime"] {
        assert!(!source.contains(forbidden));
    }
}

#[test]
fn stream_events_are_typed_bounded_complete_and_redacted() {
    let name: ToolName = "search".parse().unwrap();
    let events = [
        ModelEvent::text_delta("private text delta").unwrap(),
        ModelEvent::reasoning_delta("private reasoning delta").unwrap(),
        ModelEvent::ToolCallStart {
            tool_call_id: call_id(),
            tool_name: name,
        },
        ModelEvent::tool_call_arguments_delta(call_id(), "{\"private\":true}").unwrap(),
        ModelEvent::ToolCallEnd {
            tool_call_id: call_id(),
        },
        ModelEvent::Usage {
            usage: Usage::new(3, 2, 1),
        },
        ModelEvent::Finish {
            reason: ModelFinishReason::ToolCalls,
        },
    ];
    for event in &events {
        assert!(event.validate().is_ok());
        assert!(!event_name(event).is_empty());
    }
    assert!(ModelEvent::text_delta("x".repeat(MAX_MODEL_EVENT_TEXT_BYTES)).is_ok());
    assert!(ModelEvent::text_delta("x".repeat(MAX_MODEL_EVENT_TEXT_BYTES + 1)).is_err());
    assert!(ModelEvent::reasoning_delta("x".repeat(MAX_MODEL_EVENT_TEXT_BYTES)).is_ok());
    assert!(ModelEvent::reasoning_delta("x".repeat(MAX_MODEL_EVENT_TEXT_BYTES + 1)).is_err());
    assert!(
        ModelEvent::tool_call_arguments_delta(call_id(), "x".repeat(MAX_MODEL_EVENT_TEXT_BYTES))
            .is_ok()
    );
    assert!(
        ModelEvent::tool_call_arguments_delta(
            call_id(),
            "x".repeat(MAX_MODEL_EVENT_TEXT_BYTES + 1),
        )
        .is_err()
    );
    assert!(ModelEvent::reasoning_delta("").is_err());
    let malformed = ModelEvent::TextDelta {
        delta: BoundedText::new("x".repeat(MAX_MODEL_EVENT_TEXT_BYTES + 1)).unwrap(),
    };
    assert_eq!(malformed.validate(), Err(ModelValueError::InvalidEvent));

    let debug = events
        .iter()
        .map(|event| format!("{event:?}"))
        .collect::<String>();
    for secret in [
        "private text delta",
        "private reasoning delta",
        "{\"private\":true}",
    ] {
        assert!(!debug.contains(secret));
    }
}

#[test]
fn model_request_is_checked_host_neutral_and_redacted() {
    let request = request();
    assert_eq!(request.messages().len(), 1);
    assert_eq!(request.tools().len(), 1);
    assert_eq!(request.reasoning(), ReasoningPreference::High);
    assert!(
        ModelRequest::new(
            Vec::new(),
            Vec::new(),
            ModelLimits::default(),
            ReasoningPreference::Auto,
        )
        .is_err()
    );
    let duplicate = ToolSpec::new("same".parse().unwrap(), "one", json!({})).unwrap();
    assert_eq!(
        ModelRequest::new(
            vec![ModelMessage::user("hello").unwrap()],
            vec![duplicate.clone(), duplicate],
            ModelLimits::default(),
            ReasoningPreference::Auto,
        ),
        Err(ModelValueError::InvalidTools)
    );
    let wire = serde_json::to_value(&request).unwrap();
    for forbidden in ["selection", "provider", "endpoint", "credential"] {
        assert!(wire.get(forbidden).is_none());
    }
    let debug = format!("{request:?}");
    for secret in ["private request text", "private tool description", "schema"] {
        assert!(!debug.contains(secret));
    }

    let source = include_str!("../src/model/types.rs");
    let request_source = source
        .split_once("pub struct ModelRequest")
        .and_then(|(_, tail)| tail.split_once("pub enum ModelFinishReason"))
        .map(|(body, _)| body)
        .unwrap();
    for forbidden in ["ProviderId", "ModelSelection", "endpoint", "credential"] {
        assert!(!request_source.contains(forbidden));
    }
}

#[test]
fn stream_panic_and_cancellation_catching_live_only_in_internal_model_driver() {
    let port = include_str!("../src/model/model.rs");
    assert!(port.contains("pub cancellation: CancellationToken"));
    for forbidden in [
        "catch_unwind",
        "AssertUnwindSafe",
        "tokio::select!",
        "ModelDriver",
    ] {
        assert!(!port.contains(forbidden));
    }

    let driver = include_str!("../src/model/driver.rs");
    for required in [
        "pub(crate) struct ModelDriver",
        "catch_unwind",
        "AssertUnwindSafe(start).catch_unwind()",
        "AssertUnwindSafe(stream.next()).catch_unwind()",
        "tokio::select!",
    ] {
        assert!(driver.contains(required));
    }
}

#[test]
fn model_module_has_no_legacy_or_concrete_adapter_exports() {
    let module = include_str!("../src/model/mod.rs");
    assert!(module.contains("#[path = \"model.rs\"]\nmod model_port;"));
    assert!(module.contains("mod driver;"));
    assert!(module.contains("pub(crate) use driver::{"));
    assert!(!module.contains("pub mod driver;"));
    assert!(module.contains("pub use model_port::{Model, ModelCallContext, ModelDescriptor"));
    assert!(!module.contains("mod port;"));
    assert!(!module.contains("legacy_"));
    for forbidden in [
        "pub use legacy_",
        "pub use provider",
        "pub use registry",
        "pub use gateway",
        "Credential",
        "Endpoint",
        "OpenAi",
        "Anthropic",
    ] {
        assert!(!module.contains(forbidden));
    }
}
