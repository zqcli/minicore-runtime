use super::{ModelMessage, ModelValueError};
use crate::tools::ToolSpec;
use serde::{Deserialize, Deserializer, Serialize};
use std::{collections::BTreeSet, fmt, sync::Arc};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelLimitsError {
    #[error("model limit values must be non-zero")]
    Zero,
}

#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPreference {
    #[default]
    Auto,
    Disabled,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
    Ultra,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ModelLimits {
    context_window_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelLimitsWire {
    context_window_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
}

impl ModelLimits {
    pub fn new(
        context_window_tokens: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> Result<Self, ModelLimitsError> {
        if context_window_tokens == Some(0) || max_output_tokens == Some(0) {
            return Err(ModelLimitsError::Zero);
        }
        Ok(Self {
            context_window_tokens,
            max_output_tokens,
        })
    }

    pub const fn context_window_tokens(&self) -> Option<u32> {
        self.context_window_tokens
    }

    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }
}

impl<'de> Deserialize<'de> for ModelLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ModelLimitsWire::deserialize(deserializer)?;
        Self::new(value.context_window_tokens, value.max_output_tokens)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelRequest {
    messages: Arc<[ModelMessage]>,
    tools: Arc<[ToolSpec]>,
    limits: ModelLimits,
    reasoning: ReasoningPreference,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRequestWire {
    messages: Vec<ModelMessage>,
    tools: Vec<ToolSpec>,
    limits: ModelLimits,
    reasoning: ReasoningPreference,
}

impl ModelRequest {
    pub fn new(
        messages: Vec<ModelMessage>,
        tools: impl Into<Arc<[ToolSpec]>>,
        limits: ModelLimits,
        reasoning: ReasoningPreference,
    ) -> Result<Self, ModelValueError> {
        let tools = tools.into();
        if tools
            .windows(2)
            .any(|pair| pair[0].name() >= pair[1].name())
        {
            return Err(ModelValueError::InvalidTools);
        }
        Self::with_validated_tools(messages, tools, limits, reasoning)
    }

    /// Reuses a checked catalog (the runner's frozen ToolSet), while validating
    /// messages produced by the host prompt provider on every request.
    pub(crate) fn with_validated_tools(
        messages: Vec<ModelMessage>,
        tools: Arc<[ToolSpec]>,
        limits: ModelLimits,
        reasoning: ReasoningPreference,
    ) -> Result<Self, ModelValueError> {
        if messages.is_empty() {
            return Err(ModelValueError::EmptyMessages);
        }
        validate_tool_exchange(&messages)?;
        Ok(Self {
            messages: messages.into(),
            tools,
            limits,
            reasoning,
        })
    }

    pub fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub const fn limits(&self) -> &ModelLimits {
        &self.limits
    }

    pub const fn reasoning(&self) -> ReasoningPreference {
        self.reasoning
    }
}

impl fmt::Debug for ModelRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRequest")
            .field("message_count", &self.messages.len())
            .field("tool_count", &self.tools.len())
            .field("limits", &self.limits)
            .field("reasoning", &self.reasoning)
            .finish()
    }
}

impl<'de> Deserialize<'de> for ModelRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ModelRequestWire::deserialize(deserializer)?;
        Self::new(value.messages, value.tools, value.limits, value.reasoning)
            .map_err(serde::de::Error::custom)
    }
}

fn validate_tool_exchange(messages: &[ModelMessage]) -> Result<(), ModelValueError> {
    let mut pending = BTreeSet::new();
    let mut completed = BTreeSet::new();
    for message in messages {
        match message {
            ModelMessage::Assistant(parts) | ModelMessage::AssistantWithReplay { parts, .. } => {
                if !pending.is_empty() {
                    return Err(ModelValueError::IncompleteToolExchange);
                }
                message.validate()?;
                completed.clear();
                for part in parts {
                    if let Some(call) = part.as_tool_call() {
                        pending.insert(call.tool_call_id().clone());
                    }
                }
            }
            ModelMessage::Tool { tool_call_id, .. } => {
                if completed.contains(tool_call_id) {
                    return Err(ModelValueError::DuplicateToolResult);
                }
                if pending.is_empty() || !pending.remove(tool_call_id) {
                    return Err(ModelValueError::OrphanToolResult);
                }
                completed.insert(tool_call_id.clone());
            }
            ModelMessage::System(_) | ModelMessage::User(_) => {
                if !pending.is_empty() {
                    return Err(ModelValueError::IncompleteToolExchange);
                }
                message.validate()?;
                completed.clear();
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(ModelValueError::IncompleteToolExchange)
    }
}
