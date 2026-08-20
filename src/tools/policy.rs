use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::ids::{SessionId, ToolCallId, TurnId};

use super::types::ToolName;

const MAX_REASON_BYTES: usize = 4_096;
const MAX_QUESTION_BYTES: usize = 8_192;
const MAX_CHOICE_BYTES: usize = 1_024;
const MAX_CHOICES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolPolicyError {
    #[error("tool policy text is empty, unsafe, or exceeds its limit")]
    InvalidText,
    #[error("tool policy choices are invalid")]
    InvalidChoices,
}

fn safe_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.chars().all(|character| !character.is_control())
}

fn validate_choices(choices: Option<&[String]>) -> Result<(), ToolPolicyError> {
    if let Some(choices) = choices {
        if choices.is_empty() || choices.len() > MAX_CHOICES {
            return Err(ToolPolicyError::InvalidChoices);
        }
        if choices
            .iter()
            .any(|choice| !safe_text(choice, MAX_CHOICE_BYTES))
        {
            return Err(ToolPolicyError::InvalidChoices);
        }
    }
    Ok(())
}

pub struct ToolRequest<'a> {
    tool_call_id: &'a ToolCallId,
    tool_name: &'a ToolName,
    arguments: &'a Value,
    call_index: u32,
}

impl<'a> ToolRequest<'a> {
    pub const fn new(
        tool_call_id: &'a ToolCallId,
        tool_name: &'a ToolName,
        arguments: &'a Value,
        call_index: u32,
    ) -> Self {
        Self {
            tool_call_id,
            tool_name,
            arguments,
            call_index,
        }
    }

    pub const fn tool_call_id(&self) -> &ToolCallId {
        self.tool_call_id
    }

    pub const fn tool_name(&self) -> &ToolName {
        self.tool_name
    }

    pub const fn arguments(&self) -> &Value {
        self.arguments
    }

    pub const fn call_index(&self) -> u32 {
        self.call_index
    }
}

pub struct ToolContextView<'a> {
    session_id: SessionId,
    turn_id: TurnId,
    enabled_tools: &'a BTreeSet<ToolName>,
}

impl<'a> ToolContextView<'a> {
    pub const fn new(
        session_id: SessionId,
        turn_id: TurnId,
        enabled_tools: &'a BTreeSet<ToolName>,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            enabled_tools,
        }
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn enabled_tools(&self) -> &BTreeSet<ToolName> {
        self.enabled_tools
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", content = "data", rename_all = "snake_case")]
pub enum ToolDecision {
    Allow,
    Deny {
        reason: String,
    },
    Ask {
        question: String,
        choices: Option<Vec<String>>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "decision", content = "data", rename_all = "snake_case")]
enum ToolDecisionWire {
    Allow,
    Deny {
        reason: String,
    },
    Ask {
        question: String,
        choices: Option<Vec<String>>,
    },
}

impl ToolDecision {
    pub fn deny(reason: impl Into<String>) -> Result<Self, ToolPolicyError> {
        let decision = Self::Deny {
            reason: reason.into(),
        };
        decision.validate()?;
        Ok(decision)
    }

    pub fn ask(
        question: impl Into<String>,
        choices: Option<Vec<String>>,
    ) -> Result<Self, ToolPolicyError> {
        let decision = Self::Ask {
            question: question.into(),
            choices,
        };
        decision.validate()?;
        Ok(decision)
    }

    pub fn validate(&self) -> Result<(), ToolPolicyError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny { reason } => {
                if safe_text(reason, MAX_REASON_BYTES) {
                    Ok(())
                } else {
                    Err(ToolPolicyError::InvalidText)
                }
            }
            Self::Ask { question, choices } => {
                if !safe_text(question, MAX_QUESTION_BYTES) {
                    return Err(ToolPolicyError::InvalidText);
                }
                validate_choices(choices.as_deref())
            }
        }
    }
}

impl<'de> Deserialize<'de> for ToolDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let decision = match ToolDecisionWire::deserialize(deserializer)? {
            ToolDecisionWire::Allow => Self::Allow,
            ToolDecisionWire::Deny { reason } => Self::Deny { reason },
            ToolDecisionWire::Ask { question, choices } => Self::Ask { question, choices },
        };
        decision.validate().map_err(serde::de::Error::custom)?;
        Ok(decision)
    }
}

pub trait ToolPolicy: Send + Sync {
    fn decide(&self, request: &ToolRequest<'_>, ctx: &ToolContextView<'_>) -> ToolDecision;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllowConfiguredTools;

impl AllowConfiguredTools {
    pub const fn new() -> Self {
        Self
    }
}

impl ToolPolicy for AllowConfiguredTools {
    fn decide(&self, request: &ToolRequest<'_>, ctx: &ToolContextView<'_>) -> ToolDecision {
        if ctx.enabled_tools().contains(request.tool_name()) {
            ToolDecision::Allow
        } else {
            ToolDecision::Deny {
                reason: "tool is not enabled".to_owned(),
            }
        }
    }
}
