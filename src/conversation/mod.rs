pub(crate) mod compaction_candidate;
mod entry;
mod load;
mod log;
mod projection;
mod recovery;
pub(crate) mod session_log;
mod settlement;
mod state;
pub mod transcript;
mod validator;
mod view;

pub(crate) use crate::error::DurabilityClass;
pub(crate) use load::{LoadCompatibilityValidated, close_unopened_log};
pub(crate) use log::{
    AssistantMessageDraft, ConversationCloseOutcome, ConversationCommitError,
    ConversationCommitErrorKind, ConversationLog, SummaryDraft, TimestampSource, ToolResultDraft,
    UnsequencedEntry, UserMessageDraft,
};
pub(crate) use view::PromptConversationProjection;

pub use entry::{
    AssistantMessageEntry, ConversationEntry, ConversationSeq, SummaryEntry, ToolResultEntry,
    TurnExecutionRecord, TurnExecutionRecordError, TurnTerminal, TurnTerminalEntry,
    UserInputRecord, UserInputRecordError, UserMessageEntry,
};
pub use transcript::TranscriptPage;
pub use view::ConversationView;
