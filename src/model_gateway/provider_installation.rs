//! M14 slice: host-only provider/model installation configuration.
//!
//! Public host-only types for installing the direct provider adapters into a
//! `MiniCoreRuntime` with no Wire, durable, or live-network involvement:
//!
//! - `ProviderCredential`: the validated owned credential value (printable ASCII,
//!   non-empty, at most 256 bytes). Debug always redacts; there is no Display,
//!   Serialize, Eq/Hash, or public revealing getter. The only reader is the
//!   gateway, which hands the resolved credential to the direct adapters for
//!   header injection.
//! - `CredentialSource`: the dynamic per-attempt credential seam. The gateway
//!   resolves it on every `generate_model_turn` attempt, cancellation-aware,
//!   before any provider adapter executes. `None` means the credential is
//!   missing (typed `AuthMissing`/`NotSent`); the source itself never caches,
//!   singleflights, refreshes, or resends.
//! - `ModelProviderDescriptor`: the provider-neutral model descriptor. Every
//!   capability is explicit and validated; conservative defaults are Provider
//!   default reasoning only, Standard service class, and no structured output.
//!   Nothing is ever inferred from model names.
//! - `ProviderEndpointPolicy`: explicit endpoint policy — HTTPS-only, with a
//!   separate explicit allow-loopback-HTTP policy for tests/development where
//!   only numeric loopback IPs (`127.0.0.1/8`, `::1`) are acceptable.
//! - `ModelProviderConfig`: one validated provider installation. The route
//!   constructors are pure validated config creation: they validate
//!   endpoint/version/model list and return closed payload-free errors, and they
//!   never build a reqwest client or adapter. `MiniCoreRuntime::open` calls the
//!   crate-private `build_source()` on each installation, which materializes
//!   exactly one direct adapter (sharing one locked client) and one static
//!   `ModelSourceAdapter` sharing one credential source across all its model
//!   definitions. `discover()` is local/static and never performs network or
//!   credential resolution.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;

use crate::model_gateway::anthropic_messages::{
    AnthropicMessagesProviderAdapter, AnthropicProviderConfigError,
};
use crate::model_gateway::openai_responses::{
    OpenAiProviderConfigError, OpenAiResponsesProviderAdapter,
};
use crate::model_gateway::{
    ApiModelName, EffectiveModelLimits, ModelCapabilities, ModelDefinition, ModelDefinitionVersion,
    ModelGenerationDefaults, ModelReasoningSummary, ModelSelection, ModelServiceClass,
    ModelSourceAdapter, ModelSourceFuture, ReasoningCapabilities, TokenEstimateRate,
};
use crate::wire::lexical::validate_opaque_ascii;

/// Typed, payload-free credential validation failure. The rejected value is never
/// stored, so Debug/Display can never leak it.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ProviderCredentialError {
    #[error("provider credential must be non-empty printable ASCII within 256 bytes")]
    Invalid,
}

/// A validated owned provider credential value: non-empty printable ASCII, at most
/// 256 bytes. Debug always redacts the value; there is deliberately no Display,
/// Serialize, Eq/Hash, or public getter that reveals it. The only reader is the
/// gateway's per-attempt header injection into the direct provider adapters
/// (crate-private).
#[derive(Clone)]
pub struct ProviderCredential(Box<str>);

impl FromStr for ProviderCredential {
    type Err = ProviderCredentialError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_opaque_ascii(value, 256).map_err(|_| ProviderCredentialError::Invalid)?;
        Ok(Self(value.into()))
    }
}

impl ProviderCredential {
    /// Crate-private: hands the opaque secret to the direct provider adapters for
    /// header injection. Never part of the public API.
    pub(crate) fn for_header(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderCredential(<redacted>)")
    }
}

/// The resolved-credential future of one credential-source resolution attempt.
pub type CredentialSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Option<ProviderCredential>> + Send + 'a>>;

/// The dynamic per-attempt credential source seam. The gateway resolves it on
/// every `generate_model_turn` attempt, cancellation-aware, before any provider
/// adapter executes; `None` means no usable credential is currently available and
/// maps to a typed `AuthMissing` failure that is always `NotSent` — the adapter is
/// never invoked and the gateway never retries on its own. The source is
/// host-owned and intentionally free of caching, singleflight, refresh-and-resend,
/// and connection caching.
///
/// Contract obligations on `resolve()`:
/// - it must only construct and return its future: it must not block, and it must
///   not do any resolution work synchronously;
/// - the returned future must own all resolution work: it must not detach any task
///   that outlives the future, and dropping the future (which the gateway does on
///   cancellation) must stop all owner-visible work — no background refresh, no
///   detached I/O, no lingering locks or timers.
pub trait CredentialSource: Send + Sync {
    fn resolve(&self) -> CredentialSourceFuture<'_>;
}

/// Explicit reasoning-level support of one installed model. Conservative default:
/// `none()` supports only the provider default; levels are never inferred from
/// model names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelReasoningSupport {
    disabled: bool,
    low: bool,
    medium: bool,
    high: bool,
}

impl ModelReasoningSupport {
    pub const fn none() -> Self {
        Self {
            disabled: false,
            low: false,
            medium: false,
            high: false,
        }
    }

    pub const fn all() -> Self {
        Self {
            disabled: true,
            low: true,
            medium: true,
            high: true,
        }
    }

    pub const fn with_disabled(mut self) -> Self {
        self.disabled = true;
        self
    }

    pub const fn with_low(mut self) -> Self {
        self.low = true;
        self
    }

    pub const fn with_medium(mut self) -> Self {
        self.medium = true;
        self
    }

    pub const fn with_high(mut self) -> Self {
        self.high = true;
        self
    }
}

/// Typed, payload-free descriptor validation failure. Rejected values are never
/// stored, so Debug/Display can never leak them.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelProviderDescriptorError {
    #[error("API model name must be non-empty printable opaque ASCII within 256 bytes")]
    InvalidApiModelName,
}

/// The provider-neutral model descriptor of one installed model definition.
///
/// The stable `ModelSelection` is the durable identity; the explicit validated
/// `api_model_name` is the private provider wire name and can differ from it.
/// The definition revision/version is explicit and non-zero. Optional limits and
/// capabilities default conservatively: Provider default reasoning only,
/// Standard service class, no structured output, no inferred capability from the
/// model name. Field-level validation (default output within the model max,
/// reasoning default within the declared support) happens when an installation
/// is constructed.
///
/// Version obligation: the host-supplied `version` must change whenever any
/// nonsecret calling semantics of the definition change — protocol, endpoint,
/// API model name, capabilities, limits, defaults, or the credential binding
/// identity. Rotating the credential contents (a new token for the same binding
/// identity) never changes the version. The runtime never computes a version or
/// fingerprint: it stores and reports the host-supplied value exactly.
pub struct ModelProviderDescriptor {
    selection: ModelSelection,
    version: NonZeroU64,
    api_model_name: ApiModelName,
    context_window_tokens: Option<NonZeroU32>,
    max_output_tokens: Option<NonZeroU32>,
    default_max_output_tokens: NonZeroU32,
    bytes_per_token: NonZeroU32,
    reasoning_support: ModelReasoningSupport,
    reasoning_default: ModelReasoningSummary,
    service_class: ModelServiceClass,
    structured_output: bool,
    structured_json_schema_max_bytes: Option<NonZeroU32>,
}

impl Clone for ModelProviderDescriptor {
    fn clone(&self) -> Self {
        Self {
            selection: self.selection.clone(),
            version: self.version,
            api_model_name: self.api_model_name.clone(),
            context_window_tokens: self.context_window_tokens,
            max_output_tokens: self.max_output_tokens,
            default_max_output_tokens: self.default_max_output_tokens,
            bytes_per_token: self.bytes_per_token,
            reasoning_support: self.reasoning_support,
            reasoning_default: self.reasoning_default,
            service_class: self.service_class,
            structured_output: self.structured_output,
            structured_json_schema_max_bytes: self.structured_json_schema_max_bytes,
        }
    }
}

impl ModelProviderDescriptor {
    pub fn new(
        selection: ModelSelection,
        version: NonZeroU64,
        api_model_name: &str,
        default_max_output_tokens: NonZeroU32,
        bytes_per_token: NonZeroU32,
    ) -> Result<Self, ModelProviderDescriptorError> {
        let api_model_name = api_model_name
            .parse()
            .map_err(|_| ModelProviderDescriptorError::InvalidApiModelName)?;
        Ok(Self {
            selection,
            version,
            api_model_name,
            context_window_tokens: None,
            max_output_tokens: None,
            default_max_output_tokens,
            bytes_per_token,
            reasoning_support: ModelReasoningSupport::none(),
            reasoning_default: ModelReasoningSummary::ProviderDefault,
            service_class: ModelServiceClass::Standard,
            structured_output: false,
            structured_json_schema_max_bytes: None,
        })
    }

    pub fn with_context_window_tokens(mut self, tokens: NonZeroU32) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: NonZeroU32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    /// Declares the exact reasoning support and its default level. The default must
    /// be within the support set, or the installation that consumes this
    /// descriptor is rejected.
    pub fn with_reasoning(
        mut self,
        support: ModelReasoningSupport,
        default: ModelReasoningSummary,
    ) -> Self {
        self.reasoning_support = support;
        self.reasoning_default = default;
        self
    }

    pub fn with_service_class(mut self, service_class: ModelServiceClass) -> Self {
        self.service_class = service_class;
        self
    }

    /// Opts the model into structured JSON-schema output. `None` means the model
    /// supports structured output at the protocol capability only (no model-specific
    /// schema cap); `Some(max_schema_bytes)` additionally binds an explicit
    /// model-specific canonical schema byte cap. Conservative default: no
    /// structured output support.
    pub fn with_structured_json_schema(mut self, max_schema_bytes: Option<NonZeroU32>) -> Self {
        self.structured_output = true;
        self.structured_json_schema_max_bytes = max_schema_bytes;
        self
    }

    /// Validates the field-level constraints that do not depend on any provider
    /// adapter: the default output must fit the model max, and the reasoning
    /// default must be within the declared support. The constructor runs this
    /// purely; `build_source()` re-runs the same constraints via
    /// `ModelDefinition::new` as defense in depth.
    fn validate(&self) -> Result<(), ModelProviderConfigError> {
        if self
            .max_output_tokens
            .is_some_and(|maximum| self.default_max_output_tokens > maximum)
            || !descriptor_reasoning_is_supported(self.reasoning_default, self.reasoning_support)
        {
            return Err(ModelProviderConfigError::InvalidModelDescriptor);
        }
        Ok(())
    }
}

fn descriptor_reasoning_is_supported(
    reasoning: ModelReasoningSummary,
    support: ModelReasoningSupport,
) -> bool {
    match reasoning {
        ModelReasoningSummary::ProviderDefault => true,
        ModelReasoningSummary::Disabled => support.disabled,
        ModelReasoningSummary::Low => support.low,
        ModelReasoningSummary::Medium => support.medium,
        ModelReasoningSummary::High => support.high,
    }
}

impl fmt::Debug for ModelProviderDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The API model name is a private provider wire name and never prints.
        formatter
            .debug_struct("ModelProviderDescriptor")
            .field("selection", &self.selection)
            .field("version", &self.version)
            .field("context_window_tokens", &self.context_window_tokens)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("default_max_output_tokens", &self.default_max_output_tokens)
            .field("bytes_per_token", &self.bytes_per_token)
            .field("reasoning_support", &self.reasoning_support)
            .field("reasoning_default", &self.reasoning_default)
            .field("service_class", &self.service_class)
            .field("structured_output", &self.structured_output)
            .field(
                "structured_json_schema_max_bytes",
                &self.structured_json_schema_max_bytes,
            )
            .finish()
    }
}

/// Explicit endpoint policy for one provider installation. Production defaults to
/// HTTPS-only; the separate allow-loopback-HTTP policy exists for tests and
/// development and accepts HTTP only for numeric loopback IPs
/// (`127.0.0.0/8` or `::1`) — never arbitrary HTTP hosts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderEndpointPolicy {
    HttpsOnly,
    AllowLoopbackHttp,
}

/// Typed, payload-free installation configuration failure. Rejected endpoints,
/// versions, and descriptor details are never stored, so Debug/Display can never
/// leak them. This is the closed public config-validation taxonomy of the pure
/// route constructors: it has no client/adapter-build variant because the
/// constructors never build one (materialization failures are the crate-private
/// `ProviderSourceBuildError` surfaced at `MiniCoreRuntime::open`).
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelProviderConfigError {
    #[error("provider endpoint violates the explicit endpoint policy")]
    InvalidEndpoint,
    #[error("Anthropic version header must be non-empty printable ASCII within 64 bytes")]
    InvalidVersion,
    #[error("a provider installation requires at least one model descriptor")]
    EmptyModelList,
    #[error("a provider installation contains a duplicate stable model selection")]
    DuplicateModelSelection,
    #[error("a model descriptor is invalid for this installation")]
    InvalidModelDescriptor,
}

/// Typed, payload-free source-materialization failure: the direct adapter/client
/// could not be built (a runtime dependency), or the stored validated descriptors
/// failed to convert into definitions (a configuration error that the pure
/// constructor is expected to have already rejected). Crate-private: only
/// `MiniCoreRuntime::open` consumes it.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ProviderSourceBuildError {
    #[error("provider HTTP client construction failed")]
    ClientBuild,
    #[error("a stored model descriptor is invalid for this installation")]
    InvalidDefinition,
}

/// The stored route of one validated installation. Private and never printed by
/// `ModelProviderConfig` Debug: the endpoint is redacted, and only the protocol
/// (plus the Anthropic public version metadata, which the adapter Debug also
/// prints) is ever visible.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ProviderRoute {
    OpenAiResponses {
        endpoint: Box<str>,
    },
    AnthropicMessages {
        endpoint: Box<str>,
        version: Box<str>,
    },
}

impl fmt::Debug for ProviderRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenAiResponses { .. } => formatter
                .debug_tuple("ProviderRoute::OpenAiResponses")
                .field(&"<redacted>")
                .finish(),
            Self::AnthropicMessages { version, .. } => formatter
                .debug_struct("ProviderRoute::AnthropicMessages")
                .field("endpoint", &"<redacted>")
                .field("version", &version.as_ref())
                .finish(),
        }
    }
}

/// One validated provider installation: a private redacted route spec (protocol,
/// endpoint, Anthropic version), the credential source, and the validated
/// descriptors. The route constructors are pure validated config creation — they
/// never build a reqwest client or adapter. `MiniCoreRuntime::open` calls
/// `build_source()`, which materializes exactly one direct adapter (sharing one
/// locked client) and one static `ModelSourceAdapter` sharing the credential
/// source across all model definitions; shared-resource reload re-runs
/// `discover()` on that same installed source and never rebuilds a client.
/// `discover()` is local/static and never performs network or credential
/// resolution. Debug is fully redacted: no endpoint, API model name, credential,
/// or source internals ever print.
pub struct ModelProviderConfig {
    route: ProviderRoute,
    credential_source: Arc<dyn CredentialSource>,
    descriptors: Vec<ModelProviderDescriptor>,
}

impl ModelProviderConfig {
    /// OpenAI Responses installation against an explicit full `/responses` endpoint
    /// with an explicit endpoint policy and credential source. Pure validation
    /// only; the adapter/client is built by `MiniCoreRuntime::open`.
    pub fn openai_responses(
        endpoint: &str,
        endpoint_policy: ProviderEndpointPolicy,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelProviderDescriptor>,
    ) -> Result<Self, ModelProviderConfigError> {
        validate_model_list(&models)?;
        validate_endpoint(endpoint, endpoint_policy)?;
        validate_descriptors(&models)?;
        Ok(Self {
            route: ProviderRoute::OpenAiResponses {
                endpoint: endpoint.into(),
            },
            credential_source,
            descriptors: models,
        })
    }

    /// Anthropic Messages installation against an explicit full `/v1/messages`
    /// endpoint with an explicit endpoint policy, explicit `anthropic-version`
    /// header value, and credential source. Pure validation only; the
    /// adapter/client is built by `MiniCoreRuntime::open`.
    pub fn anthropic_messages(
        endpoint: &str,
        endpoint_policy: ProviderEndpointPolicy,
        version: &str,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelProviderDescriptor>,
    ) -> Result<Self, ModelProviderConfigError> {
        validate_opaque_ascii(version, 64).map_err(|_| ModelProviderConfigError::InvalidVersion)?;
        validate_model_list(&models)?;
        validate_endpoint(endpoint, endpoint_policy)?;
        validate_descriptors(&models)?;
        Ok(Self {
            route: ProviderRoute::AnthropicMessages {
                endpoint: endpoint.into(),
                version: version.into(),
            },
            credential_source,
            descriptors: models,
        })
    }

    /// Crate-private materialization used exactly once per installation at
    /// `MiniCoreRuntime::open`: builds exactly one direct adapter (one locked
    /// client) and one static source holding the validated definitions. The
    /// returned source owns its adapter and client for the Runtime's lifetime;
    /// shared-resource reload re-runs `discover()` on the same source and never
    /// rebuilds a client. A client/adapter build failure is `ClientBuild`; a
    /// stored descriptor that fails conversion is `InvalidDefinition` (expected to
    /// be unreachable because the constructor already ran the same validation).
    pub(crate) fn build_source(
        &self,
    ) -> Result<Arc<dyn ModelSourceAdapter>, ProviderSourceBuildError> {
        let adapter: Arc<dyn crate::model_gateway::ProviderAdapter> = match &self.route {
            ProviderRoute::OpenAiResponses { endpoint } => Arc::new(
                OpenAiResponsesProviderAdapter::new(endpoint).map_err(|error| match error {
                    // Unreachable: the constructor ran the identical validation.
                    OpenAiProviderConfigError::InvalidEndpoint => {
                        ProviderSourceBuildError::InvalidDefinition
                    }
                    OpenAiProviderConfigError::ClientBuild => ProviderSourceBuildError::ClientBuild,
                })?,
            ),
            ProviderRoute::AnthropicMessages { endpoint, version } => Arc::new(
                AnthropicMessagesProviderAdapter::new(endpoint, version).map_err(|error| {
                    match error {
                        // Unreachable: the constructor ran the identical validation.
                        AnthropicProviderConfigError::InvalidEndpoint
                        | AnthropicProviderConfigError::InvalidVersion => {
                            ProviderSourceBuildError::InvalidDefinition
                        }
                        AnthropicProviderConfigError::ClientBuild => {
                            ProviderSourceBuildError::ClientBuild
                        }
                    }
                })?,
            ),
        };
        let definitions = self
            .descriptors
            .iter()
            .map(|descriptor| {
                descriptor.to_definition(Arc::clone(&adapter), Arc::clone(&self.credential_source))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ProviderSourceBuildError::InvalidDefinition)?;
        Ok(Arc::new(InstalledModelSource { definitions }))
    }
}

impl fmt::Debug for ModelProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelProviderConfig { .. }")
    }
}

impl ModelProviderDescriptor {
    fn to_definition(
        &self,
        adapter: Arc<dyn crate::model_gateway::ProviderAdapter>,
        credential_source: Arc<dyn CredentialSource>,
    ) -> Result<ModelDefinition, ModelProviderConfigError> {
        let reasoning = ReasoningCapabilities {
            disabled: self.reasoning_support.disabled,
            low: self.reasoning_support.low,
            medium: self.reasoning_support.medium,
            high: self.reasoning_support.high,
        };
        let mut capabilities = ModelCapabilities::text_only(reasoning, true);
        let mut limits =
            EffectiveModelLimits::new(self.context_window_tokens, self.max_output_tokens);
        if self.structured_output {
            capabilities = capabilities.with_structured_json_schema();
            if let Some(max_schema_bytes) = self.structured_json_schema_max_bytes {
                limits = limits.with_max_schema_bytes(max_schema_bytes);
            }
        }
        let token_estimate_rate = TokenEstimateRate::new(self.bytes_per_token, 1)
            .expect("the installation token-estimate algorithm version is the fixed v1");
        ModelDefinition::new(
            self.selection.clone(),
            ModelDefinitionVersion::new(self.version),
            self.api_model_name.clone(),
            capabilities,
            limits,
            token_estimate_rate,
            ModelGenerationDefaults::new(
                self.default_max_output_tokens,
                self.reasoning_default,
                self.service_class,
            ),
            adapter,
            credential_source,
        )
        .map_err(|_| ModelProviderConfigError::InvalidModelDescriptor)
    }
}

fn validate_model_list(models: &[ModelProviderDescriptor]) -> Result<(), ModelProviderConfigError> {
    if models.is_empty() {
        return Err(ModelProviderConfigError::EmptyModelList);
    }
    let mut seen = BTreeSet::new();
    for descriptor in models {
        if !seen.insert(descriptor.selection.clone()) {
            return Err(ModelProviderConfigError::DuplicateModelSelection);
        }
    }
    Ok(())
}

fn validate_descriptors(
    models: &[ModelProviderDescriptor],
) -> Result<(), ModelProviderConfigError> {
    for descriptor in models {
        descriptor.validate()?;
    }
    Ok(())
}

fn validate_endpoint(
    endpoint: &str,
    policy: ProviderEndpointPolicy,
) -> Result<(), ModelProviderConfigError> {
    let url =
        reqwest::Url::parse(endpoint).map_err(|_| ModelProviderConfigError::InvalidEndpoint)?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ModelProviderConfigError::InvalidEndpoint);
    }
    match url.scheme() {
        "https" => Ok(()),
        "http"
            if policy == ProviderEndpointPolicy::AllowLoopbackHttp
                && url.host_str().is_some_and(is_loopback_host) =>
        {
            Ok(())
        }
        _ => Err(ModelProviderConfigError::InvalidEndpoint),
    }
}

/// Numeric loopback IPs only: `127.0.0.0/8` or `::1`. Hostnames (`localhost`) and
/// non-loopback numeric addresses are never acceptable for HTTP. The URL host may
/// carry IPv6 brackets (`[::1]`), so they are stripped before parsing.
fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback())
}

/// One validated installation's static source: the exact validated definitions are
/// materialized once at installation and discovered locally. `discover()` performs
/// no network and no credential resolution.
struct InstalledModelSource {
    definitions: Vec<ModelDefinition>,
}

impl ModelSourceAdapter for InstalledModelSource {
    fn discover(&self) -> ModelSourceFuture<'_> {
        let definitions = self.definitions.clone();
        Box::pin(async move { Ok(definitions) })
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;
    use crate::model_gateway::fixed_credential_source;

    fn selection() -> ModelSelection {
        ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap())
    }

    fn descriptor() -> ModelProviderDescriptor {
        ModelProviderDescriptor::new(
            selection(),
            NonZeroU64::new(1).unwrap(),
            "gpt-5",
            NonZeroU32::new(4_096).unwrap(),
            NonZeroU32::new(3).unwrap(),
        )
        .unwrap()
    }

    fn installed(endpoint: &str) -> Result<ModelProviderConfig, ModelProviderConfigError> {
        ModelProviderConfig::openai_responses(
            endpoint,
            ProviderEndpointPolicy::HttpsOnly,
            fixed_credential_source("sk-test"),
            vec![descriptor()],
        )
    }

    #[test]
    fn credential_validation_is_closed_and_debug_always_redacts() {
        assert!("sk-abc".parse::<ProviderCredential>().is_ok());
        assert!("x".repeat(256).parse::<ProviderCredential>().is_ok());
        for invalid in [
            "",
            "has space",
            "bad\ncredential",
            "bad\u{0001}credential",
            "sk-é",
            "x".repeat(257).as_str(),
        ] {
            assert_eq!(
                invalid.parse::<ProviderCredential>().unwrap_err(),
                ProviderCredentialError::Invalid,
                "credential {invalid:?} was accepted"
            );
        }
        let credential: ProviderCredential = "sk-SECRET-CREDENTIAL".parse().unwrap();
        let debug = format!("{credential:?}");
        assert!(
            !debug.contains("SECRET"),
            "credential Debug must redact: {debug}"
        );
    }

    #[test]
    fn endpoint_policy_accepts_https_and_numeric_loopback_http_only() {
        for policy in [
            ProviderEndpointPolicy::HttpsOnly,
            ProviderEndpointPolicy::AllowLoopbackHttp,
        ] {
            assert!(
                installed_with_policy("https://api.openai.com/v1/responses", policy).is_ok(),
                "HTTPS must be accepted under {policy:?}"
            );
        }
        assert_eq!(
            installed("http://api.openai.com/v1/responses").unwrap_err(),
            ModelProviderConfigError::InvalidEndpoint,
            "arbitrary HTTP must be rejected under HttpsOnly"
        );
        assert_eq!(
            installed("http://localhost:1234/v1/responses").unwrap_err(),
            ModelProviderConfigError::InvalidEndpoint,
            "a hostname for HTTP must be rejected even under AllowLoopbackHttp"
        );
        assert!(
            installed_with_policy(
                "http://127.0.0.1:1234/v1/responses",
                ProviderEndpointPolicy::AllowLoopbackHttp,
            )
            .is_ok(),
            "numeric IPv4 loopback HTTP must be accepted under AllowLoopbackHttp"
        );
        assert!(
            installed_with_policy(
                "http://[::1]:1234/v1/responses",
                ProviderEndpointPolicy::AllowLoopbackHttp,
            )
            .is_ok(),
            "numeric IPv6 loopback HTTP must be accepted under AllowLoopbackHttp"
        );
        assert_eq!(
            installed_with_policy(
                "http://127.0.0.1:1234/v1/responses",
                ProviderEndpointPolicy::HttpsOnly,
            )
            .unwrap_err(),
            ModelProviderConfigError::InvalidEndpoint,
            "loopback HTTP must still be rejected under HttpsOnly"
        );
        assert_eq!(
            installed_with_policy(
                "http://[::ffff:127.0.0.1]:1234/v1/responses",
                ProviderEndpointPolicy::AllowLoopbackHttp,
            )
            .unwrap_err(),
            ModelProviderConfigError::InvalidEndpoint,
            "an IPv4-mapped IPv6 address is not the numeric loopback ::1"
        );
        assert_eq!(
            installed("http://192.168.1.1:1234/v1/responses").unwrap_err(),
            ModelProviderConfigError::InvalidEndpoint,
            "non-loopback numeric HTTP must be rejected"
        );
        for endpoint in [
            "https://user:pass@api.openai.com/v1/responses",
            "https://user@api.openai.com/v1/responses",
            "https://api.openai.com/v1/responses?key=SECRET",
            "https://api.openai.com/v1/responses#fragment",
            "not a url",
            "",
        ] {
            assert_eq!(
                installed(endpoint).unwrap_err(),
                ModelProviderConfigError::InvalidEndpoint,
                "endpoint {endpoint:?} was accepted"
            );
        }
    }

    fn installed_with_policy(
        endpoint: &str,
        policy: ProviderEndpointPolicy,
    ) -> Result<ModelProviderConfig, ModelProviderConfigError> {
        ModelProviderConfig::openai_responses(
            endpoint,
            policy,
            fixed_credential_source("sk-test"),
            vec![descriptor()],
        )
    }

    #[test]
    fn anthropic_version_validation_is_closed() {
        let models = vec![
            ModelProviderDescriptor::new(
                ModelSelection::new("anthropic".parse().unwrap(), "claude".parse().unwrap()),
                NonZeroU64::new(1).unwrap(),
                "claude-sonnet-4-6",
                NonZeroU32::new(4_096).unwrap(),
                NonZeroU32::new(3).unwrap(),
            )
            .unwrap(),
        ];
        for version in ["", "has space", "bad\nversion", "x".repeat(65).as_str()] {
            assert_eq!(
                ModelProviderConfig::anthropic_messages(
                    "https://api.anthropic.com/v1/messages",
                    ProviderEndpointPolicy::HttpsOnly,
                    version,
                    fixed_credential_source("sk-test"),
                    models.clone(),
                )
                .unwrap_err(),
                ModelProviderConfigError::InvalidVersion,
                "version {version:?} was accepted"
            );
        }
        assert!(
            ModelProviderConfig::anthropic_messages(
                "https://api.anthropic.com/v1/messages",
                ProviderEndpointPolicy::HttpsOnly,
                "2023-06-01",
                fixed_credential_source("sk-test"),
                models.clone(),
            )
            .is_ok()
        );
    }

    #[test]
    fn model_lists_require_non_empty_unique_selections() {
        assert_eq!(
            ModelProviderConfig::openai_responses(
                "https://api.openai.com/v1/responses",
                ProviderEndpointPolicy::HttpsOnly,
                fixed_credential_source("sk-test"),
                Vec::new(),
            )
            .unwrap_err(),
            ModelProviderConfigError::EmptyModelList
        );
        let duplicate = ModelProviderDescriptor::new(
            selection(),
            NonZeroU64::new(2).unwrap(),
            "gpt-5-api-name",
            NonZeroU32::new(4_096).unwrap(),
            NonZeroU32::new(3).unwrap(),
        )
        .unwrap();
        assert_eq!(
            ModelProviderConfig::openai_responses(
                "https://api.openai.com/v1/responses",
                ProviderEndpointPolicy::HttpsOnly,
                fixed_credential_source("sk-test"),
                vec![descriptor(), duplicate],
            )
            .unwrap_err(),
            ModelProviderConfigError::DuplicateModelSelection
        );
    }

    #[test]
    fn invalid_descriptor_defaults_are_rejected() {
        // A default output above the model max output limit is invalid.
        let over_limit = descriptor().with_max_output_tokens(NonZeroU32::new(2_048).unwrap());
        assert_eq!(
            ModelProviderConfig::openai_responses(
                "https://api.openai.com/v1/responses",
                ProviderEndpointPolicy::HttpsOnly,
                fixed_credential_source("sk-test"),
                vec![over_limit],
            )
            .unwrap_err(),
            ModelProviderConfigError::InvalidModelDescriptor
        );
        // A reasoning default outside the declared support is invalid.
        let unsupported_reasoning =
            descriptor().with_reasoning(ModelReasoningSupport::none(), ModelReasoningSummary::Low);
        assert_eq!(
            ModelProviderConfig::openai_responses(
                "https://api.openai.com/v1/responses",
                ProviderEndpointPolicy::HttpsOnly,
                fixed_credential_source("sk-test"),
                vec![unsupported_reasoning],
            )
            .unwrap_err(),
            ModelProviderConfigError::InvalidModelDescriptor
        );
        // An invalid API model name fails at descriptor construction.
        assert_eq!(
            ModelProviderDescriptor::new(
                selection(),
                NonZeroU64::new(1).unwrap(),
                "",
                NonZeroU32::new(4_096).unwrap(),
                NonZeroU32::new(3).unwrap(),
            )
            .unwrap_err(),
            ModelProviderDescriptorError::InvalidApiModelName
        );
    }

    #[test]
    fn api_model_name_accepts_opaque_ascii_including_colons_and_rejects_invalid_oversize() {
        // OpenAI fine-tune names contain colons; the API model name grammar is
        // non-empty printable opaque ASCII within 256 bytes, not the stable key.
        let max_len = "x".repeat(256);
        for name in [
            "ft:gpt-4o:acme-org:2025-01-01:model-1",
            "gpt-5.2-api",
            "claude-sonnet-4-6",
            "meta-llama/Llama-3.1-405B",
            max_len.as_str(),
        ] {
            let descriptor = ModelProviderDescriptor::new(
                selection(),
                NonZeroU64::new(1).unwrap(),
                name,
                NonZeroU32::new(4_096).unwrap(),
                NonZeroU32::new(3).unwrap(),
            )
            .unwrap_or_else(|error| panic!("API model name {name:?} must parse: {error}"));
            assert_eq!(descriptor.api_model_name.as_str(), name);
        }
        let oversize = "x".repeat(257);
        for name in [
            "",
            "has space",
            "bad\"quote",
            "back\\slash",
            "bad\ncontrol",
            "bad\u{0001}control",
            "bad-é",
            oversize.as_str(),
        ] {
            assert_eq!(
                ModelProviderDescriptor::new(
                    selection(),
                    NonZeroU64::new(1).unwrap(),
                    name,
                    NonZeroU32::new(4_096).unwrap(),
                    NonZeroU32::new(3).unwrap(),
                )
                .unwrap_err(),
                ModelProviderDescriptorError::InvalidApiModelName,
                "API model name {name:?} was accepted"
            );
        }
    }

    #[test]
    fn api_model_name_debug_always_redacts() {
        let name: ApiModelName = "ft:gpt-4o:acme-org:2025-01-01:model-1".parse().unwrap();
        let debug = format!("{name:?}");
        assert!(
            !debug.contains("ft:") && !debug.contains("acme-org"),
            "ApiModelName Debug must redact: {debug}"
        );
        assert!(
            debug.contains("redacted"),
            "redaction marker missing: {debug}"
        );
    }

    #[test]
    fn structured_output_opt_in_is_conservative_and_supports_protocol_cap_only() {
        fn built_capabilities(descriptor: ModelProviderDescriptor) -> (bool, Option<NonZeroU32>) {
            let installed = ModelProviderConfig::openai_responses(
                "https://api.openai.com/v1/responses",
                ProviderEndpointPolicy::HttpsOnly,
                fixed_credential_source("sk-test"),
                vec![descriptor],
            )
            .expect("the descriptor validates");
            let source = installed
                .build_source()
                .expect("the installation source builds");
            let definitions = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime builds")
                .block_on(source.discover())
                .expect("the installed source discovers");
            let definition = &definitions[0];
            (
                definition.capabilities.structured_json_schema,
                definition.limits.max_schema_bytes(),
            )
        }

        let (plain, plain_cap) = built_capabilities(descriptor());
        assert!(
            !plain && plain_cap.is_none(),
            "structured output must default to unsupported"
        );
        let (protocol_only, protocol_cap) =
            built_capabilities(descriptor().with_structured_json_schema(None));
        assert!(
            protocol_only && protocol_cap.is_none(),
            "opt-in without an explicit cap must support the protocol cap only"
        );
        let (capped, cap) = built_capabilities(
            descriptor().with_structured_json_schema(Some(NonZeroU32::new(65_536).unwrap())),
        );
        assert!(
            capped && cap == Some(NonZeroU32::new(65_536).unwrap()),
            "opt-in with an explicit cap must bind the model-specific schema cap"
        );
    }

    #[test]
    fn installation_debug_never_prints_secrets() {
        // The API model name is deliberately distinct from the stable model id so
        // the redaction assertions cannot be satisfied accidentally.
        let descriptor = ModelProviderDescriptor::new(
            selection(),
            NonZeroU64::new(1).unwrap(),
            "api-model-name-1",
            NonZeroU32::new(4_096).unwrap(),
            NonZeroU32::new(3).unwrap(),
        )
        .unwrap();
        let installed = ModelProviderConfig::openai_responses(
            "https://api.openai.com/v1/responses",
            ProviderEndpointPolicy::HttpsOnly,
            fixed_credential_source("sk-SECRET-CREDENTIAL"),
            vec![descriptor.clone()],
        )
        .unwrap();
        let debug = format!("{installed:?}");
        assert!(
            !debug.contains("api.openai.com"),
            "endpoint leaked: {debug}"
        );
        assert!(!debug.contains("SECRET"), "credential leaked: {debug}");
        let descriptor_debug = format!("{descriptor:?}");
        assert!(
            !descriptor_debug.contains("api-model-name-1"),
            "API model name leaked: {descriptor_debug}"
        );
    }

    #[test]
    fn constructor_validation_is_pure_and_build_source_materializes_the_route() {
        // The route constructor is pure validated config creation: it performs no
        // client/adapter construction, and each `build_source()` call materializes
        // its own working source.
        let installed = ModelProviderConfig::openai_responses(
            "https://api.openai.com/v1/responses",
            ProviderEndpointPolicy::HttpsOnly,
            fixed_credential_source("sk-test"),
            vec![descriptor()],
        )
        .expect("the pure constructor validates");
        let first = installed
            .build_source()
            .expect("the first source build materializes");
        let second = installed
            .build_source()
            .expect("a second source build materializes independently");
        for source in [&first, &second] {
            let definitions = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime builds")
                .block_on(source.discover())
                .expect("the materialized source discovers");
            assert_eq!(definitions.len(), 1);
            assert_eq!(definitions[0].selection.model_id().as_str(), "gpt-5");
            assert_eq!(definitions[0].api_model_name.as_str(), "gpt-5");
        }
        // The route spec is stored redacted: Debug never prints the endpoint or
        // version even though the route is now private config state.
        let route_debug = format!("{:?}", installed.route);
        assert!(
            !route_debug.contains("api.openai.com"),
            "route Debug must redact the endpoint: {route_debug}"
        );
    }

    #[test]
    fn installed_source_discovers_the_validated_definitions_statically() {
        let installed = ModelProviderConfig::openai_responses(
            "https://api.openai.com/v1/responses",
            ProviderEndpointPolicy::HttpsOnly,
            fixed_credential_source("sk-test"),
            vec![descriptor()],
        )
        .unwrap();
        let source = installed
            .build_source()
            .expect("the installation source builds");
        let definitions = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime builds")
            .block_on(source.discover())
            .expect("the installed source discovers");
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].selection.model_id().as_str(), "gpt-5");
    }
}
