use serde::Deserialize;
use serde_json::{Value, json};

use crate::workspace_v2::{RelativePath, Workspace, WorkspaceError};

use super::super::{Tool, ToolContext, ToolError, ToolFuture, ToolSpec};
use super::path_args::validate_path;
use super::{failure, success};

const INVALID_ARGUMENTS: &str = "tool arguments are invalid";
const ACCESS_DENIED: &str = "workspace access is denied";
const READ_ONLY: &str = "workspace is read-only";
const FILE_NOT_WRITTEN: &str = "file could not be written";
const FILE_WRITTEN: &str = "file written";
const DESCRIPTION: &str = "Write UTF-8 text to one file relative to the workspace root.";

#[derive(Clone, Copy, Debug, Default)]
pub struct WriteFileTool;

impl WriteFileTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write_file".parse().expect("builtin name is valid"),
            DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": RelativePath::MAX_BYTES
                    },
                    "content": {
                        "type": "string",
                        "maxLength": Workspace::MAX_WRITE_BYTES
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
        .expect("builtin spec is valid")
    }

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments = match serde_json::from_value::<WriteFileArguments>(args) {
                Ok(arguments)
                    if validate_path(&arguments.path, false).is_ok()
                        && arguments.content.len() <= Workspace::MAX_WRITE_BYTES =>
                {
                    arguments
                }
                _ => return failure(INVALID_ARGUMENTS),
            };
            if ctx.cancellation().is_cancelled() {
                return Err(ToolError::Cancelled);
            }

            match ctx
                .workspace()
                .write_text(&arguments.path, &arguments.content)
                .await
            {
                Ok(()) => success(FILE_WRITTEN),
                Err(error) => failure(write_error_text(error)),
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArguments {
    path: RelativePath,
    content: String,
}

fn write_error_text(error: WorkspaceError) -> &'static str {
    match error {
        WorkspaceError::InvalidPath | WorkspaceError::IsSymlink => ACCESS_DENIED,
        WorkspaceError::ReadOnly => READ_ONLY,
        _ => FILE_NOT_WRITTEN,
    }
}
