use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::context::TurnContext;
use crate::model::{
    DeliveryState, ModelCallContext, ModelError, ModelErrorKind, ModelEvent, ModelFinishReason,
    ModelResponse, ToolCall, Usage,
};
use crate::prompt::{PromptError, append_validated_summary};
use crate::storage::conversation::{ConversationError, NewConversationEntry};
use crate::tools::{
    LegacyTool, LegacyToolCallSummary, LegacyToolContext, LegacyToolError, LegacyToolOutput,
    LegacyToolResultStatus, LegacyToolResultSummary, ToolContextView, ToolDecision, ToolRequest,
};

pub(crate) const MAX_RUNNER_EVENT_CAPACITY: usize = 4_096;
const MODEL_EVENT_CAPACITY: usize = 64;
const TOOL_ROUND_LIMIT_TEXT: &str = "tool round limit reached";
const CANCELLED_TEXT: &str = "cancelled";
const TOOL_EXECUTION_FAILED_TEXT: &str = "tool execution failed";
const TOOL_EXECUTION_DENIED_TEXT: &str = "tool execution denied";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunnerEvent {
    Model(ModelEvent),
    ToolStarted(LegacyToolCallSummary),
    ToolFinished(LegacyToolResultSummary),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum RunnerEventSendError {
    #[error("runner event is invalid")]
    InvalidEvent,
    #[error("runner event channel is full")]
    Full,
    #[error("runner event channel is closed")]
    Closed,
}

struct RunnerEventState {
    active: bool,
    sender: Option<mpsc::Sender<RunnerEvent>>,
}

struct RunnerEventInner {
    state: Mutex<RunnerEventState>,
}

#[derive(Clone)]
pub(crate) struct RunnerEventSink {
    inner: Arc<RunnerEventInner>,
}

impl RunnerEventSink {
    pub(crate) fn channel(
        capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<RunnerEvent>), RunnerEventSendError> {
        if capacity == 0 || capacity > MAX_RUNNER_EVENT_CAPACITY {
            return Err(RunnerEventSendError::InvalidEvent);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                inner: Arc::new(RunnerEventInner {
                    state: Mutex::new(RunnerEventState {
                        active: true,
                        sender: Some(sender),
                    }),
                }),
            },
            receiver,
        ))
    }

    pub(crate) fn try_publish_model(&self, event: RunnerEvent) -> bool {
        let RunnerEvent::Model(event) = event else {
            return false;
        };
        if !valid_model_event(&event) {
            return false;
        }
        self.try_send(RunnerEvent::Model(event)).is_ok()
    }

    pub(crate) fn try_publish_tool(&self, event: RunnerEvent) -> Result<(), RunnerEventSendError> {
        match event {
            RunnerEvent::ToolStarted(call) => self.try_send(RunnerEvent::ToolStarted(call)),
            RunnerEvent::ToolFinished(result) => self.try_send(RunnerEvent::ToolFinished(result)),
            RunnerEvent::Model(_) => Err(RunnerEventSendError::InvalidEvent),
        }
    }

    fn try_send(&self, event: RunnerEvent) -> Result<(), RunnerEventSendError> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.active {
            return Err(RunnerEventSendError::Closed);
        }
        let Some(sender) = state.sender.as_ref() else {
            state.active = false;
            return Err(RunnerEventSendError::Closed);
        };
        match sender.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(RunnerEventSendError::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                state.active = false;
                state.sender = None;
                Err(RunnerEventSendError::Closed)
            }
        }
    }
}

fn valid_model_event(event: &ModelEvent) -> bool {
    let delta = match event {
        ModelEvent::TextDelta { delta } | ModelEvent::ReasoningDelta { delta } => delta,
    };
    !delta.is_empty()
        && delta.len() <= 64 * 1024
        && delta
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum TurnFailure {
    #[error("turn model operation failed")]
    Model,
    #[error("turn conversation operation failed")]
    Conversation,
    #[error("turn compaction operation failed")]
    Compaction,
    #[error("turn response is invalid")]
    InvalidResponse,
    #[error("turn tool round limit was reached")]
    ToolRoundLimit,
    #[error("turn tool operation failed")]
    Tool,
    #[error("turn timestamp operation failed")]
    Timestamp,
    #[error("turn operation failed internally")]
    Internal,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum TurnTaskResult {
    Completed { usage: Usage },
    Cancelled { usage: Usage },
    Failed { failure: TurnFailure, usage: Usage },
}

impl fmt::Debug for TurnTaskResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed { .. } => formatter.write_str("TurnTaskResult::Completed"),
            Self::Cancelled { .. } => formatter.write_str("TurnTaskResult::Cancelled"),
            Self::Failed { failure, .. } => formatter
                .debug_tuple("TurnTaskResult::Failed")
                .field(failure)
                .finish(),
        }
    }
}

enum Flow {
    Completed,
    Cancelled,
    Failed(TurnFailure),
}

enum CallFlow<T> {
    Value(T),
    ContextOverflow,
    Cancelled,
    Failed(TurnFailure),
}

struct ModelAttempt {
    result: Result<ModelResponse, ModelError>,
    observed_event: bool,
}

struct ToolExecution {
    output: LegacyToolOutput,
    status: LegacyToolResultStatus,
    cancelled: bool,
}

enum ValidatedDisposition {
    Final,
    ToolRound(Vec<ToolCall>),
}

fn validate_response(response: &ModelResponse) -> Result<ValidatedDisposition, TurnFailure> {
    let mut calls = Vec::new();
    let mut expected_index = 0_u32;
    for part in response.parts() {
        if let Some(call) = part.as_tool_call() {
            debug_assert_eq!(call.call_index(), expected_index);
            if call.call_index() != expected_index {
                return Err(TurnFailure::InvalidResponse);
            }
            expected_index = expected_index
                .checked_add(1)
                .ok_or(TurnFailure::InvalidResponse)?;
            calls.push(call.clone());
        }
    }
    match response.finish_reason() {
        ModelFinishReason::Stop | ModelFinishReason::Refused if calls.is_empty() => {
            Ok(ValidatedDisposition::Final)
        }
        ModelFinishReason::ToolCalls | ModelFinishReason::Unknown if !calls.is_empty() => {
            Ok(ValidatedDisposition::ToolRound(calls))
        }
        ModelFinishReason::Unknown if calls.is_empty() => Ok(ValidatedDisposition::Final),
        _ => Err(TurnFailure::InvalidResponse),
    }
}

pub(crate) async fn run_turn(ctx: TurnContext) -> TurnTaskResult {
    let flow = run_turn_inner(&ctx).await;
    let usage = ctx.conversation().usage().await;
    match flow {
        Flow::Completed => TurnTaskResult::Completed { usage },
        Flow::Cancelled => TurnTaskResult::Cancelled { usage },
        Flow::Failed(failure) => TurnTaskResult::Failed { failure, usage },
    }
}

async fn run_turn_inner(ctx: &TurnContext) -> Flow {
    let mut tool_rounds = 0_u8;
    loop {
        if ctx.cancellation().is_cancelled() {
            return Flow::Cancelled;
        }
        let mut forced_recovery = false;
        let response = loop {
            let request = match build_ordinary_request(ctx).await {
                CallFlow::Value(request) => request,
                CallFlow::ContextOverflow if !forced_recovery => {
                    match force_compaction(ctx).await {
                        CallFlow::Value(()) => forced_recovery = true,
                        CallFlow::Cancelled => return Flow::Cancelled,
                        CallFlow::Failed(failure) => return Flow::Failed(failure),
                        CallFlow::ContextOverflow => {
                            return Flow::Failed(TurnFailure::Compaction);
                        }
                    }
                    continue;
                }
                CallFlow::ContextOverflow => return Flow::Failed(TurnFailure::Compaction),
                CallFlow::Cancelled => return Flow::Cancelled,
                CallFlow::Failed(failure) => return Flow::Failed(failure),
            };
            match ordinary_model_call(ctx, Arc::new(request)).await {
                CallFlow::Value(response) => break response,
                CallFlow::ContextOverflow if !forced_recovery => {
                    match force_compaction(ctx).await {
                        CallFlow::Value(()) => forced_recovery = true,
                        CallFlow::Cancelled => return Flow::Cancelled,
                        CallFlow::Failed(failure) => return Flow::Failed(failure),
                        CallFlow::ContextOverflow => {
                            return Flow::Failed(TurnFailure::Compaction);
                        }
                    }
                }
                CallFlow::ContextOverflow => return Flow::Failed(TurnFailure::Compaction),
                CallFlow::Cancelled => return Flow::Cancelled,
                CallFlow::Failed(failure) => return Flow::Failed(failure),
            }
        };
        let disposition = match validate_response(&response) {
            Ok(disposition) => disposition,
            Err(failure) => return Flow::Failed(failure),
        };
        if ctx.cancellation().is_cancelled() {
            return Flow::Cancelled;
        }
        let timestamp = match ctx.timestamp() {
            Ok(timestamp) => timestamp,
            Err(_) => return Flow::Failed(TurnFailure::Timestamp),
        };
        let assistant = match NewConversationEntry::assistant_from_response(
            ctx.turn_id(),
            timestamp,
            &response,
        ) {
            Ok(entry) => entry,
            Err(_) => return Flow::Failed(TurnFailure::InvalidResponse),
        };
        if ctx.conversation().append(assistant).await.is_err() {
            return Flow::Failed(TurnFailure::Conversation);
        }
        let calls = match disposition {
            ValidatedDisposition::Final => return Flow::Completed,
            ValidatedDisposition::ToolRound(calls) => calls,
        };
        if ctx.cancellation().is_cancelled() {
            return append_remaining_cancelled(ctx, &calls).await;
        }

        if tool_rounds >= ctx.max_tool_rounds() {
            if append_fixed_results(
                ctx,
                &calls,
                TOOL_ROUND_LIMIT_TEXT,
                LegacyToolResultStatus::Failed,
            )
            .await
            .is_err()
            {
                return Flow::Failed(TurnFailure::Conversation);
            }
            return Flow::Failed(TurnFailure::ToolRoundLimit);
        }
        tool_rounds = tool_rounds.saturating_add(1);
        match execute_tool_round(ctx, &calls).await {
            Flow::Completed => {}
            Flow::Cancelled => return Flow::Cancelled,
            Flow::Failed(failure) => return Flow::Failed(failure),
        }
    }
}

async fn build_ordinary_request(ctx: &TurnContext) -> CallFlow<crate::model::ModelRequest> {
    let mut stale_replan = false;
    loop {
        if ctx.cancellation().is_cancelled() {
            return CallFlow::Cancelled;
        }
        let view = match ctx.conversation().compaction_view().await {
            Ok(view) => view,
            Err(_) => return CallFlow::Failed(TurnFailure::Compaction),
        };
        let plan = match ctx.compactor().plan(
            ctx.prompt_builder(),
            &view,
            ctx.tool_specs(),
            ctx.prompt_options().clone(),
        ) {
            Ok(plan) => plan,
            Err(_) => return CallFlow::Failed(TurnFailure::Compaction),
        };
        let Some(plan) = plan else {
            return build_fresh_prompt(ctx).await;
        };
        if ctx.cancellation().is_cancelled() {
            return CallFlow::Cancelled;
        }
        let response = match summary_model_call(ctx, plan.clone_request()).await {
            CallFlow::Value(response) => response,
            CallFlow::ContextOverflow => return CallFlow::Failed(TurnFailure::Compaction),
            CallFlow::Cancelled => return CallFlow::Cancelled,
            CallFlow::Failed(failure) => return CallFlow::Failed(failure),
        };
        let summary = match plan.validate_summary(&response) {
            Ok(summary) => summary,
            Err(_) => return CallFlow::Failed(TurnFailure::Compaction),
        };
        if ctx.cancellation().is_cancelled() {
            return CallFlow::Cancelled;
        }
        let timestamp = match ctx.timestamp() {
            Ok(timestamp) => timestamp,
            Err(_) => return CallFlow::Failed(TurnFailure::Timestamp),
        };
        match append_validated_summary(ctx.conversation(), &plan, timestamp, &summary).await {
            Ok(_) => return build_fresh_prompt(ctx).await,
            Err(crate::storage::conversation::ConversationError::Stale) if !stale_replan => {
                stale_replan = true;
            }
            Err(_) => return CallFlow::Failed(TurnFailure::Compaction),
        }
    }
}

async fn force_compaction(ctx: &TurnContext) -> CallFlow<()> {
    if ctx.cancellation().is_cancelled() {
        return CallFlow::Cancelled;
    }
    let view = match ctx.conversation().compaction_view().await {
        Ok(view) => view,
        Err(_) => return CallFlow::Failed(TurnFailure::Compaction),
    };
    let plan = match ctx.compactor().plan_after_context_overflow(
        ctx.prompt_builder(),
        &view,
        ctx.tool_specs(),
        ctx.prompt_options().clone(),
    ) {
        Ok(plan) => plan,
        Err(_) => return CallFlow::Failed(TurnFailure::Compaction),
    };
    let response = match summary_model_call(ctx, plan.clone_request()).await {
        CallFlow::Value(response) => response,
        CallFlow::Cancelled => return CallFlow::Cancelled,
        CallFlow::ContextOverflow | CallFlow::Failed(_) => {
            return CallFlow::Failed(TurnFailure::Compaction);
        }
    };
    let summary = match plan.validate_summary(&response) {
        Ok(summary) => summary,
        Err(_) => return CallFlow::Failed(TurnFailure::Compaction),
    };
    if ctx.cancellation().is_cancelled() {
        return CallFlow::Cancelled;
    }
    let timestamp = match ctx.timestamp() {
        Ok(timestamp) => timestamp,
        Err(_) => return CallFlow::Failed(TurnFailure::Timestamp),
    };
    match append_validated_summary(ctx.conversation(), &plan, timestamp, &summary).await {
        Ok(_) => {
            if ctx.cancellation().is_cancelled() {
                CallFlow::Cancelled
            } else {
                CallFlow::Value(())
            }
        }
        Err(_) => CallFlow::Failed(TurnFailure::Compaction),
    }
}

async fn build_fresh_prompt(ctx: &TurnContext) -> CallFlow<crate::model::ModelRequest> {
    if ctx.cancellation().is_cancelled() {
        return CallFlow::Cancelled;
    }
    let view = match ctx.conversation().prompt_view().await {
        Ok(view) => view,
        Err(_) => return CallFlow::Failed(TurnFailure::Conversation),
    };
    ctx.prompt_builder()
        .build(&view, ctx.tool_specs(), ctx.prompt_options().clone())
        .map_or_else(
            |error| {
                if matches!(error, PromptError::ContextOverflow) {
                    CallFlow::ContextOverflow
                } else {
                    CallFlow::Failed(TurnFailure::Compaction)
                }
            },
            CallFlow::Value,
        )
}

async fn ordinary_model_call(
    ctx: &TurnContext,
    request: Arc<crate::model::ModelRequest>,
) -> CallFlow<ModelResponse> {
    let max_attempts = ctx.retry_policy().max_attempts();
    for attempt in 0..max_attempts {
        if ctx.cancellation().is_cancelled() {
            return CallFlow::Cancelled;
        }
        let attempt_result = generate_once(ctx, Arc::clone(&request), true).await;
        match attempt_result.result {
            Ok(response) => return CallFlow::Value(response),
            Err(error)
                if is_retryable(&error)
                    && !attempt_result.observed_event
                    && attempt + 1 < max_attempts =>
            {
                let Some(delay) = ctx
                    .retry_policy()
                    .delay_for_retry(attempt, error.retry_after())
                else {
                    return CallFlow::Failed(TurnFailure::Model);
                };
                if !wait_for_retry(ctx.cancellation(), delay).await {
                    return CallFlow::Cancelled;
                }
            }
            Err(error)
                if error.kind() == ModelErrorKind::Cancelled
                    && ctx.cancellation().is_cancelled() =>
            {
                return CallFlow::Cancelled;
            }
            Err(error) if error.kind() == ModelErrorKind::ContextOverflow => {
                return CallFlow::ContextOverflow;
            }
            Err(_) => return CallFlow::Failed(TurnFailure::Model),
        }
    }
    CallFlow::Failed(TurnFailure::Model)
}

async fn summary_model_call(
    ctx: &TurnContext,
    request: crate::model::ModelRequest,
) -> CallFlow<ModelResponse> {
    match generate_once(ctx, Arc::new(request), false).await.result {
        Ok(response) => CallFlow::Value(response),
        Err(error)
            if error.kind() == ModelErrorKind::Cancelled && ctx.cancellation().is_cancelled() =>
        {
            CallFlow::Cancelled
        }
        Err(_) => CallFlow::Failed(TurnFailure::Model),
    }
}

async fn generate_once(
    ctx: &TurnContext,
    request: Arc<crate::model::ModelRequest>,
    forward_events: bool,
) -> ModelAttempt {
    let (model_events, mut receiver) =
        match crate::model::ModelEventSink::channel(MODEL_EVENT_CAPACITY) {
            Ok(value) => value,
            Err(error) => {
                return ModelAttempt {
                    result: Err(error),
                    observed_event: false,
                };
            }
        };
    let call_context = match ModelCallContext::new(ctx.cancellation().clone(), model_events.clone())
    {
        Ok(context) => context,
        Err(error) => {
            return ModelAttempt {
                result: Err(error),
                observed_event: false,
            };
        }
    };
    let mut generation = Box::pin(
        ctx.gateway()
            .generate((*request).clone(), call_context.clone()),
    );
    let mut receiver_open = true;
    let mut observed_event = false;
    let result = loop {
        if receiver_open {
            tokio::select! {
                biased;
                event = receiver.recv() => match event {
                    Some(event) => {
                        observed_event = true;
                        if forward_events {
                            let _ = ctx.events().try_publish_model(RunnerEvent::Model(event));
                        }
                    }
                    None => receiver_open = false,
                },
                result = &mut generation => break result,
            }
        } else {
            break generation.await;
        }
    };
    let accepted = result.is_ok();
    if !accepted && !receiver.is_empty() {
        observed_event = true;
    }
    call_context.close();
    drop(model_events);
    if accepted {
        while let Ok(event) = receiver.try_recv() {
            observed_event = true;
            if forward_events {
                let _ = ctx.events().try_publish_model(RunnerEvent::Model(event));
            }
        }
    }
    ModelAttempt {
        result,
        observed_event,
    }
}

fn is_retryable(error: &ModelError) -> bool {
    matches!(
        (error.kind(), error.delivery()),
        (
            ModelErrorKind::ProviderUnavailable
                | ModelErrorKind::RateLimited
                | ModelErrorKind::Timeout
                | ModelErrorKind::TransportUnavailable,
            DeliveryState::NotSent | DeliveryState::RejectedBeforeExecution
        )
    )
}

async fn wait_for_retry(cancellation: &CancellationToken, delay: Duration) -> bool {
    if delay.is_zero() {
        return !cancellation.is_cancelled();
    }
    tokio::select! {
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

async fn execute_tool_round(ctx: &TurnContext, calls: &[ToolCall]) -> Flow {
    for (index, call) in calls.iter().enumerate() {
        if ctx.cancellation().is_cancelled() {
            return append_remaining_cancelled(ctx, &calls[index..]).await;
        }
        let execution = execute_one_tool(ctx, call).await;
        let execution = match execution {
            Ok(execution) => execution,
            Err(failure) => return Flow::Failed(failure),
        };
        let status = execution.status;
        let cancelled = execution.cancelled;
        if append_tool_result(ctx, call, execution.output)
            .await
            .is_err()
        {
            return Flow::Failed(TurnFailure::Conversation);
        }
        publish_tool_finished(ctx, call, status);
        if cancelled || ctx.cancellation().is_cancelled() {
            return append_remaining_cancelled(ctx, &calls[index + 1..]).await;
        }
    }
    Flow::Completed
}

async fn append_remaining_cancelled(ctx: &TurnContext, calls: &[ToolCall]) -> Flow {
    for call in calls {
        let output = match LegacyToolOutput::failure(CANCELLED_TEXT) {
            Ok(output) => output,
            Err(_) => return Flow::Failed(TurnFailure::Internal),
        };
        if append_tool_result(ctx, call, output).await.is_err() {
            return Flow::Failed(TurnFailure::Conversation);
        }
        publish_tool_finished(ctx, call, LegacyToolResultStatus::Cancelled);
    }
    Flow::Cancelled
}

async fn append_fixed_results(
    ctx: &TurnContext,
    calls: &[ToolCall],
    text: &str,
    status: LegacyToolResultStatus,
) -> Result<(), ()> {
    for call in calls {
        let output = LegacyToolOutput::failure(text).map_err(|_| ())?;
        append_tool_result(ctx, call, output)
            .await
            .map_err(|_| ())?;
        publish_tool_finished(ctx, call, status);
    }
    Ok(())
}

async fn append_tool_result(
    ctx: &TurnContext,
    call: &ToolCall,
    output: LegacyToolOutput,
) -> Result<(), ConversationError> {
    let timestamp = ctx
        .timestamp()
        .map_err(|_| ConversationError::InvalidEntry)?;
    ctx.conversation()
        .append(NewConversationEntry::ToolResult {
            turn_id: ctx.turn_id(),
            timestamp,
            call_id: call.tool_call_id().clone(),
            result: output,
        })
        .await
        .map(|_| ())
}

async fn execute_one_tool(
    ctx: &TurnContext,
    call: &ToolCall,
) -> Result<ToolExecution, TurnFailure> {
    if !ctx.enabled_tools().contains(call.name()) {
        return failed_tool_execution();
    }
    let request = ToolRequest::new(
        call.tool_call_id(),
        call.name(),
        call.arguments(),
        call.call_index(),
    );
    let context_view = ToolContextView::new(ctx.session_id(), ctx.turn_id(), ctx.enabled_tools());
    let decision = match catch_unwind(AssertUnwindSafe(|| {
        ctx.policy().decide(&request, &context_view)
    })) {
        Ok(decision) => decision,
        Err(_) => return failed_tool_execution(),
    };
    match decision {
        ToolDecision::Allow => execute_allowed_tool(ctx, call).await,
        ToolDecision::Deny { reason } => Ok(ToolExecution {
            output: LegacyToolOutput::failure(reason)
                .or_else(|_| LegacyToolOutput::failure(TOOL_EXECUTION_DENIED_TEXT))
                .map_err(|_| TurnFailure::Internal)?,
            status: LegacyToolResultStatus::Denied,
            cancelled: false,
        }),
        ToolDecision::Ask { question, choices } => {
            let tool_context = LegacyToolContext::new(
                ctx.session_id(),
                ctx.turn_id(),
                ctx.workspace(),
                ctx.cancellation().clone(),
                ctx.interactions(),
            )
            .map_err(|_| TurnFailure::Tool)?;
            match tool_context.ask_user(question, choices).await {
                Ok(answer)
                    if answer.text().trim().eq_ignore_ascii_case("yes")
                        || answer.text().trim().eq_ignore_ascii_case("allow") =>
                {
                    execute_allowed_tool(ctx, call).await
                }
                Err(LegacyToolError::Cancelled) if ctx.cancellation().is_cancelled() => {
                    cancelled_tool_execution()
                }
                Ok(_) | Err(_) => Ok(ToolExecution {
                    output: LegacyToolOutput::failure(TOOL_EXECUTION_DENIED_TEXT)
                        .map_err(|_| TurnFailure::Internal)?,
                    status: LegacyToolResultStatus::Denied,
                    cancelled: false,
                }),
            }
        }
    }
}

async fn execute_allowed_tool(
    ctx: &TurnContext,
    call: &ToolCall,
) -> Result<ToolExecution, TurnFailure> {
    let Some(tool) = ctx.tools().get(call.name()) else {
        return failed_tool_execution();
    };
    if ctx.cancellation().is_cancelled() {
        return cancelled_tool_execution();
    }
    let tool_context = LegacyToolContext::new(
        ctx.session_id(),
        ctx.turn_id(),
        ctx.workspace(),
        ctx.cancellation().clone(),
        ctx.interactions(),
    )
    .map_err(|_| TurnFailure::Tool)?;
    let future = catch_unwind(AssertUnwindSafe(|| {
        tool.execute(tool_context, call.arguments().clone())
    }));
    let future = match future {
        Ok(future) => future,
        Err(_) => return failed_tool_execution(),
    };
    publish_tool_started(ctx, call);
    let result = AssertUnwindSafe(future).catch_unwind().await.ok();
    match result {
        Some(Ok(output)) => {
            let status = if output.is_error() {
                LegacyToolResultStatus::Failed
            } else {
                LegacyToolResultStatus::Succeeded
            };
            Ok(ToolExecution {
                output,
                status,
                cancelled: false,
            })
        }
        Some(Err(LegacyToolError::Cancelled)) if ctx.cancellation().is_cancelled() => {
            cancelled_tool_execution()
        }
        Some(Err(_)) | None => failed_tool_execution(),
    }
}

fn publish_tool_started(ctx: &TurnContext, call: &ToolCall) {
    if let Ok(summary) = LegacyToolCallSummary::new(
        call.tool_call_id().clone(),
        call.name().clone(),
        call.call_index(),
    ) {
        let _ = ctx
            .events()
            .try_publish_tool(RunnerEvent::ToolStarted(summary));
    }
}

fn publish_tool_finished(ctx: &TurnContext, call: &ToolCall, status: LegacyToolResultStatus) {
    if let Ok(summary) = LegacyToolResultSummary::new(call.tool_call_id().clone(), status) {
        let _ = ctx
            .events()
            .try_publish_tool(RunnerEvent::ToolFinished(summary));
    }
}

fn failed_tool_execution() -> Result<ToolExecution, TurnFailure> {
    Ok(ToolExecution {
        output: LegacyToolOutput::failure(TOOL_EXECUTION_FAILED_TEXT)
            .map_err(|_| TurnFailure::Internal)?,
        status: LegacyToolResultStatus::Failed,
        cancelled: false,
    })
}

fn cancelled_tool_execution() -> Result<ToolExecution, TurnFailure> {
    Ok(ToolExecution {
        output: LegacyToolOutput::failure(CANCELLED_TEXT).map_err(|_| TurnFailure::Internal)?,
        status: LegacyToolResultStatus::Cancelled,
        cancelled: true,
    })
}

const _: () = {
    let _ = std::mem::size_of::<RunnerEvent>();
    let _ = std::mem::size_of::<RunnerEventSink>();
    let _ = std::mem::size_of::<TurnFailure>();
    let _ = std::mem::size_of::<TurnTaskResult>();
    let _: fn(LegacyToolCallSummary) -> RunnerEvent = RunnerEvent::ToolStarted;
    let _: fn(LegacyToolResultSummary) -> RunnerEvent = RunnerEvent::ToolFinished;
    let _ = RunnerEventSink::channel;
    let _ = RunnerEventSink::try_publish_model;
    let _ = RunnerEventSink::try_publish_tool;
    let _ = run_turn;
};
