use std::collections::BTreeSet;

use tokio::time::Instant as TokioInstant;

use crate::compaction::{CompactionDriverFailure, CompactionError};
use crate::context::ContextRequest;
use crate::conversation::{ConversationSeq, SummaryDraft};
use crate::model::{ModelRequest, Usage};
use crate::prompt::{PromptBuilder, PromptError};
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

/// Caches the fixed (context-free) input estimate for one conversation head.
/// Proactive compaction and the context budget both need it, and it only
/// changes when a commit advances the head, so a round serialises once.
#[derive(Default)]
pub(super) struct FixedInputEstimate {
    cached: Option<(ConversationSeq, u64)>,
}

impl FixedInputEstimate {
    fn get(&mut self, context: &TurnRunnerContext) -> Result<u64, PromptError> {
        let head = context.conversation.head();
        if let Some((cached_head, estimate)) = self.cached {
            if cached_head == head {
                return Ok(estimate);
            }
        }
        let estimate = context.environment.prompt.estimated_fixed_input_tokens(
            &context.conversation,
            context.environment.model_limits,
        )?;
        self.cached = Some((head, estimate));
        Ok(estimate)
    }
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

pub(super) async fn prepare_model_request(
    context: &mut TurnRunnerContext,
    model_round: u16,
    usage: Usage,
    state: &mut CompactionState,
) -> PreparedModelRequest {
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return PreparedModelRequest::Terminal(outcome);
    }
    let mut estimate = FixedInputEstimate::default();
    if let CompactionAttempt::Terminal(outcome) =
        proactive(context, usage, state, &mut estimate).await
    {
        return PreparedModelRequest::Terminal(outcome);
    }

    let mut forced_attempted = false;
    loop {
        if let Some(outcome) = turn_control_outcome(context, usage) {
            return PreparedModelRequest::Terminal(outcome);
        }
        let remaining = match estimate.get(context).and_then(|fixed| {
            PromptBuilder::remaining_context_budget_for(fixed, context.environment.model_limits)
        }) {
            Ok(remaining) => remaining,
            Err(PromptError::ContextOverflow) if !forced_attempted => {
                if let Some(outcome) = turn_control_outcome(context, usage) {
                    return PreparedModelRequest::Terminal(outcome);
                }
                forced_attempted = true;
                match required(context, usage).await {
                    CompactionAttempt::Applied => continue,
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
        let context_bundle = match context
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
            Ok(context_bundle) => context_bundle,
            Err(failure) => {
                return PreparedModelRequest::Terminal(context_failure(failure, usage));
            }
        };
        if let Some(outcome) = turn_control_outcome(context, usage) {
            return PreparedModelRequest::Terminal(outcome);
        }
        match context.environment.prompt.build(
            &context.conversation,
            &context_bundle,
            context.environment.model_limits,
        ) {
            Ok(request) => {
                return match turn_control_outcome(context, usage) {
                    Some(outcome) => PreparedModelRequest::Terminal(outcome),
                    None => PreparedModelRequest::Ready(request),
                };
            }
            Err(PromptError::ContextOverflow) if !forced_attempted => {
                if let Some(outcome) = turn_control_outcome(context, usage) {
                    return PreparedModelRequest::Terminal(outcome);
                }
                forced_attempted = true;
                match required(context, usage).await {
                    CompactionAttempt::Applied => continue,
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
    estimate: &mut FixedInputEstimate,
) -> CompactionAttempt {
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    let Some(compaction) = context.environment.compaction.as_ref() else {
        return CompactionAttempt::Skipped;
    };
    let estimate = match estimate.get(context) {
        Ok(estimate) => estimate,
        Err(_) => return CompactionAttempt::Skipped,
    };
    if let Some(outcome) = turn_control_outcome(context, usage) {
        return CompactionAttempt::Terminal(outcome);
    }
    if estimate < compaction.trigger_tokens {
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
