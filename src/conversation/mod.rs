mod entry;
mod log;
mod projection;
mod state;
mod validator;

const _: () = {
    // Temporary compile anchor; RunnerProtocol draft construction will replace it.
    let _ = std::mem::size_of::<log::ConversationLog>();
    let _ = log::ConversationLog::initialize;
    let _ = log::ConversationLog::append_validated;
    let _ = log::ConversationLog::projection;
    let _ = log::ConversationLog::head;
    let _ = log::UnsequencedEntry::UserMessage;
    let _ = log::UnsequencedEntry::AssistantMessage;
    let _ = log::UnsequencedEntry::ToolResult;
    let _ = log::UnsequencedEntry::Summary;
    let _ = log::UnsequencedEntry::TurnTerminal;
    let _ = log::ConversationCommitError::kind;
    let _ = log::ConversationCommitError::validation_error;
    let _ = projection::PromptProjection::head;
    let _ = projection::PromptProjection::entries;
    let _ = projection::PromptProjection::latest_summary;
    let _ = projection::PromptProjection::latest_summary_through;
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
