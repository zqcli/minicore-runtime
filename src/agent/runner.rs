use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use crate::context::ContextRequest;
use crate::conversation::ToolResultDraft;
use crate::model::{ModelCallContext, ModelDriverFailure, ModelResponse};
use crate::tools::ToolInvocation;

use super::runner_protocol::{RunnerEvent, RunnerOutcome, RunnerProgress, TurnRunnerExit};
use super::tool_driver::ToolDriverResult;
use super::turn_context::{TurnRunnerContext, TurnRunnerRequest, TurnRunnerRequestError};

mod diagnostics;
mod support;

use diagnostics::{
    budget_exceeded, context_failure, critical_failure, internal_failure, model_failure,
    prompt_failure, request_failure,
};
use support::{
    CriticalFailure, FinishControl, UsageAccumulator, assistant_draft, commit_assistant,
    commit_tool_result, finish_outcome, model_progress, progress, send_critical,
    send_critical_for_context, tool_progress, validate_assistant_ack, validate_tool_ack,
};

const LOCAL_PROGRESS_CAPACITY: usize = 64;
const LOCAL_SUSPENSION_CAPACITY: usize = 1;

pub(crate) async fn run_turn(request: TurnRunnerRequest) -> TurnRunnerExit {
    let finish = FinishControl {
        sender: request.critical_tx.clone(),
        cancellation: request.cancellation.clone(),
        deadline: request.deadline,
    };
    let task = async move {
        let mut context = TurnRunnerContext::new(request)?;
        Ok::<RunnerOutcome, TurnRunnerRequestError>(run_ordinary_loop(&mut context).await)
    };
    match AssertUnwindSafe(task).catch_unwind().await {
        Ok(Ok(outcome)) => finish_outcome(&finish, outcome).await,
        Ok(Err(error)) => finish_outcome(&finish, request_failure(error)).await,
        Err(_) => {
            let _ = send_critical(
                &finish,
                RunnerEvent::Finish {
                    outcome: internal_failure(
                        "turn runner panicked",
                        crate::model::Usage::default(),
                    ),
                },
            )
            .await;
            TurnRunnerExit::Panicked
        }
    }
}

async fn run_ordinary_loop(context: &mut TurnRunnerContext) -> RunnerOutcome {
    let mut model_round = 0_u16;
    let mut tool_round = 0_u16;
    let mut usage = UsageAccumulator::default();
    #[cfg(test)]
    if tests::take_scripted_turn_panic(context.turn_id) {
        panic!("scripted turn runner panic after context creation");
    }
    loop {
        if context.cancellation.is_cancelled() {
            return RunnerOutcome::Cancelled {
                usage: usage.current(),
            };
        }
        if TokioInstant::now() >= TokioInstant::from_std(context.deadline) {
            return budget_exceeded(usage.current());
        }

        let remaining = match context
            .prompt
            .remaining_context_budget(&context.conversation, context.model_limits)
        {
            Ok(remaining) => remaining,
            Err(error) => return prompt_failure(error, usage.current()),
        };
        let context_bundle = match context
            .context
            .provide_detailed(ContextRequest {
                session_id: context.session_id,
                instance_id: context.instance_id,
                turn_id: context.turn_id,
                model_round,
                conversation: context.conversation.clone(),
                remaining_context_budget: remaining,
                cancellation: context.cancellation.clone(),
                deadline: context.deadline,
            })
            .await
        {
            Ok(bundle) => bundle,
            Err(error) => return context_failure(error, usage.current()),
        };
        let request =
            match context
                .prompt
                .build(&context.conversation, &context_bundle, context.model_limits)
            {
                Ok(request) => request,
                Err(error) => return prompt_failure(error, usage.current()),
            };

        progress(context, RunnerProgress::ModelStarted { model_round });
        let response = match run_model(context, request, model_round).await {
            Ok(response) => response,
            Err(error) => return model_failure(error, usage.current()),
        };
        let round_usage = response.usage().copied().unwrap_or_default();
        if usage.add(round_usage).is_err() {
            return internal_failure("turn usage overflowed", usage.current());
        }
        progress(
            context,
            RunnerProgress::ModelFinished {
                model_round,
                usage: round_usage,
            },
        );

        let (draft, tool_calls) = match assistant_draft(context, &response) {
            Ok(value) => value,
            Err(()) => {
                return internal_failure("model response projection failed", usage.current());
            }
        };
        let previous_head = context.conversation.head();
        let acknowledgement = match commit_assistant(context, draft.clone()).await {
            Ok(acknowledgement) => acknowledgement,
            Err(error) => return critical_failure(error, usage.current()),
        };
        let conversation =
            match validate_assistant_ack(context, previous_head, &draft, acknowledgement) {
                Ok(conversation) => conversation,
                Err(error) => return critical_failure(error, usage.current()),
            };
        context.conversation = conversation;

        if tool_calls.is_empty() {
            return RunnerOutcome::Completed {
                usage: usage.finish(),
            };
        }
        if tool_round >= context.effective_max_tool_rounds {
            return budget_exceeded(usage.current());
        }

        for call in tool_calls {
            let invocation = match ToolInvocation::new(
                context.session_id,
                context.instance_id,
                context.turn_id,
                call.tool_call_id().clone(),
                call.name().clone(),
                call.arguments().clone(),
            ) {
                Ok(invocation) => invocation,
                Err(_) => {
                    return internal_failure("tool invocation projection failed", usage.current());
                }
            };
            let result = match run_tool(context, invocation).await {
                Ok(result) => result,
                Err(error) => return critical_failure(error, usage.current()),
            };
            let draft = ToolResultDraft {
                turn_id: context.turn_id,
                tool_call_id: call.tool_call_id().clone(),
                tool_name: call.name().clone(),
                outcome: result.outcome(),
                content: result.output().content().clone(),
            };
            let previous_head = context.conversation.head();
            let acknowledgement = match commit_tool_result(context, draft.clone()).await {
                Ok(acknowledgement) => acknowledgement,
                Err(error) => return critical_failure(error, usage.current()),
            };
            let conversation =
                match validate_tool_ack(context, previous_head, &draft, acknowledgement) {
                    Ok(conversation) => conversation,
                    Err(error) => return critical_failure(error, usage.current()),
                };
            context.conversation = conversation;
            progress(
                context,
                RunnerProgress::ToolFinished {
                    tool_call_id: draft.tool_call_id,
                    tool_name: draft.tool_name,
                    outcome: draft.outcome,
                    content_bytes: draft.content.byte_len(),
                },
            );
        }
        tool_round = match tool_round.checked_add(1) {
            Some(value) => value,
            None => return internal_failure("tool round overflowed", usage.current()),
        };
        model_round = match model_round.checked_add(1) {
            Some(value) => value,
            None => return internal_failure("model round overflowed", usage.current()),
        };
    }
}

async fn run_model(
    context: &TurnRunnerContext,
    request: crate::model::ModelRequest,
    model_round: u16,
) -> Result<ModelResponse, ModelDriverFailure> {
    let (progress_tx, mut progress_rx) = mpsc::channel(LOCAL_PROGRESS_CAPACITY);
    let result = {
        let run = context.model.run_detailed(
            request,
            ModelCallContext::new(
                context.session_id,
                context.instance_id,
                context.turn_id,
                model_round,
                context.cancellation.clone(),
                context.deadline,
            ),
            &progress_tx,
        );
        tokio::pin!(run);
        loop {
            tokio::select! {
                biased;
                result = &mut run => break result,
                value = progress_rx.recv() => match value {
                    Some(value) => model_progress(context, model_round, value),
                    None => continue,
                }
            }
        }
    };
    drop(progress_tx);
    while let Ok(value) = progress_rx.try_recv() {
        model_progress(context, model_round, value);
    }
    result
}

async fn run_tool(
    context: &TurnRunnerContext,
    invocation: ToolInvocation,
) -> Result<ToolDriverResult, CriticalFailure> {
    let (suspension_tx, mut suspension_rx) = mpsc::channel(LOCAL_SUSPENSION_CAPACITY);
    let (progress_tx, mut progress_rx) = mpsc::channel(LOCAL_PROGRESS_CAPACITY);
    let result = {
        let run = context.tools.run(
            invocation,
            context.deadline,
            context.cancellation.clone(),
            &suspension_tx,
            &progress_tx,
        );
        tokio::pin!(run);
        loop {
            tokio::select! {
                biased;
                result = &mut run => break result.map_err(CriticalFailure::Suspension),
                value = progress_rx.recv() => match value {
                    Some(value) => tool_progress(context, value),
                    None => continue,
                },
                value = suspension_rx.recv() => match value {
                    Some(suspension) => {
                        while let Ok(value) = progress_rx.try_recv() {
                            tool_progress(context, value);
                        }
                        send_critical_for_context(
                            context,
                            RunnerEvent::Suspend { suspension },
                        ).await?;
                    }
                    None => continue,
                }
            }
        }
    };
    drop(suspension_tx);
    drop(progress_tx);
    while let Ok(value) = progress_rx.try_recv() {
        tool_progress(context, value);
    }
    result
}

#[cfg(test)]
mod tests;
