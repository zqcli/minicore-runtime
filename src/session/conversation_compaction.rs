use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::model_v2::{AssistantPart, ModelMessage};

pub(crate) struct CompactionConversationView {
    latest_summary: Option<ConversationSummary>,
    completed_messages: Arc<[ModelMessage]>,
    current_turn_messages: Arc<[ModelMessage]>,
    through_seq: Option<u64>,
    snapshot_seq: u64,
}

impl CompactionConversationView {
    pub(crate) fn latest_summary(&self) -> Option<&ConversationSummary> {
        self.latest_summary.as_ref()
    }

    pub(crate) fn completed_messages(&self) -> &[ModelMessage] {
        &self.completed_messages
    }

    pub(crate) fn current_turn_messages(&self) -> &[ModelMessage] {
        &self.current_turn_messages
    }

    pub(crate) const fn through_seq(&self) -> Option<u64> {
        self.through_seq
    }

    pub(crate) const fn snapshot_seq(&self) -> u64 {
        self.snapshot_seq
    }
}

impl ConversationLog {
    pub(crate) async fn compaction_view(
        &self,
    ) -> Result<CompactionConversationView, ConversationError> {
        build_compaction_view(&read_lock(&self.inner.state))
    }

    pub(crate) async fn append_summary(
        &self,
        expected_snapshot_seq: u64,
        through_seq: u64,
        timestamp: Timestamp,
        text: String,
    ) -> Result<u64, ConversationError> {
        let (reservation, state) = reserve_append_slot(&self.inner)?;
        if state.outstanding_tools.is_some() || state.pending_restart_terminal.is_some() {
            return Err(ConversationError::IncompleteToolExchange);
        }
        let prior_summary_seq = state
            .latest_summary
            .as_ref()
            .map_or(0, |summary| summary.through_seq);
        if state.max_seq != expected_snapshot_seq
            || state.latest_terminal_seq != Some(through_seq)
            || through_seq <= prior_summary_seq
        {
            return Err(ConversationError::Stale);
        }
        let candidate = Arc::new(
            NewConversationEntry::Summary {
                timestamp,
                through_seq,
                text,
            }
            .into_entry(
                state
                    .max_seq
                    .checked_add(1)
                    .ok_or(ConversationError::Corrupt)?,
            )?,
        );
        let mut projected = state.clone();
        projected.apply(Arc::clone(&candidate))?;
        let line = encode_candidate(&candidate)?;
        submit_append(&self.inner, reservation, candidate, line, projected).await
    }
}

pub(super) async fn submit_append(
    inner: &Arc<ConversationInner>,
    reservation: BusyReservation,
    candidate: Arc<ConversationEntry>,
    line: Vec<u8>,
    projected: ConversationState,
) -> Result<u64, ConversationError> {
    let job_state = Arc::new(AppendJobState {
        started: AtomicBool::new(false),
        admitted: AtomicBool::new(false),
        finished: AtomicBool::new(false),
    });
    let settlement = AppendSettlement {
        inner: Arc::clone(inner),
        projected,
        seq: candidate.seq(),
        job_state: Arc::clone(&job_state),
    };
    let path = inner.path.clone();
    let job_state_for_worker = Arc::clone(&job_state);
    let receiver = inner.store.run_io(move || {
        job_state_for_worker.started.store(true, Ordering::Release);
        let write_result = codec::append_line_sync(&path, &line);
        settlement.settle(write_result)
    });
    if !receiver.is_admitted() {
        return match receiver.await {
            Ok(_) => Err(ConversationError::WorkerFailed),
            Err(error) => Err(error.into()),
        };
    }
    job_state.admitted.store(true, Ordering::Release);
    reservation.disarm();
    receiver.await.map_err(ConversationError::from)
}

fn build_compaction_view(
    state: &ConversationState,
) -> Result<CompactionConversationView, ConversationError> {
    if state.outstanding_tools.is_some() || state.pending_restart_terminal.is_some() {
        return Err(ConversationError::IncompleteToolExchange);
    }
    let start_seq = state
        .latest_summary
        .as_ref()
        .map_or(0, |summary| summary.through_seq);
    let terminal_seq = state.latest_terminal_seq;
    let completed_messages = match terminal_seq {
        Some(end_seq) if end_seq > start_seq => project_messages(state, start_seq, Some(end_seq))?,
        _ => Arc::from(Vec::<ModelMessage>::new().into_boxed_slice()),
    };
    let current_start = terminal_seq.map_or(start_seq, |seq| seq.max(start_seq));
    let current_turn_messages = project_messages(state, current_start, None)?;
    Ok(CompactionConversationView {
        latest_summary: state
            .latest_summary
            .as_ref()
            .map(|summary| ConversationSummary {
                through_seq: summary.through_seq,
                text: summary.text.clone(),
            }),
        completed_messages,
        current_turn_messages,
        through_seq: terminal_seq,
        snapshot_seq: state.max_seq,
    })
}

fn project_messages(
    state: &ConversationState,
    start_seq: u64,
    end_seq: Option<u64>,
) -> Result<Arc<[ModelMessage]>, ConversationError> {
    let mut messages = Vec::new();
    let mut pending_calls = Vec::<ToolCallId>::new();
    let mut pending_results = BTreeMap::<ToolCallId, ToolOutput>::new();
    for entry in state
        .entries
        .iter()
        .filter(|entry| entry.seq() > start_seq && end_seq.is_none_or(|end| entry.seq() <= end))
    {
        match entry.as_ref() {
            ConversationEntry::User { text, .. } => {
                messages.push(
                    ModelMessage::user(text.clone()).map_err(|_| ConversationError::Corrupt)?,
                );
            }
            ConversationEntry::Assistant {
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                if !pending_calls.is_empty() {
                    return Err(ConversationError::Corrupt);
                }
                let mut parts = Vec::new();
                if let Some(reasoning) = reasoning {
                    parts.push(AssistantPart::Reasoning(reasoning.clone()));
                }
                if let Some(text) = text {
                    parts.push(AssistantPart::Text(text.clone()));
                }
                parts.extend(tool_calls.iter().cloned().map(AssistantPart::ToolCall));
                messages
                    .push(ModelMessage::assistant(parts).map_err(|_| ConversationError::Corrupt)?);
                pending_calls = tool_calls
                    .iter()
                    .map(|call| call.tool_call_id().clone())
                    .collect();
            }
            ConversationEntry::ToolResult {
                call_id, result, ..
            } => {
                pending_results.insert(call_id.clone(), result.clone());
                while let Some(call_id) = pending_calls.first().cloned() {
                    let Some(output) = pending_results.remove(&call_id) else {
                        break;
                    };
                    messages.push(
                        ModelMessage::tool(call_id, output)
                            .map_err(|_| ConversationError::Corrupt)?,
                    );
                    pending_calls.remove(0);
                }
            }
            ConversationEntry::Interaction { .. }
            | ConversationEntry::Summary { .. }
            | ConversationEntry::TurnTerminal { .. } => {}
        }
    }
    if !pending_calls.is_empty() || !pending_results.is_empty() {
        return Err(ConversationError::Corrupt);
    }
    Ok(Arc::from(messages.into_boxed_slice()))
}
