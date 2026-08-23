mod entry;
mod load;
mod log;
mod projection;
mod recovery;
mod state;
pub mod transcript;
mod validator;
mod view;

const _: () = {
    // Temporary compile anchor; RunnerProtocol draft construction will replace it.
    let _ = std::mem::size_of::<log::ConversationLog>();
    let _ = log::ConversationLog::initialize;
    let _ = log::ConversationLog::begin_load;
    let _ = log::ConversationLog::append_validated;
    let _ = log::ConversationLog::projection;
    let _ = log::ConversationLog::head;
    let _ = log::ConversationLog::transcript;
    let _ = log::ConversationLog::close;
    let _ = load::PendingConversationLoad::finish;
    let _ = load::PendingConversationLoad::abort;
    let _ = load::PendingConversationLoad::manifest;
    let _ = LoadCompatibilityValidated::after_session_bindings_validation;
    let _ = view::ConversationView::from_confirmed;
    let _ = view::ConversationView::validated_prompt_projection;
    let _ = log::UnsequencedEntry::UserMessage;
    let _ = log::UnsequencedEntry::AssistantMessage;
    let _ = log::UnsequencedEntry::Summary;
};

pub(crate) use load::{LoadCompatibilityValidated, close_unopened_log};
pub(crate) use log::{
    ConversationCloseOutcome, ConversationCommitError, ConversationCommitErrorKind,
    ConversationLog, TimestampSource,
};
pub(crate) use view::PromptConversationProjection;

pub use entry::{
    AssistantMessageEntry, ConversationEntry, ConversationSeq, SummaryEntry, ToolResultEntry,
    TurnExecutionRecord, TurnExecutionRecordError, TurnTerminal, TurnTerminalEntry,
    UserInputRecord, UserInputRecordError, UserMessageEntry,
};
pub use transcript::TranscriptPage;
pub use view::ConversationView;
