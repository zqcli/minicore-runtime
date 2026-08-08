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
use crate::wire::{BoundedJsonObject, Money};

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
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveModelLimits {
    context_window_tokens: Option<NonZeroU32>,
    max_output_tokens: Option<NonZeroU32>,
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
        }
    }

    pub(crate) const fn max_output_tokens(self) -> Option<NonZeroU32> {
        self.max_output_tokens
    }

    pub(crate) const fn context_window_tokens(self) -> Option<NonZeroU32> {
        self.context_window_tokens
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
    identity: Arc<TurnModelIdentity>,
    capabilities: ModelCapabilities,
    limits: EffectiveModelLimits,
    token_estimate_rate: TokenEstimateRate,
    generation: ModelGenerationDefaults,
    adapter: Arc<dyn ProviderAdapter>,
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
        capabilities: ModelCapabilities,
        limits: EffectiveModelLimits,
        token_estimate_rate: TokenEstimateRate,
        generation: ModelGenerationDefaults,
        adapter: Arc<dyn ProviderAdapter>,
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
            identity: Arc::new(TurnModelIdentity),
            capabilities,
            limits,
            token_estimate_rate,
            generation,
            adapter,
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
    reason = "the only current variant is produced by concrete ModelSourceAdapter implementations"
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ModelSourceError {
    #[error("model definition source is unavailable")]
    Unavailable,
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
        let owner = Arc::new(ModelGatewayOwner);
        let identity = Arc::new(TurnModelIdentity);
        let limits = EffectiveModelLimits::new(context_window_tokens, NonZeroU32::new(8_192));
        let definition = ModelDefinition::new(
            ModelSelection::new("fixture".parse().unwrap(), "fixture".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            limits,
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(4_096).unwrap(),
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
            ),
            Arc::new(ScriptedProviderAdapter::new(Vec::new())),
        )
        .unwrap();
        Arc::new(Self {
            owner: Arc::clone(&owner),
            turn_ref: TurnModelRef { owner, identity },
            definition: definition.reference(),
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
            for definition in source.discover().await.map_err(|_| {
                ModelResolutionError::new(ModelResolutionErrorKind::SourceUnavailable)
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

        let attempt = ProviderAttemptRequest {
            effective_max_output_tokens: request.effective_max_output_tokens(),
            call: Arc::clone(&request),
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
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "constructed by the adjacent M7 AgentRun assembly")
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OutputContract {
    NoToolCalls,
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
        if input.output_contract().is_some() && !input.tools_empty() {
            return Err(ModelRequestValidationError::new(
                ModelRequestValidationErrorKind::UnsupportedInput,
            ));
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
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by the adjacent M7 provider adapter")
)]
impl ProviderAttemptRequest {
    const fn call(&self) -> &Arc<ModelCallRequest> {
        &self.call
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
        let scripts = vec![
            ScriptedProviderScript::success(
                Vec::new(),
                ProviderAttemptResult {
                    response_id: None,
                    content: Arc::from([ProviderAttemptContent::ToolCall {
                        tool_call_id: tool_call_id.parse().unwrap(),
                        name: tool_name.parse().unwrap(),
                        arguments: arguments.parse().unwrap(),
                    }]),
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

    fn from_scripts(scripts: Vec<ScriptedProviderScript>, context_window_tokens: u32) -> Self {
        let adapter = Arc::new(ScriptedProviderAdapter::new(scripts));
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let definition = ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
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

    pub(crate) const fn gateway(&self) -> &Arc<ModelGateway> {
        &self.gateway
    }

    pub(crate) const fn catalog(&self) -> &Arc<ModelCatalogView> {
        &self.catalog
    }

    pub(crate) fn request_count(&self) -> usize {
        self.adapter.requests().len()
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
        }
    }

    pub(crate) const fn reason(&self) -> ModelCallErrorReason {
        self.reason
    }

    pub(crate) const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

impl fmt::Debug for ModelCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCallError")
            .field("reason", &self.reason)
            .field("retry_after", &self.retry_after)
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
                validate_artifact(&text, 65_536).map_err(|_| {
                    ModelCallError::new(ModelCallErrorReason::InvalidProviderResponse)
                })?;
                has_visible_text = true;
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

fn validate_artifact(value: &str, maximum: usize) -> Result<(), ModelValueError> {
    validate_safe_text(value, maximum, false).map_err(|_| ModelValueError::InvalidArtifact)
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::Mutex;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_session_lifecycle::AgentRevisionRef;
    use crate::conversation_storage::StoredUserMessage;
    use crate::live_conversation::LiveSessionState;
    use crate::prompt::{
        AgentPromptSelection, ModelMessageRef, PromptAssemblyInput, PromptBodyIntent,
        PromptErrorKind, PromptIntent, PromptService, PromptTurnContext, SessionPromptSelection,
        TextIntent,
    };
    use crate::skills::SkillView;
    use crate::tools::{ToolDefinition, ToolExecutionMode, ToolExecutionResult, ToolSet};
    use crate::turn_item_interaction::UserMessageSource;
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

    fn text_definition(
        version: u64,
        default_max_output_tokens: u32,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> ModelDefinition {
        text_definition_with_context_limit(version, default_max_output_tokens, 128_000, adapter)
    }

    fn text_definition_with_context_limit(
        version: u64,
        default_max_output_tokens: u32,
        context_window_tokens: u32,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> ModelDefinition {
        ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(version).unwrap()),
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
        )
        .unwrap()
    }

    fn resolve_request(max_output_tokens: Option<u32>) -> ResolveTurnModelRequest {
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

    fn scripted_tool_set() -> Arc<ToolSet> {
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

    async fn request_for_model(model: Arc<TurnModelSnapshot>) -> Arc<ModelCallRequest> {
        request_for_model_with_tools(model, ToolSet::empty()).await
    }

    async fn request_for_model_with_tools(
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
}
