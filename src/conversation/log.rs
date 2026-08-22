use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use crate::config::{KernelConfig, SessionManifest};
use crate::ids::{ToolCallId, TurnId};
use crate::model::{ModelFinishReason, ModelRef, ToolCall, Usage};
use crate::storage::{AppendReceipt, LogFuture, SessionLog, SessionLogError, SessionLogErrorKind};
use crate::time::{Timestamp, TimestampError};
use crate::tools::{ToolName, ToolResultOutcome};
use crate::value::BoundedText;

use super::entry::{
    AssistantMessageEntry, ConversationEntry, ConversationSeq, SummaryEntry, ToolResultEntry,
    TurnExecutionRecord, TurnTerminal, TurnTerminalEntry, UserInputRecord, UserMessageEntry,
};
use super::projection::PromptProjection;
use super::state::ConversationState;
use super::validator::ConversationValidationError;

pub(crate) type TimestampSource = Box<dyn Fn() -> Result<Timestamp, TimestampError> + Send>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UserMessageDraft {
    pub(crate) turn_id: TurnId,
    pub(crate) input: UserInputRecord,
    pub(crate) execution: TurnExecutionRecord,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssistantMessageDraft {
    pub(crate) turn_id: TurnId,
    pub(crate) model: ModelRef,
    pub(crate) text: Option<BoundedText>,
    pub(crate) reasoning: Option<BoundedText>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: Usage,
    pub(crate) finish_reason: ModelFinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolResultDraft {
    pub(crate) turn_id: TurnId,
    pub(crate) tool_call_id: ToolCallId,
    pub(crate) tool_name: ToolName,
    pub(crate) outcome: ToolResultOutcome,
    pub(crate) content: BoundedText,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SummaryDraft {
    pub(crate) through: ConversationSeq,
    pub(crate) summary: BoundedText,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TurnTerminalDraft {
    pub(crate) turn_id: TurnId,
    pub(crate) terminal: TurnTerminal,
    pub(crate) usage: Usage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UnsequencedEntry {
    UserMessage(UserMessageDraft),
    AssistantMessage(AssistantMessageDraft),
    ToolResult(ToolResultDraft),
    Summary(SummaryDraft),
    TurnTerminal(TurnTerminalDraft),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConversationCommitErrorKind {
    Closed,
    EmptyBatch,
    InvalidConfiguration,
    InvalidManifest,
    SequenceOverflow,
    Timestamp,
    Validation,
    ContractViolation,
    DurabilityUnknown,
    Log(SessionLogErrorKind),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConversationCommitError {
    kind: ConversationCommitErrorKind,
    validation: Option<ConversationValidationError>,
}

impl ConversationCommitError {
    fn new(kind: ConversationCommitErrorKind) -> Self {
        Self {
            kind,
            validation: None,
        }
    }

    fn validation(error: ConversationValidationError) -> Self {
        Self {
            kind: ConversationCommitErrorKind::Validation,
            validation: Some(error),
        }
    }

    pub(crate) const fn kind(&self) -> ConversationCommitErrorKind {
        self.kind
    }

    pub(crate) const fn validation_error(&self) -> Option<ConversationValidationError> {
        self.validation
    }
}

impl fmt::Display for ConversationCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "conversation commit error: {:?}", self.kind)
    }
}

impl std::error::Error for ConversationCommitError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommittedBatch {
    pub(crate) entries: Vec<ConversationEntry>,
    pub(crate) head: ConversationSeq,
    pub(crate) projection: Arc<PromptProjection>,
}

pub(crate) struct ConversationLog {
    inner: Box<dyn SessionLog>,
    state: ConversationState,
    log_operation_timeout: Duration,
    timestamp_source: TimestampSource,
    closed: bool,
    durability_unknown: bool,
}

impl ConversationLog {
    pub(crate) async fn initialize(
        mut inner: Box<dyn SessionLog>,
        manifest: SessionManifest,
        kernel: KernelConfig,
        timestamp_source: TimestampSource,
    ) -> Result<Self, ConversationCommitError> {
        kernel.validate().map_err(|_| {
            ConversationCommitError::new(ConversationCommitErrorKind::InvalidConfiguration)
        })?;
        manifest.validate(&kernel.limits).map_err(|_| {
            ConversationCommitError::new(ConversationCommitErrorKind::InvalidManifest)
        })?;
        let state =
            ConversationState::new(manifest.spec.clone(), kernel.limits.clone()).map_err(|_| {
                ConversationCommitError::new(ConversationCommitErrorKind::InvalidManifest)
            })?;
        let outcome =
            run_log_operation(kernel.log_operation_timeout, || inner.initialize(manifest)).await;
        match outcome {
            OperationOutcome::Success(head) if head == ConversationSeq::ZERO => {}
            OperationOutcome::Success(_) => {
                return Err(ConversationCommitError::new(
                    ConversationCommitErrorKind::ContractViolation,
                ));
            }
            OperationOutcome::Known(error) => return Err(map_log_error(error)),
            OperationOutcome::Timeout | OperationOutcome::Panic => {
                return Err(ConversationCommitError::new(
                    ConversationCommitErrorKind::DurabilityUnknown,
                ));
            }
        };
        Ok(Self {
            inner,
            state,
            log_operation_timeout: kernel.log_operation_timeout,
            timestamp_source,
            closed: false,
            durability_unknown: false,
        })
    }

    pub(crate) async fn append_validated(
        &mut self,
        drafts: Vec<UnsequencedEntry>,
    ) -> Result<CommittedBatch, ConversationCommitError> {
        if self.durability_unknown {
            return Err(ConversationCommitError::new(
                ConversationCommitErrorKind::DurabilityUnknown,
            ));
        }
        if self.closed {
            return Err(ConversationCommitError::new(
                ConversationCommitErrorKind::Closed,
            ));
        }
        if drafts.is_empty() {
            return Err(ConversationCommitError::new(
                ConversationCommitErrorKind::EmptyBatch,
            ));
        }
        let entries = self.materialize(drafts)?;
        let expected_head = self.state.head();
        let candidate = self
            .state
            .candidate(&entries)
            .map_err(ConversationCommitError::validation)?;
        let append_entries = entries.clone();
        let outcome = run_log_operation(self.log_operation_timeout, || {
            self.inner.append(expected_head, append_entries)
        })
        .await;
        let receipt = match outcome {
            OperationOutcome::Success(receipt) => receipt,
            OperationOutcome::Known(error)
                if error.kind() == SessionLogErrorKind::UnknownOutcome =>
            {
                return Err(self.mark_durability_unknown());
            }
            OperationOutcome::Known(error) => return Err(map_log_error(error)),
            OperationOutcome::Timeout | OperationOutcome::Panic => {
                return Err(self.mark_durability_unknown());
            }
        };
        if !valid_receipt(&receipt, expected_head, candidate.head(), entries.len()) {
            return Err(self.mark_durability_unknown());
        }
        self.state.commit(candidate);
        Ok(CommittedBatch {
            entries,
            head: self.state.head(),
            projection: Arc::new(self.state.projection().clone()),
        })
    }

    pub(crate) fn projection(&self) -> PromptProjection {
        self.state.projection().clone()
    }

    pub(crate) const fn head(&self) -> ConversationSeq {
        self.state.head()
    }

    fn mark_durability_unknown(&mut self) -> ConversationCommitError {
        self.durability_unknown = true;
        ConversationCommitError::new(ConversationCommitErrorKind::DurabilityUnknown)
    }

    fn materialize(
        &self,
        drafts: Vec<UnsequencedEntry>,
    ) -> Result<Vec<ConversationEntry>, ConversationCommitError> {
        let mut head = self.state.head();
        let mut entries = Vec::with_capacity(drafts.len());
        for draft in drafts {
            head = head.next().ok_or_else(|| {
                ConversationCommitError::new(ConversationCommitErrorKind::SequenceOverflow)
            })?;
            let created_at = (self.timestamp_source)().map_err(|_| {
                ConversationCommitError::new(ConversationCommitErrorKind::Timestamp)
            })?;
            entries.push(materialize_entry(draft, head, created_at));
        }
        Ok(entries)
    }
}

fn materialize_entry(
    draft: UnsequencedEntry,
    seq: ConversationSeq,
    created_at: Timestamp,
) -> ConversationEntry {
    match draft {
        UnsequencedEntry::UserMessage(draft) => ConversationEntry::UserMessage(UserMessageEntry {
            seq,
            turn_id: draft.turn_id,
            input: draft.input,
            execution: draft.execution,
            created_at,
        }),
        UnsequencedEntry::AssistantMessage(draft) => {
            ConversationEntry::AssistantMessage(AssistantMessageEntry {
                seq,
                turn_id: draft.turn_id,
                model: draft.model,
                text: draft.text,
                reasoning: draft.reasoning,
                tool_calls: draft.tool_calls,
                usage: draft.usage,
                finish_reason: draft.finish_reason,
                created_at,
            })
        }
        UnsequencedEntry::ToolResult(draft) => ConversationEntry::ToolResult(ToolResultEntry {
            seq,
            turn_id: draft.turn_id,
            tool_call_id: draft.tool_call_id,
            tool_name: draft.tool_name,
            outcome: draft.outcome,
            content: draft.content,
            created_at,
        }),
        UnsequencedEntry::Summary(draft) => ConversationEntry::Summary(SummaryEntry {
            seq,
            through: draft.through,
            summary: draft.summary,
            created_at,
        }),
        UnsequencedEntry::TurnTerminal(draft) => {
            ConversationEntry::TurnTerminal(TurnTerminalEntry {
                seq,
                turn_id: draft.turn_id,
                terminal: draft.terminal,
                usage: draft.usage,
                created_at,
            })
        }
    }
}

fn valid_receipt(
    receipt: &AppendReceipt,
    expected_head: ConversationSeq,
    new_head: ConversationSeq,
    appended: usize,
) -> bool {
    receipt.previous_head == expected_head
        && receipt.new_head == new_head
        && receipt.appended == appended
}

enum OperationOutcome<T> {
    Success(T),
    Known(SessionLogError),
    Timeout,
    Panic,
}

async fn run_log_operation<'a, T, F>(timeout: Duration, operation: F) -> OperationOutcome<T>
where
    F: FnOnce() -> LogFuture<'a, T>,
{
    let future = match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(future) => future,
        Err(_) => return OperationOutcome::Panic,
    };
    match tokio::time::timeout(timeout, AssertUnwindSafe(future).catch_unwind()).await {
        Err(_) => OperationOutcome::Timeout,
        Ok(Err(_)) => OperationOutcome::Panic,
        Ok(Ok(Ok(value))) => OperationOutcome::Success(value),
        Ok(Ok(Err(error))) => OperationOutcome::Known(error),
    }
}

fn map_log_error(error: SessionLogError) -> ConversationCommitError {
    if error.kind() == SessionLogErrorKind::UnknownOutcome {
        ConversationCommitError::new(ConversationCommitErrorKind::DurabilityUnknown)
    } else {
        ConversationCommitError::new(ConversationCommitErrorKind::Log(error.kind()))
    }
}

#[cfg(test)]
mod tests;
