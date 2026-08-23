use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::ids::{InteractionId, ToolCallId};

use super::types::{ToolName, ToolValueError, valid_text};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum LegacyToolError {
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

const MAX_OUTPUT_BYTES: usize = 262_144;
const MAX_QUESTION_BYTES: usize = 8_192;
const MAX_CHOICE_BYTES: usize = 1_024;
const MAX_CHOICES: usize = 32;

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct LegacyToolOutput {
    text: String,
    is_error: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyToolOutputWire {
    text: String,
    is_error: bool,
}

impl LegacyToolOutput {
    pub(crate) fn new(text: impl AsRef<str>, is_error: bool) -> Result<Self, ToolValueError> {
        let text = text.as_ref();
        if !valid_text(text, MAX_OUTPUT_BYTES, true) {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self {
            text: text.to_owned(),
            is_error,
        })
    }

    pub(crate) fn success(text: impl AsRef<str>) -> Result<Self, ToolValueError> {
        Self::new(text, false)
    }

    pub(crate) fn failure(text: impl AsRef<str>) -> Result<Self, ToolValueError> {
        Self::new(text, true)
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) const fn is_error(&self) -> bool {
        self.is_error
    }
}

impl fmt::Debug for LegacyToolOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LegacyToolOutput")
            .field("text_bytes", &self.text.len())
            .field("is_error", &self.is_error)
            .finish()
    }
}

impl<'de> Deserialize<'de> for LegacyToolOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = LegacyToolOutputWire::deserialize(deserializer)?;
        Self::new(value.text, value.is_error).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyUserQuestion {
    interaction_id: InteractionId,
    question: String,
    choices: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct LegacyUserQuestionWire {
    interaction_id: InteractionId,
    question: String,
    choices: Option<Vec<String>>,
}

impl LegacyUserQuestion {
    pub(crate) fn new(
        interaction_id: InteractionId,
        question: impl Into<String>,
        choices: Option<Vec<String>>,
    ) -> Result<Self, ToolValueError> {
        let question = question.into();
        if !valid_text(&question, MAX_QUESTION_BYTES, false) {
            return Err(ToolValueError::InvalidText);
        }
        if choices.as_ref().is_some_and(|values| {
            values.is_empty()
                || values.len() > MAX_CHOICES
                || values
                    .iter()
                    .any(|value| !valid_text(value, MAX_CHOICE_BYTES, false))
        }) {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self {
            interaction_id,
            question,
            choices,
        })
    }

    pub(crate) const fn interaction_id(&self) -> InteractionId {
        self.interaction_id
    }

    pub(crate) fn question(&self) -> &str {
        &self.question
    }

    pub(crate) fn choices(&self) -> Option<&[String]> {
        self.choices.as_deref()
    }
}

impl<'de> Deserialize<'de> for LegacyUserQuestion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = LegacyUserQuestionWire::deserialize(deserializer)?;
        Self::new(value.interaction_id, value.question, value.choices).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyUserAnswer {
    text: String,
}

#[derive(Deserialize)]
struct LegacyUserAnswerWire {
    text: String,
}

impl LegacyUserAnswer {
    pub(crate) fn new(text: impl Into<String>) -> Result<Self, ToolValueError> {
        let text = text.into();
        if !valid_text(&text, MAX_QUESTION_BYTES, false) {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self { text })
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

impl<'de> Deserialize<'de> for LegacyUserAnswer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = LegacyUserAnswerWire::deserialize(deserializer)?;
        Self::new(value.text).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyToolCallSummary {
    tool_call_id: ToolCallId,
    tool_name: ToolName,
    call_index: u32,
}

#[derive(Deserialize)]
struct LegacyToolCallSummaryWire {
    tool_call_id: ToolCallId,
    tool_name: ToolName,
    call_index: u32,
}

impl LegacyToolCallSummary {
    pub(crate) fn new(
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

    pub(crate) const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub(crate) const fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }

    pub(crate) const fn call_index(&self) -> u32 {
        self.call_index
    }
}

impl<'de> Deserialize<'de> for LegacyToolCallSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = LegacyToolCallSummaryWire::deserialize(deserializer)?;
        Self::new(value.tool_call_id, value.tool_name, value.call_index).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LegacyToolResultStatus {
    Succeeded,
    Failed,
    Cancelled,
    Denied,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LegacyToolResultSummary {
    tool_call_id: ToolCallId,
    status: LegacyToolResultStatus,
}

#[derive(Deserialize)]
struct LegacyToolResultSummaryWire {
    tool_call_id: ToolCallId,
    status: LegacyToolResultStatus,
}

impl LegacyToolResultSummary {
    pub(crate) fn new(
        tool_call_id: ToolCallId,
        status: LegacyToolResultStatus,
    ) -> Result<Self, ToolValueError> {
        Ok(Self {
            tool_call_id,
            status,
        })
    }

    pub(crate) const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub(crate) const fn status(&self) -> LegacyToolResultStatus {
        self.status
    }
}

impl<'de> Deserialize<'de> for LegacyToolResultSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = LegacyToolResultSummaryWire::deserialize(deserializer)?;
        Self::new(value.tool_call_id, value.status).map_err(D::Error::custom)
    }
}

const _: () = {
    // P6 deletion target: remove with the legacy runner/storage DTOs.
    let _: fn(String) -> Result<LegacyToolOutput, ToolValueError> = LegacyToolOutput::success;
    let _ = LegacyUserQuestion::choices;
    let _ = LegacyToolCallSummary::tool_call_id;
    let _ = LegacyToolCallSummary::tool_name;
    let _ = LegacyToolCallSummary::call_index;
    let _ = LegacyToolResultSummary::tool_call_id;
    let _ = LegacyToolResultSummary::status;
};

#[cfg(test)]
mod tests {
    use super::LegacyToolOutput;

    #[test]
    fn legacy_failure_output_round_trips_the_old_wire_shape() {
        let output = LegacyToolOutput::failure("command failed").unwrap();
        let value = serde_json::to_value(&output).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"text": "command failed", "is_error": true})
        );
        let decoded: LegacyToolOutput = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.text(), "command failed");
        assert!(decoded.is_error());
        assert!(
            serde_json::from_value::<LegacyToolOutput>(serde_json::json!({
                "text": "command failed",
                "is_error": true,
                "extra": false
            }))
            .is_err()
        );
    }
}
