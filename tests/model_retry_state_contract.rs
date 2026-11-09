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
