use super::conversation::{
    CompactionConversationView, ConversationLog, ConversationSummary, PromptConversationView,
};
use crate::model_v2::ModelMessage;

const _: for<'a> fn(&'a CompactionConversationView) -> Option<&'a ConversationSummary> =
    CompactionConversationView::latest_summary;
const _: for<'a> fn(&'a CompactionConversationView) -> &'a [ModelMessage] =
    CompactionConversationView::completed_messages;
const _: for<'a> fn(&'a CompactionConversationView) -> &'a [ModelMessage] =
    CompactionConversationView::current_turn_messages;
const _: for<'a> fn(&'a PromptConversationView) -> Option<&'a ConversationSummary> =
    PromptConversationView::latest_summary;
const _: fn(&ConversationLog) = |log| {
    std::mem::drop(log.compaction_view());
};
