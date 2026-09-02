use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::history::HistoryView;
use crate::ids::LoopId;
use crate::model::{ModelDescriptor, ModelMessage, ReasoningPreference};
use crate::tools::ToolSpec;

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
