use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::TurnId;
use crate::value::BoundedText;

mod operations;

pub use operations::{
    SessionLogError, SessionLogErrorKind, SessionOpenError, SessionOpenErrorKind,
    SessionShutdownError,
};

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

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EventStreamTakenError {
    #[error("session event stream was already taken")]
    AlreadyTaken,
}

#[non_exhaustive]
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SessionError {
    #[error("session is closed")]
    Closed,
    #[error("session is busy")]
    Busy { active_turn: TurnId },
    #[error("session durability is degraded")]
    Degraded(DiagnosticSummary),
    #[error("session command queue is full")]
    Backpressure,
    #[error("session input is invalid")]
    InvalidInput(DiagnosticSummary),
    #[error("session interaction was not found")]
    InteractionNotFound,
    #[error("session interaction answer kind does not match")]
    InteractionKindMismatch,
    #[error("session interaction was already resolved")]
    InteractionAlreadyResolved,
    #[error("session transcript is unavailable")]
    TranscriptUnavailable(DiagnosticSummary),
}

#[non_exhaustive]
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TurnWaitError {
    #[error("turn durability outcome is unknown")]
    DurabilityUnknown(DiagnosticSummary),
    #[error("turn durability is unavailable")]
    DurabilityUnavailable(DiagnosticSummary),
    #[error("turn runtime terminated before durable completion")]
    RuntimeTerminated(DiagnosticSummary),
}
