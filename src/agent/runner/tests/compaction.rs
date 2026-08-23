use super::compaction_support::*;
use super::*;
use crate::agent::runner::compaction::{CompactionAttempt, CompactionState, proactive};
use crate::agent::turn_context::TurnRunnerContext;
use crate::compaction::CompactionError;

#[cfg(test)]
mod usage;

async fn complete_final_assistant(
    critical_rx: &mut mpsc::Receiver<RunnerEvent>,
    conversation: &ConversationView,
    spec: &SessionSpec,
) -> RunnerOutcome {
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            reply
                .send(Ok(ack_assistant(conversation, &draft, spec)))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn proactive_trigger_equality_and_same_head_suppression_are_exact() {
    let model = ScriptModel::new(4_096, Vec::new());
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Error(CompactionError::Internal)]);
    let spec = enabled_spec(&[], 4, 100, 1);
    let conversation = active_conversation(&spec, 4, "old history");
    let bindings = bindings_with_compaction(model, None, Vec::new(), None, Arc::clone(&strategy));
    let (request, _critical_rx, _progress_rx) = runner_request(spec, 4, bindings, conversation);
    let mut context = TurnRunnerContext::new(request).unwrap();
    let estimate = context
        .prompt
        .estimated_fixed_input_tokens(&context.conversation, context.model_limits)
        .unwrap();
    assert!(estimate > 1);
    let mut state = CompactionState::default();

    context.compaction.as_mut().unwrap().trigger_tokens = estimate.checked_add(1).unwrap();
    assert!(matches!(
        proactive(&mut context, Usage::default(), &mut state).await,
        CompactionAttempt::Skipped
    ));
    assert_eq!(strategy.calls(), 0);

    context.compaction.as_mut().unwrap().trigger_tokens = estimate;
    assert!(matches!(
        proactive(&mut context, Usage::default(), &mut state).await,
        CompactionAttempt::Skipped
    ));
    assert_eq!(strategy.calls(), 1);
    assert!(matches!(
        proactive(&mut context, Usage::default(), &mut state).await,
        CompactionAttempt::Skipped
    ));
    assert_eq!(strategy.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn proactive_compaction_commits_before_context_and_model_use_the_new_view() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let context = ScriptContext::new(vec![Ok(ContextBundle { blocks: Vec::new() })]);
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(
        3,
        "summary of prior turn",
    ))]);
    let spec = enabled_spec(&[], 4, 2, 1);
    let initial = active_conversation(&spec, 4, "old private history");
    let bindings = bindings_with_compaction(
        Arc::clone(&model),
        Some(Arc::clone(&context)),
        Vec::new(),
        None,
        Arc::clone(&strategy),
    );
    let (request, mut critical_rx, _progress_rx) =
        runner_request(spec.clone(), 4, bindings, initial.clone());
    let task = tokio::spawn(run_turn(request));

    let conversation = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary {
            snapshot_head,
            draft,
            reply,
        } => {
            assert_eq!(snapshot_head, ConversationSeq::new(4));
            assert_eq!(draft.through, ConversationSeq::new(3));
            assert!(context.requests().is_empty());
            assert!(model.requests().is_empty());
            let acknowledgement = ack_summary(&initial, snapshot_head, &draft, &spec).unwrap();
            let conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
            conversation
        }
        event => panic!("unexpected event: {event:?}"),
    };
    let outcome = complete_final_assistant(&mut critical_rx, &conversation, &spec).await;
    assert!(matches!(outcome, RunnerOutcome::Completed { .. }));
    assert_finished(task.await.unwrap());

    assert_eq!(strategy.calls(), 1);
    assert_eq!(
        strategy.requests()[0].candidate.head(),
        ConversationSeq::new(4)
    );
    assert_eq!(strategy.requests()[0].target_tokens, 1);
    let context_requests = context.requests();
    assert_eq!(context_requests.len(), 1);
    assert_eq!(
        context_requests[0].conversation.head(),
        ConversationSeq::new(5)
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let serialized = serde_json::to_string(&requests[0].0).unwrap();
    assert!(serialized.contains("summary of prior turn"));
    assert!(serialized.contains("current question"));
    assert!(!serialized.contains("old private history"));
}

#[tokio::test(flavor = "current_thread")]
async fn proactive_strategy_failure_is_best_effort_and_does_not_repeat_the_head() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Error(CompactionError::Internal)]);
    let spec = enabled_spec(&[], 4, 2, 1);
    let initial = active_conversation(&spec, 4, "old history");
    let bindings = bindings_with_compaction(
        Arc::clone(&model),
        None,
        Vec::new(),
        None,
        Arc::clone(&strategy),
    );
    let (request, mut critical_rx, _progress_rx) =
        runner_request(spec.clone(), 4, bindings, initial.clone());
    let task = tokio::spawn(run_turn(request));
    let outcome = complete_final_assistant(&mut critical_rx, &initial, &spec).await;
    assert!(matches!(outcome, RunnerOutcome::Completed { .. }));
    assert_finished(task.await.unwrap());
    assert_eq!(strategy.calls(), 1);
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn proactive_compaction_without_a_prior_boundary_continues_without_strategy_call() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let strategy = ScriptCompaction::new(Vec::new());
    let spec = enabled_spec(&[], 4, 2, 1);
    let initial = initial_conversation(&spec, 4);
    let bindings = bindings_with_compaction(
        Arc::clone(&model),
        None,
        Vec::new(),
        None,
        Arc::clone(&strategy),
    );
    let (request, mut critical_rx, _progress_rx) =
        runner_request(spec.clone(), 4, bindings, initial.clone());
    let task = tokio::spawn(run_turn(request));
    let outcome = complete_final_assistant(&mut critical_rx, &initial, &spec).await;
    assert!(matches!(outcome, RunnerOutcome::Completed { .. }));
    assert_finished(task.await.unwrap());
    assert_eq!(strategy.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn forced_final_build_overflow_commits_then_retries_the_same_model_round() {
    let model = ScriptModel::new(
        1_000,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let context = ScriptContext::new(vec![
        Ok(ContextBundle {
            blocks: vec![ContextBlock {
                source: "forced".parse().unwrap(),
                slot: ContextSlot::TurnContext,
                priority: 0,
                content: BoundedText::new("x".repeat(8_000)).unwrap(),
            }],
        }),
        Ok(ContextBundle { blocks: Vec::new() }),
    ]);
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(
        3,
        "forced summary",
    ))]);
    let spec = enabled_spec(&[], 4, 10_000, 100);
    let initial = active_conversation(&spec, 4, "old history");
    let bindings = bindings_with_compaction(
        Arc::clone(&model),
        Some(Arc::clone(&context)),
        Vec::new(),
        None,
        Arc::clone(&strategy),
    );
    let (request, mut critical_rx, _progress_rx) =
        runner_request(spec.clone(), 4, bindings, initial.clone());
    let task = tokio::spawn(run_turn(request));
    let conversation = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary {
            snapshot_head,
            draft,
            reply,
        } => {
            let acknowledgement = ack_summary(&initial, snapshot_head, &draft, &spec).unwrap();
            let conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
            conversation
        }
        event => panic!("unexpected event: {event:?}"),
    };
    let outcome = complete_final_assistant(&mut critical_rx, &conversation, &spec).await;
    assert!(matches!(outcome, RunnerOutcome::Completed { .. }));
    assert_finished(task.await.unwrap());
    assert_eq!(strategy.calls(), 1);
    assert_eq!(context.requests().len(), 2);
    assert_eq!(context.requests()[0].model_round, 0);
    assert_eq!(context.requests()[1].model_round, 0);
    assert_eq!(
        context.requests()[1].conversation.head(),
        ConversationSeq::new(5)
    );
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn forced_compaction_failure_is_a_compaction_diagnostic_without_model_call() {
    let model = ScriptModel::new(300, Vec::new());
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Error(CompactionError::Internal)]);
    let spec = enabled_spec(&[], 4, 10_000, 100);
    let initial = active_conversation(&spec, 4, &"x".repeat(8_000));
    let bindings = bindings_with_compaction(
        Arc::clone(&model),
        None,
        Vec::new(),
        None,
        Arc::clone(&strategy),
    );
    let (request, mut critical_rx, _progress_rx) = runner_request(spec, 4, bindings, initial);
    let task = tokio::spawn(run_turn(request));
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(
        outcome.diagnostic().unwrap().category,
        crate::error::DiagnosticCategory::Compaction
    );
    assert_finished(task.await.unwrap());
    assert_eq!(strategy.calls(), 1);
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn forced_retry_overflow_fails_without_a_second_strategy_call_or_loop() {
    let model = ScriptModel::new(1_000, Vec::new());
    let oversized = || ContextBundle {
        blocks: vec![ContextBlock {
            source: "oversized".parse().unwrap(),
            slot: ContextSlot::TurnContext,
            priority: 0,
            content: BoundedText::new("x".repeat(8_000)).unwrap(),
        }],
    };
    let context = ScriptContext::new(vec![Ok(oversized()), Ok(oversized())]);
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(
        3,
        "forced summary",
    ))]);
    let spec = enabled_spec(&[], 4, 10_000, 100);
    let initial = active_conversation(&spec, 4, "old history");
    let bindings = bindings_with_compaction(
        Arc::clone(&model),
        Some(Arc::clone(&context)),
        Vec::new(),
        None,
        Arc::clone(&strategy),
    );
    let (request, mut critical_rx, _progress_rx) =
        runner_request(spec.clone(), 4, bindings, initial.clone());
    let task = tokio::spawn(run_turn(request));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary {
            snapshot_head,
            draft,
            reply,
        } => reply
            .send(ack_summary(&initial, snapshot_head, &draft, &spec))
            .unwrap(),
        event => panic!("unexpected event: {event:?}"),
    }
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(
        outcome.diagnostic().unwrap().category,
        crate::error::DiagnosticCategory::Compaction
    );
    assert_finished(task.await.unwrap());
    assert_eq!(strategy.calls(), 1);
    assert_eq!(context.requests().len(), 2);
    assert!(model.requests().is_empty());
}
