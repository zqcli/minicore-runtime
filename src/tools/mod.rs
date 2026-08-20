mod builtins;
mod context;
mod policy;
mod process;
mod registry;
mod types;

pub use builtins::{AskUserTool, ListDirectoryTool, ReadFileTool, RunCommandTool, WriteFileTool};
pub use process::{ProcessPolicy, ProcessPolicyError, ProgramPolicy};

pub use context::{InteractionClient, InteractionReceiver, InteractionRequest, ToolContext};
pub use policy::{
    AllowConfiguredTools, ToolContextView, ToolDecision, ToolPolicy, ToolPolicyError, ToolRequest,
};
pub use registry::{Tool, ToolFuture, ToolRegistry, ToolRegistryBuilder};
pub(crate) use types::validate_json_shape;

use std::sync::Arc;

fn retain_p7_process_surface() {
    let policy = Arc::new(ProcessPolicy::disabled());
    let _local_policy = ProcessPolicy::coding_agent_local();
    let _any_program = ProgramPolicy::any();
    let _listed_program = ProgramPolicy::allow_list(["p7-process-surface"]).unwrap();
    let tool = RunCommandTool::new(policy);
    let _ = <RunCommandTool as Tool>::spec(&tool);
    let _execute: for<'a> fn(
        &'a RunCommandTool,
        ToolContext<'a>,
        serde_json::Value,
    ) -> ToolFuture<'a> = <RunCommandTool as Tool>::execute;
    let _policy_error: Option<ProcessPolicyError> = None;
    let _program_policy: Option<ProgramPolicy> = None;
    let _ = (_execute, _policy_error, _program_policy);
}

const _: fn() = retain_p7_process_surface;
pub use types::{
    ToolCallSummary, ToolError, ToolName, ToolNameError, ToolOutput, ToolResultStatus,
    ToolResultSummary, ToolSpec, ToolValueError, UserAnswer, UserQuestion,
};
