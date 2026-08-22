use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::SemanticLimits;
use crate::conversation::ConversationView;
use crate::ids::{ContextSourceId, SessionId, SessionInstanceId, TurnId};
use crate::value::BoundedText;

#[derive(Clone)]
pub struct ContextRequest {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub turn_id: TurnId,
    pub model_round: u16,
    pub conversation: ConversationView,
    pub remaining_context_budget: u64,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

impl fmt::Debug for ContextRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextRequest")
            .field("session_id", &self.session_id)
            .field("instance_id", &self.instance_id)
            .field("turn_id", &self.turn_id)
            .field("model_round", &self.model_round)
            .field("conversation", &self.conversation)
            .field("remaining_context_budget", &self.remaining_context_budget)
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBundle {
    pub blocks: Vec<ContextBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBlock {
    pub source: ContextSourceId,
    pub slot: ContextSlot,
    pub priority: i16,
    pub content: BoundedText,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ContextSlot {
    ProjectInstructions,
    RetrievedKnowledge,
    TurnContext,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ContextError {
    #[error("context limits are invalid")]
    InvalidLimits,
    #[error("context bundle has too many blocks")]
    TooManyBlocks,
    #[error("context block exceeds the byte limit")]
    BlockTooLarge,
    #[error("context bundle exceeds the byte limit")]
    TotalTooLarge,
    #[error("context bundle contains a duplicate source")]
    DuplicateSource,
    #[error("context bundle byte count overflowed")]
    ByteCountOverflow,
    #[error("context provider was cancelled")]
    Cancelled,
    #[error("context provider deadline expired")]
    DeadlineExceeded,
    #[error("context provider is unavailable")]
    Unavailable,
    #[error("context provider failed internally")]
    Internal,
}

impl ContextBundle {
    /// Empty block content is allowed; only block count and byte limits apply.
    pub fn validate_and_sort(mut self, limits: &SemanticLimits) -> Result<Self, ContextError> {
        limits.validate().map_err(|_| ContextError::InvalidLimits)?;
        if self.blocks.len() > limits.max_context_blocks {
            return Err(ContextError::TooManyBlocks);
        }

        let mut sources = BTreeSet::new();
        let mut total = 0usize;
        for block in &self.blocks {
            if block.content.byte_len() > limits.max_context_bytes {
                return Err(ContextError::BlockTooLarge);
            }
            if !sources.insert(block.source.clone()) {
                return Err(ContextError::DuplicateSource);
            }
            total = total
                .checked_add(block.content.byte_len())
                .ok_or(ContextError::ByteCountOverflow)?;
            if total > limits.max_context_bytes {
                return Err(ContextError::TotalTooLarge);
            }
        }

        self.blocks.sort_by(|left, right| {
            left.slot
                .cmp(&right.slot)
                .then_with(|| right.priority.cmp(&left.priority))
                .then_with(|| left.source.cmp(&right.source))
        });
        Ok(self)
    }
}

pub type ContextFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ContextBundle, ContextError>> + Send + 'a>>;

pub trait ContextProvider: Send + Sync + 'static {
    fn provide<'a>(&'a self, request: ContextRequest) -> ContextFuture<'a>;
}
