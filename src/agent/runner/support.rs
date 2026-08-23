use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant as TokioInstant;

use crate::conversation::{
    AssistantMessageDraft, ConversationEntry, ConversationSeq, ConversationView, ToolResultDraft,
};
use crate::model::{AssistantPart, ModelDriverProgress, ModelResponse, Usage};
use crate::value::BoundedText;

use super::super::runner_protocol::{
    CommitAck, RunnerCommitError, RunnerEvent, RunnerOutcome, RunnerProgress, SuspensionError,
    TurnRunnerExit,
};
use super::super::tool_driver::ToolDriverProgress;
use super::super::turn_context::TurnRunnerContext;

pub(super) fn assistant_draft(
    context: &TurnRunnerContext,
    response: &ModelResponse,
) -> Result<(AssistantMessageDraft, Vec<crate::model::ToolCall>), ()> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for part in response.parts() {
        match part {
            AssistantPart::Text(value) => append(
                &mut text,
                value,
                context.limits.max_model_text_bytes_per_round,
            )?,
            AssistantPart::Reasoning(value) => {
                if value.encrypted().is_some() || value.signature().is_some() {
                    return Err(());
                }
                if let Some(value) = value.text() {
                    append(
                        &mut reasoning,
                        value,
                        context.limits.max_model_reasoning_bytes_per_round,
                    )?;
                }
                if let Some(value) = value.summary() {
                    append(
                        &mut reasoning,
                        value,
                        context.limits.max_model_reasoning_bytes_per_round,
                    )?;
                }
            }
            AssistantPart::ToolCall(call) => tool_calls.push(call.clone()),
        }
    }
    let text = optional_text(text, context.limits.max_model_text_bytes_per_round)?;
    let reasoning = optional_text(
        reasoning,
        context.limits.max_model_reasoning_bytes_per_round,
    )?;
    Ok((
        AssistantMessageDraft {
            turn_id: context.turn_id,
            model: context.spec.model.clone(),
            text,
            reasoning,
            tool_calls: tool_calls.clone(),
            usage: response.usage().copied().unwrap_or_default(),
            finish_reason: response.finish_reason(),
        },
        tool_calls,
    ))
}

fn append(target: &mut String, value: &str, maximum: usize) -> Result<(), ()> {
    if target
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > maximum)
    {
        return Err(());
    }
    target.push_str(value);
    Ok(())
}

fn optional_text(value: String, maximum: usize) -> Result<Option<BoundedText>, ()> {
    if value.is_empty() {
        Ok(None)
    } else {
        BoundedText::new_with_max_bytes(value, maximum)
            .map(Some)
            .map_err(|_| ())
    }
}

pub(super) async fn commit_assistant(
    context: &TurnRunnerContext,
    draft: AssistantMessageDraft,
) -> Result<CommitAck, CriticalFailure> {
    let (reply, receiver) = oneshot::channel();
    send_critical_for_context(context, RunnerEvent::CommitAssistant { draft, reply }).await?;
    await_commit(context, receiver).await
}

pub(super) async fn commit_tool_result(
    context: &TurnRunnerContext,
    draft: ToolResultDraft,
) -> Result<CommitAck, CriticalFailure> {
    let (reply, receiver) = oneshot::channel();
    send_critical_for_context(context, RunnerEvent::CommitToolResult { draft, reply }).await?;
    await_commit(context, receiver).await
}

async fn await_commit(
    context: &TurnRunnerContext,
    mut receiver: oneshot::Receiver<Result<CommitAck, RunnerCommitError>>,
) -> Result<CommitAck, CriticalFailure> {
    match receiver.try_recv() {
        Ok(result) => return result.map_err(CriticalFailure::Commit),
        Err(oneshot::error::TryRecvError::Closed) => return Err(CriticalFailure::RuntimeClosed),
        Err(oneshot::error::TryRecvError::Empty) => {}
    }
    let deadline = TokioInstant::from_std(context.deadline);
    tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => Err(CriticalFailure::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(CriticalFailure::DeadlineExceeded),
        result = &mut receiver => match result {
            Ok(result) => result.map_err(CriticalFailure::Commit),
            Err(_) => Err(CriticalFailure::RuntimeClosed),
        }
    }
}

pub(super) fn validate_assistant_ack(
    context: &TurnRunnerContext,
    previous_head: ConversationSeq,
    draft: &AssistantMessageDraft,
    acknowledgement: CommitAck,
) -> Result<ConversationView, CriticalFailure> {
    validate_ack_shape(context, previous_head, &acknowledgement)?;
    match acknowledgement.conversation.entries().last() {
        Some(ConversationEntry::AssistantMessage(entry))
            if entry.seq == acknowledgement.head
                && entry.turn_id == draft.turn_id
                && entry.model == draft.model
                && entry.text == draft.text
                && entry.reasoning == draft.reasoning
                && entry.tool_calls == draft.tool_calls
                && entry.usage == draft.usage
                && entry.finish_reason == draft.finish_reason =>
        {
            Ok(acknowledgement.conversation)
        }
        _ => Err(CriticalFailure::InvalidAck),
    }
}

pub(super) fn validate_tool_ack(
    context: &TurnRunnerContext,
    previous_head: ConversationSeq,
    draft: &ToolResultDraft,
    acknowledgement: CommitAck,
) -> Result<ConversationView, CriticalFailure> {
    validate_ack_shape(context, previous_head, &acknowledgement)?;
    match acknowledgement.conversation.entries().last() {
        Some(ConversationEntry::ToolResult(entry))
            if entry.seq == acknowledgement.head
                && entry.turn_id == draft.turn_id
                && entry.tool_call_id == draft.tool_call_id
                && entry.tool_name == draft.tool_name
                && entry.outcome == draft.outcome
                && entry.content == draft.content =>
        {
            Ok(acknowledgement.conversation)
        }
        _ => Err(CriticalFailure::InvalidAck),
    }
}

fn validate_ack_shape(
    context: &TurnRunnerContext,
    previous_head: ConversationSeq,
    acknowledgement: &CommitAck,
) -> Result<(), CriticalFailure> {
    let current_entries = context.conversation.entries();
    let replacement_entries = acknowledgement.conversation.entries();
    if previous_head != context.conversation.head()
        || previous_head.next() != Some(acknowledgement.head)
        || current_entries
            .len()
            .checked_add(1)
            .is_none_or(|length| replacement_entries.len() != length)
        || replacement_entries.get(..current_entries.len()) != Some(current_entries)
        || acknowledgement.conversation.head() != acknowledgement.head
        || context
            .validate_conversation(&acknowledgement.conversation)
            .is_err()
    {
        return Err(CriticalFailure::InvalidAck);
    }
    Ok(())
}

pub(super) struct FinishControl {
    pub(super) sender: mpsc::Sender<RunnerEvent>,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) deadline: std::time::Instant,
}

pub(super) async fn finish_outcome(
    control: &FinishControl,
    outcome: RunnerOutcome,
) -> TurnRunnerExit {
    let event = RunnerEvent::Finish {
        outcome: outcome.clone(),
    };
    match send_critical(control, event).await {
        Ok(()) => TurnRunnerExit::Finished { outcome },
        Err(_) => TurnRunnerExit::ProtocolClosed { outcome },
    }
}

pub(super) async fn send_critical_for_context(
    context: &TurnRunnerContext,
    event: RunnerEvent,
) -> Result<(), CriticalFailure> {
    send_critical(
        &FinishControl {
            sender: context.critical_tx.clone(),
            cancellation: context.cancellation.clone(),
            deadline: context.deadline,
        },
        event,
    )
    .await
}

pub(super) async fn send_critical(
    control: &FinishControl,
    event: RunnerEvent,
) -> Result<(), CriticalFailure> {
    let event = match control.sender.try_send(event) {
        Ok(()) => return Ok(()),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return Err(CriticalFailure::RuntimeClosed);
        }
        Err(mpsc::error::TrySendError::Full(event)) => event,
    };
    let deadline = TokioInstant::from_std(control.deadline);
    let send = control.sender.send(event);
    tokio::pin!(send);
    tokio::select! {
        biased;
        _ = control.cancellation.cancelled() => Err(CriticalFailure::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(CriticalFailure::DeadlineExceeded),
        result = &mut send => result.map_err(|_| CriticalFailure::RuntimeClosed),
    }
}

pub(super) fn model_progress(
    context: &TurnRunnerContext,
    model_round: u16,
    value: ModelDriverProgress,
) {
    progress(
        context,
        RunnerProgress::ModelProgress {
            model_round,
            progress: value,
        },
    );
}

pub(super) fn tool_progress(context: &TurnRunnerContext, value: ToolDriverProgress) {
    match value {
        ToolDriverProgress::Started {
            tool_call_id,
            tool_name,
        } => progress(
            context,
            RunnerProgress::ToolStarted {
                tool_call_id,
                tool_name,
            },
        ),
        ToolDriverProgress::Update {
            tool_call_id,
            progress: value,
        } => progress(
            context,
            RunnerProgress::ToolProgress {
                tool_call_id,
                progress: value,
            },
        ),
    }
}

pub(super) fn progress(context: &TurnRunnerContext, event: RunnerProgress) {
    let _ = context.progress_tx.try_send(event);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CriticalFailure {
    Cancelled,
    DeadlineExceeded,
    RuntimeClosed,
    Commit(RunnerCommitError),
    Suspension(SuspensionError),
    InvalidAck,
}

#[derive(Default)]
pub(super) struct UsageAccumulator {
    value: Option<Usage>,
}

impl UsageAccumulator {
    pub(super) fn add(&mut self, value: Usage) -> Result<(), ()> {
        self.value = Some(match self.value {
            Some(current) => sum_usage(current, value)?,
            None => value,
        });
        Ok(())
    }

    pub(super) fn finish(self) -> Usage {
        self.value.unwrap_or_default()
    }

    pub(super) fn current(&self) -> Usage {
        self.value.unwrap_or_default()
    }
}

fn sum_usage(left: Usage, right: Usage) -> Result<Usage, ()> {
    Ok(Usage::from_optional(
        sum_field(left.input_tokens(), right.input_tokens())?,
        sum_field(left.output_tokens(), right.output_tokens())?,
        sum_field(left.reasoning_tokens(), right.reasoning_tokens())?,
    )
    .with_cache_read_tokens(sum_field(
        left.cache_read_tokens(),
        right.cache_read_tokens(),
    )?)
    .with_cache_write_tokens(sum_field(
        left.cache_write_tokens(),
        right.cache_write_tokens(),
    )?)
    .with_provider_total_tokens(sum_field(
        left.provider_total_tokens(),
        right.provider_total_tokens(),
    )?))
}

fn sum_field(left: Option<u64>, right: Option<u64>) -> Result<Option<u64>, ()> {
    match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right).map(Some).ok_or(()),
        _ => Ok(None),
    }
}
