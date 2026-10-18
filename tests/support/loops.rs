use minicore_runtime::BoundedText;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason,
    ModelRequest, ModelStartFuture, ModelStream, ReasoningPreference,
};
use minicore_runtime::prompt::{DefaultPromptProvider, PromptProvider};
use minicore_runtime::tools::{
    ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision, ToolError,
    ToolExecutionOutcome, ToolFuture, ToolInputAnswerKind, ToolInputRequest, ToolInvocation,
    ToolOutput, ToolPolicy, ToolPolicyError, ToolPolicyFuture, ToolPolicyRequest, ToolProgress,
    ToolSet, ToolSpec,
};
use minicore_runtime::{
    AgentLoop, ExecutionConfig, HistoryItem, LoopJoinError, LoopOptions, LoopOutcome, LoopReport,
    LoopRequest, ToolCallId, UserInput,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub fn reasoning_set() -> BTreeSet<ReasoningPreference> {
    BTreeSet::from([ReasoningPreference::Auto])
}

pub fn scripted_model(rounds: Vec<Round>) -> (Arc<dyn Model>, Arc<Notify>) {
    scripted_model_named("fake/scripted-model", rounds)
}

pub fn scripted_model_named(name: &str, rounds: Vec<Round>) -> (Arc<dyn Model>, Arc<Notify>) {
    scripted_model_full(name, reasoning_set(), rounds)
}

pub fn scripted_model_counted(
    rounds: Vec<Round>,
) -> (Arc<dyn Model>, Arc<AtomicUsize>, Arc<Notify>) {
    let descriptor = ModelDescriptor::new(
        "fake/scripted-model".parse().unwrap(),
        8192,
        reasoning_set(),
        true,
    )
    .unwrap();
    let started = Arc::new(Notify::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(ScriptedModel {
            descriptor,
            rounds: Arc::new(rounds),
            started: Arc::clone(&started),
            attempts: Arc::clone(&attempts),
        }),
        attempts,
        started,
    )
}

pub fn scripted_model_full(
    name: &str,
    reasoning: BTreeSet<ReasoningPreference>,
    rounds: Vec<Round>,
) -> (Arc<dyn Model>, Arc<Notify>) {
    let descriptor = ModelDescriptor::new(name.parse().unwrap(), 8192, reasoning, true).unwrap();
    let started = Arc::new(Notify::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(ScriptedModel {
            descriptor,
            rounds: Arc::new(rounds),
            started: Arc::clone(&started),
            attempts,
        }),
        started,
    )
}

#[derive(Clone)]
pub enum Round {
    Events(Vec<ModelEvent>),
    ReasoningThenText {
        reasoning: &'static str,
        text: &'static str,
    },
    Final {
        text: &'static str,
    },
    Tools {
        calls: Vec<(&'static str, &'static str)>,
    },
    /// Like `Tools` but the stream awaits a Notify before its first event.
    GatedTools {
        calls: Vec<(&'static str, &'static str)>,
        gate: Arc<Notify>,
    },
    /// Like `Final` but the stream awaits a Notify before its first event.
    GatedFinal {
        text: &'static str,
        gate: Arc<Notify>,
    },
    BurstDeltas {
        count: usize,
        chunk: &'static str,
    },
    Hold,
    Error(ModelError),
    StreamError {
        events: Vec<ModelEvent>,
        error: ModelError,
    },
}

pub struct ScriptedModel {
    descriptor: ModelDescriptor,
    rounds: Arc<Vec<Round>>,
    started: Arc<Notify>,
    attempts: Arc<AtomicUsize>,
}

impl Model for ScriptedModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let round = self
            .rounds
            .get(context.request_index as usize)
            .cloned()
            .unwrap_or(Round::Final {
                text: "default final",
            });
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_waiters();
            match round {
                Round::Error(err) => Err(err),
                other => Ok::<ModelStream, ModelError>(Box::pin(round_stream(other))),
            }
        })
    }
}

pub fn round_stream(round: Round) -> ModelStream {
    match round {
        Round::ReasoningThenText { reasoning, text } => round_stream(Round::Events(vec![
            ModelEvent::reasoning_delta(reasoning).unwrap(),
            ModelEvent::text_delta(text).unwrap(),
            ModelEvent::Finish {
                reason: ModelFinishReason::Stop,
            },
        ])),
        Round::Events(events) => Box::pin(futures_util::stream::iter(events.into_iter().map(Ok))),
        Round::Hold => Box::pin(futures_util::stream::pending()),
        Round::Final { text } => Box::pin(futures_util::stream::iter(vec![
            Ok(ModelEvent::text_delta(text).unwrap()),
            Ok(ModelEvent::Finish {
                reason: ModelFinishReason::Stop,
            }),
        ])),
        Round::Tools { calls } => Box::pin(futures_util::stream::iter(
            tool_call_events(calls).into_iter().map(Ok::<_, ModelError>),
        )),
        Round::GatedTools { calls, gate } => gated_stream(tool_call_events(calls), gate),
        Round::GatedFinal { text, gate } => gated_stream(
            vec![
                ModelEvent::text_delta(text).unwrap(),
                ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                },
            ],
            gate,
        ),
        Round::BurstDeltas { count, chunk } => {
            let mut events = Vec::with_capacity(count + 1);
            for _ in 0..count {
                events.push(ModelEvent::text_delta(chunk).unwrap());
            }
            events.push(ModelEvent::Finish {
                reason: ModelFinishReason::Stop,
            });
            Box::pin(futures_util::stream::iter(
                events.into_iter().map(Ok::<_, ModelError>),
            ))
        }
        Round::StreamError { events, error } => {
            let items: Vec<Result<ModelEvent, ModelError>> = events
                .into_iter()
                .map(Ok)
                .chain(std::iter::once(Err(error)))
                .collect();
            Box::pin(futures_util::stream::iter(items))
        }
        Round::Error(_) => unreachable!("Round::Error is handled in Model::start"),
    }
}

pub fn tool_call_events(calls: Vec<(&'static str, &'static str)>) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    for (name, call_id) in calls {
        let call_id: ToolCallId = call_id.parse().unwrap();
        events.push(ModelEvent::ToolCallStart {
            tool_call_id: call_id.clone(),
            tool_name: name.parse().unwrap(),
        });
        events.push(ModelEvent::tool_call_arguments_delta(call_id.clone(), "{}").unwrap());
        events.push(ModelEvent::ToolCallEnd {
            tool_call_id: call_id,
        });
    }
    events.push(ModelEvent::Finish {
        reason: ModelFinishReason::ToolCalls,
    });
    events
}

pub struct GatedSeq {
    events: Vec<ModelEvent>,
    next: usize,
    gate: Option<Arc<Notify>>,
}

/// Emits `events`, awaiting `gate` (once) before the first one.
pub fn gated_stream(events: Vec<ModelEvent>, gate: Arc<Notify>) -> ModelStream {
    Box::pin(futures_util::stream::unfold(
        GatedSeq {
            events,
            next: 0,
            gate: Some(gate),
        },
        |mut state| async move {
            if state.next == 0 {
                if let Some(gate) = state.gate.take() {
                    gate.notified().await;
                }
            }
            if state.next < state.events.len() {
                let event = state.events[state.next].clone();
                state.next += 1;
                Some((Ok::<ModelEvent, ModelError>(event), state))
            } else {
                None
            }
        },
    ))
}

#[derive(Clone)]
pub enum ToolBehavior {
    Succeed,
    Output(String),
    Fail,
    Hold,
    /// Succeeds after a Notify is released (deterministic mid-batch update).
    Gate(Arc<Notify>),
    /// Panics when the tool future is polled (isolated and treated as Failed).
    Panic,
    RequestInput,
    RequestChoiceInput,
    RequestInvalidInput(ToolInputRequest),
    ProgressBurst(usize),
    GatedProgressBurst {
        count: usize,
        entered: Arc<Notify>,
        gate: Arc<Notify>,
    },
}

pub struct RecordingTool {
    pub spec: ToolSpec,
    pub calls: Arc<Mutex<Vec<ToolCallId>>>,
    pub behavior: ToolBehavior,
    pub entered: Option<Arc<Notify>>,
}

impl Tool for RecordingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        let calls = Arc::clone(&self.calls);
        let recorded = invocation.tool_call_id().clone();
        let behavior = self.behavior.clone();
        let entered = self.entered.clone();
        Box::pin(async move {
            calls.lock().unwrap().push(recorded);
            if let Some(entered) = entered {
                entered.notify_waiters();
            }
            match behavior {
                ToolBehavior::Succeed => Ok(ToolExecutionOutcome::Completed(
                    ToolOutput::new("tool result").unwrap(),
                )),
                ToolBehavior::Output(content) => Ok(ToolExecutionOutcome::Completed(
                    ToolOutput::new(content).unwrap(),
                )),
                ToolBehavior::Fail => Err(ToolError::Failed),
                ToolBehavior::Gate(gate) => {
                    gate.notified().await;
                    Ok(ToolExecutionOutcome::Completed(
                        ToolOutput::new("tool result").unwrap(),
                    ))
                }
                ToolBehavior::Panic => panic!("scripted tool execute panic"),
                ToolBehavior::RequestInput => {
                    let request =
                        ToolInputRequest::new("type the answer", vec![], ToolInputAnswerKind::Text)
                            .unwrap();
                    Ok(ToolExecutionOutcome::RequestInput(request))
                }
                ToolBehavior::RequestChoiceInput => {
                    let request = ToolInputRequest::new(
                        "choose the option",
                        vec![
                            BoundedText::new("alpha").unwrap(),
                            BoundedText::new("beta").unwrap(),
                        ],
                        ToolInputAnswerKind::SingleChoice,
                    )
                    .unwrap();
                    Ok(ToolExecutionOutcome::RequestInput(request))
                }
                ToolBehavior::RequestInvalidInput(request) => {
                    Ok(ToolExecutionOutcome::RequestInput(request))
                }
                ToolBehavior::ProgressBurst(count) => {
                    for i in 0..count {
                        let msg = BoundedText::new(format!("progress-{i}")).unwrap();
                        let _ = context.progress.emit(
                            ToolProgress::new(Some(msg), Some(i as u64), Some(count as u64))
                                .unwrap(),
                        );
                    }
                    Ok(ToolExecutionOutcome::Completed(
                        ToolOutput::new("burst completed").unwrap(),
                    ))
                }
                ToolBehavior::GatedProgressBurst {
                    count,
                    entered,
                    gate,
                } => {
                    for i in 0..count {
                        let msg = BoundedText::new(format!("progress-{i}")).unwrap();
                        let _ = context.progress.emit(
                            ToolProgress::new(Some(msg), Some(i as u64), Some(count as u64))
                                .unwrap(),
                        );
                    }
                    entered.notify_one();
                    gate.notified().await;
                    Ok(ToolExecutionOutcome::Completed(
                        ToolOutput::new("gated burst completed").unwrap(),
                    ))
                }
                ToolBehavior::Hold => {
                    context.cancellation.cancelled().await;
                    Err(ToolError::Cancelled)
                }
            }
        })
    }
}

pub fn echo_spec() -> ToolSpec {
    ToolSpec::new(
        "echo".parse().unwrap(),
        "echo tool",
        json!({"type": "object"}),
    )
    .unwrap()
}

pub fn tool_set(calls: Arc<Mutex<Vec<ToolCallId>>>, behavior: ToolBehavior) -> ToolSet {
    tool_set_full("echo", calls, behavior, None)
}

pub fn tool_set_with_entered(
    calls: Arc<Mutex<Vec<ToolCallId>>>,
    behavior: ToolBehavior,
    entered: Option<Arc<Notify>>,
) -> ToolSet {
    tool_set_full("echo", calls, behavior, entered)
}

pub fn tool_set_full(
    name: &str,
    calls: Arc<Mutex<Vec<ToolCallId>>>,
    behavior: ToolBehavior,
    entered: Option<Arc<Notify>>,
) -> ToolSet {
    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: ToolSpec::new(
            name.parse().unwrap(),
            "echo tool",
            json!({"type": "object"}),
        )
        .unwrap(),
        calls,
        behavior,
        entered,
    });
    builder.build().unwrap()
}

#[derive(Clone)]
pub enum PolicyPlan {
    Allow,
    Deny,
    DenyReason(BoundedText),
    RequireApproval,
    Invalid,
    Hold,
    /// Panics when the decision future is polled (fail-closed Denied).
    Panic,
}

pub struct ScriptedPolicy {
    pub plan: Arc<Vec<PolicyPlan>>,
    pub calls: Arc<AtomicUsize>,
}

impl ToolPolicy for ScriptedPolicy {
    fn decide<'a>(&'a self, request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let plan = self.plan.get(index).cloned().unwrap_or(PolicyPlan::Allow);
        Box::pin(async move {
            match plan {
                PolicyPlan::Allow => Ok(ToolDecision::Allow),
                PolicyPlan::Deny => ToolDecision::deny("not today"),
                PolicyPlan::DenyReason(reason) => Ok(ToolDecision::Deny { reason }),
                PolicyPlan::RequireApproval => ToolDecision::require_approval(
                    ApprovalRequest::new("approve this call?", ApprovalRisk::High).unwrap(),
                ),
                PolicyPlan::Invalid => Ok(ToolDecision::RequireApproval {
                    request: ApprovalRequest {
                        prompt: BoundedText::new("").unwrap(),
                        risk: ApprovalRisk::High,
                    },
                }),
                PolicyPlan::Hold => {
                    request.cancellation.cancelled().await;
                    Err(ToolPolicyError::Cancelled)
                }
                PolicyPlan::Panic => panic!("scripted policy decide panic"),
            }
        })
    }
}

pub fn config(
    model: Arc<dyn Model>,
    tools: ToolSet,
    policy: Option<Arc<dyn ToolPolicy>>,
) -> ExecutionConfig {
    config_full(
        model,
        ReasoningPreference::Auto,
        tools,
        policy,
        Arc::new(DefaultPromptProvider::new(None)),
    )
}

pub fn config_full(
    model: Arc<dyn Model>,
    reasoning: ReasoningPreference,
    tools: ToolSet,
    policy: Option<Arc<dyn ToolPolicy>>,
    prompt: Arc<dyn PromptProvider>,
) -> ExecutionConfig {
    ExecutionConfig::new(model, reasoning, tools, policy, prompt)
        .expect("test config must validate")
}

pub fn project() -> Arc<dyn PromptProvider> {
    Arc::new(DefaultPromptProvider::new(None))
}

pub fn request(config: ExecutionConfig) -> LoopRequest {
    LoopRequest::new(
        Arc::from([]),
        UserInput::text("Fix the parser").unwrap(),
        config,
    )
}

pub fn assert_completed_with_text(report: &LoopReport, expected_text: &str) {
    assert_eq!(report.outcome, LoopOutcome::Completed);
    let assistant = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .next_back()
        .expect("completed loop appends an assistant");
    let has_text = assistant
        .content
        .iter()
        .any(|part| matches!(part, minicore_runtime::model::AssistantPart::Text(text) if text.as_ref() == expected_text));
    assert!(has_text, "expected assistant text {expected_text:?}");
}

pub fn spawn_join(
    agent: AgentLoop,
) -> tokio::task::JoinHandle<Result<Arc<LoopReport>, LoopJoinError>> {
    tokio::spawn(agent.join())
}

pub async fn finish(agent: AgentLoop) -> Arc<LoopReport> {
    agent.join().await.unwrap()
}

pub fn tool_results(report: &LoopReport) -> Vec<&minicore_runtime::ToolResultHistory> {
    report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

pub fn start_echo(
    model: Arc<dyn Model>,
    behavior: ToolBehavior,
    options: LoopOptions,
) -> (AgentLoop, Arc<Mutex<Vec<ToolCallId>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentLoop::start(
        request(config(model, tool_set(calls.clone(), behavior), None)),
        options,
    )
    .unwrap();
    (agent, calls)
}
