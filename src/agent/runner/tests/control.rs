use super::*;

#[tokio::test(flavor = "current_thread")]
async fn every_typed_commit_error_becomes_failed_finish_without_continuation() {
    for error in [
        RunnerCommitError::Stale,
        RunnerCommitError::Degraded,
        RunnerCommitError::DurabilityUnavailable,
        RunnerCommitError::DurabilityUnknown,
        RunnerCommitError::RuntimeClosed,
    ] {
        let committed_usage = Usage::new(3, 2, 1);
        let model = ScriptModel::new(
            4_096,
            vec![ModelBehavior::Events(final_events(
                "answer",
                committed_usage,
            ))],
        );
        let spec = session_spec(&[], 4);
        let initial = initial_conversation(&spec, 4);
        let (request, mut critical_rx, _progress_rx) = runner_request(
            spec,
            4,
            session_bindings(Arc::clone(&model), None, Vec::new(), None),
            initial,
        );
        let task = tokio::spawn(run_turn(request));
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::CommitAssistant { reply, .. } => {
                reply.send(Err(error)).unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
        let outcome = joined_outcome(task).await;
        assert_eq!(outcome.usage(), committed_usage);
        let diagnostic = outcome.diagnostic().unwrap();
        let (code, category) = match error {
            RunnerCommitError::Stale => (
                crate::error::DiagnosticCode::SessionBusy,
                crate::error::DiagnosticCategory::Internal,
            ),
            RunnerCommitError::Degraded => (
                crate::error::DiagnosticCode::SessionDegraded,
                crate::error::DiagnosticCategory::Storage,
            ),
            RunnerCommitError::DurabilityUnavailable => (
                crate::error::DiagnosticCode::LogConflict,
                crate::error::DiagnosticCategory::Storage,
            ),
            RunnerCommitError::DurabilityUnknown => (
                crate::error::DiagnosticCode::LogUnknownOutcome,
                crate::error::DiagnosticCategory::Storage,
            ),
            RunnerCommitError::RuntimeClosed => (
                crate::error::DiagnosticCode::RuntimeTerminated,
                crate::error::DiagnosticCategory::Internal,
            ),
        };
        assert_eq!((diagnostic.code, diagnostic.category), (code, category));
        assert_eq!(model.requests().len(), 1);
        assert!(critical_rx.try_recv().is_err());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_while_waiting_for_commit_ack_stops_without_next_action() {
    let committed_usage = Usage::new(5, 3, 2);
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            committed_usage,
        ))],
    );
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec,
        4,
        session_bindings(Arc::clone(&model), None, Vec::new(), None),
        initial,
    );
    let cancellation = request.cancellation.clone();
    let task = tokio::spawn(run_turn(request));
    let reply = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { reply, .. } => reply,
        event => panic!("unexpected event: {event:?}"),
    };
    cancellation.cancel();
    drop(reply);
    let outcome = joined_outcome(task).await;
    assert!(matches!(outcome, RunnerOutcome::Cancelled { usage } if usage == committed_usage));
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_commit_reply_maps_to_runtime_closed_failure() {
    let committed_usage = Usage::new(7, 4, 2);
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            committed_usage,
        ))],
    );
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec,
        4,
        session_bindings(model, None, Vec::new(), None),
        initial,
    );
    let task = tokio::spawn(run_turn(request));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { reply, .. } => drop(reply),
        event => panic!("unexpected event: {event:?}"),
    }
    let outcome = joined_outcome(task).await;
    assert_eq!(outcome.usage(), committed_usage);
    let diagnostic = outcome.diagnostic().unwrap();
    assert_eq!(
        diagnostic.code,
        crate::error::DiagnosticCode::RuntimeTerminated
    );
    assert_eq!(
        diagnostic.category,
        crate::error::DiagnosticCategory::Internal
    );
}

#[tokio::test(flavor = "current_thread")]
async fn closed_critical_channel_returns_a_joined_failure_without_orphaning_the_turn() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, critical_rx, _progress_rx) = runner_request(
        spec,
        4,
        session_bindings(model, None, Vec::new(), None),
        initial,
    );
    drop(critical_rx);
    let outcome = joined_outcome(tokio::spawn(run_turn(request))).await;
    assert!(matches!(
        outcome,
        RunnerOutcome::Failed { diagnostic, .. }
            if diagnostic.code == crate::error::DiagnosticCode::RuntimeTerminated
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn deadline_while_waiting_for_commit_ack_stops_without_continuation() {
    let committed_usage = Usage::new(11, 6, 3);
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            committed_usage,
        ))],
    );
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = request_with_control(
        spec,
        4,
        session_bindings(Arc::clone(&model), None, Vec::new(), None),
        initial,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    let reply = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { reply, .. } => reply,
        event => panic!("unexpected event: {event:?}"),
    };
    tokio::time::advance(Duration::from_secs(6)).await;
    drop(reply);
    let outcome = joined_outcome(task).await;
    assert!(matches!(outcome, RunnerOutcome::BudgetExceeded { usage } if usage == committed_usage));
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn expired_absolute_turn_deadline_is_budget_exceeded_before_model_execution() {
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, _critical_rx, _progress_rx) = request_with_control(
        spec,
        4,
        session_bindings(Arc::clone(&model), None, Vec::new(), None),
        initial,
        CancellationToken::new(),
        Instant::now(),
        4,
    );
    let task = tokio::spawn(run_turn(request));
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::BudgetExceeded { usage } if usage == Usage::default()
    ));
    assert!(model.requests().is_empty());
}
