use std::time::Instant;

use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use super::{FailPath, LoopCtx};
use crate::agent_loop::control::LoopControl;
use crate::agent_loop::event::{LoopEvent, OutputChannel};
use crate::execution::ExecutionConfig;
use crate::history::HistoryView;
use crate::model::{
    ModelCallContext, ModelDriverFailure, ModelDriverProgress, ModelDriverUpdate, ModelLimits,
    ModelMessage, ModelRequest, ModelRetryPhase, ModelValueError,
};
use crate::port_call::{PortCallOutcome, run_port_call};
use crate::prompt::PromptRequest;
use crate::time::DeadlineSource;

pub(super) enum PromptPrep {
    Ready(Vec<ModelMessage>),
    End(FailPath),
}

/// One request boundary: run the prompt provider under cancel/timeout/panic
/// isolation and hand back its messages.
pub(super) async fn prepare_prompt(
    ctx: &mut LoopCtx<'_>,
    request_index: u32,
    snapshot: &ExecutionConfig,
    view: HistoryView<'_>,
    turn_deadline: Instant,
    cancellation: &CancellationToken,
) -> PromptPrep {
    let provider = snapshot.prompt().clone();
    let descriptor = snapshot.descriptor();
    let reasoning = snapshot.reasoning();
    let tools = snapshot.tools().shared_specs();

    let outcome = run_port_call(
        cancellation,
        turn_deadline,
        ctx.options.prompt_timeout,
        |child, deadline| {
            let request = PromptRequest {
                loop_id: ctx.id,
                request_index,
                history: view,
                model: descriptor,
                reasoning,
                tools: &tools,
                cancellation: child,
                deadline: TokioInstant::from_std(deadline),
            };
            provider.prepare(request)
        },
    )
    .await;

    match outcome {
        PortCallOutcome::Returned(Ok(prepared)) => {
            // Core-enforced for every provider: an empty prompt is a Prompt failure.
            if prepared.messages.is_empty() {
                return PromptPrep::End(FailPath::Prompt);
            }
            PromptPrep::Ready(prepared.messages)
        }
        PortCallOutcome::Cancelled => PromptPrep::End(FailPath::Cancelled),
        PortCallOutcome::DeadlineExceeded(DeadlineSource::Turn) => {
            PromptPrep::End(FailPath::Deadline)
        }
        PortCallOutcome::Returned(Err(_))
        | PortCallOutcome::DeadlineExceeded(DeadlineSource::Port)
        | PortCallOutcome::InvalidDeadline(_)
        | PortCallOutcome::Panicked => PromptPrep::End(FailPath::Prompt),
    }
}

pub(super) fn build_model_request(
    messages: Vec<ModelMessage>,
    snapshot: &ExecutionConfig,
) -> Result<ModelRequest, ModelValueError> {
    let tools = snapshot.tools().shared_specs();
    ModelRequest::with_validated_tools(
        messages,
        tools,
        ModelLimits::default(),
        snapshot.reasoning(),
    )
}

/// Runs one model request, reusing the driver's delivery-aware retry, stream
/// assembly, cancel, timeout, and panic isolation. Output deltas are streamed
/// into the event sink inline, so no extra core task is created.
pub(super) async fn run_model(
    ctx: &mut LoopCtx<'_>,
    request_index: u32,
    snapshot: &ExecutionConfig,
    request: ModelRequest,
    turn_deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<crate::model::ModelResponse, ModelDriverFailure> {
    let driver = crate::model::ModelDriver::from_validated(
        snapshot.model().clone(),
        snapshot.descriptor().clone(),
        crate::model::ModelDriverConfig::new(
            ctx.options.model_timeout,
            ctx.options.model_retry_attempts,
            ctx.options.model_retry_base_delay,
        ),
    );

    let context = ModelCallContext::new(ctx.id, request_index, cancellation.clone(), turn_deadline);

    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let control = ctx.control;
    let mut state_sink = ctx.sink.clone();
    let mut update = |value| {
        let mut state = control.current_state();
        match value {
            ModelDriverUpdate::AttemptStarted(attempt) => {
                state.model_attempts = attempt;
                if let Some(retry) = &mut state.model_retry {
                    retry.phase = ModelRetryPhase::Requesting;
                }
            }
            ModelDriverUpdate::RetryWaiting(retry) => state.model_retry = Some(retry),
            ModelDriverUpdate::Finished => {
                if state.model_retry.take().is_none() {
                    return;
                }
            }
        }
        super::publish(control, &mut state_sink, state);
    };
    let run = driver.run_observed(request, context, &progress_tx, &mut update);
    tokio::pin!(run);
    let result = loop {
        tokio::select! {
            biased;
            Some(value) = progress_rx.recv() => {
                forward_progress(ctx, request_index, value);
            }
            result = &mut run => break result,
        }
    };
    while let Ok(value) = progress_rx.try_recv() {
        forward_progress(ctx, request_index, value);
    }
    ctx.model_attempts
        .push(control.current_state().model_attempts);
    result
}

fn forward_progress(ctx: &mut LoopCtx<'_>, request_index: u32, value: ModelDriverProgress) {
    let (channel, delta) = match value {
        ModelDriverProgress::TextDelta(delta) => (OutputChannel::Text, delta),
        ModelDriverProgress::ReasoningDelta(delta) => (OutputChannel::Reasoning, delta),
    };
    ctx.sink.try_emit(LoopEvent::OutputDelta {
        loop_id: ctx.id,
        request_index,
        channel,
        delta,
    });
}

pub(super) fn map_model_failure(control: &LoopControl, failure: ModelDriverFailure) -> FailPath {
    let deadline_source = failure.deadline_source();
    let error = failure.into_error();

    if control.cancellation().is_cancelled() {
        return FailPath::Cancelled;
    }
    if deadline_source == Some(DeadlineSource::Turn) {
        return FailPath::Deadline;
    }
    match error.kind() {
        crate::model::ModelErrorKind::InvalidProviderResponse
        | crate::model::ModelErrorKind::IncompleteResponse
        | crate::model::ModelErrorKind::UnexpectedToolCall => FailPath::InvalidModelResponse(error),
        _ => FailPath::Model(error),
    }
}
