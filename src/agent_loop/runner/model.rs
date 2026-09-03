use std::sync::atomic::{AtomicU64, Ordering};
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
    ModelCallContext, ModelDriverFailure, ModelDriverProgress, ModelLimits, ModelMessage,
    ModelRequest, ModelValueError,
};
use crate::port_call::{PortCallOutcome, run_port_call};
use crate::prompt::PromptRequest;
use crate::time::DeadlineSource;
use crate::tools::ToolSpec;

const MODEL_PROGRESS_CAPACITY: usize = 64;

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
    let tools: Vec<ToolSpec> = snapshot.tools().frozen_specs().cloned().collect();

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
            // Core-enforced budget for every provider: an empty prompt or one
            // over the loop's message ceiling is a Prompt failure, no matter
            // which provider produced it.
            if prepared.messages.is_empty()
                || prepared.messages.len() > ctx.options.limits.max_prompt_messages
            {
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
    let tools: Vec<ToolSpec> = snapshot.tools().frozen_specs().cloned().collect();
    ModelRequest::new(
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
        crate::model::ModelDriverConfig::from_kernel_values(
            ctx.options.model_timeout,
            ctx.options.model_retry_attempts,
            ctx.options.model_retry_base_delay,
            crate::model::SemanticLimitsSnapshot::from_kernel_values(
                ctx.options.limits.max_tool_calls_per_response,
                ctx.options.limits.max_tool_name_bytes,
                ctx.options.limits.max_tool_schema_bytes,
                ctx.options.limits.max_tool_arguments_bytes,
                ctx.options.limits.max_model_text_bytes,
                ctx.options.limits.max_model_reasoning_bytes,
            ),
        ),
    )
    .map_err(ModelDriverFailure::ordinary)?;

    let context = ModelCallContext::new(ctx.id, request_index, cancellation.clone(), turn_deadline);

    let progress_dropped = AtomicU64::new(0);
    let (progress_tx, mut progress_rx) = mpsc::channel(MODEL_PROGRESS_CAPACITY);
    let run = driver.run_detailed(request, context, &progress_tx, &progress_dropped);
    tokio::pin!(run);
    let result = loop {
        tokio::select! {
            biased;
            result = &mut run => break result,
            value = progress_rx.recv() => match value {
                Some(ModelDriverProgress::TextDelta(delta)) => {
                    ctx.sink
                        .record_dropped(progress_dropped.swap(0, Ordering::Relaxed));
                    ctx.sink.try_emit(LoopEvent::OutputDelta {
                        loop_id: ctx.id,
                        request_index,
                        channel: OutputChannel::Text,
                        delta,
                    });
                }
                Some(ModelDriverProgress::ReasoningDelta(delta)) => {
                    ctx.sink
                        .record_dropped(progress_dropped.swap(0, Ordering::Relaxed));
                    ctx.sink.try_emit(LoopEvent::OutputDelta {
                        loop_id: ctx.id,
                        request_index,
                        channel: OutputChannel::Reasoning,
                        delta,
                    });
                }
                None => continue,
            },
        }
    };
    ctx.sink
        .record_dropped(progress_dropped.swap(0, Ordering::Relaxed));
    while let Ok(value) = progress_rx.try_recv() {
        ctx.sink
            .record_dropped(progress_dropped.swap(0, Ordering::Relaxed));
        ctx.sink.try_emit(match value {
            ModelDriverProgress::TextDelta(delta) => LoopEvent::OutputDelta {
                loop_id: ctx.id,
                request_index,
                channel: OutputChannel::Text,
                delta,
            },
            ModelDriverProgress::ReasoningDelta(delta) => LoopEvent::OutputDelta {
                loop_id: ctx.id,
                request_index,
                channel: OutputChannel::Reasoning,
                delta,
            },
        });
    }
    result
}

pub(super) fn map_model_failure(control: &LoopControl, failure: ModelDriverFailure) -> FailPath {
    let deadline_source = failure.deadline_source();
    let error = failure.into_error();

    if control.cancellation().is_cancelled() {
        return FailPath::Cancelled;
    }
    if deadline_source == Some(DeadlineSource::Turn)
        && error.kind() == crate::model::ModelErrorKind::Timeout
    {
        return FailPath::Deadline;
    }
    match error.kind() {
        crate::model::ModelErrorKind::InvalidProviderResponse
        | crate::model::ModelErrorKind::IncompleteResponse
        | crate::model::ModelErrorKind::UnexpectedToolCall => FailPath::InvalidModelResponse(error),
        _ => FailPath::Model(error),
    }
}
