mod gateway;
mod provider;
mod registry;
pub(crate) mod transport;
mod types;

pub use gateway::ModelGateway;
pub use provider::{ModelCallContext, ModelEventSink, ModelFuture, ModelProvider};
pub use registry::{ProviderRegistry, ProviderRegistryBuilder, ResolvedModel};
pub use types::{
    AssistantPart, DeliveryState, ModelDescriptor, ModelError, ModelErrorDetails, ModelErrorKind,
    ModelEvent, ModelFinishReason, ModelId, ModelIdentityError, ModelLimits, ModelLimitsError,
    ModelMessage, ModelRequest, ModelResponse, ModelSelection, ModelValueError, ProviderId,
    ReasoningPreference, ToolCall, Usage,
};
