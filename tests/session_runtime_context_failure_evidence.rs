pub mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::stream;
use minicore_runtime::context::{
    ContextBundle, ContextError, ContextFuture, ContextProvider, ContextRequest,
};
use minicore_runtime::conversation::{ConversationEntry, TurnTerminal};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelRef, ModelStartFuture, ModelStream,
    ReasoningPreference,
};
use minicore_runtime::session::{SessionHealth, SessionStatus};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, SessionSpec, TurnOptions, UserInput,
};
use tokio::sync::Semaphore;

use support::fake_session_log::FakeSessionLog;

struct ErrorModel {
    descriptor: ModelDescriptor,
    calls: Arc<AtomicUsize>,
}

impl Model for ErrorModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ModelError::Unavailable) })
    }
}

struct NeverModel {
    descriptor: ModelDescriptor,
    calls: Arc<AtomicUsize>,
}

impl Model for NeverModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            let stream: ModelStream = Box::pin(stream::empty());
            Ok(stream)
        })
    }
}

enum ContextMode {
    Error,
    Panic,
    Pending { started: Arc<Semaphore> },
}

struct FailingContext {
    mode: ContextMode,
}

impl ContextProvider for FailingContext {
    fn provide<'a>(&'a self, _request: ContextRequest) -> ContextFuture<'a> {
        match &self.mode {
            ContextMode::Error => Box::pin(async { Err(ContextError::Unavailable) }),
            ContextMode::Panic => Box::pin(async {
                panic!("scripted context panic");
            }),
            ContextMode::Pending { started } => {
                let started = Arc::clone(started);
                Box::pin(async move {
                    started.add_permits(1);
                    std::future::pending::<Result<ContextBundle, ContextError>>().await
                })
            }
        }
    }
}

fn model_descriptor(model_ref: ModelRef) -> ModelDescriptor {
    ModelDescriptor::new(
        model_ref,
        4_096,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap()
}

fn spec(model_ref: ModelRef) -> SessionSpec {
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

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn options(bindings: SessionBindings, context_timeout: Duration) -> SessionRuntimeOptions {
    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.context_timeout = context_timeout;
    SessionRuntimeOptions::new(kernel, bindings, tokio::runtime::Handle::current()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_model_error_is_durable_failed_and_session_remains_healthy() {
    let model_ref: ModelRef = "host:model-error-evidence".parse().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(ErrorModel {
        descriptor: model_descriptor(model_ref.clone()),
        calls: Arc::clone(&calls),
    });
    let bindings =
        SessionBindings::new(model, ToolSet::builder().build().unwrap(), None, None, None);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(91),
        spec(model_ref),
        Box::new(log),
        options(bindings, Duration::from_secs(1)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(
            UserInput::text("fail model").unwrap(),
            TurnOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        turn.wait().await.unwrap().terminal,
        TurnTerminal::Failed { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(handle.state().status, SessionStatus::Idle);
    assert_eq!(handle.state().health, SessionHealth::Healthy);
    assert!(matches!(
        inspection.entries().last(),
        Some(ConversationEntry::TurnTerminal(entry))
            if matches!(entry.terminal, TurnTerminal::Failed { .. })
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn context_error_timeout_and_panic_each_fail_turn_and_keep_session_healthy() {
    run_context_case(ContextMode::Error, None, 92).await;
    run_context_case(ContextMode::Panic, None, 93).await;
    let started = Arc::new(Semaphore::new(0));
    run_context_case(
        ContextMode::Pending {
            started: Arc::clone(&started),
        },
        Some(started),
        94,
    )
    .await;
}

async fn run_context_case(mode: ContextMode, started: Option<Arc<Semaphore>>, id: u8) {
    let model_ref: ModelRef = format!("host:context-evidence-{id}").parse().unwrap();
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(NeverModel {
        descriptor: model_descriptor(model_ref.clone()),
        calls: Arc::clone(&model_calls),
    });
    let context: Arc<dyn ContextProvider> = Arc::new(FailingContext { mode });
    let bindings = SessionBindings::new(
        model,
        ToolSet::builder().build().unwrap(),
        None,
        Some(context),
        None,
    );
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(id),
        spec(model_ref),
        Box::new(log),
        options(bindings, Duration::from_secs(1)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("context").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    if let Some(started) = started {
        let permit = started.acquire_owned().await.unwrap();
        permit.forget();
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert!(matches!(
        turn.wait().await.unwrap().terminal,
        TurnTerminal::Failed { .. }
    ));
    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(handle.state().status, SessionStatus::Idle);
    assert_eq!(handle.state().health, SessionHealth::Healthy);
    assert!(matches!(
        inspection.entries().last(),
        Some(ConversationEntry::TurnTerminal(entry))
            if matches!(entry.terminal, TurnTerminal::Failed { .. })
    ));
    runtime.shutdown().await.unwrap();
}
