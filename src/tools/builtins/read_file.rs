use serde_json::{Value, json};

use crate::workspace_v2::{RelativePath, Workspace, WorkspaceError};

use super::super::{Tool, ToolContext, ToolError, ToolFuture, ToolSpec};
use super::path_args::parse_path_args;
use super::{failure, success};

const INVALID_ARGUMENTS: &str = "tool arguments are invalid";
const ACCESS_DENIED: &str = "workspace access is denied";
const FILE_NOT_READ: &str = "file could not be read";
const FILE_NOT_UTF8: &str = "file is not valid UTF-8";
const FILE_TOO_LARGE: &str = "file is too large";
const DESCRIPTION: &str = "Read one UTF-8 text file relative to the workspace root.";

#[derive(Clone, Copy, Debug, Default)]
pub struct ReadFileTool;

impl ReadFileTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read_file".parse().expect("builtin name is valid"),
            DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": RelativePath::MAX_BYTES
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
        .expect("builtin spec is valid")
    }

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let path = match parse_path_args(&args, false) {
                Ok(path) => path,
                Err(()) => return failure(INVALID_ARGUMENTS),
            };
            if ctx.cancellation().is_cancelled() {
                return Err(ToolError::Cancelled);
            }

            let result = ctx
                .workspace()
                .read_text(&path, Workspace::MAX_READ_BYTES)
                .await;
            match result {
                Ok(text) => match success(text) {
                    Ok(output) => Ok(output),
                    Err(_) => failure(FILE_NOT_READ),
                },
                Err(error) => failure(read_error_text(error)),
            }
        })
    }
}

fn read_error_text(error: WorkspaceError) -> &'static str {
    match error {
        WorkspaceError::InvalidPath => ACCESS_DENIED,
        WorkspaceError::TooLarge => FILE_TOO_LARGE,
        WorkspaceError::InvalidUtf8 => FILE_NOT_UTF8,
        _ => FILE_NOT_READ,
    }
}
