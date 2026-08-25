use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant as TokioInstant;

use crate::conversation::{
    AssistantMessageDraft, ConversationEntry, ConversationSeq, ConversationView, SummaryDraft,
    ToolResultDraft,
};
use crate::model::{AssistantPart, ModelDriverProgress, ModelResponse, Usage};
use crate::value::BoundedText;

use super::super::runner_protocol::{
    CommittedUpdate, RunnerCommitError, RunnerEvent, RunnerProgress, SuspensionError,
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
                context.environment.limits.max_model_text_bytes_per_round,
            )?,
            AssistantPart::Reasoning(value) => {
                if value.encrypted().is_some() || value.signature().is_some() {
                    return Err(());
                }
                if let Some(value) = value.text() {
                    append(
                        &mut reasoning,
                        value,
                        context
                            .environment
                            .limits
                            .max_model_reasoning_bytes_per_round,
                    )?;
                }
                if let Some(value) = value.summary() {
                    append(
                        &mut reasoning,
                        value,
                        context
                            .environment
                            .limits
                            .max_model_reasoning_bytes_per_round,
                    )?;
                }
            }
            AssistantPart::ToolCall(call) => tool_calls.push(call.clone()),
        }
    }
    let text = optional_text(
        text,
        context.environment.limits.max_model_text_bytes_per_round,
    )?;
    let reasoning = optional_text(
        reasoning,
        context
            .environment
            .limits
            .max_model_reasoning_bytes_per_round,
    )?;
    Ok((
        AssistantMessageDraft {
            turn_id: context.turn_id,
            model: context.environment.spec.model.clone(),
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
) -> Result<CommittedUpdate, CriticalFailure> {
    let (reply, receiver) = oneshot::channel();
    send_critical_for_context(context, RunnerEvent::CommitAssistant { draft, reply }).await?;
    await_commit(context, receiver).await
}

pub(super) async fn commit_tool_result(
    context: &TurnRunnerContext,
    draft: ToolResultDraft,
) -> Result<CommittedUpdate, CriticalFailure> {
    let (reply, receiver) = oneshot::channel();
    send_critical_for_context(context, RunnerEvent::CommitToolResult { draft, reply }).await?;
    await_commit(context, receiver).await
}

pub(super) async fn commit_summary(
    context: &TurnRunnerContext,
    snapshot_head: ConversationSeq,
    draft: SummaryDraft,
) -> Result<CommittedUpdate, CriticalFailure> {
    let (reply, receiver) = oneshot::channel();
    send_critical_for_context(
        context,
        RunnerEvent::CommitSummary {
            snapshot_head,
            draft,
            reply,
        },
    )
    .await?;
    await_commit(context, receiver).await
}

async fn await_commit(
    context: &TurnRunnerContext,
    mut receiver: oneshot::Receiver<Result<CommittedUpdate, RunnerCommitError>>,
) -> Result<CommittedUpdate, CriticalFailure> {
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
    update: CommittedUpdate,
) -> Result<ConversationView, CriticalFailure> {
    validate_update_shape(context, previous_head, &update)?;
    match &update.entry {
        ConversationEntry::AssistantMessage(entry)
            if entry.turn_id == draft.turn_id
                && entry.model == draft.model
                && entry.text == draft.text
                && entry.reasoning == draft.reasoning
                && entry.tool_calls == draft.tool_calls
                && entry.usage == draft.usage
                && entry.finish_reason == draft.finish_reason =>
        {
            Ok(update.conversation)
        }
        _ => Err(CriticalFailure::InvalidAck),
    }
}

pub(super) fn validate_tool_ack(
    context: &TurnRunnerContext,
    previous_head: ConversationSeq,
    draft: &ToolResultDraft,
    update: CommittedUpdate,
) -> Result<ConversationView, CriticalFailure> {
    validate_update_shape(context, previous_head, &update)?;
    match &update.entry {
        ConversationEntry::ToolResult(entry)
            if entry.turn_id == draft.turn_id
                && entry.tool_call_id == draft.tool_call_id
                && entry.tool_name == draft.tool_name
                && entry.outcome == draft.outcome
                && entry.content == draft.content =>
        {
            Ok(update.conversation)
        }
        _ => Err(CriticalFailure::InvalidAck),
    }
}

pub(super) fn validate_summary_ack(
    context: &TurnRunnerContext,
    snapshot_head: ConversationSeq,
    draft: &SummaryDraft,
    update: CommittedUpdate,
) -> Result<ConversationView, CriticalFailure> {
    if context.conversation.head() != snapshot_head {
        return Err(CriticalFailure::InvalidAck);
    }
    validate_update_shape(context, snapshot_head, &update)?;
    let before = context
        .conversation
        .validated_active_turn(&context.environment.spec, &context.environment.limits)
        .map_err(|_| CriticalFailure::InvalidAck)?;
    let after = update
        .conversation
        .validated_active_turn(&context.environment.spec, &context.environment.limits)
        .map_err(|_| CriticalFailure::InvalidAck)?;
    if before.turn_id != after.turn_id || before.execution != after.execution {
        return Err(CriticalFailure::InvalidAck);
    }
    match &update.entry {
        ConversationEntry::Summary(entry)
            if entry.through == draft.through && entry.summary == draft.summary =>
        {
            Ok(update.conversation)
        }
        _ => Err(CriticalFailure::InvalidAck),
    }
}

fn validate_update_shape(
    context: &TurnRunnerContext,
    previous_head: ConversationSeq,
    update: &CommittedUpdate,
) -> Result<(), CriticalFailure> {
    if !update
        .conversation
        .is_validated_for(&context.environment.spec, &context.environment.limits)
    {
        return Err(CriticalFailure::InvalidAck);
    }
    if previous_head != context.conversation.head()
        || update.previous_head != previous_head
        || previous_head.next() != Some(update.entry.seq())
        || update.conversation.head() != update.entry.seq()
        || update.conversation.entries().last() != Some(&update.entry)
        || context.validate_conversation(&update.conversation).is_err()
    {
        return Err(CriticalFailure::InvalidAck);
    }
    Ok(())
}

pub(super) struct CriticalControl {
    pub(super) sender: mpsc::Sender<RunnerEvent>,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) deadline: std::time::Instant,
}

pub(super) async fn send_critical_for_context(
    context: &TurnRunnerContext,
    event: RunnerEvent,
) -> Result<(), CriticalFailure> {
    send_critical(
        &CriticalControl {
            sender: context.critical_tx.clone(),
            cancellation: context.cancellation.clone(),
            deadline: context.deadline,
        },
        event,
    )
    .await
}

pub(super) async fn send_critical(
    control: &CriticalControl,
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
