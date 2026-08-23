mod context;
mod input;
#[cfg(test)]
mod legacy_context;
#[cfg(test)]
pub(crate) mod legacy_policy;
#[cfg(test)]
pub(crate) mod legacy_types;
mod policy;
mod progress;
#[cfg(test)]
pub(crate) mod registry;
mod set;
mod tool;
mod types;

pub use context::ToolContext;
pub use input::{ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest};
#[cfg(test)]
pub(crate) use legacy_context::{
    InteractionClient, InteractionReceiver, InteractionRequest, LegacyToolContext,
};
#[cfg(test)]
pub(crate) use legacy_types::{
    LegacyToolCallSummary, LegacyToolError, LegacyToolOutput, LegacyToolResultStatus,
    LegacyToolResultSummary, LegacyUserAnswer, LegacyUserQuestion,
};
pub use policy::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, MAX_TOOL_POLICY_TEXT_BYTES, ToolDecision,
    ToolPolicy, ToolPolicyError, ToolPolicyFuture, ToolPolicyRequest,
};
pub use progress::{ToolProgress, ToolProgressError, ToolProgressSink};
#[cfg(test)]
pub(crate) use registry::LegacyTool;
pub use set::{ToolSet, ToolSetBuilder, ToolSetError};
pub use tool::{Tool, ToolExecutionOutcome, ToolFuture, ToolInvocation, ToolOutput, ToolSpec};
pub(crate) use types::validate_json_shape;
pub use types::{ToolError, ToolName, ToolNameError, ToolResultOutcome, ToolValueError};
