//! Runtime observation and request-boundary contracts for hosts.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind, ModelMessage,
    ModelRequest, ModelStartFuture, ModelStream, ReasoningPreference,
};
use minicore_runtime::prompt::{
    DefaultPromptProvider, PromptFuture, PromptProvider, PromptRequest,
};
use minicore_runtime::tools::{ApprovalDecision, ToolInputAnswer, ToolInputAnswerKind, ToolSet};
use minicore_runtime::value::Text;
use minicore_runtime::{
    AgentLoop, CancelReason, ConfigRevision, HistoryItem, InteractionAnswer, InteractionKind,
    LoopEvent, LoopHandle, LoopInputs, LoopOptions, LoopOutcome, LoopStatus, UserInput,
    UserMessageKind,
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
            let options = LoopOptions::default_checked().unwrap();
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

            let mut saw_interaction = false;
            let mut saw_finished = false;
            while let Some(envelope) = events.recv().await {
                saw_interaction |= matches!(envelope.event, LoopEvent::InteractionRequested { .. });
                saw_finished |= matches!(envelope.event, LoopEvent::Finished { .. });
            }
            assert!(saw_interaction);
            assert!(saw_finished);
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

        let mut late = handle.watch_state();
        let first = late.borrow_and_update().clone();
        assert_eq!(first, handle.state());
        assert_eq!(first.status, LoopStatus::Finished);
        assert_eq!(first.loop_id, report.loop_id);
        assert!(first.pending_interaction.is_none());
        assert!(late.changed().now_or_never().is_none());

        drop(handle);
        assert!(states.changed().await.is_err());
        assert!(late.changed().await.is_err());
    })
    .await
    .expect("state sender lifetime test timed out");
}

#[tokio::test]
async fn final_state_and_report_survive_unreported_terminal_event_loss() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let (model, _) = scripted_model(vec![Round::BurstDeltas {
            count: 2_049,
            chunk: "part",
        }]);
        let options = LoopOptions::default_checked().unwrap();
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

        assert_completed_with_text(&report, &"part".repeat(2_049));
        assert_eq!(report.requests, 1);
        assert!(Arc::ptr_eq(&report, &handle.wait().await.unwrap()));
        let final_state = states.borrow_and_update().clone();
        assert_eq!(final_state, handle.state());
        assert_eq!(final_state.loop_id, report.loop_id);
        assert_eq!(final_state.status, LoopStatus::Finished);
        assert_eq!(final_state.request_index, 0);
        assert!(final_state.pending_interaction.is_none());

        let mut output_count = 0;
        let mut saw_finished = false;
        while let Some(envelope) = events.recv().await {
            output_count += usize::from(matches!(envelope.event, LoopEvent::OutputDelta { .. }));
            saw_finished |= matches!(envelope.event, LoopEvent::Finished { .. });
        }
        assert_eq!(output_count, 2_049);
        assert!(saw_finished);
    })
    .await
    .expect("terminal event loss test timed out");
}

#[derive(Debug, PartialEq)]
struct Delivery {
    id: &'static str,
    candidate_index: u32,
    takes_at_delivery: usize,
    observed_state: Option<(LoopStatus, u32)>,
}

#[derive(Default)]
struct BoundaryProbe {
    prepared_index: AtomicU32,
    takes: AtomicUsize,
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

    fn candidate_index(&self) -> u32 {
        let prepared = self.prepared_index.load(Ordering::SeqCst);
        match self.issued.lock().unwrap().last() {
            Some((issued, _)) if prepared <= *issued => issued.saturating_add(1),
            _ => prepared,
        }
    }
}

impl LoopInputs for BoundaryProbe {
    fn take(&self) -> Vec<UserInput> {
        self.takes.fetch_add(1, Ordering::SeqCst);
        let taken: Vec<_> = self.queue.lock().unwrap().drain(..).collect();
        if taken.is_empty() {
            return Vec::new();
        }
        let handle = self.handle.lock().unwrap().clone();
        let observed_state = handle.as_ref().map(|handle| {
            let state = handle.state();
            (state.status, state.request_index)
        });
        let candidate_index = self.candidate_index();
        let takes_at_delivery = self.takes.load(Ordering::SeqCst);
        self.deliveries
            .lock()
            .unwrap()
            .extend(taken.iter().map(|(id, _)| Delivery {
                id,
                candidate_index,
                takes_at_delivery,
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
            .prepared_index
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
                    takes_at_delivery: 1,
                    observed_state: None,
                },
                Delivery {
                    id: "after-tool",
                    candidate_index: 1,
                    takes_at_delivery: 3,
                    observed_state: Some((LoopStatus::RunningTools, 0)),
                },
                Delivery {
                    id: "during-prompt",
                    candidate_index: 1,
                    takes_at_delivery: 4,
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
                takes_at_delivery: 2,
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

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_driver_retry_is_one_request_and_reuses_the_candidate_index() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        struct RetryOnce {
            descriptor: ModelDescriptor,
            attempts: AtomicUsize,
            started: Arc<Notify>,
        }

        impl Model for RetryOnce {
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
                    started.notify_one();
                    if attempt == 0 {
                        Err(ModelError::not_started(
                            ModelErrorKind::Unavailable,
                            None,
                            DiagnosticSummary::new(
                                DiagnosticCode::ModelUnavailable,
                                DiagnosticCategory::Model,
                                Text::new("retry me"),
                                true,
                            )
                            .unwrap(),
                        ))
                    } else {
                        Ok::<ModelStream, ModelError>(Box::pin(futures_util::stream::iter(vec![
                            Ok(minicore_runtime::model::ModelEvent::text_delta("done")),
                            Ok(minicore_runtime::model::ModelEvent::Finish {
                                reason: minicore_runtime::model::ModelFinishReason::Stop,
                            }),
                        ])))
                    }
                })
            }
        }

        let probe = Arc::new(BoundaryProbe::default());
        probe.push("before-retry");
        let prompt = index_prompt(probe.clone(), 1);
        let started = Arc::new(Notify::new());
        let inner: Arc<dyn Model> = Arc::new(RetryOnce {
            descriptor: ModelDescriptor::new(
                "fake/retry".parse().unwrap(),
                8192,
                reasoning_set(),
                true,
            )
            .unwrap(),
            attempts: AtomicUsize::new(0),
            started: Arc::clone(&started),
        });
        let agent = AgentLoop::start(
            request(config_full(
                Arc::new(IndexModel {
                    inner,
                    probe: probe.clone(),
                }),
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                prompt.clone(),
            )),
            LoopOptions::default_checked().unwrap(),
            probe.clone(),
        )
        .unwrap();

        started.notified().await;
        tokio::time::advance(Duration::from_millis(250)).await;
        let report = agent.join().await.unwrap();
        assert_completed_with_text(&report, "done");
        assert_eq!(report.requests, 1, "the retry is not a second request");
        assert_eq!(probe.takes.load(Ordering::SeqCst), 2, "A and F only");
        let issued = probe.issued.lock().unwrap();
        assert_eq!(issued.len(), 2, "two attempts share one logical request");
        assert_eq!(issued[0].0, 0);
        assert_eq!(issued[1].0, 0);
        assert_eq!(issued[1].1, ["Fix the parser", "identical text"]);
        assert_eq!(
            *probe.deliveries.lock().unwrap(),
            [Delivery {
                id: "before-retry",
                candidate_index: 0,
                takes_at_delivery: 1,
                observed_state: None,
            }]
        );
    })
    .await
    .expect("retry boundary test timed out");
}

#[tokio::test]
async fn a_config_update_keeps_the_take_boundary_index() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let probe = Arc::new(BoundaryProbe::default());
        let prompt = index_prompt(probe.clone(), 99);
        let model_gate = Arc::new(Notify::new());
        let (first, _) = scripted_model(vec![Round::GatedTools {
            calls: vec![("echo", "call_1")],
            gate: model_gate.clone(),
        }]);
        // Rounds are selected by `request_index`, so the replacement config
        // needs an entry for the index it serves.
        let (second, _) = scripted_model(vec![
            Round::Final { text: "unused" },
            Round::Final { text: "from b" },
        ]);
        let model = Arc::new(IndexModel {
            inner: first,
            probe: probe.clone(),
        });
        let agent = AgentLoop::start(
            request(config_full(
                model,
                ReasoningPreference::Auto,
                tool_set(Arc::default(), ToolBehavior::Succeed),
                None,
                prompt,
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
        *probe.handle.lock().unwrap() = Some(handle.clone());
        let replacement: Arc<dyn Model> = Arc::new(IndexModel {
            inner: second,
            probe: probe.clone(),
        });
        let revision = handle
            .update(config_full(
                replacement,
                ReasoningPreference::Auto,
                ToolSet::default(),
                None,
                index_prompt(probe.clone(), 99),
            ))
            .unwrap();
        assert_eq!(revision, ConfigRevision::new(1));
        probe.push("after-update");
        model_gate.notify_one();

        let report = agent.join().await.unwrap();
        assert_completed_with_text(&report, "from b");
        assert_eq!(report.requests, 2);
        assert_eq!(report.final_config_revision, ConfigRevision::new(1));
        assert_eq!(
            *probe.deliveries.lock().unwrap(),
            [Delivery {
                id: "after-update",
                candidate_index: 1,
                takes_at_delivery: 3,
                observed_state: Some((LoopStatus::RunningTools, 0)),
            }]
        );
        let issued = probe.issued.lock().unwrap();
        assert_eq!(issued.len(), 2);
        assert_eq!(issued[1].0, 1);
        assert_eq!(
            issued[1].1,
            ["Fix the parser", "identical text"],
            "the input landed in the request served by the new config"
        );
    })
    .await
    .expect("config update boundary test timed out");
}
