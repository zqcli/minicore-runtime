mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{ModelEvent, ModelFinishReason, ProviderReplay, Usage};
use minicore_runtime::prompt::{PromptError, PromptFuture, PromptProvider, PromptRequest};
use minicore_runtime::tools::{ToolResultOutcome, ToolSet};
use minicore_runtime::{
    AgentLoop, CancelReason, HistoryFuture, HistoryItem, LoopFailureKind, LoopHistory, LoopId,
    LoopOptions, LoopOutcome, SummaryHistory, Text,
};
use tokio::sync::{Semaphore, mpsc};

use support::loops::*;

#[derive(Clone, Debug)]
struct Delivery {
    loop_id: LoopId,
    offset: usize,
    items: Vec<HistoryItem>,
}

struct RecordingHistory {
    deliveries: Mutex<Vec<Delivery>>,
    entered: mpsc::UnboundedSender<Delivery>,
    gate: Option<Semaphore>,
    fail_call: Option<usize>,
    panic_call: Option<usize>,
    saved: AtomicUsize,
}

impl LoopHistory for RecordingHistory {
    fn append<'a>(
        &'a self,
        loop_id: LoopId,
        offset: usize,
        items: &'a [HistoryItem],
    ) -> HistoryFuture<'a> {
        Box::pin(async move {
            let delivery = Delivery {
                loop_id,
                offset,
                items: items.to_vec(),
            };
            let index = {
                let mut deliveries = self.deliveries.lock().unwrap();
                let index = deliveries.len();
                deliveries.push(delivery.clone());
                index
            };
            let _ = self.entered.send(delivery);
            if let Some(gate) = &self.gate {
                gate.acquire().await.unwrap().forget();
            }
            assert_ne!(self.panic_call, Some(index), "test callback panic");
            if self.fail_call == Some(index) {
                return Err(DiagnosticSummary::new(
                    DiagnosticCode::Internal,
                    DiagnosticCategory::Internal,
                    Text::new("test save failed"),
                    false,
                )
                .unwrap());
            }
            self.saved.fetch_add(items.len(), Ordering::SeqCst);
            Ok(())
        })
    }
}

fn recorder(
    gated: bool,
    fail_call: Option<usize>,
    panic_call: Option<usize>,
) -> (Arc<RecordingHistory>, mpsc::UnboundedReceiver<Delivery>) {
    let (entered, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(RecordingHistory {
            deliveries: Mutex::new(Vec::new()),
            entered,
            gate: gated.then(|| Semaphore::new(0)),
            fail_call,
            panic_call,
            saved: AtomicUsize::new(0),
        }),
        receiver,
    )
}

fn options(history: Arc<RecordingHistory>) -> LoopOptions {
    let mut options = LoopOptions::default_checked().unwrap();
    options.history = Some(history);
    options.event_capacity = 1;
    options
}

async fn next(receiver: &mut mpsc::UnboundedReceiver<Delivery>) -> Delivery {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn incremental_handoffs_preserve_order_and_gate_tools_and_next_model() {
    let (history, mut receiver) = recorder(true, None, None);
    let (model, attempts, _) = scripted_model_counted(vec![
        Round::Tools {
            calls: vec![("echo", "first"), ("echo", "second")],
        },
        Round::Final { text: "done" },
    ]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let inputs = QueuedInputs::new();
    inputs.push("already bound");
    let mut request = request(config(
        model,
        tool_set(calls.clone(), ToolBehavior::Succeed),
        None,
    ));
    request.history = Arc::from([HistoryItem::Summary(SummaryHistory {
        content: Text::new("base is never redelivered"),
    })]);
    let agent = AgentLoop::start(request, options(history.clone()), inputs.clone()).unwrap();
    let id = agent.id();
    let first = next(&mut receiver).await;
    assert_eq!((first.loop_id, first.offset, first.items.len()), (id, 0, 3));
    assert!(matches!(
        &first.items[..],
        [
            HistoryItem::User(_),
            HistoryItem::User(_),
            HistoryItem::Assistant(_)
        ]
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(calls.lock().unwrap().is_empty());
    history.gate.as_ref().unwrap().add_permits(1);

    let second = next(&mut receiver).await;
    assert_eq!(
        (second.loop_id, second.offset, second.items.len()),
        (id, 3, 2)
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    for (item, expected) in second.items.iter().zip(["first", "second"]) {
        let HistoryItem::ToolResult(result) = item else {
            panic!("expected tool result")
        };
        assert_eq!(result.call_id.as_str(), expected);
    }
    inputs.push("next request only");
    history.gate.as_ref().unwrap().add_permits(1);

    let third = next(&mut receiver).await;
    assert_eq!((third.loop_id, third.offset, third.items.len()), (id, 5, 2));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    history.gate.as_ref().unwrap().add_permits(1);
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert!(report.history_failure.is_none());
    let delivered: Vec<_> = history
        .deliveries
        .lock()
        .unwrap()
        .iter()
        .flat_map(|delivery| delivery.items.clone())
        .collect();
    assert_eq!(&delivered, report.appended.as_ref());
    assert_eq!(history.deliveries.lock().unwrap().len(), 3);
    assert_eq!(history.saved.load(Ordering::SeqCst), report.appended.len());
}

#[tokio::test]
async fn failed_response_handoff_never_starts_tools_or_retries_accepted_range() {
    let (history, _receiver) = recorder(false, Some(0), None);
    let (model, attempts, _) = scripted_model_counted(vec![Round::Tools {
        calls: vec![("echo", "first")],
    }]);
    let (agent, calls) = start_echo(model, ToolBehavior::Succeed, options(history.clone()));
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::History && failure.model_error().is_none())
    );
    assert_save_failure(&report);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(report.appended.len(), 2);
    assert_eq!(history.deliveries.lock().unwrap().len(), 1);
    assert_eq!(history.saved.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_tool_result_handoff_preserves_side_effects_and_stops_next_request() {
    for (behavior, expected_result) in [
        (ToolBehavior::Succeed, ToolResultOutcome::Success),
        (ToolBehavior::Fail, ToolResultOutcome::Failed),
    ] {
        let (history, _receiver) = recorder(false, Some(1), None);
        let (model, attempts, _) = scripted_model_counted(vec![Round::Tools {
            calls: vec![("echo", "first"), ("echo", "second")],
        }]);
        let (agent, calls) = start_echo(model, behavior, options(history.clone()));
        let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::History)
        );
        assert_save_failure(&report);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(report.appended.len(), 4);
        assert!(
            tool_results(&report)
                .iter()
                .all(|result| result.outcome == expected_result)
        );
        assert_eq!(history.deliveries.lock().unwrap().len(), 2);
        assert_eq!(history.saved.load(Ordering::SeqCst), 2);
    }
}

struct FailingPrompt;
impl PromptProvider for FailingPrompt {
    fn prepare<'a>(&'a self, _: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async { Err(PromptError::InvalidHistory) })
    }
}

#[tokio::test]
async fn final_unwind_hands_off_consumed_inputs_after_prompt_failure() {
    let (history, _receiver) = recorder(false, None, None);
    let (model, attempts, _) = scripted_model_counted(vec![]);
    let inputs = QueuedInputs::new();
    inputs.push("taken before failed prompt");
    let config = config_full(
        model,
        minicore_runtime::model::ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(FailingPrompt),
    );
    let agent = AgentLoop::start(request(config), options(history.clone()), inputs).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::Prompt)
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    let deliveries = history.deliveries.lock().unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].offset, 0);
    assert_eq!(&deliveries[0].items, report.appended.as_ref());
    assert_eq!(report.appended.len(), 2);
}

#[tokio::test]
async fn cancellation_and_owner_drop_do_not_drop_pending_history_save() {
    for (drop_owner, fail_save) in [(false, false), (true, false), (false, true), (true, true)] {
        let (history, mut receiver) = recorder(true, fail_save.then_some(0), None);
        let (model, _) = scripted_model(vec![Round::Final {
            text: "complete response",
        }]);
        let agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            options(history.clone()),
            no_inputs(),
        )
        .unwrap();
        let handle = agent.handle();
        let delivered = next(&mut receiver).await;
        let expected = if drop_owner {
            drop(agent);
            CancelReason::OwnerDropped
        } else {
            handle.cancel();
            drop(agent);
            CancelReason::User
        };
        assert_eq!(history.saved.load(Ordering::SeqCst), 0);
        assert!(futures_util::poll!(Box::pin(handle.wait())).is_pending());
        history.gate.as_ref().unwrap().add_permits(1);
        let report = tokio::time::timeout(Duration::from_secs(5), handle.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.outcome, LoopOutcome::Cancelled(expected));
        assert_eq!(
            history.saved.load(Ordering::SeqCst),
            if fail_save { 0 } else { delivered.items.len() }
        );
        if fail_save {
            assert_save_failure(&report);
        } else {
            assert!(report.history_failure.is_none());
        }
        let HistoryItem::Assistant(assistant) = &report.appended[1] else {
            panic!("complete response lost")
        };
        assert_eq!(assistant.finish_reason, ModelFinishReason::Stop);
        assert_eq!(history.deliveries.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn panicking_history_callback_preserves_full_report_without_retry() {
    let (history, _receiver) = recorder(false, None, Some(0));
    let (model, _) = scripted_model(vec![Round::Final { text: "retained" }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        options(history.clone()),
        no_inputs(),
    )
    .unwrap();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_eq!(
        report.history_failure.as_ref().unwrap().message().as_str(),
        "history callback panicked"
    );
    assert_eq!(report.appended.len(), 2);
    assert_eq!(history.deliveries.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unsupported_complete_finish_is_handed_off_before_failed_outcome() {
    let (history, _receiver) = recorder(false, None, None);
    let usage = Usage::new(3, 4, 1);
    let replay =
        ProviderReplay::new("test/v1", serde_json::json!({"opaque": "preserved"})).unwrap();
    let (model, _) = scripted_model(vec![Round::Events(vec![
        ModelEvent::text_delta("complete provider response"),
        ModelEvent::Usage { usage },
        ModelEvent::ProviderReplay {
            replay: replay.clone(),
        },
        ModelEvent::Finish {
            reason: ModelFinishReason::Unknown,
        },
    ])]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        options(history.clone()),
        no_inputs(),
    )
    .unwrap();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::InvalidModelResponse)
    );
    assert!(report.partial.is_none());
    let [HistoryItem::User(_), HistoryItem::Assistant(assistant)] = &report.appended[..] else {
        panic!("a complete response must be retained independently of the ending outcome");
    };
    assert_eq!(assistant.finish_reason, ModelFinishReason::Unknown);
    assert_eq!(assistant.usage, usage);
    assert_eq!(assistant.provider_replay.as_ref(), Some(&replay));
    let deliveries = history.deliveries.lock().unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].offset, 0);
    assert_eq!(&deliveries[0].items, report.appended.as_ref());
}

#[tokio::test]
async fn usage_overflow_retains_the_new_complete_response_before_failure() {
    let (history, _receiver) = recorder(false, None, None);
    let mut first = tool_call_events(vec![("echo", "once")]);
    first.insert(
        0,
        ModelEvent::Usage {
            usage: Usage::new(u64::MAX, 0, 0),
        },
    );
    let (model, attempts, _) = scripted_model_counted(vec![
        Round::Events(first),
        Round::Events(vec![
            ModelEvent::text_delta("complete response before usage overflow"),
            ModelEvent::Usage {
                usage: Usage::new(1, 0, 0),
            },
            ModelEvent::Finish {
                reason: ModelFinishReason::Stop,
            },
        ]),
    ]);
    let (agent, calls) = start_echo(model, ToolBehavior::Succeed, options(history.clone()));
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::Internal)
    );
    assert!(report.partial.is_none());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(report.appended.len(), 4);
    let HistoryItem::Assistant(assistant) = &report.appended[3] else {
        panic!("usage accumulation failure must not discard a complete response");
    };
    assert_eq!(assistant.request_index, 1);
    assert_eq!(assistant.finish_reason, ModelFinishReason::Stop);
    assert_eq!(assistant.usage, Usage::new(1, 0, 0));
    let deliveries = history.deliveries.lock().unwrap();
    assert_eq!(deliveries.len(), 3);
    assert_eq!(deliveries[2].offset, 3);
    assert_eq!(&deliveries[2].items[..], &report.appended[3..]);
}

fn assert_save_failure(report: &minicore_runtime::LoopReport) {
    assert_eq!(
        report.history_failure.as_ref().unwrap().message().as_str(),
        "test save failed"
    );
}

fn primary_model_error() -> minicore_runtime::model::ModelError {
    minicore_runtime::model::ModelError::started(
        minicore_runtime::model::ModelErrorKind::ProviderUnavailable,
        DiagnosticSummary::new(
            DiagnosticCode::ModelUnavailable,
            DiagnosticCategory::Model,
            Text::new("primary provider failure"),
            false,
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn primary_prompt_failure_survives_failed_history_save() {
    let (history, _receiver) = recorder(false, Some(0), None);
    let (model, attempts, _) = scripted_model_counted(vec![]);
    let config = config_full(
        model,
        minicore_runtime::model::ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(FailingPrompt),
    );
    let agent = AgentLoop::start(request(config), options(history.clone()), no_inputs()).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::Prompt && failure.diagnostic.code == DiagnosticCode::ContextFailed)
    );
    assert_save_failure(&report);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(report.appended.len(), 1);
    assert_eq!(history.deliveries.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn primary_model_and_assembly_errors_survive_failed_history_save() {
    for invalid_assembly in [false, true] {
        let (history, _receiver) = recorder(false, Some(0), None);
        let events = vec![ModelEvent::text_delta("unfinished output")];
        let round = if invalid_assembly {
            Round::Events(events)
        } else {
            Round::StreamError {
                events,
                error: primary_model_error(),
            }
        };
        let (model, attempts, _) = scripted_model_counted(vec![round]);
        let agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            options(history.clone()),
            no_inputs(),
        )
        .unwrap();
        let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
            .await
            .unwrap()
            .unwrap();
        let LoopOutcome::Failed(failure) = &report.outcome else {
            panic!("primary failure lost")
        };
        let expected = if invalid_assembly {
            LoopFailureKind::InvalidModelResponse
        } else {
            LoopFailureKind::Model
        };
        assert_eq!(failure.kind, expected);
        let model_error = failure
            .model_error()
            .expect("structured model error must survive saving failure");
        if invalid_assembly {
            assert_eq!(
                model_error.kind(),
                minicore_runtime::model::ModelErrorKind::IncompleteResponse
            );
        } else {
            assert_eq!(model_error, &primary_model_error());
        }
        assert_save_failure(&report);
        assert!(report.partial.is_some());
        assert_eq!(report.appended.len(), 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(history.deliveries.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn complete_response_failures_survive_failed_history_save() {
    for (reason, expected) in [
        (ModelFinishReason::Length, LoopFailureKind::OutputLimit),
        (ModelFinishReason::Refused, LoopFailureKind::Refused),
        (
            ModelFinishReason::ContentFiltered,
            LoopFailureKind::ContentFiltered,
        ),
        (
            ModelFinishReason::Unknown,
            LoopFailureKind::InvalidModelResponse,
        ),
    ] {
        let (history, _receiver) = recorder(false, Some(0), None);
        let (model, _) = scripted_model(vec![Round::Events(vec![
            ModelEvent::text_delta("complete but unsuccessful response"),
            ModelEvent::Finish { reason },
        ])]);
        let agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            options(history.clone()),
            no_inputs(),
        )
        .unwrap();
        let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == expected)
        );
        assert_save_failure(&report);
        assert!(report.partial.is_none());
        let HistoryItem::Assistant(assistant) = &report.appended[1] else {
            panic!("complete response lost")
        };
        assert_eq!(assistant.finish_reason, reason);
        assert_eq!(history.deliveries.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn completed_candidate_survives_failed_save_with_explicit_history_fault() {
    let (history, _receiver) = recorder(false, Some(0), None);
    let (model, _) = scripted_model(vec![Round::Final {
        text: "completed answer",
    }]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        options(history.clone()),
        no_inputs(),
    )
    .unwrap();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, LoopOutcome::Completed);
    assert_save_failure(&report);
    assert_eq!(report.appended.len(), 2);
    assert_eq!(history.saved.load(Ordering::SeqCst), 0);
    assert_eq!(history.deliveries.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn policy_failure_claim_survives_later_cancel_and_failed_batch_save() {
    let (history, mut receiver) = recorder(true, Some(1), None);
    let (model, attempts, _) = scripted_model_counted(vec![Round::Tools {
        calls: vec![("echo", "not-executed")],
    }]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let policy = Arc::new(ScriptedPolicy {
        plan: Arc::new(vec![PolicyPlan::Invalid]),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let agent = AgentLoop::start(
        request(config(
            model,
            tool_set(calls.clone(), ToolBehavior::Succeed),
            Some(policy),
        )),
        options(history.clone()),
        no_inputs(),
    )
    .unwrap();
    let first = next(&mut receiver).await;
    assert_eq!(first.offset, 0);
    history.gate.as_ref().unwrap().add_permits(1);
    let batch = next(&mut receiver).await;
    assert_eq!(batch.offset, 2);
    assert!(
        !agent.handle().cancel(),
        "known policy failure must win before saving awaits"
    );
    assert!(futures_util::poll!(Box::pin(agent.handle().wait())).is_pending());
    history.gate.as_ref().unwrap().add_permits(1);
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::Policy && failure.diagnostic.code == DiagnosticCode::PolicyFailed)
    );
    assert_save_failure(&report);
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(report.appended.len(), 3);
    assert_eq!(history.deliveries.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn failure_claim_survives_later_cancel_during_failed_response_or_final_save() {
    for complete in [false, true] {
        let (history, mut receiver) = recorder(true, Some(0), None);
        let round = if complete {
            Round::Events(vec![
                ModelEvent::text_delta("complete response"),
                ModelEvent::Finish {
                    reason: ModelFinishReason::Unknown,
                },
            ])
        } else {
            Round::StreamError {
                events: vec![ModelEvent::text_delta("partial")],
                error: primary_model_error(),
            }
        };
        let (model, _) = scripted_model(vec![round]);
        let agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            options(history.clone()),
            no_inputs(),
        )
        .unwrap();
        next(&mut receiver).await;
        assert!(
            !agent.handle().cancel(),
            "the original failure must be claimed before saving awaits"
        );
        history.gate.as_ref().unwrap().add_permits(1);
        let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
            .await
            .unwrap()
            .unwrap();
        let expected = if complete {
            LoopFailureKind::InvalidModelResponse
        } else {
            LoopFailureKind::Model
        };
        assert!(
            matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == expected && failure.model_error().is_some())
        );
        assert_save_failure(&report);
    }
}

#[tokio::test]
async fn cancellation_claim_survives_failed_final_save() {
    let (history, mut receiver) = recorder(true, Some(0), None);
    let (model, started) = scripted_model(vec![Round::Hold]);
    let notified = started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        options(history.clone()),
        no_inputs(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), notified)
        .await
        .unwrap();
    assert!(agent.handle().cancel());
    next(&mut receiver).await;
    history.gate.as_ref().unwrap().add_permits(1);
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    assert_save_failure(&report);
    assert!(report.partial.is_none());
    assert_eq!(report.appended.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn deadline_claim_survives_later_cancel_and_failed_final_save() {
    let (history, mut receiver) = recorder(true, Some(0), None);
    let (model, started) = scripted_model(vec![Round::Hold]);
    let notified = started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let mut options = options(history.clone());
    options.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        options,
        no_inputs(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), notified)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    next(&mut receiver).await;
    assert!(
        !agent.handle().cancel(),
        "deadline was claimed before the save callback"
    );
    history.gate.as_ref().unwrap().add_permits(1);
    let report = tokio::time::timeout(Duration::from_secs(1), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(CancelReason::Deadline)
    );
    assert_save_failure(&report);
}
