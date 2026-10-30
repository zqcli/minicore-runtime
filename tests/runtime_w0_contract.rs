//! Runtime observation and request-boundary contracts for hosts.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelMessage, ModelRequest, ModelStartFuture,
    ReasoningPreference,
};
use minicore_runtime::prompt::{
    DefaultPromptProvider, PromptFuture, PromptProvider, PromptRequest,
};
use minicore_runtime::tools::{ApprovalDecision, ToolInputAnswer, ToolInputAnswerKind, ToolSet};
use minicore_runtime::{
    AgentLoop, CancelReason, HistoryItem, InteractionAnswer, InteractionKind, LoopEvent,
    LoopHandle, LoopInputs, LoopOptions, LoopOutcome, LoopStatus, UserInput, UserMessageKind,
};
use support::loops::*;
use tokio::sync::Notify;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn late_watch_observes_actionable_interaction_without_its_event() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        for approval in [true, false] {
            let (model, _) = scripted_model(vec![
                Round::Tools {
                    calls: vec![("echo", "call_1")],
                },
                Round::Final { text: "done" },
            ]);
            let policy = approval.then(|| {
                Arc::new(ScriptedPolicy {
                    plan: Arc::new(vec![PolicyPlan::RequireApproval]),
                    calls: Arc::new(AtomicUsize::new(0)),
                }) as Arc<dyn minicore_runtime::tools::ToolPolicy>
            });
            let behavior = if approval {
                ToolBehavior::Succeed
            } else {
                ToolBehavior::RequestChoiceInput
            };
            let mut options = LoopOptions::default_checked().unwrap();
            options.event_capacity = 1;
            let mut agent = AgentLoop::start(
                request(config(model, tool_set(Arc::default(), behavior), policy)),
                options,
                no_inputs(),
            )
            .unwrap();
            let handle = agent.handle();
            let mut events = agent.take_events().unwrap();
            let mut observer = handle.watch_state();
            observer
                .wait_for(|state| state.status == LoopStatus::WaitingForInput)
                .await
                .unwrap();

            let mut late = handle.watch_state();
            let state = late.borrow_and_update().clone();
            assert_eq!(state, handle.state());
            assert_eq!(state.loop_id, handle.id());
            assert_eq!(state.request_index, 0);
            assert_eq!(state.status, LoopStatus::WaitingForInput);
            assert!(!late.has_changed().unwrap());
            let pending = state.pending_interaction.unwrap();
            assert_eq!(pending.tool_call_id.as_str(), "call_1");
            assert_eq!(pending.tool_name.as_str(), "echo");
            let answer = match &pending.kind {
                InteractionKind::Approval(request) if approval => {
                    assert_eq!(request.prompt.as_str(), "approve this call?");
                    InteractionAnswer::Approval(ApprovalDecision::AllowOnce)
                }
                InteractionKind::ToolInput(request) if !approval => {
                    assert_eq!(request.prompt.as_str(), "choose the option");
                    assert_eq!(request.answer_kind, ToolInputAnswerKind::SingleChoice);
                    assert_eq!(
                        request
                            .choices
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>(),
                        ["alpha", "beta"]
                    );
                    InteractionAnswer::ToolInput(ToolInputAnswer::Choice { index: 1 })
                }
                other => panic!("unexpected interaction: {other:?}"),
            };
            handle.answer(pending.interaction_id, answer).unwrap();
            let report = agent.join().await.unwrap();
            assert_completed_with_text(&report, "done");

            let only = events.recv().await.unwrap();
            assert!(matches!(only.event, LoopEvent::Started { .. }));
            assert_eq!(only.dropped_before, 0);
            assert!(events.recv().await.is_none());
        }
    })
    .await
    .expect("late watch test timed out");
}

#[tokio::test]
async fn state_sender_stays_open_after_join_until_the_handle_is_dropped() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let (model, _) = scripted_model(vec![Round::Final { text: "done" }]);
        let agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            LoopOptions::default_checked().unwrap(),
            no_inputs(),
        )
        .unwrap();
        let handle = agent.handle();
        let mut states = handle.watch_state();
        let report = agent.join().await.unwrap();
        assert_completed_with_text(&report, "done");
        assert!(Arc::ptr_eq(&report, &handle.wait().await.unwrap()));
        assert_eq!(states.borrow_and_update().status, LoopStatus::Finished);
        assert!(!states.has_changed().expect("state sender remains open"));
        assert!(states.changed().now_or_never().is_none());

        drop(handle);
        assert!(states.changed().await.is_err());
    })
    .await
    .expect("state sender lifetime test timed out");
}

#[tokio::test]
async fn final_state_and_report_survive_unreported_terminal_event_loss() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let (model, _) = scripted_model(vec![Round::BurstDeltas {
            count: 4,
            chunk: "part",
        }]);
        let mut options = LoopOptions::default_checked().unwrap();
        options.event_capacity = 1;
        let mut agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            options,
            no_inputs(),
        )
        .unwrap();
        let handle = agent.handle();
        let mut states = handle.watch_state();
        let mut events = agent.take_events().unwrap();
        let report = agent.join().await.unwrap();

        assert_completed_with_text(&report, "partpartpartpart");
        assert_eq!(report.requests, 1);
        assert!(Arc::ptr_eq(&report, &handle.wait().await.unwrap()));
        let final_state = states.borrow_and_update().clone();
        assert_eq!(final_state, handle.state());
        assert_eq!(final_state.loop_id, report.loop_id);
        assert_eq!(final_state.status, LoopStatus::Finished);
        assert_eq!(final_state.request_index, 0);
        assert!(final_state.pending_interaction.is_none());

        let only = events.recv().await.unwrap();
        assert!(matches!(only.event, LoopEvent::Started { .. }));
        assert_eq!(only.dropped_before, 0);
        assert!(events.recv().await.is_none());
    })
    .await
    .expect("terminal event loss test timed out");
}

#[derive(Debug, PartialEq)]
struct Delivery {
    id: &'static str,
    candidate_index: u32,
    observed_state: Option<(LoopStatus, u32)>,
}

#[derive(Default)]
struct BoundaryProbe {
    next_index: AtomicU32,
    queue: Mutex<VecDeque<(&'static str, UserInput)>>,
    deliveries: Mutex<Vec<Delivery>>,
    issued: Mutex<Vec<(u32, Vec<String>)>>,
    handle: Mutex<Option<LoopHandle>>,
    cancel_on_delivery: AtomicBool,
}

impl BoundaryProbe {
    fn push(&self, id: &'static str) {
        self.queue
            .lock()
            .unwrap()
            .push_back((id, UserInput::text("identical text").unwrap()));
    }
}

impl LoopInputs for BoundaryProbe {
    fn take(&self) -> Vec<UserInput> {
        let taken: Vec<_> = self.queue.lock().unwrap().drain(..).collect();
        if taken.is_empty() {
            return Vec::new();
        }
        let handle = self.handle.lock().unwrap().clone();
        let observed_state = handle.as_ref().map(|handle| {
            let state = handle.state();
            (state.status, state.request_index)
        });
        let candidate_index = self.next_index.load(Ordering::SeqCst);
        self.deliveries
            .lock()
            .unwrap()
            .extend(taken.iter().map(|(id, _)| Delivery {
                id,
                candidate_index,
                observed_state,
            }));
        if self.cancel_on_delivery.swap(false, Ordering::SeqCst) {
            assert!(handle.unwrap().cancel());
        }
        taken.into_iter().map(|(_, input)| input).collect()
    }
}

struct IndexModel {
    inner: Arc<dyn Model>,
    probe: Arc<BoundaryProbe>,
}

impl Model for IndexModel {
    fn descriptor(&self) -> &ModelDescriptor {
        self.inner.descriptor()
    }

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let users = request
            .messages()
            .iter()
            .filter_map(|message| match message {
                ModelMessage::User(text) => Some(text.to_string()),
                _ => None,
            })
            .collect();
        self.probe
            .issued
            .lock()
            .unwrap()
            .push((context.request_index, users));
        // There is no take during the request or its tool batch.
        self.probe
            .next_index
            .store(context.request_index.saturating_add(1), Ordering::SeqCst);
        self.inner.start(request, context)
    }
}

struct IndexPrompt {
    probe: Arc<BoundaryProbe>,
    gate_index: u32,
    gated: AtomicBool,
    entered: Arc<Notify>,
    gate: Arc<Notify>,
    seen: Mutex<Vec<(u32, usize)>>,
}

impl PromptProvider for IndexPrompt {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
        self.probe
            .next_index
            .store(request.request_index, Ordering::SeqCst);
        let steering_count = request
            .history
            .appended()
            .iter()
            .filter(|item| {
                matches!(item, HistoryItem::User(user) if user.kind == UserMessageKind::Steering)
            })
            .count();
        self.seen
            .lock()
            .unwrap()
            .push((request.request_index, steering_count));
        let gate =
            request.request_index == self.gate_index && !self.gated.swap(true, Ordering::SeqCst);
        Box::pin(async move {
            if gate {
                self.entered.notify_one();
                self.gate.notified().await;
            }
            DefaultPromptProvider::new(None).prepare(request).await
        })
    }
}

fn index_prompt(probe: Arc<BoundaryProbe>, gate_index: u32) -> Arc<IndexPrompt> {
    Arc::new(IndexPrompt {
        probe,
        gate_index,
        gated: AtomicBool::new(false),
        entered: Arc::new(Notify::new()),
        gate: Arc::new(Notify::new()),
        seen: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn input_identity_and_candidate_index_can_be_mapped_across_request_boundaries() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let probe = Arc::new(BoundaryProbe::default());
        probe.push("early");
        let prompt = index_prompt(probe.clone(), 1);
        let model_gate = Arc::new(Notify::new());
        let (inner, _) = scripted_model(vec![
            Round::GatedTools {
                calls: vec![("echo", "call_1")],
                gate: model_gate.clone(),
            },
            Round::Final { text: "done" },
        ]);
        let model = Arc::new(IndexModel {
            inner,
            probe: probe.clone(),
        });
        let agent = AgentLoop::start(
            request(config_full(
                model,
                ReasoningPreference::Auto,
                tool_set(Arc::default(), ToolBehavior::Succeed),
                None,
                prompt.clone(),
            )),
            LoopOptions::default_checked().unwrap(),
            probe.clone(),
        )
        .unwrap();
        let handle = agent.handle();
        handle
            .watch_state()
            .wait_for(|state| state.status == LoopStatus::RunningModel)
            .await
            .unwrap();
        *probe.handle.lock().unwrap() = Some(handle);
        probe.push("after-tool");
        model_gate.notify_one();
        prompt.entered.notified().await;
        probe.push("during-prompt");
        prompt.gate.notify_one();

        let report = agent.join().await.unwrap();
        assert_completed_with_text(&report, "done");
        assert_eq!(report.requests, 2);
        assert_eq!(
            *probe.deliveries.lock().unwrap(),
            [
                Delivery {
                    id: "early",
                    candidate_index: 0,
                    observed_state: None,
                },
                Delivery {
                    id: "after-tool",
                    candidate_index: 1,
                    observed_state: Some((LoopStatus::RunningTools, 0)),
                },
                Delivery {
                    id: "during-prompt",
                    candidate_index: 1,
                    observed_state: Some((LoopStatus::Starting, 1)),
                },
            ]
        );
        assert_eq!(*prompt.seen.lock().unwrap(), [(0, 1), (1, 2), (1, 3)]);
        let issued = probe.issued.lock().unwrap();
        assert_eq!(issued.len(), 2);
        assert_eq!(issued[0].0, 0);
        assert_eq!(issued[0].1, ["Fix the parser", "identical text"]);
        assert_eq!(issued[1].0, 1);
        assert_eq!(
            issued[1].1,
            [
                "Fix the parser",
                "identical text",
                "identical text",
                "identical text"
            ]
        );
        let steering: Vec<_> = report
            .appended
            .iter()
            .filter_map(|item| match item {
                HistoryItem::User(user) if user.kind == UserMessageKind::Steering => {
                    Some(user.input.as_text())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            steering,
            ["identical text", "identical text", "identical text"]
        );
    })
    .await
    .expect("boundary mapping test timed out");
}

#[tokio::test]
async fn taken_input_can_have_a_candidate_index_without_an_issued_request() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let probe = Arc::new(BoundaryProbe::default());
        let prompt = index_prompt(probe.clone(), 0);
        let (inner, _) = scripted_model(vec![Round::Final { text: "never" }]);
        let model = Arc::new(IndexModel {
            inner,
            probe: probe.clone(),
        });
        let agent = AgentLoop::start(
            request(config_full(
                model,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                prompt.clone(),
            )),
            LoopOptions::default_checked().unwrap(),
            probe.clone(),
        )
        .unwrap();
        *probe.handle.lock().unwrap() = Some(agent.handle());
        prompt.entered.notified().await;
        probe.push("cancelled-before-issue");
        probe.cancel_on_delivery.store(true, Ordering::SeqCst);
        prompt.gate.notify_one();

        let report = agent.join().await.unwrap();
        assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
        assert_eq!(report.requests, 0);
        assert!(probe.issued.lock().unwrap().is_empty());
        assert_eq!(*prompt.seen.lock().unwrap(), [(0, 0)]);
        assert_eq!(
            *probe.deliveries.lock().unwrap(),
            [Delivery {
                id: "cancelled-before-issue",
                candidate_index: 0,
                observed_state: Some((LoopStatus::Starting, 0)),
            }]
        );
        assert!(report.appended.iter().any(|item| {
            matches!(item, HistoryItem::User(user) if user.kind == UserMessageKind::Steering)
        }));
    })
    .await
    .expect("unissued input test timed out");
}

#[tokio::test]
async fn final_answer_does_not_consume_or_extend_for_a_pending_steer() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let gate = Arc::new(Notify::new());
        let (model, _) = scripted_model(vec![Round::GatedFinal {
            text: "done",
            gate: gate.clone(),
        }]);
        let inputs = QueuedInputs::new();
        let agent = AgentLoop::start(
            request(config(model, ToolSet::default(), None)),
            LoopOptions::default_checked().unwrap(),
            inputs.clone(),
        )
        .unwrap();
        agent
            .handle()
            .watch_state()
            .wait_for(|state| state.status == LoopStatus::RunningModel)
            .await
            .unwrap();
        inputs.push("remain pending");
        gate.notify_one();

        let report = agent.join().await.unwrap();
        assert_completed_with_text(&report, "done");
        assert_eq!(report.requests, 1);
        assert_eq!(inputs.pending(), 1);
        assert!(!report.appended.iter().any(|item| {
            matches!(item, HistoryItem::User(user) if user.kind == UserMessageKind::Steering)
        }));
    })
    .await
    .expect("final steer test timed out");
}
