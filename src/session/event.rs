use crate::ids::{InteractionId, SessionId, SessionInstanceId, ToolCallId, TurnId};
use crate::model::Usage;
use crate::tools::{ToolName, ToolProgress, ToolResultOutcome};
use crate::value::BoundedText;

use super::state::SessionHealth;
use super::turn_handle::TurnOutcome;
use crate::interaction::PendingInteraction;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputChannel {
    Text,
    Reasoning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultSummary {
    pub outcome: ToolResultOutcome,
    pub content_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionResolutionSummary {
    Approved,
    Denied,
    InputProvided,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    TurnStarted {
        turn_id: TurnId,
    },
    ModelStarted {
        turn_id: TurnId,
        round: u16,
    },
    OutputDelta {
        turn_id: TurnId,
        channel: OutputChannel,
        delta: BoundedText,
    },
    ModelFinished {
        turn_id: TurnId,
        round: u16,
        usage: Usage,
    },
    ToolStarted {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        tool_name: ToolName,
    },
    ToolProgress {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        progress: ToolProgress,
    },
    ToolFinished {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        result: ToolResultSummary,
    },
    InteractionRequested {
        interaction: PendingInteraction,
    },
    InteractionResolved {
        interaction_id: InteractionId,
        resolution: InteractionResolutionSummary,
    },
    HealthChanged {
        health: SessionHealth,
    },
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
    EventsDropped {
        count: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEventEnvelope {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub event: SessionEvent,
}
