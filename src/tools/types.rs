use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolError {
    #[error("tool execution was cancelled")]
    Cancelled,
    #[error("tool execution failed")]
    Failed,
    #[error("tool execution timed out")]
    TimedOut,
    #[error("tool operation panicked")]
    Panicked,
    #[error("tool invocation is invalid")]
    InvalidInvocation,
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
    #[error("tool input answer is invalid for the request")]
    InvalidAnswer,
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

crate::value::string_newtype_serde!(ToolName);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultOutcome {
    Success,
    Failed,
    Denied,
    Cancelled,
    InputProvided,
}
