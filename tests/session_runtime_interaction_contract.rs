pub mod support;

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::stream;
use minicore_runtime::conversation::{ConversationEntry, TurnTerminal};
use minicore_runtime::error::{SessionError, SessionLogErrorKind, TurnWaitError};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::{
    InteractionAnswer, InteractionKind, InteractionResolutionSummary, SessionEvent, SessionStatus,
};
use minicore_runtime::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision,
    ToolExecutionOutcome, ToolFuture, ToolInputAnswer, ToolInvocation, ToolPolicy,
    ToolPolicyFuture, ToolPolicyRequest, ToolResultOutcome, ToolSet, ToolSpec,
};
use minicore_runtime::{
    BoundedText, CompactionConfig, InteractionId, KernelConfig, SessionBindings, SessionId,
    SessionRuntime, SessionRuntimeOptions, SessionSpec, ToolCallId, TurnOptions, UserInput,
};
use serde_json::json;

use support::fake_session_log::{FakeSessionLog, Operation, Script};

struct ScriptModel {
    descriptor: ModelDescriptor,
    responses: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    calls: Arc<AtomicUsize>,
    second_started: Arc<tokio::sync::Semaphore>,
    second_release: Arc<tokio::sync::Semaphore>,
}

impl Model for ScriptModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = lock(&self.responses).pop_front().unwrap();
        let second_started = Arc::clone(&self.second_started);
        let second_release = Arc::clone(&self.second_release);
        Box::pin(async move {
            if call == 1 {
                second_started.add_permits(1);
                let permit = second_release.acquire_owned().await.unwrap();
                permit.forget();
            }
            let stream: ModelStream = Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

struct NeverRunTool {
    spec: ToolSpec,
    calls: AtomicUsize,
}

impl Tool for NeverRunTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(ToolExecutionOutcome::Completed(
                minicore_runtime::tools::ToolOutput::new("unexpected").unwrap(),
            ))
        })
    }
}

struct ApprovalPolicy;

impl ToolPolicy for ApprovalPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        Box::pin(async {
            Ok(ToolDecision::require_approval(
                ApprovalRequest::new("approve search", ApprovalRisk::Medium).unwrap(),
            )
            .unwrap())
        })
    }
}

struct Fixture {
    spec: SessionSpec,
    bindings: SessionBindings,
    tool: Arc<NeverRunTool>,
    second_started: Arc<tokio::sync::Semaphore>,
    second_release: Arc<tokio::sync::Semaphore>,
    model_calls: Arc<AtomicUsize>,
}

fn fixture() -> Fixture {
    let model_ref: ModelRef = "host:interaction".parse().unwrap();
    let tool_name: minicore_runtime::tools::ToolName = "search".parse().unwrap();
    let call_id: ToolCallId = "call_00000000000000000000000000000071".parse().unwrap();
    let first = vec![
        Ok(ModelEvent::ToolCallStart {
            tool_call_id: call_id.clone(),
            tool_name: tool_name.clone(),
        }),
        Ok(ModelEvent::tool_call_arguments_delta(call_id.clone(), "{}").unwrap()),
        Ok(ModelEvent::ToolCallEnd {
            tool_call_id: call_id,
        }),
        Ok(ModelEvent::Usage {
            usage: Usage::new(4, 2, 1),
        }),
        Ok(ModelEvent::Finish {
            reason: ModelFinishReason::ToolCalls,
        }),
    ];
    let second = vec![
        Ok(ModelEvent::text_delta("done").unwrap()),
        Ok(ModelEvent::Usage {
            usage: Usage::new(3, 2, 1),
        }),
        Ok(ModelEvent::Finish {
            reason: ModelFinishReason::Stop,
        }),
    ];
    let second_started = Arc::new(tokio::sync::Semaphore::new(0));
    let second_release = Arc::new(tokio::sync::Semaphore::new(0));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(ScriptModel {
        descriptor: ModelDescriptor::new(
            model_ref.clone(),
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
        responses: Mutex::new(VecDeque::from([first, second])),
        calls: Arc::clone(&model_calls),
        second_started: Arc::clone(&second_started),
        second_release: Arc::clone(&second_release),
    });
    let tool = Arc::new(NeverRunTool {
        spec: ToolSpec::new(tool_name.clone(), "search", json!({"type": "object"})).unwrap(),
        calls: AtomicUsize::new(0),
    });
    let mut builder = ToolSet::builder();
    let registered: Arc<dyn Tool> = tool.clone();
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
        bindings: SessionBindings::new(model, tools, Some(Arc::new(ApprovalPolicy)), None, None),
        tool,
        second_started,
        second_release,
        model_calls,
    }
}

fn options(fixture: &Fixture) -> SessionRuntimeOptions {
    SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        fixture.bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap()
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

async fn wait_for_interaction(
    events: &mut minicore_runtime::SessionEventStream,
) -> minicore_runtime::PendingInteraction {
    loop {
        match events.recv().await.unwrap().event {
            SessionEvent::InteractionRequested { interaction } => return interaction,
            _ => continue,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn interaction_state_precedes_event_and_answers_are_exactly_once() {
    let fixture = fixture();
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(41),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let interaction = wait_for_interaction(&mut events).await;
    assert_eq!(handle.state().status, SessionStatus::WaitingForInput);
    assert_eq!(
        handle.state().pending_interaction,
        Some(interaction.clone())
    );
    assert!(matches!(interaction.kind, InteractionKind::Approval(_)));
    assert!(matches!(
        handle
            .answer(
                InteractionId::new().unwrap(),
                InteractionAnswer::Approval(ApprovalDecision::Deny),
            )
            .await,
        Err(SessionError::InteractionNotFound)
    ));
    assert!(matches!(
        handle
            .answer(
                interaction.interaction_id,
                InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                    BoundedText::new("wrong kind").unwrap(),
                )),
            )
            .await,
        Err(SessionError::InteractionKindMismatch)
    ));
    handle
        .answer(
            interaction.interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::Deny),
        )
        .await
        .unwrap();
    let second_started = Arc::clone(&fixture.second_started)
        .acquire_owned()
        .await
        .unwrap();
    second_started.forget();
    assert_eq!(handle.state().status, SessionStatus::Running);
    assert!(matches!(
        handle
            .answer(
                interaction.interaction_id,
                InteractionAnswer::Approval(ApprovalDecision::Deny),
            )
            .await,
        Err(SessionError::InteractionAlreadyResolved)
    ));
    loop {
        if matches!(
            events.recv().await.unwrap().event,
            SessionEvent::InteractionResolved {
                interaction_id,
                resolution: InteractionResolutionSummary::Denied,
            } if interaction_id == interaction.interaction_id
        ) {
            break;
        }
    }
    fixture.second_release.add_permits(1);
    let outcome = turn.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::Completed);
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
    assert!(inspection.entries().iter().any(|entry| matches!(
        entry,
        ConversationEntry::ToolResult(result) if result.outcome == ToolResultOutcome::Denied
    )));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_while_waiting_settles_missing_tool_result_once() {
    let fixture = fixture();
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(42),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let _interaction = wait_for_interaction(&mut events).await;
    assert!(turn.cancel());
    let outcome = turn.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::CancelledByUser);
    let entries = inspection.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(
                entry,
                ConversationEntry::ToolResult(result)
                    if result.outcome == ToolResultOutcome::Cancelled
            ))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
            .count(),
        1
    );
    let state = handle.state();
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(state.pending_interaction.is_none());
    assert!(state.active_turn.is_none());
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn known_tool_result_commit_failure_stops_before_model_continuation() {
    let fixture = fixture();
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(Script::Continue);
    log.script_append(Script::Error(SessionLogErrorKind::Unavailable));
    log.script_append(Script::Continue);
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(43),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let interaction = wait_for_interaction(&mut events).await;
    handle
        .answer(
            interaction.interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::Deny),
        )
        .await
        .unwrap();
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::DurabilityUnavailable(_))
    ));
    assert_eq!(fixture.model_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        handle.state().health,
        minicore_runtime::SessionHealth::Degraded { .. }
    ));
    assert_eq!(
        inspection
            .operations()
            .iter()
            .filter(|operation| matches!(operation, Operation::Append { .. }))
            .count(),
        3
    );
    assert!(
        !inspection
            .entries()
            .iter()
            .any(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.event, SessionEvent::TurnFinished { .. }));
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_and_answer_race_consumes_interaction_once_without_hanging() {
    for attempt in 0..8 {
        let fixture = fixture();
        let log = FakeSessionLog::new();
        let inspection = log.inspection();
        let mut runtime = SessionRuntime::create(
            session(44 + attempt),
            fixture.spec.clone(),
            Box::new(log),
            options(&fixture),
        )
        .await
        .unwrap();
        let handle = runtime.handle();
        let mut events = runtime.take_events().unwrap();
        let turn = Arc::new(
            handle
                .submit(UserInput::text("question").unwrap(), TurnOptions::default())
                .await
                .unwrap(),
        );
        let interaction = wait_for_interaction(&mut events).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let cancel_turn = Arc::clone(&turn);
        let cancel_barrier = Arc::clone(&barrier);
        let cancel = tokio::spawn(async move {
            cancel_barrier.wait().await;
            cancel_turn.cancel()
        });
        let answer_handle = handle.clone();
        let answer_barrier = Arc::clone(&barrier);
        let answer = tokio::spawn(async move {
            answer_barrier.wait().await;
            answer_handle
                .answer(
                    interaction.interaction_id,
                    InteractionAnswer::Approval(ApprovalDecision::Deny),
                )
                .await
        });
        barrier.wait().await;
        assert!(cancel.await.unwrap());
        assert!(matches!(
            answer.await.unwrap(),
            Ok(())
                | Err(SessionError::InteractionNotFound)
                | Err(SessionError::InteractionAlreadyResolved)
                | Err(SessionError::Closed)
        ));
        let turn = Arc::try_unwrap(turn).unwrap();
        assert_eq!(
            turn.wait().await.unwrap().terminal,
            TurnTerminal::CancelledByUser
        );
        assert!(handle.state().validate().is_ok());
        assert_eq!(handle.state().status, SessionStatus::Idle);
        let entries = inspection.entries();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry, ConversationEntry::ToolResult(_)))
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
                .count(),
            1
        );
        runtime.shutdown().await.unwrap();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
