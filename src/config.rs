use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

pub use crate::agent::{RetryPolicy, RetryPolicyError};
use crate::ids::SessionId;
use crate::model::{ModelSelection, ProviderRegistry};
use crate::session::store::{
    StoredCompactionConfig, StoredExecutionConfig, StoredModelConfig, StoredSessionConfig,
};
use crate::session::time::Timestamp;
use crate::tools::{ToolName, ToolRegistry};

pub const DEFAULT_EVENT_CAPACITY: usize = 64;
pub const DEFAULT_COMMAND_CAPACITY: usize = 64;
pub const DEFAULT_RUNNER_EVENT_CAPACITY: usize = 64;
pub const MAX_ENABLED_TOOLS: usize = 64;
pub const MAX_TOOL_ROUNDS: u8 = 64;
pub const MAX_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ConfigError {
    #[error("configuration path must be absolute and free of dot components")]
    InvalidPath,
    #[error("configuration text is empty, unsafe, or too large")]
    InvalidText,
    #[error("configuration value is outside its bound")]
    InvalidBounds,
    #[error("configuration retry policy is invalid")]
    InvalidRetryPolicy,
    #[error("configuration timestamp could not be created")]
    Timestamp,
}

struct RuntimeConfigParts {
    data_dir: PathBuf,
    provider_registry: ProviderRegistry,
    tool_registry: ToolRegistry,
    coding_instructions: String,
    shutdown_timeout: Duration,
    event_capacity: usize,
    command_capacity: usize,
    runner_event_capacity: usize,
    retry_policy: RetryPolicy,
}

#[derive(Clone)]
pub struct RuntimeConfig {
    data_dir: PathBuf,
    provider_registry: ProviderRegistry,
    tool_registry: ToolRegistry,
    coding_instructions: Arc<str>,
    shutdown_timeout: Duration,
    event_capacity: usize,
    command_capacity: usize,
    runner_event_capacity: usize,
    retry_policy: RetryPolicy,
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("data_dir", &"<redacted>")
            .field("provider_registry", &"<redacted>")
            .field("tool_registry", &"<redacted>")
            .field("coding_instructions_bytes", &self.coding_instructions.len())
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("event_capacity", &self.event_capacity)
            .field("command_capacity", &self.command_capacity)
            .field("runner_event_capacity", &self.runner_event_capacity)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

impl RuntimeConfig {
    pub fn new(
        data_dir: PathBuf,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
        coding_instructions: impl Into<String>,
        retry_policy: RetryPolicy,
    ) -> Result<Self, ConfigError> {
        Self::from_parts(RuntimeConfigParts {
            data_dir,
            provider_registry,
            tool_registry,
            coding_instructions: coding_instructions.into(),
            shutdown_timeout: Duration::from_secs(30),
            event_capacity: DEFAULT_EVENT_CAPACITY,
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            runner_event_capacity: DEFAULT_RUNNER_EVENT_CAPACITY,
            retry_policy,
        })
    }

    pub fn with_defaults(
        data_dir: PathBuf,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
        coding_instructions: impl Into<String>,
        retry_policy: RetryPolicy,
    ) -> Result<Self, ConfigError> {
        Self::new(
            data_dir,
            provider_registry,
            tool_registry,
            coding_instructions,
            retry_policy,
        )
    }

    pub fn builder(
        data_dir: PathBuf,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
        coding_instructions: impl Into<String>,
        retry_policy: RetryPolicy,
    ) -> RuntimeConfigBuilder {
        RuntimeConfigBuilder {
            data_dir,
            provider_registry,
            tool_registry,
            coding_instructions: coding_instructions.into(),
            shutdown_timeout: Duration::from_secs(30),
            event_capacity: DEFAULT_EVENT_CAPACITY,
            command_capacity: DEFAULT_COMMAND_CAPACITY,
            runner_event_capacity: DEFAULT_RUNNER_EVENT_CAPACITY,
            retry_policy,
        }
    }

    fn from_parts(parts: RuntimeConfigParts) -> Result<Self, ConfigError> {
        validate_root(&parts.data_dir)?;
        validate_text(&parts.coding_instructions, false)?;
        if !(Duration::from_millis(1)..=MAX_RUNTIME_SHUTDOWN_TIMEOUT)
            .contains(&parts.shutdown_timeout)
            || !bounded_capacity(parts.event_capacity)
            || !bounded_capacity(parts.command_capacity)
            || !bounded_capacity(parts.runner_event_capacity)
        {
            return Err(ConfigError::InvalidBounds);
        }
        Ok(Self {
            data_dir: parts.data_dir,
            provider_registry: parts.provider_registry,
            tool_registry: parts.tool_registry,
            coding_instructions: parts.coding_instructions.into(),
            shutdown_timeout: parts.shutdown_timeout,
            event_capacity: parts.event_capacity,
            command_capacity: parts.command_capacity,
            runner_event_capacity: parts.runner_event_capacity,
            retry_policy: parts.retry_policy,
        })
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub(crate) fn provider_registry(&self) -> ProviderRegistry {
        self.provider_registry.clone()
    }

    pub(crate) fn tool_registry(&self) -> ToolRegistry {
        self.tool_registry.clone()
    }

    pub(crate) fn coding_instructions(&self) -> Arc<str> {
        Arc::clone(&self.coding_instructions)
    }

    pub(crate) const fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }

    pub(crate) const fn event_capacity(&self) -> usize {
        self.event_capacity
    }

    pub(crate) const fn command_capacity(&self) -> usize {
        self.command_capacity
    }

    pub(crate) const fn runner_event_capacity(&self) -> usize {
        self.runner_event_capacity
    }

    pub(crate) const fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }
}

pub struct RuntimeConfigBuilder {
    data_dir: PathBuf,
    provider_registry: ProviderRegistry,
    tool_registry: ToolRegistry,
    coding_instructions: String,
    shutdown_timeout: Duration,
    event_capacity: usize,
    command_capacity: usize,
    runner_event_capacity: usize,
    retry_policy: RetryPolicy,
}

impl RuntimeConfigBuilder {
    pub fn shutdown_timeout(mut self, value: Duration) -> Self {
        self.shutdown_timeout = value;
        self
    }

    pub fn capacities(
        mut self,
        event_capacity: usize,
        command_capacity: usize,
        runner_event_capacity: usize,
    ) -> Self {
        self.event_capacity = event_capacity;
        self.command_capacity = command_capacity;
        self.runner_event_capacity = runner_event_capacity;
        self
    }

    pub fn build(self) -> Result<RuntimeConfig, ConfigError> {
        RuntimeConfig::from_parts(RuntimeConfigParts {
            data_dir: self.data_dir,
            provider_registry: self.provider_registry,
            tool_registry: self.tool_registry,
            coding_instructions: self.coding_instructions,
            shutdown_timeout: self.shutdown_timeout,
            event_capacity: self.event_capacity,
            command_capacity: self.command_capacity,
            runner_event_capacity: self.runner_event_capacity,
            retry_policy: self.retry_policy,
        })
    }
}

#[derive(Clone)]
pub struct SessionConfig {
    workspace_root: PathBuf,
    model: ModelSelection,
    system_prompt: String,
    enabled_tools: BTreeSet<ToolName>,
    compaction_trigger_tokens: u64,
    compaction_target_tokens: u64,
    max_tool_rounds: u8,
}

impl fmt::Debug for SessionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionConfig")
            .field("workspace_root", &"<redacted>")
            .field("model", &self.model)
            .field("system_prompt_bytes", &self.system_prompt.len())
            .field("enabled_tool_count", &self.enabled_tools.len())
            .field("compaction_trigger_tokens", &self.compaction_trigger_tokens)
            .field("compaction_target_tokens", &self.compaction_target_tokens)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .finish()
    }
}

impl SessionConfig {
    pub fn new(
        workspace_root: PathBuf,
        model: ModelSelection,
        system_prompt: impl Into<String>,
        enabled_tools: BTreeSet<ToolName>,
        compaction_trigger_tokens: u64,
        compaction_target_tokens: u64,
        max_tool_rounds: u8,
    ) -> Result<Self, ConfigError> {
        validate_root(&workspace_root)?;
        let system_prompt = system_prompt.into();
        validate_text(&system_prompt, true)?;
        if enabled_tools.len() > MAX_ENABLED_TOOLS
            || compaction_trigger_tokens == 0
            || compaction_target_tokens == 0
            || compaction_target_tokens >= compaction_trigger_tokens
            || !(1..=MAX_TOOL_ROUNDS).contains(&max_tool_rounds)
        {
            return Err(ConfigError::InvalidBounds);
        }
        Ok(Self {
            workspace_root,
            model,
            system_prompt,
            enabled_tools,
            compaction_trigger_tokens,
            compaction_target_tokens,
            max_tool_rounds,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub const fn model(&self) -> &ModelSelection {
        &self.model
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn enabled_tools(&self) -> &BTreeSet<ToolName> {
        &self.enabled_tools
    }

    pub const fn compaction_trigger_tokens(&self) -> u64 {
        self.compaction_trigger_tokens
    }

    pub const fn compaction_target_tokens(&self) -> u64 {
        self.compaction_target_tokens
    }

    pub const fn max_tool_rounds(&self) -> u8 {
        self.max_tool_rounds
    }

    pub(crate) fn to_stored(
        &self,
        session_id: SessionId,
        timestamp: Timestamp,
    ) -> Result<StoredSessionConfig, ConfigError> {
        let compaction = StoredCompactionConfig::new(
            self.compaction_trigger_tokens,
            self.compaction_target_tokens,
        )
        .map_err(|_| ConfigError::InvalidBounds)?;
        let execution = StoredExecutionConfig::new(
            self.enabled_tools.clone(),
            compaction,
            self.max_tool_rounds,
        )
        .map_err(|_| ConfigError::InvalidBounds)?;
        StoredSessionConfig::new(
            session_id,
            timestamp.clone(),
            timestamp,
            self.workspace_root.clone(),
            StoredModelConfig::new(self.model.clone()),
            self.system_prompt.clone(),
            execution,
        )
        .map_err(|_| ConfigError::InvalidBounds)
    }
}

fn bounded_capacity(value: usize) -> bool {
    (1..=4_096).contains(&value)
}

fn validate_root(path: &Path) -> Result<(), ConfigError> {
    let Some(raw) = path.to_str() else {
        return Err(ConfigError::InvalidPath);
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || raw
            .split(['/', '\\'])
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(ConfigError::InvalidPath);
    }
    Ok(())
}

fn validate_text(value: &str, allow_empty: bool) -> Result<(), ConfigError> {
    if (!allow_empty && value.is_empty())
        || value.len() > 262_144
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        Err(ConfigError::InvalidText)
    } else {
        Ok(())
    }
}

const _: () = {
    let _ = std::mem::size_of::<RuntimeConfig>();
    let _ = std::mem::size_of::<SessionConfig>();
    let _: fn(
        PathBuf,
        ProviderRegistry,
        ToolRegistry,
        String,
        RetryPolicy,
    ) -> RuntimeConfigBuilder = RuntimeConfig::builder;
};
