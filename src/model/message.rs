use super::{ModelValueError, ProviderReplay};
use crate::ids::ToolCallId;
use crate::tools::{ToolName, ToolOutput, ToolResultOutcome};
use crate::value::{normalize_model_text, valid_text};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{collections::BTreeSet, fmt, sync::Arc};

/// Absolute ceiling for a single `ModelMessage` text payload (system, user,
/// or an assistant text part). Default prompt projection truncates summary
/// tails so this bound is always satisfied.
pub(crate) const MAX_MODEL_MESSAGE_TEXT_BYTES: usize = 262_144;

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AssistantPart {
    Text(Arc<str>),
    Reasoning(ReasoningContent),
    ToolCall(ToolCall),
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum AssistantPartWire {
    Text(Arc<str>),
    Reasoning(ReasoningContent),
    ToolCall(ToolCall),
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ReasoningContent {
    text: Option<Arc<str>>,
    summary: Option<Arc<str>>,
    encrypted: Option<Arc<str>>,
    signature: Option<Arc<str>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasoningContentWire {
    text: Option<String>,
    summary: Option<String>,
    encrypted: Option<String>,
    signature: Option<String>,
}

impl ReasoningContent {
    pub fn new(
        text: Option<String>,
        summary: Option<String>,
        encrypted: Option<String>,
        signature: Option<String>,
    ) -> Result<Self, ModelValueError> {
        let text = normalize_optional_text(text, 262_144)?;
        let summary = normalize_optional_text(summary, 131_072)?;
        validate_optional_opaque(&encrypted, 262_144)?;
        validate_optional_opaque(&signature, 16_384)?;
        if text.is_none() && summary.is_none() && encrypted.is_none() && signature.is_none() {
            return Err(ModelValueError::EmptyReasoningContent);
        }
        Ok(Self {
            text,
            summary,
            encrypted: encrypted.map(Arc::from),
            signature: signature.map(Arc::from),
        })
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub fn encrypted(&self) -> Option<&str> {
        self.encrypted.as_deref()
    }

    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }
}

impl fmt::Debug for AssistantPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => formatter
                .debug_struct("AssistantPart::Text")
                .field("text_bytes", &text.len())
                .finish(),
            Self::Reasoning(reasoning) => formatter
                .debug_tuple("AssistantPart::Reasoning")
                .field(reasoning)
                .finish(),
            Self::ToolCall(call) => formatter
                .debug_tuple("AssistantPart::ToolCall")
                .field(call)
                .finish(),
        }
    }
}

impl fmt::Debug for ReasoningContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReasoningContent")
            .field("text_bytes", &self.text.as_ref().map(|text| text.len()))
            .field(
                "summary_bytes",
                &self.summary.as_ref().map(|text| text.len()),
            )
            .field("has_encrypted", &self.encrypted.is_some())
            .field("has_signature", &self.signature.is_some())
            .finish()
    }
}

impl<'de> Deserialize<'de> for ReasoningContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ReasoningContentWire::deserialize(deserializer)?;
        Self::new(value.text, value.summary, value.encrypted, value.signature)
            .map_err(serde::de::Error::custom)
    }
}

fn normalize_optional_text(
    value: Option<String>,
    maximum: usize,
) -> Result<Option<Arc<str>>, ModelValueError> {
    value
        .map(|value| {
            let text = normalize_model_text(&value);
            if text.len() > maximum {
                return Err(ModelValueError::InvalidText);
            }
            Ok(Arc::from(text.as_ref()))
        })
        .transpose()
}

fn validate_optional_opaque(value: &Option<String>, maximum: usize) -> Result<(), ModelValueError> {
    if value.as_deref().is_some_and(|value| {
        value.is_empty()
            || value.len() > maximum
            || value
                .bytes()
                .any(|byte| !(0x21..=0x7e).contains(&byte) || matches!(byte, b'"' | b'\\'))
    }) {
        Err(ModelValueError::InvalidText)
    } else {
        Ok(())
    }
}

impl AssistantPart {
    pub fn validate(&self) -> Result<(), ModelValueError> {
        match self {
            Self::Text(text) => {
                if valid_text(text, MAX_MODEL_MESSAGE_TEXT_BYTES, true) {
                    Ok(())
                } else {
                    Err(ModelValueError::InvalidText)
                }
            }
            Self::Reasoning(_) | Self::ToolCall(_) => Ok(()),
        }
    }

    pub const fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            Self::Text(_) | Self::Reasoning(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for AssistantPart {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = match AssistantPartWire::deserialize(deserializer)? {
            AssistantPartWire::Text(text) => {
                Self::Text(Arc::from(normalize_model_text(&text).as_ref()))
            }
            AssistantPartWire::Reasoning(reasoning) => Self::Reasoning(reasoning),
            AssistantPartWire::ToolCall(call) => Self::ToolCall(call),
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ToolCall {
    tool_call_id: ToolCallId,
    name: ToolName,
    arguments: Arc<Value>,
    call_index: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallWire {
    tool_call_id: ToolCallId,
    name: ToolName,
    arguments: Value,
    call_index: u32,
}

impl fmt::Debug for ToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCall")
            .field("tool_call_id", &self.tool_call_id)
            .field("name", &self.name)
            .field("call_index", &self.call_index)
            .field("arguments", &"<redacted>")
            .finish()
    }
}

impl ToolCall {
    pub fn new(
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: Value,
        call_index: u32,
    ) -> Result<Self, ModelValueError> {
        Self::new_with_max_bytes(
            tool_call_id,
            name,
            arguments,
            call_index,
            crate::value::MAX_JSON_BYTES,
        )
    }

    pub(crate) fn new_with_max_bytes(
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: Value,
        call_index: u32,
        max_bytes: usize,
    ) -> Result<Self, ModelValueError> {
        if !arguments.is_object()
            || crate::value::validate_json_size(&arguments, max_bytes).is_err()
        {
            return Err(ModelValueError::InvalidToolArguments);
        }
        Ok(Self {
            tool_call_id,
            name,
            arguments: Arc::new(arguments),
            call_index,
        })
    }

    pub const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    pub fn arguments(&self) -> &Value {
        &self.arguments
    }

    pub const fn call_index(&self) -> u32 {
        self.call_index
    }
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolCallWire::deserialize(deserializer)?;
        Self::new(
            value.tool_call_id,
            value.name,
            value.arguments,
            value.call_index,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ModelMessage {
    System(Arc<str>),
    User(Arc<str>),
    Assistant(Vec<AssistantPart>),
    /// Canonical assistant parts plus non-display provider-owned metadata.
    AssistantWithReplay {
        parts: Vec<AssistantPart>,
        provider_replay: ProviderReplay,
    },
    Tool {
        tool_call_id: ToolCallId,
        output: ToolOutput,
        outcome: ToolResultOutcome,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
enum ModelMessageWire {
    System(Arc<str>),
    User(Arc<str>),
    Assistant(AssistantMessageWire),
    Tool {
        tool_call_id: ToolCallId,
        output: ToolOutput,
        outcome: ToolResultOutcome,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AssistantMessageWire {
    Legacy(Vec<AssistantPart>),
    Replay(AssistantReplayWire),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssistantReplayWire {
    parts: Vec<AssistantPart>,
    #[serde(default)]
    provider_replay: Option<ProviderReplay>,
}

impl Serialize for ModelMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "role", content = "content", rename_all = "snake_case")]
        enum Wire<'a> {
            System(&'a str),
            User(&'a str),
            Assistant(Content<'a>),
            Tool {
                tool_call_id: &'a ToolCallId,
                output: &'a ToolOutput,
                outcome: ToolResultOutcome,
            },
        }
        #[derive(Serialize)]
        #[serde(untagged)]
        enum Content<'a> {
            Legacy(&'a [AssistantPart]),
            Replay {
                parts: &'a [AssistantPart],
                provider_replay: &'a ProviderReplay,
            },
        }
        let wire = match self {
            Self::System(text) => Wire::System(text),
            Self::User(text) => Wire::User(text),
            Self::Assistant(parts) => Wire::Assistant(Content::Legacy(parts)),
            Self::AssistantWithReplay {
                parts,
                provider_replay: replay,
            } => Wire::Assistant(Content::Replay {
                parts,
                provider_replay: replay,
            }),
            Self::Tool {
                tool_call_id,
                output,
                outcome,
            } => Wire::Tool {
                tool_call_id,
                output,
                outcome: *outcome,
            },
        };
        wire.serialize(serializer)
    }
}

impl fmt::Debug for ModelMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System(text) => formatter
                .debug_struct("ModelMessage::System")
                .field("text_bytes", &text.len())
                .finish(),
            Self::User(text) => formatter
                .debug_struct("ModelMessage::User")
                .field("text_bytes", &text.len())
                .finish(),
            Self::Assistant(parts) => formatter
                .debug_struct("ModelMessage::Assistant")
                .field("part_count", &parts.len())
                .field("parts", parts)
                .finish(),
            Self::AssistantWithReplay {
                parts,
                provider_replay,
            } => formatter
                .debug_struct("ModelMessage::AssistantWithReplay")
                .field("provider_replay", provider_replay)
                .field("part_count", &parts.len())
                .field("parts", parts)
                .finish(),
            Self::Tool {
                tool_call_id,
                output,
                outcome,
            } => formatter
                .debug_struct("ModelMessage::Tool")
                .field("tool_call_id", tool_call_id)
                .field("output", output)
                .field("outcome", outcome)
                .finish(),
        }
    }
}

impl ModelMessage {
    pub fn system(text: impl Into<String>) -> Result<Self, ModelValueError> {
        let text = text.into();
        if !valid_text(&text, MAX_MODEL_MESSAGE_TEXT_BYTES, false) {
            return Err(ModelValueError::InvalidText);
        }
        Ok(Self::System(text.into()))
    }

    pub fn user(text: impl Into<String>) -> Result<Self, ModelValueError> {
        let text = text.into();
        if !valid_text(&text, MAX_MODEL_MESSAGE_TEXT_BYTES, false) {
            return Err(ModelValueError::InvalidText);
        }
        Ok(Self::User(text.into()))
    }

    pub fn assistant(parts: Vec<AssistantPart>) -> Result<Self, ModelValueError> {
        Self::assistant_with_provider_replay(parts, None)
    }

    /// Preserves opaque replay; empty parts require metadata.
    pub fn assistant_with_provider_replay(
        parts: Vec<AssistantPart>,
        provider_replay: Option<ProviderReplay>,
    ) -> Result<Self, ModelValueError> {
        let parts = normalize_parts(parts);
        let message = match provider_replay {
            Some(provider_replay) => Self::AssistantWithReplay {
                parts,
                provider_replay,
            },
            None => Self::Assistant(parts),
        };
        message.validate()?;
        Ok(message)
    }

    pub fn tool_with_outcome(
        tool_call_id: ToolCallId,
        output: ToolOutput,
        outcome: ToolResultOutcome,
    ) -> Result<Self, ModelValueError> {
        Ok(Self::Tool {
            tool_call_id,
            output,
            outcome,
        })
    }

    pub fn validate(&self) -> Result<(), ModelValueError> {
        match self {
            Self::System(text) | Self::User(text) => {
                if valid_text(text, MAX_MODEL_MESSAGE_TEXT_BYTES, false) {
                    Ok(())
                } else {
                    Err(ModelValueError::InvalidText)
                }
            }
            Self::Assistant(parts) => {
                if parts.is_empty() {
                    return Err(ModelValueError::EmptyAssistantParts);
                }
                validate_tool_call_order(parts)
            }
            Self::AssistantWithReplay { parts, .. } => validate_tool_call_order(parts),
            Self::Tool { .. } => Ok(()),
        }
    }
}

impl<'de> Deserialize<'de> for ModelMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = match ModelMessageWire::deserialize(deserializer)? {
            ModelMessageWire::System(text) => Self::System(text),
            ModelMessageWire::User(text) => Self::User(text),
            ModelMessageWire::Assistant(content) => match content {
                AssistantMessageWire::Legacy(parts) => Self::Assistant(parts),
                AssistantMessageWire::Replay(content) => {
                    Self::assistant_with_provider_replay(content.parts, content.provider_replay)
                        .map_err(serde::de::Error::custom)?
                }
            },
            ModelMessageWire::Tool {
                tool_call_id,
                output,
                outcome,
            } => Self::Tool {
                tool_call_id,
                output,
                outcome,
            },
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

pub(super) fn validate_tool_call_order(parts: &[AssistantPart]) -> Result<(), ModelValueError> {
    let mut ids = BTreeSet::new();
    let mut indices = BTreeSet::new();
    let mut expected_index = 0_u32;
    for part in parts {
        part.validate()?;
        if let Some(call) = part.as_tool_call() {
            if !indices.insert(call.call_index()) {
                return Err(ModelValueError::DuplicateToolCallIndex);
            }
            if call.call_index() != expected_index {
                return Err(ModelValueError::InvalidToolCallOrder);
            }
            if !ids.insert(call.tool_call_id().clone()) {
                return Err(ModelValueError::DuplicateToolCallId);
            }
            expected_index = expected_index
                .checked_add(1)
                .ok_or(ModelValueError::InvalidToolCallOrder)?;
        }
    }
    Ok(())
}

pub(super) fn normalize_parts(mut parts: Vec<AssistantPart>) -> Vec<AssistantPart> {
    for part in &mut parts {
        if let AssistantPart::Text(text) = part
            && let std::borrow::Cow::Owned(normalized) = normalize_model_text(text)
        {
            *text = normalized.into();
        }
    }
    parts
}
