use super::compaction_support::*;
use super::*;
use crate::compaction::CompactionError;
use crate::conversation::SummaryDraft;
use tokio::sync::oneshot;

#[cfg(test)]
mod priority;

fn forced_request(
    strategy: Arc<ScriptCompaction>,
    cancellation: CancellationToken,
    turn_after: Duration,
    context_timeout: Duration,
    critical_capacity: usize,
) -> (
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    mpsc::Receiver<RunnerProgress>,
    Arc<ScriptModel>,
) {
    let model = ScriptModel::new(300, Vec::new());
    let spec = enabled_spec(&[], 4, 10_000, 100);
    let conversation = active_conversation(&spec, 4, &"x".repeat(8_000));
    let bindings = bindings_with_compaction(Arc::clone(&model), None, Vec::new(), None, strategy);
    let kernel = KernelConfig {
        context_timeout,
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, critical_rx, progress_rx) = request_with_compaction_kernel(
        spec,
        bindings,
        conversation,
        kernel,
        cancellation,
        turn_after,
        critical_capacity,
    );
    (request, critical_rx, progress_rx, model)
}

fn proactive_request(
    strategy: Arc<ScriptCompaction>,
    cancellation: CancellationToken,
    turn_after: Duration,
    context_timeout: Duration,
) -> (
    SessionSpec,
    ConversationView,
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    Arc<ScriptModel>,
) {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let spec = enabled_spec(&[], 4, 2, 1);
    let conversation = active_conversation(&spec, 4, "old history");
    let bindings = bindings_with_compaction(Arc::clone(&model), None, Vec::new(), None, strategy);
    let kernel = KernelConfig {
        context_timeout,
        ..KernelConfig::default_checked().unwrap()
    };
    let (request, critical_rx, _progress_rx) = request_with_compaction_kernel(
        spec.clone(),
        bindings,
        conversation.clone(),
        kernel,
        cancellation,
        turn_after,
        4,
    );
    (spec, conversation, request, critical_rx, model)
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn proactive_configured_timeout_is_skipped_and_model_continues() {
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Pending]);
    let (spec, conversation, request, mut critical_rx, model) = proactive_request(
        Arc::clone(&strategy),
        CancellationToken::new(),
        Duration::from_secs(30),
        Duration::from_secs(5),
    );
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => reply
            .send(Ok(ack_assistant(&conversation, &draft, &spec)))
            .unwrap(),
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::Completed { .. }
    ));
    assert_eq!(strategy.calls(), 1);
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn proactive_parent_cancellation_is_terminal_without_model_continuation() {
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Pending]);
    let cancellation = CancellationToken::new();
    let (_spec, _conversation, request, _critical_rx, model) = proactive_request(
        Arc::clone(&strategy),
        cancellation.clone(),
        Duration::from_secs(30),
        Duration::from_secs(30),
    );
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    cancellation.cancel();
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::Cancelled { .. }
    ));
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn proactive_turn_deadline_is_terminal_without_model_continuation() {
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Pending]);
    let (_spec, _conversation, request, _critical_rx, model) = proactive_request(
        Arc::clone(&strategy),
        CancellationToken::new(),
        Duration::from_secs(5),
        Duration::from_secs(30),
    );
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::BudgetExceeded { .. }
    ));
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn forced_compaction_parent_cancellation_is_cancelled() {
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Pending]);
    let cancellation = CancellationToken::new();
    let (request, _critical_rx, _progress_rx, model) = forced_request(
        Arc::clone(&strategy),
        cancellation.clone(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    cancellation.cancel();
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::Cancelled { .. }
    ));
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn forced_compaction_turn_deadline_is_budget_exceeded() {
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Pending]);
    let (request, _critical_rx, _progress_rx, model) = forced_request(
        Arc::clone(&strategy),
        CancellationToken::new(),
        Duration::from_secs(5),
        Duration::from_secs(30),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::BudgetExceeded { .. }
    ));
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn forced_compaction_configured_timeout_is_a_compaction_failure() {
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Pending]);
    let (request, _critical_rx, _progress_rx, model) = forced_request(
        Arc::clone(&strategy),
        CancellationToken::new(),
        Duration::from_secs(30),
        Duration::from_secs(5),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    let outcome = joined_outcome(task).await;
    assert_eq!(
        outcome.diagnostic().unwrap().category,
        crate::error::DiagnosticCategory::Compaction
    );
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn adapter_deadline_error_is_not_misclassified_as_the_turn_deadline() {
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Error(
        CompactionError::DeadlineExceeded,
    )]);
    let (request, _critical_rx, _progress_rx, model) = forced_request(
        strategy,
        CancellationToken::new(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    let outcome = joined_outcome(task).await;
    assert_eq!(
        outcome.diagnostic().unwrap().category,
        crate::error::DiagnosticCategory::Compaction
    );
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn full_summary_send_is_cancellable_without_a_delayed_commit() {
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(3, "summary"))]);
    let cancellation = CancellationToken::new();
    let (request, mut critical_rx, _progress_rx, _model) = forced_request(
        Arc::clone(&strategy),
        cancellation.clone(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        1,
    );
    let (reply, _receiver) = oneshot::channel();
    request
        .critical_tx
        .try_send(RunnerEvent::CommitSummary {
            snapshot_head: ConversationSeq::new(4),
            draft: SummaryDraft {
                through: ConversationSeq::new(3),
                summary: BoundedText::new("queued").unwrap(),
            },
            reply,
        })
        .unwrap();
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    cancellation.cancel();
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::Cancelled { .. }
    ));
    assert!(matches!(
        critical_rx.try_recv(),
        Ok(RunnerEvent::CommitSummary { .. })
    ));
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn full_summary_send_deadline_drops_the_pending_commit_without_a_delayed_event() {
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(3, "summary"))]);
    let (request, mut critical_rx, _progress_rx, _model) = forced_request(
        Arc::clone(&strategy),
        CancellationToken::new(),
        Duration::from_secs(5),
        Duration::from_secs(30),
        1,
    );
    let (reply, _receiver) = oneshot::channel();
    request
        .critical_tx
        .try_send(RunnerEvent::CommitSummary {
            snapshot_head: ConversationSeq::new(4),
            draft: SummaryDraft {
                through: ConversationSeq::new(3),
                summary: BoundedText::new("queued").unwrap(),
            },
            reply,
        })
        .unwrap();
    let task = tokio::spawn(run_turn(request));
    strategy.wait_called().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::BudgetExceeded { .. }
    ));
    assert!(critical_rx.try_recv().is_ok());
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_while_waiting_for_summary_ack_has_no_model_continuation() {
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(3, "summary"))]);
    let cancellation = CancellationToken::new();
    let (request, mut critical_rx, _progress_rx, model) = forced_request(
        strategy,
        cancellation.clone(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    let reply = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary { reply, .. } => reply,
        event => panic!("unexpected event: {event:?}"),
    };
    cancellation.cancel();
    drop(reply);
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::Cancelled { .. }
    ));
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn deadline_while_waiting_for_summary_ack_has_no_model_continuation() {
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(3, "summary"))]);
    let (request, mut critical_rx, _progress_rx, model) = forced_request(
        strategy,
        CancellationToken::new(),
        Duration::from_secs(5),
        Duration::from_secs(30),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    let reply = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary { reply, .. } => reply,
        event => panic!("unexpected event: {event:?}"),
    };
    tokio::time::advance(Duration::from_secs(6)).await;
    drop(reply);
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::BudgetExceeded { .. }
    ));
    assert!(model.requests().is_empty());
}
