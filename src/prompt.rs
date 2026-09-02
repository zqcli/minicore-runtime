use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::history::{HistoryItem, HistoryView};
use crate::ids::LoopId;
use crate::model::{
    MAX_MODEL_MESSAGE_TEXT_BYTES, ModelDescriptor, ModelMessage, ReasoningPreference,
};
use crate::tools::ToolSpec;
use crate::value::BoundedText;

/// Domain error returned by a `PromptProvider` while preparing a request.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PromptError {
    #[error("prompt history cannot be projected for the requested model")]
    InvalidHistory,
    #[error("prompt preparation was cancelled")]
    Cancelled,
    #[error("prompt preparation produced no messages")]
    EmptyPrompt,
}

/// Everything a `PromptProvider` may observe about one model request.
///
/// `deadline` uses Tokio clock semantics so providers can drive `sleep_until`
/// directly without converting a wall-clock instant.
#[derive(Debug)]
pub struct PromptRequest<'a> {
    pub loop_id: LoopId,
    pub request_index: u32,
    pub history: HistoryView<'a>,
    pub model: &'a ModelDescriptor,
    pub reasoning: ReasoningPreference,
    pub tools: &'a [ToolSpec],
    pub cancellation: CancellationToken,
    pub deadline: tokio::time::Instant,
}

/// Final prompt output for a request.
///
/// Model, reasoning, tools, and limits are taken from the active
/// `ExecutionConfig` snapshot by Core; a provider only produces messages.
#[derive(Clone, Debug)]
pub struct PreparedPrompt {
    pub messages: Vec<ModelMessage>,
}

pub type PromptFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PreparedPrompt, PromptError>> + Send + 'a>>;

/// The request-time context and compaction boundary of an agent loop.
///
/// Replaces session-level prompt builders and durable compaction: everything
/// the host injects for one request (AGENTS.md, memory, RAG, skills, summary)
/// happens inside `prepare`.
pub trait PromptProvider: Send + Sync + 'static {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a>;
}

impl<T: PromptProvider + ?Sized> PromptProvider for Arc<T> {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
        (**self).prepare(request)
    }
}

/// The default host/loop prompt provider: a strict, order-preserving
/// projection of the loop history with an optional leading system prompt.
///
/// The host is trusted for history semantics (no ledger revalidation here):
/// per-message construction failures and structurally inconsistent history
/// surface as `PromptError::InvalidHistory`. Core still runs every provider's
/// output through the `max_prompt_messages` / non-empty budget at the request
/// boundary, and `ModelRequest` construction validates exchange-level
/// tool-result consistency.
///
/// The fixed summary prefix `Conversation summary:\n` is always preserved
/// verbatim; when the summary content would push the projected message past
/// the absolute `ModelMessage` text ceiling, only the *tail* of the content is
/// truncated at a UTF-8 character boundary (never mid-character).
pub struct DefaultPromptProvider {
    system_prompt: Option<BoundedText>,
}

impl DefaultPromptProvider {
    /// `Some` text is emitted first only when it is non-empty.
    pub fn new(system_prompt: Option<BoundedText>) -> Self {
        Self { system_prompt }
    }
}

impl std::fmt::Debug for DefaultPromptProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DefaultPromptProvider")
            .field(
                "system_prompt_bytes",
                &self.system_prompt.as_ref().map(BoundedText::byte_len),
            )
            .finish()
    }
}

impl PromptProvider for DefaultPromptProvider {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
        if request.cancellation.is_cancelled() || request.deadline <= tokio::time::Instant::now() {
            return Box::pin(async { Err(PromptError::Cancelled) });
        }
        let system = self.system_prompt.clone();
        Box::pin(async move {
            let mut messages =
                Vec::with_capacity(request.history.len() + usize::from(system.is_some()));
            if let Some(system) = system {
                if !system.is_empty() {
                    messages.push(
                        ModelMessage::system(system.as_str())
                            .map_err(|_| PromptError::InvalidHistory)?,
                    );
                }
            }
            for item in request.history.iter() {
                let message = match item {
                    HistoryItem::User(user) => ModelMessage::user(user.input.as_text()),
                    HistoryItem::Assistant(assistant) => assistant_message(assistant),
                    HistoryItem::ToolResult(result) => ModelMessage::tool_with_outcome(
                        result.call_id.clone(),
                        result.output.clone(),
                        result.outcome,
                    ),
                    HistoryItem::Summary(summary) => summary_message(summary),
                }
                .map_err(|_| PromptError::InvalidHistory)?;
                messages.push(message);
            }
            if messages.is_empty() {
                return Err(PromptError::EmptyPrompt);
            }
            Ok(PreparedPrompt { messages })
        })
    }
}

/// Assistant messages reuse their typed parts verbatim; an empty part list is
/// structurally inconsistent history.
fn assistant_message(
    assistant: &crate::history::AssistantHistory,
) -> Result<ModelMessage, crate::model::ModelValueError> {
    if assistant.content.is_empty() {
        return Err(crate::model::ModelValueError::EmptyAssistantParts);
    }
    ModelMessage::assistant(assistant.content.clone())
}

/// Fixed leading text of a projected summary; always preserved verbatim.
const SUMMARY_PREFIX: &str = "Conversation summary:\n";

/// Projects a summary, keeping the fixed prefix and truncating the content
/// tail at a UTF-8 character boundary when it would exceed the absolute
/// `ModelMessage` text ceiling.
fn summary_message(
    summary: &crate::history::SummaryHistory,
) -> Result<ModelMessage, crate::model::ModelValueError> {
    let content = summary.content.as_str();
    let mut end = content.len();
    if SUMMARY_PREFIX.len() + end > MAX_MODEL_MESSAGE_TEXT_BYTES {
        end = MAX_MODEL_MESSAGE_TEXT_BYTES - SUMMARY_PREFIX.len();
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
    }
    ModelMessage::system(format!("{SUMMARY_PREFIX}{}", &content[..end]))
}
