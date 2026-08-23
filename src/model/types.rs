use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

use crate::ids::ToolCallId;
pub(crate) use crate::tools::ToolSpec;
use crate::tools::{ToolName, ToolOutput, ToolResultOutcome, validate_json_shape};

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

impl fmt::Display for ModelRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for ModelRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for ModelRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ModelRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

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
    #[error("model request tools are invalid or not strictly ordered")]
    InvalidTools,
}

fn deserialize_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(D::Error::custom)
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
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
    Text(String),
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
    Text(String),
    Reasoning(ReasoningContent),
    ToolCall(ToolCall),
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ReasoningContent {
    text: Option<String>,
    summary: Option<String>,
    encrypted: Option<String>,
    signature: Option<String>,
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
        validate_optional_text(&text, 262_144)?;
        validate_optional_text(&summary, 131_072)?;
        validate_optional_opaque(&encrypted, 262_144)?;
        validate_optional_opaque(&signature, 16_384)?;
        if text.is_none() && summary.is_none() && encrypted.is_none() && signature.is_none() {
            return Err(ModelValueError::EmptyReasoningContent);
        }
        Ok(Self {
            text,
            summary,
            encrypted,
            signature,
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
            .field("text_bytes", &self.text.as_ref().map(String::len))
            .field("summary_bytes", &self.summary.as_ref().map(String::len))
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

fn validate_optional_text(value: &Option<String>, maximum: usize) -> Result<(), ModelValueError> {
    if value.as_deref().is_some_and(|value| {
        value.is_empty()
            || value.len() > maximum
            || value
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    }) {
        Err(ModelValueError::InvalidText)
    } else {
        Ok(())
    }
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
                if valid_text(text, 262_144) {
                    Ok(())
                } else {
                    Err(ModelValueError::InvalidText)
                }
            }
            Self::Reasoning(reasoning) => {
                if reasoning.text.is_some()
                    || reasoning.summary.is_some()
                    || reasoning.encrypted.is_some()
                    || reasoning.signature.is_some()
                {
                    Ok(())
                } else {
                    Err(ModelValueError::EmptyReasoningContent)
                }
            }
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
            AssistantPartWire::Text(text) => Self::Text(text),
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
    arguments: Value,
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
        if !arguments.is_object() || !validate_json_shape(&arguments) {
            return Err(ModelValueError::InvalidToolArguments);
        }
        Ok(Self {
            tool_call_id,
            name,
            arguments,
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

    pub const fn arguments(&self) -> &Value {
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

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub enum ModelMessage {
    System(String),
    User(String),
    Assistant(Vec<AssistantPart>),
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
    System(String),
    User(String),
    Assistant(Vec<AssistantPart>),
    Tool {
        tool_call_id: ToolCallId,
        output: ToolOutput,
        outcome: ToolResultOutcome,
    },
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
        if !valid_text(&text, 262_144) {
            return Err(ModelValueError::InvalidText);
        }
        Ok(Self::System(text))
    }

    pub fn user(text: impl Into<String>) -> Result<Self, ModelValueError> {
        let text = text.into();
        if !valid_text(&text, 262_144) {
            return Err(ModelValueError::InvalidText);
        }
        Ok(Self::User(text))
    }

    pub fn assistant(parts: Vec<AssistantPart>) -> Result<Self, ModelValueError> {
        let message = Self::Assistant(parts);
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
                if valid_text(text, 262_144) {
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
            ModelMessageWire::Assistant(parts) => Self::Assistant(parts),
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
    messages: Vec<ModelMessage>,
    tools: Vec<ToolSpec>,
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
        tools: Vec<ToolSpec>,
        limits: ModelLimits,
        reasoning: ReasoningPreference,
    ) -> Result<Self, ModelValueError> {
        if messages.is_empty() {
            return Err(ModelValueError::EmptyMessages);
        }
        validate_tool_exchange(&messages)?;
        validate_request_tools(&tools)?;
        Ok(Self {
            messages,
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelResponseWire {
    parts: Vec<AssistantPart>,
    finish_reason: ModelFinishReason,
    usage: Option<Usage>,
}

impl ModelResponse {
    pub fn new(
        parts: Vec<AssistantPart>,
        finish_reason: ModelFinishReason,
        usage: impl Into<Option<Usage>>,
    ) -> Result<Self, ModelValueError> {
        if parts.is_empty() {
            return Err(ModelValueError::EmptyResponseParts);
        }
        validate_tool_call_order(&parts)?;
        Ok(Self {
            parts,
            finish_reason,
            usage: usage.into(),
        })
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
        Self::new(value.parts, value.finish_reason, value.usage).map_err(serde::de::Error::custom)
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
            ModelMessage::Assistant(parts) => {
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
