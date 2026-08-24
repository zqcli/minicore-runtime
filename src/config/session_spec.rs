use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::{ModelRef, ReasoningPreference};
use crate::tools::ToolName;
use crate::value::BoundedText;

use super::{ConfigError, SemanticLimits};

/// Maximum number of enabled tools permitted by absolute structural bounds.
pub const ABSOLUTE_MAX_TOOL_COUNT: usize = 4_096;
/// Maximum tool rounds per Turn permitted by absolute structural bounds.
pub const ABSOLUTE_MAX_TOOL_ROUNDS: u16 = 1_024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CompactionConfig {
    Disabled,
    Enabled {
        trigger_tokens: u64,
        target_tokens: u64,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CompactionConfigWire {
    Disabled(DisabledCompactionWire),
    Enabled(EnabledCompactionWire),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DisabledCompactionWire {
    mode: DisabledCompactionMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DisabledCompactionMode {
    Disabled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledCompactionWire {
    mode: EnabledCompactionMode,
    trigger_tokens: u64,
    target_tokens: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum EnabledCompactionMode {
    Enabled,
}

impl<'de> Deserialize<'de> for CompactionConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = match CompactionConfigWire::deserialize(deserializer)? {
            CompactionConfigWire::Disabled(DisabledCompactionWire {
                mode: DisabledCompactionMode::Disabled,
            }) => Self::Disabled,
            CompactionConfigWire::Enabled(EnabledCompactionWire {
                mode: EnabledCompactionMode::Enabled,
                trigger_tokens,
                target_tokens,
            }) => Self::Enabled {
                trigger_tokens,
                target_tokens,
            },
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl CompactionConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Disabled => Ok(()),
            Self::Enabled {
                trigger_tokens,
                target_tokens,
            } if *trigger_tokens > 0 && *target_tokens > 0 && *target_tokens < *trigger_tokens => {
                Ok(())
            }
            Self::Enabled { .. } => Err(ConfigError::InvalidBounds),
        }
    }
}

/// Checked specification for a Session.
///
/// Constructors and deserialization enforce absolute structural safety bounds.
/// Runtime instance limits are enforced when opening a session via
/// `SessionRuntime::create` or `SessionRuntime::load`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionSpec {
    pub model: ModelRef,
    pub reasoning: ReasoningPreference,
    pub system_prompt: BoundedText,
    pub enabled_tools: BTreeSet<ToolName>,
    pub max_tool_rounds: u16,
    pub compaction: CompactionConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionSpecWire {
    model: ModelRef,
    reasoning: ReasoningPreference,
    system_prompt: BoundedText,
    enabled_tools: BTreeSet<ToolName>,
    max_tool_rounds: u16,
    compaction: CompactionConfig,
}

impl<'de> Deserialize<'de> for SessionSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SessionSpecWire::deserialize(deserializer)?;
        let spec = Self {
            model: wire.model,
            reasoning: wire.reasoning,
            system_prompt: wire.system_prompt,
            enabled_tools: wire.enabled_tools,
            max_tool_rounds: wire.max_tool_rounds,
            compaction: wire.compaction,
        };
        spec.validate_structural()
            .map_err(serde::de::Error::custom)?;
        Ok(spec)
    }
}

impl SessionSpec {
    /// Creates a new `SessionSpec` after validating absolute structural bounds.
    pub fn new(
        model: ModelRef,
        reasoning: ReasoningPreference,
        system_prompt: BoundedText,
        enabled_tools: BTreeSet<ToolName>,
        max_tool_rounds: u16,
        compaction: CompactionConfig,
    ) -> Result<Self, ConfigError> {
        let spec = Self {
            model,
            reasoning,
            system_prompt,
            enabled_tools,
            max_tool_rounds,
            compaction,
        };
        spec.validate_structural()?;
        Ok(spec)
    }

    /// Validates invariant structural safety bounds independently of instance limits.
    pub fn validate_structural(&self) -> Result<(), ConfigError> {
        if self.enabled_tools.len() > ABSOLUTE_MAX_TOOL_COUNT
            || !(1..=ABSOLUTE_MAX_TOOL_ROUNDS).contains(&self.max_tool_rounds)
        {
            return Err(ConfigError::InvalidBounds);
        }
        self.compaction.validate()
    }

    /// Validates this specification against the provided runtime instance limits.
    pub fn validate(&self, limits: &SemanticLimits) -> Result<(), ConfigError> {
        limits.validate()?;
        self.validate_structural()?;
        if self.system_prompt.byte_len() > limits.max_system_prompt_bytes
            || self.enabled_tools.len() > limits.max_tool_count
            || self
                .enabled_tools
                .iter()
                .any(|tool| tool.as_str().len() > limits.max_tool_name_bytes)
            || !(1..=limits.max_tool_rounds).contains(&self.max_tool_rounds)
        {
            return Err(ConfigError::InvalidBounds);
        }
        self.compaction.validate()
    }
}
