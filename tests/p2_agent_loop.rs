//! Phase 2 v0.4 contracts: one agent loop runs without session ownership.
//!
//! All synchronization is deterministic (Notify / watch / oneshot); no test
//! relies on sleeping to prove ordering.

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;

use serde_json::json;
use tokio::sync::Notify;

use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason,
    ModelMessage, ModelRequest, ModelStartFuture, ModelStream, ReasoningPreference,
};
use minicore_runtime::prompt::{
    DefaultPromptProvider, PreparedPrompt, PromptError, PromptFuture, PromptProvider, PromptRequest,
};
use minicore_runtime::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision, ToolError,
    ToolExecutionOutcome, ToolFuture, ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest,
    ToolInvocation, ToolOutput, ToolPolicy, ToolPolicyError, ToolPolicyFuture, ToolPolicyRequest,
    ToolResultOutcome, ToolSet, ToolSpec,
};
use minicore_runtime::value::BoundedText;
use minicore_runtime::{
    AgentLoop, AnswerError, CancelReason, ConfigRevision, ExecutionConfig, HistoryItem,
    HistoryView, InteractionAnswer, LoopEvent, LoopOptions, LoopOutcome, LoopReport, LoopRequest,
    LoopStartError, LoopStatus, SteerError, ToolCallId, UpdateError, UserHistory, UserInput,
    UserMessageKind,
};

fn reasoning_set() -> BTreeSet<ReasoningPreference> {
    BTreeSet::from([ReasoningPreference::Auto])
}

fn scripted_model(rounds: Vec<Round>) -> (Arc<dyn Model>, Arc<Notify>) {
    scripted_model_named("fake/scripted-model", rounds)
}

fn scripted_model_named(name: &str, rounds: Vec<Round>) -> (Arc<dyn Model>, Arc<Notify>) {
    scripted_model_full(name, reasoning_set(), rounds)
}

fn scripted_model_full(
    name: &str,
    reasoning: BTreeSet<ReasoningPreference>,
    rounds: Vec<Round>,
) -> (Arc<dyn Model>, Arc<Notify>) {
    let descriptor = ModelDescriptor::new(name.parse().unwrap(), 8192, reasoning, true).unwrap();
    let started = Arc::new(Notify::new());
    (
        Arc::new(ScriptedModel {
            descriptor,
            rounds: Arc::new(rounds),
            started: Arc::clone(&started),
        }),
        started,
    )
}

#[derive(Clone)]
enum Round {
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
    Hold,
}

struct ScriptedModel {
    descriptor: ModelDescriptor,
    rounds: Arc<Vec<Round>>,
    started: Arc<Notify>,
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
            Ok::<ModelStream, ModelError>(Box::pin(round_stream(round)))
        })
    }
}

fn round_stream(round: Round) -> ModelStream {
    match round {
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
    }
}

fn tool_call_events(calls: Vec<(&'static str, &'static str)>) -> Vec<ModelEvent> {
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

struct GatedSeq {
    events: Vec<ModelEvent>,
    next: usize,
    gate: Option<Arc<Notify>>,
}

/// Emits `events`, awaiting `gate` (once) before the first one.
fn gated_stream(events: Vec<ModelEvent>, gate: Arc<Notify>) -> ModelStream {
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
enum ToolBehavior {
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
}

struct RecordingTool {
    spec: ToolSpec,
    calls: Arc<Mutex<Vec<ToolCallId>>>,
    behavior: ToolBehavior,
    entered: Option<Arc<Notify>>,
}

impl RecordingTool {}

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
                ToolBehavior::Hold => {
                    context.cancellation.cancelled().await;
                    Err(ToolError::Cancelled)
                }
            }
        })
    }
}

fn echo_spec() -> ToolSpec {
    ToolSpec::new(
        "echo".parse().unwrap(),
        "echo tool",
        json!({"type": "object"}),
    )
    .unwrap()
}

fn tool_set(calls: Arc<Mutex<Vec<ToolCallId>>>, behavior: ToolBehavior) -> ToolSet {
    tool_set_full("echo", calls, behavior, None)
}

fn tool_set_with_entered(
    calls: Arc<Mutex<Vec<ToolCallId>>>,
    behavior: ToolBehavior,
    entered: Option<Arc<Notify>>,
) -> ToolSet {
    tool_set_full("echo", calls, behavior, entered)
}

fn tool_set_full(
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
enum PolicyPlan {
    Allow,
    Deny,
    DenyReason(BoundedText),
    RequireApproval,
    Hold,
    /// Panics when the decision future is polled (fail-closed Denied).
    Panic,
}

struct ScriptedPolicy {
    plan: Arc<Vec<PolicyPlan>>,
    calls: Arc<AtomicUsize>,
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
                PolicyPlan::Hold => {
                    request.cancellation.cancelled().await;
                    Err(ToolPolicyError::Cancelled)
                }
                PolicyPlan::Panic => panic!("scripted policy decide panic"),
            }
        })
    }
}

struct GatedPrompt {
    entered: Arc<Notify>,
    gate: Option<Arc<Notify>>,
    calls: Arc<AtomicUsize>,
}

impl PromptProvider for GatedPrompt {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
        let entered = Arc::clone(&self.entered);
        let gate = self.gate.clone();
        let is_first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if is_first {
                if let Some(gate) = gate {
                    entered.notify_waiters();
                    gate.notified().await;
                }
            }
            let mut messages = Vec::new();
            for item in request.history.iter() {
                match item {
                    minicore_runtime::HistoryItem::User(user) => {
                        messages.push(ModelMessage::user(user.input.as_text()).unwrap())
                    }
                    minicore_runtime::HistoryItem::Assistant(assistant) => {
                        messages.push(ModelMessage::assistant(assistant.content.clone()).unwrap())
                    }
                    minicore_runtime::HistoryItem::ToolResult(result) => messages.push(
                        ModelMessage::tool_with_outcome(
                            result.call_id.clone(),
                            result.output.clone(),
                            result.outcome,
                        )
                        .unwrap(),
                    ),
                    minicore_runtime::HistoryItem::Summary(summary) => messages.push(
                        ModelMessage::system(format!(
                            "Conversation summary:\n{}",
                            summary.content.as_str()
                        ))
                        .unwrap(),
                    ),
                }
            }
            if messages.is_empty() {
                return Err(PromptError::EmptyPrompt);
            }
            Ok(PreparedPrompt { messages })
        })
    }
}

fn config(
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

fn config_full(
    model: Arc<dyn Model>,
    reasoning: ReasoningPreference,
    tools: ToolSet,
    policy: Option<Arc<dyn ToolPolicy>>,
    prompt: Arc<dyn PromptProvider>,
) -> ExecutionConfig {
    ExecutionConfig::new(model, reasoning, tools, policy, prompt)
        .expect("test config must validate")
}

fn project() -> Arc<dyn PromptProvider> {
    Arc::new(DefaultPromptProvider::new(None))
}

fn request(config: ExecutionConfig) -> LoopRequest {
    LoopRequest::new(
        Arc::from([]),
        UserInput::text("Fix the parser").unwrap(),
        config,
    )
}

fn assert_completed_with_text(report: &LoopReport, expected_text: &str) {
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
        .any(|part| matches!(part, minicore_runtime::model::AssistantPart::Text(text) if text == expected_text));
    assert!(has_text, "expected assistant text {expected_text:?}");
}

#[tokio::test]
async fn text_loop_runs_to_completion() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let joined = agent.join().await.unwrap();
    let waited = handle.wait().await.unwrap();
    assert!(Arc::ptr_eq(&joined, &waited));

    assert_completed_with_text(&joined, "done");
    assert_eq!(joined.requests, 1);
    assert_eq!(joined.tool_rounds, 0);
    assert!(
        joined
            .appended
            .iter()
            .any(|item| matches!(item, HistoryItem::User(_)))
    );
    assert!(handle.is_finished());
    assert_eq!(handle.state().status, LoopStatus::Finished);
}

#[tokio::test]
async fn tool_loop_runs_model_tool_model_to_completion() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);
    assert_eq!(report.tool_rounds, 1);
    assert_eq!(calls.lock().unwrap().len(), 1, "tool executed once");

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].outcome,
        minicore_runtime::tools::ToolResultOutcome::Success
    );
    assert_completed_with_text(&report, "done");
}

#[tokio::test]
async fn sequential_tool_calls_execute_in_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1"), ("echo", "call_2")],
        },
        Round::Final { text: "done" },
    ]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    let guard = calls.lock().unwrap();
    let recorded: Vec<_> = guard.iter().map(ToolCallId::as_str).collect();
    assert_eq!(recorded, vec!["call_1", "call_2"]);
    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
}

#[tokio::test]
async fn max_tool_rounds_ends_with_failed_budget_and_complete_results() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Tools {
            calls: vec![("echo", "call_2")],
        },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.max_tool_rounds = 1;
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        options,
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert!(matches!(
        report.outcome,
        LoopOutcome::Failed(minicore_runtime::LoopFailure {
            kind: minicore_runtime::LoopFailureKind::MaxToolRounds,
            ..
        })
    ));
    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        !tool_results.is_empty(),
        "round-limit calls receive results"
    );
}

#[tokio::test]
async fn cancel_during_model_ends_cancelled_with_report() {
    let (model, started) = scripted_model(vec![Round::Hold]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    started.notified().await;
    assert!(handle.cancel());
    assert!(!handle.cancel(), "second cancel reports no new transition");

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    assert!(handle.is_finished());
}

#[tokio::test]
async fn cancel_during_tool_ends_cancelled_with_results_for_every_call() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(Notify::new());
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "call_1")],
    }]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set_with_entered(
                Arc::clone(&calls),
                ToolBehavior::Hold,
                Some(Arc::clone(&entered)),
            ),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    handle.cancel();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].outcome,
        minicore_runtime::tools::ToolResultOutcome::Cancelled
    );
}

#[tokio::test]
async fn owner_drop_cancels_the_running_loop() {
    let (model, started) = scripted_model(vec![Round::Hold]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();

    started.notified().await;
    drop(agent);

    let report = handle.wait().await.unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(CancelReason::OwnerDropped)
    );
}

#[tokio::test]
async fn shutdown_cancels_and_joins() {
    let (model, started) = scripted_model(vec![Round::Hold]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();

    started.notified().await;
    let report = agent.shutdown().await.unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(CancelReason::Shutdown)
    );
    assert!(handle.is_finished());
}

#[tokio::test]
async fn approved_interaction_resumes_the_loop() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::RequireApproval]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    let pending = states.borrow().pending_interaction.clone().unwrap();
    assert_eq!(handle.state().status, LoopStatus::WaitingForInput);

    handle
        .answer(
            pending.interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
        )
        .unwrap();
    let report = report_task.await.unwrap().unwrap();
    assert_completed_with_text(&report, "done");
}

#[tokio::test]
async fn wrong_interaction_answers_are_rejected_and_loop_stays_waiting() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "call_1")],
    }]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::RequireApproval]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    let pending = states.borrow().pending_interaction.clone().unwrap();

    // Unknown interaction id.
    let wrong_id = minicore_runtime::InteractionId::new().unwrap();
    assert_eq!(
        handle.answer(
            wrong_id,
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce)
        ),
        Err(AnswerError::WrongInteraction)
    );
    // Wrong answer kind for the pending approval.
    assert_eq!(
        handle.answer(
            pending.interaction_id,
            InteractionAnswer::ToolInput(ToolInputAnswer::Text(BoundedText::new("sure").unwrap()))
        ),
        Err(AnswerError::WrongInteraction)
    );
    // No other interaction is pending.
    assert_eq!(
        handle.answer(
            wrong_id,
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce)
        ),
        Err(AnswerError::WrongInteraction)
    );

    handle
        .answer(
            pending.interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
        )
        .unwrap();
    let report = report_task.await.unwrap().unwrap();
    assert!(matches!(report.outcome, LoopOutcome::Completed));
}

#[tokio::test]
async fn cancel_while_waiting_for_input_ends_the_loop() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "call_1")],
    }]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::Hold]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    handle.cancel();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
}

#[tokio::test]
async fn tool_input_interaction_feeds_an_input_provided_result() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::RequestInput),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    let pending = states.borrow().pending_interaction.clone().unwrap();
    handle
        .answer(
            pending.interaction_id,
            InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                BoundedText::new("the answer is 42").unwrap(),
            )),
        )
        .unwrap();

    let report = report_task.await.unwrap().unwrap();
    assert_completed_with_text(&report, "done");
    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].outcome,
        minicore_runtime::tools::ToolResultOutcome::InputProvided
    );
    assert!(tool_results[0].output.content().as_str().contains("42"));
}

#[tokio::test]
async fn multiple_waiters_receive_the_same_report() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let w1 = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait().await.unwrap() }
    });
    let w2 = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait().await.unwrap() }
    });
    let joined = agent.join().await.unwrap();

    let a = w1.await.unwrap();
    let b = w2.await.unwrap();
    assert!(Arc::ptr_eq(&joined, &a));
    assert!(Arc::ptr_eq(&joined, &b));
}

#[tokio::test]
async fn event_stream_is_takeable_once_and_best_effort() {
    let (model, started) = scripted_model(vec![Round::Hold]);
    let mut agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut stream = agent.take_events().unwrap();
    assert_eq!(
        agent.take_events().unwrap_err(),
        minicore_runtime::TakeEventsError::AlreadyTaken
    );
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    started.notified().await;
    handle.cancel();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));

    let mut saw_started = false;
    let mut saw_finished = false;
    while let Some(envelope) = stream.recv().await {
        match envelope.event {
            LoopEvent::Started { .. } => saw_started = true,
            LoopEvent::Finished { .. } => saw_finished = true,
            _ => {}
        }
    }
    assert!(saw_started);
    assert!(saw_finished);
}

#[tokio::test]
async fn closing_the_event_stream_does_not_stop_the_loop() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let mut agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let stream = agent.take_events().unwrap();
    drop(stream);
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
}

#[test]
fn start_outside_a_tokio_runtime_is_rejected() {
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let (model, _) = scripted_model(vec![]);
            let error = AgentLoop::start(
                request(config(model, ToolSet::default(), None)),
                LoopOptions::default_checked().unwrap(),
            )
            .err()
            .unwrap();
            assert_eq!(error, LoopStartError::NoTokioRuntime);
        });
    });
}

#[tokio::test]
async fn denied_tool_calls_produce_denied_results_and_the_loop_continues() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::Deny]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert_completed_with_text(&report, "done");
    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].outcome,
        minicore_runtime::tools::ToolResultOutcome::Denied
    );
    assert_eq!(calls.lock().unwrap().len(), 0, "denied tool never executes");
}

/// Tests that a failed ordinary tool still yields a result and the loop can
/// continue asking the model.
#[tokio::test]
async fn failed_tool_call_is_a_result_not_a_loop_failure() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Fail),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert_completed_with_text(&report, "done");
    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].outcome,
        minicore_runtime::tools::ToolResultOutcome::Failed
    );
}

#[tokio::test]
async fn output_delta_events_are_streamed_inline() {
    let (model, _) = scripted_model(vec![Round::Final {
        text: "hello world",
    }]);
    let mut agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut saw_delta = false;
    while let Some(envelope) = events.recv().await {
        if let LoopEvent::OutputDelta { channel, .. } = envelope.event {
            assert_eq!(channel, minicore_runtime::OutputChannel::Text);
            saw_delta = true;
        }
    }
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert!(saw_delta, "text deltas must be observable before finish");
}

/// Ensures there is no hidden second runner task: the loop completes even when
/// the owner and all handles are gone, and a fresh loop still runs afterwards.
#[tokio::test]
async fn concurrent_loops_finish_independently_without_extra_tasks() {
    let mut join_set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
        let agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            LoopOptions::default_checked().unwrap(),
        )
        .unwrap();
        join_set.spawn(async move { agent.join().await });
    }
    while let Some(result) = join_set.join_next().await {
        let report = result.unwrap().unwrap();
        assert_eq!(report.outcome, LoopOutcome::Completed);
    }
}

// ===== reviewer-fix deterministic behavior tests =====

struct FailingPrompt;

impl PromptProvider for FailingPrompt {
    fn prepare<'a>(&'a self, _request: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async { Err(PromptError::InvalidHistory) })
    }
}

struct HoldingPrompt;

impl PromptProvider for HoldingPrompt {
    fn prepare<'a>(&'a self, _request: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async { std::future::pending::<Result<PreparedPrompt, PromptError>>().await })
    }
}

/// A model that yields text deltas separated by a paused-time gap so tests can
/// interleave reads with the runner's event bursts.
struct TimedDeltaModel {
    descriptor: ModelDescriptor,
    gap: std::time::Duration,
}

impl Model for TimedDeltaModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let gap = self.gap;
        Box::pin(async move {
            let stream: ModelStream =
                Box::pin(futures_util::stream::unfold(0_u8, move |step| async move {
                    match step {
                        0 => Some((Ok(ModelEvent::text_delta("one").unwrap()), 1)),
                        1 => {
                            tokio::time::sleep(gap).await;
                            Some((Ok(ModelEvent::text_delta("two").unwrap()), 2))
                        }
                        2 => Some((
                            Ok(ModelEvent::Finish {
                                reason: ModelFinishReason::Stop,
                            }),
                            3,
                        )),
                        _ => None,
                    }
                }));
            Ok::<ModelStream, ModelError>(stream)
        })
    }
}

#[tokio::test]
async fn late_subscriber_gets_the_same_report_immediately() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let joined = agent.join().await.unwrap();
    let late = handle.wait().await.unwrap();
    assert!(Arc::ptr_eq(&joined, &late));
    assert_eq!(
        handle.state().request_index,
        0,
        "last issued zero-based index"
    );
}

#[tokio::test]
async fn cancel_and_shutdown_after_completion_do_not_reopen_the_loop() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::Finished)
        .await
        .unwrap();
    assert!(handle.is_finished());
    assert!(
        !handle.cancel(),
        "cancel after completion must return false"
    );
    assert_eq!(handle.state().request_index, 0);

    let report = agent.shutdown().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert!(handle.is_finished());
}

#[tokio::test]
async fn owner_drop_after_completion_keeps_the_completed_outcome() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::Finished)
        .await
        .unwrap();
    drop(agent);
    let report = handle.wait().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
}

#[tokio::test]
async fn cancel_while_interaction_is_pending_ends_the_loop() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "call_1")],
    }]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::RequireApproval]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    handle.cancel();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    // The pending interaction was taken by the runner on cancel.
    assert!(
        handle
            .answer(
                minicore_runtime::InteractionId::new().unwrap(),
                InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
            )
            .is_err()
    );
}

#[tokio::test]
async fn prompt_error_is_a_prompt_failure() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let config = ExecutionConfig::new(
        model,
        ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(FailingPrompt),
    )
    .unwrap();
    let agent = AgentLoop::start(request(config), LoopOptions::default_checked().unwrap()).unwrap();
    let report = agent.join().await.unwrap();
    assert!(matches!(
        &report.outcome,
        LoopOutcome::Failed(failure)
            if failure.kind == minicore_runtime::LoopFailureKind::Prompt
    ));
}

#[tokio::test(start_paused = true)]
async fn prompt_timeout_is_a_prompt_failure() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let config = ExecutionConfig::new(
        model,
        ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(HoldingPrompt),
    )
    .unwrap();
    let mut options = LoopOptions::default_checked().unwrap();
    options.prompt_timeout = Duration::from_millis(100);
    let agent = AgentLoop::start(request(config), options).unwrap();
    tokio::time::advance(Duration::from_millis(300)).await;
    let report = agent.join().await.unwrap();
    assert!(matches!(
        &report.outcome,
        LoopOutcome::Failed(failure)
            if failure.kind == minicore_runtime::LoopFailureKind::Prompt
    ));
}

#[tokio::test(start_paused = true)]
async fn loop_deadline_during_a_model_call_cancels() {
    let (model, _) = scripted_model(vec![Round::Hold]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
    let agent =
        AgentLoop::start(request(config(model, ToolSet::default(), None)), options).unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    let report = agent.join().await.unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(CancelReason::Deadline)
    );
}

#[tokio::test(start_paused = true)]
async fn loop_deadline_during_a_tool_call_cancels() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "call_1")],
    }]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Hold),
            None,
        )),
        options,
    )
    .unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    let report = agent.join().await.unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(CancelReason::Deadline)
    );
}

#[tokio::test]
async fn unknown_tool_call_closes_the_loop_with_invalid_response() {
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("ghost", "call_1")],
    }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert!(matches!(
        &report.outcome,
        LoopOutcome::Failed(failure)
            if failure.kind == minicore_runtime::LoopFailureKind::InvalidModelResponse
    ));
}

#[tokio::test(start_paused = true)]
async fn bounded_event_queue_attaches_dropped_before_to_the_next_success() {
    let descriptor = ModelDescriptor::new(
        "fake/timed".parse().unwrap(),
        8192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap();
    let config = ExecutionConfig::new(
        Arc::new(TimedDeltaModel {
            descriptor,
            gap: Duration::from_millis(100),
        }),
        ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(DefaultPromptProvider::new(None)),
    )
    .unwrap();
    let mut options = LoopOptions::default_checked().unwrap();
    options.event_capacity = 1;
    let mut agent = AgentLoop::start(request(config), options).unwrap();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    // Phase 1: let the first delta and its surrounding states flow, then drain
    // one event so the bounded queue has room again.
    tokio::time::advance(Duration::from_millis(1)).await;
    let first = events.recv().await.unwrap();
    assert_eq!(first.dropped_before, 0);

    // Phase 2: the second delta bursts several events; the first delivery
    // after the overflow reports exactly how many were dropped.
    tokio::time::advance(Duration::from_millis(300)).await;
    let second = events.recv().await.unwrap();
    assert!(second.dropped_before > 0, "overflow events must be counted");

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
}

#[tokio::test]
async fn tool_loop_final_state_request_index_is_last_issued() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(
        handle.state().request_index,
        1,
        "last issued zero-based index"
    );
}

#[tokio::test(start_paused = true)]
async fn interaction_wait_deadline_cancels_and_settles_the_pending_call() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "call_1")],
    }]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::RequireApproval]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let mut options = LoopOptions::default_checked().unwrap();
    options.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        options,
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    assert_eq!(handle.state().status, LoopStatus::WaitingForInput);

    // The deadline is a hard bound even while waiting for user input.
    tokio::time::advance(Duration::from_secs(2)).await;

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(CancelReason::Deadline)
    );

    // The pending tool call settled as a Cancelled ToolResult.
    let result = report
        .appended
        .iter()
        .find_map(|item| match item {
            HistoryItem::ToolResult(result) if result.call_id.as_str() == "call_1" => Some(result),
            _ => None,
        })
        .expect("pending call must be settled in the appended history");
    assert_eq!(
        result.outcome,
        minicore_runtime::tools::ToolResultOutcome::Cancelled
    );

    // The interaction slot was cleaned up: answering now fails and the loop
    // is finished.
    assert!(
        handle
            .answer(
                minicore_runtime::InteractionId::new().unwrap(),
                InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
            )
            .is_err()
    );
    assert_eq!(handle.state().status, LoopStatus::Finished);
}

// ===== Phase 3: request-boundary config updates =====

fn model_names(report: &LoopReport) -> Vec<String> {
    report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::Assistant(assistant) => Some(assistant.model.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn recorded_calls(calls: &Mutex<Vec<ToolCallId>>) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| call.as_str().to_string())
        .collect()
}

async fn collect_request_starts(
    events: &mut minicore_runtime::LoopEventStream,
) -> Vec<(
    u32,
    ConfigRevision,
    String,
    minicore_runtime::model::ReasoningPreference,
)> {
    let mut started = Vec::new();
    while let Some(envelope) = events.recv().await {
        if let LoopEvent::RequestStarted {
            request_index,
            config_revision,
            model,
            reasoning,
            ..
        } = envelope.event
        {
            started.push((
                request_index,
                config_revision,
                model.as_str().to_string(),
                reasoning,
            ));
        }
    }
    started
}

/// MC4-027: an update accepted while a model request is in flight reaches the
/// next request.
#[tokio::test]
async fn update_during_model_reaches_the_next_request() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedTools {
            calls: vec![("echo", "call_1")],
            gate: Arc::clone(&gate),
        }],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "from b" }]);
    let mut agent = AgentLoop::start(
        request(config(
            model_a,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();

    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            project(),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);
    assert_eq!(report.final_config_revision, ConfigRevision::new(1));
    assert_eq!(model_names(&report), ["fake/a", "fake/b"]);
    assert_eq!(recorded_calls(&calls), ["call_1"]);

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![
            (
                0,
                ConfigRevision::INITIAL,
                "fake/a".to_string(),
                ReasoningPreference::Auto
            ),
            (
                1,
                ConfigRevision::new(1),
                "fake/b".to_string(),
                ReasoningPreference::Auto
            ),
        ]
    );
}

/// MC4-028: the tool batch produced by a request keeps that request's snapshot
/// even when an update lands while the batch is running.
#[tokio::test]
async fn update_during_tools_keeps_the_batch_on_the_old_snapshot() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "from b" }]);
    let agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            project(),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);
    assert_eq!(recorded_calls(&calls_a), ["call_1"]);
    assert_eq!(report.final_config_revision, ConfigRevision::new(1));
    assert_eq!(model_names(&report), ["fake/a", "fake/b"]);
}

/// MC4-029: several updates before one boundary hand out monotonic revisions
/// but only the latest config is applied.
#[tokio::test]
async fn multiple_updates_keep_only_the_latest() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedTools {
            calls: vec![("echo", "call_1")],
            gate: Arc::clone(&gate),
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "b" }]);
    let (model_c, _) = scripted_model_named("fake/c", vec![Round::Final { text: "c" }]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = AgentLoop::start(
        request(config(
            model_a,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();

    assert_eq!(
        handle
            .update(config_full(
                model_b,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                project(),
            ))
            .unwrap(),
        ConfigRevision::new(1)
    );
    assert_eq!(
        handle
            .update(config_full(
                model_c,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                project(),
            ))
            .unwrap(),
        ConfigRevision::new(2)
    );

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.requests, 2);
    assert_eq!(report.final_config_revision, ConfigRevision::new(2));
    assert_eq!(model_names(&report), ["fake/a", "fake/c"]);

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![
            (
                0,
                ConfigRevision::INITIAL,
                "fake/a".to_string(),
                ReasoningPreference::Auto
            ),
            (
                1,
                ConfigRevision::new(2),
                "fake/c".to_string(),
                ReasoningPreference::Auto
            ),
        ]
    );
}

/// Spec 15.1: an update arriving while the prompt is prepared discards the
/// stale prompt, rebuilds with the latest config, and never advances the
/// request index or issues the stale model request.
#[tokio::test]
async fn update_during_prompt_prep_is_rebuilt_without_advancing_the_index() {
    let entered = Arc::new(Notify::new());
    let prompt_gate = Arc::new(Notify::new());
    let prompt_a_calls = Arc::new(AtomicUsize::new(0));
    let prompt_b_calls = Arc::new(AtomicUsize::new(0));
    let (model_a, _) = scripted_model_named("fake/a", vec![Round::Final { text: "a" }]);
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "b" }]);

    let prompt_a = Arc::new(GatedPrompt {
        entered: Arc::clone(&entered),
        gate: Some(Arc::clone(&prompt_gate)),
        calls: Arc::clone(&prompt_a_calls),
    });
    let prompt_b = Arc::new(GatedPrompt {
        entered: Arc::new(Notify::new()),
        gate: None,
        calls: Arc::clone(&prompt_b_calls),
    });

    let mut agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            prompt_a,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    assert_eq!(prompt_a_calls.load(Ordering::SeqCst), 1);

    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            prompt_b,
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    prompt_gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_completed_with_text(&report, "b");
    assert_eq!(
        report.requests, 1,
        "the stale prompt never became a request"
    );
    assert_eq!(report.final_config_revision, ConfigRevision::new(1));
    assert_eq!(prompt_a_calls.load(Ordering::SeqCst), 1);
    assert_eq!(prompt_b_calls.load(Ordering::SeqCst), 1);

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![(
            0,
            ConfigRevision::new(1),
            "fake/b".to_string(),
            ReasoningPreference::Auto
        )]
    );
}

/// MC4-031: updating a completed loop fails with NotActive.
#[tokio::test]
async fn update_after_completion_returns_not_active() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);

    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "x" }]);
    let error = handle
        .update(config(model_b, ToolSet::default(), None))
        .expect_err("update after seal must fail");
    assert_eq!(error, UpdateError::NotActive);
}

/// MC4-032: a config update alone never keeps the loop alive; a final request
/// still completes and the accepted update is simply not applied.
#[tokio::test]
async fn update_alone_does_not_extend_the_final_request() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedFinal {
            text: "final",
            gate: Arc::clone(&gate),
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "b" }]);
    let agent = AgentLoop::start(
        request(config(model_a, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();
    let revision = handle
        .update(config(model_b, ToolSet::default(), None))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 1);
    assert_eq!(
        report.final_config_revision,
        ConfigRevision::INITIAL,
        "an applied-but-never-issued update must not leak into the final revision"
    );
}

/// MC4-033: the old response can only use the old ToolSet; the next request
/// runs with the new ToolSet.
#[tokio::test]
async fn toolset_switch_applies_to_the_next_request() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let calls_b = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_named(
        "fake/b",
        vec![
            Round::Final {
                text: "placeholder",
            },
            Round::Tools {
                calls: vec![("echo2", "call_2")],
            },
        ],
    );
    let agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            tool_set_full("echo2", Arc::clone(&calls_b), ToolBehavior::Succeed, None),
            None,
            project(),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.final_config_revision, ConfigRevision::new(1));
    assert_eq!(recorded_calls(&calls_a), ["call_1"]);
    assert_eq!(recorded_calls(&calls_b), ["call_2"]);
    assert_eq!(model_names(&report), ["fake/a", "fake/b", "fake/b"]);
}

/// MC4-034: the next request is prepared by the new PromptProvider.
#[tokio::test]
async fn prompt_provider_switch_applies_to_the_next_request() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let prompt_a_calls = Arc::new(AtomicUsize::new(0));
    let prompt_b_calls = Arc::new(AtomicUsize::new(0));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "from b" }]);

    let prompt_a = Arc::new(GatedPrompt {
        entered: Arc::clone(&entered),
        gate: None,
        calls: Arc::clone(&prompt_a_calls),
    });
    let prompt_b = Arc::new(GatedPrompt {
        entered: Arc::new(Notify::new()),
        gate: None,
        calls: Arc::clone(&prompt_b_calls),
    });

    let agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            prompt_a,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            prompt_b,
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);
    assert_eq!(prompt_a_calls.load(Ordering::SeqCst), 1);
    assert_eq!(prompt_b_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model_names(&report), ["fake/a", "fake/b"]);
}

/// RequestStarted must record each request's actual snapshot: revision, model,
/// and reasoning preference across an update.
#[tokio::test]
async fn request_started_records_the_actual_snapshot_revision_and_reasoning() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_full(
        "fake/a",
        BTreeSet::from([ReasoningPreference::Low]),
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_full(
        "fake/b",
        BTreeSet::from([ReasoningPreference::High]),
        vec![Round::Final { text: "from b" }],
    );
    let mut agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Low,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::High,
            ToolSet::default(),
            None,
            project(),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    report_task.await.unwrap().unwrap();

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![
            (
                0,
                ConfigRevision::INITIAL,
                "fake/a".to_string(),
                ReasoningPreference::Low
            ),
            (
                1,
                ConfigRevision::new(1),
                "fake/b".to_string(),
                ReasoningPreference::High
            ),
        ]
    );
}

/// An invalid config against the live loop limits is rejected without
/// consuming a revision.
#[tokio::test]
async fn invalid_config_update_does_not_consume_a_revision() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedFinal {
            text: "a",
            gate: Arc::clone(&gate),
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "b" }]);

    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_schema_bytes = 16;

    let agent =
        AgentLoop::start(request(config(model_a, ToolSet::default(), None)), options).unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();

    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: echo_spec(),
        calls: Arc::new(Mutex::new(Vec::new())),
        behavior: ToolBehavior::Succeed,
        entered: None,
    });
    let oversized = builder.build().unwrap();
    let error = handle
        .update(config_full(
            model_b.clone(),
            ReasoningPreference::Auto,
            oversized,
            None,
            project(),
        ))
        .expect_err("oversized tool spec must be rejected");
    assert_eq!(error, UpdateError::InvalidConfig);

    // The rejected update consumed no revision: the next accepted one is 1.
    let good = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            project(),
        ))
        .unwrap();
    assert_eq!(good, ConfigRevision::new(1));

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.final_config_revision, ConfigRevision::INITIAL);
}

/// Spec 13.2/33: an update accepted while waiting for input takes effect on
/// the request after the interaction is resolved.
#[tokio::test]
async fn update_during_interaction_waits_takes_effect_after_the_interaction() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "from b" }]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::RequireApproval]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    assert_eq!(handle.state().status, LoopStatus::WaitingForInput);

    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            project(),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    let pending = states.borrow().pending_interaction.clone().unwrap();
    handle
        .answer(
            pending.interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
        )
        .unwrap();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(recorded_calls(&calls), ["call_1"]);
    assert_eq!(report.final_config_revision, ConfigRevision::new(1));
    assert_eq!(model_names(&report), ["fake/a", "fake/b"]);
}

// ===== Phase 3 reviewer fixes: issued-vs-candidate revisions, shared limits =====

/// An update taken at a request boundary whose prompt then errors must not
/// advance the issued revision: no RequestStarted, final stays at the last
/// truly issued revision.
#[tokio::test]
async fn failed_prompt_after_taken_update_keeps_the_last_issued_revision() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "b" }]);
    let mut agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(FailingPrompt),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert!(matches!(
        &report.outcome,
        LoopOutcome::Failed(failure)
            if failure.kind == minicore_runtime::LoopFailureKind::Prompt
    ));
    assert_eq!(report.requests, 1);
    assert_eq!(
        report.final_config_revision,
        ConfigRevision::INITIAL,
        "a taken-but-never-issued revision must not become the final revision"
    );

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![(
            0,
            ConfigRevision::INITIAL,
            "fake/a".to_string(),
            ReasoningPreference::Auto
        )],
        "revision 1 must never appear in RequestStarted"
    );
}

/// Same as above for a prompt timeout: the taken update is not issued and the
/// final revision stays at the last genuinely issued one.
#[tokio::test(start_paused = true)]
async fn prompt_timeout_after_taken_update_keeps_the_last_issued_revision() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "b" }]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.prompt_timeout = Duration::from_millis(100);
    let mut agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            project(),
        )),
        options,
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(HoldingPrompt),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));

    gate.notify_waiters();
    tokio::time::advance(Duration::from_millis(300)).await;

    let report = report_task.await.unwrap().unwrap();
    assert!(matches!(
        &report.outcome,
        LoopOutcome::Failed(failure)
            if failure.kind == minicore_runtime::LoopFailureKind::Prompt
    ));
    assert_eq!(report.requests, 1);
    assert_eq!(report.final_config_revision, ConfigRevision::INITIAL);

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![(
            0,
            ConfigRevision::INITIAL,
            "fake/a".to_string(),
            ReasoningPreference::Auto
        )]
    );
}

/// Several stale rebuilds under a flood of updates keep the latest candidate
/// revision, advance no request index, and finally issue exactly one request
/// with the newest config.
#[tokio::test]
async fn multiple_stale_rebuilds_issue_only_the_latest_revision() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "b" }]);
    let (model_c, _) = scripted_model_named("fake/c", vec![Round::Final { text: "c" }]);
    let (model_d, _) = scripted_model_named("fake/d", vec![Round::Final { text: "d" }]);

    let b_entered = Arc::new(Notify::new());
    let b_gate = Arc::new(Notify::new());
    let c_entered = Arc::new(Notify::new());
    let c_gate = Arc::new(Notify::new());
    let prompt_b = Arc::new(GatedPrompt {
        entered: Arc::clone(&b_entered),
        gate: Some(Arc::clone(&b_gate)),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let prompt_c = Arc::new(GatedPrompt {
        entered: Arc::clone(&c_entered),
        gate: Some(Arc::clone(&c_gate)),
        calls: Arc::new(AtomicUsize::new(0)),
    });

    let mut agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    assert_eq!(
        handle
            .update(config_full(
                model_b,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                prompt_b,
            ))
            .unwrap(),
        ConfigRevision::new(1)
    );
    gate.notify_waiters();

    // First rebuild: B's prompt is prepared, then superseded by C.
    b_entered.notified().await;
    assert_eq!(
        handle
            .update(config_full(
                model_c,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                prompt_c,
            ))
            .unwrap(),
        ConfigRevision::new(2)
    );
    b_gate.notify_waiters();

    // Second rebuild: C's prompt is prepared, then superseded by D.
    c_entered.notified().await;
    assert_eq!(
        handle
            .update(config_full(
                model_d,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                project(),
            ))
            .unwrap(),
        ConfigRevision::new(3)
    );
    c_gate.notify_waiters();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2, "stale rebuilds must not issue requests");
    assert_eq!(report.final_config_revision, ConfigRevision::new(3));

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![
            (
                0,
                ConfigRevision::INITIAL,
                "fake/a".to_string(),
                ReasoningPreference::Auto
            ),
            (
                1,
                ConfigRevision::new(3),
                "fake/d".to_string(),
                ReasoningPreference::Auto
            ),
        ],
        "only the latest candidate revision is ever issued"
    );
}

/// `AgentLoop::start` applies the same config-vs-LoopLimits validation as
/// `update`: tool name/schema overruns fail with InvalidConfig.
#[tokio::test]
async fn start_rejects_config_outside_loop_limits() {
    let (model, _) = scripted_model(vec![Round::Final { text: "x" }]);

    // Schema bytes exceed the loop budget.
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_schema_bytes = 16;
    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: echo_spec(),
        calls: Arc::new(Mutex::new(Vec::new())),
        behavior: ToolBehavior::Succeed,
        entered: None,
    });
    let tools = builder.build().unwrap();
    let error = AgentLoop::start(
        request(config_full(
            model.clone(),
            ReasoningPreference::Auto,
            tools,
            None,
            project(),
        )),
        options,
    )
    .err()
    .expect("oversized schema must fail start");
    assert_eq!(error, LoopStartError::InvalidConfig);

    // Tool name exceeds the loop budget.
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_name_bytes = 2;
    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: echo_spec(),
        calls: Arc::new(Mutex::new(Vec::new())),
        behavior: ToolBehavior::Succeed,
        entered: None,
    });
    let tools = builder.build().unwrap();
    let error = AgentLoop::start(
        request(config_full(
            model,
            ReasoningPreference::Auto,
            tools,
            None,
            project(),
        )),
        options,
    )
    .err()
    .expect("oversized name must fail start");
    assert_eq!(error, LoopStartError::InvalidConfig);
}

// ===== Phase 4: steer active loops at request boundaries =====

fn steerings(report: &LoopReport) -> Vec<String> {
    report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::User(user) if user.kind == UserMessageKind::Steering => {
                Some(user.input.as_text().to_string())
            }
            _ => None,
        })
        .collect()
}

/// MC4-019: a steer accepted while a model request is in flight reaches the
/// next request.
#[tokio::test]
async fn steer_during_model_reaches_the_next_request() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedTools {
            calls: vec![("echo", "call_1")],
            gate: Arc::clone(&gate),
        }],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentLoop::start(
        request(config(
            model_a,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();
    handle.steer(UserInput::text("turn left").unwrap()).unwrap();

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2, "the in-flight request still ran");
    assert_eq!(steerings(&report), ["turn left"]);
    let user_seq: Vec<(UserMessageKind, String)> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::User(user) => Some((user.kind, user.input.as_text().to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_seq,
        vec![
            (UserMessageKind::Prompt, "Fix the parser".to_string()),
            (UserMessageKind::Steering, "turn left".to_string()),
        ]
    );
}

/// MC4-020: a steer accepted while the tool batch is running does not disturb
/// the batch; the next request sees it after the whole batch finished.
#[tokio::test]
async fn steer_during_tool_batch_reaches_the_next_request() {
    let gate = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let calls_a = Arc::new(Mutex::new(Vec::new()));
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::Tools {
            calls: vec![("echo", "call_1")],
        }],
    );
    let agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            tool_set_full(
                "echo",
                Arc::clone(&calls_a),
                ToolBehavior::Gate(Arc::clone(&gate)),
                Some(Arc::clone(&entered)),
            ),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    handle
        .steer(UserInput::text("keep going").unwrap())
        .unwrap();

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(recorded_calls(&calls_a), ["call_1"]);
    assert_eq!(steerings(&report), ["keep going"]);
    let marks: Vec<&'static str> = report
        .appended
        .iter()
        .map(|item| match item {
            HistoryItem::User(user) if user.kind == UserMessageKind::Steering => "steering",
            HistoryItem::User(_) => "prompt",
            HistoryItem::Assistant(_) => "assistant",
            HistoryItem::ToolResult(_) => "toolresult",
            HistoryItem::Summary(_) => "summary",
        })
        .collect();
    assert_eq!(
        marks,
        ["prompt", "assistant", "toolresult", "steering", "assistant"],
        "the batch finished before the steer is applied"
    );
}

/// MC4-021: a steer accepted while the prompt is being prepared discards the
/// stale PreparedPrompt; the rebuilt request sees the steer at index 0.
#[tokio::test]
async fn steer_during_prompt_discards_the_stale_prompt() {
    let entered = Arc::new(Notify::new());
    let prompt_gate = Arc::new(Notify::new());
    let prompt_calls = Arc::new(AtomicUsize::new(0));
    let (model_a, _) = scripted_model_named("fake/a", vec![Round::Final { text: "a" }]);
    let prompt = Arc::new(GatedPrompt {
        entered: Arc::clone(&entered),
        gate: Some(Arc::clone(&prompt_gate)),
        calls: Arc::clone(&prompt_calls),
    });
    let mut agent = AgentLoop::start(
        request(config_full(
            model_a,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            prompt,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.notified().await;
    handle.steer(UserInput::text("ping").unwrap()).unwrap();
    prompt_gate.notify_waiters();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(
        report.requests, 1,
        "the stale prompt never became a request"
    );
    assert_eq!(steerings(&report), ["ping"]);

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![(
            0,
            ConfigRevision::INITIAL,
            "fake/a".to_string(),
            ReasoningPreference::Auto
        )],
        "the rebuilt request is still index 0 and saw the steer"
    );
}

/// MC4-022: multiple steers apply in accept order at the next boundary.
#[tokio::test]
async fn multiple_steers_apply_in_accept_order() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedTools {
            calls: vec![("echo", "call_1")],
            gate: Arc::clone(&gate),
        }],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = AgentLoop::start(
        request(config(
            model_a,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();
    handle.steer(UserInput::text("one").unwrap()).unwrap();
    handle.steer(UserInput::text("two").unwrap()).unwrap();
    handle.steer(UserInput::text("three").unwrap()).unwrap();

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(steerings(&report), ["one", "two", "three"]);
}

/// MC4-023: the steer queue is bounded; accepted steers never drop and the
/// overflow reports QueueFull.
#[tokio::test]
async fn steer_queue_is_bounded_and_full_reports_queue_full() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedTools {
            calls: vec![("echo", "call_1")],
            gate: Arc::clone(&gate),
        }],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut options = LoopOptions::default_checked().unwrap();
    options.max_pending_steers = 2;
    let agent = AgentLoop::start(
        request(config(
            model_a,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        options,
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();
    handle.steer(UserInput::text("s1").unwrap()).unwrap();
    handle.steer(UserInput::text("s2").unwrap()).unwrap();
    assert_eq!(
        handle.steer(UserInput::text("s3").unwrap()),
        Err(SteerError::QueueFull)
    );

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(
        steerings(&report),
        ["s1", "s2"],
        "accepted steers are never lost"
    );
}

/// MC4-024: a steer accepted before the final seal keeps the loop alive and
/// becomes the next request.
#[tokio::test]
async fn final_race_steer_wins_and_keeps_the_loop_alive() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedFinal {
            text: "first",
            gate: Arc::clone(&gate),
        }],
    );
    let agent = AgentLoop::start(
        request(config(model_a, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();
    handle.steer(UserInput::text("hello").unwrap()).unwrap();

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2, "the steer kept the final loop alive");
    assert_eq!(steerings(&report), ["hello"]);
    let assistants = report
        .appended
        .iter()
        .filter(|item| matches!(item, HistoryItem::Assistant(_)))
        .count();
    assert_eq!(assistants, 2);
}

/// MC4-025: once the seal wins, steering a completed loop is NotActive and no
/// steer enters the report.
#[tokio::test]
async fn final_race_seal_wins_returns_not_active() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(steerings(&report), Vec::<String>::new());

    let error = handle
        .steer(UserInput::text("late").unwrap())
        .expect_err("steer after seal must fail");
    assert_eq!(error, SteerError::NotActive);
}

/// MC4-026: steering while an interaction is pending is rejected as
/// WaitingForInput and accepted again once the interaction resolves.
#[tokio::test]
async fn steer_is_rejected_while_waiting_for_interaction() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "call_1")],
    }]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::RequireApproval]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    assert_eq!(
        handle.steer(UserInput::text("nudge").unwrap()),
        Err(SteerError::WaitingForInput)
    );

    let pending = states.borrow().pending_interaction.clone().unwrap();
    handle
        .answer(
            pending.interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
        )
        .unwrap();
    handle
        .steer(UserInput::text("after answer").unwrap())
        .unwrap();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(steerings(&report), ["after answer"]);
}

/// Update and steer accepted before one boundary combine on the next request:
/// the new config serves it and the steer appears in its history.
#[tokio::test]
async fn update_and_steer_combine_on_the_next_request() {
    let gate = Arc::new(Notify::new());
    let (model_a, _) = scripted_model_named(
        "fake/a",
        vec![Round::GatedTools {
            calls: vec![("echo", "call_1")],
            gate: Arc::clone(&gate),
        }],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model_b, _) = scripted_model_named("fake/b", vec![Round::Final { text: "from b" }]);
    let mut agent = AgentLoop::start(
        request(config(
            model_a,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();
    let revision = handle
        .update(config_full(
            model_b,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            project(),
        ))
        .unwrap();
    assert_eq!(revision, ConfigRevision::new(1));
    handle
        .steer(UserInput::text("next topic").unwrap())
        .unwrap();

    gate.notify_waiters();
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.final_config_revision, ConfigRevision::new(1));
    assert_eq!(steerings(&report), ["next topic"]);
    assert_eq!(model_names(&report), ["fake/a", "fake/b"]);

    let started = collect_request_starts(&mut events).await;
    assert_eq!(
        started,
        vec![
            (
                0,
                ConfigRevision::INITIAL,
                "fake/a".to_string(),
                ReasoningPreference::Auto
            ),
            (
                1,
                ConfigRevision::new(1),
                "fake/b".to_string(),
                ReasoningPreference::Auto
            ),
        ]
    );
}

/// A steer accepted but never applied before cancellation must not enter the
/// report.
#[tokio::test]
async fn steer_accepted_but_cancelled_before_application_stays_out_of_report() {
    let (model_a, _) = scripted_model_named("fake/a", vec![Round::Hold]);
    let agent = AgentLoop::start(
        request(config(model_a, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.status == LoopStatus::RunningModel)
        .await
        .unwrap();
    handle.steer(UserInput::text("lost").unwrap()).unwrap();
    handle.cancel();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    assert_eq!(
        steerings(&report),
        Vec::<String>::new(),
        "an unapplied steer must never reach the report"
    );
}

// ===== Phase 5 reviewer fixes: end-to-end panic/timeout isolation =====

struct PanicStartModel {
    descriptor: minicore_runtime::model::ModelDescriptor,
}

impl minicore_runtime::model::Model for PanicStartModel {
    fn descriptor(&self) -> &minicore_runtime::model::ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: minicore_runtime::model::ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(async { panic!("scripted model start panic") })
    }
}

/// A stream that panics on its first poll, as some adapters can.
struct PanicStream;

impl Stream for PanicStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        panic!("scripted model stream poll panic")
    }
}

struct PanicStreamModel {
    descriptor: minicore_runtime::model::ModelDescriptor,
}

impl minicore_runtime::model::Model for PanicStreamModel {
    fn descriptor(&self) -> &minicore_runtime::model::ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: minicore_runtime::model::ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(async { Ok::<ModelStream, ModelError>(Box::pin(PanicStream)) })
    }
}

struct PanicPrompt;

impl PromptProvider for PanicPrompt {
    fn prepare<'a>(&'a self, _request: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async { panic!("scripted prompt prepare panic") })
    }
}

fn model_failure_kind(report: &LoopReport) -> Option<minicore_runtime::LoopFailureKind> {
    match &report.outcome {
        LoopOutcome::Failed(failure) => Some(failure.kind),
        _ => None,
    }
}

fn tool_results_by_call(
    report: &LoopReport,
) -> Vec<(String, minicore_runtime::tools::ToolResultOutcome)> {
    report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => {
                Some((result.call_id.as_str().to_string(), result.outcome))
            }
            _ => None,
        })
        .collect()
}

/// A model `start` panic is isolated by the driver: the loop reports
/// Failed(Model), the completion is delivered normally, and no partial
/// assistant text ever reaches the history.
#[tokio::test]
async fn model_start_panic_yields_model_failure_without_history() {
    let descriptor = minicore_runtime::model::ModelDescriptor::new(
        "fake/panic-start".parse().unwrap(),
        8192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap();
    let agent = AgentLoop::start(
        request(config_full(
            Arc::new(PanicStartModel { descriptor }),
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();

    let report = agent.join().await.unwrap();
    assert_eq!(
        model_failure_kind(&report),
        Some(minicore_runtime::LoopFailureKind::Model)
    );
    assert_eq!(report.requests, 0);
    assert!(
        !report
            .appended
            .iter()
            .any(|item| matches!(item, HistoryItem::Assistant(_))),
        "a panicked model call must not leave a partial assistant in the history"
    );
    let waited = handle.wait().await.unwrap();
    assert!(
        Arc::ptr_eq(&waited, &report),
        "completion is delivered without deadlock"
    );
}

/// The same for a panic while polling the stream: Failed(Model), no partial
/// assistant, normal completion.
#[tokio::test]
async fn model_stream_poll_panic_yields_model_failure_without_history() {
    let descriptor = minicore_runtime::model::ModelDescriptor::new(
        "fake/panic-stream".parse().unwrap(),
        8192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap();
    let agent = AgentLoop::start(
        request(config_full(
            Arc::new(PanicStreamModel { descriptor }),
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            project(),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(
        model_failure_kind(&report),
        Some(minicore_runtime::LoopFailureKind::Model)
    );
    assert_eq!(report.requests, 0);
    assert!(
        !report
            .appended
            .iter()
            .any(|item| matches!(item, HistoryItem::Assistant(_)))
    );
}

/// A `Tool::execute` panic becomes exactly one Failed ToolResult for that call
/// and the loop continues to the next model request.
#[tokio::test]
async fn tool_execute_panic_is_a_failed_result_and_the_loop_continues() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Panic),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(
        report.requests, 2,
        "the loop continued after the failed call"
    );
    assert_eq!(recorded_calls(&calls), ["call_1"]);
    let results = tool_results_by_call(&report);
    assert_eq!(
        results,
        vec![(
            "call_1".to_string(),
            minicore_runtime::tools::ToolResultOutcome::Failed
        )]
    );
    assert!(
        report
            .appended
            .iter()
            .any(|item| matches!(item, HistoryItem::Assistant(_))),
        "the continuation request runs the model again"
    );
}

/// A panicking PromptProvider surfaces as Failed(Prompt) with a delivered
/// report and no model request ever issued.
#[tokio::test]
async fn prompt_provider_panic_yields_prompt_failure() {
    let (model, _) = scripted_model(vec![Round::Final { text: "a" }]);
    let agent = AgentLoop::start(
        request(config_full(
            model,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(PanicPrompt),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(
        model_failure_kind(&report),
        Some(minicore_runtime::LoopFailureKind::Prompt)
    );
    assert_eq!(report.requests, 0);
}

/// A panicking ToolPolicy::decide fails closed: the call gets exactly one
/// Denied ToolResult and the loop continues to the next request.
#[tokio::test]
async fn policy_panic_is_denied_and_the_loop_continues() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::Panic]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);
    assert!(
        calls.lock().unwrap().is_empty(),
        "a denied call must not execute the tool"
    );
    let results = tool_results_by_call(&report);
    assert_eq!(
        results,
        vec![(
            "call_1".to_string(),
            minicore_runtime::tools::ToolResultOutcome::Denied
        )]
    );
}

/// A `tool_timeout` port deadline marks only the current call Failed; the
/// remaining calls of the same batch still run in order and the final model
/// request still happens.
#[tokio::test(start_paused = true)]
async fn tool_timeout_port_deadline_fails_the_call_and_the_batch_continues() {
    let hold_entered = Arc::new(Notify::new());
    let calls_b = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("hold", "call_1"), ("echo", "call_2")],
        },
        Round::Final { text: "done" },
    ]);
    let mut tools = ToolSet::builder();
    tools.register(RecordingTool {
        spec: ToolSpec::new(
            "hold".parse().unwrap(),
            "holds forever",
            json!({"type": "object"}),
        )
        .unwrap(),
        calls: Arc::new(Mutex::new(Vec::new())),
        behavior: ToolBehavior::Hold,
        entered: Some(Arc::clone(&hold_entered)),
    });
    tools.register(RecordingTool {
        spec: echo_spec(),
        calls: Arc::clone(&calls_b),
        behavior: ToolBehavior::Succeed,
        entered: None,
    });
    let mut options = LoopOptions::default_checked().unwrap();
    options.tool_timeout = Duration::from_millis(100);
    let agent = AgentLoop::start(
        request(config_full(
            model,
            ReasoningPreference::Auto,
            tools.build().unwrap(),
            None,
            project(),
        )),
        options,
    )
    .unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    // Let request 0 reach the first (holding) tool call, then expire only the
    // tool port deadline.
    let entered = hold_entered.notified();
    tokio::pin!(entered);
    entered.as_mut().enable();
    tokio::time::advance(Duration::from_millis(10)).await;
    entered.as_mut().await;
    tokio::time::advance(Duration::from_millis(300)).await;

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(
        report.requests, 2,
        "the model continued after the timed-out call"
    );
    assert_eq!(recorded_calls(&calls_b), ["call_2"]);
    let results = tool_results_by_call(&report);
    assert_eq!(
        results,
        vec![
            (
                "call_1".to_string(),
                minicore_runtime::tools::ToolResultOutcome::Failed
            ),
            (
                "call_2".to_string(),
                minicore_runtime::tools::ToolResultOutcome::Success
            ),
        ],
        "exactly one result per call, in order"
    );
}

// ===== Phase 6: DefaultPromptProvider and core prompt budgets =====

fn default_descriptor() -> minicore_runtime::model::ModelDescriptor {
    minicore_runtime::model::ModelDescriptor::new(
        "host:prompt-default".parse().unwrap(),
        8192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap()
}

fn default_request<'a>(
    history: HistoryView<'a>,
    cancelled: bool,
    descriptor: &'a minicore_runtime::model::ModelDescriptor,
    tools: &'a [ToolSpec],
) -> PromptRequest<'a> {
    PromptRequest {
        loop_id: minicore_runtime::LoopId::new().unwrap(),
        request_index: 0,
        history,
        model: descriptor,
        reasoning: ReasoningPreference::Auto,
        tools,
        cancellation: match cancelled {
            true => {
                let token = tokio_util::sync::CancellationToken::new();
                token.cancel();
                token
            }
            false => tokio_util::sync::CancellationToken::new(),
        },
        deadline: tokio::time::Instant::now() + Duration::from_secs(10),
    }
}

fn user_item(kind: UserMessageKind, text: &str) -> HistoryItem {
    HistoryItem::User(UserHistory {
        loop_id: minicore_runtime::LoopId::new().unwrap(),
        kind,
        input: UserInput::text(text).unwrap(),
    })
}

fn assistant_item(model: &str, text: &str) -> HistoryItem {
    HistoryItem::Assistant(minicore_runtime::history::AssistantHistory {
        loop_id: minicore_runtime::LoopId::new().unwrap(),
        request_index: 0,
        model: model.parse().unwrap(),
        reasoning: ReasoningPreference::Auto,
        content: vec![minicore_runtime::model::AssistantPart::Text(
            text.to_owned(),
        )],
        finish_reason: ModelFinishReason::Stop,
        usage: minicore_runtime::model::Usage::new(0, 0, 0),
    })
}

fn tool_result_item() -> HistoryItem {
    HistoryItem::ToolResult(minicore_runtime::history::ToolResultHistory {
        loop_id: minicore_runtime::LoopId::new().unwrap(),
        request_index: 0,
        call_id: "call_00000000000000000000000000000001".parse().unwrap(),
        tool_name: "echo".parse().unwrap(),
        outcome: minicore_runtime::tools::ToolResultOutcome::Success,
        output: minicore_runtime::tools::ToolOutput::new("out").unwrap(),
    })
}

/// Default projection preserves base+appended order, folds the summary into
/// the fixed system text, and omits an empty optional system prompt.
#[tokio::test]
async fn default_provider_projects_history_in_order_with_summary_and_optional_system() {
    let base = vec![
        user_item(UserMessageKind::Prompt, "base q"),
        assistant_item("host:model-a", "base a"),
        HistoryItem::Summary(minicore_runtime::history::SummaryHistory {
            content: BoundedText::new("s1").unwrap(),
        }),
    ];
    let appended = vec![
        user_item(UserMessageKind::Steering, "steer"),
        tool_result_item(),
        assistant_item("host:model-b", "append a"),
    ];
    let view = HistoryView::new(&base, &appended);
    let descriptor = default_descriptor();
    let tools: Vec<ToolSpec> = Vec::new();

    let expected_without_system = vec![
        ModelMessage::user("base q").unwrap(),
        ModelMessage::assistant(vec![minicore_runtime::model::AssistantPart::Text(
            "base a".to_owned(),
        )])
        .unwrap(),
        ModelMessage::system("Conversation summary:\ns1").unwrap(),
        ModelMessage::user("steer").unwrap(),
        ModelMessage::tool_with_outcome(
            "call_00000000000000000000000000000001".parse().unwrap(),
            minicore_runtime::tools::ToolOutput::new("out").unwrap(),
            minicore_runtime::tools::ToolResultOutcome::Success,
        )
        .unwrap(),
        ModelMessage::assistant(vec![minicore_runtime::model::AssistantPart::Text(
            "append a".to_owned(),
        )])
        .unwrap(),
    ];

    // No system prompt.
    let rendered = DefaultPromptProvider::new(None)
        .prepare(default_request(view, false, &descriptor, &tools))
        .await
        .unwrap();
    assert_eq!(rendered.messages, expected_without_system);

    // Non-empty system prompt is emitted first.
    let with_system = DefaultPromptProvider::new(Some(BoundedText::new("SYS").unwrap()))
        .prepare(default_request(view, false, &descriptor, &tools))
        .await
        .unwrap();
    let mut expected = vec![ModelMessage::system("SYS").unwrap()];
    expected.extend(expected_without_system.iter().cloned());
    assert_eq!(with_system.messages, expected);

    // An empty (but present) system prompt is omitted too.
    let empty_system = DefaultPromptProvider::new(Some(BoundedText::new("").unwrap()))
        .prepare(default_request(view, false, &descriptor, &tools))
        .await
        .unwrap();
    assert_eq!(empty_system.messages, expected_without_system);
}

/// Mixed model refs in the base history are accepted and projected verbatim.
#[tokio::test]
async fn default_provider_accepts_mixed_model_refs_in_the_base() {
    let base = vec![
        user_item(UserMessageKind::Prompt, "q"),
        assistant_item("host:model-a", "a"),
        assistant_item("host:model-b", "b"),
    ];
    let descriptor = default_descriptor();
    let tools: Vec<ToolSpec> = Vec::new();
    let view = HistoryView::new(&base, &[]);
    let rendered = DefaultPromptProvider::new(None)
        .prepare(default_request(view, false, &descriptor, &tools))
        .await
        .unwrap();
    assert_eq!(rendered.messages.len(), 3);
    assert!(matches!(&rendered.messages[1], ModelMessage::Assistant(parts) if parts.len() == 1));
    assert!(matches!(&rendered.messages[2], ModelMessage::Assistant(parts) if parts.len() == 1));

    // An agent loop starts and completes with such a mixed-ref base history.
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        LoopRequest::new(
            base.into(),
            UserInput::text("still runs").unwrap(),
            config_full(
                model,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                project(),
            ),
        ),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
}

/// An inconsistent old tool history (orphan tool result) is host-trusted at
/// start but fails the request as Prompt when Core builds the ModelRequest.
#[tokio::test]
async fn inconsistent_old_tool_history_fails_as_prompt_but_start_succeeds() {
    let orphan = vec![user_item(UserMessageKind::Prompt, "q"), tool_result_item()];
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        LoopRequest::new(
            orphan.into(),
            UserInput::text("go").unwrap(),
            config_full(
                model,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                project(),
            ),
        ),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(
        model_failure_kind(&report),
        Some(minicore_runtime::LoopFailureKind::Prompt)
    );
    assert_eq!(report.requests, 0);
}

struct ManyMessagesPrompt;

impl PromptProvider for ManyMessagesPrompt {
    fn prepare<'a>(&'a self, _request: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async {
            Ok(PreparedPrompt {
                messages: vec![
                    ModelMessage::user("a").unwrap(),
                    ModelMessage::user("b").unwrap(),
                    ModelMessage::user("c").unwrap(),
                ],
            })
        })
    }
}

/// `max_prompt_messages` is enforced by Core for every provider, including
/// custom ones that do not check it themselves.
#[tokio::test]
async fn prompt_message_limit_applies_to_custom_providers() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_prompt_messages = 2;
    let agent = AgentLoop::start(
        request(config_full(
            model,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(ManyMessagesPrompt),
        )),
        options,
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(
        model_failure_kind(&report),
        Some(minicore_runtime::LoopFailureKind::Prompt)
    );
    assert_eq!(report.requests, 0);
}

/// The default provider answers Cancelled for an already-cancelled or expired
/// request without touching history.
#[tokio::test]
async fn default_provider_cancelled_when_request_is_cancelled() {
    let single = vec![user_item(UserMessageKind::Prompt, "q")];
    let view = HistoryView::new(&[], &single);
    let descriptor = default_descriptor();
    let tools: Vec<ToolSpec> = Vec::new();
    let cancelled = DefaultPromptProvider::new(None)
        .prepare(default_request(view, true, &descriptor, &tools))
        .await
        .unwrap_err();
    assert_eq!(cancelled, PromptError::Cancelled);

    let expired = PromptRequest {
        loop_id: minicore_runtime::LoopId::new().unwrap(),
        request_index: 0,
        history: view,
        model: &descriptor,
        reasoning: ReasoningPreference::Auto,
        tools: &tools,
        cancellation: tokio_util::sync::CancellationToken::new(),
        deadline: tokio::time::Instant::now() - Duration::from_secs(1),
    };
    let expired_error = DefaultPromptProvider::new(None)
        .prepare(expired)
        .await
        .unwrap_err();
    assert_eq!(expired_error, PromptError::Cancelled);
}

/// End-to-end: a loop driven entirely by the default provider with a system
/// prompt projects and completes normally.
#[tokio::test]
async fn default_provider_runs_a_loop_end_to_end_with_system_prompt() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config_full(
            model,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(DefaultPromptProvider::new(Some(
                BoundedText::new("SYSTEM").unwrap(),
            ))),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_completed_with_text(&report, "done");
    assert_eq!(report.requests, 1);
}

/// The default provider always projects a legal maximum-capacity summary: the
/// fixed prefix is kept verbatim and any overflow is truncated from the
/// content tail at a UTF-8 character boundary, never mid-character.
#[tokio::test]
async fn default_provider_truncates_large_summary_at_the_absolute_limit() {
    let descriptor = default_descriptor();
    let tools: Vec<ToolSpec> = Vec::new();
    let prefix = "Conversation summary:\n";
    let absolute = BoundedText::MAX_BYTES;

    async fn render(
        content: &BoundedText,
        descriptor: &minicore_runtime::model::ModelDescriptor,
        tools: &[ToolSpec],
    ) -> Result<Vec<ModelMessage>, PromptError> {
        let history = vec![HistoryItem::Summary(
            minicore_runtime::history::SummaryHistory {
                content: content.clone(),
            },
        )];
        let view = HistoryView::new(&history, &[]);
        let prepared = DefaultPromptProvider::new(None)
            .prepare(default_request(view, false, descriptor, tools))
            .await?;
        Ok(prepared.messages)
    }

    // Max ASCII content: must truncate but keep the exact prefix, stay within
    // the absolute ceiling, and produce a valid ModelMessage.
    let ascii = BoundedText::new("x".repeat(absolute)).unwrap();
    let messages = render(&ascii, &descriptor, &tools).await.unwrap();
    assert_eq!(messages.len(), 1);
    let ModelMessage::System(text) = &messages[0] else {
        panic!("summary must project to a System message");
    };
    assert!(
        text.starts_with(prefix),
        "the fixed prefix must be preserved"
    );
    assert!(text.len() <= absolute);
    messages[0].validate().unwrap();

    // Multi-byte content crossing the truncation point: the cut must land on a
    // character boundary and never panic.
    let budget = absolute - prefix.len();
    let multibyte = BoundedText::new(format!("{}€€", "x".repeat(budget - 2))).unwrap();
    let messages = render(&multibyte, &descriptor, &tools).await.unwrap();
    let ModelMessage::System(text) = &messages[0] else {
        panic!("summary must project to a System message");
    };
    assert!(text.starts_with(prefix));
    assert!(text.len() <= absolute);
    assert!(text.is_char_boundary(text.len()));
    messages[0].validate().unwrap();

    // Exactly the remaining budget: no truncation at all.
    let exact = BoundedText::new("y".repeat(budget)).unwrap();
    let messages = render(&exact, &descriptor, &tools).await.unwrap();
    let ModelMessage::System(text) = &messages[0] else {
        panic!("summary must project to a System message");
    };
    assert!(text.starts_with(prefix));
    assert_eq!(text.len(), absolute);
    assert!(
        text.ends_with("y"),
        "no truncation when content exactly fits"
    );
    messages[0].validate().unwrap();
}

/// The default provider rejects an empty history as EmptyPrompt.
#[tokio::test]
async fn default_provider_empty_history_yields_empty_prompt() {
    let descriptor = default_descriptor();
    let tools: Vec<ToolSpec> = Vec::new();
    let empty: Vec<HistoryItem> = Vec::new();
    let view = HistoryView::new(&empty, &empty);
    let error = DefaultPromptProvider::new(None)
        .prepare(default_request(view, false, &descriptor, &tools))
        .await
        .unwrap_err();
    assert_eq!(error, PromptError::EmptyPrompt);
}

struct InvalidTextPrompt;

impl PromptProvider for InvalidTextPrompt {
    fn prepare<'a>(&'a self, _request: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async {
            Ok(PreparedPrompt {
                messages: vec![ModelMessage::User("\u{1}bad".to_string())],
            })
        })
    }
}

/// A custom provider constructing an invalid message through the public enum
/// variant is failed as Prompt by Core's ModelRequest validation.
#[tokio::test]
async fn invalid_message_from_public_variant_fails_as_prompt() {
    let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
    let agent = AgentLoop::start(
        request(config_full(
            model,
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(InvalidTextPrompt),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(
        model_failure_kind(&report),
        Some(minicore_runtime::LoopFailureKind::Prompt)
    );
    assert_eq!(report.requests, 0);
}

/// FIX-02-T01: Success ToolOutput exceeding configured limit becomes Failed,
/// the large original content does not enter history, and the model can continue.
#[tokio::test]
async fn tool_output_exceeding_limit_fails_and_original_content_omitted_from_history() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "recovered" },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_output_bytes = 16;

    let large_output = "x".repeat(64);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(
                Arc::clone(&calls),
                ToolBehavior::Output(large_output.clone()),
            ),
            None,
        )),
        options,
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Failed);
    assert!(tool_results[0].output.content().byte_len() <= 16);
    assert!(
        !tool_results[0]
            .output
            .content()
            .as_str()
            .contains(&large_output)
    );
}

/// FIX-02-T02: ToolOutput within configured limit remains Success with unchanged content.
#[tokio::test]
async fn tool_output_within_limit_succeeds_with_unchanged_content() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_output_bytes = 16;

    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(
                Arc::clone(&calls),
                ToolBehavior::Output("success!".to_owned()),
            ),
            None,
        )),
        options,
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Success);
    assert_eq!(tool_results[0].output.content().as_str(), "success!");
    assert!(tool_results[0].output.content().byte_len() <= 16);
}

/// FIX-02-T03: ToolInput answer whose encoded result exceeds limit becomes Failed and bounded.
#[tokio::test]
async fn tool_input_answer_exceeding_limit_fails_and_bounded() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_output_bytes = 16;

    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::RequestInput),
            None,
        )),
        options,
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    let pending = states.borrow().pending_interaction.clone().unwrap();
    let large_answer = "a".repeat(32);
    handle
        .answer(
            pending.interaction_id,
            InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                BoundedText::new(large_answer.clone()).unwrap(),
            )),
        )
        .unwrap();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Failed);
    assert_ne!(tool_results[0].outcome, ToolResultOutcome::InputProvided);
    assert!(tool_results[0].output.content().byte_len() <= 16);
    assert!(
        !tool_results[0]
            .output
            .content()
            .as_str()
            .contains(&large_answer)
    );
}

/// FIX-02-T04: Policy deny reason exceeding configured limit remains Denied and bounded.
#[tokio::test]
async fn policy_denied_reason_exceeding_limit_remains_denied_and_bounded() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_output_bytes = 16;

    let large_reason = "x".repeat(64);
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::DenyReason(
            BoundedText::new(large_reason.clone()).unwrap(),
        )]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            Some(policy),
        )),
        options,
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Denied);
    assert!(tool_results[0].output.content().byte_len() <= 16);
    assert!(
        !tool_results[0]
            .output
            .content()
            .as_str()
            .contains(&large_reason)
    );
}

/// FIX-02-T05a: When max_output_bytes=1, max tool rounds terminal result does not panic
/// and output byte len is <= 1.
#[tokio::test]
async fn minimal_limit_one_byte_bounds_max_tool_rounds_terminal_result() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Tools {
            calls: vec![("echo", "call_2")],
        },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.max_tool_rounds = 1;
    options.limits.max_tool_output_bytes = 1;

    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Succeed),
            None,
        )),
        options,
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert!(matches!(
        report.outcome,
        LoopOutcome::Failed(ref failure)
            if failure.kind == minicore_runtime::LoopFailureKind::MaxToolRounds
    ));

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
    assert_eq!(tool_results[1].outcome, ToolResultOutcome::Failed);
    for result in &tool_results {
        assert!(
            result.output.content().byte_len() <= 1,
            "tool result output byte len {} exceeds limit 1",
            result.output.content().byte_len()
        );
    }
}

/// FIX-02-T05b: When max_output_bytes=1, ordinary tool execution failure (tool failed)
/// allows the loop to continue and bounds the Failed ToolResult output to <= 1 byte.
#[tokio::test]
async fn minimal_limit_one_byte_bounds_tool_failure_result() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "recovered" },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_output_bytes = 1;

    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Fail),
            None,
        )),
        options,
    )
    .unwrap();

    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].call_id.as_str(), "call_1");
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Failed);
    assert!(
        tool_results[0].output.content().byte_len() <= 1,
        "tool result output byte len {} exceeds limit 1",
        tool_results[0].output.content().byte_len()
    );
}

/// FIX-02-T05c: When max_output_bytes=1, active and cancelled remaining tool calls
/// do not panic and output byte len is <= 1.
#[tokio::test]
async fn minimal_limit_one_byte_bounds_cancelled_tool_batch() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("hold", "call_1"), ("echo", "call_2")],
    }]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_output_bytes = 1;

    let hold_entered = Arc::new(Notify::new());
    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: ToolSpec::new(
            "hold".parse().unwrap(),
            "hold tool",
            json!({"type": "object"}),
        )
        .unwrap(),
        calls: Arc::clone(&calls),
        behavior: ToolBehavior::Hold,
        entered: Some(Arc::clone(&hold_entered)),
    });
    builder.register(RecordingTool {
        spec: echo_spec(),
        calls: Arc::clone(&calls),
        behavior: ToolBehavior::Succeed,
        entered: None,
    });
    let tools = builder.build().unwrap();

    let entered = hold_entered.notified();
    tokio::pin!(entered);
    entered.as_mut().enable();

    let agent = AgentLoop::start(request(config(model, tools, None)), options).unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    entered.as_mut().await;
    handle.cancel();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
    assert_eq!(tool_results[0].call_id.as_str(), "call_1");
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Cancelled);
    assert_eq!(tool_results[1].call_id.as_str(), "call_2");
    assert_eq!(tool_results[1].outcome, ToolResultOutcome::Cancelled);

    for result in &tool_results {
        assert!(
            result.output.content().byte_len() <= 1,
            "tool result {} byte_len {} exceeds limit 1",
            result.call_id.as_str(),
            result.output.content().byte_len()
        );
    }
}

/// FIX-02-T06: ToolFinished.output_bytes equals the final bounded History ToolResult output bytes.
#[tokio::test]
async fn tool_finished_event_output_bytes_matches_history_bounded_bytes() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_output_bytes = 16;

    let large_output = "x".repeat(64);
    let mut agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::Output(large_output)),
            None,
        )),
        options,
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut finished_events = Vec::new();
    while let Some(envelope) = events.recv().await {
        if let LoopEvent::ToolFinished {
            call_id,
            outcome,
            output_bytes,
            ..
        } = envelope.event
        {
            finished_events.push((call_id, outcome, output_bytes));
        }
    }

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(finished_events.len(), 1);

    let (event_call_id, event_outcome, event_output_bytes) = &finished_events[0];
    assert_eq!(event_call_id.as_str(), "call_1");
    assert_eq!(*event_outcome, ToolResultOutcome::Failed);
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Failed);
    assert_eq!(
        *event_output_bytes,
        tool_results[0].output.content().byte_len()
    );
    assert!(*event_output_bytes <= 16);
    assert_ne!(*event_output_bytes, 64);
}

/// Helper to verify that an invalid ToolInputRequest returned by a tool fails without
/// interaction, does not transition to WaitingForInput or emit InteractionRequested,
/// produces a Failed ToolResult, and allows the model to continue to completion.
async fn assert_invalid_tool_input_request_fails(invalid_request: ToolInputRequest) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "recovered" },
    ]);
    let mut agent = AgentLoop::start(
        request(config(
            model,
            tool_set(
                Arc::clone(&calls),
                ToolBehavior::RequestInvalidInput(invalid_request),
            ),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| {
            state.pending_interaction.is_some()
                || state.status == LoopStatus::WaitingForInput
                || state.status == LoopStatus::Finished
        })
        .await
        .unwrap();

    let state_snapshot = states.borrow().clone();
    let entered_waiting = state_snapshot.pending_interaction.is_some()
        || state_snapshot.status == LoopStatus::WaitingForInput;

    if entered_waiting {
        handle.cancel();
    }

    assert!(
        !entered_waiting,
        "invalid tool input request must not create pending interaction or enter WaitingForInput",
    );

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 2);

    let mut interaction_requested = false;
    while let Some(envelope) = events.recv().await {
        if matches!(envelope.event, LoopEvent::InteractionRequested { .. }) {
            interaction_requested = true;
        }
    }
    assert!(
        !interaction_requested,
        "invalid tool input request must not emit InteractionRequested event",
    );

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].call_id.as_str(), "call_1");
    assert_eq!(tool_results[0].outcome, ToolResultOutcome::Failed);
}

/// FIX-05-T05: Fake Tool returning a hand-crafted invalid ToolInputRequest (SingleChoice with empty choices)
/// does not emit InteractionRequested, does not enter WaitingForInput, produces Failed ToolResult,
/// and model continues to next request and completes.
#[tokio::test]
async fn invalid_tool_input_single_choice_empty_choices_fails_without_interaction() {
    let invalid_request = ToolInputRequest {
        prompt: BoundedText::new("choose").unwrap(),
        choices: vec![],
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_invalid_tool_input_request_fails(invalid_request).await;
}

/// FIX-05-T05 (empty prompt): Fake Tool returning a ToolInputRequest with empty prompt fails without interaction.
#[tokio::test]
async fn invalid_tool_input_empty_prompt_fails_without_interaction() {
    let invalid_request = ToolInputRequest {
        prompt: BoundedText::new("").unwrap(),
        choices: vec![BoundedText::new("choice_1").unwrap()],
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_invalid_tool_input_request_fails(invalid_request).await;
}

/// FIX-05-T05 (too many choices): Fake Tool returning a ToolInputRequest with 33 choices fails without interaction.
#[tokio::test]
async fn invalid_tool_input_too_many_choices_fails_without_interaction() {
    let invalid_request = ToolInputRequest {
        prompt: BoundedText::new("choose").unwrap(),
        choices: (0..33)
            .map(|i| BoundedText::new(format!("choice_{i}")).unwrap())
            .collect(),
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_invalid_tool_input_request_fails(invalid_request).await;
}

/// FIX-05-T05 (oversized choice): Fake Tool returning a ToolInputRequest with a choice > 1024 bytes fails without interaction.
#[tokio::test]
async fn invalid_tool_input_oversized_choice_fails_without_interaction() {
    let invalid_request = ToolInputRequest {
        prompt: BoundedText::new("choose").unwrap(),
        choices: vec![BoundedText::new("x".repeat(1025)).unwrap()],
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_invalid_tool_input_request_fails(invalid_request).await;
}

/// FIX-05-T06: Valid SingleChoice tool input interaction feeds an InputProvided result and loop completes.
#[tokio::test]
async fn tool_input_single_choice_interaction_feeds_an_input_provided_result() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "call_1")],
        },
        Round::Final { text: "done" },
    ]);
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(Arc::clone(&calls), ToolBehavior::RequestChoiceInput),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    let pending = states.borrow().pending_interaction.clone().unwrap();
    handle
        .answer(
            pending.interaction_id,
            InteractionAnswer::ToolInput(ToolInputAnswer::Choice { index: 1 }),
        )
        .unwrap();

    let report = report_task.await.unwrap().unwrap();
    assert_completed_with_text(&report, "done");
    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].outcome,
        minicore_runtime::tools::ToolResultOutcome::InputProvided
    );
    assert!(tool_results[0].output.content().as_str().contains("beta"));
}
