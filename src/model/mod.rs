mod driver;
#[path = "model.rs"]
mod model_port;
mod response;
mod types;

pub use model_port::{Model, ModelCallContext, ModelDescriptor, ModelStartFuture, ModelStream};
pub use response::{
    DeliveryState, MAX_MODEL_EVENT_TEXT_BYTES, ModelError, ModelErrorKind, ModelEvent, RetryHint,
};
pub use types::{
    AssistantPart, ModelFinishReason, ModelLimits, ModelLimitsError, ModelMessage, ModelRef,
    ModelRefError, ModelRequest, ModelResponse, ModelValueError, ReasoningContent,
    ReasoningPreference, ToolCall, Usage,
};

pub(crate) use driver::{
    ModelDriver, ModelDriverConfig, ModelDriverFailure, ModelDriverProgress, SemanticLimitsSnapshot,
};
