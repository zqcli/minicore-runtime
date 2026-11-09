use std::fmt;

use serde::{Deserialize, Serialize};

use crate::value::{Text, ValueError, valid_text};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCategory {
    Configuration,
    Model,
    Tool,
    Policy,
    Context,
    Cancellation,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    InvalidConfiguration,
    ModelTimeout,
    ModelMalformedResponse,
    ModelUnavailable,
    RuntimeTerminated,
    ContextFailed,
    PolicyDenied,
    PolicyFailed,
    ToolNotFound,
    ToolTimeout,
    ToolFailed,
    InteractionNotFound,
    InteractionKindMismatch,
    Internal,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct DiagnosticSummary {
    pub code: DiagnosticCode,
    pub category: DiagnosticCategory,
    message: Text,
    pub retryable: bool,
}

impl fmt::Debug for DiagnosticSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticSummary")
            .field("code", &self.code)
            .field("category", &self.category)
            .field("message_bytes", &self.message.byte_len())
            .field("retryable", &self.retryable)
            .finish()
    }
}

impl DiagnosticSummary {
    pub const fn message(&self) -> &Text {
        &self.message
    }

    pub fn new(
        code: DiagnosticCode,
        category: DiagnosticCategory,
        message: Text,
        retryable: bool,
    ) -> Result<Self, ValueError> {
        if !valid_text(message.as_str(), true) {
            return Err(ValueError::InvalidText);
        }
        Ok(Self {
            code,
            category,
            message,
            retryable,
        })
    }

    pub(crate) fn static_text(
        code: DiagnosticCode,
        category: DiagnosticCategory,
        message: &'static str,
        retryable: bool,
    ) -> Self {
        Self::new(code, category, Text::new(message), retryable)
            .expect("static diagnostic text is valid")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticSummaryWire {
    code: DiagnosticCode,
    category: DiagnosticCategory,
    message: Text,
    retryable: bool,
}

impl<'de> Deserialize<'de> for DiagnosticSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = DiagnosticSummaryWire::deserialize(deserializer)?;
        Self::new(value.code, value.category, value.message, value.retryable)
            .map_err(serde::de::Error::custom)
    }
}
