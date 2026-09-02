//! Phase 2 v0.4 contracts: one agent loop runs without session ownership.
//!
//! All synchronization is deterministic (Notify / watch / oneshot); no test
//! relies on sleeping to prove ordering.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::Notify;

use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason,
    ModelMessage, ModelRequest, ModelStartFuture, ModelStream, ReasoningPreference,
};
use minicore_runtime::prompt_provider::{
    PreparedPrompt, PromptError, PromptFuture, PromptProvider, PromptRequest,
};
use minicore_runtime::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision, ToolError,
    ToolExecutionOutcome, ToolFuture, ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest,
    ToolInvocation, ToolOutput, ToolPolicy, ToolPolicyError, ToolPolicyFuture, ToolPolicyRequest,
    ToolSet, ToolSpec,
};
use minicore_runtime::value::BoundedText;
use minicore_runtime::{
    AgentLoop, AnswerError, CancelReason, ExecutionConfig, HistoryItem, InteractionAnswer,
    LoopEvent, LoopOptions, LoopOutcome, LoopReport, LoopRequest, LoopStartError, LoopStatus,
    ToolCallId, UserInput,
};

fn reasoning_set() -> BTreeSet<ReasoningPreference> {
    BTreeSet::from([ReasoningPreference::Auto])
}

fn scripted_model(rounds: Vec<Round>) -> (Arc<dyn Model>, Arc<Notify>) {
    let descriptor = ModelDescriptor::new(
        "fake/scripted-model".parse().unwrap(),
        8192,
        reasoning_set(),
        true,
    )
    .unwrap();
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
            .get(usize::from(context.round))
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
        Round::Tools { calls } => {
            let mut events = Vec::new();
            for (name, call_id) in calls {
                let call_id: ToolCallId = call_id.parse().unwrap();
                events.push(Ok(ModelEvent::ToolCallStart {
                    tool_call_id: call_id.clone(),
                    tool_name: name.parse().unwrap(),
                }));
                events.push(Ok(ModelEvent::tool_call_arguments_delta(
                    call_id.clone(),
                    "{}",
                )
                .unwrap()));
                events.push(Ok(ModelEvent::ToolCallEnd {
                    tool_call_id: call_id,
                }));
            }
            events.push(Ok(ModelEvent::Finish {
                reason: ModelFinishReason::ToolCalls,
            }));
            Box::pin(futures_util::stream::iter(events))
        }
    }
}

#[derive(Clone)]
enum ToolBehavior {
    Succeed,
    Fail,
    Hold,
    RequestInput,
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
                ToolBehavior::Fail => Err(ToolError::Failed),
                ToolBehavior::RequestInput => {
                    let request =
                        ToolInputRequest::new("type the answer", vec![], ToolInputAnswerKind::Text)
                            .unwrap();
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
    tool_set_with_entered(calls, behavior, None)
}

fn tool_set_with_entered(
    calls: Arc<Mutex<Vec<ToolCallId>>>,
    behavior: ToolBehavior,
    entered: Option<Arc<Notify>>,
) -> ToolSet {
    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: echo_spec(),
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
    RequireApproval,
    Hold,
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
                PolicyPlan::RequireApproval => ToolDecision::require_approval(
                    ApprovalRequest::new("approve this call?", ApprovalRisk::High).unwrap(),
                ),
                PolicyPlan::Hold => {
                    request.cancellation.cancelled().await;
                    Err(ToolPolicyError::Cancelled)
                }
            }
        })
    }
}

struct ProjectingPrompt {
    system: Option<BoundedText>,
}

impl PromptProvider for ProjectingPrompt {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async move {
            let mut messages = Vec::new();
            if let Some(system) = &self.system {
                messages.push(ModelMessage::system(system.as_str()).unwrap());
            }
            for item in request.history.iter() {
                match item {
                    HistoryItem::User(user) => {
                        messages.push(ModelMessage::user(user.input.as_text()).unwrap())
                    }
                    HistoryItem::Assistant(assistant) => {
                        messages.push(ModelMessage::assistant(assistant.content.clone()).unwrap())
                    }
                    HistoryItem::ToolResult(result) => messages.push(
                        ModelMessage::tool_with_outcome(
                            result.call_id.clone(),
                            result.output.clone(),
                            result.outcome,
                        )
                        .unwrap(),
                    ),
                    HistoryItem::Summary(summary) => messages.push(
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
    ExecutionConfig::new(
        model,
        ReasoningPreference::Auto,
        tools,
        policy,
        Arc::new(ProjectingPrompt { system: None }),
    )
    .expect("test config must validate")
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
        Arc::new(ProjectingPrompt { system: None }),
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
