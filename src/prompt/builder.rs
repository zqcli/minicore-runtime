use std::fmt;
use std::sync::Arc;

use serde_json::to_vec;
use thiserror::Error;

use crate::model::{ModelLimits, ModelMessage, ModelRequest, ModelSelection, ReasoningPreference};
use crate::storage::conversation::PromptConversationView;
use crate::tools::{ToolName, ToolSpec};

pub(crate) const MAX_PROMPT_TEXT_BYTES: usize = 262_144;
pub(crate) const MAX_SUMMARY_TEXT_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PromptError {
    #[error("prompt text is invalid")]
    InvalidText,
    #[error("prompt tools are not strictly sorted and unique")]
    InvalidTools,
    #[error("prompt token estimate overflowed")]
    TokenOverflow,
    #[error("prompt exceeds the model context window")]
    ContextOverflow,
    #[error("prompt request is invalid")]
    InvalidRequest,
    #[error("prompt serialization failed")]
    Serialization,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PromptBuilder {
    system_prompt: Arc<str>,
    coding_instructions: Arc<str>,
}

impl fmt::Debug for PromptBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptBuilder")
            .field("system_prompt_bytes", &self.system_prompt.len())
            .field("coding_instructions_bytes", &self.coding_instructions.len())
            .finish()
    }
}

impl PromptBuilder {
    pub(crate) fn new(
        system_prompt: impl Into<String>,
        coding_instructions: impl Into<String>,
    ) -> Result<Self, PromptError> {
        let system_prompt = system_prompt.into();
        let coding_instructions = coding_instructions.into();
        if !valid_text(&system_prompt, true) || !valid_text(&coding_instructions, false) {
            return Err(PromptError::InvalidText);
        }
        Ok(Self {
            system_prompt: system_prompt.into(),
            coding_instructions: coding_instructions.into(),
        })
    }

    pub(crate) fn build(
        &self,
        conversation: &PromptConversationView,
        tools: &[ToolSpec],
        options: PromptBuildOptions,
    ) -> Result<ModelRequest, PromptError> {
        self.build_parts(
            conversation.latest_summary().map(|summary| summary.text()),
            conversation.messages(),
            tools,
            &options,
        )
    }

    pub(super) fn build_parts(
        &self,
        latest_summary: Option<&str>,
        conversation_messages: &[ModelMessage],
        tools: &[ToolSpec],
        options: &PromptBuildOptions,
    ) -> Result<ModelRequest, PromptError> {
        let messages = self.compose_messages(latest_summary, conversation_messages)?;
        validate_tools(tools)?;
        let estimated_input = estimate_serialized(&messages, tools)?;
        check_context(estimated_input, options.limits)?;
        ModelRequest::new(
            options.selection.clone(),
            messages,
            tools.to_vec(),
            options.limits,
            options.reasoning,
        )
        .map_err(|_| PromptError::InvalidRequest)
    }

    pub(super) fn estimate_parts(
        &self,
        latest_summary: Option<&str>,
        conversation_messages: &[ModelMessage],
        tools: &[ToolSpec],
    ) -> Result<u64, PromptError> {
        let messages = self.compose_messages(latest_summary, conversation_messages)?;
        validate_tools(tools)?;
        estimate_serialized(&messages, tools)
    }

    fn compose_messages(
        &self,
        latest_summary: Option<&str>,
        conversation_messages: &[ModelMessage],
    ) -> Result<Vec<ModelMessage>, PromptError> {
        let mut messages = Vec::with_capacity(
            usize::from(!self.system_prompt.is_empty())
                .saturating_add(2)
                .saturating_add(conversation_messages.len()),
        );
        if !self.system_prompt.is_empty() {
            messages.push(
                ModelMessage::system(self.system_prompt.to_string())
                    .map_err(|_| PromptError::InvalidRequest)?,
            );
        }
        messages.push(
            ModelMessage::system(self.coding_instructions.to_string())
                .map_err(|_| PromptError::InvalidRequest)?,
        );
        if let Some(summary) = latest_summary {
            messages.push(
                ModelMessage::user(summary.to_owned()).map_err(|_| PromptError::InvalidText)?,
            );
        }
        messages.extend(conversation_messages.iter().cloned());
        Ok(messages)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromptBuildOptions {
    selection: ModelSelection,
    limits: ModelLimits,
    reasoning: ReasoningPreference,
}

impl PromptBuildOptions {
    pub(crate) const fn new(
        selection: ModelSelection,
        limits: ModelLimits,
        reasoning: ReasoningPreference,
    ) -> Self {
        Self {
            selection,
            limits,
            reasoning,
        }
    }

    pub(crate) const fn selection(&self) -> &ModelSelection {
        &self.selection
    }

    pub(crate) const fn limits(&self) -> ModelLimits {
        self.limits
    }

    pub(crate) const fn reasoning(&self) -> ReasoningPreference {
        self.reasoning
    }
}

fn valid_text(value: &str, allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty())
        && value.len() <= MAX_PROMPT_TEXT_BYTES
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

fn validate_tools(tools: &[ToolSpec]) -> Result<(), PromptError> {
    let mut previous: Option<&ToolName> = None;
    for tool in tools {
        if previous.is_some_and(|previous| previous >= tool.name()) {
            return Err(PromptError::InvalidTools);
        }
        previous = Some(tool.name());
    }
    Ok(())
}

pub(super) fn estimate_serialized(
    messages: &[ModelMessage],
    tools: &[ToolSpec],
) -> Result<u64, PromptError> {
    let message_bytes = to_vec(messages)
        .map_err(|_| PromptError::Serialization)?
        .len();
    let tool_bytes = to_vec(tools).map_err(|_| PromptError::Serialization)?.len();
    let total_bytes = message_bytes
        .checked_add(tool_bytes)
        .ok_or(PromptError::TokenOverflow)?;
    let rounded = total_bytes
        .checked_add(3)
        .ok_or(PromptError::TokenOverflow)?;
    u64::try_from(rounded / 4).map_err(|_| PromptError::TokenOverflow)
}

fn check_context(estimated_input: u64, limits: ModelLimits) -> Result<(), PromptError> {
    let Some(context_window) = limits.context_window_tokens() else {
        return Ok(());
    };
    let reserved = limits.max_output_tokens().map(u64::from).unwrap_or(0);
    let required = estimated_input
        .checked_add(reserved)
        .ok_or(PromptError::TokenOverflow)?;
    if required > u64::from(context_window) {
        return Err(PromptError::ContextOverflow);
    }
    Ok(())
}
