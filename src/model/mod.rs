mod legacy_gateway;
pub(crate) mod legacy_provider;
mod legacy_registry;
#[path = "model.rs"]
mod model_port;
mod response;
mod types;

pub use model_port::{Model, ModelCallContext, ModelDescriptor, ModelStartFuture, ModelStream};
pub use response::{
    DeliveryState, MAX_MODEL_EVENT_TEXT_BYTES, ModelError, ModelErrorDetails, ModelErrorKind,
    ModelEvent,
};
pub use types::{
    AssistantPart, ModelFinishReason, ModelLimits, ModelLimitsError, ModelMessage, ModelRef,
    ModelRefError, ModelRequest, ModelResponse, ModelValueError, ReasoningContent,
    ReasoningPreference, ToolCall, Usage,
};

pub(crate) use legacy_gateway::LegacyModelGateway;
pub(crate) use legacy_provider::{LegacyModelCallContext, LegacyModelEventSink};
pub(crate) use legacy_registry::{
    LegacyProviderRegistry, LegacyProviderRegistryBuilder, LegacyResolvedModel,
};
pub(crate) use types::{
    LegacyModelDescriptor, LegacyModelEvent, LegacyModelId, LegacyModelIdentityError,
    LegacyModelSelection, LegacyProviderId,
};

const _: () = {
    // P5/P6 deletion target: remove with crate-private legacy model exports.
    let _ = std::mem::size_of::<LegacyProviderRegistryBuilder>();
    let _ = std::mem::size_of::<LegacyResolvedModel>();
    let _ = std::mem::size_of::<LegacyModelIdentityError>();
};
