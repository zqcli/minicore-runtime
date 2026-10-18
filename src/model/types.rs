use crate::value::{normalize_model_text, valid_text};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

use super::ProviderReplay;
use crate::ids::ToolCallId;
pub(crate) use crate::tools::ToolSpec;
use crate::tools::{ToolName, ToolOutput, ToolResultOutcome};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelRefError {
    #[error("model reference must be 1..=256 bytes")]
    InvalidLength,
    #[error("model reference violates its stable symbolic grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModelRef(Box<str>);

impl ModelRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ModelRef {
    type Err = ModelRefError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > 256 {
            return Err(ModelRefError::InvalidLength);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        }) {
            return Err(ModelRefError::InvalidGrammar);
        }
        Ok(Self(value.into()))
    }
}

crate::value::string_newtype_serde!(ModelRef);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelLimitsError {
    #[error("model limit values must be non-zero")]
    Zero,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
pub enum ModelValueError {
    #[error("model text is empty, unsafe, or exceeds its limit")]
    InvalidText,
    #[error("reasoning content has no artifact")]
    EmptyReasoningContent,
    #[error("model message list must not be empty")]
    EmptyMessages,
    #[error("assistant parts must not be empty")]
    EmptyAssistantParts,
    #[error("tool call arguments must be a JSON object")]
    InvalidToolArguments,
    #[error("tool result does not match a pending assistant tool call")]
    OrphanToolResult,
    #[error("tool result id is duplicated in the current exchange")]
    DuplicateToolResult,
    #[error("assistant tool calls must be completed before the next message")]
    IncompleteToolExchange,
    #[error("model response parts must not be empty")]
    EmptyResponseParts,
    #[error("assistant tool call IDs must be unique")]
    DuplicateToolCallId,
    #[error("assistant tool call indices must be unique")]
    DuplicateToolCallIndex,
    #[error("assistant tool call indices must be contiguous and ordered")]
    InvalidToolCallOrder,
    #[error("model descriptor is invalid")]
    InvalidDescriptor,
    #[error("model stream event is invalid")]
    InvalidEvent,
    #[error("provider replay format or payload exceeds its bounds")]
    InvalidProviderReplay,
    #[error("model request tools are invalid or not strictly ordered")]
    InvalidTools,
}

/// Absolute ceiling for a single `ModelMessage` text payload (system, user,
/// or an assistant text part). Default prompt projection truncates summary
/// tails so this bound is always satisfied.
pub(crate) const MAX_MODEL_MESSAGE_TEXT_BYTES: usize = 262_144;

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
            Self::Reasoning(_) => Ok(()),
            Self::ToolCall(call) => call.validate(),
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
        if !arguments.is_object()
            || crate::value::validate_json_size(&arguments, crate::value::MAX_JSON_BYTES).is_err()
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

    pub fn validate(&self) -> Result<(), ModelValueError> {
        if self.arguments.is_object() {
            Ok(())
        } else {
            Err(ModelValueError::InvalidToolArguments)
        }
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
                .debug_struct("ModelMessage::Assistant")
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
        if messages.is_empty() {
            return Err(ModelValueError::EmptyMessages);
        }
        let tools = tools.into();
        validate_tool_exchange(&messages)?;
        validate_request_tools(&tools)?;
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_total_tokens: Option<u64>,
}

impl Usage {
    pub const fn new(input_tokens: u64, output_tokens: u64, reasoning_tokens: u64) -> Self {
        Self::from_optional(
            Some(input_tokens),
            Some(output_tokens),
            Some(reasoning_tokens),
        )
    }

    pub const fn from_optional(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens: None,
            cache_write_tokens: None,
            provider_total_tokens: None,
        }
    }

    pub const fn with_cache_read_tokens(mut self, value: Option<u64>) -> Self {
        self.cache_read_tokens = value;
        self
    }

    pub const fn with_cache_write_tokens(mut self, value: Option<u64>) -> Self {
        self.cache_write_tokens = value;
        self
    }

    pub const fn with_provider_total_tokens(mut self, value: Option<u64>) -> Self {
        self.provider_total_tokens = value;
        self
    }

    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    pub const fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }

    pub const fn cache_read_tokens(&self) -> Option<u64> {
        self.cache_read_tokens
    }

    pub const fn cache_write_tokens(&self) -> Option<u64> {
        self.cache_write_tokens
    }

    pub const fn provider_total_tokens(&self) -> Option<u64> {
        self.provider_total_tokens
    }

    /// Sums only when all three core token counts are reported and the sum fits.
    pub const fn total_tokens(&self) -> Option<u64> {
        let (Some(input), Some(output), Some(reasoning)) =
            (self.input_tokens, self.output_tokens, self.reasoning_tokens)
        else {
            return None;
        };
        let Some(total) = input.checked_add(output) else {
            return None;
        };
        total.checked_add(reasoning)
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

fn validate_tool_call_order(parts: &[AssistantPart]) -> Result<(), ModelValueError> {
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

fn validate_request_tools(tools: &[ToolSpec]) -> Result<(), ModelValueError> {
    let mut previous: Option<&ToolName> = None;
    for tool in tools {
        tool.validate().map_err(|_| ModelValueError::InvalidTools)?;
        if previous.is_some_and(|previous| previous >= tool.name()) {
            return Err(ModelValueError::InvalidTools);
        }
        previous = Some(tool.name());
    }
    Ok(())
}

fn normalize_parts(mut parts: Vec<AssistantPart>) -> Vec<AssistantPart> {
    for part in &mut parts {
        if let AssistantPart::Text(text) = part {
            if let std::borrow::Cow::Owned(normalized) = normalize_model_text(text) {
                *text = normalized.into();
            }
        }
    }
    parts
}
