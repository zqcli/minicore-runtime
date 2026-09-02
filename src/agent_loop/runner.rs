use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use crate::agent::runner::support::UsageAccumulator;
use crate::agent_loop::event::{LoopEvent, LoopEventSink, LoopOutcomeSummary, OutputChannel};
use crate::agent_loop::{
    CancelReason, LoopFailure, LoopFailureKind, LoopOptions, LoopOutcome, LoopReport, LoopRequest,
    LoopState, LoopStatus,
};
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::execution::{ConfigRevision, ExecutionConfig};
use crate::history::{HistoryView, WorkingHistory};
use crate::ids::{InteractionId, LoopId, SessionId, SessionInstanceId, ToolCallId, TurnId};
use crate::interaction::{InteractionAnswer, InteractionKind, PendingInteraction};
use crate::model::{
    ModelCallContext, ModelDriverFailure, ModelFinishReason, ModelLimits, ModelMessage,
    ModelRequest, ModelValueError, Usage,
};
use crate::port_call::{PortCallOutcome, run_port_call};
use crate::prompt_provider::PromptRequest;
use crate::time::DeadlineSource;
use crate::tools::{
    ApprovalDecision, ApprovalRequest, EnabledTool, EnabledTools, ToolContext, ToolDecision,
    ToolExecutionOutcome, ToolInputRequest, ToolInvocation, ToolOutput, ToolPolicy,
    ToolPolicyRequest, ToolProgress, ToolProgressEmitter, ToolProgressSink, ToolResultOutcome,
    ToolSet, ToolSpec,
};
use crate::value::BoundedText;

use super::control::FinishSeal;
use super::control::{BoundaryChanges, FinalGate, InteractionSlot, LoopControl};

use crate::history::{AssistantHistory, ToolResultHistory, UserHistory, UserMessageKind};
use crate::model::{ModelDriverProgress, ToolCall};

#[cfg(test)]
mod tests;

const MODEL_PROGRESS_CAPACITY: usize = 64;
const TOOL_PROGRESS_CAPACITY: usize = 64;
/// Horizon used when the loop has no absolute deadline so per-port timeouts
/// govern each call without an artificial loop-level cap.
const LOOP_HORIZON: Duration = Duration::from_secs(24 * 60 * 60);

/// Immutable per-loop context shared by every runner phase.
struct LoopCtx<'a> {
    id: LoopId,
    control: &'a LoopControl,
    identity: &'a PortIdentity,
    options: &'a LoopOptions,
    sink: &'a mut LoopEventSink,
    completion_tx: &'a watch::Sender<Option<Arc<LoopReport>>>,
}

impl LoopCtx<'_> {
    fn publish(&mut self, state: LoopState) {
        publish(self.control, &mut *self.sink, state);
    }
}

/// Applies one boundary take to the runner: the config candidate becomes the
/// working (still uncommitted) snapshot, and every accepted steer is appended
/// to the working history in order as a `Steering` user item.
fn apply_boundary(
    ctx: &mut LoopCtx<'_>,
    working: &mut WorkingHistory,
    current_config: &mut Arc<ExecutionConfig>,
    candidate_revision: &mut Option<ConfigRevision>,
    changes: BoundaryChanges,
) {
    if let Some((revision, config)) = changes.config {
        *current_config = config;
        *candidate_revision = Some(revision);
    }
    for steer in changes.steers {
        working.append_user(UserHistory {
            loop_id: ctx.id,
            kind: UserMessageKind::Steering,
            input: steer,
        });
    }
}

/// The single runner task for one agent loop. All control touches are short
/// critical sections; every event is a best-effort `try_send`. The final watch
/// sender is owned exclusively by this task: when it drops (normal exit or an
/// external abort) every waiter sees the completion channel as closed.
pub(crate) async fn run_loop(
    request: LoopRequest,
    options: Arc<LoopOptions>,
    control: Arc<LoopControl>,
    events: LoopEventSink,
    completion_tx: watch::Sender<Option<Arc<LoopReport>>>,
) -> Arc<LoopReport> {
    let run = run_loop_inner(
        request,
        options,
        control.clone(),
        events,
        completion_tx.clone(),
    );
    match AssertUnwindSafe(run).catch_unwind().await {
        Ok(report) => report,
        Err(_) => panic_report(&control, &completion_tx),
    }
}

async fn run_loop_inner(
    request: LoopRequest,
    options: Arc<LoopOptions>,
    control: Arc<LoopControl>,
    mut sink: LoopEventSink,
    completion_tx: watch::Sender<Option<Arc<LoopReport>>>,
) -> Arc<LoopReport> {
    let id = control.id;
    let identity = port_identity(id);
    let cancellation = control.cancellation();
    let loop_deadline = options.deadline;
    let mut ctx = LoopCtx {
        id,
        control: &control,
        identity: &identity,
        options: &options,
        sink: &mut sink,
        completion_tx: &completion_tx,
    };

    ctx.publish(LoopState::new(
        id,
        LoopStatus::Starting,
        0,
        ConfigRevision::INITIAL,
    ));
    ctx.sink.try_emit(LoopEvent::Started { loop_id: id });

    let mut working = WorkingHistory::new(request.history.clone());
    working.append_user(UserHistory {
        loop_id: id,
        kind: UserMessageKind::Prompt,
        input: request.input.clone(),
    });

    let mut current_config = Arc::new(request.config);
    // Last revision whose snapshot actually served an issued Model Request.
    // A taken-but-never-issued candidate must not advance this (prompt or
    // build failures, cancellation or deadline while preparing).
    let mut issued_revision = ConfigRevision::INITIAL;
    // Revision pulled from control by the A/F steps; only committed at the
    // issue boundary below, so repeated stale rebuilds keep the latest one
    // while request_index stays put.
    let mut candidate_revision: Option<ConfigRevision> = None;
    let mut request_index: u32 = 0;
    let mut tool_rounds: u16 = 0;
    let mut requests: u32 = 0;
    let mut usage = UsageAccumulator::default();

    let end = loop {
        if cancellation.is_cancelled() {
            break FailPath::Cancelled;
        }
        if loop_deadline.is_some_and(|deadline| TokioInstant::now() >= deadline) {
            break FailPath::Deadline;
        }

        // A. Atomically pull the latest config candidate plus every accepted
        // steer and apply them as the basis for this request attempt. The
        // config is still only a *candidate*: its revision is recorded once a
        // Model Request really goes out. The snapshot below is pinned for the
        // whole request (model call and its tool batch).
        let changes = ctx.control.take_boundary();
        apply_boundary(
            &mut ctx,
            &mut working,
            &mut current_config,
            &mut candidate_revision,
            changes,
        );

        let snapshot = Arc::clone(&current_config);
        let messages = match prepare_prompt(
            &mut ctx,
            request_index,
            &snapshot,
            working.view(),
            turn_deadline(loop_deadline),
            &cancellation,
        )
        .await
        {
            PromptPrep::Ready(messages) => messages,
            PromptPrep::End(path) => break path,
        };

        // F. Newer config/steers may have landed while the prompt was being
        // prepared. Discard the stale prompt and rebuild with the latest
        // changes: request_index does not advance, no model request is ever
        // issued for the stale snapshot, and the candidate revision keeps the
        // latest value (spec 15.1).
        let changes = ctx.control.take_boundary();
        if changes.config.is_some() || !changes.steers.is_empty() {
            apply_boundary(
                &mut ctx,
                &mut working,
                &mut current_config,
                &mut candidate_revision,
                changes,
            );
            continue;
        }

        let model_request = match build_model_request(messages, &snapshot) {
            Ok(request) => request,
            Err(_) => break FailPath::Prompt,
        };

        // Issue boundary: a real Model Request is about to be dispatched under
        // the candidate revision (or the previously issued one). Only now is
        // the revision recorded as actually applied/issued, for RequestStarted,
        // per-request states, and the final report.
        let revision = candidate_revision.take().unwrap_or(issued_revision);
        issued_revision = revision;
        ctx.control.commit_revision(revision);

        ctx.sink.try_emit(LoopEvent::RequestStarted {
            loop_id: id,
            request_index,
            config_revision: revision,
            model: snapshot.descriptor().model_ref.clone(),
            reasoning: snapshot.reasoning(),
        });
        ctx.publish(LoopState {
            loop_id: id,
            status: LoopStatus::RunningModel,
            request_index,
            config_revision: revision,
            model: Some(snapshot.descriptor().model_ref.clone()),
            pending_interaction: None,
        });

        let response = match run_model(
            &mut ctx,
            request_index,
            &snapshot,
            model_request,
            turn_deadline(loop_deadline),
            &cancellation,
        )
        .await
        {
            Ok(response) => response,
            Err(driver) => break map_model_failure(&control, driver),
        };

        let round_usage = response.usage().copied().unwrap_or_default();
        if usage.add(round_usage).is_err() {
            break FailPath::Internal;
        }

        working.append_assistant(AssistantHistory {
            loop_id: id,
            request_index,
            model: snapshot.descriptor().model_ref.clone(),
            reasoning: snapshot.reasoning(),
            content: response.parts().to_vec(),
            finish_reason: response.finish_reason(),
            usage: round_usage,
        });
        requests = requests.saturating_add(1);

        let tool_calls: Vec<_> = response
            .parts()
            .iter()
            .filter_map(|part| part.as_tool_call())
            .collect();
        if tool_calls.is_empty() {
            let path = match response.finish_reason() {
                ModelFinishReason::Stop => FailPath::Completed,
                ModelFinishReason::Length => FailPath::OutputLimit,
                ModelFinishReason::ContentFiltered => FailPath::ContentFiltered,
                ModelFinishReason::Refused => FailPath::Refused,
                ModelFinishReason::Unknown | ModelFinishReason::ToolCalls => {
                    FailPath::InvalidModelResponse
                }
            };
            if path == FailPath::Completed {
                // Final-seal race (spec 20.6): steer/update and this seal
                // decision linearize on the same mutex. A steer accepted
                // before the seal keeps the loop alive and feeds the next
                // request; otherwise accepting closes and the loop ends
                // normally. A pending config alone never extends a final.
                match ctx.control.begin_final() {
                    FinalGate::Continue(changes) => {
                        apply_boundary(
                            &mut ctx,
                            &mut working,
                            &mut current_config,
                            &mut candidate_revision,
                            changes,
                        );
                        request_index = request_index.saturating_add(1);
                        continue;
                    }
                    FinalGate::Seal => break FailPath::Completed,
                }
            }
            break path;
        }
        if tool_rounds >= options.max_tool_rounds {
            for call in &tool_calls {
                working.append_tool_result(terminal_tool_result(
                    id,
                    request_index,
                    call,
                    ToolResultOutcome::Failed,
                    "tool round limit reached",
                ));
            }
            break FailPath::MaxToolRounds;
        }
        tool_rounds = tool_rounds.saturating_add(1);

        let enabled = all_enabled(snapshot.tools());
        let policy = snapshot.policy().cloned();
        ctx.publish(LoopState {
            loop_id: id,
            status: LoopStatus::RunningTools,
            request_index,
            config_revision: revision,
            model: Some(snapshot.descriptor().model_ref.clone()),
            pending_interaction: None,
        });

        let mut abort: Option<FailPath> = None;
        for call in &tool_calls {
            if abort.is_some() {
                working.append_tool_result(terminal_tool_result(
                    id,
                    request_index,
                    call,
                    ToolResultOutcome::Cancelled,
                    "tool batch interrupted",
                ));
                continue;
            }
            if cancellation.is_cancelled() {
                abort = Some(FailPath::Cancelled);
                working.append_tool_result(terminal_tool_result(
                    id,
                    request_index,
                    call,
                    ToolResultOutcome::Cancelled,
                    "tool batch interrupted",
                ));
                continue;
            }
            let Some(enabled_tool) = enabled.get(call.name()) else {
                working.append_tool_result(terminal_tool_result(
                    id,
                    request_index,
                    call,
                    ToolResultOutcome::Failed,
                    "tool unavailable",
                ));
                continue;
            };
            let invocation = match ToolInvocation::new(
                identity.session,
                identity.instance,
                identity.turn,
                call.tool_call_id().clone(),
                call.name().clone(),
                call.arguments().clone(),
            ) {
                Ok(invocation) => invocation,
                Err(_) => {
                    working.append_tool_result(terminal_tool_result(
                        id,
                        request_index,
                        call,
                        ToolResultOutcome::Failed,
                        "invalid tool invocation",
                    ));
                    continue;
                }
            };
            ctx.sink.try_emit(LoopEvent::ToolStarted {
                loop_id: id,
                request_index,
                call_id: call.tool_call_id().clone(),
                tool_name: call.name().clone(),
            });
            match run_tool_call(
                &mut ctx,
                request_index,
                &invocation,
                enabled_tool,
                policy.clone(),
                turn_deadline(loop_deadline),
            )
            .await
            {
                ToolStep::Result(result) => {
                    let output_bytes = result.output.content().byte_len();
                    let outcome = result.outcome;
                    working.append_tool_result(result);
                    ctx.sink.try_emit(LoopEvent::ToolFinished {
                        loop_id: id,
                        request_index,
                        call_id: call.tool_call_id().clone(),
                        outcome,
                        output_bytes,
                    });
                }
                ToolStep::End(path) => {
                    abort = Some(path);
                    working.append_tool_result(terminal_tool_result(
                        id,
                        request_index,
                        call,
                        ToolResultOutcome::Cancelled,
                        "tool batch interrupted",
                    ));
                }
            }
        }
        if let Some(path) = abort {
            break path;
        }
        request_index = request_index.saturating_add(1);
    };

    finish_loop(
        &mut ctx,
        end,
        working,
        usage.finish(),
        requests,
        tool_rounds,
    )
}

fn finish_loop(
    ctx: &mut LoopCtx<'_>,
    path: FailPath,
    working: WorkingHistory,
    usage: Usage,
    requests: u32,
    tool_rounds: u16,
) -> Arc<LoopReport> {
    let id = ctx.id;
    // The revision of the config that was actually applied: a pending update
    // that never reached a request boundary does not show up here.
    let revision = ctx.control.applied_revision();
    // The zero-based index of the last request actually issued, not the count.
    let last_request_index = requests.saturating_sub(1);
    // Read cancellation intent before sealing: cancel_reason is only readable
    // while the shared state is still a cancel marker.
    let intended = outcome_for(ctx.control, path);
    let finish = ctx.control.finish_once();
    let outcome = match finish {
        FinishSeal::Clean => intended,
        // A cancellation won the linearization point; it is authoritative.
        FinishSeal::CancelledPrior(reason) => LoopOutcome::Cancelled(reason),
        FinishSeal::AlreadyFinished => {
            // Exactly-once: a prior completion already sealed the loop and
            // published its report. Hand back the very same `Arc` so join/wait
            // compare equal (panic-after-publish contract). Unreachable inside
            // the single runner task; the fallback guards an as-yet-unpublished
            // seal.
            return ctx.control.published_report().unwrap_or_else(|| {
                Arc::new(LoopReport {
                    loop_id: id,
                    outcome: intended,
                    appended: working.into_appended(),
                    usage,
                    requests,
                    tool_rounds,
                    final_config_revision: revision,
                })
            });
        }
    };

    let report = Arc::new(LoopReport {
        loop_id: id,
        outcome: outcome.clone(),
        appended: working.into_appended(),
        usage,
        requests,
        tool_rounds,
        final_config_revision: revision,
    });

    publish_final_states(ctx, id, last_request_index, revision);
    let _ = ctx.completion_tx.send_replace(Some(Arc::clone(&report)));
    ctx.sink.try_emit(LoopEvent::Finished {
        loop_id: id,
        outcome: match &outcome {
            LoopOutcome::Completed => LoopOutcomeSummary::Completed,
            LoopOutcome::Cancelled(reason) => LoopOutcomeSummary::Cancelled(*reason),
            LoopOutcome::Failed(failure) => LoopOutcomeSummary::Failed(failure.kind),
        },
    });
    // Every ending path cancels the root token so provider temporary state is
    // released even when cancellation was not what ended the loop.
    ctx.control.cancellation().cancel();
    report
}

fn publish_final_states(
    ctx: &mut LoopCtx<'_>,
    id: LoopId,
    request_index: u32,
    revision: ConfigRevision,
) {
    let model = ctx.control.current_state().model;
    ctx.publish(LoopState {
        loop_id: id,
        status: LoopStatus::Finishing,
        request_index,
        config_revision: revision,
        model,
        pending_interaction: None,
    });
    ctx.publish(LoopState {
        loop_id: id,
        status: LoopStatus::Finished,
        request_index,
        config_revision: revision,
        model: ctx.control.current_state().model,
        pending_interaction: None,
    });
}

fn panic_report(
    control: &LoopControl,
    completion_tx: &watch::Sender<Option<Arc<LoopReport>>>,
) -> Arc<LoopReport> {
    let report = Arc::new(LoopReport {
        loop_id: control.id,
        outcome: LoopOutcome::Failed(failure(
            LoopFailureKind::Internal,
            DiagnosticCode::Internal,
            DiagnosticCategory::Internal,
            "loop runner panicked",
        )),
        appended: Arc::from([]),
        usage: Usage::default(),
        requests: 0,
        tool_rounds: 0,
        final_config_revision: ConfigRevision::INITIAL,
    });
    if matches!(control.finish_once(), FinishSeal::AlreadyFinished) {
        // Exactly-once: never overwrite a report already delivered by the
        // normal completion path. Return that same `Arc` so waiters still
        // observe one report identity (panic-after-publish case).
        return control.published_report().unwrap_or(report);
    }
    let state = LoopState::new(control.id, LoopStatus::Finished, 0, ConfigRevision::INITIAL);
    control.publish_state(state);
    let _ = completion_tx.send_replace(Some(Arc::clone(&report)));
    control.cancellation().cancel();
    report
}

enum PromptPrep {
    Ready(Vec<ModelMessage>),
    End(FailPath),
}

/// One request boundary: run the prompt provider under cancel/timeout/panic
/// isolation and hand back its messages.
async fn prepare_prompt(
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
        PortCallOutcome::Returned(Ok(prepared)) => PromptPrep::Ready(prepared.messages),
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

fn build_model_request(
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
async fn run_model(
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

    let context = ModelCallContext::new(
        ctx.identity.session,
        ctx.identity.instance,
        ctx.identity.turn,
        u16::try_from(request_index).unwrap_or(u16::MAX),
        cancellation.clone(),
        turn_deadline,
    );

    let (progress_tx, mut progress_rx) = mpsc::channel(MODEL_PROGRESS_CAPACITY);
    let run = driver.run_detailed(request, context, &progress_tx);
    tokio::pin!(run);
    let result = loop {
        tokio::select! {
            biased;
            result = &mut run => break result,
            value = progress_rx.recv() => match value {
                Some(ModelDriverProgress::TextDelta(delta)) => {
                    ctx.sink.try_emit(LoopEvent::OutputDelta {
                        loop_id: ctx.id,
                        request_index,
                        channel: OutputChannel::Text,
                        delta,
                    });
                }
                Some(ModelDriverProgress::ReasoningDelta(delta)) => {
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
    while let Ok(value) = progress_rx.try_recv() {
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

enum ToolStep {
    Result(ToolResultHistory),
    End(FailPath),
}

/// Executes one tool call: policy decision, optional interaction, and the
/// tool port itself. Ordinary tool failures become `Failed` results so the
/// model can continue; only cancelled/deadline/broken interactions end the
/// batch.
async fn run_tool_call(
    ctx: &mut LoopCtx<'_>,
    request_index: u32,
    invocation: &ToolInvocation,
    enabled: &EnabledTool,
    policy: Option<Arc<dyn ToolPolicy>>,
    turn_deadline: Instant,
) -> ToolStep {
    let cancellation = ctx.control.cancellation();

    let decision = match decide_policy(
        policy,
        invocation,
        enabled.spec.clone(),
        turn_deadline,
        &cancellation,
        ctx.options.policy_timeout,
    )
    .await
    {
        PolicyStep::Allow => None,
        PolicyStep::Denied(reason) => {
            return ToolStep::Result(denied_result(ctx.id, request_index, invocation, reason));
        }
        PolicyStep::RequireApproval(request) => Some(request),
        PolicyStep::Cancelled => return ToolStep::End(FailPath::Cancelled),
        PolicyStep::TurnDeadline => return ToolStep::End(FailPath::Deadline),
        PolicyStep::Invalid => return ToolStep::End(FailPath::Policy),
    };

    if let Some(request) = decision {
        let interaction_id = match InteractionId::new() {
            Ok(id) => id,
            Err(_) => return ToolStep::End(FailPath::Internal),
        };
        let (reply, receiver) = oneshot::channel();
        let answer = match wait_interaction(
            ctx,
            invocation,
            interaction_id,
            InteractionKind::Approval(request),
            turn_deadline,
            reply,
            receiver,
        )
        .await
        {
            Ok(answer) => answer,
            Err(path) => return ToolStep::End(path),
        };
        match answer {
            InteractionAnswer::Approval(ApprovalDecision::AllowOnce) => {}
            InteractionAnswer::Approval(ApprovalDecision::Deny) => {
                return ToolStep::Result(denied_result(ctx.id, request_index, invocation, None));
            }
            _ => return ToolStep::End(FailPath::Internal),
        }
    }

    match run_tool_port(
        ctx,
        request_index,
        invocation,
        Arc::clone(&enabled.implementation),
        turn_deadline,
    )
    .await
    {
        ToolPort::Completed(output) => {
            ToolStep::Result(completed_result(ctx.id, request_index, invocation, output))
        }
        ToolPort::RequestInput(request) => {
            let interaction_id = match InteractionId::new() {
                Ok(id) => id,
                Err(_) => return ToolStep::End(FailPath::Internal),
            };
            let (reply, receiver) = oneshot::channel();
            let answer = match wait_interaction(
                ctx,
                invocation,
                interaction_id,
                InteractionKind::ToolInput(request.clone()),
                turn_deadline,
                reply,
                receiver,
            )
            .await
            {
                Ok(answer) => answer,
                Err(path) => return ToolStep::End(path),
            };
            match answer {
                InteractionAnswer::ToolInput(answer) => {
                    let content = match answer.encode_result(&request) {
                        Ok(content) => content,
                        Err(_) => return ToolStep::End(FailPath::Internal),
                    };
                    if content.len() > ctx.options.limits.max_tool_output_bytes {
                        return ToolStep::Result(invocation_terminal_result(
                            ctx.id,
                            request_index,
                            invocation,
                            ToolResultOutcome::Failed,
                            "tool output too large",
                        ));
                    }
                    match ToolOutput::new(content) {
                        Ok(output) => ToolStep::Result(ToolResultHistory {
                            loop_id: ctx.id,
                            request_index,
                            call_id: invocation.tool_call_id().clone(),
                            tool_name: invocation.tool_name().clone(),
                            outcome: ToolResultOutcome::InputProvided,
                            output,
                        }),
                        Err(_) => ToolStep::Result(invocation_terminal_result(
                            ctx.id,
                            request_index,
                            invocation,
                            ToolResultOutcome::Failed,
                            "tool output too large",
                        )),
                    }
                }
                _ => ToolStep::End(FailPath::Internal),
            }
        }
        ToolPort::Failed => ToolStep::Result(invocation_terminal_result(
            ctx.id,
            request_index,
            invocation,
            ToolResultOutcome::Failed,
            "tool failed",
        )),
        ToolPort::End(path) => ToolStep::End(path),
    }
}

enum PolicyStep {
    Allow,
    Denied(Option<BoundedText>),
    RequireApproval(ApprovalRequest),
    Cancelled,
    TurnDeadline,
    Invalid,
}

async fn decide_policy(
    policy: Option<Arc<dyn ToolPolicy>>,
    invocation: &ToolInvocation,
    spec: ToolSpec,
    turn_deadline: Instant,
    cancellation: &CancellationToken,
    policy_timeout: Duration,
) -> PolicyStep {
    let Some(policy) = policy else {
        // The policy is optional in v0.4; without one tools run directly.
        return PolicyStep::Allow;
    };
    let outcome = run_port_call(
        cancellation,
        turn_deadline,
        policy_timeout,
        |child, deadline| {
            let request = ToolPolicyRequest {
                invocation: invocation.clone(),
                spec,
                cancellation: child,
                deadline,
            };
            policy.decide(request)
        },
    )
    .await;
    match outcome {
        PortCallOutcome::Returned(Ok(decision)) => {
            if !decision.validate().is_ok() {
                return PolicyStep::Invalid;
            }
            match decision {
                ToolDecision::Allow => PolicyStep::Allow,
                ToolDecision::Deny { reason } => PolicyStep::Denied(Some(reason)),
                ToolDecision::RequireApproval { request } => PolicyStep::RequireApproval(request),
            }
        }
        PortCallOutcome::Cancelled => PolicyStep::Cancelled,
        PortCallOutcome::DeadlineExceeded(DeadlineSource::Turn) => PolicyStep::TurnDeadline,
        // Fail-closed: an unavailable or panicking policy denies the call
        // rather than ending the loop.
        PortCallOutcome::Returned(Err(_))
        | PortCallOutcome::DeadlineExceeded(DeadlineSource::Port)
        | PortCallOutcome::InvalidDeadline(_)
        | PortCallOutcome::Panicked => PolicyStep::Denied(None),
    }
}

enum ToolPort {
    Completed(ToolOutput),
    RequestInput(ToolInputRequest),
    Failed,
    End(FailPath),
}

async fn run_tool_port(
    ctx: &mut LoopCtx<'_>,
    request_index: u32,
    invocation: &ToolInvocation,
    implementation: Arc<dyn crate::tools::Tool>,
    turn_deadline: Instant,
) -> ToolPort {
    let call_id = invocation.tool_call_id().clone();
    let cancellation = ctx.control.cancellation();
    let (progress_tx, mut progress_rx) = mpsc::channel(TOOL_PROGRESS_CAPACITY);

    let run = run_port_call(
        &cancellation,
        turn_deadline,
        ctx.options.tool_timeout,
        |child, deadline| {
            let context = ToolContext {
                cancellation: child,
                deadline,
                progress: ToolProgressSink::from_emitter(LoopToolProgress {
                    call_id: call_id.clone(),
                    sender: progress_tx.clone(),
                }),
            };
            implementation.execute(invocation.clone(), context)
        },
    );
    tokio::pin!(run);
    let result = loop {
        tokio::select! {
            biased;
            result = &mut run => break result,
            value = progress_rx.recv() => match value {
                Some((channel, progress)) => {
                    ctx.sink.try_emit(LoopEvent::ToolProgress {
                        loop_id: ctx.id,
                        request_index,
                        call_id: channel,
                        progress,
                    });
                }
                None => continue,
            },
        }
    };
    while let Ok(value) = progress_rx.try_recv() {
        ctx.sink.try_emit(LoopEvent::ToolProgress {
            loop_id: ctx.id,
            request_index,
            call_id: value.0,
            progress: value.1,
        });
    }

    match result {
        PortCallOutcome::Returned(Ok(ToolExecutionOutcome::Completed(output))) => {
            ToolPort::Completed(output)
        }
        PortCallOutcome::Returned(Ok(ToolExecutionOutcome::RequestInput(request))) => {
            ToolPort::RequestInput(request)
        }
        PortCallOutcome::Cancelled => ToolPort::End(FailPath::Cancelled),
        PortCallOutcome::DeadlineExceeded(DeadlineSource::Turn) => {
            ToolPort::End(FailPath::Deadline)
        }
        // A tool timeout/panic/error is an ordinary tool failure: the call
        // still receives a terminal result and the model can continue.
        PortCallOutcome::Returned(Err(_)) => ToolPort::Failed,
        PortCallOutcome::DeadlineExceeded(DeadlineSource::Port) => ToolPort::Failed,
        PortCallOutcome::InvalidDeadline(_) => ToolPort::End(FailPath::Internal),
        PortCallOutcome::Panicked => ToolPort::Failed,
    }
}

struct LoopToolProgress {
    call_id: ToolCallId,
    sender: mpsc::Sender<(ToolCallId, ToolProgress)>,
}

impl ToolProgressEmitter for LoopToolProgress {
    fn emit(&self, progress: ToolProgress) -> bool {
        if progress.validate().is_err() {
            return false;
        }
        self.sender
            .try_send((self.call_id.clone(), progress))
            .is_ok()
    }
}

async fn wait_interaction(
    ctx: &mut LoopCtx<'_>,
    invocation: &ToolInvocation,
    interaction_id: InteractionId,
    kind: InteractionKind,
    turn_deadline: Instant,
    reply: oneshot::Sender<InteractionAnswer>,
    mut receiver: oneshot::Receiver<InteractionAnswer>,
) -> Result<InteractionAnswer, FailPath> {
    let pending = PendingInteraction {
        interaction_id,
        turn_id: ctx.identity.turn,
        tool_call_id: invocation.tool_call_id().clone(),
        tool_name: invocation.tool_name().clone(),
        kind: kind.clone(),
    };
    let slot = InteractionSlot::new(interaction_id, kind, reply);
    if ctx.control.set_interaction(slot).is_err() {
        return Err(FailPath::Internal);
    }

    let mut waiting = ctx.control.current_state();
    waiting.status = LoopStatus::WaitingForInput;
    waiting.pending_interaction = Some(pending.clone());
    ctx.publish(waiting);
    ctx.sink.try_emit(LoopEvent::InteractionRequested {
        loop_id: ctx.id,
        interaction: pending,
    });

    let cancellation = ctx.control.cancellation();
    // The loop deadline is a hard upper bound even while waiting for user
    // input: when it wins, the loop ends as Cancelled(Deadline) through the
    // same control linearization as every other ending path.
    let deadline = TokioInstant::from_std(turn_deadline);
    let answer = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            ctx.control.take_interaction();
            return Err(FailPath::Cancelled);
        }
        _ = tokio::time::sleep_until(deadline) => {
            ctx.control.take_interaction();
            return Err(FailPath::Deadline);
        }
        answer = &mut receiver => match answer {
            Ok(answer) => answer,
            Err(_) => {
                ctx.control.take_interaction();
                return Err(FailPath::Interaction);
            }
        },
    };
    ctx.control.take_interaction();
    ctx.sink.try_emit(LoopEvent::InteractionResolved {
        loop_id: ctx.id,
        interaction_id,
    });

    let mut running = ctx.control.current_state();
    running.status = LoopStatus::RunningTools;
    running.pending_interaction = None;
    ctx.publish(running);
    Ok(answer)
}

fn completed_result(
    id: LoopId,
    request_index: u32,
    invocation: &ToolInvocation,
    output: ToolOutput,
) -> ToolResultHistory {
    ToolResultHistory {
        loop_id: id,
        request_index,
        call_id: invocation.tool_call_id().clone(),
        tool_name: invocation.tool_name().clone(),
        outcome: ToolResultOutcome::Success,
        output,
    }
}

fn denied_result(
    id: LoopId,
    request_index: u32,
    invocation: &ToolInvocation,
    reason: Option<BoundedText>,
) -> ToolResultHistory {
    let content = reason
        .map(|reason| reason.into_string())
        .unwrap_or_else(|| "tool denied".to_owned());
    ToolResultHistory {
        loop_id: id,
        request_index,
        call_id: invocation.tool_call_id().clone(),
        tool_name: invocation.tool_name().clone(),
        outcome: ToolResultOutcome::Denied,
        output: ToolOutput::new(content).expect("denial reason fits tool output bounds"),
    }
}

fn terminal_tool_result(
    id: LoopId,
    request_index: u32,
    call: &ToolCall,
    outcome: ToolResultOutcome,
    message: &str,
) -> ToolResultHistory {
    ToolResultHistory {
        loop_id: id,
        request_index,
        call_id: call.tool_call_id().clone(),
        tool_name: call.name().clone(),
        outcome,
        output: ToolOutput::new(message).expect("terminal tool message is valid text"),
    }
}

fn invocation_terminal_result(
    id: LoopId,
    request_index: u32,
    invocation: &ToolInvocation,
    outcome: ToolResultOutcome,
    message: &str,
) -> ToolResultHistory {
    ToolResultHistory {
        loop_id: id,
        request_index,
        call_id: invocation.tool_call_id().clone(),
        tool_name: invocation.tool_name().clone(),
        outcome,
        output: ToolOutput::new(message).expect("terminal tool message is valid text"),
    }
}

/// v0.3 Model/Tool ports still carry session identity. A loop owns no session,
/// so these fields receive a deterministic derivation of the loop id. The
/// bridge disappears when the session-shaped port fields are removed.
struct PortIdentity {
    session: SessionId,
    instance: SessionInstanceId,
    turn: TurnId,
}

fn port_identity(loop_id: LoopId) -> PortIdentity {
    let mut hex = String::with_capacity(32);
    for byte in loop_id.as_bytes() {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    PortIdentity {
        session: format!("ses_{hex}")
            .parse()
            .expect("loop-derived session id is canonical"),
        instance: format!("ins_{hex}")
            .parse()
            .expect("loop-derived instance id is canonical"),
        turn: format!("trn_{hex}")
            .parse()
            .expect("loop-derived turn id is canonical"),
    }
}

fn turn_deadline(loop_deadline: Option<TokioInstant>) -> Instant {
    match loop_deadline {
        Some(deadline) => deadline.into_std(),
        None => Instant::now() + LOOP_HORIZON,
    }
}

fn all_enabled(set: &ToolSet) -> EnabledTools {
    let names = set.frozen_specs().map(|spec| spec.name().clone()).collect();
    set.enabled_subset(&names)
}

fn outcome_for(control: &LoopControl, path: FailPath) -> LoopOutcome {
    match path {
        FailPath::Completed => LoopOutcome::Completed,
        FailPath::Cancelled => LoopOutcome::Cancelled(control.cancel_reason()),
        FailPath::Deadline => LoopOutcome::Cancelled(CancelReason::Deadline),
        FailPath::Prompt => LoopOutcome::Failed(failure(
            LoopFailureKind::Prompt,
            DiagnosticCode::ContextFailed,
            DiagnosticCategory::Context,
            "prompt preparation failed",
        )),
        FailPath::Model => LoopOutcome::Failed(failure(
            LoopFailureKind::Model,
            DiagnosticCode::ModelUnavailable,
            DiagnosticCategory::Model,
            "model request failed",
        )),
        FailPath::InvalidModelResponse => LoopOutcome::Failed(failure(
            LoopFailureKind::InvalidModelResponse,
            DiagnosticCode::ModelMalformedResponse,
            DiagnosticCategory::Model,
            "model returned an invalid response",
        )),
        FailPath::OutputLimit => LoopOutcome::Failed(failure(
            LoopFailureKind::OutputLimit,
            DiagnosticCode::ModelMalformedResponse,
            DiagnosticCategory::Model,
            "model output exceeded the configured limit",
        )),
        FailPath::Refused => LoopOutcome::Failed(failure(
            LoopFailureKind::Refused,
            DiagnosticCode::ModelMalformedResponse,
            DiagnosticCategory::Model,
            "model refused to answer",
        )),
        FailPath::ContentFiltered => LoopOutcome::Failed(failure(
            LoopFailureKind::ContentFiltered,
            DiagnosticCode::ModelMalformedResponse,
            DiagnosticCategory::Model,
            "model response was content filtered",
        )),
        FailPath::Policy => LoopOutcome::Failed(failure(
            LoopFailureKind::Policy,
            DiagnosticCode::PolicyFailed,
            DiagnosticCategory::Policy,
            "tool policy evaluation failed",
        )),
        FailPath::Interaction => LoopOutcome::Failed(failure(
            LoopFailureKind::Interaction,
            DiagnosticCode::InteractionNotFound,
            DiagnosticCategory::Internal,
            "pending interaction was not resolved",
        )),
        FailPath::MaxToolRounds => LoopOutcome::Failed(failure(
            LoopFailureKind::MaxToolRounds,
            DiagnosticCode::TurnBudgetExceeded,
            DiagnosticCategory::Internal,
            "maximum tool rounds reached",
        )),
        FailPath::Internal => LoopOutcome::Failed(failure(
            LoopFailureKind::Internal,
            DiagnosticCode::Internal,
            DiagnosticCategory::Internal,
            "loop runner hit an internal error",
        )),
    }
}

fn failure(
    kind: LoopFailureKind,
    code: DiagnosticCode,
    category: DiagnosticCategory,
    message: &'static str,
) -> LoopFailure {
    LoopFailure {
        kind,
        diagnostic: DiagnosticSummary::bounded_static(code, category, message, false),
    }
}

fn map_model_failure(control: &LoopControl, failure: ModelDriverFailure) -> FailPath {
    if control.cancellation().is_cancelled() {
        return FailPath::Cancelled;
    }
    if failure.deadline_source() == Some(DeadlineSource::Turn)
        && failure.error().kind() == crate::model::ModelErrorKind::Timeout
    {
        return FailPath::Deadline;
    }
    match failure.error().kind() {
        crate::model::ModelErrorKind::InvalidProviderResponse
        | crate::model::ModelErrorKind::IncompleteResponse
        | crate::model::ModelErrorKind::UnexpectedToolCall => FailPath::InvalidModelResponse,
        _ => FailPath::Model,
    }
}

fn publish(control: &LoopControl, sink: &mut LoopEventSink, state: LoopState) {
    control.publish_state(state.clone());
    sink.try_emit(LoopEvent::StateChanged { state });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailPath {
    Completed,
    Cancelled,
    Deadline,
    Prompt,
    Model,
    InvalidModelResponse,
    OutputLimit,
    Refused,
    ContentFiltered,
    Policy,
    Interaction,
    MaxToolRounds,
    Internal,
}
