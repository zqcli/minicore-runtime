use super::message::{normalize_parts, validate_tool_call_order};
use super::{AssistantPart, ModelValueError, ProviderReplay, Usage};
use crate::ids::ToolCallId;
use crate::tools::ToolName;
use crate::value::{BoundedText, normalize_model_text};
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

pub const MAX_MODEL_EVENT_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Eq, PartialEq)]
pub enum ModelEvent {
    TextDelta {
        delta: BoundedText,
    },
    ReasoningDelta {
        delta: BoundedText,
    },
    ToolCallStart {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
    },
    ToolCallArgumentsDelta {
        tool_call_id: ToolCallId,
        delta: BoundedText,
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
    pub fn text_delta(delta: impl AsRef<str>) -> Result<Self, ModelValueError> {
        Ok(Self::TextDelta {
            delta: checked_event_text(delta.as_ref())?,
        })
    }

    pub fn reasoning_delta(delta: impl AsRef<str>) -> Result<Self, ModelValueError> {
        Ok(Self::ReasoningDelta {
            delta: checked_event_text(delta.as_ref())?,
        })
    }

    pub fn tool_call_arguments_delta(
        tool_call_id: ToolCallId,
        delta: impl AsRef<str>,
    ) -> Result<Self, ModelValueError> {
        Ok(Self::ToolCallArgumentsDelta {
            tool_call_id,
            delta: checked_event_text(delta.as_ref())?,
        })
    }

    pub fn validate(&self) -> Result<(), ModelValueError> {
        match self {
            Self::TextDelta { delta }
            | Self::ReasoningDelta { delta }
            | Self::ToolCallArgumentsDelta { delta, .. } => {
                normalized_event_text(delta.as_str()).map(|_| ())
            }
            Self::ToolCallStart { .. }
            | Self::ToolCallEnd { .. }
            | Self::Usage { .. }
            | Self::ProviderReplay { .. }
            | Self::Finish { .. } => Ok(()),
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

fn checked_event_text(value: &str) -> Result<BoundedText, ModelValueError> {
    BoundedText::new(normalized_event_text(value)?).map_err(|_| ModelValueError::InvalidEvent)
}

pub(super) fn normalized_event_text(
    value: &str,
) -> Result<std::borrow::Cow<'_, str>, ModelValueError> {
    let text = normalize_model_text(value);
    if text.len() <= MAX_MODEL_EVENT_TEXT_BYTES {
        Ok(text)
    } else {
        Err(ModelValueError::InvalidEvent)
    }
}
