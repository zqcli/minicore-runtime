use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::{ModelRef, ReasoningPreference};
use crate::tools::ToolName;
use crate::value::BoundedText;

use super::{ConfigError, SemanticLimits};

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
        Self::new(
            wire.model,
            wire.reasoning,
            wire.system_prompt,
            wire.enabled_tools,
            wire.max_tool_rounds,
            wire.compaction,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl SessionSpec {
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
        spec.validate(&SemanticLimits::default())?;
        Ok(spec)
    }

    pub fn validate(&self, limits: &SemanticLimits) -> Result<(), ConfigError> {
        limits.validate()?;
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
