//! Phase 8 v0.4 close-out: closes the contract gaps left open by p1/p2.
//!
//! Every gap here is one the p1/p2 suites did not pin exactly: reasoning vs
//! text deltas, event-free runs, multi-tool cancel settlement, repeated
//! answers, policy snapshot swaps, history limits, report delta-only
//! semantics, malformed model responses that must never enter Assistant
//! history, loop-level delivery retry, shared-resource cancel isolation, and
//! steers surviving a later prompt failure.
//!
//! All synchronization is deterministic (Notify / watch / oneshot /
//! start_paused + advance); no test sleeps.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream;

use serde_json::json;
use tokio::sync::Notify;

use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind, ModelEvent,
    ModelFinishReason, ModelMessage, ModelRequest, ModelStartFuture, ModelStream,
    ReasoningPreference,
};
use minicore_runtime::prompt::{
    PreparedPrompt, PromptError, PromptFuture, PromptProvider, PromptRequest,
};
use minicore_runtime::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision,
    ToolExecutionOutcome, ToolFuture, ToolInvocation, ToolOutput, ToolPolicy, ToolPolicyFuture,
    ToolPolicyRequest, ToolResultOutcome, ToolSet, ToolSpec,
};
use minicore_runtime::value::BoundedText;
use minicore_runtime::{
    AgentLoop, AnswerError, ConfigRevision, ExecutionConfig, HistoryItem, InteractionAnswer,
    LoopEvent, LoopFailureKind, LoopOptions, LoopOutcome, LoopRequest, LoopStartError,
    OutputChannel, ToolCallId, UserHistory, UserInput, UserMessageKind,
};

fn reasoning_set() -> BTreeSet<ReasoningPreference> {
    BTreeSet::from([ReasoningPreference::Auto])
}

fn descriptor(name: &str) -> ModelDescriptor {
    ModelDescriptor::new(name.parse().unwrap(), 8192, reasoning_set(), true).unwrap()
}

// ---------------------------------------------------------------------------
// Scripted model (same vocabulary as p2): finals, tool rounds, gated rounds.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Round {
    Final {
        text: &'static str,
    },
    Tools {
        calls: Vec<(&'static str, &'static str)>,
    },
    ReasoningThenText {
        reasoning: &'static str,
        text: &'static str,
    },
    Malformed {
        events: Vec<ModelEvent>,
    },
    Hold,
}

struct ScriptedModel {
    descriptor: ModelDescriptor,
    rounds: Arc<Vec<Round>>,
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
        Box::pin(async move { Ok::<ModelStream, ModelError>(Box::pin(round_stream(round))) })
    }
}

fn round_stream(round: Round) -> ModelStream {
    match round {
        Round::Hold => Box::pin(stream::pending()),
        Round::Final { text } => Box::pin(stream::iter(vec![
            Ok(ModelEvent::text_delta(text).unwrap()),
            Ok(ModelEvent::Finish {
                reason: ModelFinishReason::Stop,
            }),
        ])),
        Round::ReasoningThenText { reasoning, text } => Box::pin(stream::iter(vec![
            Ok(ModelEvent::reasoning_delta(reasoning).unwrap()),
            Ok(ModelEvent::text_delta(text).unwrap()),
            Ok(ModelEvent::Finish {
                reason: ModelFinishReason::Stop,
            }),
        ])),
        Round::Tools { calls } => Box::pin(stream::iter(
            tool_call_events(calls).into_iter().map(Ok::<_, ModelError>),
        )),
        Round::Malformed { events } => {
            Box::pin(stream::iter(events.into_iter().map(Ok::<_, ModelError>)))
        }
    }
}

fn tool_call_event(name: &str, call_id: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCallStart {
            tool_call_id: call_id.parse().unwrap(),
            tool_name: name.parse().unwrap(),
        },
        ModelEvent::tool_call_arguments_delta(call_id.parse().unwrap(), "{}").unwrap(),
        ModelEvent::ToolCallEnd {
            tool_call_id: call_id.parse().unwrap(),
        },
    ]
}

fn tool_call_events(calls: Vec<(&'static str, &'static str)>) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    for (name, call_id) in calls {
        events.extend(tool_call_event(name, call_id));
    }
    events.push(ModelEvent::Finish {
        reason: ModelFinishReason::ToolCalls,
    });
    events
}

fn model(rounds: Vec<Round>) -> Arc<dyn Model> {
    Arc::new(ScriptedModel {
        descriptor: descriptor("fake/scripted"),
        rounds: Arc::new(rounds),
    })
}

// ---------------------------------------------------------------------------
// Recording tools.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum ToolBehavior {
    Succeed,
    Hold,
}

struct RecordingTool {
    spec: ToolSpec,
    calls: Arc<Mutex<Vec<ToolCallId>>>,
    behavior: ToolBehavior,
}

impl Tool for RecordingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        let calls = Arc::clone(&self.calls);
        let recorded = invocation.tool_call_id().clone();
        let behavior = self.behavior.clone();
        Box::pin(async move {
            calls.lock().unwrap().push(recorded);
            match behavior {
                ToolBehavior::Succeed => Ok(ToolExecutionOutcome::Completed(
                    ToolOutput::new("tool result").unwrap(),
                )),
                ToolBehavior::Hold => {
                    // The batch runner cancels this call; it never completes
                    // side effects before that.
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        })
    }
}

fn tool_set(name: &str, calls: Arc<Mutex<Vec<ToolCallId>>>, behavior: ToolBehavior) -> ToolSet {
    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: ToolSpec::new(name.parse().unwrap(), "a tool", json!({"type": "object"})).unwrap(),
        calls,
        behavior,
    });
    builder.build().unwrap()
}

fn echo_toolset() -> (ToolSet, Arc<Mutex<Vec<ToolCallId>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    (
        tool_set("echo", Arc::clone(&calls), ToolBehavior::Succeed),
        calls,
    )
}

// ---------------------------------------------------------------------------
// Policies.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Plan {
    Allow,
    Deny,
    RequireApproval,
}

struct ScriptedPolicy {
    plan: Arc<Vec<Plan>>,
    calls: Arc<AtomicUsize>,
}

impl ToolPolicy for ScriptedPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let plan = self.plan.get(index).copied().unwrap_or(Plan::Allow);
        Box::pin(async move {
            match plan {
                Plan::Allow => Ok(ToolDecision::Allow),
                Plan::Deny => ToolDecision::deny("not today"),
                Plan::RequireApproval => ToolDecision::require_approval(
                    ApprovalRequest::new("approve this call?", ApprovalRisk::High).unwrap(),
                ),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Config / request helpers.
// ---------------------------------------------------------------------------

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
        Arc::new(minicore_runtime::prompt::DefaultPromptProvider::new(None)),
    )
    .expect("test config must validate")
}

fn start_with(request: LoopRequest, options: LoopOptions) -> Result<AgentLoop, LoopStartError> {
    AgentLoop::start(request, options)
}

fn request_with(config: ExecutionConfig, history: Vec<HistoryItem>) -> LoopRequest {
    LoopRequest::new(
        Arc::from(history),
        UserInput::text("hello").unwrap(),
        config,
    )
}

fn request(config: ExecutionConfig) -> LoopRequest {
    request_with(config, Vec::new())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Reasoning deltas stream on their own channel, never mixed with text.
#[tokio::test]
async fn reasoning_and_text_deltas_stream_on_separate_channels() {
    let mut agent = start_with(
        request(config(
            model(vec![Round::ReasoningThenText {
                reasoning: "thinking hard",
                text: "the answer",
            }]),
            ToolSet::default(),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut saw_reasoning = false;
    let mut saw_text = false;
    while let Some(envelope) = events.recv().await {
        if let LoopEvent::OutputDelta { channel, delta, .. } = envelope.event {
            match channel {
                OutputChannel::Reasoning => {
                    assert_eq!(delta.as_str(), "thinking hard");
                    saw_reasoning = true;
                }
                OutputChannel::Text => {
                    assert_eq!(delta.as_str(), "the answer");
                    saw_text = true;
                }
            }
        }
    }
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert!(saw_reasoning && saw_text, "both channels must be observed");
}

/// A loop still completes when the host never takes the event stream.
#[tokio::test]
async fn loop_completes_without_an_event_consumer() {
    let agent = start_with(
        request(config(
            model(vec![Round::Final { text: "hi" }]),
            ToolSet::default(),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 1);
}

/// Cancelling a multi-tool batch settles every in-flight call with exactly
/// one Cancelled result (no duplicates, no gaps).
#[tokio::test]
async fn cancelling_a_multi_tool_batch_settles_each_call_exactly_once() {
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(3);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let held_calls = Arc::clone(&calls);
    let tool = Arc::new(RecordingTool {
        spec: ToolSpec::new("echo".parse().unwrap(), "a tool", json!({"type": "object"})).unwrap(),
        calls: held_calls,
        behavior: ToolBehavior::Hold,
    });
    let tool_for_entered = Arc::clone(&tool);

    // Wrap the recording tool so each entered call also signals the channel.
    struct EnteredTool {
        inner: Arc<RecordingTool>,
        tx: tokio::sync::mpsc::Sender<ToolCallId>,
    }
    impl Tool for EnteredTool {
        fn spec(&self) -> &ToolSpec {
            self.inner.spec()
        }
        fn execute<'a>(
            &'a self,
            invocation: ToolInvocation,
            _context: ToolContext,
        ) -> ToolFuture<'a> {
            let inner = Arc::clone(&self.inner);
            let tx = self.tx.clone();
            let id = invocation.tool_call_id().clone();
            Box::pin(async move {
                let _ = tx.try_send(id.clone());
                inner.execute(invocation, _context).await
            })
        }
    }
    let mut builder = ToolSet::builder();
    builder.register(EnteredTool {
        inner: tool_for_entered,
        tx: entered_tx,
    });
    let tools = builder.build().unwrap();

    let agent = start_with(
        request(config(
            model(vec![Round::Tools {
                calls: vec![("echo", "call_1"), ("echo", "call_2"), ("echo", "call_3")],
            }]),
            tools,
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle().clone();
    let report_task = tokio::spawn(async move { agent.join().await });

    // Calls execute sequentially; the first one is in flight when we cancel.
    entered_rx
        .recv()
        .await
        .expect("the first call must enter before cancel");
    assert!(handle.cancel(), "live loop must accept cancel");
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(minicore_runtime::CancelReason::User)
    );

    let results: Vec<ToolCallId> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some((result.outcome, result.call_id.clone())),
            _ => None,
        })
        .map(|(outcome, call_id)| {
            assert_eq!(outcome, ToolResultOutcome::Cancelled);
            call_id
        })
        .collect();
    assert_eq!(results.len(), 3);
    let mut ids: Vec<String> = results.into_iter().map(|id| id.to_string()).collect();
    ids.sort();
    assert_eq!(ids, vec!["call_1", "call_2", "call_3"]);
}

/// A second answer to the same interaction is rejected.
#[tokio::test]
async fn answering_the_same_interaction_twice_fails() {
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![Plan::RequireApproval]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let (tools, _) = echo_toolset();
    let agent = start_with(
        request(config(
            model(vec![Round::Tools {
                calls: vec![("echo", "call_1")],
            }]),
            tools,
            Some(policy),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let handle = agent.handle().clone();
    let report_task = tokio::spawn(async move { agent.join().await });

    let mut states = handle.watch_state();
    states
        .wait_for(|state| state.pending_interaction.is_some())
        .await
        .unwrap();
    let interaction_id = states
        .borrow()
        .pending_interaction
        .clone()
        .unwrap()
        .interaction_id;

    handle
        .answer(
            interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
        )
        .expect("first answer resolves the interaction");
    let second = handle.answer(
        interaction_id,
        InteractionAnswer::Approval(ApprovalDecision::AllowOnce),
    );
    assert!(
        second.is_err(),
        "a resolved interaction must not answer twice, got {second:?}"
    );
    assert!(matches!(
        second,
        Err(AnswerError::InteractionNotFound | AnswerError::NotActive)
    ));

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(
        report
            .appended
            .iter()
            .filter(|item| matches!(item, HistoryItem::ToolResult(_)))
            .count(),
        1
    );
}

/// A policy update lands as a full snapshot at the next request boundary:
/// the in-flight batch keeps the old policy, the next batch sees the new one.
#[tokio::test]
async fn policy_update_applies_the_full_snapshot_at_the_next_request_batch() {
    /// Request 0 holds on the gate before streaming its tool call; later
    /// requests stream normally, so an update landing during request 0 cannot
    /// affect request 0's batch snapshot.
    struct GatedToolModel {
        descriptor: ModelDescriptor,
        gate: Arc<Notify>,
        rounds: Arc<Vec<Round>>,
    }
    impl Model for GatedToolModel {
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
                .unwrap_or(Round::Final { text: "done" });
            let gate = Arc::clone(&self.gate);
            Box::pin(async move {
                if context.request_index == 0 {
                    gate.notified().await;
                }
                Ok::<ModelStream, ModelError>(Box::pin(round_stream(round)))
            })
        }
    }

    let gate = Arc::new(Notify::new());
    let old_policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![Plan::Allow]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let new_policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![Plan::Deny]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let (tools, _) = echo_toolset();
    let gated_model: Arc<dyn Model> = Arc::new(GatedToolModel {
        descriptor: descriptor("fake/gated"),
        gate: Arc::clone(&gate),
        rounds: Arc::new(vec![
            Round::Tools {
                calls: vec![("echo", "call_1")],
            },
            Round::Tools {
                calls: vec![("echo", "call_2")],
            },
        ]),
    });

    let mut agent = start_with(
        request(config(
            Arc::clone(&gated_model),
            tools.clone(),
            Some(Arc::clone(&old_policy) as Arc<dyn ToolPolicy>),
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let handle = agent.handle().clone();
    let report_task = tokio::spawn(async move { agent.join().await });

    // Wait for request 0 to be issued (holding on the gate), then swap the
    // whole config snapshot: model stays, policy is replaced.
    wait_for_request(&mut events, 0).await;
    let revision = handle
        .update(config(
            Arc::clone(&gated_model),
            tools,
            Some(Arc::clone(&new_policy) as Arc<dyn ToolPolicy>),
        ))
        .expect("update is accepted while the loop is live");
    assert_eq!(revision, ConfigRevision::new(1));
    gate.notify_one();

    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);

    // Request 0's batch ran under the OLD policy (allowed); request 1's batch
    // under the NEW policy (denied) - the full snapshot swapped atomically.
    let results: Vec<(String, ToolResultOutcome)> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some((result.call_id.to_string(), result.outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "call_1");
    assert_eq!(results[0].1, ToolResultOutcome::Success);
    assert_eq!(results[1].0, "call_2");
    assert_eq!(results[1].1, ToolResultOutcome::Denied);
}

/// Oversized base history (items or bytes) is rejected at start.
#[tokio::test]
async fn start_rejects_history_over_item_and_byte_limits() {
    let loop_id = minicore_runtime::LoopId::new().unwrap();
    let item = |input: &str| {
        HistoryItem::User(UserHistory {
            loop_id,
            kind: UserMessageKind::Prompt,
            input: UserInput::text(input).unwrap(),
        })
    };
    let base = config(
        model(vec![Round::Final { text: "hi" }]),
        ToolSet::default(),
        None,
    );

    // Item count over the limit.
    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_history_items = 2;
    let too_many_items = vec![item("a"), item("b"), item("c")];
    assert!(matches!(
        start_with(request_with(base.clone(), too_many_items), options.clone()),
        Err(LoopStartError::HistoryTooLarge)
    ));

    // Byte estimate over the limit.
    options.limits.max_history_items = 100;
    options.limits.max_history_bytes = 64;
    let bloat = vec![item(&"x".repeat(200))];
    assert!(matches!(
        start_with(request_with(base, bloat), options),
        Err(LoopStartError::HistoryTooLarge)
    ));
}

/// The report carries only the loop's own in-memory delta, never a copy of
/// the base history the host passed in.
#[tokio::test]
async fn report_appended_is_only_the_loop_delta_not_the_base_history() {
    let loop_id = minicore_runtime::LoopId::new().unwrap();
    let base = vec![
        HistoryItem::User(UserHistory {
            loop_id,
            kind: UserMessageKind::Prompt,
            input: UserInput::text("first").unwrap(),
        }),
        HistoryItem::User(UserHistory {
            loop_id,
            kind: UserMessageKind::Prompt,
            input: UserInput::text("second").unwrap(),
        }),
    ];
    let agent = start_with(
        request_with(
            config(
                model(vec![Round::Final { text: "answer" }]),
                ToolSet::default(),
                None,
            ),
            base,
        ),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    // The delta is the fresh prompt plus the assistant response: the two base
    // users ("first"/"second") must never be copied into the report.
    let texts: Vec<String> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::User(user) => Some(user.input.as_text().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["hello"],
        "only the loop's own prompt is appended"
    );
    assert!(
        report
            .appended
            .iter()
            .any(|item| matches!(item, HistoryItem::Assistant(_))),
        "the delta contains the assistant response"
    );
    assert!(
        !report.appended.iter().any(
            |item| matches!(item, HistoryItem::User(user) if user.input.as_text() == "first"
                || user.input.as_text() == "second")
        ),
        "base history must never be copied into the report"
    );
}

/// A completed tool round survives a later cancel: the report keeps the
/// finished delta and drops only what was in flight.
#[tokio::test]
async fn cancel_after_a_completed_tool_round_keeps_that_delta_in_the_report() {
    let (tools, _) = echo_toolset();
    let mut agent = start_with(
        request(config(
            model(vec![
                Round::Tools {
                    calls: vec![("echo", "call_1")],
                },
                Round::Hold,
            ]),
            tools,
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let handle = agent.handle().clone();
    let report_task = tokio::spawn(async move { agent.join().await });

    // Request 1 is now in flight (held); request 0's tool round is done.
    wait_for_request(&mut events, 1).await;
    assert!(handle.cancel());
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(minicore_runtime::CancelReason::User)
    );

    let tool_results: Vec<_> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ToolResult(result) => Some((result.call_id.to_string(), result.outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_results,
        vec![("call_1".to_string(), ToolResultOutcome::Success)]
    );
    // Request 0's tool-call response appends its own ToolCall assistant
    // (completed delta); the held request 1 must not add a Text assistant.
    let has_text_assistant = report.appended.iter().any(|item| {
        matches!(
            item,
            HistoryItem::Assistant(assistant)
                if assistant.content.iter().any(|part| matches!(part, minicore_runtime::model::AssistantPart::Text(_)))
        )
    });
    assert!(
        !has_text_assistant,
        "the held request must not add a text assistant"
    );
}

/// A tool call without its terminal is an invalid response and never enters
/// Assistant history.
#[tokio::test]
async fn missing_tool_call_terminal_is_invalid_and_not_appended() {
    let start_event = ModelEvent::ToolCallStart {
        tool_call_id: "call_1".parse().unwrap(),
        tool_name: "echo".parse().unwrap(),
    };
    let malformed = model(vec![Round::Malformed {
        events: vec![
            start_event,
            ModelEvent::tool_call_arguments_delta("call_1".parse().unwrap(), "{}").unwrap(),
            ModelEvent::Finish {
                reason: ModelFinishReason::ToolCalls,
            },
        ],
    }]);
    let agent = start_with(
        request(config(malformed, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    match &report.outcome {
        LoopOutcome::Failed(failure) => {
            assert_eq!(failure.kind, LoopFailureKind::InvalidModelResponse)
        }
        other => panic!("expected InvalidModelResponse, got {other:?}"),
    }
    assert!(
        report
            .appended
            .iter()
            .all(|item| !matches!(item, HistoryItem::Assistant(_)))
    );
}

/// Two tool calls sharing one id in a single response are rejected and never
/// enter Assistant history.
#[tokio::test]
async fn duplicate_tool_call_ids_are_rejected_and_not_appended() {
    let mut events = Vec::new();
    events.extend(tool_call_event("echo", "call_1"));
    events.extend(tool_call_event("echo", "call_1"));
    events.push(ModelEvent::Finish {
        reason: ModelFinishReason::ToolCalls,
    });
    let malformed = model(vec![Round::Malformed { events }]);
    let agent = start_with(
        request(config(malformed, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    match &report.outcome {
        LoopOutcome::Failed(failure) => {
            assert_eq!(failure.kind, LoopFailureKind::InvalidModelResponse)
        }
        other => panic!("expected InvalidModelResponse, got {other:?}"),
    }
    assert!(
        report
            .appended
            .iter()
            .all(|item| !matches!(item, HistoryItem::Assistant(_)))
    );
}

/// A stream failing mid-response is a model failure; the partial text never
/// becomes an Assistant item.
#[tokio::test]
async fn partial_stream_failure_is_a_model_failure_without_an_assistant_item() {
    let model = Arc::new(ScriptedModel {
        descriptor: descriptor("fake/scripted"),
        rounds: Arc::new(vec![Round::Malformed {
            events: vec![ModelEvent::text_delta("partial").unwrap()],
        }]),
    });
    // The ScriptedModel helper streams only Ok events above; build a stream
    // that fails after the first delta.
    struct FailingModel {
        descriptor: ModelDescriptor,
    }
    impl Model for FailingModel {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }
        fn start<'a>(
            &'a self,
            _request: ModelRequest,
            _context: ModelCallContext,
        ) -> ModelStartFuture<'a> {
            Box::pin(async move {
                Ok::<ModelStream, ModelError>(Box::pin(stream::iter(vec![
                    Ok(ModelEvent::text_delta("partial").unwrap()),
                    Err(ModelError::started(
                        ModelErrorKind::Unavailable,
                        DiagnosticSummary::new(
                            DiagnosticCode::ModelUnavailable,
                            DiagnosticCategory::Model,
                            BoundedText::new("stream died").unwrap(),
                            false,
                        ),
                    )),
                ])))
            })
        }
    }
    let _ = model;
    let failing = Arc::new(FailingModel {
        descriptor: descriptor("fake/failing"),
    });
    let agent = start_with(
        request(config(failing, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    match &report.outcome {
        LoopOutcome::Failed(failure) => assert_eq!(failure.kind, LoopFailureKind::Model),
        other => panic!("expected Model failure, got {other:?}"),
    }
    assert!(
        report
            .appended
            .iter()
            .all(|item| !matches!(item, HistoryItem::Assistant(_)))
    );
}

/// FIX-06-T05: The delivery-aware driver retries at the loop level: a first NotStarted
/// error is retried and the loop completes on the second attempt. Single RequestStarted,
/// report.requests = 1, attempts = 2.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn delivery_retry_recovers_at_the_loop_level() {
    struct RetryOnceModel {
        descriptor: ModelDescriptor,
        attempts: AtomicUsize,
        started: Arc<Notify>,
    }
    impl Model for RetryOnceModel {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }
        fn start<'a>(
            &'a self,
            _request: ModelRequest,
            _context: ModelCallContext,
        ) -> ModelStartFuture<'a> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            let started = Arc::clone(&self.started);
            Box::pin(async move {
                if attempt == 0 {
                    started.notify_one();
                    Err(ModelError::not_started(
                        ModelErrorKind::Unavailable,
                        None,
                        DiagnosticSummary::new(
                            DiagnosticCode::ModelUnavailable,
                            DiagnosticCategory::Model,
                            BoundedText::new("retry me").unwrap(),
                            true,
                        ),
                    ))
                } else {
                    Ok::<ModelStream, ModelError>(Box::pin(stream::iter(vec![
                        Ok(ModelEvent::text_delta("recovered").unwrap()),
                        Ok(ModelEvent::Finish {
                            reason: ModelFinishReason::Stop,
                        }),
                    ])))
                }
            })
        }
    }
    let started = Arc::new(Notify::new());
    let model = Arc::new(RetryOnceModel {
        descriptor: descriptor("fake/retry"),
        attempts: AtomicUsize::new(0),
        started: Arc::clone(&started),
    });
    let config_model: Arc<dyn Model> = model.clone();
    let mut agent = start_with(
        request(config(config_model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let report_task = tokio::spawn(async move { agent.join().await });

    // First attempt fails immediately; the driver sleeps one base delay
    // (100 ms default) before retrying. Advance past it deterministically.
    started.notified().await;
    tokio::time::advance(Duration::from_millis(250)).await;
    let report = report_task.await.unwrap().unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 1);
    assert_eq!(model.attempts.load(Ordering::SeqCst), 2);

    let mut request_started_count = 0;
    while let Some(envelope) = events.recv().await {
        if matches!(envelope.event, LoopEvent::RequestStarted { .. }) {
            request_started_count += 1;
        }
    }
    assert_eq!(
        request_started_count, 1,
        "driver retry must emit only one RequestStarted event"
    );
    let has_recovered = report.appended.iter().any(|item| {
        matches!(
            item,
            HistoryItem::Assistant(assistant)
                if assistant.content.iter().any(|part| matches!(part, minicore_runtime::model::AssistantPart::Text(text) if text == "recovered"))
        )
    });
    assert!(
        has_recovered,
        "the retried request must be the one recorded"
    );
}

/// Two loops sharing one Model instance and one ToolSet isolate cancellation:
/// cancelling one leaves the other completing, and a fresh loop still runs.
#[tokio::test]
async fn shared_model_and_toolset_loops_cancel_isolated_without_orphans() {
    let gate = Arc::new(Notify::new());
    struct GatedHoldModel {
        descriptor: ModelDescriptor,
        gate: Arc<Notify>,
    }
    impl Model for GatedHoldModel {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }
        fn start<'a>(
            &'a self,
            _request: ModelRequest,
            _context: ModelCallContext,
        ) -> ModelStartFuture<'a> {
            let gate = Arc::clone(&self.gate);
            Box::pin(async move {
                gate.notified().await;
                Ok::<ModelStream, ModelError>(Box::pin(stream::iter(vec![
                    Ok(ModelEvent::text_delta("shared").unwrap()),
                    Ok(ModelEvent::Finish {
                        reason: ModelFinishReason::Stop,
                    }),
                ])))
            })
        }
    }
    let shared_model: Arc<dyn Model> = Arc::new(GatedHoldModel {
        descriptor: descriptor("fake/shared"),
        gate: Arc::clone(&gate),
    });
    let shared_tools = echo_toolset().0;

    let mut agent_a = start_with(
        request(config(
            Arc::clone(&shared_model),
            shared_tools.clone(),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut agent_b = start_with(
        request(config(Arc::clone(&shared_model), shared_tools, None)),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();

    let mut events_a = agent_a.take_events().unwrap();
    let mut events_b = agent_b.take_events().unwrap();
    let handle_a = agent_a.handle().clone();
    let report_a = tokio::spawn(async move { agent_a.join().await });
    let report_b = tokio::spawn(async move { agent_b.join().await });

    wait_for_request(&mut events_a, 0).await;
    wait_for_request(&mut events_b, 0).await;

    assert!(handle_a.cancel(), "loop A must accept cancel");
    gate.notify_waiters();

    let report_a = report_a.await.unwrap().unwrap();
    let report_b = report_b.await.unwrap().unwrap();
    assert_eq!(
        report_a.outcome,
        LoopOutcome::Cancelled(minicore_runtime::CancelReason::User)
    );
    assert_eq!(report_b.outcome, LoopOutcome::Completed);

    // A fresh loop after the concurrent pair proves no runner task leaked.
    let fresh = start_with(
        request(config(
            Arc::new(ScriptedModel {
                descriptor: descriptor("fake/scripted"),
                rounds: Arc::new(vec![Round::Final { text: "fresh" }]),
            }),
            ToolSet::default(),
            None,
        )),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let fresh_report = fresh.join().await.unwrap();
    assert_eq!(fresh_report.outcome, LoopOutcome::Completed);
}

/// A steer applied at a request boundary survives a later prompt failure and
/// appears in the report as a Steering item.
#[tokio::test]
async fn steer_applied_before_a_prompt_failure_stays_in_the_report() {
    struct FailSecondPrompt {
        calls: Arc<AtomicUsize>,
    }
    impl PromptProvider for FailSecondPrompt {
        fn prepare<'a>(&'a self, _request: PromptRequest<'a>) -> PromptFuture<'a> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call >= 1 {
                    return Err(PromptError::InvalidHistory);
                }
                Ok(PreparedPrompt {
                    messages: vec![ModelMessage::user("first prompt").unwrap()],
                })
            })
        }
    }
    let gate = Arc::new(Notify::new());
    struct GatedFinalModel {
        descriptor: ModelDescriptor,
        gate: Arc<Notify>,
    }
    impl Model for GatedFinalModel {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }
        fn start<'a>(
            &'a self,
            _request: ModelRequest,
            _context: ModelCallContext,
        ) -> ModelStartFuture<'a> {
            let gate = Arc::clone(&self.gate);
            Box::pin(async move {
                gate.notified().await;
                Ok::<ModelStream, ModelError>(Box::pin(stream::iter(vec![
                    Ok(ModelEvent::text_delta("first answer").unwrap()),
                    Ok(ModelEvent::Finish {
                        reason: ModelFinishReason::Stop,
                    }),
                ])))
            })
        }
    }
    let prompt = Arc::new(FailSecondPrompt {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let mut agent = start_with(
        LoopRequest::new(
            Arc::from(Vec::<HistoryItem>::new()),
            UserInput::text("hello").unwrap(),
            ExecutionConfig::new(
                Arc::new(GatedFinalModel {
                    descriptor: descriptor("fake/gated-final"),
                    gate: Arc::clone(&gate),
                }) as Arc<dyn Model>,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                prompt,
            )
            .unwrap(),
        ),
        LoopOptions::default_checked().unwrap(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let handle = agent.handle().clone();
    let report_task = tokio::spawn(async move { agent.join().await });

    // Request 0 is in flight (gated); steer lands for the next boundary.
    wait_for_request(&mut events, 0).await;
    handle
        .steer(UserInput::text("focus").unwrap())
        .expect("steer accepted while live");
    gate.notify_one();

    let report = report_task.await.unwrap().unwrap();
    match &report.outcome {
        LoopOutcome::Failed(failure) => {
            assert_eq!(failure.kind, LoopFailureKind::Prompt)
        }
        other => panic!("expected Prompt failure, got {other:?}"),
    }
    let has_steering = report.appended.iter().any(|item| {
        matches!(
            item,
            HistoryItem::User(user) if user.kind == UserMessageKind::Steering
        )
    });
    assert!(
        has_steering,
        "the applied steer must be in the failed report"
    );
}

/// Waits until a request at `index` has actually been issued by the runner.
async fn wait_for_request(events: &mut minicore_runtime::LoopEventStream, index: u32) {
    while let Some(envelope) = events.recv().await {
        if matches!(envelope.event, LoopEvent::RequestStarted { request_index: i, .. } if i == index)
        {
            return;
        }
    }
    panic!("loop ended before request {index}");
}

/// FIX-01-T04: Registering multiple tools succeeds in AgentLoop when max_tool_calls_per_response is 1.
#[tokio::test]
async fn loop_completes_with_multiple_registered_tools_under_single_call_limit() {
    let mut builder = ToolSet::builder();
    builder.register(RecordingTool {
        spec: ToolSpec::new(
            "lookup".parse().unwrap(),
            "lookup tool",
            json!({"type": "object"}),
        )
        .unwrap(),
        calls: Arc::new(Mutex::new(Vec::new())),
        behavior: ToolBehavior::Succeed,
    });
    builder.register(RecordingTool {
        spec: ToolSpec::new(
            "search".parse().unwrap(),
            "search tool",
            json!({"type": "object"}),
        )
        .unwrap(),
        calls: Arc::new(Mutex::new(Vec::new())),
        behavior: ToolBehavior::Succeed,
    });
    let tools = builder.build().unwrap();

    let mut options = LoopOptions::default_checked().unwrap();
    options.limits.max_tool_calls_per_response = 1;

    let agent = start_with(
        request(config(
            model(vec![Round::Final { text: "completed" }]),
            tools,
            None,
        )),
        options,
    )
    .unwrap();
    let report = agent.join().await.unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(report.requests, 1);
}

/// FIX-01-T05: Model timeout exceeding 24 hours fails in AgentLoop::start.
#[tokio::test]
async fn loop_rejects_model_timeout_exceeding_maximum_on_start() {
    let mut options = LoopOptions::default_checked().unwrap();
    options.model_timeout = Duration::from_secs(24 * 60 * 60 + 1);

    let result = start_with(
        request(config(
            model(vec![Round::Final { text: "completed" }]),
            ToolSet::default(),
            None,
        )),
        options,
    );
    let Err(error) = result else {
        panic!("loop start unexpectedly succeeded with model timeout exceeding 24 hours");
    };
    assert_eq!(error, LoopStartError::InvalidOptions);
}

/// FIX-01-T06: Model retry base delay exceeding 30 seconds fails in AgentLoop::start.
#[tokio::test]
async fn loop_rejects_model_retry_delay_exceeding_maximum_on_start() {
    let mut options = LoopOptions::default_checked().unwrap();
    options.model_retry_base_delay = Duration::from_secs(31);

    let result = start_with(
        request(config(
            model(vec![Round::Final { text: "completed" }]),
            ToolSet::default(),
            None,
        )),
        options,
    );
    let Err(error) = result else {
        panic!("loop start unexpectedly succeeded with retry delay exceeding 30 seconds");
    };
    assert_eq!(error, LoopStartError::InvalidOptions);
}
