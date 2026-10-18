//! In-crate completion-semantics tests for the loop runner.
//!
//! These cover behavior the public API cannot reach deterministically:
//! aborting the runner task's join handle, and the exactly-once panic
//! fallback. Ports (model/prompt/tool/policy) isolate their own panics, so
//! the runner-level panic path is exercised directly against its contract.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::json;

use crate::agent_loop::control::{FinishSeal, LoopControl};
use crate::agent_loop::{
    AgentLoop, LoopHandle, LoopOptions, LoopOutcome, LoopReport, LoopRequest, LoopWaitError,
};
use crate::execution::{ConfigRevision, ExecutionConfig};
use crate::history::WorkingHistory;
use crate::ids::{LoopId, ToolCallId};
use crate::limits::LoopLimits;
use crate::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelRequest, ModelStartFuture,
    ModelStream, ReasoningPreference, ToolCall, Usage,
};
use crate::tools::{ToolResultOutcome, ToolSet};

use super::{FailPath, LoopCtx, finish_loop, panic_report};

/// A model whose stream never yields: the loop stays alive until aborted.
struct HoldingModel {
    descriptor: ModelDescriptor,
}

impl Model for HoldingModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(async { Ok::<ModelStream, ModelError>(Box::pin(futures_util::stream::pending())) })
    }
}

fn holding_config() -> ExecutionConfig {
    let descriptor = ModelDescriptor::new(
        "fake/holding".parse().unwrap(),
        8192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap();
    ExecutionConfig::new(
        Arc::new(HoldingModel { descriptor }),
        ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(crate::prompt::DefaultPromptProvider::new(None)),
    )
    .expect("holding config must validate")
}

/// M1: when the runner task is aborted from outside, every waiter observes the
/// completion channel as closed instead of waiting forever.
#[tokio::test]
async fn aborted_runner_makes_wait_return_completion_closed() {
    let request = LoopRequest::new(
        Arc::from([]),
        crate::execution::UserInput::text("hello").unwrap(),
        holding_config(),
    );
    let agent = AgentLoop::start(request, LoopOptions::default_checked().unwrap()).unwrap();
    let handle = agent.handle();

    // The runner task is spawned inside `start`; abort its join handle.
    agent.join.as_ref().expect("runner task handle").abort();

    let error = handle.wait().await.unwrap_err();
    assert_eq!(error, LoopWaitError::CompletionClosed);
}

/// F1: `finish_loop` losing the exactly-once seal returns the already-
/// published Arc (never a second, distinct report).
#[tokio::test]
async fn finish_loop_losing_the_seal_returns_the_published_arc() {
    let id = LoopId::new().unwrap();
    let (control, mut sink, _stream, completion_tx) =
        LoopControl::new(id, 8, LoopLimits::default(), 16).unwrap();
    let control = Arc::new(control);

    // Simulate a prior completion that won the seal and published its report.
    let published = Arc::new(LoopReport {
        loop_id: id,
        outcome: LoopOutcome::Completed,
        appended: Arc::from([]),
        usage: Usage::default(),
        requests: 0,
        tool_rounds: 0,
        final_config_revision: ConfigRevision::INITIAL,
    });
    completion_tx.send_replace(Some(Arc::clone(&published)));
    assert_eq!(control.finish_once().0, FinishSeal::Clean);

    let options = LoopOptions::default_checked().unwrap();
    let mut ctx = LoopCtx {
        id,
        control: &control,
        options: &options,
        sink: &mut sink,
        completion_tx: &completion_tx,
    };
    let report = finish_loop(
        &mut ctx,
        FailPath::Completed,
        WorkingHistory::new(Arc::from([])),
        Usage::default(),
        0,
        None,
        0,
    );
    assert!(
        Arc::ptr_eq(&report, &published),
        "finish_loop must hand back the published Arc on AlreadyFinished"
    );

    // A waiter still observes the exact same Arc.
    let handle = LoopHandle::new(Arc::clone(&control));
    let waited = handle.wait().await.unwrap();
    assert!(Arc::ptr_eq(&waited, &published));
}

#[tokio::test]
async fn completion_preserves_tool_round_counts_beyond_u16() {
    for tool_rounds in [65_535_u64, 65_536, 70_000] {
        let id = LoopId::new().unwrap();
        let (control, mut sink, _stream, completion_tx) =
            LoopControl::new(id, 8, LoopLimits::default(), 16).unwrap();
        let control = Arc::new(control);
        let options = LoopOptions::default_checked().unwrap();
        let mut ctx = LoopCtx {
            id,
            control: &control,
            options: &options,
            sink: &mut sink,
            completion_tx: &completion_tx,
        };
        let report = finish_loop(
            &mut ctx,
            FailPath::Completed,
            WorkingHistory::new(Arc::from([])),
            Usage::default(),
            0,
            None,
            tool_rounds,
        );
        assert_eq!(report.tool_rounds, tool_rounds);
        let waited = LoopHandle::new(Arc::clone(&control)).wait().await.unwrap();
        assert_eq!(waited.tool_rounds, tool_rounds);
        assert!(Arc::ptr_eq(&report, &waited));
    }
}

/// M2/L3: completing once cancels the root token, seals the loop, makes later
/// cancels return false, and a second completion attempt never overwrites the
/// delivered report.
#[tokio::test]
async fn completion_is_exactly_once_and_token_cancel_is_terminal() {
    let id = LoopId::new().unwrap();
    let (control, _sink, _stream, completion_tx) =
        LoopControl::new(id, 8, LoopLimits::default(), 16).unwrap();
    let control = Arc::new(control);
    let handle = LoopHandle::new(Arc::clone(&control));

    // First completion via the panic fallback (the simplest exactly-once
    // consumer): it must publish, seal, and cancel the root token.
    let first = panic_report(&control, &completion_tx);
    assert!(matches!(first.outcome, LoopOutcome::Failed(_)));
    assert!(control.is_finished(), "the loop must read as finished");
    assert!(
        control.cancellation().is_cancelled(),
        "every ending path must cancel the root token"
    );

    // A later ending attempt must never overwrite the published report: it
    // hands back the exact same Arc that waiters already hold.
    let second = panic_report(&control, &completion_tx);
    assert_eq!(control.finish_once().0, FinishSeal::AlreadyFinished);
    assert!(
        Arc::ptr_eq(&second, &first),
        "losing ending path must reuse the published Arc"
    );

    let waited = handle.wait().await.unwrap();
    assert!(
        Arc::ptr_eq(&waited, &first),
        "waiters must receive the first, and only, report"
    );
    assert!(Arc::ptr_eq(&waited, &second));

    // After completion, cancel/shutdown-style requests report false and the
    // finished state is not reopened.
    assert!(!control.mark_cancel(crate::agent_loop::control::CancelSource::User));
    assert!(!control.mark_cancel(crate::agent_loop::control::CancelSource::Shutdown));
    assert!(control.is_finished());
}

/// FIX-02-T05: Runner terminal tool result messages must be bounded to max_output_bytes.
#[test]
fn terminal_tool_result_messages_are_bounded_to_max_output_bytes() {
    let loop_id = LoopId::new().unwrap();
    let call = ToolCall::new(
        ToolCallId::new("call_1").unwrap(),
        "echo".parse().unwrap(),
        json!({}),
        0,
    )
    .unwrap();

    let messages = ["tool batch interrupted", "tool round limit reached"];

    for message in messages {
        let result = super::tools::terminal_tool_result(
            loop_id,
            0,
            call.tool_call_id(),
            call.name(),
            ToolResultOutcome::Failed,
            message,
            1,
        );
        assert!(
            result.output.content().byte_len() <= 1,
            "message '{message}' byte len {} exceeds expected max 1",
            result.output.content().byte_len(),
        );
    }
}

#[tokio::test]
async fn discarded_config_drop_panics_cannot_lose_or_replace_the_report() {
    use crate::model::{ModelEvent, ModelFinishReason};
    use crate::prompt::{PromptFuture, PromptProvider, PromptRequest};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropPrompt {
        control: Arc<LoopControl>,
        published: Arc<AtomicBool>,
    }
    impl PromptProvider for DropPrompt {
        fn prepare<'a>(&'a self, _: PromptRequest<'a>) -> PromptFuture<'a> {
            panic!("discarded config must not be used")
        }
    }
    impl Drop for DropPrompt {
        fn drop(&mut self) {
            let report = self
                .control
                .published_report()
                .expect("publish before host Drop");
            assert_eq!(report.requests, 1);
            assert!(!report.appended.is_empty());
            self.published.store(true, Ordering::SeqCst);
            panic!("host destructor panicked");
        }
    }
    struct UpdatingModel {
        descriptor: ModelDescriptor,
        control: Arc<LoopControl>,
        pending: std::sync::Mutex<Option<ExecutionConfig>>,
        finish: ModelFinishReason,
    }
    impl Model for UpdatingModel {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }
        fn start<'a>(&'a self, _: ModelRequest, _: ModelCallContext) -> ModelStartFuture<'a> {
            self.control
                .update(self.pending.lock().unwrap().take().unwrap())
                .unwrap();
            Box::pin(async move {
                Ok::<ModelStream, ModelError>(Box::pin(futures_util::stream::iter(vec![
                    Ok(ModelEvent::text_delta("retained output").unwrap()),
                    Ok(ModelEvent::Finish {
                        reason: self.finish,
                    }),
                ])))
            })
        }
    }

    for finish in [ModelFinishReason::Stop, ModelFinishReason::Length] {
        let id = LoopId::new().unwrap();
        let (control, sink, _events, completion_tx) =
            LoopControl::new(id, 16, LoopLimits::default(), 4).unwrap();
        let control = Arc::new(control);
        let published = Arc::new(AtomicBool::new(false));
        let base = holding_config();
        let pending = ExecutionConfig::new(
            base.model().clone(),
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(DropPrompt {
                control: control.clone(),
                published: published.clone(),
            }),
        )
        .unwrap();
        let config = ExecutionConfig::new(
            Arc::new(UpdatingModel {
                descriptor: base.descriptor().clone(),
                control: control.clone(),
                pending: std::sync::Mutex::new(Some(pending)),
                finish,
            }),
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(crate::prompt::DefaultPromptProvider::new(None)),
        )
        .unwrap();
        let request = LoopRequest::new(
            Arc::from([]),
            crate::UserInput::text("hello").unwrap(),
            config,
        );
        let report = super::run_loop(
            request,
            Arc::new(LoopOptions::default_checked().unwrap()),
            control.clone(),
            sink,
            completion_tx,
        )
        .await;
        assert!(published.load(Ordering::SeqCst));
        assert_eq!(report.requests, 1);
        assert_eq!(report.appended.len(), 2);
        assert!(Arc::ptr_eq(
            &report,
            &LoopHandle::new(control).wait().await.unwrap()
        ));
        match finish {
            ModelFinishReason::Stop => assert_eq!(report.outcome, LoopOutcome::Completed),
            _ => assert!(
                matches!(&report.outcome, LoopOutcome::Failed(failure) if failure.kind == crate::LoopFailureKind::OutputLimit)
            ),
        }
    }
}
