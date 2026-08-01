use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;

use crate::wire::lexical::{
    LexicalError, normalize_newlines, validate_opaque_ascii, validate_safe_text,
    validate_stable_symbolic_key,
};
use crate::wire::{Money, ProtocolLimits};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelIdentityError {
    #[error("model identity must be 1..=128 bytes")]
    InvalidLength,
    #[error("model identity violates its stable symbolic key grammar")]
    InvalidGrammar,
}

macro_rules! stable_model_identity {
    ($name:ident, $allow_slash:literal) => {
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Box<str>);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = ModelIdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                validate_stable_symbolic_key(value, 128, $allow_slash).map_err(
                    |error| match error {
                        LexicalError::Empty | LexicalError::TooLong => {
                            ModelIdentityError::InvalidLength
                        }
                        LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                            ModelIdentityError::InvalidGrammar
                        }
                    },
                )?;
                Ok(Self(value.into()))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }
    };
}

stable_model_identity!(ProviderId, false);
stable_model_identity!(ModelId, true);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ProviderOpaqueValueError {
    #[error("provider opaque value must be 1..=256 bytes")]
    InvalidLength,
    #[error("provider opaque value violates the printable ASCII grammar")]
    InvalidGrammar,
}

macro_rules! provider_opaque_value {
    ($name:ident) => {
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Box<str>);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = ProviderOpaqueValueError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                validate_opaque_ascii(value, 256).map_err(|error| match error {
                    LexicalError::Empty | LexicalError::TooLong => {
                        ProviderOpaqueValueError::InvalidLength
                    }
                    LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                        ProviderOpaqueValueError::InvalidGrammar
                    }
                })?;
                Ok(Self(value.into()))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }
    };
}

provider_opaque_value!(ProviderRequestId);
provider_opaque_value!(ProviderResponseId);
provider_opaque_value!(ProviderItemId);
provider_opaque_value!(RedactedProviderCode);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModelSelection {
    provider_id: ProviderId,
    model_id: ModelId,
}

impl ModelSelection {
    pub fn new(provider_id: ProviderId, model_id: ModelId) -> Self {
        Self {
            provider_id,
            model_id,
        }
    }

    pub const fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReasoningPreference {
    Auto,
    Disabled,
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelReasoningSummary {
    ProviderDefault,
    Disabled,
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelServiceClass {
    Standard,
    Priority,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModelResponseSummary {
    provider_id: ProviderId,
    model_id: ModelId,
    reasoning: ModelReasoningSummary,
    service_class: ModelServiceClass,
}

impl ModelResponseSummary {
    #[allow(
        dead_code,
        reason = "constructed by ModelGateway response validation in M6"
    )]
    fn new(
        provider_id: ProviderId,
        model_id: ModelId,
        reasoning: ModelReasoningSummary,
        service_class: ModelServiceClass,
    ) -> Self {
        Self {
            provider_id,
            model_id,
            reasoning,
            service_class,
        }
    }

    pub const fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    pub const fn reasoning(&self) -> ModelReasoningSummary {
        self.reasoning
    }

    pub const fn service_class(&self) -> ModelServiceClass {
        self.service_class
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelValueError {
    #[error("model artifact is empty, unsafe, or exceeds its limit")]
    InvalidArtifact,
    #[error("reasoning content has no portable artifact")]
    EmptyReasoningContent,
    #[error("redacted model error message is invalid")]
    InvalidErrorMessage,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReasoningContent {
    text: Option<Arc<str>>,
    summary: Option<Arc<str>>,
    encrypted: Option<Arc<str>>,
    signature: Option<Arc<str>>,
    provider_item_id: Option<ProviderItemId>,
}

impl ReasoningContent {
    #[allow(
        dead_code,
        reason = "constructed by ProviderAdapter normalization in M14"
    )]
    fn new(
        text: Option<String>,
        summary: Option<String>,
        encrypted: Option<String>,
        signature: Option<String>,
        provider_item_id: Option<ProviderItemId>,
    ) -> Result<Self, ModelValueError> {
        let text = validate_optional_readable_artifact(text, 262_144)?;
        let summary = validate_optional_readable_artifact(summary, 131_072)?;
        let encrypted = validate_optional_opaque_artifact(encrypted, 262_144)?;
        let signature = validate_optional_opaque_artifact(signature, 16_384)?;
        if text.is_none() && summary.is_none() && encrypted.is_none() && signature.is_none() {
            return Err(ModelValueError::EmptyReasoningContent);
        }
        Ok(Self {
            text,
            summary,
            encrypted,
            signature,
            provider_item_id,
        })
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub fn encrypted(&self) -> Option<&str> {
        self.encrypted.as_deref()
    }

    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }

    pub const fn provider_item_id(&self) -> Option<&ProviderItemId> {
        self.provider_item_id.as_ref()
    }
}

impl fmt::Debug for ReasoningContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReasoningContent")
            .field("text_bytes", &self.text.as_ref().map(|value| value.len()))
            .field(
                "summary_bytes",
                &self.summary.as_ref().map(|value| value.len()),
            )
            .field("has_encrypted", &self.encrypted.is_some())
            .field("has_signature", &self.signature.is_some())
            .field("has_provider_item_id", &self.provider_item_id.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelFinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFiltered,
    Refused,
    Unknown,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderResponseMetadata {
    provider_request_id: Option<ProviderRequestId>,
    raw_finish_code: Option<RedactedProviderCode>,
    service_tier: Option<RedactedProviderCode>,
}

impl ProviderResponseMetadata {
    #[allow(
        dead_code,
        reason = "constructed by ProviderAdapter normalization in M14"
    )]
    fn new(
        provider_request_id: Option<ProviderRequestId>,
        raw_finish_code: Option<RedactedProviderCode>,
        service_tier: Option<RedactedProviderCode>,
    ) -> Self {
        Self {
            provider_request_id,
            raw_finish_code,
            service_tier,
        }
    }

    pub const fn provider_request_id(&self) -> Option<&ProviderRequestId> {
        self.provider_request_id.as_ref()
    }

    pub const fn raw_finish_code(&self) -> Option<&RedactedProviderCode> {
        self.raw_finish_code.as_ref()
    }

    pub const fn service_tier(&self) -> Option<&RedactedProviderCode> {
        self.service_tier.as_ref()
    }
}

impl fmt::Debug for ProviderResponseMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderResponseMetadata")
            .field(
                "has_provider_request_id",
                &self.provider_request_id.is_some(),
            )
            .field("has_raw_finish_code", &self.raw_finish_code.is_some())
            .field("has_service_tier", &self.service_tier.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    provider_total_tokens: Option<u64>,
    reported_cost: Option<Money>,
}

impl ModelUsage {
    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the closed provider usage shape"
    )]
    #[allow(
        dead_code,
        reason = "constructed from provider-reported usage in M6/M14"
    )]
    fn new(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        provider_total_tokens: Option<u64>,
        reported_cost: Option<Money>,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens,
            cache_write_tokens,
            provider_total_tokens,
            reported_cost,
        }
    }

    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    pub const fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }

    pub const fn cache_read_tokens(&self) -> Option<u64> {
        self.cache_read_tokens
    }

    pub const fn cache_write_tokens(&self) -> Option<u64> {
        self.cache_write_tokens
    }

    pub const fn provider_total_tokens(&self) -> Option<u64> {
        self.provider_total_tokens
    }

    pub const fn reported_cost(&self) -> Option<&Money> {
        self.reported_cost.as_ref()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RedactedErrorMessage(Arc<str>);

impl RedactedErrorMessage {
    #[allow(
        dead_code,
        reason = "constructed by ProviderAdapter error mapping in M14"
    )]
    fn new(message: impl AsRef<str>) -> Result<Self, ModelValueError> {
        let message = normalize_newlines(message.as_ref());
        validate_safe_text(
            &message,
            ProtocolLimits::v1_0().text.max_diagnostic_message_bytes as usize,
            false,
        )
        .map_err(|_| ModelValueError::InvalidErrorMessage)?;
        Ok(Self(message.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedErrorMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactedErrorMessage")
            .field("bytes", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelCallErrorReason {
    Cancelled,
    ModelUnavailable,
    AuthMissing,
    AuthRejected,
    RateLimited,
    QuotaExceeded,
    ContextOverflow,
    UnsupportedCapability,
    InvalidRequest,
    SafetyBlocked,
    Timeout,
    TransportUnavailable,
    ProviderUnavailable,
    ProviderRejected,
    RequestOutcomeUnknown,
    StreamInterrupted,
    UnexpectedToolCall,
    InvalidStructuredOutput,
    InvalidProviderResponse,
    IncompleteResponse,
}

fn validate_optional_readable_artifact(
    value: Option<String>,
    maximum: usize,
) -> Result<Option<Arc<str>>, ModelValueError> {
    value
        .map(|value| {
            validate_artifact(&value, maximum)?;
            Ok(value.into())
        })
        .transpose()
}

fn validate_optional_opaque_artifact(
    value: Option<String>,
    maximum: usize,
) -> Result<Option<Arc<str>>, ModelValueError> {
    value
        .map(|value| {
            if value.is_empty()
                || value.len() > maximum
                || value
                    .chars()
                    .any(|character| matches!(u32::from(character), 0x00..=0x1f | 0x7f..=0x9f))
            {
                return Err(ModelValueError::InvalidArtifact);
            }
            Ok(value.into())
        })
        .transpose()
}

fn validate_artifact(value: &str, maximum: usize) -> Result<(), ModelValueError> {
    validate_safe_text(value, maximum, false).map_err(|_| ModelValueError::InvalidArtifact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_requires_a_bounded_artifact_and_redacts_debug() {
        assert!(ReasoningContent::new(None, None, None, None, None).is_err());
        assert!(
            ReasoningContent::new(None, None, None, None, Some("item_1".parse().unwrap())).is_err()
        );

        assert!(
            ReasoningContent::new(Some("mutated\r\ntext".to_owned()), None, None, None, None,)
                .is_err()
        );

        let reasoning = ReasoningContent::new(
            Some("SECRET-REASONING\nline".to_owned()),
            Some("brief".to_owned()),
            Some("密文".to_owned()),
            Some("SECRET-SIGNATURE".to_owned()),
            Some("item_1".parse().unwrap()),
        )
        .unwrap();
        assert_eq!(reasoning.text(), Some("SECRET-REASONING\nline"));
        assert_eq!(reasoning.encrypted(), Some("密文"));
        let debug = format!("{reasoning:?}");
        assert!(!debug.contains("SECRET-REASONING"));
        assert!(!debug.contains("密文"));
        assert!(!debug.contains("SECRET-SIGNATURE"));
        assert!(!debug.contains("item_1"));
        assert!(ReasoningContent::new(Some(String::new()), None, None, None, None).is_err());
        assert!(ReasoningContent::new(Some("x".repeat(262_144)), None, None, None, None).is_ok());
        assert!(ReasoningContent::new(Some("x".repeat(262_145)), None, None, None, None).is_err());
        assert!(ReasoningContent::new(None, Some("x".repeat(131_072)), None, None, None).is_ok());
        assert!(ReasoningContent::new(None, Some("x".repeat(131_073)), None, None, None).is_err());
        assert!(ReasoningContent::new(None, None, Some("x".repeat(262_144)), None, None).is_ok());
        assert!(ReasoningContent::new(None, None, Some("x".repeat(262_145)), None, None).is_err());
        assert!(ReasoningContent::new(None, None, None, Some("x".repeat(16_384)), None).is_ok());
        assert!(ReasoningContent::new(None, None, None, Some("x".repeat(16_385)), None).is_err());
        assert!(
            ReasoningContent::new(None, None, Some("bad\nartifact".to_owned()), None, None)
                .is_err()
        );
    }

    #[test]
    fn redacted_errors_are_bounded_and_debug_safe() {
        let message = RedactedErrorMessage::new("provider unavailable\r\nretry").unwrap();
        assert_eq!(message.as_str(), "provider unavailable\nretry");
        assert!(!format!("{message:?}").contains("provider unavailable"));
        assert!(RedactedErrorMessage::new("bad\u{001b}").is_err());
        assert!(RedactedErrorMessage::new("x".repeat(2_048)).is_ok());
        assert!(RedactedErrorMessage::new("x".repeat(2_049)).is_err());
    }

    #[test]
    fn actual_model_metadata_and_usage_are_owner_constructed() {
        let summary = ModelResponseSummary::new(
            "openai".parse().unwrap(),
            "gpt-5".parse().unwrap(),
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
        );
        assert_eq!(summary.provider_id().as_str(), "openai");
        assert_eq!(summary.model_id().as_str(), "gpt-5");

        let metadata = ProviderResponseMetadata::new(
            Some("SECRET-REQUEST-ID".parse().unwrap()),
            Some("stop".parse().unwrap()),
            Some("priority".parse().unwrap()),
        );
        assert_eq!(
            metadata.provider_request_id().unwrap().as_str(),
            "SECRET-REQUEST-ID"
        );
        assert!(!format!("{metadata:?}").contains("SECRET-REQUEST-ID"));

        let cost = Money::new("0.01".parse().unwrap(), "USD".parse().unwrap());
        let usage = ModelUsage::new(
            Some(10),
            Some(3),
            None,
            Some(2),
            Some(1),
            Some(13),
            Some(cost),
        );
        assert_eq!(usage.input_tokens(), Some(10));
        assert_eq!(usage.cache_write_tokens(), Some(1));
        assert_eq!(usage.reported_cost(), Some(&cost));
    }
}
