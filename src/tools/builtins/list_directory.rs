use serde_json::{Value, json};

use crate::workspace_v2::{RelativePath, Workspace, WorkspaceError};

use super::super::{Tool, ToolContext, ToolError, ToolFuture, ToolSpec};
use super::path_args::parse_path_args;
use super::{MAX_TOOL_OUTPUT_BYTES, failure, success};

const INVALID_ARGUMENTS: &str = "tool arguments are invalid";
const ACCESS_DENIED: &str = "workspace access is denied";
const DIRECTORY_NOT_LISTED: &str = "directory could not be listed";
const DIRECTORY_TOO_LARGE: &str = "directory listing is too large";
const UNSUPPORTED_ENTRY_NAME: &str = "directory contains an unsupported entry name";
const DESCRIPTION: &str = "List direct workspace directory entries as sorted compact JSON.";

#[derive(Clone, Copy, Debug, Default)]
pub struct ListDirectoryTool;

impl ListDirectoryTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for ListDirectoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "list_directory".parse().expect("builtin name is valid"),
            DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
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
            let path = match parse_path_args(&args, true) {
                Ok(path) => path,
                Err(()) => return failure(INVALID_ARGUMENTS),
            };
            if ctx.cancellation().is_cancelled() {
                return Err(ToolError::Cancelled);
            }

            let result = ctx
                .workspace()
                .list(&path, Workspace::MAX_LIST_ENTRIES)
                .await;
            let entries = match result {
                Ok(entries) => entries,
                Err(error) => return failure(list_error_text(error)),
            };
            let text = match serde_json::to_string(&entries) {
                Ok(text) if text.len() <= MAX_TOOL_OUTPUT_BYTES => text,
                Ok(_) | Err(_) => return failure(DIRECTORY_TOO_LARGE),
            };
            match success(text) {
                Ok(output) => Ok(output),
                Err(_) => failure(DIRECTORY_TOO_LARGE),
            }
        })
    }
}

fn list_error_text(error: WorkspaceError) -> &'static str {
    match error {
        WorkspaceError::InvalidPath => ACCESS_DENIED,
        WorkspaceError::InvalidEntryName => UNSUPPORTED_ENTRY_NAME,
        WorkspaceError::ListingTooLarge => DIRECTORY_TOO_LARGE,
        _ => DIRECTORY_NOT_LISTED,
    }
}
