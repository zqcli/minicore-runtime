use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::bindings::{SessionBindingError, SessionBindings};
use crate::compaction::CompactionDriver;
use crate::config::{CompactionConfig, KernelConfig, SemanticLimits, SessionSpec};
use crate::context::ContextDriver;
use crate::model::{ModelDriver, ModelDriverConfig, ModelLimits, SemanticLimitsSnapshot};
use crate::prompt::PromptBuilder;

use super::tool_driver::{ToolDriver, ToolDriverConfig};

pub(crate) struct SessionEnvironment {
    pub(super) spec: SessionSpec,
    pub(super) limits: SemanticLimits,
    pub(super) model_limits: ModelLimits,
    pub(super) model: ModelDriver,
    pub(super) tools: ToolDriver,
    pub(super) context: ContextDriver,
    pub(super) compaction: Option<SessionCompaction>,
    pub(super) prompt: PromptBuilder,
    session_channels: SessionChannelCapacities,
}

pub(crate) struct SessionCompaction {
    pub(super) driver: CompactionDriver,
    pub(super) trigger_tokens: u64,
    pub(super) target_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionEnvironmentError {
    #[error("session environment kernel is invalid")]
    InvalidKernel,
    #[error("session environment bindings are invalid")]
    Bindings(SessionBindingError),
    #[error("session environment driver construction failed")]
    Driver,
}

#[derive(Clone, Copy)]
pub(crate) struct SessionChannelCapacities {
    pub(crate) command: usize,
    pub(crate) runner: usize,
    pub(crate) event: usize,
}

impl SessionEnvironment {
    pub(crate) fn build(
        kernel: &KernelConfig,
        spec: &SessionSpec,
        bindings: &SessionBindings,
    ) -> Result<Arc<Self>, SessionEnvironmentError> {
        kernel
            .validate()
            .map_err(|_| SessionEnvironmentError::InvalidKernel)?;
        let validated = bindings
            .freeze(spec, &kernel.limits)
            .map_err(SessionEnvironmentError::Bindings)?;
        let model_config = ModelDriverConfig::from_kernel_values(
            kernel.model_call_timeout,
            kernel.retry_policy.max_attempts(),
            kernel.retry_policy.base_delay(),
            SemanticLimitsSnapshot::from_kernel_values(
                kernel.limits.max_tool_count,
                kernel.limits.max_tool_name_bytes,
                kernel.limits.max_tool_schema_bytes,
                kernel.limits.max_tool_input_bytes,
                kernel.limits.max_model_text_bytes_per_round,
                kernel.limits.max_model_reasoning_bytes_per_round,
            ),
        );
        let model = ModelDriver::from_validated(
            Arc::clone(&validated.model),
            validated.model_descriptor.clone(),
            model_config,
        )
        .map_err(|_| SessionEnvironmentError::Driver)?;
        let prompt = PromptBuilder::new(
            spec,
            validated.enabled_tools.specs().to_vec(),
            kernel.limits.clone(),
        )
        .map_err(|_| SessionEnvironmentError::Driver)?;
        let context = ContextDriver::new(
            validated.context.clone(),
            kernel.context_timeout,
            kernel.limits.clone(),
        )
        .map_err(|_| SessionEnvironmentError::Driver)?;
        let compaction = match &spec.compaction {
            CompactionConfig::Disabled => None,
            CompactionConfig::Enabled {
                trigger_tokens,
                target_tokens,
            } => Some(SessionCompaction {
                driver: CompactionDriver::new(
                    validated.compaction.clone(),
                    kernel.context_timeout,
                    kernel.limits.clone(),
                )
                .map_err(|_| SessionEnvironmentError::Driver)?,
                trigger_tokens: *trigger_tokens,
                target_tokens: *target_tokens,
            }),
        };
        let tools = ToolDriver::from_enabled(
            validated.enabled_tools,
            validated.tool_policy,
            ToolDriverConfig::from_kernel_values(
                kernel.policy_timeout,
                kernel.tool_call_timeout,
                kernel.limits.max_tool_input_bytes,
                kernel.limits.max_tool_output_bytes,
            ),
        )
        .map_err(|_| SessionEnvironmentError::Driver)?;
        Ok(Arc::new(Self {
            spec: spec.clone(),
            limits: kernel.limits.clone(),
            model_limits: validated.model_limits,
            model,
            tools,
            context,
            compaction,
            prompt,
            session_channels: SessionChannelCapacities {
                command: kernel.command_capacity,
                runner: kernel.runner_capacity,
                event: kernel.event_capacity,
            },
        }))
    }

    pub(crate) fn session_inputs(
        &self,
    ) -> (&SessionSpec, &SemanticLimits, SessionChannelCapacities) {
        (&self.spec, &self.limits, self.session_channels)
    }
}

impl fmt::Debug for SessionEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionEnvironment")
            .field("model", &self.spec.model)
            .field("enabled_tool_count", &self.spec.enabled_tools.len())
            .field("has_compaction", &self.compaction.is_some())
            .finish()
    }
}
