mod context;
mod input;
mod legacy_context;
pub(crate) mod legacy_policy;
pub(crate) mod legacy_types;
mod policy;
mod progress;
pub(crate) mod registry;
mod set;
mod tool;
mod types;

pub use context::ToolContext;
pub use input::{ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest};
pub(crate) use legacy_context::{
    InteractionClient, InteractionReceiver, InteractionRequest, LegacyToolContext,
};
pub(crate) use legacy_types::{
    LegacyToolCallSummary, LegacyToolError, LegacyToolOutput, LegacyToolResultStatus,
    LegacyToolResultSummary, LegacyUserAnswer, LegacyUserQuestion,
};
pub use policy::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, MAX_TOOL_POLICY_TEXT_BYTES, ToolDecision,
    ToolPolicy, ToolPolicyError, ToolPolicyFuture, ToolPolicyRequest,
};
pub use progress::{ToolProgress, ToolProgressError, ToolProgressSink};
pub(crate) use registry::LegacyTool;
pub use set::{ToolSet, ToolSetBuilder, ToolSetError};
pub use tool::{Tool, ToolExecutionOutcome, ToolFuture, ToolInvocation, ToolOutput, ToolSpec};
pub(crate) use types::validate_json_shape;
pub use types::{ToolError, ToolName, ToolNameError, ToolResultOutcome, ToolValueError};
