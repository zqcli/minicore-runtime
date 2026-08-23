use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::conversation::ConversationSeq;
use crate::ids::{SessionId, TurnId};
use crate::value::BoundedText;

use super::CompactionCandidate;

#[derive(Clone)]
pub struct CompactionRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub candidate: CompactionCandidate,
    pub target_tokens: u64,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

impl fmt::Debug for CompactionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionRequest")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("candidate", &self.candidate)
            .field("target_tokens", &self.target_tokens)
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionProposal {
    pub through_seq: ConversationSeq,
    pub summary: BoundedText,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CompactionError {
    #[error("compaction request is invalid")]
    InvalidRequest,
    #[error("compaction candidate is unavailable")]
    Unavailable,
    #[error("compaction was cancelled")]
    Cancelled,
    #[error("compaction deadline expired")]
    DeadlineExceeded,
    #[error("compaction strategy failed internally")]
    Internal,
}

pub type CompactionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompactionProposal, CompactionError>> + Send + 'a>>;

pub trait CompactionStrategy: Send + Sync + 'static {
    fn compact<'a>(&'a self, request: CompactionRequest) -> CompactionFuture<'a>;
}
