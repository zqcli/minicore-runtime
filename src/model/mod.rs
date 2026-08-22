mod gateway;
mod provider;
mod providers;
mod registry;
pub(crate) mod transport;
mod types;

pub use gateway::ModelGateway;
pub use provider::{
    CredentialSource, CredentialSourceFuture, OpenAiReasoningProgress, ProviderCredential,
    ProviderCredentialError, ProviderEndpointPolicy, fixed_credential_source,
};
pub use provider::{ModelCallContext, ModelEventSink, ModelFuture, ModelProvider};
pub use providers::{
    AnthropicMessagesProvider, AnthropicProviderError, OpenAiProviderError, OpenAiResponsesProvider,
};
pub use registry::{ProviderRegistry, ProviderRegistryBuilder, ResolvedModel};
pub use types::{
    AssistantPart, DeliveryState, ModelDescriptor, ModelError, ModelErrorDetails, ModelErrorKind,
    ModelEvent, ModelFinishReason, ModelId, ModelIdentityError, ModelLimits, ModelLimitsError,
    ModelMessage, ModelRef, ModelRefError, ModelRequest, ModelResponse, ModelSelection,
    ModelValueError, ProviderId, ProviderItemId, ProviderItemIdError, ReasoningContent,
    ReasoningPreference, ToolCall, Usage,
};
