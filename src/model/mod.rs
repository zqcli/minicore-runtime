#[cfg(test)]
mod legacy_gateway;
#[cfg(test)]
pub(crate) mod legacy_provider;
#[cfg(test)]
pub(crate) mod legacy_registry;
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

#[cfg(test)]
pub(crate) use legacy_gateway::LegacyModelGateway;
#[cfg(test)]
pub(crate) use legacy_provider::{LegacyModelCallContext, LegacyModelEventSink};
#[cfg(test)]
pub(crate) use types::{
    LegacyModelDescriptor, LegacyModelEvent, LegacyModelId, LegacyModelSelection, LegacyProviderId,
};
