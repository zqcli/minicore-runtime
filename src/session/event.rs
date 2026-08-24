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
}

/// A best-effort live event with the number of preceding events lost by the
/// bounded event queue.
///
/// `dropped_before` is attached to the next successfully delivered event and
/// is zero when no event was lost since the previous successful delivery. It
/// is informational and is not a replay cursor or a durability guarantee.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEventEnvelope {
    /// The durable Session identity that produced this event.
    pub session_id: SessionId,
    /// The loaded Session instance that produced this event.
    pub instance_id: SessionInstanceId,
    /// The saturating count of events lost immediately before this event.
    pub dropped_before: u64,
    /// The redacted, typed live event.
    pub event: SessionEvent,
}
