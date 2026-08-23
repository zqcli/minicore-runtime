use std::time::Duration;

use minicore_runtime::config::ConfigError;
use minicore_runtime::value::{MAX_JSON_BYTES, MAX_TEXT_BYTES};
use minicore_runtime::{KernelConfig, RetryPolicy, SemanticLimits};

const MAX_CAPACITY: usize = 4_096;
const MAX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_PORT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_CONTEXT_BYTES: usize = 4 * MAX_TEXT_BYTES;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_TOOL_ROUNDS: usize = 64;

fn valid_config() -> KernelConfig {
    KernelConfig::default_checked().expect("recommended kernel defaults are valid")
}

fn assert_capacity_bounds(set: fn(&mut KernelConfig, usize)) {
    let mut config = valid_config();
    set(&mut config, 0);
    assert_eq!(config.validate(), Err(ConfigError::InvalidBounds));

    let mut config = valid_config();
    set(&mut config, MAX_CAPACITY);
    assert!(config.validate().is_ok());

    let mut config = valid_config();
    set(&mut config, MAX_CAPACITY + 1);
    assert_eq!(config.validate(), Err(ConfigError::InvalidBounds));
}

fn assert_timeout_bounds(set: fn(&mut KernelConfig, Duration), maximum: Duration) {
    let mut config = valid_config();
    set(&mut config, Duration::ZERO);
    assert_eq!(config.validate(), Err(ConfigError::InvalidBounds));

    let mut config = valid_config();
    set(&mut config, maximum);
    assert!(config.validate().is_ok());

    let mut config = valid_config();
    set(&mut config, maximum + Duration::from_nanos(1));
    assert_eq!(config.validate(), Err(ConfigError::InvalidBounds));
}

fn assert_limit_bounds(set: fn(&mut SemanticLimits, usize), maximum: usize) {
    let mut limits = SemanticLimits::default();
    set(&mut limits, 0);
    assert_eq!(limits.validate(), Err(ConfigError::InvalidBounds));

    let mut limits = SemanticLimits::default();
    set(&mut limits, maximum);
    assert!(limits.validate().is_ok());

    let mut limits = SemanticLimits::default();
    set(&mut limits, maximum + 1);
    assert_eq!(limits.validate(), Err(ConfigError::InvalidBounds));
}

#[test]
fn kernel_defaults_and_custom_retry_policy_are_exact() {
    let retry_default = RetryPolicy::default();
    assert_eq!(retry_default.max_attempts(), 3);
    assert_eq!(retry_default.base_delay(), Duration::from_millis(250));

    let config = valid_config();
    assert_eq!(config.command_capacity, 64);
    assert_eq!(config.runner_capacity, 64);
    assert_eq!(config.event_capacity, 256);
    assert_eq!(config.shutdown_timeout, Duration::from_secs(30));
    assert_eq!(config.model_call_timeout, Duration::from_secs(10 * 60));
    assert_eq!(config.tool_call_timeout, Duration::from_secs(30 * 60));
    assert_eq!(config.policy_timeout, Duration::from_secs(30));
    assert_eq!(config.context_timeout, Duration::from_secs(30));
    assert_eq!(config.log_operation_timeout, Duration::from_secs(30));
    assert_eq!(config.retry_policy, RetryPolicy::default());

    let limits = SemanticLimits::default();
    assert_eq!(limits.max_user_input_bytes, MAX_TEXT_BYTES);
    assert_eq!(limits.max_system_prompt_bytes, MAX_TEXT_BYTES);
    assert_eq!(limits.max_context_blocks, 64);
    assert_eq!(limits.max_context_bytes, MAX_CONTEXT_BYTES);
    assert_eq!(limits.max_tool_count, 64);
    assert_eq!(limits.max_tool_name_bytes, MAX_TOOL_NAME_BYTES);
    assert_eq!(limits.max_tool_schema_bytes, MAX_JSON_BYTES);
    assert_eq!(limits.max_tool_input_bytes, MAX_JSON_BYTES);
    assert_eq!(limits.max_tool_output_bytes, MAX_TEXT_BYTES);
    assert_eq!(limits.max_model_text_bytes_per_round, MAX_TEXT_BYTES);
    assert_eq!(limits.max_model_reasoning_bytes_per_round, MAX_TEXT_BYTES);
    assert_eq!(limits.max_tool_rounds, MAX_TOOL_ROUNDS as u16);
    assert_eq!(limits.max_transcript_page_size, 200);
    assert_eq!(limits.max_replay_page_size, 200);

    let retry = RetryPolicy::new(2, Duration::from_secs(1)).unwrap();
    let mut custom_limits = limits.clone();
    custom_limits.max_context_blocks = 1;
    let custom = KernelConfig::new(retry, custom_limits.clone()).unwrap();
    assert_eq!(custom.retry_policy, retry);
    assert_eq!(custom.limits.max_context_blocks, 1);
    assert_eq!(custom.limits.max_context_bytes, limits.max_context_bytes);
    assert!(RetryPolicy::default().delay_for_retry(0, None).is_some());
}

#[test]
fn kernel_capacity_and_timeout_boundaries_are_checked() {
    assert_capacity_bounds(|config, value| config.command_capacity = value);
    assert_capacity_bounds(|config, value| config.runner_capacity = value);
    assert_capacity_bounds(|config, value| config.event_capacity = value);

    assert_timeout_bounds(
        |config, value| config.shutdown_timeout = value,
        MAX_SHUTDOWN_TIMEOUT,
    );
    assert_timeout_bounds(
        |config, value| config.model_call_timeout = value,
        MAX_PORT_TIMEOUT,
    );
    assert_timeout_bounds(
        |config, value| config.tool_call_timeout = value,
        MAX_PORT_TIMEOUT,
    );
    assert_timeout_bounds(
        |config, value| config.policy_timeout = value,
        MAX_PORT_TIMEOUT,
    );
    assert_timeout_bounds(
        |config, value| config.context_timeout = value,
        MAX_PORT_TIMEOUT,
    );
    assert_timeout_bounds(
        |config, value| config.log_operation_timeout = value,
        MAX_PORT_TIMEOUT,
    );
}

#[test]
fn semantic_limit_categories_have_zero_exact_and_plus_one_boundaries() {
    assert_limit_bounds(
        |limits, value| limits.max_user_input_bytes = value,
        MAX_TEXT_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_system_prompt_bytes = value,
        MAX_TEXT_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_context_blocks = value,
        MAX_CAPACITY,
    );
    assert_limit_bounds(
        |limits, value| limits.max_context_bytes = value,
        MAX_CONTEXT_BYTES,
    );
    assert_limit_bounds(|limits, value| limits.max_tool_count = value, MAX_CAPACITY);
    assert_limit_bounds(
        |limits, value| limits.max_tool_name_bytes = value,
        MAX_TOOL_NAME_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_tool_schema_bytes = value,
        MAX_JSON_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_tool_input_bytes = value,
        MAX_JSON_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_tool_output_bytes = value,
        MAX_TEXT_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_model_text_bytes_per_round = value,
        MAX_TEXT_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_model_reasoning_bytes_per_round = value,
        MAX_TEXT_BYTES,
    );
    assert_limit_bounds(
        |limits, value| limits.max_tool_rounds = value as u16,
        MAX_TOOL_ROUNDS,
    );
    assert_limit_bounds(
        |limits, value| limits.max_transcript_page_size = value,
        MAX_CAPACITY,
    );
    assert_limit_bounds(
        |limits, value| limits.max_replay_page_size = value,
        MAX_CAPACITY,
    );
}

#[test]
fn kernel_types_clone_and_debug_without_sensitive_surface() {
    let config = valid_config();
    let cloned = config.clone();
    assert_eq!(cloned.command_capacity, config.command_capacity);
    assert_eq!(
        cloned.limits.max_tool_input_bytes,
        config.limits.max_tool_input_bytes
    );

    let debug = format!("{config:?}");
    assert!(debug.contains("KernelConfig"));
    assert!(debug.contains("SemanticLimits"));
    assert!(!debug.contains("path"));
    assert!(!debug.contains("credential"));
}
