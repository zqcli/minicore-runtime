use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::value::BoundedText;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicErrorCode {
    InvalidInput,
    Busy,
    NotFound,
    Closing,
    InteractionMismatch,
    Unavailable,
    Cancelled,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicErrorSummary {
    pub code: PublicErrorCode,
    pub retryable: bool,
}

impl PublicErrorSummary {
    pub const fn new(code: PublicErrorCode) -> Self {
        Self {
            code,
            retryable: matches!(code, PublicErrorCode::Busy | PublicErrorCode::Unavailable),
        }
    }

    pub const fn with_retryable(code: PublicErrorCode, retryable: bool) -> Self {
        Self { code, retryable }
    }

    pub const fn code(&self) -> PublicErrorCode {
        self.code
    }

    pub const fn retryable(&self) -> bool {
        self.retryable
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCategory {
    Configuration,
    Model,
    Tool,
    Policy,
    Context,
    Compaction,
    Storage,
    Cancellation,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    InvalidConfiguration,
    InvalidSessionManifest,
    SessionClosed,
    SessionBusy,
    SessionDegraded,
    CommandBackpressure,
    InteractionNotFound,
    InteractionKindMismatch,
    ModelMismatch,
    ModelTimeout,
    ModelMalformedResponse,
    ModelUnavailable,
    ContextFailed,
    PolicyDenied,
    PolicyFailed,
    ToolNotFound,
    ToolTimeout,
    ToolFailed,
    TurnBudgetExceeded,
    LogConflict,
    LogCorrupt,
    LogUnknownOutcome,
    RuntimeTerminated,
    ShutdownTimeout,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticSummary {
    pub code: DiagnosticCode,
    pub category: DiagnosticCategory,
    pub message: BoundedText,
    pub retryable: bool,
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

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
pub(crate) enum RuntimeError {
    #[error("invalid runtime configuration")]
    InvalidConfiguration,
    #[error("runtime is closing")]
    Closing,
    #[error("runtime internal failure")]
    Internal,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
pub(crate) enum SessionError {
    #[error("session not found")]
    NotFound,
    #[error("session is already loaded")]
    AlreadyLoaded,
    #[error("session is busy")]
    Busy,
    #[error("session is closing")]
    Closing,
    #[error("interaction does not match the current turn")]
    InteractionMismatch,
    #[error("session input is invalid")]
    InvalidInput,
    #[error("session is unavailable")]
    Unavailable,
    #[error("session internal failure")]
    Internal,
}
