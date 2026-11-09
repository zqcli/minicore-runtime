use crate::model::ProviderReplay;
use serde_json::Value;

use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::ToolCallId;
use crate::tools::{ToolName, ToolSpec};
use crate::value::{Text, normalize_model_text};

use super::super::{
    AssistantPart, IncompleteToolCall, ModelFinishReason, ModelPartial, ModelResponse,
    ReasoningContent, ToolCall, Usage,
};
use super::super::{ModelError, ModelErrorKind, ModelEvent};
use super::ModelDriverProgress;

pub(super) struct Assembler<'a> {
    request_tools: &'a [ToolSpec],
    slots: Vec<PartSlot>,
    text: String,
    reasoning: String,
    text_ended_with_cr: bool,
    reasoning_ended_with_cr: bool,
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
    pub(super) fn new(request_tools: &'a [ToolSpec]) -> Self {
        Self {
            request_tools,
            slots: Vec::new(),
            text: String::new(),
            reasoning: String::new(),
            text_ended_with_cr: false,
            reasoning_ended_with_cr: false,
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
        if self.finish_reason.is_some() {
            return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
        }
        match event {
            ModelEvent::TextDelta { delta } => {
                let delta = normalized_delta(delta, &mut self.text_ended_with_cr);
                if delta.is_empty() {
                    return Ok(None);
                }
                if self.text.is_empty() {
                    self.slots.push(PartSlot::Text);
                }
                self.text.push_str(delta.as_str());
                Ok(Some(ModelDriverProgress::TextDelta(delta)))
            }
            ModelEvent::ReasoningDelta { delta } => {
                let delta = normalized_delta(delta, &mut self.reasoning_ended_with_cr);
                if delta.is_empty() {
                    return Ok(None);
                }
                if self.reasoning.is_empty() {
                    self.slots.push(PartSlot::Reasoning);
                }
                self.reasoning.push_str(delta.as_str());
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
                if self
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
                let delta = normalize_model_text(delta.as_str());
                let tool = self.tool_mut(&tool_call_id)?;
                if tool.call.is_some() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                tool.arguments_text.push_str(&delta);
                Ok(None)
            }
            ModelEvent::ToolCallEnd { tool_call_id } => {
                let tool = self.tool_mut(&tool_call_id)?;
                if tool.call.is_some() || tool.arguments_text.is_empty() {
                    return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
                }
                let arguments: Value = serde_json::from_str(&tool.arguments_text)
                    .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))?;
                tool.call = Some(
                    ToolCall::new(
                        tool.tool_call_id.clone(),
                        tool.name.clone(),
                        arguments,
                        tool.call_index,
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

    pub(super) fn finish(&mut self) -> Result<ModelResponse, ModelError> {
        let reason = self.finish_reason.ok_or_else(|| {
            super::failure::generated_error(
                ModelErrorKind::IncompleteResponse,
                super::failure::observed_delivery(self.observed_event),
            )
        })?;
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
        if self.slots.is_empty() && self.provider_replay.is_none() {
            return Err(assembler_error(ModelErrorKind::InvalidProviderResponse));
        }
        let mut parts = Vec::with_capacity(self.slots.len());
        for slot in std::mem::take(&mut self.slots) {
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
            self.provider_replay.take(),
        )
        .map_err(|_| assembler_error(ModelErrorKind::InvalidProviderResponse))
    }

    pub(super) fn into_partial(mut self, request_index: u32) -> Option<ModelPartial> {
        let mut content = Vec::with_capacity(self.slots.len());
        let mut incomplete_tool_calls = Vec::new();
        for (part_index, slot) in self.slots.into_iter().enumerate() {
            match slot {
                PartSlot::Text if !self.text.is_empty() => {
                    content.push(AssistantPart::Text(std::mem::take(&mut self.text).into()));
                }
                PartSlot::Reasoning if !self.reasoning.is_empty() => {
                    content.push(AssistantPart::Reasoning(
                        ReasoningContent::new(
                            Some(std::mem::take(&mut self.reasoning)),
                            None,
                            None,
                            None,
                        )
                        .expect("assembled reasoning is normalized"),
                    ));
                }
                PartSlot::Tool(index) => {
                    let tool = &mut self.tools[index];
                    match tool.call.take() {
                        Some(call) => content.push(AssistantPart::ToolCall(call)),
                        None => incomplete_tool_calls.push(IncompleteToolCall {
                            part_index,
                            call_index: tool.call_index,
                            tool_call_id: tool.tool_call_id.clone(),
                            tool_name: tool.name.clone(),
                            arguments_text: std::mem::take(&mut tool.arguments_text),
                        }),
                    }
                }
                PartSlot::Text | PartSlot::Reasoning => {}
            }
        }
        if content.is_empty() && incomplete_tool_calls.is_empty() && self.provider_replay.is_none()
        {
            return None;
        }
        Some(ModelPartial {
            request_index,
            content,
            incomplete_tool_calls,
            usage: self.usage,
            provider_replay: self.provider_replay,
        })
    }

    fn tool_mut(&mut self, tool_call_id: &ToolCallId) -> Result<&mut ToolAssembly, ModelError> {
        self.tools
            .iter_mut()
            .find(|tool| &tool.tool_call_id == tool_call_id)
            .ok_or_else(|| assembler_error(ModelErrorKind::InvalidProviderResponse))
    }
}

fn normalized_delta(delta: Text, ended_with_cr: &mut bool) -> Text {
    if delta.is_empty() {
        return delta;
    }
    let raw = delta.as_str();
    let text = if *ended_with_cr {
        raw.strip_prefix('\n').unwrap_or(raw)
    } else {
        raw
    };
    *ended_with_cr = raw.ends_with('\r');
    match normalize_model_text(text) {
        std::borrow::Cow::Borrowed(text) if text.len() == raw.len() => delta,
        text => Text::new(text),
    }
}

fn assembler_error(kind: ModelErrorKind) -> ModelError {
    let message = match kind {
        ModelErrorKind::UnexpectedToolCall => "unexpected tool call in model response",
        _ => "invalid model provider response",
    };
    let diagnostic = DiagnosticSummary::new(
        DiagnosticCode::ModelMalformedResponse,
        DiagnosticCategory::Model,
        Text::new(message),
        false,
    )
    .expect("valid diagnostic text");
    ModelError::started(kind, diagnostic)
}
