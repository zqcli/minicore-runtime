use super::message::{normalize_parts, validate_tool_call_order};
use super::{AssistantPart, ModelValueError, ProviderReplay, Usage};
use crate::ids::ToolCallId;
use crate::tools::ToolName;
use crate::value::Text;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFiltered,
    Refused,
    Unknown,
}

/// Assembled output of an unfinished request, distinct from complete model-context history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPartial {
    pub request_index: u32,
    pub content: Vec<AssistantPart>,
    pub incomplete_tool_calls: Vec<IncompleteToolCall>,
    pub usage: Option<Usage>,
    pub provider_replay: Option<ProviderReplay>,
}

/// An unfinished tool fragment with its original position among the assembled content parts.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncompleteToolCall {
    pub part_index: usize,
    pub call_index: u32,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments_text: String,
}

impl fmt::Debug for IncompleteToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IncompleteToolCall")
            .field("part_index", &self.part_index)
            .field("call_index", &self.call_index)
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("arguments_bytes", &self.arguments_text.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelResponse {
    parts: Vec<AssistantPart>,
    finish_reason: ModelFinishReason,
    usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_replay: Option<ProviderReplay>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelResponseWire {
    parts: Vec<AssistantPart>,
    finish_reason: ModelFinishReason,
    usage: Option<Usage>,
    #[serde(default)]
    provider_replay: Option<ProviderReplay>,
}

impl ModelResponse {
    pub fn new(
        parts: Vec<AssistantPart>,
        finish_reason: ModelFinishReason,
        usage: impl Into<Option<Usage>>,
    ) -> Result<Self, ModelValueError> {
        Self::new_with_provider_replay(parts, finish_reason, usage, None)
    }

    /// Constructs a response, permitting no canonical parts only when replay exists.
    pub fn new_with_provider_replay(
        parts: Vec<AssistantPart>,
        finish_reason: ModelFinishReason,
        usage: impl Into<Option<Usage>>,
        provider_replay: Option<ProviderReplay>,
    ) -> Result<Self, ModelValueError> {
        let parts = normalize_parts(parts);
        if parts.is_empty() && provider_replay.is_none() {
            return Err(ModelValueError::EmptyResponseParts);
        }
        validate_tool_call_order(&parts)?;
        Ok(Self {
            parts,
            finish_reason,
            usage: usage.into(),
            provider_replay,
        })
    }

    /// Attaches validated opaque metadata without changing canonical response parts.
    pub fn with_provider_replay(mut self, replay: ProviderReplay) -> Self {
        self.provider_replay = Some(replay);
        self
    }

    pub const fn provider_replay(&self) -> Option<&ProviderReplay> {
        self.provider_replay.as_ref()
    }

    pub fn parts(&self) -> &[AssistantPart] {
        &self.parts
    }

    pub const fn finish_reason(&self) -> ModelFinishReason {
        self.finish_reason
    }

    pub const fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }
}

impl fmt::Debug for ModelResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelResponse")
            .field("part_count", &self.parts.len())
            .field("parts", &self.parts)
            .field("provider_replay", &self.provider_replay)
            .field("finish_reason", &self.finish_reason)
            .field("usage", &self.usage)
            .finish()
    }
}

impl<'de> Deserialize<'de> for ModelResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ModelResponseWire::deserialize(deserializer)?;
        Self::new_with_provider_replay(
            value.parts,
            value.finish_reason,
            value.usage,
            value.provider_replay,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Deltas retain raw text; assembly normalizes text and reasoning across events.
#[derive(Clone, Eq, PartialEq)]
pub enum ModelEvent {
    TextDelta {
        delta: Text,
    },
    ReasoningDelta {
        delta: Text,
    },
    ToolCallStart {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
    },
    ToolCallArgumentsDelta {
        tool_call_id: ToolCallId,
        delta: Text,
    },
    ToolCallEnd {
        tool_call_id: ToolCallId,
    },
    Usage {
        usage: Usage,
    },
    /// Opaque metadata, accepted once before Finish; never emitted as display progress.
    ProviderReplay {
        replay: ProviderReplay,
    },
    Finish {
        reason: ModelFinishReason,
    },
}

impl ModelEvent {
    pub fn text_delta(delta: impl AsRef<str>) -> Self {
        Self::TextDelta {
            delta: Text::new(delta),
        }
    }

    pub fn reasoning_delta(delta: impl AsRef<str>) -> Self {
        Self::ReasoningDelta {
            delta: Text::new(delta),
        }
    }

    pub fn tool_call_arguments_delta(tool_call_id: ToolCallId, delta: impl AsRef<str>) -> Self {
        Self::ToolCallArgumentsDelta {
            tool_call_id,
            delta: Text::new(delta),
        }
    }
}

impl fmt::Debug for ModelEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextDelta { delta } => formatter
                .debug_struct("TextDelta")
                .field("delta_bytes", &delta.byte_len())
                .finish(),
            Self::ReasoningDelta { delta } => formatter
                .debug_struct("ReasoningDelta")
                .field("delta_bytes", &delta.byte_len())
                .finish(),
            Self::ToolCallStart {
                tool_call_id,
                tool_name,
            } => formatter
                .debug_struct("ToolCallStart")
                .field("tool_call_id", tool_call_id)
                .field("tool_name", tool_name)
                .finish(),
            Self::ToolCallArgumentsDelta {
                tool_call_id,
                delta,
            } => formatter
                .debug_struct("ToolCallArgumentsDelta")
                .field("tool_call_id", tool_call_id)
                .field("delta_bytes", &delta.byte_len())
                .finish(),
            Self::ToolCallEnd { tool_call_id } => formatter
                .debug_struct("ToolCallEnd")
                .field("tool_call_id", tool_call_id)
                .finish(),
            Self::Usage { usage } => formatter.debug_tuple("Usage").field(usage).finish(),
            Self::ProviderReplay { replay } => formatter
                .debug_tuple("ProviderReplay")
                .field(replay)
                .finish(),
            Self::Finish { reason } => formatter.debug_tuple("Finish").field(reason).finish(),
        }
    }
}
