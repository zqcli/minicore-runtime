use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use crate::session::{InteractionAnswer, InteractionKind};
use crate::tools::{
    ApprovalDecision, Tool, ToolContext, ToolDecision, ToolExecutionOutcome, ToolInputRequest,
    ToolInvocation, ToolName, ToolOutput, ToolPolicy, ToolPolicyRequest, ToolProgress,
    ToolResultOutcome, ToolSet, ToolSpec,
};
use crate::value::{BoundedText, MAX_JSON_BYTES, validate_json_size};

use super::runner_protocol::{SuspensionError, TurnSuspension};

mod support;

use support::progress_sink;

const MAX_PORT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const FAILED_TEXT: &str = "tool failed";
const DENIED_TEXT: &str = "tool denied";
const CANCELLED_TEXT: &str = "tool cancelled";
pub(crate) struct ToolDriverConfig {
    policy_timeout: Duration,
    tool_timeout: Duration,
    max_tool_input_bytes: usize,
    max_tool_output_bytes: usize,
}

impl ToolDriverConfig {
    pub(crate) fn from_kernel_values(
        policy_timeout: Duration,
        tool_timeout: Duration,
        max_tool_input_bytes: usize,
        max_tool_output_bytes: usize,
    ) -> Self {
        Self {
            policy_timeout,
            tool_timeout,
            max_tool_input_bytes,
            max_tool_output_bytes,
        }
    }

    fn valid(&self) -> bool {
        valid_timeout(self.policy_timeout)
            && valid_timeout(self.tool_timeout)
            && (1..=MAX_JSON_BYTES).contains(&self.max_tool_input_bytes)
            && (1..=BoundedText::MAX_BYTES).contains(&self.max_tool_output_bytes)
    }
}
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ToolDriverBuildError {
    #[error("tool driver configuration is invalid")]
    InvalidConfiguration,
    #[error("enabled tools require a tool policy")]
    MissingPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolDriverProgress {
    pub(crate) tool_call_id: crate::ids::ToolCallId,
    pub(crate) progress: ToolProgress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolDriverResult {
    pub(crate) output: ToolOutput,
    pub(crate) outcome: ToolResultOutcome,
}

pub(crate) struct ToolDriver {
    tools: ToolSet,
    enabled: BTreeSet<ToolName>,
    policy: Option<Arc<dyn ToolPolicy>>,
    config: ToolDriverConfig,
}

impl ToolDriver {
    pub(crate) fn new(
        tools: ToolSet,
        enabled: BTreeSet<ToolName>,
        policy: Option<Arc<dyn ToolPolicy>>,
        config: ToolDriverConfig,
    ) -> Result<Self, ToolDriverBuildError> {
        if !config.valid() {
            return Err(ToolDriverBuildError::InvalidConfiguration);
        }
        if !enabled.is_empty() && policy.is_none() {
            return Err(ToolDriverBuildError::MissingPolicy);
        }
        Ok(Self {
            tools,
            enabled,
            policy,
            config,
        })
    }

    pub(crate) async fn run(
        &self,
        invocation: ToolInvocation,
        turn_deadline: Instant,
        cancellation: CancellationToken,
        suspensions: &mpsc::Sender<TurnSuspension>,
        progress: &mpsc::Sender<ToolDriverProgress>,
    ) -> Result<ToolDriverResult, SuspensionError> {
        let Some((tool, spec)) = self.preflight(&invocation) else {
            return Ok(self.failed());
        };
        match self
            .decide(&invocation, spec, turn_deadline, &cancellation)
            .await
        {
            PolicyResolution::Cancelled => Ok(self.cancelled()),
            PolicyResolution::Denied => Ok(self.denied(None)),
            PolicyResolution::Decision(ToolDecision::Deny { reason }) => {
                Ok(self.denied(Some(reason)))
            }
            PolicyResolution::Decision(ToolDecision::RequireApproval { request }) => {
                let kind = InteractionKind::Approval(request);
                let answer =
                    wait_for_answer(&invocation, kind, turn_deadline, &cancellation, suspensions)
                        .await?;
                match answer {
                    InteractionAnswer::Approval(ApprovalDecision::AllowOnce) => {
                        self.execute(
                            tool,
                            invocation,
                            turn_deadline,
                            cancellation,
                            suspensions,
                            progress,
                        )
                        .await
                    }
                    InteractionAnswer::Approval(ApprovalDecision::Deny) => Ok(self.denied(None)),
                    _ => Err(SuspensionError::InvalidState),
                }
            }
            PolicyResolution::Decision(ToolDecision::Allow) => {
                self.execute(
                    tool,
                    invocation,
                    turn_deadline,
                    cancellation,
                    suspensions,
                    progress,
                )
                .await
            }
        }
    }

    fn preflight(&self, invocation: &ToolInvocation) -> Option<(Arc<dyn Tool>, ToolSpec)> {
        if !self.enabled.contains(invocation.tool_name())
            || !invocation.arguments().is_object()
            || validate_json_size(invocation.arguments(), self.config.max_tool_input_bytes).is_err()
        {
            return None;
        }
        let tool = self.tools.get(invocation.tool_name())?;
        let spec = self.tools.frozen_spec(invocation.tool_name())?.clone();
        if spec.name() != invocation.tool_name() {
            return None;
        }
        Some((tool, spec))
    }

    async fn decide(
        &self,
        invocation: &ToolInvocation,
        spec: ToolSpec,
        turn_deadline: Instant,
        cancellation: &CancellationToken,
    ) -> PolicyResolution {
        if cancellation.is_cancelled() {
            return PolicyResolution::Cancelled;
        }
        let Some(policy) = self.policy.as_ref() else {
            return PolicyResolution::Denied;
        };
        let Some((deadline, adapter_deadline)) =
            effective_deadline(turn_deadline, self.config.policy_timeout)
        else {
            return PolicyResolution::Denied;
        };
        if TokioInstant::now() >= deadline {
            return PolicyResolution::Denied;
        }
        let child = cancellation.child_token();
        let request = ToolPolicyRequest {
            invocation: invocation.clone(),
            spec,
            cancellation: child.clone(),
            deadline: adapter_deadline,
        };
        let future = match catch_unwind(AssertUnwindSafe(|| policy.decide(request))) {
            Ok(future) => AssertUnwindSafe(future).catch_unwind(),
            Err(_) => return PolicyResolution::Denied,
        };
        tokio::pin!(future);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                child.cancel();
                return PolicyResolution::Cancelled;
            }
            _ = tokio::time::sleep_until(deadline) => {
                child.cancel();
                return PolicyResolution::Denied;
            }
            result = &mut future => result,
        };
        match result {
            Ok(Ok(decision)) if decision.validate().is_ok() => PolicyResolution::Decision(decision),
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => PolicyResolution::Denied,
        }
    }

    async fn execute(
        &self,
        tool: Arc<dyn Tool>,
        invocation: ToolInvocation,
        turn_deadline: Instant,
        cancellation: CancellationToken,
        suspensions: &mpsc::Sender<TurnSuspension>,
        progress: &mpsc::Sender<ToolDriverProgress>,
    ) -> Result<ToolDriverResult, SuspensionError> {
        match self
            .execute_once(
                tool,
                invocation.clone(),
                turn_deadline,
                &cancellation,
                progress,
            )
            .await
        {
            ExecutionResolution::Cancelled => Ok(self.cancelled()),
            ExecutionResolution::Failed => Ok(self.failed()),
            ExecutionResolution::Outcome(ToolExecutionOutcome::Completed(output)) => {
                Ok(self.completed(output))
            }
            ExecutionResolution::Outcome(ToolExecutionOutcome::RequestInput(request)) => {
                let Some(request) = checked_input_request(request) else {
                    return Ok(self.failed());
                };
                let kind = InteractionKind::ToolInput(request.clone());
                let answer =
                    wait_for_answer(&invocation, kind, turn_deadline, &cancellation, suspensions)
                        .await?;
                match answer {
                    InteractionAnswer::ToolInput(answer) => {
                        let Ok(content) = answer.encode_result(&request) else {
                            return Err(SuspensionError::InvalidState);
                        };
                        if content.len() > self.config.max_tool_output_bytes {
                            return Ok(self.failed());
                        }
                        match ToolOutput::new(content) {
                            Ok(output) => Ok(ToolDriverResult {
                                output,
                                outcome: ToolResultOutcome::InputProvided,
                            }),
                            Err(_) => Ok(self.failed()),
                        }
                    }
                    _ => Err(SuspensionError::InvalidState),
                }
            }
        }
    }

    async fn execute_once(
        &self,
        tool: Arc<dyn Tool>,
        invocation: ToolInvocation,
        turn_deadline: Instant,
        cancellation: &CancellationToken,
        progress: &mpsc::Sender<ToolDriverProgress>,
    ) -> ExecutionResolution {
        if cancellation.is_cancelled() {
            return ExecutionResolution::Cancelled;
        }
        let Some((deadline, adapter_deadline)) =
            effective_deadline(turn_deadline, self.config.tool_timeout)
        else {
            return ExecutionResolution::Failed;
        };
        if TokioInstant::now() >= deadline {
            return ExecutionResolution::Failed;
        }
        let child = cancellation.child_token();
        let context = ToolContext {
            cancellation: child.clone(),
            deadline: adapter_deadline,
            progress: progress_sink(invocation.tool_call_id().clone(), progress.clone()),
        };
        let future = match catch_unwind(AssertUnwindSafe(|| tool.execute(invocation, context))) {
            Ok(future) => AssertUnwindSafe(future).catch_unwind(),
            Err(_) => return ExecutionResolution::Failed,
        };
        tokio::pin!(future);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                child.cancel();
                return ExecutionResolution::Cancelled;
            }
            _ = tokio::time::sleep_until(deadline) => {
                child.cancel();
                return ExecutionResolution::Failed;
            }
            result = &mut future => result,
        };
        match result {
            Ok(Ok(outcome)) => ExecutionResolution::Outcome(outcome),
            Ok(Err(_)) | Err(_) => ExecutionResolution::Failed,
        }
    }

    fn completed(&self, output: ToolOutput) -> ToolDriverResult {
        if output.content().byte_len() > self.config.max_tool_output_bytes {
            return self.failed();
        }
        match ToolOutput::new(output.content().as_str()) {
            Ok(output) => ToolDriverResult {
                output,
                outcome: ToolResultOutcome::Success,
            },
            Err(_) => self.failed(),
        }
    }

    fn denied(&self, reason: Option<BoundedText>) -> ToolDriverResult {
        let output = reason
            .filter(|reason| reason.byte_len() <= self.config.max_tool_output_bytes)
            .and_then(|reason| ToolOutput::new(reason.as_str()).ok())
            .unwrap_or_else(|| static_output(DENIED_TEXT, self.config.max_tool_output_bytes));
        ToolDriverResult {
            output,
            outcome: ToolResultOutcome::Denied,
        }
    }

    fn failed(&self) -> ToolDriverResult {
        self.static_result(FAILED_TEXT, ToolResultOutcome::Failed)
    }

    fn cancelled(&self) -> ToolDriverResult {
        self.static_result(CANCELLED_TEXT, ToolResultOutcome::Cancelled)
    }

    fn static_result(&self, text: &'static str, outcome: ToolResultOutcome) -> ToolDriverResult {
        ToolDriverResult {
            output: static_output(text, self.config.max_tool_output_bytes),
            outcome,
        }
    }
}
enum PolicyResolution {
    Decision(ToolDecision),
    Denied,
    Cancelled,
}

enum ExecutionResolution {
    Outcome(ToolExecutionOutcome),
    Failed,
    Cancelled,
}

async fn wait_for_answer(
    invocation: &ToolInvocation,
    kind: InteractionKind,
    turn_deadline: Instant,
    cancellation: &CancellationToken,
    suspensions: &mpsc::Sender<TurnSuspension>,
) -> Result<InteractionAnswer, SuspensionError> {
    if cancellation.is_cancelled() {
        return Err(SuspensionError::Cancelled);
    }
    let deadline = TokioInstant::from_std(turn_deadline);
    if TokioInstant::now() >= deadline {
        return Err(SuspensionError::DeadlineExceeded);
    }
    let (resume, receiver) = tokio::sync::oneshot::channel();
    let suspension = TurnSuspension {
        turn_id: invocation.turn_id(),
        tool_call_id: invocation.tool_call_id().clone(),
        tool_name: invocation.tool_name().clone(),
        kind: kind.clone(),
        resume,
    };
    let send_result = {
        let send = suspensions.send(suspension);
        tokio::pin!(send);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SuspensionError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => {
                return Err(SuspensionError::DeadlineExceeded);
            }
            result = &mut send => result,
        }
    };
    if send_result.is_err() {
        return Err(SuspensionError::RuntimeClosed);
    }
    tokio::pin!(receiver);
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(SuspensionError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => {
            return Err(SuspensionError::DeadlineExceeded);
        }
        result = &mut receiver => result,
    };
    match result {
        Ok(Ok(answer)) if answer.validate(&kind).is_ok() => Ok(answer),
        Ok(Ok(_)) => Err(SuspensionError::InvalidState),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(SuspensionError::RuntimeClosed),
    }
}

fn checked_input_request(request: ToolInputRequest) -> Option<ToolInputRequest> {
    ToolInputRequest::new(
        request.prompt().as_str(),
        request.choices().to_vec(),
        request.answer_kind(),
    )
    .ok()
}

fn effective_deadline(turn_deadline: Instant, timeout: Duration) -> Option<EffectiveDeadline> {
    let configured = TokioInstant::now().checked_add(timeout)?;
    let deadline = TokioInstant::from_std(turn_deadline).min(configured);
    Some((deadline, deadline.into_std()))
}

fn valid_timeout(timeout: Duration) -> bool {
    !timeout.is_zero() && timeout <= MAX_PORT_TIMEOUT
}
fn static_output(text: &'static str, maximum: usize) -> ToolOutput {
    let text = &text[..text.len().min(maximum)];
    ToolOutput::new(text).expect("static tool result output is valid")
}

type EffectiveDeadline = (TokioInstant, Instant);

#[cfg(test)]
mod tests;
