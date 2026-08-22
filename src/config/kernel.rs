use std::time::Duration;

use crate::agent::RetryPolicy;
use crate::value::{BoundedText, MAX_JSON_BYTES};

use super::ConfigError;

const MAX_KERNEL_CAPACITY: usize = 4_096;
const MAX_KERNEL_PORT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_CONTEXT_BYTES: usize = 4 * BoundedText::MAX_BYTES;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_SEMANTIC_TOOL_ROUNDS: u16 = 64;

const DEFAULT_KERNEL_COMMAND_CAPACITY: usize = 64;
const DEFAULT_KERNEL_RUNNER_CAPACITY: usize = 64;
const DEFAULT_KERNEL_EVENT_CAPACITY: usize = 256;
const DEFAULT_KERNEL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_KERNEL_MODEL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_KERNEL_TOOL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_KERNEL_POLICY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_KERNEL_CONTEXT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_KERNEL_LOG_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct KernelConfig {
    pub command_capacity: usize,
    pub runner_capacity: usize,
    pub event_capacity: usize,
    pub shutdown_timeout: Duration,
    pub model_call_timeout: Duration,
    pub tool_call_timeout: Duration,
    pub policy_timeout: Duration,
    pub context_timeout: Duration,
    pub log_operation_timeout: Duration,
    pub retry_policy: RetryPolicy,
    pub limits: SemanticLimits,
}

impl KernelConfig {
    pub fn new(retry_policy: RetryPolicy, limits: SemanticLimits) -> Result<Self, ConfigError> {
        let config = Self {
            command_capacity: DEFAULT_KERNEL_COMMAND_CAPACITY,
            runner_capacity: DEFAULT_KERNEL_RUNNER_CAPACITY,
            event_capacity: DEFAULT_KERNEL_EVENT_CAPACITY,
            shutdown_timeout: DEFAULT_KERNEL_SHUTDOWN_TIMEOUT,
            model_call_timeout: DEFAULT_KERNEL_MODEL_TIMEOUT,
            tool_call_timeout: DEFAULT_KERNEL_TOOL_TIMEOUT,
            policy_timeout: DEFAULT_KERNEL_POLICY_TIMEOUT,
            context_timeout: DEFAULT_KERNEL_CONTEXT_TIMEOUT,
            log_operation_timeout: DEFAULT_KERNEL_LOG_TIMEOUT,
            retry_policy,
            limits,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn default_checked() -> Result<Self, ConfigError> {
        Self::new(RetryPolicy::default(), SemanticLimits::default())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !bounded_capacity(self.command_capacity)
            || !bounded_capacity(self.runner_capacity)
            || !bounded_capacity(self.event_capacity)
            || !valid_timeout(self.shutdown_timeout, super::MAX_RUNTIME_SHUTDOWN_TIMEOUT)
            || !valid_timeout(self.model_call_timeout, MAX_KERNEL_PORT_TIMEOUT)
            || !valid_timeout(self.tool_call_timeout, MAX_KERNEL_PORT_TIMEOUT)
            || !valid_timeout(self.policy_timeout, MAX_KERNEL_PORT_TIMEOUT)
            || !valid_timeout(self.context_timeout, MAX_KERNEL_PORT_TIMEOUT)
            || !valid_timeout(self.log_operation_timeout, MAX_KERNEL_PORT_TIMEOUT)
        {
            return Err(ConfigError::InvalidBounds);
        }
        self.limits.validate()
    }
}

#[derive(Clone, Debug)]
pub struct SemanticLimits {
    pub max_user_input_bytes: usize,
    pub max_system_prompt_bytes: usize,
    pub max_context_blocks: usize,
    pub max_context_bytes: usize,
    pub max_tool_count: usize,
    pub max_tool_name_bytes: usize,
    pub max_tool_schema_bytes: usize,
    pub max_tool_input_bytes: usize,
    pub max_tool_output_bytes: usize,
    pub max_model_text_bytes_per_round: usize,
    pub max_model_reasoning_bytes_per_round: usize,
    pub max_tool_rounds: u16,
    pub max_transcript_page_size: usize,
    pub max_replay_page_size: usize,
}

impl Default for SemanticLimits {
    fn default() -> Self {
        Self {
            max_user_input_bytes: BoundedText::MAX_BYTES,
            max_system_prompt_bytes: BoundedText::MAX_BYTES,
            max_context_blocks: 64,
            max_context_bytes: MAX_CONTEXT_BYTES,
            max_tool_count: 64,
            max_tool_name_bytes: MAX_TOOL_NAME_BYTES,
            max_tool_schema_bytes: MAX_JSON_BYTES,
            max_tool_input_bytes: MAX_JSON_BYTES,
            max_tool_output_bytes: BoundedText::MAX_BYTES,
            max_model_text_bytes_per_round: BoundedText::MAX_BYTES,
            max_model_reasoning_bytes_per_round: BoundedText::MAX_BYTES,
            max_tool_rounds: MAX_SEMANTIC_TOOL_ROUNDS,
            max_transcript_page_size: 200,
            max_replay_page_size: 200,
        }
    }
}

impl SemanticLimits {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !bounded_nonzero_at_most(self.max_user_input_bytes, BoundedText::MAX_BYTES)
            || !bounded_nonzero_at_most(self.max_system_prompt_bytes, BoundedText::MAX_BYTES)
            || !bounded_nonzero_at_most(self.max_context_blocks, MAX_KERNEL_CAPACITY)
            || !bounded_nonzero_at_most(self.max_context_bytes, MAX_CONTEXT_BYTES)
            || !bounded_nonzero_at_most(self.max_tool_count, MAX_KERNEL_CAPACITY)
            || !bounded_nonzero_at_most(self.max_tool_name_bytes, MAX_TOOL_NAME_BYTES)
            || !bounded_nonzero_at_most(self.max_tool_schema_bytes, MAX_JSON_BYTES)
            || !bounded_nonzero_at_most(self.max_tool_input_bytes, MAX_JSON_BYTES)
            || !bounded_nonzero_at_most(self.max_tool_output_bytes, BoundedText::MAX_BYTES)
            || !bounded_nonzero_at_most(self.max_model_text_bytes_per_round, BoundedText::MAX_BYTES)
            || !bounded_nonzero_at_most(
                self.max_model_reasoning_bytes_per_round,
                BoundedText::MAX_BYTES,
            )
            || !bounded_nonzero_at_most(
                self.max_tool_rounds as usize,
                MAX_SEMANTIC_TOOL_ROUNDS as usize,
            )
            || !bounded_nonzero_at_most(self.max_transcript_page_size, MAX_KERNEL_CAPACITY)
            || !bounded_nonzero_at_most(self.max_replay_page_size, MAX_KERNEL_CAPACITY)
        {
            return Err(ConfigError::InvalidBounds);
        }
        Ok(())
    }
}

pub(super) fn bounded_capacity(value: usize) -> bool {
    (1..=MAX_KERNEL_CAPACITY).contains(&value)
}

fn bounded_nonzero_at_most(value: usize, maximum: usize) -> bool {
    value != 0 && value <= maximum
}

fn valid_timeout(value: Duration, maximum: Duration) -> bool {
    !value.is_zero() && value <= maximum
}
