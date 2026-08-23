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
        let outcome = match critical_rx.recv().await.unwrap() {
            RunnerEvent::Finish { outcome } => outcome,
            event => panic!("unexpected event: {event:?}"),
        };
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
        assert_finished(task.await.unwrap());
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
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled { usage }
        }) if usage == committed_usage
    ));
    drop(reply);
    assert_finished(task.await.unwrap());
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
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
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
    assert_finished(task.await.unwrap());
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
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::BudgetExceeded { usage }
        }) if usage == committed_usage
    ));
    drop(reply);
    assert_finished(task.await.unwrap());
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn expired_absolute_turn_deadline_is_budget_exceeded_before_model_execution() {
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = request_with_control(
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
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::BudgetExceeded { usage }
        }) if usage == Usage::default()
    ));
    assert_finished(task.await.unwrap());
    assert!(model.requests().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn full_critical_channel_is_cancellable_without_delayed_event_leak() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let cancellation = CancellationToken::new();
    let (request, mut critical_rx, mut progress_rx) = request_with_control(
        spec,
        4,
        session_bindings(model, None, Vec::new(), None),
        initial,
        cancellation.clone(),
        Instant::now() + Duration::from_secs(30),
        1,
    );
    request
        .critical_tx
        .try_send(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled {
                usage: Usage::default(),
            },
        })
        .unwrap();
    let task = tokio::spawn(run_turn(request));
    assert!(matches!(
        progress_rx.recv().await,
        Some(RunnerProgress::ModelStarted { model_round: 0 })
    ));
    cancellation.cancel();
    assert_eq!(
        task.await.unwrap(),
        TurnRunnerExit::ProtocolClosed {
            outcome: RunnerOutcome::Cancelled {
                usage: Usage::default(),
            },
        }
    );
    assert!(matches!(
        critical_rx.try_recv(),
        Ok(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled { .. }
        })
    ));
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn full_critical_channel_deadline_drops_pending_send_without_leak() {
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, mut progress_rx) = request_with_control(
        spec,
        4,
        session_bindings(model, None, Vec::new(), None),
        initial,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
        1,
    );
    request
        .critical_tx
        .try_send(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled {
                usage: Usage::default(),
            },
        })
        .unwrap();
    let task = tokio::spawn(run_turn(request));
    assert!(matches!(
        progress_rx.recv().await,
        Some(RunnerProgress::ModelStarted { model_round: 0 })
    ));
    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(
        task.await.unwrap(),
        TurnRunnerExit::ProtocolClosed {
            outcome: RunnerOutcome::BudgetExceeded {
                usage: Usage::default(),
            },
        }
    );
    assert!(critical_rx.try_recv().is_ok());
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}
