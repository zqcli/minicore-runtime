#[cfg(test)]
use crate::tools::ToolSet;
#[cfg(test)]
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use crate::interaction::{InteractionAnswer, InteractionKind};
use crate::port_call::{PortCallOutcome, run_port_call};
use crate::time::DeadlineSource;
use crate::tools::{
    ApprovalDecision, EnabledTools, Tool, ToolContext, ToolDecision, ToolExecutionOutcome,
    ToolInputRequest, ToolInvocation, ToolName, ToolOutput, ToolPolicy, ToolPolicyRequest,
    ToolProgress, ToolResultOutcome, ToolSpec,
};
use crate::value::{BoundedText, MAX_JSON_BYTES, validate_json_size};

use super::runner_protocol::{SuspensionError, TurnSuspension};

mod support;

use support::{ExecutionResolution, PolicyResolution, progress_sink};

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
pub(crate) enum ToolDriverProgress {
    Started {
        tool_call_id: crate::ids::ToolCallId,
        tool_name: ToolName,
    },
    Update {
        tool_call_id: crate::ids::ToolCallId,
        progress: ToolProgress,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolDriverResult {
    pub(crate) output: ToolOutput,
    pub(crate) outcome: ToolResultOutcome,
}

pub(crate) struct ToolDriver {
    enabled: EnabledTools,
    policy: Option<Arc<dyn ToolPolicy>>,
    config: ToolDriverConfig,
}

impl ToolDriver {
    pub(crate) fn from_enabled(
        enabled: EnabledTools,
        policy: Option<Arc<dyn ToolPolicy>>,
        config: ToolDriverConfig,
    ) -> Result<Self, ToolDriverBuildError> {
        if !config.valid() {
            return Err(ToolDriverBuildError::InvalidConfiguration);
        }
        if !enabled.specs().is_empty() && policy.is_none() {
            return Err(ToolDriverBuildError::MissingPolicy);
        }
        Ok(Self {
            enabled,
            policy,
            config,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(
        tools: crate::tools::ToolSet,
        enabled: BTreeSet<ToolName>,
        policy: Option<Arc<dyn ToolPolicy>>,
        config: ToolDriverConfig,
    ) -> Result<Self, ToolDriverBuildError> {
        if !enabled.is_empty() && policy.is_none() {
            return Err(ToolDriverBuildError::MissingPolicy);
        }
        Self::from_enabled(tools.enabled_subset(&enabled), policy, config)
    }

    pub(crate) async fn run(
        &self,
        invocation: ToolInvocation,
        turn_deadline: Instant,
        cancellation: CancellationToken,
        suspensions: &mpsc::Sender<TurnSuspension>,
        progress: &mpsc::Sender<ToolDriverProgress>,
    ) -> Result<ToolDriverResult, SuspensionError> {
        let Some(enabled) = self.preflight(&invocation) else {
            return Ok(self.failed());
        };
        let implementation = Arc::clone(&enabled.implementation);
        let spec = enabled.spec.clone();
        match self
            .decide(&invocation, spec, turn_deadline, &cancellation)
            .await
        {
            PolicyResolution::Cancelled => Ok(self.cancelled()),
            PolicyResolution::DeadlineExceeded => Err(SuspensionError::DeadlineExceeded),
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
                            implementation,
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
                    implementation,
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

    fn preflight(&self, invocation: &ToolInvocation) -> Option<&crate::tools::EnabledTool> {
        if !invocation.arguments().is_object()
            || validate_json_size(invocation.arguments(), self.config.max_tool_input_bytes).is_err()
        {
            return None;
        }
        self.enabled.get(invocation.tool_name())
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
        let policy = Arc::clone(policy);
        match run_port_call(
            cancellation,
            turn_deadline,
            self.config.policy_timeout,
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
        .await
        {
            PortCallOutcome::Returned(Ok(decision)) if decision.validate().is_ok() => {
                PolicyResolution::Decision(decision)
            }
            PortCallOutcome::Cancelled => PolicyResolution::Cancelled,
            PortCallOutcome::DeadlineExceeded(DeadlineSource::Turn) => {
                PolicyResolution::DeadlineExceeded
            }
            PortCallOutcome::Returned(Ok(_))
            | PortCallOutcome::Returned(Err(_))
            | PortCallOutcome::DeadlineExceeded(DeadlineSource::Port)
            | PortCallOutcome::InvalidDeadline(_)
            | PortCallOutcome::Panicked => PolicyResolution::Denied,
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
            ExecutionResolution::DeadlineExceeded => Err(SuspensionError::DeadlineExceeded),
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
        let tool_call_id = invocation.tool_call_id().clone();
        let tool_name = invocation.tool_name().clone();
        let progress = progress.clone();
        match run_port_call(
            cancellation,
            turn_deadline,
            self.config.tool_timeout,
            |child, deadline| {
                let context = ToolContext {
                    cancellation: child,
                    deadline,
                    progress: progress_sink(tool_call_id.clone(), progress.clone()),
                };
                let _ = progress.try_send(ToolDriverProgress::Started {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                });
                tool.execute(invocation, context)
            },
        )
        .await
        {
            PortCallOutcome::Returned(Ok(outcome)) => ExecutionResolution::Outcome(outcome),
            PortCallOutcome::Cancelled => ExecutionResolution::Cancelled,
            PortCallOutcome::DeadlineExceeded(DeadlineSource::Turn) => {
                ExecutionResolution::DeadlineExceeded
            }
            PortCallOutcome::Returned(Err(_))
            | PortCallOutcome::DeadlineExceeded(DeadlineSource::Port)
            | PortCallOutcome::InvalidDeadline(_)
            | PortCallOutcome::Panicked => ExecutionResolution::Failed,
        }
    }

    fn completed(&self, output: ToolOutput) -> ToolDriverResult {
        if output.content().byte_len() > self.config.max_tool_output_bytes {
            return self.failed();
        }
        ToolDriverResult {
            output,
            outcome: ToolResultOutcome::Success,
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

fn valid_timeout(timeout: Duration) -> bool {
    !timeout.is_zero() && timeout <= MAX_PORT_TIMEOUT
}
fn static_output(text: &'static str, maximum: usize) -> ToolOutput {
    let text = &text[..text.len().min(maximum)];
    match ToolOutput::new(text) {
        Ok(output) => output,
        Err(_) => unreachable!("validated static tool output is invalid"),
    }
}

#[cfg(test)]
mod tests;
