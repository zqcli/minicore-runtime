use std::fmt;

use serde::{Deserialize, Serialize};

use crate::value::BoundedText;

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
    TurnBudgetExceeded,
    Internal,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct DiagnosticSummary {
    pub code: DiagnosticCode,
    pub category: DiagnosticCategory,
    pub message: BoundedText,
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
    pub const fn new(
        code: DiagnosticCode,
        category: DiagnosticCategory,
        message: BoundedText,
        retryable: bool,
    ) -> Self {
        Self {
            code,
            category,
            message,
            retryable,
        }
    }

    pub(crate) fn bounded_static(
        code: DiagnosticCode,
        category: DiagnosticCategory,
        message: &'static str,
        retryable: bool,
    ) -> Self {
        Self::new(
            code,
            category,
            BoundedText::new(message).expect("static diagnostic must fit BoundedText"),
            retryable,
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticSummaryWire {
    code: DiagnosticCode,
    category: DiagnosticCategory,
    message: BoundedText,
    retryable: bool,
}

impl<'de> Deserialize<'de> for DiagnosticSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = DiagnosticSummaryWire::deserialize(deserializer)?;
        Ok(Self::new(
            value.code,
            value.category,
            value.message,
            value.retryable,
        ))
    }
}
