use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::stream;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelEvent, ModelFinishReason, ModelRef,
    ModelRequest, ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::{SessionEventStream, SessionHandle};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, SessionSpec,
};

use super::fake_session_log::{FakeSessionLog, InspectionHandle};

pub struct TestModel {
    descriptor: ModelDescriptor,
    gate: Option<(Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>)>,
}

impl TestModel {
    pub fn simple(model_ref: ModelRef) -> Self {
        Self {
            descriptor: ModelDescriptor::new(
                model_ref,
                4_096,
                BTreeSet::from([ReasoningPreference::Auto]),
                true,
            )
            .unwrap(),
            gate: None,
        }
    }

    pub fn blocking(
        model_ref: ModelRef,
        started: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            descriptor: ModelDescriptor::new(
                model_ref,
                4_096,
                BTreeSet::from([ReasoningPreference::Auto]),
                true,
            )
            .unwrap(),
            gate: Some((started, release)),
        }
    }
}

impl Model for TestModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let gate = self.gate.clone();
        let cancellation = context.cancellation;
        Box::pin(async move {
            if let Some((started, release)) = gate {
                started.add_permits(1);
                tokio::select! {
                    _ = cancellation.cancelled() => {}
                    permit = release.acquire_owned() => {
                        if let Ok(permit) = permit {
                            permit.forget();
                        }
                    }
                }
            }
            let events = vec![
                Ok(ModelEvent::text_delta("done").unwrap()),
                Ok(ModelEvent::Usage {
                    usage: Usage::new(1, 1, 0),
                }),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                }),
            ];
            let stream: ModelStream = Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

pub fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

pub fn test_spec(model_ref: ModelRef) -> SessionSpec {
    SessionSpec::new(
        model_ref,
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

pub async fn create_runtime(
    session_id: SessionId,
    log: FakeSessionLog,
) -> (
    SessionRuntime,
    SessionHandle,
    InspectionHandle,
    SessionEventStream,
) {
    let model_ref: ModelRef = "host:transcript-evidence".parse().unwrap();
    let spec = test_spec(model_ref.clone());
    let model: Arc<dyn Model> = Arc::new(TestModel::simple(model_ref));
    let bindings = SessionBindings::new(model, ToolSet::default(), None, None, None);
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        bindings,
        tokio::runtime::Handle::current(),
    )
    .unwrap();

    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(session_id, spec, Box::new(log), options)
        .await
        .unwrap();

    let handle = runtime.handle();
    let events = runtime.take_events().unwrap();
    (runtime, handle, inspection, events)
}
