pub mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::stream;
use minicore_runtime::config::{KernelConfig, RetryPolicy, SessionSpec, TurnOptions, UserInput};
use minicore_runtime::conversation::TurnTerminal;
use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{
    DeliveryState, Model, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind,
    ModelEvent, ModelFinishReason, ModelRef, ModelRequest, ModelStartFuture, ModelStream,
    ReasoningPreference, Usage,
};
use minicore_runtime::session::{SessionBindings, SessionRuntime, SessionRuntimeOptions};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{BoundedText, CompactionConfig, SessionId};

use support::fake_session_log::FakeSessionLog;

fn test_diagnostic(code: DiagnosticCode) -> DiagnosticSummary {
    DiagnosticSummary::new(
        code,
        DiagnosticCategory::Model,
        BoundedText::new("test model diagnostic").unwrap(),
        true,
    )
}

fn test_descriptor() -> ModelDescriptor {
    let model_ref: ModelRef = "host:retry-test".parse().unwrap();
    ModelDescriptor::new(
        model_ref,
        4_096,
        BTreeSet::from([ReasoningPreference::Auto]),
        true,
    )
    .unwrap()
}

fn test_spec() -> SessionSpec {
    SessionSpec::new(
        "host:retry-test".parse().unwrap(),
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

struct ScriptedModel {
    descriptor: ModelDescriptor,
    attempts: Arc<AtomicUsize>,
    responses: Vec<Box<dyn Fn() -> Result<ModelStream, ModelError> + Send + Sync>>,
}

impl ScriptedModel {
    fn new(
        responses: Vec<Box<dyn Fn() -> Result<ModelStream, ModelError> + Send + Sync>>,
        attempts: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            descriptor: test_descriptor(),
            attempts,
            responses,
        }
    }
}

impl Model for ScriptedModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let index = self.attempts.fetch_add(1, Ordering::SeqCst);
        let result = if index < self.responses.len() {
            (self.responses[index])()
        } else {
            Err(ModelError::permanent(
                ModelErrorKind::Internal,
                DeliveryState::NotStarted,
                test_diagnostic(DiagnosticCode::Internal),
            ))
        };
        Box::pin(async move { result })
    }
}

fn success_stream(text: &'static str) -> ModelStream {
    let events = vec![
        Ok(ModelEvent::text_delta(text).unwrap()),
        Ok(ModelEvent::Usage {
            usage: Usage::new(1, 1, 0),
        }),
        Ok(ModelEvent::Finish {
            reason: ModelFinishReason::Stop,
        }),
    ];
    Box::pin(stream::iter(events))
}

async fn run_turn_with_model(
    session_id: SessionId,
    model: Arc<dyn Model>,
    retry_policy: RetryPolicy,
) -> TurnTerminal {
    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.retry_policy = retry_policy;
    let bindings = SessionBindings::new(model, ToolSet::default(), None, None, None);
    let options =
        SessionRuntimeOptions::new(kernel, bindings, tokio::runtime::Handle::current()).unwrap();

    let log = FakeSessionLog::new();
    let runtime = SessionRuntime::create(session_id, test_spec(), Box::new(log), options)
        .await
        .unwrap();

    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("run").unwrap(), TurnOptions::default())
        .await
        .unwrap();

    let outcome = turn.wait().await.unwrap();
    runtime.shutdown().await.unwrap();
    outcome.terminal
}

#[tokio::test(flavor = "current_thread")]
async fn model_retry_explicit_not_started_retries_and_succeeds() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let diag = test_diagnostic(DiagnosticCode::Internal);
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(
        vec![
            Box::new(move || {
                Err(ModelError::not_started(
                    ModelErrorKind::ProviderUnavailable,
                    None,
                    diag.clone(),
                ))
            }),
            Box::new(|| Ok(success_stream("done"))),
        ],
        Arc::clone(&attempts),
    ));

    let policy = RetryPolicy::new(3, Duration::ZERO).unwrap();
    let terminal = run_turn_with_model(session(86), model, policy).await;
    assert_eq!(terminal, TurnTerminal::Completed);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn model_retry_unknown_timeout_does_not_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let diag = test_diagnostic(DiagnosticCode::ModelTimeout);
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(
        vec![
            Box::new(move || Err(ModelError::unknown(ModelErrorKind::Timeout, diag.clone()))),
            Box::new(|| Ok(success_stream("unexpected"))),
        ],
        Arc::clone(&attempts),
    ));

    let policy = RetryPolicy::new(3, Duration::ZERO).unwrap();
    let terminal = run_turn_with_model(session(87), model, policy).await;
    assert!(matches!(terminal, TurnTerminal::Failed { .. }));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn model_retry_started_stream_interrupted_does_not_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let diag = test_diagnostic(DiagnosticCode::Internal);
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(
        vec![
            Box::new(move || {
                let diag = diag.clone();
                let events = vec![
                    Ok(ModelEvent::text_delta("partial").unwrap()),
                    Err(ModelError::started(ModelErrorKind::StreamInterrupted, diag)),
                ];
                let stream: ModelStream = Box::pin(stream::iter(events));
                Ok(stream)
            }),
            Box::new(|| Ok(success_stream("unexpected"))),
        ],
        Arc::clone(&attempts),
    ));

    let policy = RetryPolicy::new(3, Duration::ZERO).unwrap();
    let terminal = run_turn_with_model(session(88), model, policy).await;
    assert!(matches!(terminal, TurnTerminal::Failed { .. }));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn model_retry_rate_limited_retry_depends_on_explicit_delivery_and_hint() {
    let diag = test_diagnostic(DiagnosticCode::ModelUnavailable);

    // Case 1: not_started RateLimited with delay retries and succeeds.
    let attempts = Arc::new(AtomicUsize::new(0));
    let diag_clone = diag.clone();
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(
        vec![
            Box::new(move || {
                Err(ModelError::not_started(
                    ModelErrorKind::RateLimited,
                    Some(Duration::ZERO),
                    diag_clone.clone(),
                ))
            }),
            Box::new(|| Ok(success_stream("done"))),
        ],
        Arc::clone(&attempts),
    ));
    let policy = RetryPolicy::new(3, Duration::ZERO).unwrap();
    let terminal = run_turn_with_model(session(89), model, policy).await;
    assert_eq!(terminal, TurnTerminal::Completed);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    // Case 2: unknown RateLimited does NOT retry.
    let attempts = Arc::new(AtomicUsize::new(0));
    let diag_clone = diag.clone();
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(
        vec![
            Box::new(move || {
                Err(ModelError::unknown(
                    ModelErrorKind::RateLimited,
                    diag_clone.clone(),
                ))
            }),
            Box::new(|| Ok(success_stream("unexpected"))),
        ],
        Arc::clone(&attempts),
    ));
    let policy = RetryPolicy::new(3, Duration::ZERO).unwrap();
    let terminal = run_turn_with_model(session(189), model, policy).await;
    assert!(matches!(terminal, TurnTerminal::Failed { .. }));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    // Case 3: started RateLimited does NOT retry.
    let attempts = Arc::new(AtomicUsize::new(0));
    let diag_clone = diag.clone();
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(
        vec![
            Box::new(move || {
                Err(ModelError::started(
                    ModelErrorKind::RateLimited,
                    diag_clone.clone(),
                ))
            }),
            Box::new(|| Ok(success_stream("unexpected"))),
        ],
        Arc::clone(&attempts),
    ));
    let policy = RetryPolicy::new(3, Duration::ZERO).unwrap();
    let terminal = run_turn_with_model(session(239), model, policy).await;
    assert!(matches!(terminal, TurnTerminal::Failed { .. }));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn model_retry_sleep_responds_to_cancellation() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let diag = test_diagnostic(DiagnosticCode::Internal);
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new(
        vec![
            Box::new(move || {
                Err(ModelError::not_started(
                    ModelErrorKind::ProviderUnavailable,
                    Some(Duration::from_secs(30)),
                    diag.clone(),
                ))
            }),
            Box::new(|| Ok(success_stream("unexpected"))),
        ],
        Arc::clone(&attempts),
    ));

    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.retry_policy = RetryPolicy::new(3, Duration::from_secs(1)).unwrap();
    let bindings = SessionBindings::new(model, ToolSet::default(), None, None, None);
    let options =
        SessionRuntimeOptions::new(kernel, bindings, tokio::runtime::Handle::current()).unwrap();

    let log = FakeSessionLog::new();
    let runtime = SessionRuntime::create(session(90), test_spec(), Box::new(log), options)
        .await
        .unwrap();

    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("run").unwrap(), TurnOptions::default())
        .await
        .unwrap();

    // Advance time slightly to ensure attempt 1 fails and enters retry sleep.
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    // Cancel while in retry sleep.
    assert!(turn.cancel());

    let outcome = turn.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::CancelledByUser);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    runtime.shutdown().await.unwrap();
}
