use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::compaction::CompactionDriver;
use crate::config::{CompactionConfig, KernelConfig, SemanticLimits, SessionSpec};
use crate::context::{ContextDriver, ContextError};
use crate::conversation::ConversationView;
use crate::ids::{SessionId, SessionInstanceId, TurnId};
use crate::model::{
    ModelDriver, ModelDriverConfig, ModelError, ModelLimits, SemanticLimitsSnapshot,
};
use crate::prompt::{PromptBuilder, PromptError};
use crate::session::{SessionBindingError, SessionBindings};

use super::runner_protocol::{RunnerEvent, RunnerProgress};
use super::tool_driver::{ToolDriver, ToolDriverBuildError, ToolDriverConfig};

const MAX_PORT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
pub(crate) struct TurnRunnerKernel {
    model_call_timeout: Duration,
    tool_call_timeout: Duration,
    policy_timeout: Duration,
    context_timeout: Duration,
    retry_attempts: u8,
    retry_base_delay: Duration,
    limits: SemanticLimits,
}

impl TurnRunnerKernel {
    pub(crate) fn from_kernel(kernel: &KernelConfig) -> Result<Self, TurnRunnerRequestError> {
        kernel
            .validate()
            .map_err(|_| TurnRunnerRequestError::Configuration)?;
        Ok(Self {
            model_call_timeout: kernel.model_call_timeout,
            tool_call_timeout: kernel.tool_call_timeout,
            policy_timeout: kernel.policy_timeout,
            context_timeout: kernel.context_timeout,
            retry_attempts: kernel.retry_policy.max_attempts(),
            retry_base_delay: kernel.retry_policy.base_delay(),
            limits: kernel.limits.clone(),
        })
    }

    fn validate(&self) -> Result<(), TurnRunnerRequestError> {
        if !valid_timeout(self.model_call_timeout)
            || !valid_timeout(self.tool_call_timeout)
            || !valid_timeout(self.policy_timeout)
            || !valid_timeout(self.context_timeout)
            || !(1..=4).contains(&self.retry_attempts)
            || self.retry_base_delay > Duration::from_secs(30)
            || self.limits.validate().is_err()
        {
            return Err(TurnRunnerRequestError::Configuration);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TurnRunnerIdentity {
    pub(crate) session_id: SessionId,
    pub(crate) instance_id: SessionInstanceId,
    pub(crate) turn_id: TurnId,
}

pub(crate) struct TurnRunnerControl {
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Instant,
    pub(crate) critical_tx: mpsc::Sender<RunnerEvent>,
    pub(crate) progress_tx: mpsc::Sender<RunnerProgress>,
}

pub(crate) struct TurnRunnerRequest {
    pub(crate) session_id: SessionId,
    pub(crate) instance_id: SessionInstanceId,
    pub(crate) turn_id: TurnId,
    pub(crate) spec: SessionSpec,
    pub(crate) effective_max_tool_rounds: u16,
    pub(crate) bindings: SessionBindings,
    pub(crate) conversation: ConversationView,
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Instant,
    pub(crate) kernel: TurnRunnerKernel,
    pub(crate) critical_tx: mpsc::Sender<RunnerEvent>,
    pub(crate) progress_tx: mpsc::Sender<RunnerProgress>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum TurnRunnerRequestError {
    #[error("turn runner configuration is invalid")]
    Configuration,
    #[error("turn runner bindings are invalid")]
    Bindings,
    #[error("turn runner conversation is invalid")]
    Conversation,
    #[error("turn runner model descriptor is invalid")]
    ModelDescriptor,
}

impl TurnRunnerRequest {
    pub(crate) fn new(
        identity: TurnRunnerIdentity,
        spec: SessionSpec,
        effective_max_tool_rounds: u16,
        bindings: SessionBindings,
        conversation: ConversationView,
        kernel: TurnRunnerKernel,
        control: TurnRunnerControl,
    ) -> Result<Self, TurnRunnerRequestError> {
        kernel.validate()?;
        if !(1..=spec.max_tool_rounds).contains(&effective_max_tool_rounds)
            || effective_max_tool_rounds > kernel.limits.max_tool_rounds
        {
            return Err(TurnRunnerRequestError::Configuration);
        }
        bindings
            .validate(&spec, &kernel.limits)
            .map_err(map_binding_error)?;
        let projection = conversation
            .validated_prompt_projection(&spec, &kernel.limits)
            .map_err(|_| TurnRunnerRequestError::Conversation)?;
        if projection.active_turn_id() != Some(identity.turn_id)
            || projection
                .active_turn_execution()
                .is_none_or(|execution| execution.max_tool_rounds != effective_max_tool_rounds)
        {
            return Err(TurnRunnerRequestError::Conversation);
        }
        let _ = descriptor_model_limits(&bindings)?;
        Ok(Self {
            session_id: identity.session_id,
            instance_id: identity.instance_id,
            turn_id: identity.turn_id,
            spec,
            effective_max_tool_rounds,
            bindings,
            conversation,
            cancellation: control.cancellation,
            deadline: control.deadline,
            kernel,
            critical_tx: control.critical_tx,
            progress_tx: control.progress_tx,
        })
    }
}

pub(super) struct TurnRunnerContext {
    pub(super) session_id: SessionId,
    pub(super) instance_id: SessionInstanceId,
    pub(super) turn_id: TurnId,
    pub(super) spec: SessionSpec,
    pub(super) effective_max_tool_rounds: u16,
    pub(super) conversation: ConversationView,
    pub(super) cancellation: CancellationToken,
    pub(super) deadline: Instant,
    pub(super) critical_tx: mpsc::Sender<RunnerEvent>,
    pub(super) progress_tx: mpsc::Sender<RunnerProgress>,
    pub(super) model_limits: ModelLimits,
    pub(super) limits: SemanticLimits,
    pub(super) prompt: PromptBuilder,
    pub(super) context: ContextDriver,
    pub(super) compaction: Option<TurnCompaction>,
    pub(super) model: ModelDriver,
    pub(super) tools: ToolDriver,
}

pub(super) struct TurnCompaction {
    pub(super) driver: CompactionDriver,
    pub(super) trigger_tokens: u64,
    pub(super) target_tokens: u64,
}

impl TurnRunnerContext {
    pub(super) fn new(request: TurnRunnerRequest) -> Result<Self, TurnRunnerRequestError> {
        let model_limits = descriptor_model_limits(&request.bindings)?;
        let tool_specs = request
            .bindings
            .tools
            .specs_for(&request.spec.enabled_tools);
        let prompt = PromptBuilder::new(&request.spec, tool_specs, request.kernel.limits.clone())
            .map_err(map_prompt_error)?;
        let context = ContextDriver::new(
            request.bindings.context.clone(),
            request.kernel.context_timeout,
            request.kernel.limits.clone(),
        )
        .map_err(map_context_error)?;
        let compaction = match &request.spec.compaction {
            CompactionConfig::Disabled => None,
            CompactionConfig::Enabled {
                trigger_tokens,
                target_tokens,
            } => Some(TurnCompaction {
                // Compaction is external context shaping, so it shares the explicit Context
                // adapter timeout while still being capped by the absolute Turn deadline.
                driver: CompactionDriver::new(
                    request.bindings.compaction.clone(),
                    request.kernel.context_timeout,
                    request.kernel.limits.clone(),
                )
                .map_err(map_compaction_error)?,
                trigger_tokens: *trigger_tokens,
                target_tokens: *target_tokens,
            }),
        };
        let model = ModelDriver::new(
            request.bindings.model.clone(),
            ModelDriverConfig::from_kernel_values(
                request.kernel.model_call_timeout,
                request.kernel.retry_attempts,
                request.kernel.retry_base_delay,
                SemanticLimitsSnapshot::from_kernel_values(
                    request.kernel.limits.max_tool_count,
                    request.kernel.limits.max_tool_name_bytes,
                    request.kernel.limits.max_tool_schema_bytes,
                    request.kernel.limits.max_tool_input_bytes,
                    request.kernel.limits.max_model_text_bytes_per_round,
                    request.kernel.limits.max_model_reasoning_bytes_per_round,
                ),
            ),
        )
        .map_err(map_model_error)?;
        let tools = ToolDriver::new(
            request.bindings.tools.clone(),
            request.spec.enabled_tools.clone(),
            request.bindings.tool_policy.clone(),
            ToolDriverConfig::from_kernel_values(
                request.kernel.policy_timeout,
                request.kernel.tool_call_timeout,
                request.kernel.limits.max_tool_input_bytes,
                request.kernel.limits.max_tool_output_bytes,
            ),
        )
        .map_err(map_tool_driver_error)?;
        Ok(Self {
            session_id: request.session_id,
            instance_id: request.instance_id,
            turn_id: request.turn_id,
            spec: request.spec,
            effective_max_tool_rounds: request.effective_max_tool_rounds,
            conversation: request.conversation,
            cancellation: request.cancellation,
            deadline: request.deadline,
            critical_tx: request.critical_tx,
            progress_tx: request.progress_tx,
            model_limits,
            limits: request.kernel.limits,
            prompt,
            context,
            compaction,
            model,
            tools,
        })
    }

    pub(super) fn validate_conversation(
        &self,
        conversation: &ConversationView,
    ) -> Result<(), TurnRunnerRequestError> {
        let projection = conversation
            .validated_prompt_projection(&self.spec, &self.limits)
            .map_err(|_| TurnRunnerRequestError::Conversation)?;
        if projection.active_turn_id() != Some(self.turn_id)
            || projection
                .active_turn_execution()
                .is_none_or(|execution| execution.max_tool_rounds != self.effective_max_tool_rounds)
        {
            return Err(TurnRunnerRequestError::Conversation);
        }
        Ok(())
    }
}

impl fmt::Debug for TurnRunnerRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnRunnerRequest")
            .field("session_id", &self.session_id)
            .field("instance_id", &self.instance_id)
            .field("turn_id", &self.turn_id)
            .field("model", &self.spec.model)
            .field("effective_max_tool_rounds", &self.effective_max_tool_rounds)
            .field("conversation", &self.conversation)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

fn descriptor_model_limits(
    bindings: &SessionBindings,
) -> Result<ModelLimits, TurnRunnerRequestError> {
    let descriptor = catch_unwind(AssertUnwindSafe(|| bindings.model.descriptor().clone()))
        .map_err(|_| TurnRunnerRequestError::ModelDescriptor)?;
    descriptor
        .validate()
        .map_err(|_| TurnRunnerRequestError::ModelDescriptor)?;
    let context_window = u32::try_from(descriptor.context_window).unwrap_or(u32::MAX);
    ModelLimits::new(Some(context_window), None)
        .map_err(|_| TurnRunnerRequestError::ModelDescriptor)
}

fn valid_timeout(timeout: Duration) -> bool {
    !timeout.is_zero() && timeout <= MAX_PORT_TIMEOUT
}

fn map_binding_error(_error: SessionBindingError) -> TurnRunnerRequestError {
    TurnRunnerRequestError::Bindings
}

fn map_prompt_error(_error: PromptError) -> TurnRunnerRequestError {
    TurnRunnerRequestError::Configuration
}

fn map_context_error(_error: ContextError) -> TurnRunnerRequestError {
    TurnRunnerRequestError::Configuration
}

fn map_compaction_error(_error: crate::compaction::CompactionError) -> TurnRunnerRequestError {
    TurnRunnerRequestError::Configuration
}

fn map_model_error(_error: ModelError) -> TurnRunnerRequestError {
    TurnRunnerRequestError::ModelDescriptor
}

fn map_tool_driver_error(_error: ToolDriverBuildError) -> TurnRunnerRequestError {
    TurnRunnerRequestError::Bindings
}
