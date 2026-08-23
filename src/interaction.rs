use std::fmt;

use crate::ids::{InteractionId, ToolCallId, TurnId};
use crate::tools::{
    ApprovalDecision, ApprovalRequest, ToolInputAnswer, ToolInputRequest, ToolName, ToolValueError,
};

#[derive(Clone, Eq, PartialEq)]
pub struct PendingInteraction {
    pub interaction_id: InteractionId,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub kind: InteractionKind,
}

impl PendingInteraction {
    pub fn validate_answer(&self, answer: &InteractionAnswer) -> Result<(), ToolValueError> {
        answer.validate(&self.kind)
    }
}

impl fmt::Debug for PendingInteraction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingInteraction")
            .field("interaction_id", &self.interaction_id)
            .field("turn_id", &self.turn_id)
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum InteractionKind {
    Approval(ApprovalRequest),
    ToolInput(ToolInputRequest),
}

impl fmt::Debug for InteractionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Approval(request) => formatter.debug_tuple("Approval").field(request).finish(),
            Self::ToolInput(request) => formatter
                .debug_struct("ToolInput")
                .field("prompt_bytes", &request.prompt.byte_len())
                .field("choice_count", &request.choices.len())
                .field("answer_kind", &request.answer_kind)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum InteractionAnswer {
    Approval(ApprovalDecision),
    ToolInput(ToolInputAnswer),
}

impl InteractionAnswer {
    pub fn validate(&self, kind: &InteractionKind) -> Result<(), ToolValueError> {
        match (self, kind) {
            (Self::Approval(_), InteractionKind::Approval(_)) => Ok(()),
            (Self::ToolInput(answer), InteractionKind::ToolInput(request)) => {
                answer.validate(request)
            }
            _ => Err(ToolValueError::InvalidAnswer),
        }
    }
}

impl fmt::Debug for InteractionAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Approval(decision) => formatter.debug_tuple("Approval").field(decision).finish(),
            Self::ToolInput(ToolInputAnswer::Text(text)) => formatter
                .debug_struct("ToolInputText")
                .field("text_bytes", &text.byte_len())
                .finish(),
            Self::ToolInput(ToolInputAnswer::Choice { index }) => formatter
                .debug_struct("ToolInputChoice")
                .field("index", index)
                .finish(),
        }
    }
}
