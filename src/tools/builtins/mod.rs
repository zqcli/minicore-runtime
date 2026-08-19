mod ask_user;
mod list_directory;
mod path_args;
mod read_file;
mod run_command;
mod write_file;

pub use ask_user::AskUserTool;
pub use list_directory::ListDirectoryTool;
pub use read_file::ReadFileTool;
pub use run_command::RunCommandTool;
pub use write_file::WriteFileTool;

use super::{ToolError, ToolOutput};

pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

pub(crate) fn success(text: impl Into<String>) -> Result<ToolOutput, ToolError> {
    ToolOutput::success(text).map_err(|_| ToolError::Internal)
}

pub(crate) fn failure(text: &'static str) -> Result<ToolOutput, ToolError> {
    ToolOutput::failure(text).map_err(|_| ToolError::Internal)
}
