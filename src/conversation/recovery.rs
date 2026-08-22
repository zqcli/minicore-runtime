use crate::ids::TurnId;
use crate::model::Usage;
use crate::tools::ToolResultOutcome;
use crate::value::BoundedText;

use super::entry::TurnTerminal;
use super::log::{ToolResultDraft, TurnTerminalDraft, UnsequencedEntry};
use super::state::ConversationState;
use super::validator::PendingToolCall;

const RESTART_CANCELLED_CONTENT: &str = "tool call cancelled by restart";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryPlan {
    pub(crate) turn_id: TurnId,
    pub(crate) unresolved_tools: Vec<PendingToolCall>,
    pub(crate) terminal: TurnTerminal,
    max_tool_output_bytes: usize,
}

impl RecoveryPlan {
    pub(crate) fn from_state(state: &ConversationState) -> Option<Self> {
        let turn_id = state.active_turn_id()?;
        let unresolved_tools = state.unresolved_tool_calls().to_vec();
        Some(Self {
            turn_id,
            unresolved_tools,
            terminal: TurnTerminal::CancelledByRestart,
            max_tool_output_bytes: state.max_tool_output_bytes(),
        })
    }

    pub(crate) fn drafts(&self) -> Vec<UnsequencedEntry> {
        let length = self
            .max_tool_output_bytes
            .min(RESTART_CANCELLED_CONTENT.len())
            .max(1);
        let content = BoundedText::new(&RESTART_CANCELLED_CONTENT[..length])
            .expect("restart cancellation prefix is within bounds");
        let mut drafts = self
            .unresolved_tools
            .iter()
            .map(|pending| {
                UnsequencedEntry::ToolResult(ToolResultDraft {
                    turn_id: self.turn_id,
                    tool_call_id: pending.tool_call_id.clone(),
                    tool_name: pending.tool_name.clone(),
                    outcome: ToolResultOutcome::Cancelled,
                    content: content.clone(),
                })
            })
            .collect::<Vec<_>>();
        drafts.push(UnsequencedEntry::TurnTerminal(TurnTerminalDraft {
            turn_id: self.turn_id,
            terminal: self.terminal.clone(),
            usage: Usage::default(),
        }));
        drafts
    }
}
