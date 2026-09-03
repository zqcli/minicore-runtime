use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{Stream, stream};
use serde_json::{Value, json};
use tokio::sync::{Barrier, Notify, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::{LoopId, ToolCallId};
use crate::model::{
    AssistantPart, ModelFinishReason, ModelLimits, ModelMessage, ModelRef, ModelStartFuture,
    ModelStream, ReasoningPreference, RetryHint, Usage,
};
use crate::tools::ToolSpec;

#[cfg(test)]
mod assembly;
#[cfg(test)]
mod cancellation;
#[cfg(test)]
mod deadline;
#[cfg(test)]
mod preflight_progress;
#[cfg(test)]
mod retry;
#[cfg(test)]
mod semantics;
#[cfg(test)]
mod settlement;

/// Test-local mirror of the removed session kernel config: the driver needs
/// only a timeout, a retry budget, and the semantic limits tuple.
#[derive(Clone)]
struct RetryPolicy {
    attempts: u8,
    base_delay: Duration,
}

impl RetryPolicy {
    fn new(attempts: u8, base_delay: Duration) -> Result<Self, &'static str> {
        Ok(Self {
            attempts,
            base_delay,
        })
    }

    fn max_attempts(&self) -> u8 {
        self.attempts
    }

    fn base_delay(&self) -> Duration {
        self.base_delay
    }
}

#[derive(Clone)]
struct SemanticLimits {
    max_tool_calls_per_response: usize,
    max_tool_name_bytes: usize,
    max_tool_schema_bytes: usize,
    max_tool_input_bytes: usize,
    max_model_text_bytes_per_round: usize,
    max_model_reasoning_bytes_per_round: usize,
}

impl Default for SemanticLimits {
    fn default() -> Self {
        Self {
            max_tool_calls_per_response: 64,
            max_tool_name_bytes: 64,
            max_tool_schema_bytes: crate::value::MAX_JSON_BYTES,
            max_tool_input_bytes: crate::value::MAX_JSON_BYTES,
            max_model_text_bytes_per_round: BoundedText::MAX_BYTES,
            max_model_reasoning_bytes_per_round: BoundedText::MAX_BYTES,
        }
    }
}

#[derive(Clone)]
struct KernelConfig {
    model_call_timeout: Duration,
    retry_policy: RetryPolicy,
    limits: SemanticLimits,
}

impl KernelConfig {
    fn default_checked() -> Result<Self, &'static str> {
        Ok(Self {
            model_call_timeout: Duration::from_secs(60),
            retry_policy: RetryPolicy::new(2, Duration::ZERO).unwrap(),
            limits: SemanticLimits::default(),
        })
    }
}

fn loop_id() -> LoopId {
    "lup_00000000000000000000000000000041".parse().unwrap()
}

fn call_id(value: u8) -> ToolCallId {
    format!("call_{value:032}").parse().unwrap()
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec::new(name.parse().unwrap(), "tool", json!({"type": "object"})).unwrap()
}

fn descriptor() -> ModelDescriptor {
    ModelDescriptor::new(
        "host:driver".parse::<ModelRef>().unwrap(),
        128,
        BTreeSet::from([
            ReasoningPreference::Auto,
            ReasoningPreference::Disabled,
            ReasoningPreference::High,
        ]),
        true,
    )
    .unwrap()
}

fn request_with(
    reasoning: ReasoningPreference,
    tools: Vec<ToolSpec>,
    context_window: Option<u32>,
) -> ModelRequest {
    ModelRequest::new(
        vec![ModelMessage::user("request").unwrap()],
        tools,
        ModelLimits::new(context_window, Some(32)).unwrap(),
        reasoning,
    )
    .unwrap()
}

fn request() -> ModelRequest {
    request_with(ReasoningPreference::High, Vec::new(), Some(64))
}

fn tool_request() -> ModelRequest {
    request_with(
        ReasoningPreference::High,
        vec![tool_spec("search")],
        Some(64),
    )
}

fn context(cancellation: CancellationToken, deadline_after: Duration) -> ModelCallContext {
    ModelCallContext::new(loop_id(), 0, cancellation, Instant::now() + deadline_after)
}

fn kernel(retry_policy: RetryPolicy) -> KernelConfig {
    KernelConfig {
        model_call_timeout: Duration::from_secs(60),
        retry_policy,
        ..KernelConfig::default_checked().unwrap()
    }
}

fn limits_kernel(limits: SemanticLimits) -> KernelConfig {
    KernelConfig {
        model_call_timeout: Duration::from_secs(60),
        limits,
        ..KernelConfig::default_checked().unwrap()
    }
}

fn driver_config(kernel: &KernelConfig) -> ModelDriverConfig {
    ModelDriverConfig::from_kernel_values(
        kernel.model_call_timeout,
        kernel.retry_policy.max_attempts(),
        kernel.retry_policy.base_delay(),
        SemanticLimitsSnapshot::from_kernel_values(
            kernel.limits.max_tool_calls_per_response,
            kernel.limits.max_tool_name_bytes,
            kernel.limits.max_tool_schema_bytes,
            kernel.limits.max_tool_input_bytes,
            kernel.limits.max_model_text_bytes_per_round,
            kernel.limits.max_model_reasoning_bytes_per_round,
        ),
    )
}

fn finish(reason: ModelFinishReason) -> ModelEvent {
    ModelEvent::Finish { reason }
}

fn start(id: ToolCallId, name: &str) -> ModelEvent {
    ModelEvent::ToolCallStart {
        tool_call_id: id,
        tool_name: name.parse().unwrap(),
    }
}

fn args(id: ToolCallId, value: &str) -> ModelEvent {
    ModelEvent::tool_call_arguments_delta(id, value).unwrap()
}

fn end(id: ToolCallId) -> ModelEvent {
    ModelEvent::ToolCallEnd { tool_call_id: id }
}

fn text_success(value: &str) -> Vec<Result<ModelEvent, ModelError>> {
    vec![
        Ok(ModelEvent::text_delta(value).unwrap()),
        Ok(finish(ModelFinishReason::Stop)),
    ]
}

fn retryable_error(retry_after: Option<Duration>) -> ModelError {
    let diagnostic = DiagnosticSummary::new(
        DiagnosticCode::ModelUnavailable,
        DiagnosticCategory::Model,
        BoundedText::new("provider unavailable").unwrap(),
        true,
    );
    ModelError::not_started(ModelErrorKind::ProviderUnavailable, retry_after, diagnostic)
}

fn test_error(kind: ModelErrorKind) -> ModelError {
    let diagnostic = DiagnosticSummary::new(
        DiagnosticCode::Internal,
        DiagnosticCategory::Model,
        BoundedText::new("test model error").unwrap(),
        false,
    );
    ModelError::permanent(kind, DeliveryState::NotStarted, diagnostic)
}

fn progress_channel() -> (
    mpsc::Sender<ModelDriverProgress>,
    mpsc::Receiver<ModelDriverProgress>,
) {
    mpsc::channel(16)
}

async fn run_events(
    request: ModelRequest,
    events: Vec<Result<ModelEvent, ModelError>>,
    kernel: KernelConfig,
) -> Result<ModelResponse, ModelError> {
    let model = ScriptModel::new(descriptor(), vec![Behavior::Events(events)]);
    let driver = model.driver(&kernel);
    let (progress, _receiver) = progress_channel();
    driver
        .run(
            request,
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
        )
        .await
}

struct ScriptModel {
    descriptor: ModelDescriptor,
    behaviors: Mutex<VecDeque<Behavior>>,
    requests: Mutex<Vec<ModelRequest>>,
    starts: AtomicUsize,
    start_notify: Notify,
    completions: AtomicUsize,
    completion_notify: Arc<Notify>,
}

impl ScriptModel {
    fn new(descriptor: ModelDescriptor, behaviors: Vec<Behavior>) -> Arc<Self> {
        Arc::new(Self {
            descriptor,
            behaviors: Mutex::new(behaviors.into()),
            requests: Mutex::new(Vec::new()),
            starts: AtomicUsize::new(0),
            start_notify: Notify::new(),
            completions: AtomicUsize::new(0),
            completion_notify: Arc::new(Notify::new()),
        })
    }

    fn driver(self: &Arc<Self>, kernel: &KernelConfig) -> ModelDriver {
        let model = Arc::clone(self);
        let model: Arc<dyn Model> = model;
        ModelDriver::from_validated(model, self.descriptor.clone(), driver_config(kernel)).unwrap()
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<ModelRequest> {
        lock(&self.requests).clone()
    }

    async fn wait_for_starts(&self, count: usize) {
        loop {
            let notified = self.start_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.starts() >= count {
                return;
            }
            notified.await;
        }
    }

    async fn wait_for_completions(&self, count: usize) {
        loop {
            let notified = self.completion_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.completions.load(Ordering::SeqCst) >= count {
                return;
            }
            notified.await;
        }
    }

    fn take_behavior(&self) -> Behavior {
        lock(&self.behaviors)
            .pop_front()
            .unwrap_or_else(|| Behavior::Events(text_success("default")))
    }
}

impl Model for ScriptModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let behavior = self.take_behavior();
        if let Behavior::RequireDropThenEvents {
            dropped, confirmed, ..
        } = &behavior
        {
            confirmed.store(dropped.dropped(), Ordering::SeqCst);
        }
        lock(&self.requests).push(request);
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.start_notify.notify_waiters();
        match behavior {
            Behavior::Events(events) => {
                Box::pin(async move { Ok(Box::pin(stream::iter(events)) as ModelStream) })
            }
            Behavior::StartError(error) => {
                let completion_notify = Arc::clone(&self.completion_notify);
                let completions = &self.completions;
                Box::pin(async move {
                    completions.fetch_add(1, Ordering::SeqCst);
                    completion_notify.notify_waiters();
                    Err(error)
                })
            }
            Behavior::StartPanic => panic!("scripted start construction panic"),
            Behavior::FuturePanic => Box::pin(async { panic!("scripted start future panic") }),
            Behavior::PendingStart(probe) => Box::pin(PendingStart { probe }),
            Behavior::ProbedStream {
                events,
                terminal,
                probe,
            } => Box::pin(async move {
                Ok(Box::pin(ProbedStream::new(events, terminal, probe)) as ModelStream)
            }),
            Behavior::RequireDropThenEvents { events, .. } => {
                Box::pin(async move { Ok(Box::pin(stream::iter(events)) as ModelStream) })
            }
            Behavior::Barrier(barrier, events) => Box::pin(async move {
                barrier.wait().await;
                Ok(Box::pin(stream::iter(events)) as ModelStream)
            }),
        }
    }
}

enum Behavior {
    Events(Vec<Result<ModelEvent, ModelError>>),
    StartError(ModelError),
    StartPanic,
    FuturePanic,
    PendingStart(Arc<AtomicBool>),
    ProbedStream {
        events: Vec<Result<ModelEvent, ModelError>>,
        terminal: StreamTerminal,
        probe: Arc<StreamProbe>,
    },
    RequireDropThenEvents {
        dropped: Arc<StreamProbe>,
        confirmed: Arc<AtomicBool>,
        events: Vec<Result<ModelEvent, ModelError>>,
    },
    Barrier(Arc<Barrier>, Vec<Result<ModelEvent, ModelError>>),
}

struct PendingStart {
    probe: Arc<AtomicBool>,
}

impl Future for PendingStart {
    type Output = Result<ModelStream, ModelError>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

impl Drop for PendingStart {
    fn drop(&mut self) {
        self.probe.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum StreamTerminal {
    Eof,
    Pending,
    Panic,
}

struct StreamProbe {
    dropped: AtomicBool,
    terminal_reached: AtomicBool,
    terminal_notify: Notify,
}

impl StreamProbe {
    fn shared() -> Arc<Self> {
        Arc::new(Self {
            dropped: AtomicBool::new(false),
            terminal_reached: AtomicBool::new(false),
            terminal_notify: Notify::new(),
        })
    }

    fn dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }

    async fn wait_for_terminal(&self) {
        loop {
            let notified = self.terminal_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.terminal_reached.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

struct ProbedStream {
    events: VecDeque<Result<ModelEvent, ModelError>>,
    terminal: StreamTerminal,
    probe: Arc<StreamProbe>,
}

impl ProbedStream {
    fn new(
        events: Vec<Result<ModelEvent, ModelError>>,
        terminal: StreamTerminal,
        probe: Arc<StreamProbe>,
    ) -> Self {
        Self {
            events: events.into(),
            terminal,
            probe,
        }
    }
}

impl Stream for ProbedStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(Some(event));
        }
        self.probe.terminal_reached.store(true, Ordering::SeqCst);
        self.probe.terminal_notify.notify_waiters();
        match self.terminal {
            StreamTerminal::Eof => Poll::Ready(None),
            StreamTerminal::Pending => Poll::Pending,
            StreamTerminal::Panic => panic!("scripted stream poll panic"),
        }
    }
}

impl Drop for ProbedStream {
    fn drop(&mut self) {
        self.probe.dropped.store(true, Ordering::SeqCst);
    }
}

struct DescriptorPanicModel;

impl Model for DescriptorPanicModel {
    fn descriptor(&self) -> &ModelDescriptor {
        panic!("scripted descriptor panic")
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        panic!("descriptor panic model must not start")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn assert_text(response: &ModelResponse, expected: &str) {
    assert!(matches!(
        response.parts(),
        [AssistantPart::Text(text)] if text == expected
    ));
}
