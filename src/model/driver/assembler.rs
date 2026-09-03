use serde_json::Value;

use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::ToolCallId;
use crate::tools::{ToolName, ToolSpec};
use crate::value::{BoundedText, MAX_JSON_BYTES, validate_json_size};

use super::super::response::{ModelError, ModelErrorKind, ModelEvent};
use super::super::types::{
    AssistantPart, ModelFinishReason, ModelResponse, ReasoningContent, ToolCall, Usage,
};
use super::{ModelDriverProgress, SemanticLimitsSnapshot};

pub(super) struct Assembler<'a> {
    request_tools: &'a [ToolSpec],
    limits: &'a SemanticLimitsSnapshot,
    slots: Vec<PartSlot>,
    text: String,
    reasoning: String,
    tools: Vec<ToolAssembly>,
    usage: Option<Usage>,
    finish_reason: Option<ModelFinishReason>,
    pub(super) observed_event: bool,
}

#[derive(Clone, Copy)]
enum PartSlot {
    Text,
    Reasoning,
    Tool(usize),
}

struct ToolAssembly {
    tool_call_id: ToolCallId,
    name: ToolName,
    arguments_text: String,
    arguments: Option<Value>,
    call_index: u32,
}

impl<'a> Assembler<'a> {
    pub(super) fn new(request_tools: &'a [ToolSpec], limits: &'a SemanticLimitsSnapshot) -> Self {
        Self {
            request_tools,
            limits,
            slots: Vec::new(),
            text: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
            usage: None,
            finish_reason: None,
            observed_event: false,
        }
    }

    pub(super) fn push(
        &mut self,
        event: ModelEvent,
    ) -> Result<Option<ModelDriverProgress>, ModelError> {
        if self.finish_reason.is_some() || event.validate().is_err() {
            return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
        }
        match event {
            ModelEvent::TextDelta { delta } => {
                if self.text.is_empty() {
                    self.slots.push(PartSlot::Text);
                }
                append_bounded(
                    &mut self.text,
                    delta.as_str(),
                    self.limits.max_model_text_bytes_per_round,
                )?;
                Ok(Some(ModelDriverProgress::TextDelta(delta)))
            }
            ModelEvent::ReasoningDelta { delta } => {
                if self.reasoning.is_empty() {
                    self.slots.push(PartSlot::Reasoning);
                }
                append_bounded(
                    &mut self.reasoning,
                    delta.as_str(),
                    self.limits.max_model_reasoning_bytes_per_round,
                )?;
                Ok(Some(ModelDriverProgress::ReasoningDelta(delta)))
            }
            ModelEvent::ToolCallStart {
                tool_call_id,
                tool_name,
            } => {
                if !self
                    .request_tools
                    .iter()
                    .any(|tool| tool.name() == &tool_name)
                {
                    return Err(assembler_error(ModelErrorKind::UnexpectedToolCall));
                }
                if self.tools.len() >= self.limits.max_tool_calls_per_response
                    || self
                        .tools
                        .iter()
                        .any(|tool| tool.tool_call_id == tool_call_id)
                {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                let call_index = u32::try_from(self.tools.len())
                    .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                let index = self.tools.len();
                self.tools.push(ToolAssembly {
                    tool_call_id,
                    name: tool_name,
                    arguments_text: String::new(),
                    arguments: None,
                    call_index,
                });
                self.slots.push(PartSlot::Tool(index));
                Ok(None)
            }
            ModelEvent::ToolCallArgumentsDelta {
                tool_call_id,
                delta,
            } => {
                let tool = self.tool_mut(&tool_call_id)?;
                if tool.arguments.is_some() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                append_bounded(&mut tool.arguments_text, delta.as_str(), MAX_JSON_BYTES)?;
                Ok(None)
            }
            ModelEvent::ToolCallEnd { tool_call_id } => {
                let maximum = self.limits.max_tool_input_bytes;
                let tool = self.tool_mut(&tool_call_id)?;
                if tool.arguments.is_some() || tool.arguments_text.is_empty() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                let arguments: Value = serde_json::from_str(&tool.arguments_text)
                    .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                if !arguments.is_object() || validate_json_size(&arguments, maximum).is_err() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                tool.arguments = Some(arguments);
                Ok(None)
            }
            ModelEvent::Usage { usage } => {
                if self.usage.replace(usage).is_some() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                Ok(None)
            }
            ModelEvent::Finish { reason } => {
                if self.tools.iter().any(|tool| tool.arguments.is_none()) {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                self.finish_reason = Some(reason);
                Ok(None)
            }
        }
    }

    pub(super) fn finish(self) -> Result<ModelResponse, ModelError> {
        let reason = self
            .finish_reason
            .ok_or_else(|| assembler_error(ModelErrorKind::IncompleteResponse))?;
        let has_tools = !self.tools.is_empty();
        if (has_tools
            && !matches!(
                reason,
                ModelFinishReason::ToolCalls | ModelFinishReason::Unknown
            ))
            || (!has_tools && reason == ModelFinishReason::ToolCalls)
        {
            return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
        }
        if matches!(
            reason,
            ModelFinishReason::ContentFiltered | ModelFinishReason::Refused
        ) && self.text.is_empty()
        {
            return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
        }
        let mut parts = Vec::with_capacity(self.slots.len());
        for slot in self.slots {
            match slot {
                PartSlot::Text => parts.push(AssistantPart::Text(self.text.clone())),
                PartSlot::Reasoning => {
                    let reasoning =
                        ReasoningContent::new(Some(self.reasoning.clone()), None, None, None)
                            .map_err(|_| {
                                assembler_error(ModelErrorKind::InvalidProviderResponse)
                            })?;
                    parts.push(AssistantPart::Reasoning(reasoning));
                }
                PartSlot::Tool(index) => {
                    let tool = self
                        .tools
                        .get(index)
                        .ok_or_else(|| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                    let arguments = tool
                        .arguments
                        .clone()
                        .ok_or_else(|| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                    let call = ToolCall::new(
                        tool.tool_call_id.clone(),
                        tool.name.clone(),
                        arguments,
                        tool.call_index,
                    )
                    .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                    parts.push(AssistantPart::ToolCall(call));
                }
            }
        }
        ModelResponse::new(parts, reason, Some(self.usage.unwrap_or_default()))
            .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))
    }

    fn tool_mut(&mut self, tool_call_id: &ToolCallId) -> Result<&mut ToolAssembly, ModelError> {
        self.tools
            .iter_mut()
            .find(|tool| &tool.tool_call_id == tool_call_id)
            .ok_or_else(|| assembler_error(ModelErrorKind::InvalidProviderResponse))
    }
}

fn assembler_error(kind: ModelErrorKind) -> ModelError {
    let message = match kind {
        ModelErrorKind::UnexpectedToolCall => "unexpected tool call in model response",
        ModelErrorKind::IncompleteResponse => "incomplete model response",
        _ => "invalid model provider response",
    };
    let diagnostic = DiagnosticSummary::new(
        DiagnosticCode::ModelMalformedResponse,
        DiagnosticCategory::Model,
        BoundedText::new(message).expect("static diagnostic must fit BoundedText"),
        false,
    );
    ModelError::started(kind, diagnostic)
}

fn append_bounded(target: &mut String, delta: &str, maximum: usize) -> Result<(), ModelError> {
    let length = target
        .len()
        .checked_add(delta.len())
        .ok_or_else(|| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
    if length > maximum {
        return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
    }
    target.push_str(delta);
    Ok(())
}
