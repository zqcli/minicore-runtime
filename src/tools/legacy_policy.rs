// P5/P6 deletion target: remove with the legacy runner policy and interaction flow.

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
pub(crate) enum LegacyToolPolicyError {
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

fn validate_choices(choices: Option<&[String]>) -> Result<(), LegacyToolPolicyError> {
    if let Some(choices) = choices {
        if choices.is_empty() || choices.len() > MAX_CHOICES {
            return Err(LegacyToolPolicyError::InvalidChoices);
        }
        if choices
            .iter()
            .any(|choice| !safe_text(choice, MAX_CHOICE_BYTES))
        {
            return Err(LegacyToolPolicyError::InvalidChoices);
        }
    }
    Ok(())
}

pub(crate) struct LegacyToolRequest<'a> {
    tool_call_id: &'a ToolCallId,
    tool_name: &'a ToolName,
    arguments: &'a Value,
    call_index: u32,
}

impl<'a> LegacyToolRequest<'a> {
    pub(crate) const fn new(
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

    pub(crate) const fn tool_call_id(&self) -> &ToolCallId {
        self.tool_call_id
    }

    pub(crate) const fn tool_name(&self) -> &ToolName {
        self.tool_name
    }

    pub(crate) const fn arguments(&self) -> &Value {
        self.arguments
    }

    pub(crate) const fn call_index(&self) -> u32 {
        self.call_index
    }
}

pub(crate) struct LegacyToolContextView<'a> {
    session_id: SessionId,
    turn_id: TurnId,
    enabled_tools: &'a BTreeSet<ToolName>,
}

impl<'a> LegacyToolContextView<'a> {
    pub(crate) const fn new(
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

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn enabled_tools(&self) -> &BTreeSet<ToolName> {
        self.enabled_tools
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", content = "data", rename_all = "snake_case")]
pub(crate) enum LegacyToolDecision {
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
enum LegacyToolDecisionWire {
    Allow,
    Deny {
        reason: String,
    },
    Ask {
        question: String,
        choices: Option<Vec<String>>,
    },
}

impl LegacyToolDecision {
    pub(crate) fn deny(reason: impl Into<String>) -> Result<Self, LegacyToolPolicyError> {
        let decision = Self::Deny {
            reason: reason.into(),
        };
        decision.validate()?;
        Ok(decision)
    }

    pub(crate) fn ask(
        question: impl Into<String>,
        choices: Option<Vec<String>>,
    ) -> Result<Self, LegacyToolPolicyError> {
        let decision = Self::Ask {
            question: question.into(),
            choices,
        };
        decision.validate()?;
        Ok(decision)
    }

    pub(crate) fn validate(&self) -> Result<(), LegacyToolPolicyError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny { reason } => {
                if safe_text(reason, MAX_REASON_BYTES) {
                    Ok(())
                } else {
                    Err(LegacyToolPolicyError::InvalidText)
                }
            }
            Self::Ask { question, choices } => {
                if !safe_text(question, MAX_QUESTION_BYTES) {
                    return Err(LegacyToolPolicyError::InvalidText);
                }
                validate_choices(choices.as_deref())
            }
        }
    }
}

impl<'de> Deserialize<'de> for LegacyToolDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let decision = match LegacyToolDecisionWire::deserialize(deserializer)? {
            LegacyToolDecisionWire::Allow => Self::Allow,
            LegacyToolDecisionWire::Deny { reason } => Self::Deny { reason },
            LegacyToolDecisionWire::Ask { question, choices } => Self::Ask { question, choices },
        };
        decision.validate().map_err(serde::de::Error::custom)?;
        Ok(decision)
    }
}

pub(crate) trait LegacyToolPolicy: Send + Sync {
    fn decide(
        &self,
        request: &LegacyToolRequest<'_>,
        ctx: &LegacyToolContextView<'_>,
    ) -> LegacyToolDecision;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LegacyAllowConfiguredTools;

impl LegacyAllowConfiguredTools {
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl LegacyToolPolicy for LegacyAllowConfiguredTools {
    fn decide(
        &self,
        request: &LegacyToolRequest<'_>,
        ctx: &LegacyToolContextView<'_>,
    ) -> LegacyToolDecision {
        if ctx.enabled_tools().contains(request.tool_name()) {
            LegacyToolDecision::Allow
        } else {
            LegacyToolDecision::Deny {
                reason: "tool is not enabled".to_owned(),
            }
        }
    }
}

const _: () = {
    let _ = LegacyToolRequest::tool_call_id;
    let _ = LegacyToolRequest::arguments;
    let _ = LegacyToolRequest::call_index;
    let _ = LegacyToolContextView::session_id;
    let _ = LegacyToolContextView::turn_id;
    let _: fn(String) -> Result<LegacyToolDecision, LegacyToolPolicyError> =
        LegacyToolDecision::deny;
    let _: fn(String, Option<Vec<String>>) -> Result<LegacyToolDecision, LegacyToolPolicyError> =
        LegacyToolDecision::ask;
};
