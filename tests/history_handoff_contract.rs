mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::ModelFinishReason;
use minicore_runtime::prompt::{PromptError, PromptFuture, PromptProvider, PromptRequest};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    AgentLoop, BoundedText, CancelReason, HistoryFuture, HistoryItem, LoopFailureKind, LoopHistory,
    LoopId, LoopOptions, LoopOutcome, SummaryHistory,
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
                    BoundedText::new("test save failed").unwrap(),
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
        content: BoundedText::new("base is never redelivered").unwrap(),
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
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(report.appended.len(), 2);
    assert_eq!(history.deliveries.lock().unwrap().len(), 1);
    assert_eq!(history.saved.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_tool_result_handoff_preserves_side_effects_and_stops_next_request() {
    let (history, _receiver) = recorder(false, Some(1), None);
    let (model, attempts, _) = scripted_model_counted(vec![Round::Tools {
        calls: vec![("echo", "first"), ("echo", "second")],
    }]);
    let (agent, calls) = start_echo(model, ToolBehavior::Succeed, options(history.clone()));
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::History)
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(report.appended.len(), 4);
    assert_eq!(history.deliveries.lock().unwrap().len(), 2);
    assert_eq!(history.saved.load(Ordering::SeqCst), 2);
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
    for drop_owner in [false, true] {
        let (history, mut receiver) = recorder(true, None, None);
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
        assert_eq!(history.saved.load(Ordering::SeqCst), delivered.items.len());
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
    assert!(
        matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == LoopFailureKind::History)
    );
    assert_eq!(report.appended.len(), 2);
    assert_eq!(history.deliveries.lock().unwrap().len(), 1);
}
