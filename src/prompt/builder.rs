use std::fmt;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use thiserror::Error;

use crate::config::{SemanticLimits, SessionSpec};
use crate::context::{ContextBlock, ContextSlot, ValidatedContextBundle};
use crate::conversation::{ConversationEntry, ConversationView, PromptConversationProjection};
use crate::model::{AssistantPart, ModelLimits, ModelMessage, ModelRequest, ReasoningContent};
use crate::tools::{ToolOutput, ToolSpec};

pub(crate) const KERNEL_INVARIANT: &str = concat!(
    "Honor message roles and the tool-call protocol. ",
    "Use only declared tools and match every tool result to its call."
);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PromptError {
    #[error("prompt builder configuration is invalid")]
    InvalidConfiguration,
    #[error("conversation projection is invalid")]
    InvalidConversation,
    #[error("context projection is invalid")]
    InvalidContext,
    #[error("prompt tools are invalid")]
    InvalidTools,
    #[error("prompt serialization failed")]
    Serialization,
    #[error("prompt token estimate overflowed")]
    TokenOverflow,
    #[error("prompt exceeds the model context window")]
    ContextOverflow,
}

pub(crate) struct PromptPlan {
    fixed_request: ModelRequest,
    context_index: usize,
    fixed_input_tokens: u64,
    context_budget: Result<u64, PromptError>,
}

impl PromptPlan {
    pub(crate) fn fixed_input_tokens(&self) -> u64 {
        self.fixed_input_tokens
    }

    pub(crate) fn remaining_context_budget(&self) -> Result<u64, PromptError> {
        self.context_budget
    }

    pub(crate) fn finish(
        self,
        context: &ValidatedContextBundle,
    ) -> Result<ModelRequest, PromptError> {
        let (mut messages, tools, model_limits, reasoning) = self.fixed_request.into_parts();
        let context_messages = context
            .blocks()
            .iter()
            .map(format_context_block)
            .collect::<Result<Vec<_>, _>>()?;
        messages.splice(self.context_index..self.context_index, context_messages);
        let request = ModelRequest::new(messages, tools, model_limits, reasoning)
            .map_err(|_| PromptError::InvalidConversation)?;
        let estimated = estimate_request(&request)?;
        if let Some(window) = model_limits.context_window_tokens() {
            let _ = remaining_budget(estimated, u64::from(window), model_limits)?;
        }
        Ok(request)
    }
}

#[derive(Clone)]
pub(crate) struct PromptBuilder {
    spec: SessionSpec,
    tools: Arc<[ToolSpec]>,
    limits: SemanticLimits,
    static_messages: Arc<[ModelMessage]>,
    #[cfg(test)]
    projection_calls: Arc<AtomicUsize>,
}

impl fmt::Debug for PromptBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptBuilder")
            .field("model", &self.spec.model)
            .field("system_prompt_bytes", &self.spec.system_prompt.byte_len())
            .field("reasoning", &self.spec.reasoning)
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

impl PromptBuilder {
    pub(crate) fn new(
        spec: &SessionSpec,
        tools: Vec<ToolSpec>,
        limits: SemanticLimits,
    ) -> Result<Self, PromptError> {
        limits
            .validate()
            .map_err(|_| PromptError::InvalidConfiguration)?;
        if spec.enabled_tools.len() > limits.max_tool_count
            || spec
                .enabled_tools
                .iter()
                .any(|name| name.as_str().len() > limits.max_tool_name_bytes)
        {
            return Err(PromptError::InvalidTools);
        }
        if spec.validate(&limits).is_err() {
            return Err(PromptError::InvalidConfiguration);
        }
        validate_tools(&tools, spec, &limits)?;
        let mut static_messages = Vec::with_capacity(2);
        static_messages.push(
            ModelMessage::system(KERNEL_INVARIANT)
                .map_err(|_| PromptError::InvalidConfiguration)?,
        );
        if !spec.system_prompt.is_empty() {
            static_messages.push(
                ModelMessage::system(spec.system_prompt.as_str())
                    .map_err(|_| PromptError::InvalidConfiguration)?,
            );
        }
        Ok(Self {
            spec: spec.clone(),
            tools: tools.into(),
            limits,
            static_messages: static_messages.into(),
            #[cfg(test)]
            projection_calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) fn plan(
        &self,
        conversation: &ConversationView,
        model_limits: ModelLimits,
    ) -> Result<PromptPlan, PromptError> {
        #[cfg(test)]
        self.projection_calls.fetch_add(1, Ordering::Relaxed);
        let projection = conversation
            .validated_prompt_projection(&self.spec, &self.limits)
            .map_err(|_| PromptError::InvalidConversation)?;
        let projected = project_conversation(&projection)?;
        let mut messages = self.static_messages.to_vec();
        let context_index = messages.len();
        messages.extend(projected);
        let fixed_request = self.request(messages, model_limits)?;
        let fixed_input_tokens = estimate_request(&fixed_request)?;
        let context_budget = match model_limits.context_window_tokens() {
            Some(window) => remaining_budget(fixed_input_tokens, u64::from(window), model_limits),
            None => Ok(u64::MAX),
        };
        Ok(PromptPlan {
            fixed_request,
            context_index,
            fixed_input_tokens,
            context_budget,
        })
    }

    #[cfg(test)]
    pub(crate) fn projection_calls(&self) -> usize {
        self.projection_calls.load(Ordering::Relaxed)
    }

    fn request(
        &self,
        messages: Vec<ModelMessage>,
        model_limits: ModelLimits,
    ) -> Result<ModelRequest, PromptError> {
        ModelRequest::new(
            messages,
            self.tools.to_vec(),
            model_limits,
            self.spec.reasoning,
        )
        .map_err(|_| PromptError::InvalidConversation)
    }
}

fn project_conversation(
    projection: &PromptConversationProjection,
) -> Result<Vec<ModelMessage>, PromptError> {
    let mut messages = Vec::new();
    if let Some(summary) = projection.selected_summary() {
        messages.push(
            ModelMessage::system(summary.summary.as_str())
                .map_err(|_| PromptError::InvalidConversation)?,
        );
    }
    for entry in projection.entries() {
        match entry {
            ConversationEntry::UserMessage(entry) => {
                messages.push(
                    ModelMessage::user(entry.input.text.as_str())
                        .map_err(|_| PromptError::InvalidConversation)?,
                );
            }
            ConversationEntry::AssistantMessage(entry) => {
                messages.push(project_assistant(entry)?);
            }
            ConversationEntry::ToolResult(entry) => {
                let output = ToolOutput::new(entry.content.as_str())
                    .map_err(|_| PromptError::InvalidConversation)?;
                messages.push(
                    ModelMessage::tool_with_outcome(
                        entry.tool_call_id.clone(),
                        output,
                        entry.outcome,
                    )
                    .map_err(|_| PromptError::InvalidConversation)?,
                );
            }
            ConversationEntry::Summary(_) | ConversationEntry::TurnTerminal(_) => {}
        }
    }
    Ok(messages)
}

fn project_assistant(
    entry: &crate::conversation::AssistantMessageEntry,
) -> Result<ModelMessage, PromptError> {
    let mut parts = Vec::with_capacity(
        usize::from(entry.reasoning.is_some())
            .saturating_add(usize::from(entry.text.is_some()))
            .saturating_add(entry.tool_calls.len()),
    );
    if let Some(reasoning) = &entry.reasoning {
        parts.push(AssistantPart::Reasoning(
            ReasoningContent::new(Some(reasoning.as_str().to_owned()), None, None, None)
                .map_err(|_| PromptError::InvalidConversation)?,
        ));
    }
    if let Some(text) = &entry.text {
        parts.push(AssistantPart::Text(text.as_str().to_owned()));
    }
    for call in &entry.tool_calls {
        parts.push(AssistantPart::ToolCall(call.clone()));
    }
    ModelMessage::assistant(parts).map_err(|_| PromptError::InvalidConversation)
}

fn validate_tools(
    tools: &[ToolSpec],
    spec: &SessionSpec,
    limits: &SemanticLimits,
) -> Result<(), PromptError> {
    if tools.len() != spec.enabled_tools.len() || tools.len() > limits.max_tool_count {
        return Err(PromptError::InvalidTools);
    }
    for (tool, enabled) in tools.iter().zip(&spec.enabled_tools) {
        if tool.name() != enabled
            || tool
                .validate_for_bindings(limits.max_tool_name_bytes, limits.max_tool_schema_bytes)
                .is_err()
        {
            return Err(PromptError::InvalidTools);
        }
    }
    Ok(())
}

fn format_context_block(block: &ContextBlock) -> Result<ModelMessage, PromptError> {
    let slot = match block.slot {
        ContextSlot::ProjectInstructions => "project_instructions",
        ContextSlot::RetrievedKnowledge => "retrieved_knowledge",
        ContextSlot::TurnContext => "turn_context",
    };
    ModelMessage::system(format!(
        "[minicore-context slot={slot} source={}]\n{}",
        block.source,
        block.content.as_str(),
    ))
    .map_err(|_| PromptError::InvalidContext)
}

/// Measures the serialized request without materialising it. The byte count is
/// exactly the length serde would produce, but nothing is allocated, which
/// matters because a turn estimates once per round over the whole prompt.
fn estimate_request(request: &ModelRequest) -> Result<u64, PromptError> {
    let mut sink = ByteCountingWriter::default();
    serde_json::to_writer(&mut sink, request).map_err(|_| PromptError::Serialization)?;
    estimate_tokens(sink.written)
}

#[derive(Default)]
struct ByteCountingWriter {
    written: usize,
}

impl std::io::Write for ByteCountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn estimate_tokens(bytes: usize) -> Result<u64, PromptError> {
    let rounded = bytes.checked_add(3).ok_or(PromptError::TokenOverflow)?;
    u64::try_from(rounded / 4).map_err(|_| PromptError::TokenOverflow)
}

fn remaining_budget(
    estimated: u64,
    context_window: u64,
    limits: ModelLimits,
) -> Result<u64, PromptError> {
    let output = limits.max_output_tokens().map(u64::from).unwrap_or(0);
    let available = context_window
        .checked_sub(output)
        .ok_or(PromptError::ContextOverflow)?;
    available
        .checked_sub(estimated)
        .ok_or(PromptError::ContextOverflow)
}

#[cfg(test)]
mod tests;
