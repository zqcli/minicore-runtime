use std::collections::BTreeSet;

use thiserror::Error;

use crate::config::{SemanticLimits, SessionSpec};
use crate::ids::{ToolCallId, TurnId};
use crate::model::ModelFinishReason;
use crate::tools::ToolName;
use crate::value::{BoundedText, validate_json_size};

use super::entry::{
    AssistantMessageEntry, ConversationEntry, ConversationSeq, SummaryEntry, ToolResultEntry,
    TurnExecutionRecord, TurnTerminalEntry, UserMessageEntry,
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ConversationValidationError {
    #[error("conversation limits are invalid")]
    InvalidLimits,
    #[error("conversation session specification is invalid")]
    InvalidSpec,
    #[error("conversation sequence has a gap")]
    SequenceGap,
    #[error("conversation sequence overflowed")]
    SequenceOverflow,
    #[error("a conversation turn is already active")]
    ActiveTurnExists,
    #[error("conversation entry requires an active turn")]
    MissingActiveTurn,
    #[error("conversation entry turn does not match the active turn")]
    TurnMismatch,
    #[error("conversation user input is invalid")]
    InvalidUserInput,
    #[error("conversation model does not match the session specification")]
    ModelMismatch,
    #[error("conversation reasoning does not match the session specification")]
    ReasoningMismatch,
    #[error("conversation tool-round limit is invalid")]
    InvalidToolRounds,
    #[error("conversation assistant content is invalid")]
    InvalidAssistantContent,
    #[error("conversation assistant finish shape is invalid")]
    InvalidAssistantShape,
    #[error("conversation tool call is invalid")]
    InvalidToolCall,
    #[error("conversation tool is not enabled")]
    ToolNotEnabled,
    #[error("conversation tool name exceeds its limit")]
    ToolNameTooLong,
    #[error("conversation tool input exceeds its limit")]
    ToolInputTooLarge,
    #[error("conversation tool call ID is duplicated")]
    DuplicateToolCallId,
    #[error("conversation tool call order is invalid")]
    InvalidToolCallOrder,
    #[error("conversation tool exchange is incomplete")]
    IncompleteToolExchange,
    #[error("conversation turn is in an invalid phase")]
    InvalidPhase,
    #[error("conversation tool result has no pending tool call")]
    ToolResultWithoutPending,
    #[error("conversation tool result does not match the pending call")]
    ToolResultMismatch,
    #[error("conversation tool output exceeds its limit")]
    ToolOutputTooLarge,
    #[error("conversation terminal has no active turn")]
    TerminalWithoutActiveTurn,
    #[error("conversation terminal turn does not match the active turn")]
    TerminalTurnMismatch,
    #[error("conversation terminal has unresolved tool calls")]
    TerminalWithPendingTools,
    #[error("completed conversation turn is missing its final assistant")]
    MissingFinalAssistant,
    #[error("conversation summary cannot be written during a turn")]
    SummaryDuringActiveTurn,
    #[error("conversation summary boundary is invalid")]
    SummaryInvalidBoundary,
    #[error("conversation summary boundary does not advance")]
    SummaryNotAdvanced,
    #[error("conversation summary text is invalid")]
    InvalidSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingToolCall {
    pub(crate) turn_id: TurnId,
    pub(crate) tool_call_id: ToolCallId,
    pub(crate) tool_name: ToolName,
    pub(crate) call_index: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveTurnPhase {
    AwaitingAssistant,
    AwaitingToolResults,
    FinalAssistant,
}

#[derive(Clone, Debug)]
pub(crate) struct ConversationValidator {
    spec: SessionSpec,
    limits: SemanticLimits,
    head: ConversationSeq,
    active_turn: Option<TurnId>,
    active_phase: Option<ActiveTurnPhase>,
    pending_tools: Vec<PendingToolCall>,
    seen_tool_call_ids: BTreeSet<ToolCallId>,
    terminal_boundaries: BTreeSet<ConversationSeq>,
    last_summary_through: Option<ConversationSeq>,
}

impl ConversationValidator {
    pub(crate) fn new(
        spec: SessionSpec,
        limits: SemanticLimits,
    ) -> Result<Self, ConversationValidationError> {
        limits
            .validate()
            .map_err(|_| ConversationValidationError::InvalidLimits)?;
        spec.validate(&limits)
            .map_err(|_| ConversationValidationError::InvalidSpec)?;
        Ok(Self {
            spec,
            limits,
            head: ConversationSeq::ZERO,
            active_turn: None,
            active_phase: None,
            pending_tools: Vec::new(),
            seen_tool_call_ids: BTreeSet::new(),
            terminal_boundaries: BTreeSet::new(),
            last_summary_through: None,
        })
    }

    pub(crate) fn validate_batch(
        &self,
        entries: &[ConversationEntry],
    ) -> Result<Self, ConversationValidationError> {
        let mut candidate = self.clone();
        for entry in entries {
            candidate.apply(entry)?;
        }
        Ok(candidate)
    }

    pub(crate) const fn head(&self) -> ConversationSeq {
        self.head
    }

    pub(crate) const fn active_turn_id(&self) -> Option<TurnId> {
        self.active_turn
    }

    pub(crate) fn unresolved_tool_calls(&self) -> &[PendingToolCall] {
        &self.pending_tools
    }

    pub(crate) fn terminal_boundaries(&self) -> &BTreeSet<ConversationSeq> {
        &self.terminal_boundaries
    }

    pub(crate) const fn latest_summary_through(&self) -> Option<ConversationSeq> {
        self.last_summary_through
    }

    fn apply(&mut self, entry: &ConversationEntry) -> Result<(), ConversationValidationError> {
        let expected = self
            .head
            .next()
            .ok_or(ConversationValidationError::SequenceOverflow)?;
        if entry.seq() != expected {
            return Err(ConversationValidationError::SequenceGap);
        }
        match entry {
            ConversationEntry::UserMessage(value) => self.apply_user(value)?,
            ConversationEntry::AssistantMessage(value) => self.apply_assistant(value)?,
            ConversationEntry::ToolResult(value) => self.apply_tool_result(value)?,
            ConversationEntry::Summary(value) => self.apply_summary(value)?,
            ConversationEntry::TurnTerminal(value) => self.apply_terminal(value)?,
        }
        self.head = entry.seq();
        Ok(())
    }

    fn apply_user(&mut self, entry: &UserMessageEntry) -> Result<(), ConversationValidationError> {
        if self.active_turn.is_some() {
            return Err(ConversationValidationError::ActiveTurnExists);
        }
        self.validate_user_input(&entry.input.text)?;
        self.validate_execution(&entry.execution)?;
        self.active_turn = Some(entry.turn_id);
        self.active_phase = Some(ActiveTurnPhase::AwaitingAssistant);
        Ok(())
    }

    fn apply_assistant(
        &mut self,
        entry: &AssistantMessageEntry,
    ) -> Result<(), ConversationValidationError> {
        let active_turn = self
            .active_turn
            .ok_or(ConversationValidationError::MissingActiveTurn)?;
        if entry.turn_id != active_turn {
            return Err(ConversationValidationError::TurnMismatch);
        }
        if self.active_phase == Some(ActiveTurnPhase::FinalAssistant) {
            return Err(ConversationValidationError::InvalidPhase);
        }
        if !self.pending_tools.is_empty() {
            return Err(ConversationValidationError::IncompleteToolExchange);
        }
        if self.active_phase != Some(ActiveTurnPhase::AwaitingAssistant) {
            return Err(ConversationValidationError::InvalidPhase);
        }
        if entry.model != self.spec.model {
            return Err(ConversationValidationError::ModelMismatch);
        }
        self.validate_optional_text(
            entry.text.as_ref(),
            self.limits.max_model_text_bytes_per_round,
        )?;
        self.validate_optional_text(
            entry.reasoning.as_ref(),
            self.limits.max_model_reasoning_bytes_per_round,
        )?;
        let has_text = entry.text.is_some();
        let has_reasoning = entry.reasoning.is_some();
        let has_tool_calls = !entry.tool_calls.is_empty();
        if !has_text && !has_reasoning && !has_tool_calls {
            return Err(ConversationValidationError::InvalidAssistantContent);
        }
        if (has_tool_calls
            && !matches!(
                entry.finish_reason,
                ModelFinishReason::ToolCalls | ModelFinishReason::Unknown
            ))
            || (!has_tool_calls && entry.finish_reason == ModelFinishReason::ToolCalls)
        {
            return Err(ConversationValidationError::InvalidAssistantShape);
        }
        if entry.tool_calls.len() > self.limits.max_tool_count {
            return Err(ConversationValidationError::InvalidToolCall);
        }

        let mut pending = Vec::with_capacity(entry.tool_calls.len());
        for (position, call) in entry.tool_calls.iter().enumerate() {
            let expected_index = u32::try_from(position)
                .map_err(|_| ConversationValidationError::InvalidToolCallOrder)?;
            if call.call_index() != expected_index {
                return Err(ConversationValidationError::InvalidToolCallOrder);
            }
            call.validate()
                .map_err(|_| ConversationValidationError::InvalidToolCall)?;
            if validate_json_size(call.arguments(), self.limits.max_tool_input_bytes).is_err() {
                return Err(ConversationValidationError::ToolInputTooLarge);
            }
            if call.name().as_str().len() > self.limits.max_tool_name_bytes {
                return Err(ConversationValidationError::ToolNameTooLong);
            }
            if !self.spec.enabled_tools.contains(call.name()) {
                return Err(ConversationValidationError::ToolNotEnabled);
            }
            if !self.seen_tool_call_ids.insert(call.tool_call_id().clone()) {
                return Err(ConversationValidationError::DuplicateToolCallId);
            }
            pending.push(PendingToolCall {
                turn_id: entry.turn_id,
                tool_call_id: call.tool_call_id().clone(),
                tool_name: call.name().clone(),
                call_index: call.call_index(),
            });
        }
        self.pending_tools = pending;
        self.active_phase = Some(if entry.tool_calls.is_empty() {
            ActiveTurnPhase::FinalAssistant
        } else {
            ActiveTurnPhase::AwaitingToolResults
        });
        Ok(())
    }

    fn apply_tool_result(
        &mut self,
        entry: &ToolResultEntry,
    ) -> Result<(), ConversationValidationError> {
        let active_turn = self
            .active_turn
            .ok_or(ConversationValidationError::MissingActiveTurn)?;
        if entry.turn_id != active_turn {
            return Err(ConversationValidationError::TurnMismatch);
        }
        let expected = self
            .pending_tools
            .first()
            .ok_or(ConversationValidationError::ToolResultWithoutPending)?;
        if self.active_phase != Some(ActiveTurnPhase::AwaitingToolResults) {
            return Err(ConversationValidationError::InvalidPhase);
        }
        if entry.tool_call_id != expected.tool_call_id || entry.tool_name != expected.tool_name {
            return Err(ConversationValidationError::ToolResultMismatch);
        }
        if entry.content.byte_len() > self.limits.max_tool_output_bytes {
            return Err(ConversationValidationError::ToolOutputTooLarge);
        }
        self.pending_tools.remove(0);
        if self.pending_tools.is_empty() {
            self.active_phase = Some(ActiveTurnPhase::AwaitingAssistant);
        }
        Ok(())
    }

    fn apply_terminal(
        &mut self,
        entry: &TurnTerminalEntry,
    ) -> Result<(), ConversationValidationError> {
        let active_turn = self
            .active_turn
            .ok_or(ConversationValidationError::TerminalWithoutActiveTurn)?;
        if entry.turn_id != active_turn {
            return Err(ConversationValidationError::TerminalTurnMismatch);
        }
        if !self.pending_tools.is_empty() {
            return Err(ConversationValidationError::TerminalWithPendingTools);
        }
        if matches!(&entry.terminal, super::entry::TurnTerminal::Completed)
            && self.active_phase != Some(ActiveTurnPhase::FinalAssistant)
        {
            return Err(ConversationValidationError::MissingFinalAssistant);
        }
        self.active_turn = None;
        self.active_phase = None;
        self.terminal_boundaries.insert(entry.seq);
        Ok(())
    }

    fn apply_summary(&mut self, entry: &SummaryEntry) -> Result<(), ConversationValidationError> {
        if self.active_turn.is_some() {
            return Err(ConversationValidationError::SummaryDuringActiveTurn);
        }
        if !self.valid_nonempty_text(&entry.summary, self.limits.max_model_text_bytes_per_round) {
            return Err(ConversationValidationError::InvalidSummary);
        }
        if entry.through > self.head || !self.terminal_boundaries.contains(&entry.through) {
            return Err(ConversationValidationError::SummaryInvalidBoundary);
        }
        if self
            .last_summary_through
            .is_some_and(|last| entry.through <= last)
        {
            return Err(ConversationValidationError::SummaryNotAdvanced);
        }
        self.last_summary_through = Some(entry.through);
        Ok(())
    }

    fn validate_user_input(&self, text: &BoundedText) -> Result<(), ConversationValidationError> {
        if !self.valid_nonempty_text(text, self.limits.max_user_input_bytes) {
            return Err(ConversationValidationError::InvalidUserInput);
        }
        Ok(())
    }

    fn validate_execution(
        &self,
        execution: &TurnExecutionRecord,
    ) -> Result<(), ConversationValidationError> {
        if execution.model != self.spec.model {
            return Err(ConversationValidationError::ModelMismatch);
        }
        if execution.reasoning != self.spec.reasoning {
            return Err(ConversationValidationError::ReasoningMismatch);
        }
        if !(1..=self.spec.max_tool_rounds).contains(&execution.max_tool_rounds)
            || execution.max_tool_rounds > self.limits.max_tool_rounds
        {
            return Err(ConversationValidationError::InvalidToolRounds);
        }
        Ok(())
    }

    fn validate_optional_text(
        &self,
        text: Option<&BoundedText>,
        maximum: usize,
    ) -> Result<(), ConversationValidationError> {
        if text.is_some_and(|value| value.is_empty() || value.byte_len() > maximum) {
            return Err(ConversationValidationError::InvalidAssistantContent);
        }
        Ok(())
    }

    fn valid_nonempty_text(&self, text: &BoundedText, maximum: usize) -> bool {
        !text.is_empty() && text.byte_len() <= maximum
    }
}

#[cfg(test)]
mod tests;
