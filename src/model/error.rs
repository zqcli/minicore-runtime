use crate::error::DiagnosticSummary;
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, time::Duration};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotStarted,
    Started,
    Unknown,
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
    #[error("provider replay format or payload exceeds its bounds")]
    InvalidProviderReplay,
    #[error("model request tools are invalid or not strictly ordered")]
    InvalidTools,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    InvalidRequest,
    Unavailable,
    ProviderUnavailable,
    AuthMissing,
    AuthRejected,
    RateLimited,
    QuotaExceeded,
    ContextOverflow,
    Timeout,
    TransportUnavailable,
    Cancelled,
    InvalidProviderResponse,
    IncompleteResponse,
    StreamInterrupted,
    RequestOutcomeUnknown,
    UnexpectedToolCall,
    Panicked,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryHint {
    Never,
    Retryable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after: Option<Duration>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryableWire {
    #[serde(default)]
    retry_after: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RetryHintWire {
    Never,
    Retryable(RetryableWire),
}

impl<'de> Deserialize<'de> for RetryHint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RetryHintWire::deserialize(deserializer)?;
        match wire {
            RetryHintWire::Never => Ok(Self::Never),
            RetryHintWire::Retryable(RetryableWire { retry_after }) => {
                Ok(Self::Retryable { retry_after })
            }
        }
    }
}

/// Structured model error carrying explicit delivery state, retry hint, and diagnostic.
///
/// Deserialization strictly rejects unknown fields and rejects `RetryHint::Retryable`
/// if `delivery` is not `DeliveryState::NotStarted`. In both constructors and
/// deserialization, the inner `diagnostic.retryable` flag is automatically normalized
/// to match `retry_hint` (`true` for `Retryable`, `false` for `Never`).
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelError {
    kind: ModelErrorKind,
    delivery: DeliveryState,
    retry_hint: RetryHint,
    diagnostic: DiagnosticSummary,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelErrorWire {
    kind: ModelErrorKind,
    delivery: DeliveryState,
    retry_hint: RetryHint,
    diagnostic: DiagnosticSummary,
}

impl<'de> Deserialize<'de> for ModelError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelErrorWire::deserialize(deserializer)?;
        match wire.retry_hint {
            RetryHint::Retryable { .. } if wire.delivery != DeliveryState::NotStarted => {
                Err(serde::de::Error::custom(
                    "retryable model error requires delivery state NotStarted",
                ))
            }
            RetryHint::Retryable { retry_after } => {
                Ok(Self::not_started(wire.kind, retry_after, wire.diagnostic))
            }
            RetryHint::Never => Ok(Self::permanent(wire.kind, wire.delivery, wire.diagnostic)),
        }
    }
}

impl ModelError {
    /// Constructs a retryable model error with `NotStarted` delivery state.
    ///
    /// The inner diagnostic summary's `retryable` flag is normalized to `true`
    /// to remain strictly consistent with `RetryHint::Retryable`.
    pub fn not_started(
        kind: ModelErrorKind,
        retry_after: Option<Duration>,
        mut diagnostic: DiagnosticSummary,
    ) -> Self {
        diagnostic.retryable = true;
        Self {
            kind,
            delivery: DeliveryState::NotStarted,
            retry_hint: RetryHint::Retryable { retry_after },
            diagnostic,
        }
    }

    /// Constructs a non-retryable model error with `Started` delivery state.
    ///
    /// The inner diagnostic summary's `retryable` flag is normalized to `false`
    /// to remain strictly consistent with `RetryHint::Never`.
    pub fn started(kind: ModelErrorKind, mut diagnostic: DiagnosticSummary) -> Self {
        diagnostic.retryable = false;
        Self {
            kind,
            delivery: DeliveryState::Started,
            retry_hint: RetryHint::Never,
            diagnostic,
        }
    }

    /// Constructs a non-retryable model error with `Unknown` delivery state.
    ///
    /// The inner diagnostic summary's `retryable` flag is normalized to `false`
    /// to remain strictly consistent with `RetryHint::Never`.
    pub fn unknown(kind: ModelErrorKind, mut diagnostic: DiagnosticSummary) -> Self {
        diagnostic.retryable = false;
        Self {
            kind,
            delivery: DeliveryState::Unknown,
            retry_hint: RetryHint::Never,
            diagnostic,
        }
    }

    /// Constructs a permanent non-retryable model error with explicit delivery.
    ///
    /// The inner diagnostic summary's `retryable` flag is normalized to `false`
    /// to remain strictly consistent with `RetryHint::Never`.
    pub fn permanent(
        kind: ModelErrorKind,
        delivery: DeliveryState,
        mut diagnostic: DiagnosticSummary,
    ) -> Self {
        diagnostic.retryable = false;
        Self {
            kind,
            delivery,
            retry_hint: RetryHint::Never,
            diagnostic,
        }
    }

    pub const fn kind(&self) -> ModelErrorKind {
        self.kind
    }

    pub const fn delivery(&self) -> DeliveryState {
        self.delivery
    }

    pub const fn retry_hint(&self) -> &RetryHint {
        &self.retry_hint
    }

    pub const fn diagnostic(&self) -> &DiagnosticSummary {
        &self.diagnostic
    }
}

impl fmt::Debug for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelError")
            .field("kind", &self.kind)
            .field("delivery", &self.delivery)
            .field("retry_hint", &self.retry_hint)
            .field("diagnostic", &self.diagnostic)
            .finish()
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "model error: kind={:?}, delivery={:?}, retry_hint={:?}",
            self.kind, self.delivery, self.retry_hint
        )
    }
}

impl std::error::Error for ModelError {}
