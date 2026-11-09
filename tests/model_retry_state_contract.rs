//! Authoritative retry state and actual model-call accounting.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind, ModelRequest,
    ModelStartFuture,
};
use minicore_runtime::{
    AgentLoop, CancelReason, HistoryFuture, HistoryItem, LoopHandle, LoopHistory, LoopId,
    LoopOptions, LoopOutcome, LoopState, LoopStatus, ModelRetryPhase,
};
use support::loops::*;
use tokio::sync::Notify;
use tokio::time::Instant;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const DELAY: Duration = Duration::from_secs(2);

struct Step {
    gate: Option<Arc<Notify>>,
    result: Result<Round, ModelError>,
}

fn failure(error: ModelError) -> Step {
    Step {
        gate: None,
        result: Err(error),
    }
}

fn gated(result: Result<Round, ModelError>) -> (Step, Arc<Notify>) {
    let gate = Arc::new(Notify::new());
    (
        Step {
            gate: Some(gate.clone()),
            result,
        },
        gate,
    )
}

struct AttemptModel {
    descriptor: ModelDescriptor,
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<(LoopId, u32)>>,
}

impl AttemptModel {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            descriptor: ModelDescriptor::new(
                "fake/retry-state".parse().unwrap(),
                8192,
                reasoning_set(),
                true,
            )
            .unwrap(),
            steps: Mutex::new(steps.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(LoopId, u32)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Model for AttemptModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls
            .lock()
            .unwrap()
            .push((context.loop_id, context.request_index));
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model start");
        Box::pin(async move {
            if let Some(gate) = step.gate {
                gate.notified().await;
            }
            step.result.map(round_stream)
        })
    }
}

fn retryable(message: &str, after: Option<Duration>) -> ModelError {
    ModelError::not_started(
        ModelErrorKind::ProviderUnavailable,
        after,
        DiagnosticSummary::new(
            DiagnosticCode::ModelUnavailable,
            DiagnosticCategory::Model,
            minicore_runtime::Text::new(message),
            true,
        )
        .unwrap(),
    )
}

#[derive(Default)]
struct HistoryGate {
    entered: Notify,
    release: Notify,
}

impl LoopHistory for HistoryGate {
    fn append<'a>(
        &'a self,
        _loop_id: LoopId,
        _offset: usize,
        _items: &'a [HistoryItem],
    ) -> HistoryFuture<'a> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        })
    }
}

fn options() -> LoopOptions {
    let mut options = LoopOptions::default_checked().unwrap();
    options.model_retry_attempts = 3;
    options.model_retry_base_delay = DELAY;
    options
}

fn start(model: Arc<AttemptModel>, options: LoopOptions) -> AgentLoop {
    AgentLoop::start(
        request(config(model, Default::default(), None)),
        options,
        no_inputs(),
    )
    .unwrap()
}

async fn waiting(handle: &LoopHandle, attempt: u8) -> LoopState {
    handle
        .watch_state()
        .wait_for(|state| {
            state.model_retry.as_ref().is_some_and(|retry| {
                retry.attempt == attempt && matches!(retry.phase, ModelRetryPhase::Waiting { .. })
            })
        })
        .await
        .unwrap()
        .clone()
}

async fn requesting(handle: &LoopHandle, attempt: u8) -> LoopState {
    handle
        .watch_state()
        .wait_for(|state| {
            state.model_retry.as_ref().is_some_and(|retry| {
                retry.attempt == attempt && retry.phase == ModelRetryPhase::Requesting
            })
        })
        .await
        .unwrap()
        .clone()
}

#[tokio::test(start_paused = true)]
async fn waiting_uses_source_time_and_actual_start_updates_late_state_without_events() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let error = retryable("first error", None);
        let (second, release) = gated(Ok(Round::Final { text: "done" }));
        let model = AttemptModel::new(vec![failure(error.clone()), second]);
        let history = Arc::new(HistoryGate::default());
        let mut options = options();
        options.history = Some(history.clone());
        let wall_before = SystemTime::now();
        let began = Instant::now();
        let mut agent = start(model.clone(), options);
        drop(agent.take_events().unwrap());
        let handle = agent.handle();
        let state = waiting(&handle, 2).await;
        let wall_after = SystemTime::now();
        assert_eq!(state.loop_id, handle.id());
        assert_eq!(state.request_index, 0);
        assert_eq!(state.status, LoopStatus::RunningModel);
        assert_eq!(state.model_attempts, 1);
        let retry = state.model_retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, 3);
        assert_eq!(retry.error, error);
        let ModelRetryPhase::Waiting {
            retry_at,
            retry_deadline,
        } = retry.phase
        else {
            panic!("waiting")
        };
        assert_eq!(retry_deadline, began + DELAY);
        assert!(retry_at >= wall_before + DELAY && retry_at <= wall_after + DELAY);
        let mut late = handle.watch_state();
        assert_eq!(*late.borrow_and_update(), state);
        tokio::time::advance(DELAY - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(handle.state(), state);
        assert_eq!(model.calls(), [(handle.id(), 0)]);
        tokio::time::advance(Duration::from_millis(1)).await;
        let active = requesting(&handle, 2).await;
        assert_eq!(active.model_attempts, 2);
        assert_eq!(active.model_retry.as_ref().unwrap().error, error);
        assert_eq!(model.calls(), [(handle.id(), 0), (handle.id(), 0)]);
        release.notify_one();
        history.entered.notified().await;
        let cleared = late
            .wait_for(|state| state.model_retry.is_none())
            .await
            .unwrap()
            .clone();
        assert_eq!(cleared.status, LoopStatus::RunningModel);
        assert_eq!(cleared.model_attempts, 2);
        history.release.notify_one();
        let report = agent.join().await.unwrap();
        assert_eq!(report.outcome, LoopOutcome::Completed);
        assert_eq!(report.requests, 1);
        assert_eq!(&*report.model_attempts, [2]);
        assert!(handle.state().model_retry.is_none());
        assert_eq!(handle.state().model_attempts, 2);
    })
    .await
    .expect("retry state test timed out");
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_wait_clears_before_saving_without_counting_planned_attempt() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let model = AttemptModel::new(vec![failure(retryable("cancel waiting", None))]);
        let history = Arc::new(HistoryGate::default());
        let mut options = options();
        options.history = Some(history.clone());
        let mut agent = start(model.clone(), options);
        drop(agent.take_events().unwrap());
        let handle = agent.handle();
        waiting(&handle, 2).await;
        let mut watch = handle.watch_state();
        watch.borrow_and_update();
        handle.cancel();
        history.entered.notified().await;
        let cleared = watch
            .wait_for(|state| state.model_retry.is_none())
            .await
            .unwrap()
            .clone();
        assert_eq!(cleared.model_attempts, 1);
        assert_eq!(model.calls(), [(handle.id(), 0)]);
        history.release.notify_one();
        let report = agent.join().await.unwrap();
        assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
        assert_eq!(report.requests, 1);
        assert_eq!(&*report.model_attempts, [1]);
        assert_eq!(handle.state().model_attempts, 1);
    })
    .await
    .expect("cancel retry test timed out");
}

#[tokio::test(start_paused = true)]
async fn failed_and_cancelled_started_retries_clear_before_history_handoff() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        for cancel in [false, true] {
            let final_error = ModelError::unknown(
                ModelErrorKind::Unavailable,
                retryable("final error", None).diagnostic().clone(),
            );
            let (second, release) = gated(Err(final_error.clone()));
            let model = AttemptModel::new(vec![failure(retryable("retry", None)), second]);
            let history = Arc::new(HistoryGate::default());
            let mut options = options();
            options.history = Some(history.clone());
            let mut agent = start(model.clone(), options);
            drop(agent.take_events().unwrap());
            let handle = agent.handle();
            waiting(&handle, 2).await;
            tokio::time::advance(DELAY).await;
            requesting(&handle, 2).await;
            let mut watch = handle.watch_state();
            watch.borrow_and_update();
            if cancel {
                handle.cancel();
            } else {
                release.notify_one();
            }
            history.entered.notified().await;
            let cleared = watch
                .wait_for(|state| state.model_retry.is_none())
                .await
                .unwrap()
                .clone();
            assert_eq!(cleared.model_attempts, 2);
            assert_eq!(model.calls().len(), 2);
            history.release.notify_one();
            let report = agent.join().await.unwrap();
            assert_eq!(&*report.model_attempts, [2]);
            if cancel {
                assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
            } else {
                let LoopOutcome::Failed(failure) = &report.outcome else {
                    panic!("failed")
                };
                assert_eq!(failure.model_error(), Some(&final_error));
            }
            assert!(handle.state().model_retry.is_none());
        }
    })
    .await
    .expect("retry settlement test timed out");
}

#[tokio::test(start_paused = true)]
async fn retry_clears_for_tools_and_resets_at_the_next_logical_request() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let (second, release_second) = gated(Ok(Round::Tools {
            calls: vec![("echo", "call_retry")],
        }));
        let (next_request, release_next) = gated(Ok(Round::Final { text: "next" }));
        let model = AttemptModel::new(vec![
            failure(retryable("retry tools", None)),
            second,
            next_request,
        ]);
        let tool_gate = Arc::new(Notify::new());
        let agent = AgentLoop::start(
            request(config(
                model.clone(),
                tool_set(Arc::default(), ToolBehavior::Gate(tool_gate.clone())),
                None,
            )),
            options(),
            no_inputs(),
        )
        .unwrap();
        let handle = agent.handle();
        waiting(&handle, 2).await;
        tokio::time::advance(DELAY).await;
        requesting(&handle, 2).await;
        release_second.notify_one();
        let mut watch = handle.watch_state();
        let tools = watch
            .wait_for(|state| state.status == LoopStatus::RunningTools)
            .await
            .unwrap()
            .clone();
        assert_eq!(tools.request_index, 0);
        assert_eq!(tools.model_attempts, 2);
        assert!(tools.model_retry.is_none());
        tool_gate.notify_one();
        let next = watch
            .wait_for(|state| state.request_index == 1 && state.model_attempts == 1)
            .await
            .unwrap()
            .clone();
        assert_eq!(next.status, LoopStatus::RunningModel);
        assert!(next.model_retry.is_none());
        assert_eq!(
            model.calls(),
            [(handle.id(), 0), (handle.id(), 0), (handle.id(), 1)]
        );
        release_next.notify_one();
        let report = agent.join().await.unwrap();
        assert_eq!(report.outcome, LoopOutcome::Completed);
        assert_eq!(report.requests, 2);
        assert_eq!(&*report.model_attempts, [2, 1]);
        assert_eq!(handle.state().request_index, 1);
        assert_eq!(handle.state().model_attempts, 1);
        assert!(handle.state().model_retry.is_none());
    })
    .await
    .expect("next request retry state test timed out");
}

#[tokio::test(start_paused = true)]
async fn no_observer_is_needed_for_retries_or_late_terminal_state() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let model = AttemptModel::new(vec![
            failure(retryable("one", None)),
            failure(retryable("two", None)),
            Step {
                gate: None,
                result: Ok(Round::Final { text: "done" }),
            },
        ]);
        let mut agent = start(model.clone(), options());
        drop(agent.take_events().unwrap());
        let handle = agent.handle();
        let report = agent.join().await.unwrap();
        assert_eq!(report.outcome, LoopOutcome::Completed);
        assert_eq!(report.requests, 1);
        assert_eq!(&*report.model_attempts, [3]);
        assert_eq!(model.calls(), vec![(handle.id(), 0); 3]);
        let mut late = handle.watch_state();
        let state = late.borrow_and_update().clone();
        assert_eq!(state, handle.state());
        assert_eq!(state.status, LoopStatus::Finished);
        assert_eq!(state.model_attempts, 3);
        assert!(state.model_retry.is_none());
    })
    .await
    .expect("unobserved retry test timed out");
}

#[tokio::test(start_paused = true)]
async fn delayed_events_preserve_four_total_attempts_and_each_preceding_error() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let errors: Vec<_> = (1..=4)
            .map(|index| retryable(&format!("attempt {index}"), None))
            .collect();
        let model = AttemptModel::new(errors.iter().cloned().map(failure).collect());
        let mut options = options();
        options.model_retry_attempts = 4;
        let mut agent = start(model.clone(), options);
        let mut events = agent.take_events().unwrap();
        let handle = agent.handle();
        let report = agent.join().await.unwrap();
        assert_eq!(report.requests, 1);
        assert_eq!(&*report.model_attempts, [4]);
        assert_eq!(model.calls(), vec![(handle.id(), 0); 4]);
        let LoopOutcome::Failed(failure) = &report.outcome else {
            panic!("failed")
        };
        assert_eq!(failure.model_error(), Some(&errors[3]));
        let mut transitions = Vec::new();
        let mut cleared = false;
        while let Ok(envelope) = events.try_recv() {
            if let minicore_runtime::LoopEvent::StateChanged { state } = envelope.event {
                assert_eq!(state.loop_id, handle.id());
                assert_eq!(state.request_index, 0);
                if let Some(retry) = state.model_retry {
                    assert!(!cleared);
                    assert_eq!(retry.max_attempts, 4);
                    assert_eq!(retry.error, errors[usize::from(retry.attempt - 2)]);
                    let waiting = matches!(retry.phase, ModelRetryPhase::Waiting { .. });
                    assert_eq!(state.model_attempts, retry.attempt - u8::from(waiting));
                    transitions.push((retry.attempt, waiting));
                } else if state.model_attempts == 4 {
                    cleared = true;
                }
            }
        }
        assert_eq!(
            transitions,
            [
                (2, true),
                (2, false),
                (3, true),
                (3, false),
                (4, true),
                (4, false)
            ]
        );
        assert!(cleared);
        assert!(handle.state().model_retry.is_none());
    })
    .await
    .expect("delayed retry observation test timed out");
}

#[tokio::test(start_paused = true)]
async fn retry_after_over_cap_or_not_fitting_deadline_never_schedules_a_shorter_wait() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        for (after, timeout) in [
            (Duration::from_secs(31), Duration::from_secs(60)),
            (Duration::from_secs(5), Duration::from_secs(5)),
            (Duration::from_secs(5), Duration::from_secs(4)),
        ] {
            let error = retryable("retry must not be shortened", Some(after));
            let model = AttemptModel::new(vec![failure(error.clone())]);
            let mut options = options();
            options.model_timeout = timeout;
            let mut agent = start(model.clone(), options);
            let mut events = agent.take_events().unwrap();
            let handle = agent.handle();
            let report = agent.join().await.unwrap();
            assert_eq!(&*report.model_attempts, [1]);
            assert_eq!(model.calls(), [(handle.id(), 0)]);
            let LoopOutcome::Failed(failure) = &report.outcome else {
                panic!("failed")
            };
            assert_eq!(failure.model_error(), Some(&error));
            while let Ok(envelope) = events.try_recv() {
                if let minicore_runtime::LoopEvent::StateChanged { state } = envelope.event {
                    assert!(state.model_retry.is_none());
                }
            }
        }
    })
    .await
    .expect("retry-after rejection test timed out");
}

#[tokio::test(start_paused = true)]
async fn retry_after_at_cap_uses_the_full_lower_bound() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let after = Duration::from_secs(30);
        let error = retryable("full thirty seconds", Some(after));
        let (second, release) = gated(Ok(Round::Final { text: "done" }));
        let model = AttemptModel::new(vec![failure(error.clone()), second]);
        let began = Instant::now();
        let mut options = options();
        options.model_timeout = Duration::from_secs(31);
        let agent = start(model.clone(), options);
        let handle = agent.handle();
        let state = waiting(&handle, 2).await;
        let retry = state.model_retry.unwrap();
        assert_eq!(retry.error, error);
        let ModelRetryPhase::Waiting { retry_deadline, .. } = retry.phase else {
            panic!("waiting")
        };
        assert_eq!(retry_deadline, began + after);
        tokio::time::advance(after - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(model.calls().len(), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(requesting(&handle, 2).await.model_attempts, 2);
        release.notify_one();
        let report = agent.join().await.unwrap();
        assert_eq!(report.outcome, LoopOutcome::Completed);
        assert_eq!(&*report.model_attempts, [2]);
    })
    .await
    .expect("retry-after lower bound test timed out");
}

#[tokio::test(start_paused = true)]
async fn crossing_original_model_or_loop_deadline_clears_wait_without_another_start() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        for loop_deadline in [false, true] {
            let error = retryable("cause retained at timeout", None);
            let model = AttemptModel::new(vec![failure(error.clone())]);
            let mut options = options();
            options.model_retry_base_delay = Duration::from_secs(5);
            if loop_deadline {
                options.deadline = Some(Instant::now() + Duration::from_secs(9));
            } else {
                options.model_timeout = Duration::from_secs(9);
            }
            let agent = start(model.clone(), options);
            let handle = agent.handle();
            waiting(&handle, 2).await;
            tokio::time::advance(Duration::from_secs(10)).await;
            let report = agent.join().await.unwrap();
            assert_eq!(&*report.model_attempts, [1]);
            assert_eq!(model.calls(), [(handle.id(), 0)]);
            assert!(handle.state().model_retry.is_none());
            if loop_deadline {
                assert_eq!(
                    report.outcome,
                    LoopOutcome::Cancelled(CancelReason::Deadline)
                );
            } else {
                let LoopOutcome::Failed(failure) = &report.outcome else {
                    panic!("failed")
                };
                assert_eq!(failure.model_error(), Some(&error));
            }
        }
    })
    .await
    .expect("retry deadline test timed out");
}

#[tokio::test(start_paused = true)]
async fn an_event_on_the_retried_attempt_prevents_replay_and_preserves_partial() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let (second, release) = gated(Ok(Round::StreamError {
            events: vec![minicore_runtime::model::ModelEvent::text_delta("actual partial")],
            error: retryable("cannot replay after output", None),
        }));
        let model = AttemptModel::new(vec![failure(retryable("first error", None)), second]);
        let mut agent = start(model.clone(), options());
        drop(agent.take_events().unwrap());
        let handle = agent.handle();
        waiting(&handle, 2).await;
        tokio::time::advance(DELAY).await;
        requesting(&handle, 2).await;
        release.notify_one();
        let report = agent.join().await.unwrap();
        assert_eq!(&*report.model_attempts, [2]);
        assert_eq!(model.calls().len(), 2);
        assert!(handle.state().model_retry.is_none());
        let LoopOutcome::Failed(failure) = &report.outcome else { panic!("failed") };
        let error = failure.model_error().unwrap();
        assert_eq!(error.delivery(), minicore_runtime::model::DeliveryState::Started);
        assert_eq!(error.retry_hint(), &minicore_runtime::model::RetryHint::Never);
        let partial = report.partial.as_ref().unwrap();
        assert_eq!(partial.request_index, 0);
        assert!(matches!(&*partial.content, [minicore_runtime::model::AssistantPart::Text(text)] if text.as_ref() == "actual partial"));
        assert!(!report.appended.iter().any(|item| matches!(item, HistoryItem::Assistant(_))));
    }).await.expect("retry partial test timed out");
}

#[tokio::test(start_paused = true)]
async fn failed_next_preparation_keeps_the_last_issued_request_attempt_count() {
    use minicore_runtime::prompt::{
        DefaultPromptProvider, PromptError, PromptFuture, PromptProvider, PromptRequest,
    };

    struct FailNextPrompt(DefaultPromptProvider);

    impl PromptProvider for FailNextPrompt {
        fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
            if request.request_index == 1 {
                Box::pin(async { Err(PromptError::InvalidHistory) })
            } else {
                self.0.prepare(request)
            }
        }
    }

    tokio::time::timeout(TEST_TIMEOUT, async {
        let model = AttemptModel::new(vec![
            failure(retryable("retry before tools", None)),
            Step {
                gate: None,
                result: Ok(Round::Tools {
                    calls: vec![("echo", "call_retry")],
                }),
            },
        ]);
        let mut agent = AgentLoop::start(
            request(config_full(
                model.clone(),
                minicore_runtime::model::ReasoningPreference::Auto,
                tool_set(Arc::default(), ToolBehavior::Succeed),
                None,
                Arc::new(FailNextPrompt(DefaultPromptProvider::new(None))),
            )),
            options(),
            no_inputs(),
        )
        .unwrap();
        let mut events = agent.take_events().unwrap();
        let handle = agent.handle();
        let report = agent.join().await.unwrap();
        assert_eq!(report.requests, 1);
        assert_eq!(&*report.model_attempts, [2]);
        assert_eq!(handle.state().request_index, 0);
        assert_eq!(handle.state().model_attempts, 2);
        assert!(handle.state().model_retry.is_none());
        assert_eq!(model.calls(), vec![(handle.id(), 0); 2]);
        let LoopOutcome::Failed(failure) = &report.outcome else {
            panic!("failed")
        };
        assert_eq!(failure.kind, minicore_runtime::LoopFailureKind::Prompt);
        let mut next_preparation = false;
        while let Ok(envelope) = events.try_recv() {
            if let minicore_runtime::LoopEvent::StateChanged { state } = envelope.event
                && state.request_index == 1
            {
                assert_eq!(state.status, LoopStatus::Starting);
                assert_eq!(state.model_attempts, 0);
                assert!(state.model_retry.is_none());
                next_preparation = true;
            }
        }
        assert!(next_preparation);
    })
    .await
    .expect("failed next preparation test timed out");
}
