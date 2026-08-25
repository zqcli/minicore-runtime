use std::collections::BTreeSet;

use tokio::time::Instant as TokioInstant;

use crate::compaction::{CompactionDriverFailure, CompactionError};
use crate::context::ContextRequest;
use crate::conversation::{ConversationSeq, SummaryDraft};
use crate::model::{ModelRequest, Usage};
use crate::prompt::{PromptError, PromptPlan};
use crate::time::DeadlineSource;

use super::super::runner_protocol::RunnerOutcome;
use super::super::turn_context::TurnRunnerContext;
use super::diagnostics::{
    budget_exceeded, compaction_failure, context_failure, critical_failure, prompt_failure,
};
use super::support::{commit_summary, validate_summary_ack};

#[derive(Default)]
pub(super) struct CompactionState {
    proactive_heads: BTreeSet<ConversationSeq>,
}

pub(super) enum PreparedModelRequest {
    Ready(ModelRequest),
    Terminal(RunnerOutcome),
}

pub(super) enum CompactionAttempt {
    Applied,
    Skipped,
    Terminal(RunnerOutcome),
}

#[derive(Clone, Copy)]
enum CompactionMode {
    Proactive,
    Forced,
}

fn plan_current_head(context: &TurnRunnerContext) -> Result<PromptPlan, PromptError> {
    context
        .environment
        .prompt
        .plan(&context.conversation, context.environment.model_limits)
}

pub(super) async fn prepare_model_request(
    context: &mut TurnRunnerContext,
    model_round: u16,
    usage: Usage,
    state: &mut CompactionState,
) -> PreparedModelRequest {
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return PreparedModelRequest::Terminal(outcome);
    }
    let mut plan = match plan_current_head(context) {
        Ok(plan) => plan,
        Err(error) => return PreparedModelRequest::Terminal(prompt_failure(error, usage)),
    };
    let mut compaction_applied = false;
    match proactive(context, usage, state, &plan).await {
        CompactionAttempt::Applied => {
            compaction_applied = true;
            if let Some(outcome) = turn_control_outcome(context, usage) {
                return PreparedModelRequest::Terminal(outcome);
            }
            plan = match plan_current_head(context) {
                Ok(plan) => plan,
                Err(error) => return PreparedModelRequest::Terminal(prompt_failure(error, usage)),
            };
        }
        CompactionAttempt::Skipped => {}
        CompactionAttempt::Terminal(outcome) => {
            return PreparedModelRequest::Terminal(outcome);
        }
    }
    let mut forced_attempted = false;
    loop {
        if let Some(outcome) = turn_control_outcome(context, usage) {
            return PreparedModelRequest::Terminal(outcome);
        }
        let remaining = match plan.remaining_context_budget() {
            Ok(remaining) => remaining,
            Err(PromptError::ContextOverflow) if !forced_attempted && !compaction_applied => {
                if let Some(outcome) = turn_control_outcome(context, usage) {
                    return PreparedModelRequest::Terminal(outcome);
                }
                forced_attempted = true;
                match required(context, usage).await {
                    CompactionAttempt::Applied => {
                        compaction_applied = true;
                        plan = match plan_current_head(context) {
                            Ok(plan) => plan,
                            Err(error) => {
                                return PreparedModelRequest::Terminal(prompt_failure(
                                    error, usage,
                                ));
                            }
                        };
                        continue;
                    }
                    CompactionAttempt::Skipped => {
                        return PreparedModelRequest::Terminal(compaction_failure(usage));
                    }
                    CompactionAttempt::Terminal(outcome) => {
                        return PreparedModelRequest::Terminal(outcome);
                    }
                }
            }
            Err(PromptError::ContextOverflow) => {
                return PreparedModelRequest::Terminal(
                    turn_control_outcome(context, usage)
                        .unwrap_or_else(|| compaction_failure(usage)),
                );
            }
            Err(error) => {
                return PreparedModelRequest::Terminal(prompt_failure(error, usage));
            }
        };
        let validated_context = match context
            .environment
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
            Ok(validated_context) => validated_context,
            Err(failure) => {
                return PreparedModelRequest::Terminal(context_failure(failure, usage));
            }
        };
        if let Some(outcome) = turn_control_outcome(context, usage) {
            return PreparedModelRequest::Terminal(outcome);
        }
        match plan.finish(&validated_context) {
            Ok(request) => {
                return match turn_control_outcome(context, usage) {
                    Some(outcome) => PreparedModelRequest::Terminal(outcome),
                    None => PreparedModelRequest::Ready(request),
                };
            }
            Err(PromptError::ContextOverflow) if !forced_attempted && !compaction_applied => {
                if let Some(outcome) = turn_control_outcome(context, usage) {
                    return PreparedModelRequest::Terminal(outcome);
                }
                forced_attempted = true;
                match required(context, usage).await {
                    CompactionAttempt::Applied => {
                        compaction_applied = true;
                        plan = match plan_current_head(context) {
                            Ok(plan) => plan,
                            Err(error) => {
                                return PreparedModelRequest::Terminal(prompt_failure(
                                    error, usage,
                                ));
                            }
                        };
                        continue;
                    }
                    CompactionAttempt::Skipped => {
                        return PreparedModelRequest::Terminal(compaction_failure(usage));
                    }
                    CompactionAttempt::Terminal(outcome) => {
                        return PreparedModelRequest::Terminal(outcome);
                    }
                }
            }
            Err(PromptError::ContextOverflow) => {
                return PreparedModelRequest::Terminal(
                    turn_control_outcome(context, usage)
                        .unwrap_or_else(|| compaction_failure(usage)),
                );
            }
            Err(error) => {
                return PreparedModelRequest::Terminal(prompt_failure(error, usage));
            }
        }
    }
}

pub(super) async fn proactive(
    context: &mut TurnRunnerContext,
    usage: Usage,
    state: &mut CompactionState,
    plan: &PromptPlan,
) -> CompactionAttempt {
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    let Some(compaction) = context.environment.compaction.as_ref() else {
        return CompactionAttempt::Skipped;
    };
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    if plan.fixed_input_tokens() < compaction.trigger_tokens {
        return CompactionAttempt::Skipped;
    }
    let head = context.conversation.head();
    if !state.proactive_heads.insert(head) {
        return CompactionAttempt::Skipped;
    }
    attempt(context, usage, CompactionMode::Proactive).await
}

async fn required(context: &mut TurnRunnerContext, usage: Usage) -> CompactionAttempt {
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    if context.environment.compaction.is_none() {
        return CompactionAttempt::Terminal(compaction_failure(usage));
    }
    attempt(context, usage, CompactionMode::Forced).await
}

async fn attempt(
    context: &mut TurnRunnerContext,
    usage: Usage,
    mode: CompactionMode,
) -> CompactionAttempt {
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    let candidate = match context
        .conversation
        .validated_compaction_candidate(&context.environment.spec, &context.environment.limits)
    {
        Ok(candidate) => candidate,
        Err(_) => return failed_attempt(mode, usage),
    };
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    let Some(compaction) = context.environment.compaction.as_ref() else {
        return failed_attempt(mode, usage);
    };
    let proposal = match compaction
        .driver
        .run_detailed(
            context.session_id,
            context.turn_id,
            candidate,
            compaction.target_tokens,
            context.deadline,
            context.cancellation.clone(),
        )
        .await
    {
        Ok(proposal) => proposal,
        Err(failure) => return driver_failure(failure, mode, usage),
    };
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    let snapshot_head = proposal.snapshot_head();
    if context.conversation.head() != snapshot_head {
        return CompactionAttempt::Terminal(compaction_failure(usage));
    }
    let draft = SummaryDraft {
        through: proposal.through_seq(),
        summary: proposal.summary().clone(),
    };
    let acknowledgement = match commit_summary(context, snapshot_head, draft.clone()).await {
        Ok(acknowledgement) => acknowledgement,
        Err(error) => return CompactionAttempt::Terminal(critical_failure(error, usage)),
    };
    let conversation = match validate_summary_ack(context, snapshot_head, &draft, acknowledgement) {
        Ok(conversation) => conversation,
        Err(error) => return CompactionAttempt::Terminal(critical_failure(error, usage)),
    };
    context.conversation = conversation;
    CompactionAttempt::Applied
}

fn driver_failure(
    failure: CompactionDriverFailure,
    mode: CompactionMode,
    usage: Usage,
) -> CompactionAttempt {
    match (failure.error(), failure.deadline_source()) {
        (CompactionError::Cancelled, _) => {
            CompactionAttempt::Terminal(RunnerOutcome::Cancelled { usage })
        }
        (CompactionError::DeadlineExceeded, Some(DeadlineSource::Turn)) => {
            CompactionAttempt::Terminal(budget_exceeded(usage))
        }
        _ => failed_attempt(mode, usage),
    }
}

fn failed_attempt(mode: CompactionMode, usage: Usage) -> CompactionAttempt {
    match mode {
        CompactionMode::Proactive => CompactionAttempt::Skipped,
        CompactionMode::Forced => CompactionAttempt::Terminal(compaction_failure(usage)),
    }
}

pub(super) fn turn_control_outcome(
    context: &TurnRunnerContext,
    usage: Usage,
) -> Option<RunnerOutcome> {
    if context.cancellation.is_cancelled() {
        Some(RunnerOutcome::Cancelled { usage })
    } else if TokioInstant::now() >= TokioInstant::from_std(context.deadline) {
        Some(budget_exceeded(usage))
    } else {
        None
    }
}
