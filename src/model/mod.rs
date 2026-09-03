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
    MAX_MODEL_CALL_TIMEOUT, MAX_MODEL_RETRY_DELAY, ModelDriver, ModelDriverConfig,
    ModelDriverFailure, ModelDriverProgress, SemanticLimitsSnapshot,
};
pub(crate) use types::MAX_MODEL_MESSAGE_TEXT_BYTES;
