pub mod support;

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::stream;
use minicore_runtime::conversation::{ConversationEntry, TurnTerminal};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::{SessionHealth, SessionStatus};
use minicore_runtime::tools::{
    Tool, ToolContext, ToolDecision, ToolError, ToolExecutionOutcome, ToolFuture, ToolInvocation,
    ToolPolicy, ToolPolicyError, ToolPolicyFuture, ToolPolicyRequest, ToolResultOutcome, ToolSet,
    ToolSpec,
};
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, SessionSpec, ToolCallId, TurnOptions, UserInput,
};
use serde_json::json;
use tokio::sync::Semaphore;

use support::fake_session_log::FakeSessionLog;

struct TwoRoundModel {
    descriptor: ModelDescriptor,
    responses: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    calls: Arc<AtomicUsize>,
}

impl Model for TwoRoundModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = lock(&self.responses).pop_front().unwrap();
        Box::pin(async move {
            let stream: ModelStream = Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

enum ToolMode {
    Error,
    Panic,
    Pending { started: Arc<Semaphore> },
}

struct EvidenceTool {
    spec: ToolSpec,
    mode: ToolMode,
    calls: Arc<AtomicUsize>,
}

impl Tool for EvidenceTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.mode {
            ToolMode::Error => Box::pin(async { Err(ToolError::Failed) }),
            ToolMode::Panic => Box::pin(async {
                panic!("scripted tool panic");
            }),
            ToolMode::Pending { started } => {
                let started = Arc::clone(started);
                Box::pin(async move {
                    started.add_permits(1);
                    std::future::pending::<Result<ToolExecutionOutcome, ToolError>>().await
                })
            }
        }
    }
}

struct AllowPolicy;

impl ToolPolicy for AllowPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        Box::pin(async { Ok(ToolDecision::Allow) })
    }
}

enum PolicyMode {
    Error,
    Panic,
    Pending { started: Arc<Semaphore> },
}

struct EvidencePolicy {
    mode: PolicyMode,
}

impl ToolPolicy for EvidencePolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        match &self.mode {
            PolicyMode::Error => Box::pin(async { Err(ToolPolicyError::Failed) }),
            PolicyMode::Panic => Box::pin(async {
                panic!("scripted policy panic");
            }),
            PolicyMode::Pending { started } => {
                let started = Arc::clone(started);
                Box::pin(async move {
                    started.add_permits(1);
                    std::future::pending::<Result<ToolDecision, ToolPolicyError>>().await
                })
            }
        }
    }
}

struct Fixture {
    spec: SessionSpec,
    model: Arc<dyn Model>,
    tools: ToolSet,
    tool_calls: Arc<AtomicUsize>,
}

fn fixture(tool_mode: ToolMode, id: u8) -> Fixture {
    let model_ref: ModelRef = format!("host:tool-policy-{id}").parse().unwrap();
    let tool_name: minicore_runtime::tools::ToolName = "work".parse().unwrap();
    let tool_call_id: ToolCallId = format!("call_{id:032}").parse().unwrap();
    let responses = VecDeque::from([
        vec![
            Ok(ModelEvent::ToolCallStart {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
            }),
            Ok(ModelEvent::tool_call_arguments_delta(tool_call_id.clone(), "{}").unwrap()),
            Ok(ModelEvent::ToolCallEnd { tool_call_id }),
            Ok(ModelEvent::Usage {
                usage: Usage::new(2, 1, 0),
            }),
            Ok(ModelEvent::Finish {
                reason: ModelFinishReason::ToolCalls,
            }),
        ],
        vec![
            Ok(ModelEvent::text_delta("continued").unwrap()),
            Ok(ModelEvent::Usage {
                usage: Usage::new(2, 1, 0),
            }),
            Ok(ModelEvent::Finish {
                reason: ModelFinishReason::Stop,
            }),
        ],
    ]);
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(TwoRoundModel {
        descriptor: ModelDescriptor::new(
            model_ref.clone(),
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
        responses: Mutex::new(responses),
        calls: model_calls,
    });
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(EvidenceTool {
        spec: ToolSpec::new(tool_name.clone(), "work", json!({"type": "object"})).unwrap(),
        mode: tool_mode,
        calls: Arc::clone(&tool_calls),
    });
    let mut builder = ToolSet::builder();
    let registered: Arc<dyn Tool> = tool;
    builder.register_arc(registered);
    let tools = builder.build().unwrap();
    let spec = SessionSpec::new(
        model_ref,
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::from([tool_name]),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    Fixture {
        spec,
        model,
        tools,
        tool_calls,
    }
}

fn options(bindings: SessionBindings) -> SessionRuntimeOptions {
    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.tool_call_timeout = Duration::from_secs(1);
    kernel.policy_timeout = Duration::from_secs(1);
    SessionRuntimeOptions::new(kernel, bindings, tokio::runtime::Handle::current()).unwrap()
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn ordinary_tool_error_is_durable_failed_result_and_session_remains_healthy() {
    run_tool_case(ToolMode::Error, None, 101).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_timeout_and_panic_each_commit_failed_result_and_actor_finishes() {
    let started = Arc::new(Semaphore::new(0));
    run_tool_case(
        ToolMode::Pending {
            started: Arc::clone(&started),
        },
        Some(started),
        102,
    )
    .await;
    run_tool_case(ToolMode::Panic, None, 103).await;
}

async fn run_tool_case(mode: ToolMode, started: Option<Arc<Semaphore>>, id: u8) {
    let fixture = fixture(mode, id);
    let bindings = SessionBindings::new(
        Arc::clone(&fixture.model),
        fixture.tools.clone(),
        Some(Arc::new(AllowPolicy)),
        None,
        None,
    );
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(id),
        fixture.spec.clone(),
        Box::new(log),
        options(bindings),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("tool").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    if let Some(started) = started {
        let permit = started.acquire_owned().await.unwrap();
        permit.forget();
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(turn.wait().await.unwrap().terminal, TurnTerminal::Completed);
    assert_eq!(fixture.tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(handle.state().status, SessionStatus::Idle);
    assert_eq!(handle.state().health, SessionHealth::Healthy);
    assert!(inspection.entries().iter().any(|entry| matches!(
        entry,
        ConversationEntry::ToolResult(result) if result.outcome == ToolResultOutcome::Failed
    )));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn policy_error_timeout_and_panic_fail_closed_and_actor_finishes() {
    run_policy_case(PolicyMode::Error, None, 104).await;
    let started = Arc::new(Semaphore::new(0));
    run_policy_case(
        PolicyMode::Pending {
            started: Arc::clone(&started),
        },
        Some(started),
        105,
    )
    .await;
    run_policy_case(PolicyMode::Panic, None, 106).await;
}

async fn run_policy_case(mode: PolicyMode, started: Option<Arc<Semaphore>>, id: u8) {
    let fixture = fixture(ToolMode::Error, id);
    let policy: Arc<dyn ToolPolicy> = Arc::new(EvidencePolicy { mode });
    let bindings = SessionBindings::new(
        Arc::clone(&fixture.model),
        fixture.tools.clone(),
        Some(policy),
        None,
        None,
    );
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(id),
        fixture.spec.clone(),
        Box::new(log),
        options(bindings),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("policy").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    if let Some(started) = started {
        let permit = started.acquire_owned().await.unwrap();
        permit.forget();
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(turn.wait().await.unwrap().terminal, TurnTerminal::Completed);
    assert_eq!(fixture.tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(handle.state().status, SessionStatus::Idle);
    assert_eq!(handle.state().health, SessionHealth::Healthy);
    assert!(inspection.entries().iter().any(|entry| matches!(
        entry,
        ConversationEntry::ToolResult(result) if result.outcome == ToolResultOutcome::Denied
    )));
    runtime.shutdown().await.unwrap();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
