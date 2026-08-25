use super::compaction_support::*;
use super::*;

fn forced_ack_case() -> (
    SessionSpec,
    ConversationView,
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    Arc<ScriptModel>,
) {
    let model = ScriptModel::new(300, Vec::new());
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(3, "summary"))]);
    let spec = enabled_spec(&[], 4, 10_000, 100);
    let initial = active_conversation(&spec, 4, &"x".repeat(8_000));
    let bindings = bindings_with_compaction(Arc::clone(&model), None, Vec::new(), None, strategy);
    let (request, critical_rx, _progress_rx) =
        runner_request(spec.clone(), 4, bindings, initial.clone());
    (spec, initial, request, critical_rx, model)
}

async fn assert_commit_failure(
    task: tokio::task::JoinHandle<TurnRunnerExit>,
    critical_rx: &mut mpsc::Receiver<RunnerEvent>,
    model: &ScriptModel,
    code: crate::error::DiagnosticCode,
    category: crate::error::DiagnosticCategory,
) {
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    let diagnostic = outcome.diagnostic().unwrap();
    assert_eq!((diagnostic.code, diagnostic.category), (code, category));
    assert_finished(task.await.unwrap());
    assert!(model.requests().is_empty());
    assert!(critical_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn stale_summary_snapshot_is_rejected_before_actor_append() {
    let (spec, initial, request, mut critical_rx, model) = forced_ack_case();
    let task = tokio::spawn(run_turn(request));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary {
            snapshot_head,
            draft,
            reply,
        } => {
            assert_eq!(snapshot_head, ConversationSeq::new(4));
            let current = ack_summary(&initial, snapshot_head, &draft, &spec)
                .unwrap()
                .conversation;
            assert_eq!(
                ack_summary(&current, snapshot_head, &draft, &spec),
                Err(RunnerCommitError::Stale)
            );
            reply.send(Err(RunnerCommitError::Stale)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert_commit_failure(
        task,
        &mut critical_rx,
        &model,
        crate::error::DiagnosticCode::SessionBusy,
        crate::error::DiagnosticCategory::Internal,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn summary_ack_rejects_active_turn_provenance_change() {
    let (spec, initial, request, mut critical_rx, model) = forced_ack_case();
    let task = tokio::spawn(run_turn(request));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary {
            snapshot_head,
            draft,
            reply,
        } => {
            let update = ack_summary(&initial, snapshot_head, &draft, &spec).unwrap();
            let mut entries = update.conversation.entries().to_vec();
            let active = entries
                .iter_mut()
                .rev()
                .find_map(|entry| match entry {
                    ConversationEntry::UserMessage(entry) => Some(entry),
                    _ => None,
                })
                .unwrap();
            active.execution.max_tool_rounds = 3;
            let conversation = ConversationView::from_validated_entries(
                &spec,
                &SemanticLimits::default(),
                entries.into(),
            )
            .unwrap();
            assert!(conversation.is_validated_for(&spec, &SemanticLimits::default()));
            reply
                .send(Ok(CommittedUpdate {
                    previous_head: update.previous_head,
                    entry: update.entry,
                    conversation,
                }))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert_commit_failure(
        task,
        &mut critical_rx,
        &model,
        crate::error::DiagnosticCode::SessionBusy,
        crate::error::DiagnosticCategory::Internal,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn summary_ack_must_end_with_the_exact_committed_summary() {
    let (spec, initial, request, mut critical_rx, model) = forced_ack_case();
    let task = tokio::spawn(run_turn(request));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary {
            snapshot_head,
            draft,
            reply,
        } => {
            let acknowledgement = ack_summary(&initial, snapshot_head, &draft, &spec).unwrap();
            let mut entries = acknowledgement.conversation.entries().to_vec();
            let entry = match entries.last_mut().unwrap() {
                ConversationEntry::Summary(entry) => {
                    entry.summary = BoundedText::new("forged summary").unwrap();
                    ConversationEntry::Summary(entry.clone())
                }
                entry => panic!("unexpected entry: {entry:?}"),
            };
            let conversation = ConversationView::from_validated_entries(
                &spec,
                &SemanticLimits::default(),
                entries.into(),
            )
            .unwrap();
            reply
                .send(Ok(CommittedUpdate {
                    previous_head: acknowledgement.previous_head,
                    entry,
                    conversation,
                }))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert_commit_failure(
        task,
        &mut critical_rx,
        &model,
        crate::error::DiagnosticCode::SessionBusy,
        crate::error::DiagnosticCategory::Internal,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn every_summary_commit_error_uses_the_existing_critical_taxonomy() {
    for (error, code, category) in [
        (
            RunnerCommitError::Stale,
            crate::error::DiagnosticCode::SessionBusy,
            crate::error::DiagnosticCategory::Internal,
        ),
        (
            RunnerCommitError::Degraded,
            crate::error::DiagnosticCode::SessionDegraded,
            crate::error::DiagnosticCategory::Storage,
        ),
        (
            RunnerCommitError::DurabilityUnavailable,
            crate::error::DiagnosticCode::LogConflict,
            crate::error::DiagnosticCategory::Storage,
        ),
        (
            RunnerCommitError::DurabilityUnknown,
            crate::error::DiagnosticCode::LogUnknownOutcome,
            crate::error::DiagnosticCategory::Storage,
        ),
        (
            RunnerCommitError::RuntimeClosed,
            crate::error::DiagnosticCode::RuntimeTerminated,
            crate::error::DiagnosticCategory::Internal,
        ),
    ] {
        let (_spec, _initial, request, mut critical_rx, model) = forced_ack_case();
        let task = tokio::spawn(run_turn(request));
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::CommitSummary { reply, .. } => reply.send(Err(error)).unwrap(),
            event => panic!("unexpected event: {event:?}"),
        }
        assert_commit_failure(task, &mut critical_rx, &model, code, category).await;
    }
}
