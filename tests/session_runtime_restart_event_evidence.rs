pub mod support;

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::stream;
use minicore_runtime::conversation::{ConversationEntry, TurnTerminal};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::{InteractionAnswer, InteractionKind, SessionEvent, SessionStatus};
use minicore_runtime::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision,
    ToolExecutionOutcome, ToolFuture, ToolInputAnswerKind, ToolInputRequest, ToolInvocation,
    ToolPolicy, ToolPolicyFuture, ToolPolicyRequest, ToolResultOutcome, ToolSet, ToolSpec,
};
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, PendingInteraction, SessionBindings,
    SessionHandle, SessionId, SessionRuntime, SessionRuntimeOptions, SessionSpec, ToolCallId,
    TurnOptions, UserInput,
};
use serde_json::json;

use support::fake_session_log::{FakeSessionLog, Operation};

struct InteractionModel {
    descriptor: ModelDescriptor,
    responses: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
}

impl Model for InteractionModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let events = lock(&self.responses).pop_front().unwrap();
        Box::pin(async move {
            let stream: ModelStream = Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

enum ToolMode {
    Never,
    RequestInput,
}

struct InteractionTool {
    spec: ToolSpec,
    mode: ToolMode,
    calls: Arc<AtomicUsize>,
}

impl Tool for InteractionTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.mode {
            ToolMode::Never => panic!("approval fixture must suspend before Tool execution"),
            ToolMode::RequestInput => Box::pin(async {
                Ok(ToolExecutionOutcome::RequestInput(
                    ToolInputRequest::new("provide input", Vec::new(), ToolInputAnswerKind::Text)
                        .unwrap(),
                ))
            }),
        }
    }
}

struct ApprovalPolicy;

impl ToolPolicy for ApprovalPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        Box::pin(async {
            Ok(ToolDecision::require_approval(
                ApprovalRequest::new("approve", ApprovalRisk::Medium).unwrap(),
            )
            .unwrap())
        })
    }
}

struct AllowPolicy;

impl ToolPolicy for AllowPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        Box::pin(async { Ok(ToolDecision::Allow) })
    }
}

struct Fixture {
    spec: SessionSpec,
    bindings: SessionBindings,
}

fn fixture(mode: ToolMode, approval: bool, call_count: usize, id: u8) -> Fixture {
    let model_ref: ModelRef = format!("host:restart-event-{id}").parse().unwrap();
    let tool_name: minicore_runtime::tools::ToolName = "inspect".parse().unwrap();
    let mut first = Vec::new();
    for index in 0..call_count {
        let call_id: ToolCallId = format!("call_{id:02}{index:030}").parse().unwrap();
        first.push(Ok(ModelEvent::ToolCallStart {
            tool_call_id: call_id.clone(),
            tool_name: tool_name.clone(),
        }));
        first.push(Ok(ModelEvent::tool_call_arguments_delta(
            call_id.clone(),
            "{}",
        )
        .unwrap()));
        first.push(Ok(ModelEvent::ToolCallEnd {
            tool_call_id: call_id,
        }));
    }
    first.extend([
        Ok(ModelEvent::Usage {
            usage: Usage::new(2, 1, 0),
        }),
        Ok(ModelEvent::Finish {
            reason: ModelFinishReason::ToolCalls,
        }),
    ]);
    let final_response = vec![
        Ok(ModelEvent::text_delta("complete").unwrap()),
        Ok(ModelEvent::Usage {
            usage: Usage::new(2, 1, 0),
        }),
        Ok(ModelEvent::Finish {
            reason: ModelFinishReason::Stop,
        }),
    ];
    let model: Arc<dyn Model> = Arc::new(InteractionModel {
        descriptor: ModelDescriptor::new(
            model_ref.clone(),
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
        responses: Mutex::new(VecDeque::from([first, final_response])),
    });
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(InteractionTool {
        spec: ToolSpec::new(tool_name.clone(), "inspect", json!({"type": "object"})).unwrap(),
        mode,
        calls: Arc::clone(&tool_calls),
    });
    let mut builder = ToolSet::builder();
    let registered: Arc<dyn Tool> = tool;
    builder.register_arc(registered);
    let tools = builder.build().unwrap();
    let policy: Arc<dyn ToolPolicy> = if approval {
        Arc::new(ApprovalPolicy)
    } else {
        Arc::new(AllowPolicy)
    };
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
        bindings: SessionBindings::new(model, tools, Some(policy), None, None),
    }
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn options(fixture: &Fixture, event_capacity: usize) -> SessionRuntimeOptions {
    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.event_capacity = event_capacity;
    SessionRuntimeOptions::new(
        kernel,
        fixture.bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap()
}

async fn wait_pending(handle: &SessionHandle) -> PendingInteraction {
    let mut state = handle.watch_state();
    loop {
        let pending = state.borrow().pending_interaction.clone();
        if let Some(interaction) = pending {
            return interaction;
        }
        state.changed().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pending_approval_history_restarts_cancelled_without_restoring_interaction() {
    restart_pending_interaction(fixture(ToolMode::Never, true, 1, 111), 111, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn pending_tool_input_history_restarts_cancelled_without_restoring_interaction() {
    restart_pending_interaction(fixture(ToolMode::RequestInput, false, 1, 112), 112, false).await;
}

async fn restart_pending_interaction(fixture: Fixture, id: u8, approval: bool) {
    let session_id = session(id);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, 8),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("pending").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let interaction = wait_pending(&handle).await;
    assert_eq!(
        matches!(interaction.kind, InteractionKind::Approval(_)),
        approval
    );
    let manifest = inspection.manifest().unwrap();
    let unfinished = inspection.entries();
    assert!(matches!(
        unfinished.as_slice(),
        [
            ConversationEntry::UserMessage(_),
            ConversationEntry::AssistantMessage(_)
        ]
    ));
    runtime.shutdown().await.unwrap();
    assert_eq!(
        turn.wait().await.unwrap().terminal,
        TurnTerminal::CancelledByShutdown
    );

    let restart_log = FakeSessionLog::with_initial(manifest, unfinished).unwrap();
    let restart_inspection = restart_log.inspection();
    let mut loaded = SessionRuntime::load(session_id, Box::new(restart_log), options(&fixture, 8))
        .await
        .unwrap();
    let state = loaded.handle().state();
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(state.pending_interaction.is_none());
    assert!(matches!(
        state.last_terminal,
        Some(ref outcome) if outcome.terminal == TurnTerminal::CancelledByRestart
    ));
    assert!(loaded.take_events().unwrap().try_recv().is_err());
    let entries = restart_inspection.entries();
    assert!(entries.iter().any(|entry| matches!(
        entry,
        ConversationEntry::ToolResult(result) if result.outcome == ToolResultOutcome::Cancelled
    )));
    assert!(matches!(
        entries.last(),
        Some(ConversationEntry::TurnTerminal(entry))
            if entry.terminal == TurnTerminal::CancelledByRestart
    ));
    loaded.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_interaction_and_turn_events_leave_state_wait_and_transcript_authoritative() {
    let fixture = fixture(ToolMode::Never, true, 1, 113);
    let log = FakeSessionLog::new();
    let mut runtime = SessionRuntime::create(
        session(113),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, 1),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("lossy").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let interaction = wait_pending(&handle).await;
    assert_eq!(handle.state().status, SessionStatus::WaitingForInput);
    handle
        .answer(
            interaction.interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::Deny),
        )
        .await
        .unwrap();
    assert_eq!(turn.wait().await.unwrap().terminal, TurnTerminal::Completed);
    let transcript = handle.transcript(None, 16).await.unwrap();
    assert!(matches!(
        transcript.entries.last(),
        Some(ConversationEntry::TurnTerminal(entry))
            if entry.terminal == TurnTerminal::Completed
    ));
    let mut delivered_event_count = 0;
    while let Ok(event) = events.try_recv() {
        delivered_event_count += 1;
        let _dropped_before: u64 = event.dropped_before;
        assert!(!matches!(
            event.event,
            SessionEvent::InteractionRequested { .. } | SessionEvent::TurnFinished { .. }
        ));
    }
    assert!(delivered_event_count > 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_settlement_appends_all_missing_results_and_terminal_atomically() {
    let fixture = fixture(ToolMode::Never, true, 2, 114);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(114),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, 8),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("cancel").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let _interaction = wait_pending(&handle).await;
    assert!(turn.cancel());
    assert_eq!(
        turn.wait().await.unwrap().terminal,
        TurnTerminal::CancelledByUser
    );
    let settlement = inspection
        .operations()
        .into_iter()
        .find_map(|operation| match operation {
            Operation::Append { entries, .. }
                if matches!(entries.last(), Some(ConversationEntry::TurnTerminal(_))) =>
            {
                Some(entries)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(settlement.len(), 3);
    assert!(matches!(
        settlement.as_slice(),
        [
            ConversationEntry::ToolResult(first),
            ConversationEntry::ToolResult(second),
            ConversationEntry::TurnTerminal(_),
        ] if first.outcome == ToolResultOutcome::Cancelled
            && second.outcome == ToolResultOutcome::Cancelled
    ));
    runtime.shutdown().await.unwrap();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
