use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use crate::config::SemanticLimits;
use crate::conversation::{ConversationEntry, ConversationSeq};
use crate::ids::{SessionId, TurnId};
use crate::time::effective_deadline;
use crate::value::BoundedText;

use super::{
    CompactionCandidate, CompactionError, CompactionProposal, CompactionRequest, CompactionStrategy,
};

const MAX_COMPACTION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) struct CompactionDriver {
    strategy: Option<Arc<dyn CompactionStrategy>>,
    timeout: Duration,
    max_summary_bytes: usize,
}

/// P5-E2 actor code must require current_head == snapshot_head before CommitSummary.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ValidatedCompactionProposal {
    snapshot_head: ConversationSeq,
    through_seq: ConversationSeq,
    summary: BoundedText,
}

impl fmt::Debug for ValidatedCompactionProposal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedCompactionProposal")
            .field("snapshot_head", &self.snapshot_head)
            .field("through_seq", &self.through_seq)
            .field("summary_bytes", &self.summary.byte_len())
            .finish()
    }
}

impl ValidatedCompactionProposal {
    pub(crate) const fn snapshot_head(&self) -> ConversationSeq {
        self.snapshot_head
    }

    pub(crate) const fn through_seq(&self) -> ConversationSeq {
        self.through_seq
    }

    pub(crate) const fn summary(&self) -> &BoundedText {
        &self.summary
    }
}

impl CompactionDriver {
    pub(crate) fn new(
        strategy: Option<Arc<dyn CompactionStrategy>>,
        timeout: Duration,
        limits: SemanticLimits,
    ) -> Result<Self, CompactionError> {
        let max_summary_bytes = limits.max_model_text_bytes_per_round;
        if timeout.is_zero()
            || timeout > MAX_COMPACTION_TIMEOUT
            || max_summary_bytes == 0
            || max_summary_bytes > BoundedText::MAX_BYTES
        {
            return Err(CompactionError::InvalidRequest);
        }
        Ok(Self {
            strategy,
            timeout,
            max_summary_bytes,
        })
    }

    pub(crate) async fn run(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        candidate: CompactionCandidate,
        target_tokens: u64,
        turn_deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<ValidatedCompactionProposal, CompactionError> {
        if target_tokens == 0 {
            return Err(CompactionError::InvalidRequest);
        }
        let has_newer_completed_boundary = validate_candidate(&candidate)?;
        if !has_newer_completed_boundary {
            return Err(CompactionError::Unavailable);
        }
        let Some(strategy) = self.strategy.as_ref() else {
            return Err(CompactionError::Unavailable);
        };
        if cancellation.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        let deadline = effective_deadline(turn_deadline, self.timeout)
            .map_err(|_| CompactionError::DeadlineExceeded)?;
        if TokioInstant::now() >= deadline.tokio() {
            return Err(CompactionError::DeadlineExceeded);
        }

        let child_cancellation = cancellation.child_token();
        let request = CompactionRequest {
            session_id,
            turn_id,
            candidate: candidate.clone(),
            target_tokens,
            cancellation: child_cancellation.clone(),
            deadline: deadline.standard(),
        };
        let future = match catch_unwind(AssertUnwindSafe(|| strategy.compact(request))) {
            Ok(future) => future,
            Err(_) => {
                child_cancellation.cancel();
                return Err(CompactionError::Internal);
            }
        };
        let future = AssertUnwindSafe(future).catch_unwind();
        tokio::pin!(future);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                child_cancellation.cancel();
                return Err(CompactionError::Cancelled);
            }
            _ = tokio::time::sleep_until(deadline.tokio()) => {
                child_cancellation.cancel();
                return Err(CompactionError::DeadlineExceeded);
            }
            result = &mut future => result,
        };
        child_cancellation.cancel();
        match result {
            Ok(Ok(proposal)) => validate_proposal(candidate, proposal, self.max_summary_bytes),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(CompactionError::Internal),
        }
    }
}

fn validate_candidate(candidate: &CompactionCandidate) -> Result<bool, CompactionError> {
    if candidate.entries().is_empty() {
        return if candidate.head() == ConversationSeq::ZERO
            && candidate.completed_boundaries().is_empty()
        {
            Ok(false)
        } else {
            Err(CompactionError::InvalidRequest)
        };
    }
    if candidate.head() == ConversationSeq::ZERO {
        return Err(CompactionError::InvalidRequest);
    }

    let boundaries = candidate.completed_boundaries();
    let mut boundary_index = 0;
    let mut previous_entry = ConversationSeq::ZERO;
    let mut previous_boundary = None;
    let mut has_newer_completed_boundary = false;
    for entry in candidate.entries() {
        let expected = previous_entry
            .next()
            .ok_or(CompactionError::InvalidRequest)?;
        let entry_seq = entry.seq();
        if entry_seq != expected {
            return Err(CompactionError::InvalidRequest);
        }
        previous_entry = entry_seq;

        if let Some(boundary) = boundaries.get(boundary_index).copied() {
            if boundary > candidate.head()
                || previous_boundary.is_some_and(|previous| previous >= boundary)
                || boundary < entry_seq
            {
                return Err(CompactionError::InvalidRequest);
            }
            if boundary == entry_seq {
                if !matches!(entry, ConversationEntry::TurnTerminal(_)) {
                    return Err(CompactionError::InvalidRequest);
                }
                has_newer_completed_boundary |= candidate
                    .latest_summary_through()
                    .is_none_or(|through| boundary > through);
                previous_boundary = Some(boundary);
                boundary_index += 1;
            }
        }
    }
    if previous_entry != candidate.head() || boundary_index != boundaries.len() {
        return Err(CompactionError::InvalidRequest);
    }
    Ok(has_newer_completed_boundary)
}

fn validate_proposal(
    candidate: CompactionCandidate,
    proposal: CompactionProposal,
    max_summary_bytes: usize,
) -> Result<ValidatedCompactionProposal, CompactionError> {
    let proposal_is_terminal = candidate
        .entries()
        .binary_search_by_key(&proposal.through_seq, ConversationEntry::seq)
        .ok()
        .and_then(|index| candidate.entries().get(index))
        .is_some_and(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)));
    if proposal.through_seq > candidate.head()
        || candidate
            .latest_summary_through()
            .is_some_and(|through| proposal.through_seq <= through)
        || candidate
            .completed_boundaries()
            .binary_search(&proposal.through_seq)
            .is_err()
        || !proposal_is_terminal
        || !valid_summary(&proposal.summary, max_summary_bytes)
    {
        return Err(CompactionError::InvalidRequest);
    }
    Ok(ValidatedCompactionProposal {
        snapshot_head: candidate.head(),
        through_seq: proposal.through_seq,
        summary: proposal.summary,
    })
}

fn valid_summary(summary: &BoundedText, maximum: usize) -> bool {
    !summary.is_empty()
        && summary.byte_len() <= maximum
        && summary
            .as_str()
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

#[cfg(test)]
mod tests;
