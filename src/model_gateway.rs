use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::live_conversation::ConversationRevision;
use crate::prompt::AssembledModelContext;
use crate::tools::{ToolCallId, ToolName};
use crate::wire::lexical::{
    LexicalError, validate_opaque_ascii, validate_safe_text, validate_stable_symbolic_key,
};
use crate::wire::{BoundedJsonObject, BoundedJsonSchema, Money, ProtocolLimits};

mod anthropic_messages;
mod openai_responses;
mod provider_installation;
mod provider_transport;

pub(crate) use provider_installation::ProviderSourceBuildError;
pub use provider_installation::{
    CredentialSource, CredentialSourceFuture, ModelProviderConfig, ModelProviderConfigError,
    ModelProviderDescriptor, ModelProviderDescriptorError, ModelReasoningSupport,
    ProviderCredential, ProviderCredentialError, ProviderEndpointPolicy,
};

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

/// The private validated provider wire model name. It is deliberately separate from
/// the stable `ModelId`: the durable identity never changes, while the provider API
/// model name may need to differ (and stays invisible to hosts and to durable
/// provenance). It is not a stable identity: it is validated as non-empty printable
/// opaque ASCII within 256 bytes (no control characters, spaces, quotes, or
/// backslashes) so provider names such as OpenAI fine-tune names containing `:`
/// parse. Debug always redacts the value.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ApiModelName(Box<str>);

impl ApiModelName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ApiModelName {
    type Err = ProviderOpaqueValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_opaque_ascii(value, 256).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => ProviderOpaqueValueError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                ProviderOpaqueValueError::InvalidGrammar
            }
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Debug for ApiModelName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiModelName(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ModelDefinitionVersion(NonZeroU64);

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by the adjacent M7 model source slice"
    )
)]
impl ModelDefinitionVersion {
    pub(crate) const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0.get()
    }
}

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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModelSelection {
    provider_id: ProviderId,
    model_id: ModelId,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ModelDefinitionRef {
    provider_id: ProviderId,
    model_id: ModelId,
    version: ModelDefinitionVersion,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "read by the adjacent M7 model-call consumer")
)]
impl ModelDefinitionRef {
    pub(crate) const fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    pub(crate) const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    pub(crate) const fn version(&self) -> ModelDefinitionVersion {
        self.version
    }
}

impl fmt::Debug for ModelDefinitionRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelDefinitionRef")
            .field("provider_id", &self.provider_id)
            .field("model_id", &self.model_id)
            .field("version", &self.version)
            .finish()
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReasoningCapabilities {
    disabled: bool,
    low: bool,
    medium: bool,
    high: bool,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "validated by the adjacent M7 model source slice")
)]
impl ReasoningCapabilities {
    pub(crate) const fn all() -> Self {
        Self {
            disabled: true,
            low: true,
            medium: true,
            high: true,
        }
    }

    const fn supports(self, preference: ReasoningPreference) -> bool {
        match preference {
            ReasoningPreference::Auto => true,
            ReasoningPreference::Disabled => self.disabled,
            ReasoningPreference::Low => self.low,
            ReasoningPreference::Medium => self.medium,
            ReasoningPreference::High => self.high,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelCapabilities {
    text_input: bool,
    reasoning: ReasoningCapabilities,
    supports_streaming: bool,
    structured_json_schema: bool,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by the adjacent M7 model source slice"
    )
)]
impl ModelCapabilities {
    pub(crate) const fn text_only(
        reasoning: ReasoningCapabilities,
        supports_streaming: bool,
    ) -> Self {
        Self {
            text_input: true,
            reasoning,
            supports_streaming,
            structured_json_schema: false,
        }
    }

    /// Opts a model into the minimal structured JSON-schema output capability.  A plain
    /// text-only model never supports structured output unless it opts in explicitly.
    pub(crate) const fn with_structured_json_schema(self) -> Self {
        Self {
            structured_json_schema: true,
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveModelLimits {
    context_window_tokens: Option<NonZeroU32>,
    max_output_tokens: Option<NonZeroU32>,
    max_schema_bytes: Option<NonZeroU32>,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "completed by the adjacent M7 model source slice")
)]
impl EffectiveModelLimits {
    pub(crate) const fn new(
        context_window_tokens: Option<NonZeroU32>,
        max_output_tokens: Option<NonZeroU32>,
    ) -> Self {
        Self {
            context_window_tokens,
            max_output_tokens,
            max_schema_bytes: None,
        }
    }

    /// Binds an explicit canonical schema byte cap for structured output.  The effective cap is
    /// `min(protocol schema max, model max_schema_bytes)`; absent a model cap the protocol cap
    /// still applies.
    pub(crate) const fn with_max_schema_bytes(self, max_schema_bytes: NonZeroU32) -> Self {
        Self {
            max_schema_bytes: Some(max_schema_bytes),
            ..self
        }
    }

    pub(crate) const fn max_output_tokens(self) -> Option<NonZeroU32> {
        self.max_output_tokens
    }

    pub(crate) const fn context_window_tokens(self) -> Option<NonZeroU32> {
        self.context_window_tokens
    }

    pub(crate) const fn max_schema_bytes(self) -> Option<NonZeroU32> {
        self.max_schema_bytes
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "returned by the adjacent M7 model source slice")
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum TokenEstimateRateError {
    #[error("token estimate algorithm version must be non-zero")]
    InvalidAlgorithmVersion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TokenEstimateRate {
    bytes_per_token: NonZeroU32,
    algorithm_version: u16,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by the adjacent M7 model source slice"
    )
)]
impl TokenEstimateRate {
    pub(crate) const fn new(
        bytes_per_token: NonZeroU32,
        algorithm_version: u16,
    ) -> Result<Self, TokenEstimateRateError> {
        if algorithm_version == 0 {
            return Err(TokenEstimateRateError::InvalidAlgorithmVersion);
        }
        Ok(Self {
            bytes_per_token,
            algorithm_version,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TokenEstimator {
    rate: TokenEstimateRate,
}

impl TokenEstimator {
    pub(crate) fn estimate_utf8_bytes(self, bytes: usize) -> u64 {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let divisor = u64::from(self.rate.bytes_per_token.get());
        bytes.saturating_add(divisor - 1) / divisor
    }

    pub(crate) fn checked_estimate_utf8_bytes(self, bytes: u64) -> Option<u64> {
        let divisor = u64::from(self.rate.bytes_per_token.get());
        bytes.checked_add(divisor - 1).map(|value| value / divisor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelGenerationDefaults {
    max_output_tokens: NonZeroU32,
    reasoning: ModelReasoningSummary,
    service_class: ModelServiceClass,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by the adjacent M7 model source slice"
    )
)]
impl ModelGenerationDefaults {
    pub(crate) const fn new(
        max_output_tokens: NonZeroU32,
        reasoning: ModelReasoningSummary,
        service_class: ModelServiceClass,
    ) -> Self {
        Self {
            max_output_tokens,
            reasoning,
            service_class,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveGenerationPolicy {
    max_output_tokens: NonZeroU32,
    reasoning: ModelReasoningSummary,
    service_class: ModelServiceClass,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "read by the adjacent M7 model-call consumer")
)]
impl EffectiveGenerationPolicy {
    pub(crate) const fn max_output_tokens(self) -> NonZeroU32 {
        self.max_output_tokens
    }

    pub(crate) const fn reasoning(self) -> ModelReasoningSummary {
        self.reasoning
    }

    pub(crate) const fn service_class(self) -> ModelServiceClass {
        self.service_class
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "implemented by the adjacent M7 provider adapter")
)]
pub(crate) type ProviderAttemptFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderAttemptResult, ProviderAttemptError>> + Send + 'a>>;

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "implemented by the adjacent M7 provider adapter")
)]
pub(crate) trait ProviderAdapter: Send + Sync {
    fn execute(
        &self,
        request: ProviderAttemptRequest,
        progress: ModelProgressPublisher,
        cancel: CancellationToken,
    ) -> ProviderAttemptFuture<'_>;
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "materialized by the adjacent M7 model source slice"
    )
)]
#[derive(Clone)]
pub(crate) struct ModelDefinition {
    selection: ModelSelection,
    version: ModelDefinitionVersion,
    api_model_name: ApiModelName,
    identity: Arc<TurnModelIdentity>,
    capabilities: ModelCapabilities,
    limits: EffectiveModelLimits,
    token_estimate_rate: TokenEstimateRate,
    generation: ModelGenerationDefaults,
    adapter: Arc<dyn ProviderAdapter>,
    credential_source: Arc<dyn CredentialSource>,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "materialized by the adjacent M7 model source slice"
    )
)]
impl ModelDefinition {
    #[allow(
        clippy::too_many_arguments,
        reason = "one validated model definition binds exact calling semantics"
    )]
    pub(crate) fn new(
        selection: ModelSelection,
        version: ModelDefinitionVersion,
        api_model_name: ApiModelName,
        capabilities: ModelCapabilities,
        limits: EffectiveModelLimits,
        token_estimate_rate: TokenEstimateRate,
        generation: ModelGenerationDefaults,
        adapter: Arc<dyn ProviderAdapter>,
        credential_source: Arc<dyn CredentialSource>,
    ) -> Result<Self, ModelResolutionError> {
        if !capabilities.text_input
            || limits
                .max_output_tokens
                .is_some_and(|maximum| generation.max_output_tokens > maximum)
            || !reasoning_summary_is_supported(generation.reasoning, capabilities.reasoning)
        {
            return Err(ModelResolutionError::new(
                ModelResolutionErrorKind::InvalidDefinition,
            ));
        }
        Ok(Self {
            selection,
            version,
            api_model_name,
            identity: Arc::new(TurnModelIdentity),
            capabilities,
            limits,
            token_estimate_rate,
            generation,
            adapter,
            credential_source,
        })
    }

    fn reference(&self) -> ModelDefinitionRef {
        ModelDefinitionRef {
            provider_id: self.selection.provider_id.clone(),
            model_id: self.selection.model_id.clone(),
            version: self.version,
        }
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "used by adjacent M7 model definition validation")
)]
fn reasoning_summary_is_supported(
    reasoning: ModelReasoningSummary,
    capabilities: ReasoningCapabilities,
) -> bool {
    match reasoning {
        ModelReasoningSummary::ProviderDefault => true,
        ModelReasoningSummary::Disabled => capabilities.disabled,
        ModelReasoningSummary::Low => capabilities.low,
        ModelReasoningSummary::Medium => capabilities.medium,
        ModelReasoningSummary::High => capabilities.high,
    }
}

#[allow(
    dead_code,
    reason = "the current variants are produced by concrete ModelSourceAdapter implementations"
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ModelSourceError {
    #[error("model definition source is unavailable")]
    Unavailable,
    #[error("model definition source produced an invalid definition")]
    InvalidDefinition,
}

pub(crate) type ModelSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<ModelDefinition>, ModelSourceError>> + Send + 'a>>;

pub(crate) trait ModelSourceAdapter: Send + Sync {
    fn discover(&self) -> ModelSourceFuture<'_>;
}

#[allow(
    dead_code,
    reason = "the closed resolution taxonomy is consumed by the adjacent M7 Turn capture"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelResolutionErrorKind {
    SourceUnavailable,
    CatalogUnavailable,
    ModelUnavailable,
    UnsupportedReasoning,
    InvalidOutputLimit,
    InvalidDefinition,
}

#[derive(Clone, Copy, Eq, Error, PartialEq)]
#[error("model resolution failed")]
pub(crate) struct ModelResolutionError {
    kind: ModelResolutionErrorKind,
}

impl ModelResolutionError {
    const fn new(kind: ModelResolutionErrorKind) -> Self {
        Self { kind }
    }

    #[allow(dead_code, reason = "mapped by the adjacent M7 Turn capture")]
    pub(crate) const fn kind(self) -> ModelResolutionErrorKind {
        self.kind
    }
}

impl fmt::Debug for ModelResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelResolutionError")
            .field("kind", &self.kind)
            .finish()
    }
}

struct ModelGatewayOwner;

struct TurnModelIdentity;

/// Opaque, process-local proof of one exact retained model definition.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "validated by the adjacent M7 ModelCallRequest constructor"
    )
)]
#[derive(Clone)]
pub(crate) struct TurnModelRef {
    owner: Arc<ModelGatewayOwner>,
    identity: Arc<TurnModelIdentity>,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "validated by the adjacent M7 ModelCallRequest constructor"
    )
)]
impl TurnModelRef {
    pub(crate) fn is_exact(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner) && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl fmt::Debug for TurnModelRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TurnModelRef(<process-local>)")
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "resolved by the adjacent M7 Turn capture")
)]
pub(crate) struct ModelCatalogView {
    owner: Arc<ModelGatewayOwner>,
    definitions: BTreeMap<ModelSelection, Arc<ModelDefinition>>,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "reported by the adjacent M7 catalog consumer")
)]
impl ModelCatalogView {
    pub(crate) fn definition_count(&self) -> usize {
        self.definitions.len()
    }
}

impl fmt::Debug for ModelCatalogView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCatalogView")
            .field("definition_count", &self.definitions.len())
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 Turn capture")
)]
pub(crate) struct ResolveTurnModelRequest {
    selection: ModelSelection,
    requested_reasoning: ReasoningPreference,
    requested_max_output_tokens: Option<NonZeroU32>,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 Turn capture")
)]
impl ResolveTurnModelRequest {
    pub(crate) const fn new(
        selection: ModelSelection,
        requested_reasoning: ReasoningPreference,
        requested_max_output_tokens: Option<NonZeroU32>,
    ) -> Self {
        Self {
            selection,
            requested_reasoning,
            requested_max_output_tokens,
        }
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "executed by the adjacent M7 ActiveTurnTask")
)]
#[derive(Clone)]
pub(crate) struct TurnModelSnapshot {
    owner: Arc<ModelGatewayOwner>,
    turn_ref: TurnModelRef,
    definition: ModelDefinitionRef,
    api_model_name: ApiModelName,
    capabilities: ModelCapabilities,
    limits: EffectiveModelLimits,
    token_estimate_rate: TokenEstimateRate,
    generation: EffectiveGenerationPolicy,
    execution: Arc<ModelDefinition>,
}

impl TurnModelSnapshot {
    pub(crate) fn turn_model_ref(&self) -> TurnModelRef {
        self.turn_ref.clone()
    }

    pub(crate) const fn definition(&self) -> &ModelDefinitionRef {
        &self.definition
    }

    /// The private provider wire model name of the exact definition. Exposed only
    /// crate-privately so the child provider adapters can encode the wire `model`
    /// field; durable provenance always uses the stable `ModelSelection` ids.
    pub(crate) const fn api_model_name(&self) -> &ApiModelName {
        &self.api_model_name
    }

    pub(crate) const fn generation(&self) -> EffectiveGenerationPolicy {
        self.generation
    }

    pub(crate) const fn limits(&self) -> EffectiveModelLimits {
        self.limits
    }

    pub(crate) const fn token_estimator(&self) -> TokenEstimator {
        TokenEstimator {
            rate: self.token_estimate_rate,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_fixture(context_window_tokens: Option<NonZeroU32>) -> Arc<Self> {
        Self::test_fixture_with_policy(
            context_window_tokens,
            NonZeroU32::new(8_192),
            NonZeroU32::new(4_096).expect("non-zero fixture output limit"),
            NonZeroU32::new(3).expect("non-zero fixture estimate rate"),
        )
    }

    #[cfg(test)]
    pub(crate) fn test_fixture_with_policy(
        context_window_tokens: Option<NonZeroU32>,
        model_max_output_tokens: Option<NonZeroU32>,
        generation_max_output_tokens: NonZeroU32,
        bytes_per_token: NonZeroU32,
    ) -> Arc<Self> {
        let limits = EffectiveModelLimits::new(context_window_tokens, model_max_output_tokens);
        Self::fixture_with(
            limits,
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            generation_max_output_tokens,
            bytes_per_token,
        )
    }

    /// A structured-output-capable fixture: same shape as the text fixture but explicitly
    /// supporting structured JSON-schema output with an explicit canonical schema byte cap.
    #[cfg(test)]
    pub(crate) fn test_fixture_with_structured(
        context_window_tokens: Option<NonZeroU32>,
        max_schema_bytes: NonZeroU32,
    ) -> Arc<Self> {
        let limits = EffectiveModelLimits::new(context_window_tokens, NonZeroU32::new(8_192))
            .with_max_schema_bytes(max_schema_bytes);
        Self::fixture_with(
            limits,
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true)
                .with_structured_json_schema(),
            NonZeroU32::new(4_096).expect("non-zero fixture output limit"),
            NonZeroU32::new(3).expect("non-zero fixture estimate rate"),
        )
    }

    #[cfg(test)]
    fn fixture_with(
        limits: EffectiveModelLimits,
        capabilities: ModelCapabilities,
        generation_max_output_tokens: NonZeroU32,
        bytes_per_token: NonZeroU32,
    ) -> Arc<Self> {
        let owner = Arc::new(ModelGatewayOwner);
        let identity = Arc::new(TurnModelIdentity);
        let definition = ModelDefinition::new(
            ModelSelection::new("fixture".parse().unwrap(), "fixture".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            "fixture".parse().unwrap(),
            capabilities,
            limits,
            TokenEstimateRate::new(bytes_per_token, 1).unwrap(),
            ModelGenerationDefaults::new(
                generation_max_output_tokens,
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            Arc::new(ScriptedProviderAdapter::new(Vec::new())),
            fixed_credential_source("fixture-credential"),
        )
        .unwrap();
        Arc::new(Self {
            owner: Arc::clone(&owner),
            turn_ref: TurnModelRef { owner, identity },
            definition: definition.reference(),
            api_model_name: definition.api_model_name.clone(),
            capabilities: definition.capabilities,
            limits,
            token_estimate_rate: definition.token_estimate_rate,
            generation: EffectiveGenerationPolicy {
                max_output_tokens: definition.generation.max_output_tokens,
                reasoning: definition.generation.reasoning,
                service_class: definition.generation.service_class,
            },
            execution: Arc::new(definition),
        })
    }
}

impl fmt::Debug for TurnModelSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnModelSnapshot")
            .field("definition", &self.definition)
            .field("capabilities", &self.capabilities)
            .field("limits", &self.limits)
            .field("generation", &self.generation)
            .finish()
    }
}

pub(crate) struct ModelGateway {
    owner: Arc<ModelGatewayOwner>,
    sources: Arc<[Arc<dyn ModelSourceAdapter>]>,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "resolved and invoked by the adjacent M7 Turn slice"
    )
)]
impl ModelGateway {
    pub(crate) fn new(sources: Vec<Arc<dyn ModelSourceAdapter>>) -> Self {
        Self {
            owner: Arc::new(ModelGatewayOwner),
            sources: sources.into(),
        }
    }

    pub(crate) async fn initialize(&self) -> Result<Arc<ModelCatalogView>, ModelResolutionError> {
        self.build_candidate().await
    }

    pub(crate) async fn build_reload_candidate(
        &self,
    ) -> Result<Arc<ModelCatalogView>, ModelResolutionError> {
        self.build_candidate().await
    }

    async fn build_candidate(&self) -> Result<Arc<ModelCatalogView>, ModelResolutionError> {
        let mut definitions = BTreeMap::new();
        for source in &*self.sources {
            for definition in source.discover().await.map_err(|error| match error {
                ModelSourceError::Unavailable => {
                    ModelResolutionError::new(ModelResolutionErrorKind::SourceUnavailable)
                }
                ModelSourceError::InvalidDefinition => {
                    ModelResolutionError::new(ModelResolutionErrorKind::InvalidDefinition)
                }
            })? {
                let selection = definition.selection.clone();
                if definitions
                    .insert(selection, Arc::new(definition))
                    .is_some()
                {
                    return Err(ModelResolutionError::new(
                        ModelResolutionErrorKind::InvalidDefinition,
                    ));
                }
            }
        }
        Ok(Arc::new(ModelCatalogView {
            owner: Arc::clone(&self.owner),
            definitions,
        }))
    }

    pub(crate) fn resolve_for_turn(
        &self,
        catalog: Arc<ModelCatalogView>,
        request: ResolveTurnModelRequest,
    ) -> Result<Arc<TurnModelSnapshot>, ModelResolutionError> {
        if !Arc::ptr_eq(&self.owner, &catalog.owner) {
            return Err(ModelResolutionError::new(
                ModelResolutionErrorKind::CatalogUnavailable,
            ));
        }
        let definition = catalog
            .definitions
            .get(&request.selection)
            .ok_or_else(|| ModelResolutionError::new(ModelResolutionErrorKind::ModelUnavailable))?;
        let reasoning = match request.requested_reasoning {
            ReasoningPreference::Auto => definition.generation.reasoning,
            preference if definition.capabilities.reasoning.supports(preference) => {
                match preference {
                    ReasoningPreference::Auto => unreachable!("Auto is handled above"),
                    ReasoningPreference::Disabled => ModelReasoningSummary::Disabled,
                    ReasoningPreference::Low => ModelReasoningSummary::Low,
                    ReasoningPreference::Medium => ModelReasoningSummary::Medium,
                    ReasoningPreference::High => ModelReasoningSummary::High,
                }
            }
            _ => {
                return Err(ModelResolutionError::new(
                    ModelResolutionErrorKind::UnsupportedReasoning,
                ));
            }
        };
        let max_output_tokens = request
            .requested_max_output_tokens
            .unwrap_or(definition.generation.max_output_tokens);
        if definition
            .limits
            .max_output_tokens
            .is_some_and(|maximum| max_output_tokens > maximum)
        {
            return Err(ModelResolutionError::new(
                ModelResolutionErrorKind::InvalidOutputLimit,
            ));
        }
        Ok(Arc::new(TurnModelSnapshot {
            owner: Arc::clone(&self.owner),
            turn_ref: TurnModelRef {
                owner: Arc::clone(&self.owner),
                identity: Arc::clone(&definition.identity),
            },
            definition: definition.reference(),
            api_model_name: definition.api_model_name.clone(),
            capabilities: definition.capabilities,
            limits: definition.limits,
            token_estimate_rate: definition.token_estimate_rate,
            generation: EffectiveGenerationPolicy {
                max_output_tokens,
                reasoning,
                service_class: definition.generation.service_class,
            },
            execution: Arc::clone(definition),
        }))
    }

    pub(crate) async fn generate_model_turn(
        &self,
        request: Arc<ModelCallRequest>,
        progress: ModelProgressPublisher,
        cancel: CancellationToken,
    ) -> Result<ModelCallResult, ModelCallError> {
        if !Arc::ptr_eq(&self.owner, &request.model.owner) {
            return Err(ModelCallError::new(ModelCallErrorReason::ModelUnavailable));
        }
        if cancel.is_cancelled() {
            return Err(ModelCallError::new(ModelCallErrorReason::Cancelled));
        }

        // The dynamic credential source resolves on every attempt, cancellation-aware,
        // strictly before any provider adapter executes. Cancellation during or after
        // resolution but before execute is Cancelled/NotSent; a resolved `None` is a
        // missing credential typed AuthMissing/NotSent with the adapter never invoked.
        let credential = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(ModelCallError::new(ModelCallErrorReason::Cancelled));
            }
            credential = request.model.execution.credential_source.resolve() => credential,
        };
        if cancel.is_cancelled() {
            return Err(ModelCallError::new(ModelCallErrorReason::Cancelled));
        }
        let Some(credential) = credential else {
            return Err(ModelCallError::new(ModelCallErrorReason::AuthMissing));
        };

        let attempt = ProviderAttemptRequest {
            effective_max_output_tokens: request.effective_max_output_tokens(),
            call: Arc::clone(&request),
            credential,
        };
        let result = request
            .model
            .execution
            .adapter
            .execute(attempt, progress, cancel)
            .await
            .map_err(ModelCallError::from_provider)?;

        finalize_provider_result(&request, result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelCallPurpose {
    AgentRun,
    CompactionSummary,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 AgentRun assembly")
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OutputContract {
    NoToolCalls,
    Structured(StructuredOutputContract),
}

/// Minimal typed failure taxonomy for one structured output contract.  Variants carry no
/// payload, so a redacted error never echoes the name or the schema.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum StructuredOutputContractError {
    #[error("structured output contract name is invalid")]
    InvalidName,
    #[error("model does not support structured output")]
    UnsupportedModel,
    #[error("structured output schema exceeds the model schema byte cap")]
    SchemaTooLarge,
    #[error("structured output schema is outside the supported v1 subset")]
    UnsupportedSchema,
}

/// One structured output contract bound to the exact `TurnModelRef` it was validated against.
/// The contract is the only fact source for a structured model call: it carries the validated
/// schema v1 subset (compiled in memory from the bounded `BoundedJsonSchema`) and re-verifies
/// capability and byte cap against the current exact model at request construction.
#[derive(Clone)]
pub(crate) struct StructuredOutputContract {
    turn_ref: TurnModelRef,
    name: Option<Box<str>>,
    schema: BoundedJsonSchema,
    schema_value: serde_json::Value,
}

const STRUCTURED_CONTRACT_NAME_MAX_BYTES: usize = 64;
const STRUCTURED_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 Turn slice")
)]
impl StructuredOutputContract {
    pub(crate) fn new(
        model: &TurnModelSnapshot,
        name: Option<&str>,
        schema: BoundedJsonSchema,
    ) -> Result<Self, StructuredOutputContractError> {
        if let Some(name) = name {
            validate_stable_symbolic_key(name, STRUCTURED_CONTRACT_NAME_MAX_BYTES, false)
                .map_err(|_| StructuredOutputContractError::InvalidName)?;
        }
        if !model.capabilities.structured_json_schema {
            return Err(StructuredOutputContractError::UnsupportedModel);
        }
        if !schema_within_model_cap(
            model.limits.max_schema_bytes(),
            schema.canonical_bytes().len(),
        ) {
            return Err(StructuredOutputContractError::SchemaTooLarge);
        }
        let schema_value = parse_structured_schema(&schema)?;
        Ok(Self {
            turn_ref: model.turn_model_ref(),
            name: name.map(Into::into),
            schema,
            schema_value,
        })
    }

    pub(crate) const fn name(&self) -> Option<&str> {
        match &self.name {
            Some(name) => Some(name),
            None => None,
        }
    }

    pub(crate) const fn schema(&self) -> &BoundedJsonSchema {
        &self.schema
    }

    /// Parses one bounded terminal object exactly once and validates it against the compiled
    /// schema.  The caller (never the ActiveTurnTask) is responsible for this single parse.
    pub(crate) fn validate_instance(&self, object: &BoundedJsonObject) -> bool {
        let Ok(instance) = serde_json::from_slice::<serde_json::Value>(object.canonical_bytes())
        else {
            return false;
        };
        validate_structured_instance(&self.schema_value, &instance)
    }

    /// Fail-closed re-verification against the current exact model: the contract must be bound
    /// to this exact model, the model must support structured output, and the canonical schema
    /// bytes must still fit `min(protocol schema max, model max_schema_bytes)`.  A contract
    /// assembled for one model can never be reused against another.
    fn is_supported_by(&self, model: &TurnModelSnapshot) -> bool {
        self.turn_ref.is_exact(&model.turn_model_ref())
            && model.capabilities.structured_json_schema
            && schema_within_model_cap(
                model.limits.max_schema_bytes(),
                self.schema.canonical_bytes().len(),
            )
    }
}

impl PartialEq for StructuredOutputContract {
    fn eq(&self, other: &Self) -> bool {
        self.turn_ref.is_exact(&other.turn_ref)
            && self.name == other.name
            && self.schema == other.schema
    }
}

impl Eq for StructuredOutputContract {}

impl fmt::Debug for StructuredOutputContract {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StructuredOutputContract")
            .field("has_name", &self.name.is_some())
            .field("schema_bytes", &self.schema.canonical_bytes().len())
            .finish()
    }
}

fn schema_within_model_cap(
    model_max_schema_bytes: Option<NonZeroU32>,
    schema_bytes: usize,
) -> bool {
    let protocol_max = usize::try_from(
        ProtocolLimits::v1_0()
            .embedded_json
            .schema
            .max_encoded_bytes,
    )
    .unwrap_or(usize::MAX);
    let maximum = model_max_schema_bytes.map_or(protocol_max, |limit| {
        usize::try_from(limit.get())
            .unwrap_or(usize::MAX)
            .min(protocol_max)
    });
    schema_bytes <= maximum
}

/// Parses the bounded canonical schema and validates the supported v1 subset in memory.  Both
/// schema and instance are already bounded, so this recursion never performs I/O.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "compiled by the structured contract constructor")
)]
fn parse_structured_schema(
    schema: &BoundedJsonSchema,
) -> Result<serde_json::Value, StructuredOutputContractError> {
    let value = serde_json::from_slice::<serde_json::Value>(schema.canonical_bytes())
        .map_err(|_| StructuredOutputContractError::UnsupportedSchema)?;
    validate_structured_schema_node(&value, true)
        .map_err(|_| StructuredOutputContractError::UnsupportedSchema)?;
    Ok(value)
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "compiled by the structured contract constructor")
)]
fn validate_structured_schema_node(
    node: &serde_json::Value,
    is_root: bool,
) -> Result<(), StructuredOutputContractError> {
    let object = node
        .as_object()
        .ok_or(StructuredOutputContractError::UnsupportedSchema)?;
    if is_root && object.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return Err(StructuredOutputContractError::UnsupportedSchema);
    }
    for (key, value) in object {
        match key.as_str() {
            "$schema" => {
                if !is_root || value.as_str() != Some(STRUCTURED_SCHEMA_DIALECT) {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                }
            }
            "type" => {
                let Some(type_name) = value.as_str() else {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                };
                if !matches!(
                    type_name,
                    "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
                ) {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                }
            }
            "description" => {
                if !value.is_string() {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                }
            }
            "properties" => {
                let Some(properties) = value.as_object() else {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                };
                for (_, child) in properties {
                    validate_structured_schema_node(child, false)?;
                }
            }
            "required" => {
                let Some(items) = value.as_array() else {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                };
                let mut seen = Vec::new();
                for item in items {
                    let Some(name) = item.as_str() else {
                        return Err(StructuredOutputContractError::UnsupportedSchema);
                    };
                    if seen.contains(&name) {
                        return Err(StructuredOutputContractError::UnsupportedSchema);
                    }
                    seen.push(name);
                }
            }
            "additionalProperties" => {
                if !value.is_boolean() {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                }
            }
            "items" => validate_structured_schema_node(value, false)?,
            "enum" => {
                let Some(items) = value.as_array() else {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                };
                if items.is_empty() {
                    return Err(StructuredOutputContractError::UnsupportedSchema);
                }
                for (index, item) in items.iter().enumerate() {
                    if items[..index].contains(item) {
                        return Err(StructuredOutputContractError::UnsupportedSchema);
                    }
                }
            }
            "const" => {}
            _ => return Err(StructuredOutputContractError::UnsupportedSchema),
        }
    }
    Ok(())
}

/// Validates one parsed bounded instance against the compiled schema node.  Both sides are
/// canonical JSON, so enum/const equality is exact.
fn validate_structured_instance(schema: &serde_json::Value, instance: &serde_json::Value) -> bool {
    let Some(object) = schema.as_object() else {
        return true;
    };
    if let Some(type_name) = object.get("type").and_then(serde_json::Value::as_str) {
        if !structured_instance_matches_type(type_name, instance) {
            return false;
        }
    }
    if let Some(enum_values) = object.get("enum").and_then(serde_json::Value::as_array) {
        if !enum_values.iter().any(|candidate| candidate == instance) {
            return false;
        }
    }
    if let Some(const_value) = object.get("const") {
        if const_value != instance {
            return false;
        }
    }
    if let Some(properties) = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        if let Some(instance_object) = instance.as_object() {
            for (name, child_schema) in properties {
                if let Some(child) = instance_object.get(name) {
                    if !validate_structured_instance(child_schema, child) {
                        return false;
                    }
                }
            }
        }
    }
    if let Some(required) = object.get("required").and_then(serde_json::Value::as_array) {
        if let Some(instance_object) = instance.as_object() {
            for name in required {
                let name = name
                    .as_str()
                    .expect("subset validation guarantees string required names");
                if !instance_object.contains_key(name) {
                    return false;
                }
            }
        }
    }
    if object
        .get("additionalProperties")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        if let Some(instance_object) = instance.as_object() {
            let declared = object
                .get("properties")
                .and_then(serde_json::Value::as_object);
            for name in instance_object.keys() {
                if declared.is_none_or(|properties| !properties.contains_key(name)) {
                    return false;
                }
            }
        }
    }
    if let Some(items) = object.get("items") {
        if let Some(instance_array) = instance.as_array() {
            for item in instance_array {
                if !validate_structured_instance(items, item) {
                    return false;
                }
            }
        }
    }
    true
}

fn structured_instance_matches_type(type_name: &str, instance: &serde_json::Value) -> bool {
    match type_name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => instance
            .as_number()
            .map(serde_json::Number::as_str)
            .is_some_and(is_canonical_integer_literal),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => false,
    }
}

/// An integer is judged from the canonical JSON number's decimal scale. No float coercion is
/// applied, including when canonical form retains scientific notation for a large magnitude.
fn is_canonical_integer_literal(literal: &str) -> bool {
    let unsigned = literal.strip_prefix('-').unwrap_or(literal);
    let (coefficient, exponent) = match unsigned.split_once('e') {
        Some((coefficient, exponent)) => {
            let Ok(exponent) = exponent.parse::<i32>() else {
                return false;
            };
            (coefficient, exponent)
        }
        None => (unsigned, 0),
    };
    let fractional_digits = coefficient
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let Ok(fractional_digits) = i32::try_from(fractional_digits) else {
        return false;
    };
    exponent >= fractional_digits
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "published by the adjacent M7 provider adapter")
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ModelContentDelta {
    Text(Arc<str>),
}

impl fmt::Debug for ModelContentDelta {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::Text(value) = self;
        formatter
            .debug_struct("ModelContentDelta")
            .field("kind", &"text")
            .field("bytes", &value.len())
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "published by the adjacent M7 provider adapter")
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ModelProgressEvent {
    ContentDelta {
        content_index: u32,
        delta: ModelContentDelta,
    },
}

impl fmt::Debug for ModelProgressEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContentDelta {
                content_index,
                delta,
            } => formatter
                .debug_struct("ModelProgressEvent::ContentDelta")
                .field("content_index", content_index)
                .field("delta", delta)
                .finish(),
        }
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 ActiveTurnTask")
)]
#[derive(Clone)]
pub(crate) struct ModelProgressPublisher {
    publish: Arc<dyn Fn(ModelProgressEvent) + Send + Sync>,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 ActiveTurnTask")
)]
impl ModelProgressPublisher {
    pub(crate) fn new(publish: impl Fn(ModelProgressEvent) + Send + Sync + 'static) -> Self {
        Self {
            publish: Arc::new(publish),
        }
    }

    pub(crate) fn discard() -> Self {
        Self::new(|_| {})
    }

    fn publish(&self, event: ModelProgressEvent) {
        (self.publish)(event);
    }
}

impl fmt::Debug for ModelProgressPublisher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelProgressPublisher { .. }")
    }
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "returned by the adjacent M7 ModelCallRequest constructor"
    )
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelRequestValidationErrorKind {
    AssemblyMismatch,
    InvalidOutputLimit,
    UnsupportedInput,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "returned by the adjacent M7 ModelCallRequest constructor"
    )
)]
#[derive(Clone, Copy, Eq, Error, PartialEq)]
#[error("model request validation failed")]
pub(crate) struct ModelRequestValidationError {
    kind: ModelRequestValidationErrorKind,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "mapped by the adjacent M7 ActiveTurnTask")
)]
impl ModelRequestValidationError {
    const fn new(kind: ModelRequestValidationErrorKind) -> Self {
        Self { kind }
    }

    pub(crate) const fn kind(self) -> ModelRequestValidationErrorKind {
        self.kind
    }
}

impl fmt::Debug for ModelRequestValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRequestValidationError")
            .field("kind", &self.kind)
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 ActiveTurnTask")
)]
pub(crate) struct ModelCallRequest {
    model: Arc<TurnModelSnapshot>,
    purpose: ModelCallPurpose,
    input: Arc<AssembledModelContext>,
    source_revision: ConversationRevision,
    max_output_tokens: Option<NonZeroU32>,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed and read by the adjacent M7 ActiveTurnTask"
    )
)]
impl ModelCallRequest {
    pub(crate) fn new(
        model: Arc<TurnModelSnapshot>,
        purpose: ModelCallPurpose,
        input: Arc<AssembledModelContext>,
        source_revision: ConversationRevision,
        max_output_tokens: Option<NonZeroU32>,
    ) -> Result<Self, ModelRequestValidationError> {
        let proof = input.assembly_proof();
        if proof.purpose() != purpose
            || !proof.turn_model().is_exact(&model.turn_ref)
            || proof.source_revision() != source_revision
            || proof.output_contract() != input.output_contract()
        {
            return Err(ModelRequestValidationError::new(
                ModelRequestValidationErrorKind::AssemblyMismatch,
            ));
        }
        match purpose {
            ModelCallPurpose::AgentRun if proof.compaction_summary_budget().is_some() => {
                return Err(ModelRequestValidationError::new(
                    ModelRequestValidationErrorKind::AssemblyMismatch,
                ));
            }
            ModelCallPurpose::CompactionSummary => {
                let Some(budget) = proof.compaction_summary_budget() else {
                    return Err(ModelRequestValidationError::new(
                        ModelRequestValidationErrorKind::AssemblyMismatch,
                    ));
                };
                if max_output_tokens != Some(budget.max_output_tokens())
                    || input.output_contract() != Some(&OutputContract::NoToolCalls)
                {
                    return Err(ModelRequestValidationError::new(
                        ModelRequestValidationErrorKind::AssemblyMismatch,
                    ));
                }
            }
            ModelCallPurpose::AgentRun => {}
        }
        if input.output_contract().is_some() && !input.tools_empty() {
            return Err(ModelRequestValidationError::new(
                ModelRequestValidationErrorKind::UnsupportedInput,
            ));
        }
        if let Some(OutputContract::Structured(contract)) = input.output_contract() {
            if !contract.is_supported_by(&model) {
                return Err(ModelRequestValidationError::new(
                    ModelRequestValidationErrorKind::AssemblyMismatch,
                ));
            }
        }
        if max_output_tokens.is_some_and(|requested| {
            model
                .limits
                .max_output_tokens()
                .is_some_and(|maximum| requested > maximum)
        }) {
            return Err(ModelRequestValidationError::new(
                ModelRequestValidationErrorKind::InvalidOutputLimit,
            ));
        }
        Ok(Self {
            model,
            purpose,
            input,
            source_revision,
            max_output_tokens,
        })
    }

    pub(crate) const fn purpose(&self) -> ModelCallPurpose {
        self.purpose
    }

    pub(crate) const fn input(&self) -> &Arc<AssembledModelContext> {
        &self.input
    }

    pub(crate) const fn source_revision(&self) -> ConversationRevision {
        self.source_revision
    }

    pub(crate) fn effective_max_output_tokens(&self) -> NonZeroU32 {
        self.max_output_tokens
            .unwrap_or(self.model.generation.max_output_tokens())
    }
}

impl fmt::Debug for ModelCallRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCallRequest")
            .field("model", &self.model)
            .field("purpose", &self.purpose)
            .field("input", &self.input)
            .field("source_revision", &self.source_revision)
            .field(
                "effective_max_output_tokens",
                &self.effective_max_output_tokens(),
            )
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by the adjacent M7 provider adapter")
)]
pub(crate) struct ProviderAttemptRequest {
    call: Arc<ModelCallRequest>,
    effective_max_output_tokens: NonZeroU32,
    credential: ProviderCredential,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by the adjacent M7 provider adapter")
)]
impl ProviderAttemptRequest {
    const fn call(&self) -> &Arc<ModelCallRequest> {
        &self.call
    }

    /// The gateway-resolved per-attempt credential; the direct adapters inject it
    /// into the bearer/x-api-key headers. Never printed by Debug.
    const fn credential(&self) -> &ProviderCredential {
        &self.credential
    }
}

impl fmt::Debug for ProviderAttemptRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAttemptRequest")
            .field("call", &self.call)
            .field(
                "effective_max_output_tokens",
                &self.effective_max_output_tokens,
            )
            .field("credential", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for ModelGateway {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelGateway")
            .field("source_count", &self.sources.len())
            .finish()
    }
}

/// A `#[cfg(test)]` credential source that resolves one fixed credential forever.
/// Used by every scripted/fixture definition so attempts through the gateway always
/// resolve `Some`; contract suites that assert exact auth headers build explicit
/// sources via `fixed_credential_source`.
#[cfg(test)]
pub(crate) fn fixed_credential_source(credential: &str) -> Arc<dyn CredentialSource> {
    struct FixedCredentialSource(ProviderCredential);

    impl CredentialSource for FixedCredentialSource {
        fn resolve(&self) -> CredentialSourceFuture<'_> {
            let credential = self.0.clone();
            Box::pin(async move { Some(credential) })
        }
    }

    Arc::new(FixedCredentialSource(
        credential
            .parse()
            .expect("test credential must be valid opaque ASCII"),
    ))
}

#[cfg(test)]
pub(crate) struct ScriptedModelFixture {
    gateway: Arc<ModelGateway>,
    catalog: Arc<ModelCatalogView>,
    adapter: Arc<ScriptedProviderAdapter>,
}

#[cfg(test)]
impl ScriptedModelFixture {
    pub(crate) fn new(responses: Vec<&str>) -> Self {
        Self::with_context_window_tokens(responses, 128_000)
    }

    pub(crate) fn with_context_window_tokens(
        responses: Vec<&str>,
        context_window_tokens: u32,
    ) -> Self {
        let scripts = responses
            .into_iter()
            .map(|text| {
                ScriptedProviderScript::success(
                    Vec::new(),
                    ProviderAttemptResult {
                        response_id: None,
                        content: Arc::from([ProviderAttemptContent::Text(Arc::from(text))]),
                        finish_reason: ModelFinishReason::Stop,
                        usage: None,
                        metadata: ProviderResponseMetadata::new(None, None, None),
                    },
                )
            })
            .collect();
        Self::from_scripts(scripts, context_window_tokens)
    }

    pub(crate) fn with_tool_round(
        tool_call_id: &str,
        tool_name: &str,
        arguments: &str,
        final_text: &str,
    ) -> Self {
        Self::with_tool_round_calls(&[(tool_call_id, tool_name, arguments)], final_text)
    }

    /// One Tool round with multiple Tool calls in the first response (call_index follows the
    /// slice order), then one final text response.
    pub(crate) fn with_tool_round_calls(calls: &[(&str, &str, &str)], final_text: &str) -> Self {
        let scripts = vec![
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from(
                        calls
                            .iter()
                            .map(|(tool_call_id, name, arguments)| {
                                ProviderAttemptContent::ToolCall {
                                    tool_call_id: tool_call_id.parse().unwrap(),
                                    name: name.parse().unwrap(),
                                    arguments: arguments.parse().unwrap(),
                                }
                            })
                            .collect::<Vec<_>>(),
                    ),
                    finish_reason: ModelFinishReason::ToolCalls,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::Text(Arc::from(final_text))]),
                    finish_reason: ModelFinishReason::Stop,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
        ];
        Self::from_scripts(scripts, 128_000)
    }

    /// Two sequential Tool rounds, each `(tool_call_id, tool_name, arguments, final_text)`, as
    /// exactly four scripts: the first ToolCall response, the first Turn's final text, the
    /// second ToolCall response, and the second Turn's final text.  The two rounds carry
    /// distinct tool_call_ids so their ToolResults are distinguishable in the exact model
    /// request sequence across two public Submits.
    pub(crate) fn with_two_tool_rounds(
        first: (&str, &str, &str, &str),
        second: (&str, &str, &str, &str),
    ) -> Self {
        let scripts = vec![
            Self::scripted_success(
                ProviderAttemptContent::ToolCall {
                    tool_call_id: first.0.parse().unwrap(),
                    name: first.1.parse().unwrap(),
                    arguments: first.2.parse().unwrap(),
                },
                ModelFinishReason::ToolCalls,
            ),
            Self::scripted_success(
                ProviderAttemptContent::Text(Arc::from(first.3)),
                ModelFinishReason::Stop,
            ),
            Self::scripted_success(
                ProviderAttemptContent::ToolCall {
                    tool_call_id: second.0.parse().unwrap(),
                    name: second.1.parse().unwrap(),
                    arguments: second.2.parse().unwrap(),
                },
                ModelFinishReason::ToolCalls,
            ),
            Self::scripted_success(
                ProviderAttemptContent::Text(Arc::from(second.3)),
                ModelFinishReason::Stop,
            ),
        ];
        Self::from_scripts(scripts, 128_000)
    }

    pub(crate) fn with_failure_reasons_then_responses(
        failures: Vec<ModelCallErrorReason>,
        responses: Vec<&str>,
    ) -> Self {
        Self::with_failure_reasons_then_responses_and_context_window(failures, responses, 128_000)
    }

    pub(crate) fn with_failure_reasons_then_responses_and_context_window(
        failures: Vec<ModelCallErrorReason>,
        responses: Vec<&str>,
        context_window_tokens: u32,
    ) -> Self {
        let scripts = failures
            .into_iter()
            .map(|reason| ScriptedProviderScript::failure(ProviderAttemptError::new(reason)))
            .chain(responses.into_iter().map(|text| {
                ScriptedProviderScript::success(
                    Vec::new(),
                    ProviderAttemptResult {
                        response_id: None,
                        content: Arc::from([ProviderAttemptContent::Text(Arc::from(text))]),
                        finish_reason: ModelFinishReason::Stop,
                        usage: None,
                        metadata: ProviderResponseMetadata::new(None, None, None),
                    },
                )
            }))
            .collect();
        Self::from_scripts(scripts, context_window_tokens)
    }

    pub(crate) fn with_responses_then_failure_reasons(
        responses: Vec<&str>,
        failures: Vec<ModelCallErrorReason>,
    ) -> Self {
        let scripts =
            responses
                .into_iter()
                .map(|text| {
                    ScriptedProviderScript::success(
                        Vec::new(),
                        ProviderAttemptResult {
                            response_id: None,
                            content: Arc::from([ProviderAttemptContent::Text(Arc::from(text))]),
                            finish_reason: ModelFinishReason::Stop,
                            usage: None,
                            metadata: ProviderResponseMetadata::new(None, None, None),
                        },
                    )
                })
                .chain(failures.into_iter().map(|reason| {
                    ScriptedProviderScript::failure(ProviderAttemptError::new(reason))
                }))
                .collect();
        Self::from_scripts(scripts, 4_300)
    }

    fn from_scripts(scripts: Vec<ScriptedProviderScript>, context_window_tokens: u32) -> Self {
        let adapter = Arc::new(ScriptedProviderAdapter::new(scripts));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let definition = ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            "gpt-5".parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(
                NonZeroU32::new(context_window_tokens),
                NonZeroU32::new(8_192),
            ),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(4_096).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            provider,
            fixed_credential_source("scripted-credential"),
        )
        .unwrap();
        let gateway = Arc::new(ModelGateway::new(Vec::new()));
        let mut definitions = BTreeMap::new();
        definitions.insert(definition.selection.clone(), Arc::new(definition));
        let catalog = Arc::new(ModelCatalogView {
            owner: Arc::clone(&gateway.owner),
            definitions,
        });
        Self {
            gateway,
            catalog,
            adapter,
        }
    }

    /// One private scripted-success helper: a single ToolCall result or a single text result
    /// with its exact finish reason.
    fn scripted_success(
        content: ProviderAttemptContent,
        finish_reason: ModelFinishReason,
    ) -> ScriptedProviderScript {
        ScriptedProviderScript::success(
            Vec::new(),
            ProviderAttemptResult {
                response_id: None,
                content: Arc::from([content]),
                finish_reason,
                usage: None,
                metadata: ProviderResponseMetadata::new(None, None, None),
            },
        )
    }

    pub(crate) const fn gateway(&self) -> &Arc<ModelGateway> {
        &self.gateway
    }

    pub(crate) const fn catalog(&self) -> &Arc<ModelCatalogView> {
        &self.catalog
    }

    pub(crate) fn request_count(&self) -> usize {
        self.adapter.requests().len()
    }

    pub(crate) fn requests(&self) -> Vec<Arc<ModelCallRequest>> {
        self.adapter.requests()
    }
}

#[cfg(test)]
struct ScriptedProviderAdapter {
    scripts: std::sync::Mutex<std::collections::VecDeque<ScriptedProviderScript>>,
    requests: std::sync::Mutex<Vec<Arc<ModelCallRequest>>>,
}

#[cfg(test)]
impl ScriptedProviderAdapter {
    fn new(scripts: Vec<ScriptedProviderScript>) -> Self {
        Self {
            scripts: std::sync::Mutex::new(scripts.into()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Arc<ModelCallRequest>> {
        self.requests.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl ProviderAdapter for ScriptedProviderAdapter {
    fn execute(
        &self,
        request: ProviderAttemptRequest,
        progress: ModelProgressPublisher,
        cancel: CancellationToken,
    ) -> ProviderAttemptFuture<'_> {
        self.requests
            .lock()
            .unwrap()
            .push(Arc::clone(request.call()));
        let script = self.scripts.lock().unwrap().pop_front();
        Box::pin(async move {
            let Some(script) = script else {
                return Err(ProviderAttemptError::new(
                    ModelCallErrorReason::InvalidRequest,
                ));
            };
            for event in script.progress {
                if cancel.is_cancelled() {
                    return Err(ProviderAttemptError::new(ModelCallErrorReason::Cancelled));
                }
                progress.publish(event);
            }
            match script.terminal {
                ScriptedProviderTerminal::Success(result) if !cancel.is_cancelled() => Ok(result),
                ScriptedProviderTerminal::Failure(error) if !cancel.is_cancelled() => Err(error),
                ScriptedProviderTerminal::SuccessThenCancel(result) => {
                    cancel.cancel();
                    Ok(result)
                }
                ScriptedProviderTerminal::WaitForCancellation => {
                    cancel.cancelled().await;
                    Err(ProviderAttemptError::new(ModelCallErrorReason::Cancelled))
                }
                ScriptedProviderTerminal::Success(_) | ScriptedProviderTerminal::Failure(_) => {
                    Err(ProviderAttemptError::new(ModelCallErrorReason::Cancelled))
                }
            }
        })
    }
}

#[cfg(test)]
struct ScriptedProviderScript {
    progress: Vec<ModelProgressEvent>,
    terminal: ScriptedProviderTerminal,
}

#[cfg(test)]
impl ScriptedProviderScript {
    fn success(progress: Vec<ModelProgressEvent>, result: ProviderAttemptResult) -> Self {
        Self {
            progress,
            terminal: ScriptedProviderTerminal::Success(result),
        }
    }

    fn wait_for_cancellation() -> Self {
        Self {
            progress: Vec::new(),
            terminal: ScriptedProviderTerminal::WaitForCancellation,
        }
    }

    fn success_then_cancel(result: ProviderAttemptResult) -> Self {
        Self {
            progress: Vec::new(),
            terminal: ScriptedProviderTerminal::SuccessThenCancel(result),
        }
    }

    fn failure(error: ProviderAttemptError) -> Self {
        Self {
            progress: Vec::new(),
            terminal: ScriptedProviderTerminal::Failure(error),
        }
    }
}

#[cfg(test)]
enum ScriptedProviderTerminal {
    Success(ProviderAttemptResult),
    Failure(ProviderAttemptError),
    SuccessThenCancel(ProviderAttemptResult),
    WaitForCancellation,
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

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        provider_id: ProviderId,
        model_id: ModelId,
        reasoning: ModelReasoningSummary,
        service_class: ModelServiceClass,
    ) -> Self {
        Self::new(provider_id, model_id, reasoning, service_class)
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

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        text: Option<String>,
        summary: Option<String>,
        encrypted: Option<String>,
        signature: Option<String>,
        provider_item_id: Option<ProviderItemId>,
    ) -> Result<Self, ModelValueError> {
        Self::new(text, summary, encrypted, signature, provider_item_id)
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

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        provider_request_id: Option<ProviderRequestId>,
        raw_finish_code: Option<RedactedProviderCode>,
        service_tier: Option<RedactedProviderCode>,
    ) -> Self {
        Self::new(provider_request_id, raw_finish_code, service_tier)
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

    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the closed provider usage shape"
    )]
    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        provider_total_tokens: Option<u64>,
        reported_cost: Option<Money>,
    ) -> Self {
        Self::new(
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens,
            cache_write_tokens,
            provider_total_tokens,
            reported_cost,
        )
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

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 provider adapter")
)]
#[derive(Clone)]
enum ProviderAttemptContent {
    Reasoning(ReasoningContent),
    Text(Arc<str>),
    ToolCall {
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}

impl fmt::Debug for ProviderAttemptContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Reasoning(_) => "reasoning",
            Self::Text(_) => "text",
            Self::ToolCall { .. } => "tool_call",
        };
        formatter
            .debug_struct("ProviderAttemptContent")
            .field("kind", &kind)
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 provider adapter")
)]
pub(crate) struct ProviderAttemptResult {
    response_id: Option<ProviderResponseId>,
    content: Arc<[ProviderAttemptContent]>,
    finish_reason: ModelFinishReason,
    usage: Option<ModelUsage>,
    metadata: ProviderResponseMetadata,
}

impl fmt::Debug for ProviderAttemptResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAttemptResult")
            .field("has_response_id", &self.response_id.is_some())
            .field("content_blocks", &self.content.len())
            .field("finish_reason", &self.finish_reason)
            .field("has_usage", &self.usage.is_some())
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 provider adapter")
)]
pub(crate) struct ProviderAttemptError {
    reason: ModelCallErrorReason,
    retry_after: Option<Duration>,
    delivery: ProviderRequestDeliveryState,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 provider adapter")
)]
impl ProviderAttemptError {
    const fn new(reason: ModelCallErrorReason) -> Self {
        Self {
            reason,
            retry_after: None,
            delivery: ProviderRequestDeliveryState::NotSent,
        }
    }
}

impl fmt::Debug for ProviderAttemptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAttemptError")
            .field("reason", &self.reason)
            .field("retry_after", &self.retry_after)
            .field("delivery", &self.delivery)
            .finish()
    }
}

#[allow(
    dead_code,
    reason = "the closed delivery taxonomy is populated by concrete provider adapters"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRequestDeliveryState {
    NotSent,
    RejectedBeforeExecution,
    AcceptedNoOutput,
    OutputStarted,
    Unknown,
}

#[allow(
    dead_code,
    reason = "the spec-closed error taxonomy is populated by concrete provider adapters"
)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ModelCallErrorReason {
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

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "returned to the adjacent M7 ActiveTurnTask")
)]
#[derive(Clone, Eq, Error, PartialEq)]
#[error("model call failed")]
pub(crate) struct ModelCallError {
    reason: ModelCallErrorReason,
    retry_after: Option<Duration>,
    delivery: ProviderRequestDeliveryState,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "returned to the adjacent M7 ActiveTurnTask")
)]
impl ModelCallError {
    const fn new(reason: ModelCallErrorReason) -> Self {
        Self {
            reason,
            retry_after: None,
            delivery: ProviderRequestDeliveryState::NotSent,
        }
    }

    const fn from_provider(error: ProviderAttemptError) -> Self {
        let reason = match (error.reason, error.delivery) {
            (
                ModelCallErrorReason::Timeout
                | ModelCallErrorReason::TransportUnavailable
                | ModelCallErrorReason::ProviderUnavailable
                | ModelCallErrorReason::RateLimited,
                ProviderRequestDeliveryState::AcceptedNoOutput
                | ProviderRequestDeliveryState::Unknown,
            ) => ModelCallErrorReason::RequestOutcomeUnknown,
            (
                ModelCallErrorReason::Timeout
                | ModelCallErrorReason::TransportUnavailable
                | ModelCallErrorReason::ProviderUnavailable
                | ModelCallErrorReason::RateLimited,
                ProviderRequestDeliveryState::OutputStarted,
            ) => ModelCallErrorReason::StreamInterrupted,
            (reason, _) => reason,
        };
        Self {
            reason,
            retry_after: if matches!(reason, ModelCallErrorReason::RateLimited) {
                error.retry_after
            } else {
                None
            },
            delivery: error.delivery,
        }
    }

    pub(crate) const fn reason(&self) -> ModelCallErrorReason {
        self.reason
    }

    pub(crate) const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub(crate) const fn delivery(&self) -> ProviderRequestDeliveryState {
        self.delivery
    }

    pub(crate) const fn cancelled() -> Self {
        Self::new(ModelCallErrorReason::Cancelled)
    }
}

impl fmt::Debug for ModelCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCallError")
            .field("reason", &self.reason)
            .field("retry_after", &self.retry_after)
            .field("delivery", &self.delivery)
            .finish()
    }
}

#[allow(
    dead_code,
    reason = "read by the adjacent M7 Assistant candidate reducer"
)]
#[derive(Clone)]
pub(crate) enum FinalizedAssistantContent {
    Reasoning(ReasoningContent),
    Text {
        text: Arc<str>,
    },
    ToolCall {
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}

impl fmt::Debug for FinalizedAssistantContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Reasoning(_) => "reasoning",
            Self::Text { .. } => "text",
            Self::ToolCall { .. } => "tool_call",
        };
        formatter
            .debug_struct("FinalizedAssistantContent")
            .field("kind", &kind)
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "read by the adjacent M7 Assistant candidate reducer"
    )
)]
pub(crate) struct FinalizedAssistantResponse {
    model: ModelResponseSummary,
    response_id: Option<ProviderResponseId>,
    content: Arc<[FinalizedAssistantContent]>,
    finish_reason: ModelFinishReason,
    effective_max_output_tokens: NonZeroU32,
    usage: Option<ModelUsage>,
    metadata: ProviderResponseMetadata,
}

#[allow(
    dead_code,
    reason = "read by the adjacent M7 Assistant candidate reducer"
)]
impl FinalizedAssistantResponse {
    pub(crate) const fn model(&self) -> &ModelResponseSummary {
        &self.model
    }

    pub(crate) const fn response_id(&self) -> Option<&ProviderResponseId> {
        self.response_id.as_ref()
    }

    pub(crate) fn content(&self) -> &[FinalizedAssistantContent] {
        &self.content
    }

    pub(crate) const fn finish_reason(&self) -> ModelFinishReason {
        self.finish_reason
    }

    pub(crate) const fn effective_max_output_tokens(&self) -> NonZeroU32 {
        self.effective_max_output_tokens
    }

    pub(crate) const fn usage(&self) -> Option<&ModelUsage> {
        self.usage.as_ref()
    }

    pub(crate) const fn metadata(&self) -> &ProviderResponseMetadata {
        &self.metadata
    }
}

impl fmt::Debug for FinalizedAssistantResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FinalizedAssistantResponse")
            .field("model", &self.model)
            .field("has_response_id", &self.response_id.is_some())
            .field("content_blocks", &self.content.len())
            .field("finish_reason", &self.finish_reason)
            .field(
                "effective_max_output_tokens",
                &self.effective_max_output_tokens,
            )
            .field("has_usage", &self.usage.is_some())
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "returned to the adjacent M7 ActiveTurnTask")
)]
pub(crate) struct ModelCallResult {
    response: FinalizedAssistantResponse,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "returned to the adjacent M7 ActiveTurnTask")
)]
impl ModelCallResult {
    pub(crate) const fn response(&self) -> &FinalizedAssistantResponse {
        &self.response
    }

    #[cfg(test)]
    pub(crate) fn for_compaction_test(
        model: ModelResponseSummary,
        content: Arc<[FinalizedAssistantContent]>,
        finish_reason: ModelFinishReason,
        effective_max_output_tokens: NonZeroU32,
    ) -> Self {
        Self {
            response: FinalizedAssistantResponse {
                model,
                response_id: None,
                content,
                finish_reason,
                effective_max_output_tokens,
                usage: None,
                metadata: ProviderResponseMetadata::reconstruct(None, None, None),
            },
        }
    }
}

impl fmt::Debug for ModelCallResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCallResult")
            .field("response", &self.response)
            .finish()
    }
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "invoked by the adjacent M7 ModelGateway call path"
    )
)]
fn finalize_provider_result(
    request: &ModelCallRequest,
    result: ProviderAttemptResult,
) -> Result<ModelCallResult, ModelCallError> {
    let mut content = Vec::with_capacity(result.content.len());
    let mut has_visible_text = false;
    let mut has_tool_call = false;
    let mut tool_call_ids = BTreeSet::new();

    for block in result.content.iter().cloned() {
        match block {
            ProviderAttemptContent::Reasoning(reasoning) => {
                content.push(FinalizedAssistantContent::Reasoning(reasoning));
            }
            ProviderAttemptContent::Text(text) => {
                // An empty Text block is not user-visible: it must not fail artifact
                // validation up front (ADR 0120 classifies empty Stop/Unknown as
                // `IncompleteResponse`), but it still counts as a content block for
                // structured multiple-text and downstream fidelity.
                if !text.is_empty() {
                    validate_artifact(&text, 65_536).map_err(|_| {
                        ModelCallError::new(ModelCallErrorReason::InvalidProviderResponse)
                    })?;
                    has_visible_text = true;
                }
                content.push(FinalizedAssistantContent::Text { text });
            }
            ProviderAttemptContent::ToolCall {
                tool_call_id,
                name,
                arguments,
            } => {
                has_tool_call = true;
                if !tool_call_ids.insert(tool_call_id.clone()) {
                    return Err(ModelCallError::new(
                        ModelCallErrorReason::InvalidProviderResponse,
                    ));
                }
                if !request
                    .input
                    .tools()
                    .iter()
                    .any(|definition| definition.name() == &name)
                {
                    return Err(ModelCallError::new(
                        ModelCallErrorReason::UnexpectedToolCall,
                    ));
                }
                content.push(FinalizedAssistantContent::ToolCall {
                    tool_call_id,
                    name,
                    arguments,
                });
            }
        }
    }

    let tool_calls_allowed =
        !request.input.tools_empty() && request.input.output_contract().is_none();
    if has_tool_call && !tool_calls_allowed {
        return Err(ModelCallError::new(
            ModelCallErrorReason::UnexpectedToolCall,
        ));
    }
    if matches!(
        result.finish_reason,
        ModelFinishReason::Length | ModelFinishReason::ContentFiltered
    ) {
        return Err(ModelCallError::new(
            ModelCallErrorReason::IncompleteResponse,
        ));
    }
    if (result.finish_reason == ModelFinishReason::ToolCalls && !has_tool_call)
        || (matches!(
            result.finish_reason,
            ModelFinishReason::Stop | ModelFinishReason::Refused
        ) && has_tool_call)
    {
        return Err(ModelCallError::new(
            ModelCallErrorReason::InvalidProviderResponse,
        ));
    }
    match result.finish_reason {
        ModelFinishReason::Refused if !has_visible_text => {
            return Err(ModelCallError::new(
                ModelCallErrorReason::InvalidProviderResponse,
            ));
        }
        ModelFinishReason::Stop if !has_visible_text => {
            return Err(ModelCallError::new(
                ModelCallErrorReason::IncompleteResponse,
            ));
        }
        ModelFinishReason::Unknown if !has_tool_call && !has_visible_text => {
            return Err(ModelCallError::new(
                ModelCallErrorReason::IncompleteResponse,
            ));
        }
        _ => {}
    }

    if let Some(OutputContract::Structured(contract)) = request.input.output_contract() {
        // A non-empty Refused response is a terminal success that skips structured schema
        // validation; every other complete finish must satisfy the contract exactly.
        if result.finish_reason != ModelFinishReason::Refused {
            validate_structured_output(&content, contract)
                .map_err(|_| ModelCallError::new(ModelCallErrorReason::InvalidStructuredOutput))?;
        }
    }

    let definition = request.model.definition();
    Ok(ModelCallResult {
        response: FinalizedAssistantResponse {
            model: ModelResponseSummary::new(
                definition.provider_id().clone(),
                definition.model_id().clone(),
                request.model.generation().reasoning(),
                request.model.generation().service_class(),
            ),
            response_id: result.response_id,
            content: content.into(),
            finish_reason: result.finish_reason,
            effective_max_output_tokens: request.effective_max_output_tokens(),
            usage: result.usage,
            metadata: result.metadata,
        },
    })
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

/// A structured terminal must be exactly one non-empty Text block (Reasoning blocks may be
/// retained alongside it) whose text parses exactly as a `BoundedJsonObject` that satisfies
/// the contract schema.  Multiple Text blocks, JSON fences, syntax errors, scalar roots, and
/// schema mismatches are all `InvalidStructuredOutput`.
fn validate_structured_output(
    content: &[FinalizedAssistantContent],
    contract: &StructuredOutputContract,
) -> Result<(), ()> {
    let mut text_blocks = content.iter().filter_map(|block| match block {
        FinalizedAssistantContent::Text { text } => Some(text),
        _ => None,
    });
    let Some(text) = text_blocks.next() else {
        return Err(());
    };
    if text_blocks.next().is_some() {
        return Err(());
    }
    let object: BoundedJsonObject = text.parse().map_err(|_| ())?;
    if !contract.validate_instance(&object) {
        return Err(());
    }
    Ok(())
}

fn validate_artifact(value: &str, maximum: usize) -> Result<(), ModelValueError> {
    validate_safe_text(value, maximum, false).map_err(|_| ModelValueError::InvalidArtifact)
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::Mutex;

    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_session_lifecycle::AgentRevisionRef;
    use crate::conversation_storage::{
        StoredAssistantContent, StoredAssistantMessage, StoredToolMessage, StoredToolOutcome,
        StoredUserMessage,
    };
    use crate::live_conversation::LiveSessionState;
    use crate::prompt::{
        AgentPromptSelection, ModelMessageRef, PromptAssemblyInput, PromptBodyIntent,
        PromptErrorKind, PromptIntent, PromptService, PromptTurnContext, SessionPromptSelection,
        TextIntent,
    };
    use crate::skills::SkillView;
    use crate::tools::{
        ToolDefinition, ToolExecutionMode, ToolExecutionResult, ToolOutcomeSource,
        ToolResultContent, ToolResultDisposition, ToolSet,
    };
    use crate::turn_item_interaction::{AssistantDisposition, UserMessageSource};
    use crate::wire::{
        AgentRevision, ItemId, SessionDefinitionRevision, SessionId, Timestamp, TurnId,
    };
    use crate::workspace::prompt_candidate_for_test;

    struct MutableModelSource {
        definitions: Mutex<Vec<ModelDefinition>>,
    }

    impl MutableModelSource {
        fn new(definitions: Vec<ModelDefinition>) -> Self {
            Self {
                definitions: Mutex::new(definitions),
            }
        }

        fn replace(&self, definitions: Vec<ModelDefinition>) {
            *self.definitions.lock().unwrap() = definitions;
        }
    }

    impl ModelSourceAdapter for MutableModelSource {
        fn discover(&self) -> ModelSourceFuture<'_> {
            let definitions = self.definitions.lock().unwrap().clone();
            Box::pin(async move { Ok(definitions) })
        }
    }

    pub(super) fn text_definition(
        version: u64,
        default_max_output_tokens: u32,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> ModelDefinition {
        text_definition_with_credential(
            version,
            default_max_output_tokens,
            128_000,
            adapter,
            "test-credential",
        )
    }

    pub(super) fn text_definition_with_credential(
        version: u64,
        default_max_output_tokens: u32,
        context_window_tokens: u32,
        adapter: Arc<dyn ProviderAdapter>,
        credential: &str,
    ) -> ModelDefinition {
        ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(version).unwrap()),
            "gpt-5".parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(
                NonZeroU32::new(context_window_tokens),
                NonZeroU32::new(8_192),
            ),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(default_max_output_tokens).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            adapter,
            fixed_credential_source(credential),
        )
        .unwrap()
    }

    fn text_definition_with_context_limit(
        version: u64,
        default_max_output_tokens: u32,
        context_window_tokens: u32,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> ModelDefinition {
        text_definition_with_credential(
            version,
            default_max_output_tokens,
            context_window_tokens,
            adapter,
            "test-credential",
        )
    }

    pub(super) fn resolve_request(max_output_tokens: Option<u32>) -> ResolveTurnModelRequest {
        ResolveTurnModelRequest::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ReasoningPreference::Auto,
            max_output_tokens.and_then(NonZeroU32::new),
        )
    }

    async fn prompt_set_for_model(model: Arc<TurnModelSnapshot>) -> Arc<crate::prompt::PromptSet> {
        prompt_set_for_model_with_tools(model, ToolSet::empty()).await
    }

    async fn prompt_set_for_model_with_tools(
        model: Arc<TurnModelSnapshot>,
        tools: Arc<ToolSet>,
    ) -> Arc<crate::prompt::PromptSet> {
        let service = PromptService::new(
            Arc::from("SECRET required system"),
            Some(Arc::from("SECRET base system")),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let resources = service.initialize().await.unwrap();
        let session_id = "ses_00000000000000000000000000000001"
            .parse::<SessionId>()
            .unwrap();
        let workspace = prompt_candidate_for_test(session_id, vec!["root".parse().unwrap()])
            .finish(Arc::from([]), Arc::from([]))
            .unwrap()
            .prompt_context();
        let skills = SkillView::empty();
        service
            .for_turn(PromptTurnContext::new(
                AgentRevisionRef::new(
                    "agt_00000000000000000000000000000001".parse().unwrap(),
                    AgentRevision::new(NonZeroU64::new(1).unwrap()),
                ),
                session_id,
                SessionDefinitionRevision::new(NonZeroU64::new(1).unwrap()),
                resources,
                AgentPromptSelection::new(Vec::new()).unwrap(),
                SessionPromptSelection::new(Vec::new()).unwrap(),
                workspace,
                tools.prompt_view(),
                skills.prompt_view(),
                model,
            ))
            .unwrap()
    }

    pub(super) fn scripted_tool_set() -> Arc<ToolSet> {
        ToolSet::with_executor(
            vec![
                ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    ToolExecutionMode::Parallel,
                )
                .unwrap(),
            ],
            |_| {
                Box::pin(async {
                    ToolExecutionResult::completed_text("unused scripted result").unwrap()
                })
            },
        )
    }

    fn live_user_context(
        set: &crate::prompt::PromptSet,
    ) -> (
        LiveSessionState,
        crate::live_conversation::ConversationRevision,
    ) {
        let session_id = "ses_00000000000000000000000000000001"
            .parse::<SessionId>()
            .unwrap();
        let user = set
            .compose_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("SECRET live user input").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut live = LiveSessionState::new(session_id, []);
        live.apply_user_message(
            StoredUserMessage::reconstruct(
                "itm_00000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                UserMessageSource::Input,
                user,
            ),
            "trn_00000000000000000000000000000001"
                .parse::<TurnId>()
                .unwrap(),
            "2026-08-08T00:00:00.000Z".parse::<Timestamp>().unwrap(),
        )
        .unwrap();
        let revision = live
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        (live, revision)
    }

    fn text_attempt_result(text: &str) -> ProviderAttemptResult {
        ProviderAttemptResult {
            response_id: None,
            content: Arc::from([ProviderAttemptContent::Text(Arc::from(text))]),
            finish_reason: ModelFinishReason::Stop,
            usage: None,
            metadata: ProviderResponseMetadata::new(None, None, None),
        }
    }

    pub(super) async fn request_for_model(model: Arc<TurnModelSnapshot>) -> Arc<ModelCallRequest> {
        request_for_model_with_tools(model, ToolSet::empty()).await
    }

    pub(super) async fn request_for_model_with_tools(
        model: Arc<TurnModelSnapshot>,
        tools: Arc<ToolSet>,
    ) -> Arc<ModelCallRequest> {
        let prompt_set = prompt_set_for_model_with_tools(Arc::clone(&model), tools).await;
        let (live, revision) = live_user_context(&prompt_set);
        let views = live.capture_conversation_views().unwrap();
        let input = Arc::new(
            prompt_set
                .assemble(PromptAssemblyInput::agent_run(views.conversation(), None))
                .unwrap(),
        );
        Arc::new(
            ModelCallRequest::new(model, ModelCallPurpose::AgentRun, input, revision, None)
                .unwrap(),
        )
    }

    /// The two provider-truthful replay shapes share one assembly helper: the
    /// conversation is identical (user input, an Intermediate assistant with
    /// replayable reasoning, text and a tool call, the tool result, and a Steer
    /// user message); only the reasoning artifact differs per provider protocol.
    #[derive(Clone, Copy)]
    enum ReplayReasoningShape {
        /// OpenAI Responses replay: provider item id + text (plus a summary so
        /// the official-required `summary` field carries a summary_text entry);
        /// never an Anthropic signature.
        OpenAi,
        /// Anthropic Messages replay: exact text + exact signature (the Claude
        /// thinking-block requirement); never a provider item id or OpenAI-only
        /// artifact.
        Anthropic,
    }

    /// Assembles an OpenAI-truthful replay request: reasoning carries a
    /// `ProviderItemId` (plus text/summary) and no Anthropic signature, and the
    /// Turn-pinned tool set includes the `echo` tool the replayed ToolCall
    /// refers to (the current request therefore carries the echo tool
    /// definition). Test-only; no production interface.
    pub(super) async fn openai_replay_request_for_model(
        model: Arc<TurnModelSnapshot>,
    ) -> Arc<ModelCallRequest> {
        replay_request_for_model(model, ReplayReasoningShape::OpenAi).await
    }

    /// Assembles an Anthropic-truthful replay request: reasoning carries the
    /// exact text with its original signature (the Claude thinking-block replay
    /// requirement) and no `ProviderItemId`/OpenAI-only artifact, and the
    /// Turn-pinned tool set includes the `echo` tool the replayed ToolCall
    /// refers to. Test-only; no production interface.
    pub(super) async fn anthropic_replay_request_for_model(
        model: Arc<TurnModelSnapshot>,
    ) -> Arc<ModelCallRequest> {
        replay_request_for_model(model, ReplayReasoningShape::Anthropic).await
    }

    async fn replay_request_for_model(
        model: Arc<TurnModelSnapshot>,
        shape: ReplayReasoningShape,
    ) -> Arc<ModelCallRequest> {
        // The replayed ToolCall refers to the `echo` tool, so the Turn-pinned
        // tool set must include it: the current request then carries the echo
        // tool definition and the historical echo ToolCall stays compatible
        // with the pinned tool set.
        let prompt_set =
            prompt_set_for_model_with_tools(Arc::clone(&model), scripted_tool_set()).await;
        let session_id = "ses_00000000000000000000000000000001".parse().unwrap();
        let turn_id = "trn_00000000000000000000000000000001".parse().unwrap();
        let timestamp = "2026-08-08T00:00:00.000Z".parse().unwrap();
        let input_user = prompt_set
            .compose_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("SECRET live user input").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut live = LiveSessionState::new(session_id, []);
        live.apply_user_message(
            StoredUserMessage::reconstruct(
                "itm_00000000000000000000000000000001".parse().unwrap(),
                UserMessageSource::Input,
                input_user,
            ),
            turn_id,
            timestamp,
        )
        .unwrap();
        // Replayable reasoning carries a provider item id; the assistant also
        // carries text and one tool call, and the tool result completes the
        // exchange so the sanitized view keeps it. The reasoning artifact is
        // provider-truthful: OpenAI replays from text/summary + item id,
        // Anthropic replays from exact text + signature.
        let reasoning = match shape {
            ReplayReasoningShape::OpenAi => ReasoningContent::new(
                Some("prior reasoning".to_owned()),
                Some("prior summary".to_owned()),
                None,
                None,
                Some("rs_replay".parse().unwrap()),
            )
            .unwrap(),
            ReplayReasoningShape::Anthropic => ReasoningContent::new(
                Some("prior reasoning".to_owned()),
                None,
                None,
                Some("sig_replay".to_owned()),
                None,
            )
            .unwrap(),
        };
        let assistant = StoredAssistantMessage::reconstruct(
            AssistantDisposition::Intermediate,
            vec![
                StoredAssistantContent::Reasoning {
                    item_id: "itm_00000000000000000000000000000002".parse().unwrap(),
                    content: reasoning,
                },
                StoredAssistantContent::Text {
                    item_id: "itm_00000000000000000000000000000003".parse().unwrap(),
                    text: Arc::from("let me check"),
                },
                StoredAssistantContent::ToolCall {
                    item_id: "itm_00000000000000000000000000000004".parse().unwrap(),
                    tool_call_id: "call_replay".parse().unwrap(),
                    name: "echo".parse().unwrap(),
                    arguments: r#"{}"#.parse().unwrap(),
                },
            ],
            ModelResponseSummary::new(
                model.execution.selection.provider_id().clone(),
                model.execution.selection.model_id().clone(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            None,
            ModelFinishReason::ToolCalls,
            NonZeroU32::new(4_096).unwrap(),
            None,
            0,
            ProviderResponseMetadata::reconstruct(None, None, None),
        )
        .unwrap();
        live.apply_assistant_message(assistant, turn_id, timestamp)
            .unwrap();
        // The tool result settles the exact ToolInvocation item (the same ItemId
        // the assistant tool call projected), completing the exchange.
        let tool = StoredToolMessage::reconstruct(
            "itm_00000000000000000000000000000004".parse().unwrap(),
            "call_replay".parse().unwrap(),
            StoredToolOutcome::completed(
                ToolOutcomeSource::Executed,
                ToolResultDisposition::Succeeded,
                ToolResultContent::from_text_parts(vec!["ok".to_owned()]).unwrap(),
            )
            .unwrap(),
        );
        live.apply_tool_message(tool, turn_id, timestamp).unwrap();
        let steer = prompt_set
            .compose_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("continue").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        live.apply_user_message(
            StoredUserMessage::reconstruct(
                "itm_00000000000000000000000000000005".parse().unwrap(),
                UserMessageSource::Steer,
                steer,
            ),
            turn_id,
            timestamp,
        )
        .unwrap();
        let views = live.capture_conversation_views().unwrap();
        let input = Arc::new(
            prompt_set
                .assemble(PromptAssemblyInput::agent_run(views.conversation(), None))
                .unwrap(),
        );
        Arc::new(
            ModelCallRequest::new(
                model,
                ModelCallPurpose::AgentRun,
                input,
                views.conversation().revision(),
                None,
            )
            .unwrap(),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn catalog_resolution_pins_exact_definition_across_reload_candidates() {
        let old_scripted = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(Vec::new(), text_attempt_result("old definition")),
        ]));
        let old_adapter: Arc<dyn ProviderAdapter> = old_scripted.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1,
            4_096,
            old_adapter,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source.clone();
        let gateway = ModelGateway::new(vec![source_adapter]);

        let old_catalog = gateway.initialize().await.unwrap();
        let old_snapshot = gateway
            .resolve_for_turn(Arc::clone(&old_catalog), resolve_request(None))
            .unwrap();
        assert_eq!(old_catalog.definition_count(), 1);
        assert_eq!(old_snapshot.definition().version().get(), 1);
        assert_eq!(old_snapshot.generation().max_output_tokens().get(), 4_096);
        assert_eq!(old_snapshot.token_estimator().estimate_utf8_bytes(7), 3);

        let new_scripted = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(Vec::new(), text_attempt_result("new definition")),
        ]));
        let new_adapter: Arc<dyn ProviderAdapter> = new_scripted.clone();
        source.replace(vec![text_definition(2, 2_048, new_adapter)]);
        let new_catalog = gateway.build_reload_candidate().await.unwrap();
        let new_snapshot = gateway
            .resolve_for_turn(new_catalog, resolve_request(None))
            .unwrap();

        assert_eq!(old_snapshot.definition().version().get(), 1);
        assert_eq!(old_snapshot.generation().max_output_tokens().get(), 4_096);
        assert_eq!(new_snapshot.definition().version().get(), 2);
        assert_eq!(new_snapshot.generation().max_output_tokens().get(), 2_048);

        let old_result = gateway
            .generate_model_turn(
                request_for_model(Arc::clone(&old_snapshot)).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let new_result = gateway
            .generate_model_turn(
                request_for_model(new_snapshot).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match &old_result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => assert_eq!(&**text, "old definition"),
            _ => panic!("old model snapshot changed content family"),
        }
        match &new_result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => assert_eq!(&**text, "new definition"),
            _ => panic!("new model snapshot changed content family"),
        }
        assert_eq!(old_scripted.requests().len(), 1);
        assert_eq!(new_scripted.requests().len(), 1);
    }

    /// A provider adapter that records the exact credential it received per attempt.
    struct RecordingCredentialAdapter {
        observed: Arc<Mutex<Vec<String>>>,
    }

    impl ProviderAdapter for RecordingCredentialAdapter {
        fn execute(
            &self,
            request: ProviderAttemptRequest,
            _progress: ModelProgressPublisher,
            _cancel: CancellationToken,
        ) -> ProviderAttemptFuture<'_> {
            self.observed
                .lock()
                .unwrap()
                .push(request.credential().for_header().to_owned());
            Box::pin(async move {
                Ok(ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::Text(Arc::from("ok"))]),
                    finish_reason: ModelFinishReason::Stop,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                })
            })
        }
    }

    /// A model source that counts how many times the catalog rebuilt it.
    struct CountingModelSource {
        definition: ModelDefinition,
        discover_count: Mutex<usize>,
    }

    impl ModelSourceAdapter for CountingModelSource {
        fn discover(&self) -> ModelSourceFuture<'_> {
            *self.discover_count.lock().unwrap() += 1;
            let definition = self.definition.clone();
            Box::pin(async move { Ok(vec![definition]) })
        }
    }

    /// A mutable credential source that pops the next credential per resolution.
    pub(super) struct MutableCredentialSource {
        pub(super) credentials: Mutex<Vec<ProviderCredential>>,
    }

    impl CredentialSource for MutableCredentialSource {
        fn resolve(&self) -> CredentialSourceFuture<'_> {
            let credential = self
                .credentials
                .lock()
                .unwrap()
                .pop()
                .expect("test credential queue must not underflow");
            Box::pin(async move { Some(credential) })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_credential_is_auth_missing_not_sent_without_adapter_execution() {
        struct MissingCredentialSource;

        impl CredentialSource for MissingCredentialSource {
            fn resolve(&self) -> CredentialSourceFuture<'_> {
                Box::pin(async { None })
            }
        }

        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(Vec::new(), text_attempt_result("must not execute")),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let definition = ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            "gpt-5".parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192)),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(4_096).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            provider,
            Arc::new(MissingCredentialSource),
        )
        .unwrap();
        let source = Arc::new(MutableModelSource::new(vec![definition]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();

        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.reason(), ModelCallErrorReason::AuthMissing);
        assert_eq!(error.delivery(), ProviderRequestDeliveryState::NotSent);
        assert!(
            adapter.requests().is_empty(),
            "a missing credential must never reach the provider adapter"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retained_snapshot_resolves_the_credential_source_on_each_attempt() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let adapter = Arc::new(RecordingCredentialAdapter {
            observed: Arc::clone(&observed),
        });
        let provider: Arc<dyn ProviderAdapter> = adapter;
        let credential_source = Arc::new(MutableCredentialSource {
            // Vec::pop yields the last element first, so the queue is reversed.
            credentials: Mutex::new(vec![
                "cred-two".parse().unwrap(),
                "cred-one".parse().unwrap(),
            ]),
        });
        let definition = ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            "gpt-5".parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192)),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(4_096).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            provider,
            credential_source,
        )
        .unwrap();
        let source = Arc::new(CountingModelSource {
            definition,
            discover_count: Mutex::new(0),
        });
        let counting_source = Arc::clone(&source);
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let snapshot = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let request = request_for_model(Arc::clone(&snapshot)).await;

        for _ in 0..2 {
            gateway
                .generate_model_turn(
                    Arc::clone(&request),
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        assert_eq!(
            *observed.lock().unwrap(),
            ["cred-one", "cred-two"],
            "the same retained snapshot must resolve the dynamic source on each attempt"
        );
        assert_eq!(
            *counting_source.discover_count.lock().unwrap(),
            1,
            "resolving credentials must never rebuild the catalog"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_during_credential_resolution_never_executes_the_adapter() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        struct GatedCredentialSource {
            started: Arc<Notify>,
            release: Arc<Notify>,
        }

        impl CredentialSource for GatedCredentialSource {
            fn resolve(&self) -> CredentialSourceFuture<'_> {
                let started = Arc::clone(&self.started);
                let release = Arc::clone(&self.release);
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    None
                })
            }
        }

        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(Vec::new(), text_attempt_result("must not execute")),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let definition = ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            "gpt-5".parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192)),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(4_096).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            provider,
            Arc::new(GatedCredentialSource {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            }),
        )
        .unwrap();
        let source = Arc::new(MutableModelSource::new(vec![definition]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = Arc::new(ModelGateway::new(vec![source_adapter]));
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let request = request_for_model(model).await;

        let cancel = CancellationToken::new();
        let gateway_task = Arc::clone(&gateway);
        let request_task = Arc::clone(&request);
        let cancel_task = cancel.clone();
        let generation = tokio::spawn(async move {
            gateway_task
                .generate_model_turn(request_task, ModelProgressPublisher::discard(), cancel_task)
                .await
        });

        // Deterministic ordering: the source signals that resolution is parked, then
        // the test cancels exactly inside that window; no sleep or timeout is used.
        started.notified().await;
        cancel.cancel();

        let error = generation
            .await
            .expect("generation task must settle")
            .expect_err("cancellation must fail the attempt");
        assert_eq!(error.reason(), ModelCallErrorReason::Cancelled);
        assert_eq!(error.delivery(), ProviderRequestDeliveryState::NotSent);
        assert!(
            adapter.requests().is_empty(),
            "cancellation during credential resolution must never execute the adapter"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn model_call_request_rejects_cross_model_stale_and_over_limit_assembly() {
        let first_adapter: Arc<dyn ProviderAdapter> =
            Arc::new(ScriptedProviderAdapter::new(Vec::new()));
        let first_source = Arc::new(MutableModelSource::new(vec![text_definition(
            1,
            4_096,
            first_adapter,
        )]));
        let first_source_adapter: Arc<dyn ModelSourceAdapter> = first_source;
        let first_gateway = ModelGateway::new(vec![first_source_adapter]);
        let first_model = first_gateway
            .resolve_for_turn(
                first_gateway.initialize().await.unwrap(),
                resolve_request(None),
            )
            .unwrap();
        let valid = request_for_model(Arc::clone(&first_model)).await;

        let second_adapter: Arc<dyn ProviderAdapter> =
            Arc::new(ScriptedProviderAdapter::new(Vec::new()));
        let second_source = Arc::new(MutableModelSource::new(vec![text_definition(
            1,
            4_096,
            second_adapter,
        )]));
        let second_source_adapter: Arc<dyn ModelSourceAdapter> = second_source;
        let second_gateway = ModelGateway::new(vec![second_source_adapter]);
        let second_model = second_gateway
            .resolve_for_turn(
                second_gateway.initialize().await.unwrap(),
                resolve_request(None),
            )
            .unwrap();

        let cross_model = ModelCallRequest::new(
            second_model,
            valid.purpose(),
            Arc::clone(valid.input()),
            valid.source_revision(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            cross_model.kind(),
            ModelRequestValidationErrorKind::AssemblyMismatch
        );

        let stale = ModelCallRequest::new(
            Arc::clone(&first_model),
            valid.purpose(),
            Arc::clone(valid.input()),
            valid.source_revision().checked_next().unwrap(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            stale.kind(),
            ModelRequestValidationErrorKind::AssemblyMismatch
        );

        let over_limit = ModelCallRequest::new(
            first_model,
            valid.purpose(),
            Arc::clone(valid.input()),
            valid.source_revision(),
            NonZeroU32::new(8_193),
        )
        .unwrap_err();
        assert_eq!(
            over_limit.kind(),
            ModelRequestValidationErrorKind::InvalidOutputLimit
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_prompt_fails_before_provider_attempt() {
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(Vec::new(), text_attempt_result("must not execute")),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![
            text_definition_with_context_limit(1, 4_096, 1, provider),
        ]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let prompt_set = prompt_set_for_model(model).await;
        let (live, _) = live_user_context(&prompt_set);
        let views = live.capture_conversation_views().unwrap();

        let error = prompt_set
            .assemble(PromptAssemblyInput::agent_run(views.conversation(), None))
            .unwrap_err();

        assert_eq!(error.kind(), PromptErrorKind::ContextLimitExceeded);
        assert!(adapter.requests().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_agent_run_preserves_exact_request_progress_and_terminal_text() {
        let terminal = ProviderAttemptResult {
            response_id: Some("response-1".parse().unwrap()),
            content: Arc::from([ProviderAttemptContent::Text(Arc::from(
                "SECRET final answer",
            ))]),
            finish_reason: ModelFinishReason::Stop,
            usage: Some(ModelUsage::new(
                Some(12),
                Some(4),
                None,
                None,
                None,
                Some(16),
                None,
            )),
            metadata: ProviderResponseMetadata::new(
                Some("SECRET-provider-request".parse().unwrap()),
                Some("stop".parse().unwrap()),
                None,
            ),
        };
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(
                vec![
                    ModelProgressEvent::ContentDelta {
                        content_index: 0,
                        delta: ModelContentDelta::Text(Arc::from("first")),
                    },
                    ModelProgressEvent::ContentDelta {
                        content_index: 0,
                        delta: ModelContentDelta::Text(Arc::from(" second")),
                    },
                ],
                terminal,
            ),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let catalog = gateway.initialize().await.unwrap();
        let model = gateway
            .resolve_for_turn(catalog, resolve_request(None))
            .unwrap();
        let prompt_set = prompt_set_for_model(Arc::clone(&model)).await;
        let (live, revision) = live_user_context(&prompt_set);
        let views = live.capture_conversation_views().unwrap();
        let input = Arc::new(
            prompt_set
                .assemble(PromptAssemblyInput::agent_run(views.conversation(), None))
                .unwrap(),
        );
        let request = Arc::new(
            ModelCallRequest::new(
                Arc::clone(&model),
                ModelCallPurpose::AgentRun,
                input,
                revision,
                None,
            )
            .unwrap(),
        );
        let progress_events = Arc::new(Mutex::new(Vec::new()));
        let captured_progress = Arc::clone(&progress_events);
        let progress = ModelProgressPublisher::new(move |event| {
            captured_progress.lock().unwrap().push(event);
        });

        let result = gateway
            .generate_model_turn(Arc::clone(&request), progress, CancellationToken::new())
            .await
            .unwrap();

        let attempts = adapter.requests();
        assert_eq!(attempts.len(), 1);
        assert!(Arc::ptr_eq(&attempts[0], &request));
        assert_eq!(
            attempts[0]
                .input()
                .system()
                .iter()
                .map(crate::prompt::PromptSection::text)
                .collect::<Vec<_>>(),
            ["SECRET required system", "SECRET base system"]
        );
        assert_eq!(attempts[0].input().messages().len(), 1);
        match attempts[0].input().messages()[0].as_ref() {
            ModelMessageRef::User { content } => {
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].as_text(), "SECRET live user input");
            }
            _ => panic!("adapter request changed the sanitized conversation role"),
        }
        assert_eq!(
            *progress_events.lock().unwrap(),
            [
                ModelProgressEvent::ContentDelta {
                    content_index: 0,
                    delta: ModelContentDelta::Text(Arc::from("first")),
                },
                ModelProgressEvent::ContentDelta {
                    content_index: 0,
                    delta: ModelContentDelta::Text(Arc::from(" second")),
                },
            ]
        );
        assert_eq!(result.response().model().provider_id().as_str(), "openai");
        assert_eq!(result.response().model().model_id().as_str(), "gpt-5");
        assert_eq!(result.response().effective_max_output_tokens().get(), 4_096);
        assert_eq!(result.response().content().len(), 1);
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, "SECRET final answer");
            }
            _ => panic!("scripted text response changed content family"),
        }
        assert_eq!(result.response().usage().unwrap().output_tokens(), Some(4));

        for debug in [
            format!("{request:?}"),
            format!("{:?}", attempts[0]),
            format!("{result:?}"),
        ] {
            assert!(!debug.contains("SECRET"));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_observes_zero_attempt_and_terminal_first_linearization() {
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::wait_for_cancellation(),
            ScriptedProviderScript::success_then_cancel(text_attempt_result("terminal wins")),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let prompt_set = prompt_set_for_model(Arc::clone(&model)).await;
        let (live, revision) = live_user_context(&prompt_set);
        let views = live.capture_conversation_views().unwrap();
        let input = Arc::new(
            prompt_set
                .assemble(PromptAssemblyInput::agent_run(views.conversation(), None))
                .unwrap(),
        );
        let request = Arc::new(
            ModelCallRequest::new(model, ModelCallPurpose::AgentRun, input, revision, None)
                .unwrap(),
        );

        let cancelled_before_call = CancellationToken::new();
        cancelled_before_call.cancel();
        let error = gateway
            .generate_model_turn(
                Arc::clone(&request),
                ModelProgressPublisher::discard(),
                cancelled_before_call,
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason(), ModelCallErrorReason::Cancelled);
        assert!(adapter.requests().is_empty());

        let cancel_during_attempt = CancellationToken::new();
        let cancel_from_task = cancel_during_attempt.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            cancel_from_task.cancel();
        });
        let error = gateway
            .generate_model_turn(
                Arc::clone(&request),
                ModelProgressPublisher::discard(),
                cancel_during_attempt,
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason(), ModelCallErrorReason::Cancelled);
        assert_eq!(adapter.requests().len(), 1);

        let terminal_first = CancellationToken::new();
        let result = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                terminal_first.clone(),
            )
            .await
            .unwrap();
        assert!(terminal_first.is_cancelled());
        assert_eq!(adapter.requests().len(), 2);
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => assert_eq!(&**text, "terminal wins"),
            _ => panic!("terminal-first result changed content family"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn response_validation_fails_closed_before_returning_an_assistant_candidate() {
        let reasoning_only = ProviderAttemptResult {
            response_id: None,
            content: Arc::from([ProviderAttemptContent::Reasoning(
                ReasoningContent::new(None, Some("summary only".to_owned()), None, None, None)
                    .unwrap(),
            )]),
            finish_reason: ModelFinishReason::Stop,
            usage: None,
            metadata: ProviderResponseMetadata::new(None, None, None),
        };
        let forbidden_tool = ProviderAttemptResult {
            response_id: None,
            content: Arc::from([ProviderAttemptContent::ToolCall {
                tool_call_id: "call_forbidden".parse().unwrap(),
                name: "missing".parse().unwrap(),
                arguments: "{}".parse().unwrap(),
            }]),
            finish_reason: ModelFinishReason::ToolCalls,
            usage: None,
            metadata: ProviderResponseMetadata::new(None, None, None),
        };
        let scripts = vec![
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    finish_reason: ModelFinishReason::Length,
                    ..text_attempt_result("partial")
                },
            ),
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    finish_reason: ModelFinishReason::ContentFiltered,
                    ..text_attempt_result("filtered partial")
                },
            ),
            ScriptedProviderScript::success(Vec::new(), reasoning_only),
            ScriptedProviderScript::success(Vec::new(), forbidden_tool),
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    finish_reason: ModelFinishReason::ToolCalls,
                    ..text_attempt_result("finish mismatch")
                },
            ),
        ];
        let adapter = Arc::new(ScriptedProviderAdapter::new(scripts));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let prompt_set = prompt_set_for_model(Arc::clone(&model)).await;
        let (live, revision) = live_user_context(&prompt_set);
        let views = live.capture_conversation_views().unwrap();
        let input = Arc::new(
            prompt_set
                .assemble(PromptAssemblyInput::agent_run(views.conversation(), None))
                .unwrap(),
        );
        let request = Arc::new(
            ModelCallRequest::new(model, ModelCallPurpose::AgentRun, input, revision, None)
                .unwrap(),
        );

        for expected in [
            ModelCallErrorReason::IncompleteResponse,
            ModelCallErrorReason::IncompleteResponse,
            ModelCallErrorReason::IncompleteResponse,
            ModelCallErrorReason::UnexpectedToolCall,
            ModelCallErrorReason::InvalidProviderResponse,
        ] {
            let error = gateway
                .generate_model_turn(
                    Arc::clone(&request),
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.reason(), expected);
        }
        assert_eq!(adapter.requests().len(), 5);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_text_finish_reasons_follow_the_completeness_contract() {
        let scripts = vec![
            // Empty Stop is an explicitly incomplete terminal, not an invalid response.
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    finish_reason: ModelFinishReason::Stop,
                    ..text_attempt_result("")
                },
            ),
            // Empty Unknown is an incomplete terminal, not an invalid response.
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    finish_reason: ModelFinishReason::Unknown,
                    ..text_attempt_result("")
                },
            ),
            // Refused without non-empty refusal text stays InvalidProviderResponse.
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    finish_reason: ModelFinishReason::Refused,
                    ..text_attempt_result("")
                },
            ),
            // Non-empty text still runs artifact validation: unsafe text is rejected.
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::Text(Arc::from(
                        "unsafe\u{0001} text",
                    ))]),
                    finish_reason: ModelFinishReason::Stop,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
            // Non-empty oversize text is still rejected by the byte cap.
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::Text(Arc::from(
                        "x".repeat(65_537),
                    ))]),
                    finish_reason: ModelFinishReason::Stop,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
        ];
        let adapter = Arc::new(ScriptedProviderAdapter::new(scripts));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let request = request_for_model(Arc::clone(&model)).await;

        for expected in [
            ModelCallErrorReason::IncompleteResponse,
            ModelCallErrorReason::IncompleteResponse,
            ModelCallErrorReason::InvalidProviderResponse,
            ModelCallErrorReason::InvalidProviderResponse,
            ModelCallErrorReason::InvalidProviderResponse,
        ] {
            let error = gateway
                .generate_model_turn(
                    Arc::clone(&request),
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.reason(), expected);
        }
        assert_eq!(adapter.requests().len(), 5);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn structured_empty_stop_is_incomplete_response() {
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::Text(Arc::from(""))]),
                    finish_reason: ModelFinishReason::Stop,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![structured_definition(
            1,
            4_096,
            NonZeroU32::new(65_536).unwrap(),
            provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let contract =
            StructuredOutputContract::new(&model, None, structured_schema(r#"{"type":"object"}"#))
                .unwrap();
        let request = structured_request(&model, &contract).await;

        let error = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason(), ModelCallErrorReason::IncompleteResponse);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn allowed_tool_responses_reject_duplicate_ids_but_accept_unknown_finish_reason() {
        let duplicate_id: ToolCallId = "call_duplicate".parse().unwrap();
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([
                        ProviderAttemptContent::ToolCall {
                            tool_call_id: duplicate_id.clone(),
                            name: "echo".parse().unwrap(),
                            arguments: "{}".parse().unwrap(),
                        },
                        ProviderAttemptContent::ToolCall {
                            tool_call_id: duplicate_id,
                            name: "echo".parse().unwrap(),
                            arguments: "{}".parse().unwrap(),
                        },
                    ]),
                    finish_reason: ModelFinishReason::ToolCalls,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::ToolCall {
                        tool_call_id: "call_unknown_finish".parse().unwrap(),
                        name: "echo".parse().unwrap(),
                        arguments: "{}".parse().unwrap(),
                    }]),
                    finish_reason: ModelFinishReason::Unknown,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let tools = scripted_tool_set();
        let request = request_for_model_with_tools(Arc::clone(&model), tools).await;

        let duplicate = gateway
            .generate_model_turn(
                Arc::clone(&request),
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            duplicate.reason(),
            ModelCallErrorReason::InvalidProviderResponse
        );

        let accepted = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            accepted.response().content(),
            [FinalizedAssistantContent::ToolCall {
                tool_call_id,
                name,
                ..
            }] if tool_call_id.as_str() == "call_unknown_finish" && name.as_str() == "echo"
        ));
        assert_eq!(adapter.requests().len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_tool_calls_output_contract_rejects_a_non_empty_tool_prompt() {
        let adapter: Arc<dyn ProviderAdapter> = Arc::new(ScriptedProviderAdapter::new(Vec::new()));
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, adapter,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let tool_set = scripted_tool_set();
        let prompt_set = prompt_set_for_model_with_tools(Arc::clone(&model), tool_set).await;
        let (live, revision) = live_user_context(&prompt_set);
        let views = live.capture_conversation_views().unwrap();
        let contract = OutputContract::NoToolCalls;
        let input = Arc::new(
            prompt_set
                .assemble(PromptAssemblyInput::agent_run(
                    views.conversation(),
                    Some(&contract),
                ))
                .unwrap(),
        );
        assert_eq!(input.tools().len(), 1);
        assert_eq!(input.tools()[0].name().as_str(), "echo");

        let error = ModelCallRequest::new(model, ModelCallPurpose::AgentRun, input, revision, None)
            .unwrap_err();
        assert_eq!(
            error.kind(),
            ModelRequestValidationErrorKind::UnsupportedInput
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provider_error_is_typed_terminal_without_gateway_retry() {
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::failure(ProviderAttemptError {
                reason: ModelCallErrorReason::RateLimited,
                retry_after: Some(Duration::from_secs(3)),
                delivery: ProviderRequestDeliveryState::RejectedBeforeExecution,
            }),
            ScriptedProviderScript::failure(ProviderAttemptError {
                reason: ModelCallErrorReason::ProviderUnavailable,
                retry_after: None,
                delivery: ProviderRequestDeliveryState::Unknown,
            }),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let request = request_for_model(model).await;

        let error = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.reason(), ModelCallErrorReason::RateLimited);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));

        let unknown_outcome = gateway
            .generate_model_turn(
                request_for_model(
                    gateway
                        .resolve_for_turn(
                            gateway.build_reload_candidate().await.unwrap(),
                            resolve_request(None),
                        )
                        .unwrap(),
                )
                .await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            unknown_outcome.reason(),
            ModelCallErrorReason::RequestOutcomeUnknown
        );
        assert_eq!(adapter.requests().len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unsafe_provider_delivery_never_preserves_a_retryable_reason() {
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::failure(ProviderAttemptError {
                reason: ModelCallErrorReason::RateLimited,
                retry_after: Some(Duration::from_secs(3)),
                delivery: ProviderRequestDeliveryState::Unknown,
            }),
            ScriptedProviderScript::failure(ProviderAttemptError {
                reason: ModelCallErrorReason::RateLimited,
                retry_after: Some(Duration::from_secs(3)),
                delivery: ProviderRequestDeliveryState::AcceptedNoOutput,
            }),
            ScriptedProviderScript::failure(ProviderAttemptError {
                reason: ModelCallErrorReason::RateLimited,
                retry_after: Some(Duration::from_secs(3)),
                delivery: ProviderRequestDeliveryState::OutputStarted,
            }),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![text_definition(
            1, 4_096, provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let catalog = gateway.initialize().await.unwrap();

        for expected in [
            ModelCallErrorReason::RequestOutcomeUnknown,
            ModelCallErrorReason::RequestOutcomeUnknown,
            ModelCallErrorReason::StreamInterrupted,
        ] {
            let model = gateway
                .resolve_for_turn(Arc::clone(&catalog), resolve_request(None))
                .unwrap();
            let error = gateway
                .generate_model_turn(
                    request_for_model(model).await,
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();

            assert_eq!(error.reason(), expected);
            assert_eq!(error.retry_after(), None);
        }
        assert_eq!(adapter.requests().len(), 3);
    }

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

    pub(super) fn structured_schema(json: &str) -> BoundedJsonSchema {
        json.parse().unwrap()
    }

    fn bounded_object(json: &str) -> BoundedJsonObject {
        json.parse().unwrap()
    }

    pub(super) fn structured_definition(
        version: u64,
        default_max_output_tokens: u32,
        max_schema_bytes: NonZeroU32,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> ModelDefinition {
        structured_definition_with_credential(
            version,
            default_max_output_tokens,
            max_schema_bytes,
            adapter,
            "test-credential",
        )
    }

    pub(super) fn structured_definition_with_credential(
        version: u64,
        default_max_output_tokens: u32,
        max_schema_bytes: NonZeroU32,
        adapter: Arc<dyn ProviderAdapter>,
        credential: &str,
    ) -> ModelDefinition {
        ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(version).unwrap()),
            "gpt-5".parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true)
                .with_structured_json_schema(),
            EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192))
                .with_max_schema_bytes(max_schema_bytes),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(default_max_output_tokens).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            adapter,
            fixed_credential_source(credential),
        )
        .unwrap()
    }

    fn text_definition_for_selection(
        selection: ModelSelection,
        version: u64,
        default_max_output_tokens: u32,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> ModelDefinition {
        ModelDefinition::new(
            selection.clone(),
            ModelDefinitionVersion::new(NonZeroU64::new(version).unwrap()),
            selection.model_id().as_str().parse().unwrap(),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192)),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(default_max_output_tokens).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            adapter,
            fixed_credential_source("test-credential"),
        )
        .unwrap()
    }

    pub(super) async fn structured_request(
        model: &Arc<TurnModelSnapshot>,
        contract: &StructuredOutputContract,
    ) -> Arc<ModelCallRequest> {
        request_with_output_contract(model, Some(&OutputContract::Structured(contract.clone())))
            .await
    }

    /// Assembles a real AgentRun request with the given output contract (Structured or
    /// NoToolCalls) and an empty tool set, through the exact prompt assembly path.
    pub(super) async fn request_with_output_contract(
        model: &Arc<TurnModelSnapshot>,
        output_contract: Option<&OutputContract>,
    ) -> Arc<ModelCallRequest> {
        let prompt_set = prompt_set_for_model(Arc::clone(model)).await;
        let (live, revision) = live_user_context(&prompt_set);
        let views = live.capture_conversation_views().unwrap();
        let input = Arc::new(
            prompt_set
                .assemble(PromptAssemblyInput::agent_run(
                    views.conversation(),
                    output_contract,
                ))
                .unwrap(),
        );
        Arc::new(
            ModelCallRequest::new(
                Arc::clone(model),
                ModelCallPurpose::AgentRun,
                input,
                revision,
                None,
            )
            .unwrap(),
        )
    }

    #[test]
    fn structured_output_contract_rejects_invalid_or_oversize_names() {
        let model =
            TurnModelSnapshot::test_fixture_with_structured(None, NonZeroU32::new(65_536).unwrap());
        let schema = structured_schema(r#"{"type":"object"}"#);
        for name in [
            "",
            "has/slash",
            "has space",
            "has\"quote",
            "has\\backslash",
            "unicode-\u{00e9}",
        ] {
            assert_eq!(
                StructuredOutputContract::new(&model, Some(name), schema.clone()).unwrap_err(),
                StructuredOutputContractError::InvalidName,
                "name {name:?} was accepted"
            );
        }
        let oversize = "x".repeat(65);
        assert_eq!(
            StructuredOutputContract::new(&model, Some(&oversize), schema.clone()).unwrap_err(),
            StructuredOutputContractError::InvalidName
        );
        let boundary = "x".repeat(64);
        let contract = StructuredOutputContract::new(&model, Some(&boundary), schema).unwrap();
        assert_eq!(contract.name(), Some(boundary.as_str()));
        assert!(!format!("{contract:?}").contains("xxx"));
    }

    #[test]
    fn structured_output_contract_fails_closed_on_unsupported_model_and_schema_cap() {
        let text_model = TurnModelSnapshot::test_fixture(None);
        assert_eq!(
            StructuredOutputContract::new(
                &text_model,
                None,
                structured_schema(r#"{"type":"object"}"#),
            )
            .unwrap_err(),
            StructuredOutputContractError::UnsupportedModel
        );

        let tight_schema = structured_schema(r#"{"type":"object"}"#);
        let cap =
            NonZeroU32::new(u32::try_from(tight_schema.canonical_bytes().len()).unwrap()).unwrap();
        let boundary_model = TurnModelSnapshot::test_fixture_with_structured(None, cap);
        assert!(
            StructuredOutputContract::new(&boundary_model, None, tight_schema).is_ok(),
            "a schema exactly at the model cap must be accepted"
        );
        let over_schema = structured_schema(r#"{"type":"object","description":"x"}"#);
        assert_eq!(
            StructuredOutputContract::new(&boundary_model, None, over_schema).unwrap_err(),
            StructuredOutputContractError::SchemaTooLarge
        );

        let roomy_model =
            TurnModelSnapshot::test_fixture_with_structured(None, NonZeroU32::new(65_536).unwrap());
        let contract = StructuredOutputContract::new(
            &roomy_model,
            None,
            structured_schema(r#"{"type":"object","description":"SECRET-DESCRIPTION"}"#),
        )
        .unwrap();
        assert!(!format!("{contract:?}").contains("SECRET-DESCRIPTION"));
    }

    #[test]
    fn structured_schema_subset_fails_closed_on_unsupported_keywords_and_roots() {
        let model =
            TurnModelSnapshot::test_fixture_with_structured(None, NonZeroU32::new(65_536).unwrap());
        for schema_json in [
            r#"{"type":"string"}"#,
            r#"{}"#,
            r#"{"type":"object","properties":{"a":{"type":"tuple"}}}"#,
            r##"{"type":"object","$ref":"#"}"##,
            r#"{"type":"object","$defs":{}}"#,
            r#"{"type":"object","pattern":"x"}"#,
            r#"{"type":"object","anyOf":[{"type":"string"}]}"#,
            r#"{"type":"object","allOf":[]}"#,
            r#"{"type":"object","minLength":1}"#,
            r#"{"type":"object","format":"date"}"#,
            r#"{"type":"object","$schema":"https://json-schema.org/draft/2020-12/other"}"#,
            r#"{"type":"object","$schema":"https://json-schema.org/draft/07/schema"}"#,
            r#"{"type":"object","$schema":"https://json-schema.org/draft/2020-12/schema","description":1}"#,
            r#"{"type":"object","properties":{"a":{"$schema":"https://json-schema.org/draft/2020-12/schema"}}}"#,
            r#"{"type":"object","properties":[]}"#,
            r#"{"type":"object","properties":{"a":"string"}}"#,
            r##"{"type":"object","properties":{"a":{"type":"string"},"b":{"$ref":"#/a"}}}"##,
            r#"{"type":"object","required":"a"}"#,
            r#"{"type":"object","required":[1]}"#,
            r#"{"type":"object","required":["a","a"]}"#,
            r#"{"type":"object","additionalProperties":{"type":"boolean"}}"#,
            r#"{"type":"object","additionalProperties":"false"}"#,
            r#"{"type":"object","items":[{"type":"string"}]}"#,
            r#"{"type":"object","items":"x"}"#,
            r#"{"type":"object","enum":[]}"#,
            r#"{"type":"object","enum":[1,1]}"#,
            r#"{"type":"object","enum":"x"}"#,
        ] {
            assert_eq!(
                StructuredOutputContract::new(&model, None, structured_schema(schema_json))
                    .unwrap_err(),
                StructuredOutputContractError::UnsupportedSchema,
                "schema {schema_json} was accepted"
            );
        }
        // A non-object root is rejected by the bounded schema constructor before the contract.
        assert!("42".parse::<BoundedJsonSchema>().is_err());
        assert!("[]".parse::<BoundedJsonSchema>().is_err());
    }

    #[test]
    fn structured_schema_subset_accepts_nested_objects_arrays_enum_and_const() {
        let model =
            TurnModelSnapshot::test_fixture_with_structured(None, NonZeroU32::new(65_536).unwrap());
        let contract = StructuredOutputContract::new(
            &model,
            Some("report"),
            structured_schema(
                r#"{
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "description": "top level",
                    "required": ["status", "tags"],
                    "additionalProperties": false,
                    "properties": {
                        "status": {"type": "string", "enum": ["ok", "pending"]},
                        "kind": {"type": "string", "const": "report"},
                        "count": {"type": "integer"},
                        "ratio": {"type": "number"},
                        "flag": {"type": "boolean"},
                        "nothing": {"type": "null"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "meta": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {"nested": {"type": "array", "items": {"type": "integer"}}}
                        }
                    }
                }"#,
            ),
        )
        .unwrap();
        assert_eq!(contract.name(), Some("report"));

        for valid in [
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":null,"tags":["a","b"],"meta":{"nested":[1,2]}}"#,
            // Integer is judged by canonical decimal scale: 1.0 becomes 1, 1e2 becomes 100,
            // and large-magnitude 1e30 remains scientific but still has no fractional part.
            r#"{"status":"pending","kind":"report","count":1.0,"ratio":-0.5,"flag":false,"nothing":null,"tags":[],"meta":{"nested":[1e2,1e30]}}"#,
        ] {
            assert!(
                contract.validate_instance(&bounded_object(valid)),
                "instance {valid} should validate"
            );
        }

        for invalid in [
            r#"{"status":"ok","kind":"report","count":"2","ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{}}"#,
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":"true","nothing":null,"tags":[],"meta":{}}"#,
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":"x","tags":[],"meta":{}}"#,
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":null,"tags":[1],"meta":{}}"#,
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{"nested":[1.5]}}"#,
            r#"{"status":"ok","kind":"report","count":1.5,"ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{}}"#,
            r#"{"status":"ok","kind":"report","count":1e-30,"ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{}}"#,
            r#"{"status":"unknown","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{}}"#,
            r#"{"status":"ok","kind":"other","count":2,"ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{}}"#,
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":null,"meta":{}}"#,
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{},"extra":1}"#,
            r#"{"status":"ok","kind":"report","count":2,"ratio":1.5,"flag":true,"nothing":null,"tags":[],"meta":{"nested":[1],"extra":2}}"#,
        ] {
            assert!(
                !contract.validate_instance(&bounded_object(invalid)),
                "instance {invalid} should be rejected"
            );
        }
        assert!(!format!("{contract:?}").contains("top level"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn structured_output_end_to_end_validates_exact_terminal_json() {
        let schema = structured_schema(
            r#"{"type":"object","required":["summary"],"additionalProperties":false,"properties":{"summary":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}}}}"#,
        );
        let terminal = |text: &str| ProviderAttemptResult {
            response_id: None,
            content: Arc::from([ProviderAttemptContent::Text(Arc::from(text))]),
            finish_reason: ModelFinishReason::Stop,
            usage: None,
            metadata: ProviderResponseMetadata::new(None, None, None),
        };
        let scripts = vec![
            ScriptedProviderScript::success(
                Vec::new(),
                terminal(r#"{"summary":"SECRET hello","tags":["a","b"]}"#),
            ),
            ScriptedProviderScript::success(Vec::new(), terminal("not json at all")),
            ScriptedProviderScript::success(
                Vec::new(),
                terminal("```json\n{\"summary\":\"fenced\"}\n```"),
            ),
            ScriptedProviderScript::success(Vec::new(), terminal("42")),
            ScriptedProviderScript::success(Vec::new(), terminal(r#"{"summary":42}"#)),
            ScriptedProviderScript::success(Vec::new(), terminal(r#"{"tags":[]}"#)),
            ScriptedProviderScript::success(Vec::new(), terminal(r#"{"summary":"x","extra":1}"#)),
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([
                        ProviderAttemptContent::Text(Arc::from(r#"{"summary":"first"}"#)),
                        ProviderAttemptContent::Text(Arc::from("SECRET second")),
                    ]),
                    finish_reason: ModelFinishReason::Stop,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
        ];
        let adapter = Arc::new(ScriptedProviderAdapter::new(scripts));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![structured_definition(
            1,
            4_096,
            NonZeroU32::new(65_536).unwrap(),
            provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let contract = StructuredOutputContract::new(&model, Some("weather"), schema).unwrap();
        let request = structured_request(&model, &contract).await;

        let result = gateway
            .generate_model_turn(
                Arc::clone(&request),
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, r#"{"summary":"SECRET hello","tags":["a","b"]}"#);
            }
            _ => panic!("structured success changed content family"),
        }

        for _ in 0..7 {
            let error = gateway
                .generate_model_turn(
                    Arc::clone(&request),
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.reason(),
                ModelCallErrorReason::InvalidStructuredOutput
            );
        }

        let attempts = adapter.requests();
        assert_eq!(attempts.len(), 8);
        for attempt in &attempts {
            assert!(Arc::ptr_eq(attempt, &request));
            assert_eq!(
                attempt.input().output_contract(),
                Some(&OutputContract::Structured(contract.clone()))
            );
        }
        for debug in [
            format!("{contract:?}"),
            format!("{request:?}"),
            format!("{result:?}"),
        ] {
            assert!(!debug.contains("SECRET"));
            assert!(!debug.contains("weather"));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refused_nonempty_structured_response_bypasses_schema_validation() {
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::Text(Arc::from(
                        "I cannot produce structured JSON",
                    ))]),
                    finish_reason: ModelFinishReason::Refused,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![structured_definition(
            1,
            4_096,
            NonZeroU32::new(65_536).unwrap(),
            provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let contract =
            StructuredOutputContract::new(&model, None, structured_schema(r#"{"type":"object"}"#))
                .unwrap();
        let request = structured_request(&model, &contract).await;

        let result = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.response().finish_reason(),
            ModelFinishReason::Refused
        );
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, "I cannot produce structured JSON");
            }
            _ => panic!("refused response changed content family"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn structured_contract_tool_call_is_unexpected() {
        let adapter = Arc::new(ScriptedProviderAdapter::new(vec![
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::ToolCall {
                        tool_call_id: "call_1".parse().unwrap(),
                        name: "echo".parse().unwrap(),
                        arguments: "{}".parse().unwrap(),
                    }]),
                    finish_reason: ModelFinishReason::ToolCalls,
                    usage: None,
                    metadata: ProviderResponseMetadata::new(None, None, None),
                },
            ),
        ]));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let source = Arc::new(MutableModelSource::new(vec![structured_definition(
            1,
            4_096,
            NonZeroU32::new(65_536).unwrap(),
            provider,
        )]));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = ModelGateway::new(vec![source_adapter]);
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let contract =
            StructuredOutputContract::new(&model, None, structured_schema(r#"{"type":"object"}"#))
                .unwrap();
        let request = structured_request(&model, &contract).await;

        let error = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason(), ModelCallErrorReason::UnexpectedToolCall);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn structured_contract_cannot_cross_the_exact_model_boundary() {
        let a_adapter: Arc<dyn ProviderAdapter> =
            Arc::new(ScriptedProviderAdapter::new(Vec::new()));
        let a_source = Arc::new(MutableModelSource::new(vec![structured_definition(
            1,
            4_096,
            NonZeroU32::new(65_536).unwrap(),
            a_adapter,
        )]));
        let a_source_adapter: Arc<dyn ModelSourceAdapter> = a_source;
        let a_gateway = ModelGateway::new(vec![a_source_adapter]);
        let a_model = a_gateway
            .resolve_for_turn(a_gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        let contract = StructuredOutputContract::new(
            &a_model,
            Some("weather"),
            structured_schema(r#"{"type":"object"}"#),
        )
        .unwrap();

        let b_selection =
            ModelSelection::new("anthropic".parse().unwrap(), "claude".parse().unwrap());
        let b_adapter: Arc<dyn ProviderAdapter> =
            Arc::new(ScriptedProviderAdapter::new(Vec::new()));
        let b_source = Arc::new(MutableModelSource::new(vec![
            text_definition_for_selection(b_selection.clone(), 1, 4_096, b_adapter),
        ]));
        let b_source_adapter: Arc<dyn ModelSourceAdapter> = b_source;
        let b_gateway = ModelGateway::new(vec![b_source_adapter]);
        let b_model = b_gateway
            .resolve_for_turn(
                b_gateway.initialize().await.unwrap(),
                ResolveTurnModelRequest::new(b_selection, ReasoningPreference::Auto, None),
            )
            .unwrap();

        let b_prompt_set = prompt_set_for_model(Arc::clone(&b_model)).await;
        let (live, revision) = live_user_context(&b_prompt_set);
        let views = live.capture_conversation_views().unwrap();
        let output_contract = OutputContract::Structured(contract.clone());
        let input = Arc::new(
            b_prompt_set
                .assemble(PromptAssemblyInput::agent_run(
                    views.conversation(),
                    Some(&output_contract),
                ))
                .unwrap(),
        );
        let error =
            ModelCallRequest::new(b_model, ModelCallPurpose::AgentRun, input, revision, None)
                .unwrap_err();
        assert_eq!(
            error.kind(),
            ModelRequestValidationErrorKind::AssemblyMismatch
        );

        let a_request = structured_request(&a_model, &contract).await;
        assert_eq!(
            a_request.input().output_contract(),
            Some(&OutputContract::Structured(contract))
        );
    }
}
