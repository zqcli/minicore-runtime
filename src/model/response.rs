use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

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
pub struct ModelErrorDetails {
    kind: ModelErrorKind,
    delivery: DeliveryState,
    retryable: bool,
    retry_after_ms: Option<u64>,
}

impl ModelErrorDetails {
    fn new(
        kind: ModelErrorKind,
        delivery: DeliveryState,
        retryable: bool,
        retry_after_ms: Option<u64>,
    ) -> Result<Self, ModelError> {
        if (retryable || retry_after_ms.is_some()) && delivery != DeliveryState::NotStarted {
            return Err(ModelError::InvalidRequest);
        }
        if retry_after_ms.is_some() && !retryable {
            return Err(ModelError::InvalidRequest);
        }
        Ok(Self {
            kind,
            delivery,
            retryable,
            retry_after_ms,
        })
    }

    pub const fn kind(&self) -> ModelErrorKind {
        self.kind
    }

    pub const fn delivery(&self) -> DeliveryState {
        self.delivery
    }

    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    pub const fn retry_after(&self) -> Option<Duration> {
        match self.retry_after_ms {
            Some(milliseconds) => Some(Duration::from_millis(milliseconds)),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ModelError {
    #[error("model request is invalid")]
    InvalidRequest,
    #[error("model is unavailable")]
    Unavailable,
    #[error("model provider is unavailable")]
    ProviderUnavailable,
    #[error("model credentials are missing")]
    AuthMissing,
    #[error("model credentials were rejected")]
    AuthRejected,
    #[error("model provider rate limited the request")]
    RateLimited,
    #[error("model quota was exceeded")]
    QuotaExceeded,
    #[error("model request exceeds context limits")]
    ContextOverflow,
    #[error("model request timed out")]
    Timeout,
    #[error("model transport is unavailable")]
    TransportUnavailable,
    #[error("model request was cancelled")]
    Cancelled,
    #[error("model provider response is invalid")]
    InvalidProviderResponse,
    #[error("model response is incomplete")]
    IncompleteResponse,
    #[error("model stream was interrupted")]
    StreamInterrupted,
    #[error("model request outcome is unknown")]
    RequestOutcomeUnknown,
    #[error("model returned an unexpected tool call")]
    UnexpectedToolCall,
    #[error("model operation panicked")]
    Panicked,
    #[error("model operation failed internally")]
    Internal,
    #[error("model operation failed")]
    Detailed(ModelErrorDetails),
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ModelErrorWire {
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
    Detailed {
        kind: ModelErrorKind,
        delivery: DeliveryState,
        retryable: bool,
        retry_after_ms: Option<u64>,
    },
}

impl ModelError {
    pub fn detailed(
        kind: ModelErrorKind,
        delivery: DeliveryState,
        retryable: bool,
        retry_after: Option<Duration>,
    ) -> Result<Self, Self> {
        let retry_after_ms = retry_after
            .map(|value| value.as_millis().try_into())
            .transpose()
            .map_err(|_| Self::InvalidRequest)?;
        Ok(Self::Detailed(ModelErrorDetails::new(
            kind,
            delivery,
            retryable,
            retry_after_ms,
        )?))
    }

    pub const fn kind(self) -> ModelErrorKind {
        match self {
            Self::InvalidRequest => ModelErrorKind::InvalidRequest,
            Self::Unavailable => ModelErrorKind::Unavailable,
            Self::ProviderUnavailable => ModelErrorKind::ProviderUnavailable,
            Self::AuthMissing => ModelErrorKind::AuthMissing,
            Self::AuthRejected => ModelErrorKind::AuthRejected,
            Self::RateLimited => ModelErrorKind::RateLimited,
            Self::QuotaExceeded => ModelErrorKind::QuotaExceeded,
            Self::ContextOverflow => ModelErrorKind::ContextOverflow,
            Self::Timeout => ModelErrorKind::Timeout,
            Self::TransportUnavailable => ModelErrorKind::TransportUnavailable,
            Self::Cancelled => ModelErrorKind::Cancelled,
            Self::InvalidProviderResponse => ModelErrorKind::InvalidProviderResponse,
            Self::IncompleteResponse => ModelErrorKind::IncompleteResponse,
            Self::StreamInterrupted => ModelErrorKind::StreamInterrupted,
            Self::RequestOutcomeUnknown => ModelErrorKind::RequestOutcomeUnknown,
            Self::UnexpectedToolCall => ModelErrorKind::UnexpectedToolCall,
            Self::Panicked => ModelErrorKind::Panicked,
            Self::Internal => ModelErrorKind::Internal,
            Self::Detailed(details) => details.kind(),
        }
    }

    pub const fn delivery(&self) -> DeliveryState {
        match self {
            Self::Detailed(details) => details.delivery(),
            Self::InvalidRequest
            | Self::Unavailable
            | Self::ProviderUnavailable
            | Self::AuthMissing
            | Self::AuthRejected
            | Self::RateLimited
            | Self::QuotaExceeded
            | Self::ContextOverflow
            | Self::Timeout
            | Self::TransportUnavailable
            | Self::Cancelled => DeliveryState::NotStarted,
            Self::InvalidProviderResponse
            | Self::IncompleteResponse
            | Self::StreamInterrupted
            | Self::UnexpectedToolCall => DeliveryState::Started,
            Self::RequestOutcomeUnknown | Self::Panicked | Self::Internal => DeliveryState::Unknown,
        }
    }

    pub const fn retryable(&self) -> bool {
        match self {
            Self::Detailed(details) => details.retryable(),
            Self::ProviderUnavailable
            | Self::RateLimited
            | Self::Timeout
            | Self::TransportUnavailable => true,
            _ => false,
        }
    }

    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Detailed(details) => details.retry_after(),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ModelError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match ModelErrorWire::deserialize(deserializer)? {
            ModelErrorWire::InvalidRequest => Self::InvalidRequest,
            ModelErrorWire::Unavailable => Self::Unavailable,
            ModelErrorWire::ProviderUnavailable => Self::ProviderUnavailable,
            ModelErrorWire::AuthMissing => Self::AuthMissing,
            ModelErrorWire::AuthRejected => Self::AuthRejected,
            ModelErrorWire::RateLimited => Self::RateLimited,
            ModelErrorWire::QuotaExceeded => Self::QuotaExceeded,
            ModelErrorWire::ContextOverflow => Self::ContextOverflow,
            ModelErrorWire::Timeout => Self::Timeout,
            ModelErrorWire::TransportUnavailable => Self::TransportUnavailable,
            ModelErrorWire::Cancelled => Self::Cancelled,
            ModelErrorWire::InvalidProviderResponse => Self::InvalidProviderResponse,
            ModelErrorWire::IncompleteResponse => Self::IncompleteResponse,
            ModelErrorWire::StreamInterrupted => Self::StreamInterrupted,
            ModelErrorWire::RequestOutcomeUnknown => Self::RequestOutcomeUnknown,
            ModelErrorWire::UnexpectedToolCall => Self::UnexpectedToolCall,
            ModelErrorWire::Panicked => Self::Panicked,
            ModelErrorWire::Internal => Self::Internal,
            ModelErrorWire::Detailed {
                kind,
                delivery,
                retryable,
                retry_after_ms,
            } => Self::Detailed(
                ModelErrorDetails::new(kind, delivery, retryable, retry_after_ms)
                    .map_err(serde::de::Error::custom)?,
            ),
        })
    }
}

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
