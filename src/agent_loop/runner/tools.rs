use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{FailPath, LoopCtx};
use crate::agent_loop::LoopStatus;
use crate::agent_loop::control::InteractionSlot;
use crate::agent_loop::event::LoopEvent;
use crate::history::ToolResultHistory;
use crate::ids::{InteractionId, LoopId, ToolCallId};
use crate::interaction::{InteractionAnswer, InteractionKind, PendingInteraction};
use crate::port_call::{PortCallOutcome, run_port_call};
use crate::time::DeadlineSource;
use crate::tools::{
    ApprovalDecision, ApprovalRequest, EnabledTool, ToolContext, ToolDecision,
    ToolExecutionOutcome, ToolInputRequest, ToolInvocation, ToolName, ToolOutput, ToolPolicy,
    ToolPolicyRequest, ToolProgress, ToolProgressEmitter, ToolProgressSink, ToolResultOutcome,
    ToolSpec,
};
use crate::value::BoundedText;

const TOOL_PROGRESS_CAPACITY: usize = 64;

pub(super) enum ToolStep {
    Result(ToolResultHistory),
    End(FailPath),
}

pub(super) fn settle_calls(
    ctx: &LoopCtx<'_>,
    working: &mut crate::history::WorkingHistory,
    request_index: u32,
    calls: &[&crate::model::ToolCall],
    outcome: ToolResultOutcome,
    message: &'static str,
) {
    for call in calls {
        working.append_tool_result(terminal_tool_result(
            ctx.id,
            request_index,
            call.tool_call_id(),
            call.name(),
            outcome,
            message,
            ctx.options.limits.max_tool_output_bytes,
        ));
    }
}

pub(super) async fn run_tool_batch(
    ctx: &mut LoopCtx<'_>,
    working: &mut crate::history::WorkingHistory,
    request_index: u32,
    snapshot: &crate::execution::ExecutionConfig,
    calls: &[&crate::model::ToolCall],
) -> Option<FailPath> {
    let cancellation = ctx.control.cancellation();
    for (index, call) in calls.iter().enumerate() {
        let step = if cancellation.is_cancelled() {
            ToolStep::End(FailPath::Cancelled)
        } else {
            let enabled = snapshot
                .tools()
                .all_enabled()
                .get(call.name())
                .expect("assembler accepts only tools from this request's frozen set");
            let invocation = ToolInvocation {
                tool_call_id: call.tool_call_id().clone(),
                tool_name: call.name().clone(),
                arguments: call.arguments().clone(),
            };
            ctx.sink.try_emit(LoopEvent::ToolStarted {
                loop_id: ctx.id,
                request_index,
                call_id: call.tool_call_id().clone(),
                tool_name: call.name().clone(),
            });
            run_tool_call(
                ctx,
                request_index,
                &invocation,
                enabled,
                snapshot.policy().cloned(),
            )
            .await
        };
        match step {
            ToolStep::Result(result) => {
                ctx.sink.try_emit(LoopEvent::ToolFinished {
                    loop_id: ctx.id,
                    request_index,
                    call_id: call.tool_call_id().clone(),
                    outcome: result.outcome,
                    output_bytes: result.output.content().byte_len(),
                });
                working.append_tool_result(result);
            }
            ToolStep::End(path) => {
                settle_calls(
                    ctx,
                    working,
                    request_index,
                    &calls[index..],
                    ToolResultOutcome::Cancelled,
                    "tool batch interrupted",
                );
                return Some(path);
            }
        }
    }
    None
}

/// Executes one tool call: policy decision, optional interaction, and the
/// tool port itself. Ordinary tool failures become `Failed` results so the
/// model can continue; only cancelled/deadline/broken interactions end the
/// batch.
pub(super) async fn run_tool_call(
    ctx: &mut LoopCtx<'_>,
    request_index: u32,
    invocation: &ToolInvocation,
    enabled: &EnabledTool,
    policy: Option<Arc<dyn ToolPolicy>>,
) -> ToolStep {
    let cancellation = ctx.control.cancellation();

    let decision = match decide_policy(
        policy,
        invocation,
        enabled.spec.clone(),
        super::turn_deadline(ctx.options.deadline),
        &cancellation,
        ctx.options.policy_timeout,
    )
    .await
    {
        PolicyStep::Allow => None,
        PolicyStep::Denied(reason) => {
            return ToolStep::Result(denied_result(
                ctx.id,
                request_index,
                invocation,
                reason,
                ctx.options.limits.max_tool_output_bytes,
            ));
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
                return ToolStep::Result(denied_result(
                    ctx.id,
                    request_index,
                    invocation,
                    None,
                    ctx.options.limits.max_tool_output_bytes,
                ));
            }
            _ => return ToolStep::End(FailPath::Internal),
        }
    }

    match run_tool_port(
        ctx,
        request_index,
        invocation,
        Arc::clone(&enabled.implementation),
        super::turn_deadline(ctx.options.deadline),
    )
    .await
    {
        ToolPort::Completed(output) => ToolStep::Result(completed_result(
            ctx.id,
            request_index,
            invocation,
            output,
            ctx.options.limits.max_tool_output_bytes,
        )),
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
                    ToolStep::Result(provided_input_result(
                        ctx.id,
                        request_index,
                        invocation,
                        content,
                        ctx.options.limits.max_tool_output_bytes,
                    ))
                }
                _ => ToolStep::End(FailPath::Internal),
            }
        }
        ToolPort::Failed => ToolStep::Result(terminal_tool_result(
            ctx.id,
            request_index,
            invocation.tool_call_id(),
            invocation.tool_name(),
            ToolResultOutcome::Failed,
            "tool failed",
            ctx.options.limits.max_tool_output_bytes,
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
    let progress_dropped = Arc::new(AtomicU64::new(0));
    let (progress_tx, mut progress_rx) = mpsc::channel(TOOL_PROGRESS_CAPACITY);

    let progress_dropped_clone = Arc::clone(&progress_dropped);
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
                    dropped: Arc::clone(&progress_dropped_clone),
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
                    ctx.sink
                        .record_dropped(progress_dropped.swap(0, Ordering::Relaxed));
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
    ctx.sink
        .record_dropped(progress_dropped.swap(0, Ordering::Relaxed));
    while let Ok(value) = progress_rx.try_recv() {
        ctx.sink
            .record_dropped(progress_dropped.swap(0, Ordering::Relaxed));
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
        PortCallOutcome::Returned(Ok(ToolExecutionOutcome::RequestInput(request)))
            if request.validate().is_ok() =>
        {
            ToolPort::RequestInput(request)
        }
        PortCallOutcome::Returned(Ok(ToolExecutionOutcome::RequestInput(_))) => ToolPort::Failed,
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
    dropped: Arc<AtomicU64>,
}

impl ToolProgressEmitter for LoopToolProgress {
    fn emit(&self, progress: ToolProgress) -> bool {
        match self.sender.try_send((self.call_id.clone(), progress)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

async fn wait_interaction(
    ctx: &mut LoopCtx<'_>,
    invocation: &ToolInvocation,
    interaction_id: InteractionId,
    kind: InteractionKind,
    reply: oneshot::Sender<InteractionAnswer>,
    mut receiver: oneshot::Receiver<InteractionAnswer>,
) -> Result<InteractionAnswer, FailPath> {
    let pending = PendingInteraction {
        interaction_id,
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
    let timeout = async {
        match ctx.options.deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    let answer = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            ctx.control.take_interaction();
            return Err(FailPath::Cancelled);
        }
        _ = timeout => {
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

fn static_output(message: &'static str, max_bytes: usize) -> ToolOutput {
    let value = if message.len() <= max_bytes {
        message
    } else {
        ""
    };

    ToolOutput::new(value).expect("static or empty tool output fits absolute bounds")
}

fn completed_result(
    id: LoopId,
    request_index: u32,
    invocation: &ToolInvocation,
    output: ToolOutput,
    max_output_bytes: usize,
) -> ToolResultHistory {
    if output.content().byte_len() <= max_output_bytes {
        ToolResultHistory {
            loop_id: id,
            request_index,
            call_id: invocation.tool_call_id().clone(),
            tool_name: invocation.tool_name().clone(),
            outcome: ToolResultOutcome::Success,
            output,
        }
    } else {
        terminal_tool_result(
            id,
            request_index,
            invocation.tool_call_id(),
            invocation.tool_name(),
            ToolResultOutcome::Failed,
            "tool output too large",
            max_output_bytes,
        )
    }
}

fn provided_input_result(
    id: LoopId,
    request_index: u32,
    invocation: &ToolInvocation,
    content: String,
    max_output_bytes: usize,
) -> ToolResultHistory {
    if content.len() <= max_output_bytes {
        if let Ok(output) = ToolOutput::new(content) {
            return ToolResultHistory {
                loop_id: id,
                request_index,
                call_id: invocation.tool_call_id().clone(),
                tool_name: invocation.tool_name().clone(),
                outcome: ToolResultOutcome::InputProvided,
                output,
            };
        }
    }
    terminal_tool_result(
        id,
        request_index,
        invocation.tool_call_id(),
        invocation.tool_name(),
        ToolResultOutcome::Failed,
        "tool output too large",
        max_output_bytes,
    )
}

fn denied_result(
    id: LoopId,
    request_index: u32,
    invocation: &ToolInvocation,
    reason: Option<BoundedText>,
    max_output_bytes: usize,
) -> ToolResultHistory {
    let output = match reason {
        Some(reason) if reason.byte_len() <= max_output_bytes => {
            ToolOutput::new(reason.as_str()).expect("denial reason fits tool output bounds")
        }
        _ => static_output("tool denied", max_output_bytes),
    };
    ToolResultHistory {
        loop_id: id,
        request_index,
        call_id: invocation.tool_call_id().clone(),
        tool_name: invocation.tool_name().clone(),
        outcome: ToolResultOutcome::Denied,
        output,
    }
}

pub(super) fn terminal_tool_result(
    id: LoopId,
    request_index: u32,
    call_id: &ToolCallId,
    tool_name: &ToolName,
    outcome: ToolResultOutcome,
    message: &'static str,
    max_output_bytes: usize,
) -> ToolResultHistory {
    ToolResultHistory {
        loop_id: id,
        request_index,
        call_id: call_id.clone(),
        tool_name: tool_name.clone(),
        outcome,
        output: static_output(message, max_output_bytes),
    }
}
