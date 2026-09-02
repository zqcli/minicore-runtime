use thiserror::Error;

use crate::value::{BoundedText, MAX_JSON_BYTES};

const MAX_HISTORY_ITEMS: usize = 4_096;
const MAX_HISTORY_BYTES: usize = 8 * BoundedText::MAX_BYTES;
const MAX_TOOL_CALLS_PER_RESPONSE: usize = 64;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_PROMPT_MESSAGES: usize = 4_096;

/// Runtime safety ceilings for one live agent loop.
///
/// These are execution-time budgets checked when a loop starts (and again on
/// config update). They carry no durable session semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopLimits {
    pub max_history_items: usize,
    pub max_history_bytes: usize,
    pub max_user_input_bytes: usize,
    pub max_model_text_bytes: usize,
    pub max_model_reasoning_bytes: usize,
    pub max_tool_calls_per_response: usize,
    pub max_tool_name_bytes: usize,
    pub max_tool_schema_bytes: usize,
    pub max_tool_arguments_bytes: usize,
    pub max_tool_output_bytes: usize,
    pub max_prompt_messages: usize,
}

impl Default for LoopLimits {
    fn default() -> Self {
        Self {
            max_history_items: MAX_HISTORY_ITEMS,
            max_history_bytes: MAX_HISTORY_BYTES,
            max_user_input_bytes: BoundedText::MAX_BYTES,
            max_model_text_bytes: BoundedText::MAX_BYTES,
            max_model_reasoning_bytes: BoundedText::MAX_BYTES,
            max_tool_calls_per_response: MAX_TOOL_CALLS_PER_RESPONSE,
            max_tool_name_bytes: MAX_TOOL_NAME_BYTES,
            max_tool_schema_bytes: MAX_JSON_BYTES,
            max_tool_arguments_bytes: MAX_JSON_BYTES,
            max_tool_output_bytes: BoundedText::MAX_BYTES,
            max_prompt_messages: MAX_PROMPT_MESSAGES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LoopLimitsError {
    #[error("loop limits contain an out-of-bounds value")]
    InvalidBounds,
}

impl LoopLimits {
    /// Validates every budget is non-zero and within its absolute cap.
    pub fn validate(&self) -> Result<(), LoopLimitsError> {
        if self.max_history_items == 0
            || self.max_history_items > MAX_HISTORY_ITEMS
            || self.max_history_bytes == 0
            || self.max_history_bytes > MAX_HISTORY_BYTES
            || self.max_user_input_bytes == 0
            || self.max_user_input_bytes > BoundedText::MAX_BYTES
            || self.max_model_text_bytes == 0
            || self.max_model_text_bytes > BoundedText::MAX_BYTES
            || self.max_model_reasoning_bytes == 0
            || self.max_model_reasoning_bytes > BoundedText::MAX_BYTES
            || self.max_tool_calls_per_response == 0
            || self.max_tool_calls_per_response > MAX_TOOL_CALLS_PER_RESPONSE
            || self.max_tool_name_bytes == 0
            || self.max_tool_name_bytes > MAX_TOOL_NAME_BYTES
            || self.max_tool_schema_bytes == 0
            || self.max_tool_schema_bytes > MAX_JSON_BYTES
            || self.max_tool_arguments_bytes == 0
            || self.max_tool_arguments_bytes > MAX_JSON_BYTES
            || self.max_tool_output_bytes == 0
            || self.max_tool_output_bytes > BoundedText::MAX_BYTES
            || self.max_prompt_messages == 0
            || self.max_prompt_messages > MAX_PROMPT_MESSAGES
        {
            return Err(LoopLimitsError::InvalidBounds);
        }
        Ok(())
    }
}
