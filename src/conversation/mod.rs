mod entry;
mod validator;

const _: () = {
    // Temporary compile anchor; P2-C ConversationLog will replace it.
    let _ = std::mem::size_of::<validator::ConversationValidator>();
    let _ = validator::ConversationValidator::new;
    let _ = validator::ConversationValidator::validate_batch;
    let _ = validator::ConversationValidator::head;
    let _ = validator::ConversationValidator::active_turn_id;
    let _ = validator::ConversationValidator::unresolved_tool_calls;
    let _ = validator::ConversationValidator::terminal_boundaries;
    let _ = validator::ConversationValidator::latest_summary_through;
};

pub use entry::{
    AssistantMessageEntry, ConversationEntry, ConversationSeq, SummaryEntry, ToolResultEntry,
    TurnExecutionRecord, TurnExecutionRecordError, TurnTerminal, TurnTerminalEntry,
    UserInputRecord, UserInputRecordError, UserMessageEntry,
};
