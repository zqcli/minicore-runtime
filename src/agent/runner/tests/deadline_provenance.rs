use std::future::pending;

use super::*;
use tokio::sync::Notify;

fn request_with_kernel(
    spec: SessionSpec,
    bindings: SessionBindings,
    conversation: ConversationView,
    kernel: KernelConfig,
    turn_after: Duration,
) -> (
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    mpsc::Receiver<RunnerProgress>,
) {
    let (critical_tx, critical_rx) = mpsc::channel(8);
    let (progress_tx, progress_rx) = mpsc::channel(64);
    let environment = SessionEnvironment::build(&kernel, &spec, &bindings).unwrap();
    let request = TurnRunnerRequest::new(
        TurnRunnerIdentity {
            session_id: session_id(),
            instance_id: instance_id(),
            turn_id: turn_id(),
        },
        environment,
        4,
        conversation,
        TurnRunnerControl {
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + turn_after,
            critical_tx,
            progress_tx,
        },
    )
    .unwrap();
    (request, critical_rx, progress_rx)
}

struct PendingContext {
    started: Notify,
}

impl ContextProvider for PendingContext {
    fn provide<'a>(&'a self, _request: ContextRequest) -> ContextFuture<'a> {
        Box::pin(async move {
            self.started.notify_waiters();
            pending::<Result<ContextBundle, ContextError>>().await
        })
    }
}

struct PendingModel {
    descriptor: ModelDescriptor,
    started: Notify,
}

impl PendingModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            descriptor: ModelDescriptor::new(
                model_ref(),
                4_096,
                BTreeSet::from([ReasoningPreference::Auto]),
                false,
            )
            .unwrap(),
            started: Notify::new(),
        })
    }
}

impl Model for PendingModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: crate::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(async move {
            self.started.notify_waiters();
            pending::<Result<ModelStream, ModelError>>().await
        })
    }
}

struct PendingPolicy {
    started: Notify,
}

impl ToolPolicy for PendingPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        Box::pin(async move {
            self.started.notify_waiters();
            pending::<Result<ToolDecision, crate::tools::ToolPolicyError>>().await
        })
    }
}

struct PendingTool {
    spec: ToolSpec,
    started: Notify,
    calls: AtomicUsize,
}

impl PendingTool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            spec: tool_spec("search"),
            started: Notify::new(),
            calls: AtomicUsize::new(0),
        })
    }
}

impl Tool for PendingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.started.notify_waiters();
            pending::<Result<ToolExecutionOutcome, crate::tools::ToolError>>().await
        })
    }
}

fn empty_bindings(model: Arc<dyn Model>) -> SessionBindings {
    SessionBindings::new(model, ToolSet::builder().build().unwrap(), None, None, None)
}

async fn context_deadline_case(turn_after: Duration, port_timeout: Duration) -> RunnerOutcome {
    let context = Arc::new(PendingContext {
        started: Notify::new(),
    });
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let mut bindings = session_bindings(model, None, Vec::new(), None);
    let context_port: Arc<dyn ContextProvider> = context.clone();
    bindings.context = Some(context_port);
    let kernel = KernelConfig {
        context_timeout: port_timeout,
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, mut critical_rx, _progress_rx) =
        request_with_kernel(spec, bindings, initial, kernel, turn_after);
    let notified = context.started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = tokio::spawn(run_turn(request));
    notified.await;
    tokio::time::advance(Duration::from_secs(6)).await;
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_finished(task.await.unwrap());
    outcome
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn context_turn_deadline_is_budget_exceeded() {
    let outcome = context_deadline_case(Duration::from_secs(5), Duration::from_secs(30)).await;
    assert!(matches!(outcome, RunnerOutcome::BudgetExceeded { .. }));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn context_port_timeout_keeps_context_diagnostic() {
    let outcome = context_deadline_case(Duration::from_secs(30), Duration::from_secs(5)).await;
    let diagnostic = outcome.diagnostic().unwrap();
    assert_eq!(diagnostic.code, crate::error::DiagnosticCode::ContextFailed);
    assert_eq!(
        diagnostic.category,
        crate::error::DiagnosticCategory::Context
    );
}

async fn model_deadline_case(turn_after: Duration, port_timeout: Duration) -> RunnerOutcome {
    let model = PendingModel::new();
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let model_port: Arc<dyn Model> = model.clone();
    let kernel = KernelConfig {
        model_call_timeout: port_timeout,
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, mut critical_rx, _progress_rx) = request_with_kernel(
        spec,
        empty_bindings(model_port),
        initial,
        kernel,
        turn_after,
    );
    let notified = model.started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = tokio::spawn(run_turn(request));
    notified.await;
    tokio::time::advance(Duration::from_secs(6)).await;
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_finished(task.await.unwrap());
    outcome
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn model_turn_deadline_is_budget_exceeded() {
    let outcome = model_deadline_case(Duration::from_secs(5), Duration::from_secs(30)).await;
    assert!(matches!(outcome, RunnerOutcome::BudgetExceeded { .. }));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn configured_model_timeout_keeps_model_timeout_diagnostic() {
    let outcome = model_deadline_case(Duration::from_secs(30), Duration::from_secs(5)).await;
    let diagnostic = outcome.diagnostic().unwrap();
    assert_eq!(diagnostic.code, crate::error::DiagnosticCode::ModelTimeout);
    assert_eq!(diagnostic.category, crate::error::DiagnosticCategory::Model);
}

async fn acknowledge_first_assistant(
    critical_rx: &mut mpsc::Receiver<RunnerEvent>,
    conversation: &ConversationView,
    spec: &SessionSpec,
) -> ConversationView {
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let acknowledgement = ack_assistant(conversation, &draft, spec);
            let conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
            conversation
        }
        event => panic!("unexpected event: {event:?}"),
    }
}

async fn finish_after_tool_result(
    critical_rx: &mut mpsc::Receiver<RunnerEvent>,
    mut conversation: ConversationView,
    spec: &SessionSpec,
    expected: ToolResultOutcome,
) {
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitToolResult { draft, reply } => {
            assert_eq!(draft.outcome, expected);
            let acknowledgement = ack_tool(&conversation, &draft, spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            reply
                .send(Ok(ack_assistant(&conversation, &draft, spec)))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::Completed { .. }
        })
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn policy_turn_deadline_has_no_tool_result_commit() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(tool_events(
            &[(41, "search")],
            Usage::new(3, 2, 1),
        ))],
    );
    let tool = ScriptTool::new("search", Vec::new());
    let policy = Arc::new(PendingPolicy {
        started: Notify::new(),
    });
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let policy_port: Arc<dyn ToolPolicy> = policy.clone();
    let bindings = session_bindings(model, None, vec![Arc::clone(&tool)], Some(policy_port));
    let kernel = KernelConfig {
        policy_timeout: Duration::from_secs(30),
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, mut critical_rx, _progress_rx) = request_with_kernel(
        spec.clone(),
        bindings,
        initial.clone(),
        kernel,
        Duration::from_secs(5),
    );
    let notified = policy.started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = tokio::spawn(run_turn(request));
    let _ = acknowledge_first_assistant(&mut critical_rx, &initial, &spec).await;
    notified.await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::BudgetExceeded { .. }
        })
    ));
    assert_finished(task.await.unwrap());
    assert_eq!(tool.calls(), 0);
    assert!(critical_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn configured_policy_timeout_commits_denied_result_and_continues() {
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(42, "search")], Usage::default())),
            ModelBehavior::Events(final_events("done", Usage::default())),
        ],
    );
    let tool = ScriptTool::new("search", Vec::new());
    let policy = Arc::new(PendingPolicy {
        started: Notify::new(),
    });
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let policy_port: Arc<dyn ToolPolicy> = policy.clone();
    let bindings = session_bindings(model, None, vec![tool], Some(policy_port));
    let kernel = KernelConfig {
        policy_timeout: Duration::from_secs(5),
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, mut critical_rx, _progress_rx) = request_with_kernel(
        spec.clone(),
        bindings,
        initial.clone(),
        kernel,
        Duration::from_secs(30),
    );
    let notified = policy.started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = tokio::spawn(run_turn(request));
    let conversation = acknowledge_first_assistant(&mut critical_rx, &initial, &spec).await;
    notified.await;
    tokio::time::advance(Duration::from_secs(6)).await;
    finish_after_tool_result(
        &mut critical_rx,
        conversation,
        &spec,
        ToolResultOutcome::Denied,
    )
    .await;
    assert_finished(task.await.unwrap());
}

fn pending_tool_bindings(model: Arc<ScriptModel>, tool: Arc<PendingTool>) -> SessionBindings {
    let mut builder = ToolSet::builder();
    let registered: Arc<dyn Tool> = tool;
    builder.register_arc(registered);
    SessionBindings::new(
        model,
        builder.build().unwrap(),
        Some(ScriptPolicy::new(vec![ToolDecision::Allow])),
        None,
        None,
    )
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_turn_deadline_has_no_tool_result_commit() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(tool_events(
            &[(43, "search")],
            Usage::new(5, 3, 2),
        ))],
    );
    let tool = PendingTool::new();
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let kernel = KernelConfig {
        tool_call_timeout: Duration::from_secs(30),
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, mut critical_rx, _progress_rx) = request_with_kernel(
        spec.clone(),
        pending_tool_bindings(model, Arc::clone(&tool)),
        initial.clone(),
        kernel,
        Duration::from_secs(5),
    );
    let notified = tool.started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = tokio::spawn(run_turn(request));
    let _ = acknowledge_first_assistant(&mut critical_rx, &initial, &spec).await;
    notified.await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::BudgetExceeded { .. }
        })
    ));
    assert_finished(task.await.unwrap());
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert!(critical_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn configured_tool_timeout_commits_failed_result_and_continues() {
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(44, "search")], Usage::default())),
            ModelBehavior::Events(final_events("done", Usage::default())),
        ],
    );
    let tool = PendingTool::new();
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let kernel = KernelConfig {
        tool_call_timeout: Duration::from_secs(5),
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, mut critical_rx, _progress_rx) = request_with_kernel(
        spec.clone(),
        pending_tool_bindings(model, Arc::clone(&tool)),
        initial.clone(),
        kernel,
        Duration::from_secs(30),
    );
    let notified = tool.started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = tokio::spawn(run_turn(request));
    let conversation = acknowledge_first_assistant(&mut critical_rx, &initial, &spec).await;
    notified.await;
    tokio::time::advance(Duration::from_secs(6)).await;
    finish_after_tool_result(
        &mut critical_rx,
        conversation,
        &spec,
        ToolResultOutcome::Failed,
    )
    .await;
    assert_finished(task.await.unwrap());
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}
