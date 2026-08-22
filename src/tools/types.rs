use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

use crate::ids::{InteractionId, ToolCallId};
use crate::value::{MAX_JSON_BYTES, validate_json_size};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolError {
    #[error("tool is unknown")]
    UnknownTool,
    #[error("tool name is already registered")]
    DuplicateTool,
    #[error("tool execution was cancelled")]
    Cancelled,
    #[error("tool interaction channel is closed")]
    InteractionClosed,
    #[error("tool interaction is already pending")]
    InteractionBusy,
    #[error("tool interaction is invalid")]
    InvalidInteraction,
    #[error("tool operation panicked")]
    Panicked,
    #[error("tool operation failed internally")]
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolNameError {
    #[error("tool name must be 1..=64 bytes")]
    InvalidLength,
    #[error("tool name must use the stable symbolic grammar")]
    InvalidGrammar,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolValueError {
    #[error("tool value text is empty, unsafe, or exceeds its limit")]
    InvalidText,
    #[error("tool specification schema must be a JSON object")]
    InvalidSchema,
    #[error("tool summary is invalid")]
    InvalidSummary,
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
    !value.is_empty() && valid_text_or_empty(value, maximum)
}

fn valid_text_or_empty(value: &str, maximum: usize) -> bool {
    value.len() <= maximum
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

pub(crate) fn validate_json_shape(value: &Value) -> bool {
    validate_json_size(value, MAX_JSON_BYTES).is_ok()
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolName(Box<str>);

impl ToolName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolName {
    type Err = ToolNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > 64 {
            return Err(ToolNameError::InvalidLength);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ToolNameError::InvalidGrammar);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for ToolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolSpec {
    name: ToolName,
    description: String,
    input_schema: Value,
}

#[derive(Deserialize)]
struct ToolSpecWire {
    name: ToolName,
    description: String,
    input_schema: Value,
}

impl ToolSpec {
    pub fn new(
        name: ToolName,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Result<Self, ToolValueError> {
        let description = description.into();
        if !valid_text(&description, 4_096) {
            return Err(ToolValueError::InvalidText);
        }
        if !input_schema.is_object() || !validate_json_shape(&input_schema) {
            return Err(ToolValueError::InvalidSchema);
        }
        Ok(Self {
            name,
            description,
            input_schema,
        })
    }

    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

impl<'de> Deserialize<'de> for ToolSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolSpecWire::deserialize(deserializer)?;
        Self::new(value.name, value.description, value.input_schema)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolOutput {
    text: String,
    is_error: bool,
}

#[derive(Deserialize)]
struct ToolOutputWire {
    text: String,
    is_error: bool,
}

impl ToolOutput {
    pub fn new(text: impl Into<String>, is_error: bool) -> Result<Self, ToolValueError> {
        let text = text.into();
        if !valid_text_or_empty(&text, 262_144) {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self { text, is_error })
    }

    pub fn success(text: impl Into<String>) -> Result<Self, ToolValueError> {
        Self::new(text, false)
    }

    pub fn failure(text: impl Into<String>) -> Result<Self, ToolValueError> {
        Self::new(text, true)
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn is_error(&self) -> bool {
        self.is_error
    }
}

impl<'de> Deserialize<'de> for ToolOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolOutputWire::deserialize(deserializer)?;
        Self::new(value.text, value.is_error).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UserQuestion {
    interaction_id: InteractionId,
    question: String,
    choices: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct UserQuestionWire {
    interaction_id: InteractionId,
    question: String,
    choices: Option<Vec<String>>,
}

impl UserQuestion {
    pub fn new(
        interaction_id: InteractionId,
        question: impl Into<String>,
        choices: Option<Vec<String>>,
    ) -> Result<Self, ToolValueError> {
        let question = question.into();
        if !valid_text(&question, 8_192) {
            return Err(ToolValueError::InvalidText);
        }
        if choices.as_ref().is_some_and(|values| {
            values.is_empty()
                || values.len() > 32
                || values.iter().any(|value| !valid_text(value, 1_024))
        }) {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self {
            interaction_id,
            question,
            choices,
        })
    }

    pub const fn interaction_id(&self) -> InteractionId {
        self.interaction_id
    }

    pub fn question(&self) -> &str {
        &self.question
    }

    pub fn choices(&self) -> Option<&[String]> {
        self.choices.as_deref()
    }
}

impl<'de> Deserialize<'de> for UserQuestion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = UserQuestionWire::deserialize(deserializer)?;
        Self::new(value.interaction_id, value.question, value.choices)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UserAnswer {
    text: String,
}

#[derive(Deserialize)]
struct UserAnswerWire {
    text: String,
}

impl UserAnswer {
    pub fn new(text: impl Into<String>) -> Result<Self, ToolValueError> {
        let text = text.into();
        if !valid_text(&text, 8_192) {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self { text })
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl<'de> Deserialize<'de> for UserAnswer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = UserAnswerWire::deserialize(deserializer)?;
        Self::new(value.text).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolCallSummary {
    tool_call_id: ToolCallId,
    tool_name: ToolName,
    call_index: u32,
}

#[derive(Deserialize)]
struct ToolCallSummaryWire {
    tool_call_id: ToolCallId,
    tool_name: ToolName,
    call_index: u32,
}

impl ToolCallSummary {
    pub fn new(
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        call_index: u32,
    ) -> Result<Self, ToolValueError> {
        Ok(Self {
            tool_call_id,
            tool_name,
            call_index,
        })
    }

    pub const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub const fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }

    pub const fn call_index(&self) -> u32 {
        self.call_index
    }
}

impl<'de> Deserialize<'de> for ToolCallSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolCallSummaryWire::deserialize(deserializer)?;
        Self::new(value.tool_call_id, value.tool_name, value.call_index)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Succeeded,
    Failed,
    Cancelled,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultOutcome {
    Success,
    Failed,
    Denied,
    Cancelled,
    InputProvided,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolResultSummary {
    tool_call_id: ToolCallId,
    status: ToolResultStatus,
}

#[derive(Deserialize)]
struct ToolResultSummaryWire {
    tool_call_id: ToolCallId,
    status: ToolResultStatus,
}

impl ToolResultSummary {
    pub fn new(tool_call_id: ToolCallId, status: ToolResultStatus) -> Result<Self, ToolValueError> {
        Ok(Self {
            tool_call_id,
            status,
        })
    }

    pub const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub const fn status(&self) -> ToolResultStatus {
        self.status
    }
}

impl<'de> Deserialize<'de> for ToolResultSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolResultSummaryWire::deserialize(deserializer)?;
        Self::new(value.tool_call_id, value.status).map_err(serde::de::Error::custom)
    }
}
