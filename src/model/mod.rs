mod driver;
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

pub(crate) use driver::{
    ModelDriver, ModelDriverConfig, ModelDriverFailure, ModelDriverProgress, SemanticLimitsSnapshot,
};

const _: () = {
    let _ = ModelDriverConfig::from_kernel_values;
    let _ = SemanticLimitsSnapshot::from_kernel_values;
    let _ = ModelDriver::new;
    let _ = ModelDriver::run;
    let _ = ModelDriver::run_detailed;
    let _ = std::mem::size_of::<ModelDriverFailure>();
    let _ = ModelDriverFailure::error;
    let _ = ModelDriverFailure::deadline_source;
    let _ = ModelDriverProgress::delta;
};
