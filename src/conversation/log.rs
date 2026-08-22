use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use crate::config::{KernelConfig, SessionManifest};
use crate::ids::{SessionId, ToolCallId, TurnId};
use crate::model::{ModelFinishReason, ModelRef, ToolCall, Usage};
use crate::storage::{AppendReceipt, LogFuture, SessionLog, SessionLogError, SessionLogErrorKind};
use crate::time::{Timestamp, TimestampError};
use crate::tools::{ToolName, ToolResultOutcome};
use crate::value::BoundedText;

use super::entry::{
    AssistantMessageEntry, ConversationEntry, ConversationSeq, SummaryEntry, ToolResultEntry,
    TurnExecutionRecord, TurnTerminal, TurnTerminalEntry, UserInputRecord, UserMessageEntry,
};
use super::load::PendingConversationLoad;
use super::projection::PromptProjection;
use super::recovery::RecoveryPlan;
use super::state::ConversationState;
use super::transcript::{TranscriptPage, valid_page_contract};
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
    CompatibilityProofMismatch,
    SessionIdMismatch,
    ReplayInvalid,
    RecoveryUncertain,
    TranscriptLimit,
    TranscriptInvalid,
    SequenceOverflow,
    Timestamp,
    Validation,
    ContractViolation,
    DurabilityUnknown,
    Log(SessionLogErrorKind),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConversationCloseOutcome {
    Known(SessionLogError),
    Timeout,
    Panic,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ConversationCommitError {
    kind: ConversationCommitErrorKind,
    validation: Option<ConversationValidationError>,
    primary_log: Option<SessionLogError>,
    secondary_close: Option<ConversationCloseOutcome>,
    session_id_mismatch: Option<(SessionId, SessionId)>,
}

impl fmt::Debug for ConversationCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversationCommitError")
            .field("kind", &self.kind)
            .field("has_validation", &self.validation_error().is_some())
            .field("has_primary_log", &self.primary_log_error().is_some())
            .field("secondary_close_kind", &self.secondary_close_kind())
            .field(
                "has_session_id_mismatch",
                &self.session_id_mismatch().is_some(),
            )
            .finish()
    }
}

impl ConversationCommitError {
    fn new(kind: ConversationCommitErrorKind) -> Self {
        Self {
            kind,
            validation: None,
            primary_log: None,
            secondary_close: None,
            session_id_mismatch: None,
        }
    }
    fn validation(error: ConversationValidationError) -> Self {
        Self {
            kind: ConversationCommitErrorKind::Validation,
            validation: Some(error),
            primary_log: None,
            secondary_close: None,
            session_id_mismatch: None,
        }
    }
    pub(super) fn with_session_id_mismatch(expected: SessionId, actual: SessionId) -> Self {
        Self {
            kind: ConversationCommitErrorKind::SessionIdMismatch,
            validation: None,
            primary_log: None,
            secondary_close: None,
            session_id_mismatch: Some((expected, actual)),
        }
    }
    pub(super) fn with_primary_log(mut self, error: SessionLogError) -> Self {
        self.primary_log = Some(error);
        self
    }

    pub(super) fn with_kind(mut self, kind: ConversationCommitErrorKind) -> Self {
        self.kind = kind;
        self
    }

    pub(crate) fn with_secondary_close(
        mut self,
        outcome: Option<ConversationCloseOutcome>,
    ) -> Self {
        self.secondary_close = outcome;
        self
    }
    pub(crate) const fn kind(&self) -> ConversationCommitErrorKind {
        self.kind
    }

    pub(crate) const fn validation_error(&self) -> Option<ConversationValidationError> {
        self.validation
    }

    pub(crate) fn primary_log_error(&self) -> Option<&SessionLogError> {
        self.primary_log.as_ref()
    }

    pub(crate) fn secondary_close_error(&self) -> Option<&SessionLogError> {
        match self.secondary_close_outcome() {
            Some(ConversationCloseOutcome::Known(error)) => Some(error),
            Some(ConversationCloseOutcome::Timeout | ConversationCloseOutcome::Panic) | None => {
                None
            }
        }
    }

    pub(crate) fn secondary_close_outcome(&self) -> Option<&ConversationCloseOutcome> {
        self.secondary_close.as_ref()
    }

    pub(crate) fn secondary_close_kind(&self) -> Option<SessionLogErrorKind> {
        if let Some(error) = self.secondary_close_error() {
            return Some(error.kind());
        }
        match self.secondary_close_outcome() {
            Some(ConversationCloseOutcome::Timeout) => Some(SessionLogErrorKind::UnknownOutcome),
            Some(ConversationCloseOutcome::Panic) => Some(SessionLogErrorKind::Internal),
            Some(ConversationCloseOutcome::Known(_)) | None => None,
        }
    }

    pub(crate) const fn session_id_mismatch(&self) -> Option<(SessionId, SessionId)> {
        self.session_id_mismatch
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
    max_replay_page_size: usize,
    max_transcript_page_size: usize,
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
        let kernel_valid = kernel.validate().is_ok();
        if !kernel_valid {
            return Err(super::load::close_after_error(
                inner,
                FAILED_OPEN_CLOSE_TIMEOUT,
                commit_error(ConversationCommitErrorKind::InvalidConfiguration),
            )
            .await);
        }
        if manifest.validate(&kernel.limits).is_err() {
            return Err(super::load::close_after_error(
                inner,
                kernel.log_operation_timeout,
                commit_error(ConversationCommitErrorKind::InvalidManifest),
            )
            .await);
        }
        let state = match ConversationState::new(manifest.spec.clone(), kernel.limits.clone()) {
            Ok(state) => state,
            Err(_) => {
                return Err(super::load::close_after_error(
                    inner,
                    kernel.log_operation_timeout,
                    commit_error(ConversationCommitErrorKind::InvalidManifest),
                )
                .await);
            }
        };
        let outcome = run_log_operation(kernel.log_operation_timeout, || {
            inner.initialize(manifest.clone())
        })
        .await;
        match outcome {
            OperationOutcome::Success(head) if head == ConversationSeq::ZERO => {}
            OperationOutcome::Success(_) => {
                return Err(super::load::close_after_error(
                    inner,
                    kernel.log_operation_timeout,
                    commit_error(ConversationCommitErrorKind::ContractViolation),
                )
                .await);
            }
            OperationOutcome::Known(error) => {
                return Err(super::load::close_after_error(
                    inner,
                    kernel.log_operation_timeout,
                    map_log_error(error),
                )
                .await);
            }
            OperationOutcome::Timeout | OperationOutcome::Panic => {
                return Err(super::load::close_after_error(
                    inner,
                    kernel.log_operation_timeout,
                    commit_error(ConversationCommitErrorKind::DurabilityUnknown),
                )
                .await);
            }
        };
        Ok(Self {
            inner,
            state,
            max_replay_page_size: kernel.limits.max_replay_page_size,
            max_transcript_page_size: kernel.limits.max_transcript_page_size,
            log_operation_timeout: kernel.log_operation_timeout,
            timestamp_source,
            closed: false,
            durability_unknown: false,
        })
    }
    pub(crate) async fn begin_load(
        expected_session_id: SessionId,
        inner: Box<dyn SessionLog>,
        kernel: KernelConfig,
        timestamp_source: TimestampSource,
    ) -> Result<PendingConversationLoad, ConversationCommitError> {
        PendingConversationLoad::begin_load(expected_session_id, inner, kernel, timestamp_source)
            .await
    }

    pub(super) fn from_loaded_parts(
        inner: Box<dyn SessionLog>,
        state: ConversationState,
        limits: crate::config::SemanticLimits,
        log_operation_timeout: Duration,
        timestamp_source: TimestampSource,
    ) -> Self {
        Self {
            inner,
            state,
            max_replay_page_size: limits.max_replay_page_size,
            max_transcript_page_size: limits.max_transcript_page_size,
            log_operation_timeout,
            timestamp_source,
            closed: false,
            durability_unknown: false,
        }
    }
    pub(crate) async fn append_validated(
        &mut self,
        drafts: Vec<UnsequencedEntry>,
    ) -> Result<CommittedBatch, ConversationCommitError> {
        if self.durability_unknown {
            return Err(commit_error(ConversationCommitErrorKind::DurabilityUnknown));
        }
        if self.closed {
            return Err(commit_error(ConversationCommitErrorKind::Closed));
        }
        if drafts.is_empty() {
            return Err(commit_error(ConversationCommitErrorKind::EmptyBatch));
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
                return Err(self.mark_durability_unknown(Some(error)));
            }
            OperationOutcome::Known(error) => return Err(map_log_error(error)),
            OperationOutcome::Timeout | OperationOutcome::Panic => {
                return Err(self.mark_durability_unknown(None));
            }
        };
        if !valid_receipt(&receipt, expected_head, candidate.head(), entries.len()) {
            return Err(self.mark_durability_unknown(None));
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
    pub(crate) fn recovery_plan(&self) -> Option<RecoveryPlan> {
        RecoveryPlan::from_state(&self.state)
    }
    pub(crate) const fn head(&self) -> ConversationSeq {
        self.state.head()
    }

    pub(crate) async fn transcript(
        &mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> Result<TranscriptPage, ConversationCommitError> {
        if self.closed {
            return Err(commit_error(ConversationCommitErrorKind::Closed));
        }
        debug_assert!(self.max_replay_page_size > 0);
        if limit == 0 || limit > self.max_transcript_page_size {
            return Err(commit_error(ConversationCommitErrorKind::TranscriptLimit));
        }
        if after.is_some_and(|cursor| {
            cursor > self.head()
                || (cursor != ConversationSeq::ZERO && !self.state.contains_seq(cursor))
        }) {
            return Err(commit_error(ConversationCommitErrorKind::TranscriptInvalid));
        }
        if self.durability_unknown {
            return Ok(self.confirmed_transcript(after, limit));
        }
        let outcome = run_log_operation(self.log_operation_timeout, || {
            self.inner.read_page(after, limit)
        })
        .await;
        let page = match outcome {
            OperationOutcome::Success(page) => page,
            OperationOutcome::Known(error)
                if error.kind() == SessionLogErrorKind::UnknownOutcome =>
            {
                return Err(self.mark_durability_unknown(Some(error)));
            }
            OperationOutcome::Known(error) => return Err(map_log_error(error)),
            OperationOutcome::Timeout | OperationOutcome::Panic => {
                return Err(self.mark_durability_unknown(None));
            }
        };
        let after_head = after.unwrap_or(ConversationSeq::ZERO);
        if page.observed_head != self.head()
            || !valid_page_contract(&page, after, limit)
            || !self.state.matches_confirmed_page(after, &page.entries)
            || (page.entries.is_empty() && after_head != self.head())
        {
            return Err(commit_error(ConversationCommitErrorKind::TranscriptInvalid));
        }
        Ok(TranscriptPage {
            entries: page.entries,
            next_after: page.next_after,
            observed_head: page.observed_head,
            complete: true,
        })
    }

    fn confirmed_transcript(&self, after: Option<ConversationSeq>, limit: usize) -> TranscriptPage {
        let entries = self.state.projection().entries();
        let start = after.map_or(0, |cursor| cursor.get() as usize);
        let end = start.saturating_add(limit).min(entries.len());
        let page_entries = entries[start..end].to_vec();
        let next_after = (end < entries.len()).then(|| entries[end - 1].seq());
        TranscriptPage {
            entries: page_entries,
            next_after,
            observed_head: self.head(),
            complete: false,
        }
    }

    pub(crate) async fn close(&mut self) -> Result<(), ConversationCommitError> {
        if self.closed {
            return Err(ConversationCommitError::new(
                ConversationCommitErrorKind::Closed,
            ));
        }
        self.closed = true;
        match run_log_operation(self.log_operation_timeout, || self.inner.close()).await {
            OperationOutcome::Success(()) => Ok(()),
            OperationOutcome::Known(error)
                if error.kind() == SessionLogErrorKind::UnknownOutcome =>
            {
                Err(self.mark_durability_unknown(Some(error)))
            }
            OperationOutcome::Known(error) => Err(map_log_error(error)),
            OperationOutcome::Timeout | OperationOutcome::Panic => {
                Err(self.mark_durability_unknown(None))
            }
        }
    }

    pub(super) async fn close_for_load(&mut self) -> Option<ConversationCloseOutcome> {
        self.closed = true;
        match run_log_operation(self.log_operation_timeout, || self.inner.close()).await {
            OperationOutcome::Success(()) => None,
            OperationOutcome::Known(error) => Some(ConversationCloseOutcome::Known(error)),
            OperationOutcome::Timeout => Some(ConversationCloseOutcome::Timeout),
            OperationOutcome::Panic => Some(ConversationCloseOutcome::Panic),
        }
    }

    fn mark_durability_unknown(
        &mut self,
        primary_log: Option<SessionLogError>,
    ) -> ConversationCommitError {
        self.durability_unknown = true;
        let error = ConversationCommitError::new(ConversationCommitErrorKind::DurabilityUnknown);
        match primary_log {
            Some(primary_log) => error.with_primary_log(primary_log),
            None => error,
        }
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

pub(super) enum OperationOutcome<T> {
    Success(T),
    Known(SessionLogError),
    Timeout,
    Panic,
}

pub(super) async fn run_log_operation<'a, T, F>(
    timeout: Duration,
    operation: F,
) -> OperationOutcome<T>
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

pub(super) fn commit_error(kind: ConversationCommitErrorKind) -> ConversationCommitError {
    ConversationCommitError::new(kind)
}

pub(super) const FAILED_OPEN_CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn map_log_error(error: SessionLogError) -> ConversationCommitError {
    if error.kind() == SessionLogErrorKind::UnknownOutcome {
        ConversationCommitError::new(ConversationCommitErrorKind::DurabilityUnknown)
            .with_primary_log(error)
    } else {
        ConversationCommitError::new(ConversationCommitErrorKind::Log(error.kind()))
            .with_primary_log(error)
    }
}

#[cfg(test)]
mod append_support;
#[cfg(test)]
mod append_tests;
#[cfg(test)]
mod load_support;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod transcript_close_tests;
