use crate::ids::TurnId;
use crate::model::Usage;
use crate::tools::ToolResultOutcome;
use crate::value::BoundedText;

use super::entry::TurnTerminal;
use super::log::{ToolResultDraft, TurnTerminalDraft, UnsequencedEntry};
use super::state::ConversationState;

const CANCELLED_CONTENT: &str = "tool call cancelled before turn settlement";
const FAILED_CONTENT: &str = "tool call failed before turn settlement";

pub(super) fn build_settlement(
    state: &ConversationState,
    turn_id: TurnId,
    terminal: TurnTerminal,
    usage: Usage,
) -> Option<Vec<UnsequencedEntry>> {
    if state.active_turn_id() != Some(turn_id) {
        return None;
    }
    let failed = matches!(
        &terminal,
        TurnTerminal::Failed { .. } | TurnTerminal::Completed
    );
    let (outcome, text) = if failed {
        (ToolResultOutcome::Failed, FAILED_CONTENT)
    } else {
        (ToolResultOutcome::Cancelled, CANCELLED_CONTENT)
    };
    let length = state.max_tool_output_bytes().min(text.len()).max(1);
    let content = BoundedText::new(&text[..length]).ok()?;
    let mut drafts = state
        .unresolved_tool_calls()
        .iter()
        .map(|pending| {
            UnsequencedEntry::ToolResult(ToolResultDraft {
                turn_id,
                tool_call_id: pending.tool_call_id.clone(),
                tool_name: pending.tool_name.clone(),
                outcome,
                content: content.clone(),
            })
        })
        .collect::<Vec<_>>();
    drafts.push(UnsequencedEntry::TurnTerminal(TurnTerminalDraft {
        turn_id,
        terminal,
        usage,
    }));
    Some(drafts)
}

pub(super) fn confirmed_turn_usage(state: &ConversationState, turn_id: TurnId) -> Usage {
    let mut total = None;
    for entry in state.projection().entries() {
        let super::ConversationEntry::AssistantMessage(entry) = entry else {
            continue;
        };
        if entry.turn_id != turn_id {
            continue;
        }
        total = Some(match total {
            Some(current) => match sum_usage(current, entry.usage) {
                Some(total) => total,
                None => return Usage::default(),
            },
            None => entry.usage,
        });
    }
    total.unwrap_or_default()
}

fn sum_usage(left: Usage, right: Usage) -> Option<Usage> {
    Some(
        Usage::from_optional(
            sum_field(left.input_tokens(), right.input_tokens())?,
            sum_field(left.output_tokens(), right.output_tokens())?,
            sum_field(left.reasoning_tokens(), right.reasoning_tokens())?,
        )
        .with_cache_read_tokens(sum_field(
            left.cache_read_tokens(),
            right.cache_read_tokens(),
        )?)
        .with_cache_write_tokens(sum_field(
            left.cache_write_tokens(),
            right.cache_write_tokens(),
        )?)
        .with_provider_total_tokens(sum_field(
            left.provider_total_tokens(),
            right.provider_total_tokens(),
        )?),
    )
}

fn sum_field(left: Option<u64>, right: Option<u64>) -> Option<Option<u64>> {
    match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right).map(Some),
        _ => Some(None),
    }
}
