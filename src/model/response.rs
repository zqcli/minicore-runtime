use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::DiagnosticSummary;
use crate::ids::ToolCallId;
use crate::tools::ToolName;
use crate::value::BoundedText;

use super::types::{ModelFinishReason, ModelValueError, Usage};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotStarted,
    Started,
    Unknown,
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

pub const MAX_MODEL_EVENT_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Eq, PartialEq)]
pub enum ModelEvent {
    TextDelta {
        delta: BoundedText,
    },
    ReasoningDelta {
        delta: BoundedText,
    },
    ToolCallStart {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
    },
    ToolCallArgumentsDelta {
        tool_call_id: ToolCallId,
        delta: BoundedText,
    },
    ToolCallEnd {
        tool_call_id: ToolCallId,
    },
    Usage {
        usage: Usage,
    },
    Finish {
        reason: ModelFinishReason,
    },
}

impl ModelEvent {
    pub fn text_delta(delta: impl AsRef<str>) -> Result<Self, ModelValueError> {
        Ok(Self::TextDelta {
            delta: checked_event_text(delta.as_ref())?,
        })
    }

    pub fn reasoning_delta(delta: impl AsRef<str>) -> Result<Self, ModelValueError> {
        Ok(Self::ReasoningDelta {
            delta: checked_event_text(delta.as_ref())?,
        })
    }

    pub fn tool_call_arguments_delta(
        tool_call_id: ToolCallId,
        delta: impl AsRef<str>,
    ) -> Result<Self, ModelValueError> {
        Ok(Self::ToolCallArgumentsDelta {
            tool_call_id,
            delta: checked_event_text(delta.as_ref())?,
        })
    }

    pub fn validate(&self) -> Result<(), ModelValueError> {
        match self {
            Self::TextDelta { delta }
            | Self::ReasoningDelta { delta }
            | Self::ToolCallArgumentsDelta { delta, .. } => validate_event_text(delta.as_str()),
            Self::ToolCallStart { .. }
            | Self::ToolCallEnd { .. }
            | Self::Usage { .. }
            | Self::Finish { .. } => Ok(()),
        }
    }
}

impl fmt::Debug for ModelEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextDelta { delta } => formatter
                .debug_struct("TextDelta")
                .field("delta_bytes", &delta.byte_len())
                .finish(),
            Self::ReasoningDelta { delta } => formatter
                .debug_struct("ReasoningDelta")
                .field("delta_bytes", &delta.byte_len())
                .finish(),
            Self::ToolCallStart {
                tool_call_id,
                tool_name,
            } => formatter
                .debug_struct("ToolCallStart")
                .field("tool_call_id", tool_call_id)
                .field("tool_name", tool_name)
                .finish(),
            Self::ToolCallArgumentsDelta {
                tool_call_id,
                delta,
            } => formatter
                .debug_struct("ToolCallArgumentsDelta")
                .field("tool_call_id", tool_call_id)
                .field("delta_bytes", &delta.byte_len())
                .finish(),
            Self::ToolCallEnd { tool_call_id } => formatter
                .debug_struct("ToolCallEnd")
                .field("tool_call_id", tool_call_id)
                .finish(),
            Self::Usage { usage } => formatter.debug_tuple("Usage").field(usage).finish(),
            Self::Finish { reason } => formatter.debug_tuple("Finish").field(reason).finish(),
        }
    }
}

fn checked_event_text(value: &str) -> Result<BoundedText, ModelValueError> {
    validate_event_text(value)?;
    BoundedText::new_with_max_bytes(value, MAX_MODEL_EVENT_TEXT_BYTES)
        .map_err(|_| ModelValueError::InvalidEvent)
}

fn validate_event_text(value: &str) -> Result<(), ModelValueError> {
    if !value.is_empty()
        && value.len() <= MAX_MODEL_EVENT_TEXT_BYTES
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
    {
        Ok(())
    } else {
        Err(ModelValueError::InvalidEvent)
    }
}
