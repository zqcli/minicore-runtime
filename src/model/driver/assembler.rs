use crate::model::ProviderReplay;
use serde_json::Value;

use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::ToolCallId;
use crate::tools::{ToolName, ToolSpec};
use crate::value::{BoundedText, MAX_JSON_BYTES};

use super::super::{
    AssistantPart, ModelFinishReason, ModelResponse, ReasoningContent, ToolCall, Usage,
};
use super::super::{ModelError, ModelErrorKind, ModelEvent};
use super::{LoopLimits, ModelDriverProgress};

pub(super) struct Assembler<'a> {
    request_tools: &'a [ToolSpec],
    limits: &'a LoopLimits,
    slots: Vec<PartSlot>,
    text: String,
    reasoning: String,
    tools: Vec<ToolAssembly>,
    usage: Option<Usage>,
    provider_replay: Option<ProviderReplay>,
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
    call: Option<ToolCall>,
    call_index: u32,
}

impl<'a> Assembler<'a> {
    pub(super) fn new(request_tools: &'a [ToolSpec], limits: &'a LoopLimits) -> Self {
        Self {
            request_tools,
            limits,
            slots: Vec::new(),
            text: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
            usage: None,
            provider_replay: None,
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
                let delta = normalized_delta(delta)?;
                if delta.is_empty() {
                    return Ok(None);
                }
                if self.text.is_empty() {
                    self.slots.push(PartSlot::Text);
                }
                append_bounded(
                    &mut self.text,
                    delta.as_str(),
                    self.limits.max_model_text_bytes,
                )?;
                Ok(Some(ModelDriverProgress::TextDelta(delta)))
            }
            ModelEvent::ReasoningDelta { delta } => {
                let delta = normalized_delta(delta)?;
                if delta.is_empty() {
                    return Ok(None);
                }
                if self.reasoning.is_empty() {
                    self.slots.push(PartSlot::Reasoning);
                }
                append_bounded(
                    &mut self.reasoning,
                    delta.as_str(),
                    self.limits.max_model_reasoning_bytes,
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
                    call: None,
                    call_index,
                });
                self.slots.push(PartSlot::Tool(index));
                Ok(None)
            }
            ModelEvent::ToolCallArgumentsDelta {
                tool_call_id,
                delta,
            } => {
                let delta = normalized_delta(delta)?;
                let tool = self.tool_mut(&tool_call_id)?;
                if tool.call.is_some() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                append_bounded(&mut tool.arguments_text, delta.as_str(), MAX_JSON_BYTES)?;
                Ok(None)
            }
            ModelEvent::ToolCallEnd { tool_call_id } => {
                let maximum = self.limits.max_tool_arguments_bytes;
                let tool = self.tool_mut(&tool_call_id)?;
                if tool.call.is_some() || tool.arguments_text.is_empty() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                let arguments: Value = serde_json::from_str(&tool.arguments_text)
                    .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                tool.call = Some(
                    ToolCall::new_with_max_bytes(
                        tool.tool_call_id.clone(),
                        tool.name.clone(),
                        arguments,
                        tool.call_index,
                        maximum,
                    )
                    .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))?,
                );
                Ok(None)
            }
            ModelEvent::Usage { usage } => {
                if self.usage.replace(usage).is_some() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                Ok(None)
            }
            ModelEvent::ProviderReplay { replay } => {
                if self.provider_replay.replace(replay).is_some() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                Ok(None)
            }
            ModelEvent::Finish { reason } => {
                if self.tools.iter().any(|tool| tool.call.is_none()) {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                self.finish_reason = Some(reason);
                Ok(None)
            }
        }
    }

    pub(super) fn finish(mut self) -> Result<ModelResponse, ModelError> {
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
                PartSlot::Text => {
                    parts.push(AssistantPart::Text(std::mem::take(&mut self.text).into()))
                }
                PartSlot::Reasoning => {
                    let reasoning = ReasoningContent::new(
                        Some(std::mem::take(&mut self.reasoning)),
                        None,
                        None,
                        None,
                    )
                    .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                    parts.push(AssistantPart::Reasoning(reasoning));
                }
                PartSlot::Tool(index) => {
                    let call = self.tools[index]
                        .call
                        .take()
                        .expect("Finish requires completed tools");
                    parts.push(AssistantPart::ToolCall(call));
                }
            }
        }
        ModelResponse::new_with_provider_replay(
            parts,
            reason,
            Some(self.usage.unwrap_or_default()),
            self.provider_replay,
        )
        .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))
    }

    fn tool_mut(&mut self, tool_call_id: &ToolCallId) -> Result<&mut ToolAssembly, ModelError> {
        self.tools
            .iter_mut()
            .find(|tool| &tool.tool_call_id == tool_call_id)
            .ok_or_else(|| assembler_error(ModelErrorKind::InvalidProviderResponse))
    }
}

fn normalized_delta(delta: BoundedText) -> Result<BoundedText, ModelError> {
    match crate::value::normalize_model_text(delta.as_str()) {
        std::borrow::Cow::Borrowed(_) => Ok(delta),
        std::borrow::Cow::Owned(text) => {
            BoundedText::new_with_max_bytes(text, crate::model::MAX_MODEL_EVENT_TEXT_BYTES)
                .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))
        }
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
    )
    .expect("valid diagnostic text");
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
