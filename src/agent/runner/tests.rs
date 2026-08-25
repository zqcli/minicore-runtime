use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::stream;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::run_turn;
use crate::agent::SessionEnvironment;
use crate::agent::runner_protocol::{
    CommittedUpdate, RunnerCommitError, RunnerEvent, RunnerOutcome, RunnerProgress,
    SuspensionError, TurnRunnerExit,
};
use crate::agent::turn_context::{
    TurnRunnerControl, TurnRunnerIdentity, TurnRunnerRequest, TurnRunnerRequestError,
};
use crate::bindings::SessionBindings;
use crate::config::{CompactionConfig, KernelConfig, SemanticLimits, SessionSpec};
use crate::context::{
    ContextBlock, ContextBundle, ContextError, ContextFuture, ContextProvider, ContextRequest,
    ContextSlot,
};
use crate::conversation::{
    AssistantMessageDraft, AssistantMessageEntry, ConversationEntry, ConversationSeq,
    ConversationView, ToolResultDraft, ToolResultEntry, TurnExecutionRecord, UserInputRecord,
    UserMessageEntry,
};
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::{ContextSourceId, SessionId, SessionInstanceId, ToolCallId, TurnId};
use crate::interaction::InteractionAnswer;
use crate::model::{
    DeliveryState, Model, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind,
    ModelEvent, ModelFinishReason, ModelMessage, ModelRef, ModelStartFuture, ModelStream,
    ReasoningPreference, ToolCall, Usage,
};
use crate::time::Timestamp;
use crate::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision,
    ToolExecutionOutcome, ToolFuture, ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest,
    ToolInvocation, ToolOutput, ToolPolicy, ToolPolicyFuture, ToolPolicyRequest, ToolResultOutcome,
    ToolSet, ToolSpec,
};
use crate::value::BoundedText;

#[cfg(test)]
mod acknowledgements;
#[cfg(test)]
mod compaction;
#[cfg(test)]
mod compaction_acknowledgements;
#[cfg(test)]
mod compaction_control;
#[cfg(test)]
mod compaction_support;
#[cfg(test)]
mod control;
#[cfg(test)]
mod deadline_provenance;
#[cfg(test)]
mod interactions;
#[cfg(test)]
mod model_only;
#[cfg(test)]
mod panic;
#[cfg(test)]
mod panic_support;
#[cfg(test)]
mod request_validation;
#[cfg(test)]
mod tools;
#[cfg(test)]
mod usage_errors;

pub(super) use panic_support::{
    next_scripted_turn_id, script_turn_panic, take_scripted_turn_panic,
};

fn timestamp() -> Timestamp {
    "2026-08-19T12:34:56.789Z".parse().unwrap()
}

fn session_id() -> SessionId {
    "ses_00000000000000000000000000000081".parse().unwrap()
}

fn instance_id() -> SessionInstanceId {
    "ins_00000000000000000000000000000081".parse().unwrap()
}

fn turn_id() -> TurnId {
    "trn_00000000000000000000000000000081".parse().unwrap()
}

fn call_id(value: u8) -> ToolCallId {
    format!("call_{value:032}").parse().unwrap()
}

fn model_ref() -> ModelRef {
    "host:runner".parse().unwrap()
}

fn session_spec(tool_names: &[&str], max_tool_rounds: u16) -> SessionSpec {
    SessionSpec::new(
        model_ref(),
        ReasoningPreference::Auto,
        BoundedText::new("session rules").unwrap(),
        tool_names
            .iter()
            .map(|name| name.parse().unwrap())
            .collect(),
        max_tool_rounds,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

fn initial_conversation(spec: &SessionSpec, effective_max_tool_rounds: u16) -> ConversationView {
    let entry = ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(1),
        turn_id: turn_id(),
        input: UserInputRecord::new(BoundedText::new("question").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            spec.model.clone(),
            spec.reasoning,
            effective_max_tool_rounds,
        )
        .unwrap(),
        created_at: timestamp(),
    });
    ConversationView::from_validated_entries(
        spec,
        &SemanticLimits::default(),
        Arc::from(vec![entry]),
    )
    .unwrap()
}

fn pending_tool_conversation(
    spec: &SessionSpec,
    tool_name: &str,
    tool_call_id: ToolCallId,
) -> ConversationView {
    let mut entries = initial_conversation(spec, 4).entries().to_vec();
    entries.push(ConversationEntry::AssistantMessage(AssistantMessageEntry {
        seq: ConversationSeq::new(2),
        turn_id: turn_id(),
        model: spec.model.clone(),
        text: None,
        reasoning: None,
        tool_calls: vec![
            ToolCall::new(tool_call_id, tool_name.parse().unwrap(), json!({}), 0).unwrap(),
        ],
        usage: Usage::default(),
        finish_reason: ModelFinishReason::ToolCalls,
        created_at: timestamp(),
    }));
    ConversationView::from_validated_entries(spec, &SemanticLimits::default(), entries.into())
        .unwrap()
}

fn runner_request(
    spec: SessionSpec,
    effective_max_tool_rounds: u16,
    bindings: SessionBindings,
    conversation: ConversationView,
) -> (
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    mpsc::Receiver<RunnerProgress>,
) {
    request_with_control(
        spec,
        effective_max_tool_rounds,
        bindings,
        conversation,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(30),
        8,
    )
}

fn request_with_control(
    spec: SessionSpec,
    effective_max_tool_rounds: u16,
    bindings: SessionBindings,
    conversation: ConversationView,
    cancellation: CancellationToken,
    deadline: Instant,
    critical_capacity: usize,
) -> (
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    mpsc::Receiver<RunnerProgress>,
) {
    let kernel = KernelConfig::default_checked().unwrap();
    let environment = SessionEnvironment::build(&kernel, &spec, &bindings).unwrap();
    let (critical_tx, critical_rx) = mpsc::channel(critical_capacity);
    let (progress_tx, progress_rx) = mpsc::channel(64);
    let request = TurnRunnerRequest::new(
        TurnRunnerIdentity {
            session_id: session_id(),
            instance_id: instance_id(),
            turn_id: turn_id(),
        },
        environment,
        effective_max_tool_rounds,
        conversation,
        TurnRunnerControl {
            cancellation,
            deadline,
            critical_tx,
            progress_tx,
        },
    )
    .unwrap();
    (request, critical_rx, progress_rx)
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec::new(
        name.parse().unwrap(),
        "tool description",
        json!({"type": "object"}),
    )
    .unwrap()
}

fn session_bindings(
    model: Arc<ScriptModel>,
    context: Option<Arc<ScriptContext>>,
    tools: Vec<Arc<ScriptTool>>,
    policy: Option<Arc<dyn ToolPolicy>>,
) -> SessionBindings {
    let mut builder = ToolSet::builder();
    for tool in tools {
        builder.register_arc(tool as Arc<dyn Tool>);
    }
    SessionBindings::new(
        model,
        builder.build().unwrap(),
        policy,
        context.map(|value| value as Arc<dyn ContextProvider>),
        None,
    )
}

fn ack_assistant(
    conversation: &ConversationView,
    draft: &AssistantMessageDraft,
    spec: &SessionSpec,
) -> CommittedUpdate {
    let previous_head = conversation.head();
    let seq = previous_head.next().unwrap();
    let entry = ConversationEntry::AssistantMessage(AssistantMessageEntry {
        seq,
        turn_id: draft.turn_id,
        model: draft.model.clone(),
        text: draft.text.clone(),
        reasoning: draft.reasoning.clone(),
        tool_calls: draft.tool_calls.clone(),
        usage: draft.usage,
        finish_reason: draft.finish_reason,
        created_at: timestamp(),
    });
    let mut entries = conversation.entries().to_vec();
    entries.push(entry.clone());
    let conversation =
        ConversationView::from_validated_entries(spec, &SemanticLimits::default(), entries.into())
            .unwrap();
    CommittedUpdate {
        previous_head,
        entry,
        conversation,
    }
}

fn ack_tool(
    conversation: &ConversationView,
    draft: &ToolResultDraft,
    spec: &SessionSpec,
) -> CommittedUpdate {
    let previous_head = conversation.head();
    let seq = previous_head.next().unwrap();
    let entry = ConversationEntry::ToolResult(ToolResultEntry {
        seq,
        turn_id: draft.turn_id,
        tool_call_id: draft.tool_call_id.clone(),
        tool_name: draft.tool_name.clone(),
        outcome: draft.outcome,
        content: draft.content.clone(),
        created_at: timestamp(),
    });
    let mut entries = conversation.entries().to_vec();
    entries.push(entry.clone());
    let conversation =
        ConversationView::from_validated_entries(spec, &SemanticLimits::default(), entries.into())
            .unwrap();
    CommittedUpdate {
        previous_head,
        entry,
        conversation,
    }
}

fn final_events(text: &str, usage: Usage) -> Vec<Result<ModelEvent, ModelError>> {
    vec![
        Ok(ModelEvent::text_delta(text).unwrap()),
        Ok(ModelEvent::Usage { usage }),
        Ok(ModelEvent::Finish {
            reason: ModelFinishReason::Stop,
        }),
    ]
}

fn tool_events(calls: &[(u8, &str)], usage: Usage) -> Vec<Result<ModelEvent, ModelError>> {
    let mut events = Vec::new();
    for (id, name) in calls {
        let tool_call_id = call_id(*id);
        events.push(Ok(ModelEvent::ToolCallStart {
            tool_call_id: tool_call_id.clone(),
            tool_name: name.parse().unwrap(),
        }));
        events.push(Ok(ModelEvent::tool_call_arguments_delta(
            tool_call_id.clone(),
            "{}",
        )
        .unwrap()));
        events.push(Ok(ModelEvent::ToolCallEnd { tool_call_id }));
    }
    events.push(Ok(ModelEvent::Usage { usage }));
    events.push(Ok(ModelEvent::Finish {
        reason: ModelFinishReason::ToolCalls,
    }));
    events
}

fn test_model_error(kind: ModelErrorKind) -> ModelError {
    let diagnostic = DiagnosticSummary::new(
        DiagnosticCode::Internal,
        DiagnosticCategory::Model,
        BoundedText::new("test model error").unwrap(),
        false,
    );
    ModelError::permanent(kind, DeliveryState::NotStarted, diagnostic)
}

fn retryable_not_started_model_error(kind: ModelErrorKind) -> ModelError {
    let diagnostic = DiagnosticSummary::new(
        DiagnosticCode::Internal,
        DiagnosticCategory::Model,
        BoundedText::new("test retryable model error").unwrap(),
        true,
    );
    ModelError::not_started(kind, None, diagnostic)
}

enum ModelBehavior {
    Events(Vec<Result<ModelEvent, ModelError>>),
    Error(ModelError),
}

struct ScriptModel {
    descriptor: ModelDescriptor,
    behaviors: Mutex<VecDeque<ModelBehavior>>,
    requests: Mutex<Vec<(crate::model::ModelRequest, ModelCallContext)>>,
    dropped: Option<Arc<AtomicBool>>,
}

impl ScriptModel {
    fn new(context_window: u64, behaviors: Vec<ModelBehavior>) -> Arc<Self> {
        Arc::new(Self {
            descriptor: ModelDescriptor::new(
                model_ref(),
                context_window,
                BTreeSet::from([ReasoningPreference::Auto]),
                true,
            )
            .unwrap(),
            behaviors: Mutex::new(behaviors.into()),
            requests: Mutex::new(Vec::new()),
            dropped: None,
        })
    }

    fn with_drop_probe(
        context_window: u64,
        behaviors: Vec<ModelBehavior>,
        dropped: Arc<AtomicBool>,
    ) -> Arc<Self> {
        let mut model = Self::new(context_window, behaviors);
        Arc::get_mut(&mut model).unwrap().dropped = Some(dropped);
        model
    }

    fn requests(&self) -> Vec<(crate::model::ModelRequest, ModelCallContext)> {
        lock(&self.requests).clone()
    }
}

impl Drop for ScriptModel {
    fn drop(&mut self) {
        if let Some(dropped) = &self.dropped {
            dropped.store(true, Ordering::SeqCst);
        }
    }
}

impl Model for ScriptModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        request: crate::model::ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        lock(&self.requests).push((request, context));
        let behavior = lock(&self.behaviors)
            .pop_front()
            .unwrap_or_else(|| ModelBehavior::Error(test_model_error(ModelErrorKind::Internal)));
        Box::pin(async move {
            match behavior {
                ModelBehavior::Events(events) => {
                    let stream: ModelStream = Box::pin(stream::iter(events));
                    Ok(stream)
                }
                ModelBehavior::Error(error) => Err(error),
            }
        })
    }
}

struct ScriptContext {
    results: Mutex<VecDeque<Result<ContextBundle, ContextError>>>,
    requests: Mutex<Vec<ContextRequest>>,
}

impl ScriptContext {
    fn new(results: Vec<Result<ContextBundle, ContextError>>) -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(results.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<ContextRequest> {
        lock(&self.requests).clone()
    }
}

impl ContextProvider for ScriptContext {
    fn provide<'a>(&'a self, request: ContextRequest) -> ContextFuture<'a> {
        lock(&self.requests).push(request);
        let result = lock(&self.results)
            .pop_front()
            .unwrap_or(Ok(ContextBundle { blocks: Vec::new() }));
        Box::pin(async move { result })
    }
}

enum ToolBehavior {
    Complete(ToolOutput),
    Input(ToolInputRequest),
}

struct ScriptTool {
    spec: ToolSpec,
    behaviors: Mutex<VecDeque<ToolBehavior>>,
    invocations: Mutex<Vec<ToolInvocation>>,
    calls: AtomicUsize,
}

impl ScriptTool {
    fn new(name: &str, behaviors: Vec<ToolBehavior>) -> Arc<Self> {
        Arc::new(Self {
            spec: tool_spec(name),
            behaviors: Mutex::new(behaviors.into()),
            invocations: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Tool for ScriptTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.invocations).push(invocation);
        let behavior = lock(&self.behaviors)
            .pop_front()
            .unwrap_or_else(|| ToolBehavior::Complete(ToolOutput::new("done").unwrap()));
        Box::pin(async move {
            Ok(match behavior {
                ToolBehavior::Complete(output) => ToolExecutionOutcome::Completed(output),
                ToolBehavior::Input(request) => ToolExecutionOutcome::RequestInput(request),
            })
        })
    }
}

struct ScriptPolicy {
    decisions: Mutex<VecDeque<ToolDecision>>,
}

impl ScriptPolicy {
    fn new(decisions: Vec<ToolDecision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions.into()),
        })
    }
}

impl ToolPolicy for ScriptPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        let decision = lock(&self.decisions)
            .pop_front()
            .unwrap_or(ToolDecision::Allow);
        Box::pin(async move { Ok(decision) })
    }
}

fn approval() -> ToolDecision {
    ToolDecision::require_approval(ApprovalRequest::new("approve", ApprovalRisk::Medium).unwrap())
        .unwrap()
}

fn input_request() -> ToolInputRequest {
    ToolInputRequest::new("input", Vec::new(), ToolInputAnswerKind::Text).unwrap()
}

async fn joined_outcome(task: tokio::task::JoinHandle<TurnRunnerExit>) -> RunnerOutcome {
    match task.await.unwrap() {
        TurnRunnerExit::Finished { outcome } => outcome,
        TurnRunnerExit::Panicked => panic!("runner panicked"),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
