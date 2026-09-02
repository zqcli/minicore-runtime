use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant as TokioInstant;
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
    ToolExecutionOutcome, ToolInputRequest, ToolInvocation, ToolOutput, ToolPolicy,
    ToolPolicyRequest, ToolProgress, ToolProgressEmitter, ToolProgressSink, ToolResultOutcome,
    ToolSpec,
};
use crate::value::BoundedText;

const TOOL_PROGRESS_CAPACITY: usize = 64;

pub(super) enum ToolStep {
    Result(ToolResultHistory),
    End(FailPath),
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
