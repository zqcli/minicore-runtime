use super::*;
use tokio::sync::Notify;

struct SignallingApproval {
    notify: Arc<Notify>,
}

impl ToolPolicy for SignallingApproval {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        let notify = Arc::clone(&self.notify);
        Box::pin(async move {
            notify.notify_waiters();
            Ok(approval())
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn approval_suspension_forwards_exact_resume_sender_through_runner() {
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(10, "search")], Usage::default())),
            ModelBehavior::Events(final_events("done", Usage::default())),
        ],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("approved").unwrap())],
    );
    let policy = ScriptPolicy::new(vec![approval()]);
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(model, None, vec![Arc::clone(&tool)], Some(policy)),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));
    let mut conversation = initial;

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let acknowledgement = ack_assistant(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::Suspend { suspension } => {
            assert_eq!(suspension.turn_id, turn_id());
            assert_eq!(suspension.tool_call_id, call_id(10));
            assert_eq!(suspension.tool_name.as_str(), "search");
            assert!(matches!(
                suspension.kind,
                crate::session::InteractionKind::Approval(_)
            ));
            suspension
                .resume
                .send(Ok(InteractionAnswer::Approval(ApprovalDecision::AllowOnce)))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitToolResult { draft, reply } => {
            assert_eq!(draft.content.as_str(), "approved");
            assert_eq!(draft.outcome, ToolResultOutcome::Success);
            let acknowledgement = ack_tool(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let acknowledgement = ack_assistant(&conversation, &draft, &spec);
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::Completed { .. }
        })
    ));
    assert_finished(task.await.unwrap());
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn tool_input_orders_started_before_suspend_and_finished_after_commit_without_reexecution() {
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(11, "search")], Usage::default())),
            ModelBehavior::Events(final_events("done", Usage::default())),
        ],
    );
    let tool = ScriptTool::new("search", vec![ToolBehavior::Input(input_request())]);
    let policy = ScriptPolicy::new(vec![ToolDecision::Allow]);
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, mut progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(model, None, vec![Arc::clone(&tool)], Some(policy)),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));
    let mut conversation = initial;

    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let acknowledgement = ack_assistant(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
            while progress_rx.try_recv().is_ok() {}
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::Suspend { suspension } => {
            let before_suspend =
                std::iter::from_fn(|| progress_rx.try_recv().ok()).collect::<Vec<_>>();
            assert!(before_suspend.iter().any(|event| matches!(
                event,
                RunnerProgress::ToolStarted { tool_call_id, tool_name }
                    if tool_call_id == &call_id(11) && tool_name.as_str() == "search"
            )));
            assert!(matches!(
                suspension.kind,
                crate::session::InteractionKind::ToolInput(_)
            ));
            suspension
                .resume
                .send(Ok(InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                    BoundedText::new("value").unwrap(),
                ))))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitToolResult { draft, reply } => {
            assert_eq!(draft.outcome, ToolResultOutcome::InputProvided);
            assert_eq!(draft.content.as_str(), r#"{"answer":"value"}"#);
            assert!(
                !std::iter::from_fn(|| progress_rx.try_recv().ok()).any(|event| matches!(
                    event,
                    RunnerProgress::ToolFinished { tool_call_id, .. }
                        if tool_call_id == call_id(11)
                ))
            );
            let acknowledgement = ack_tool(&conversation, &draft, &spec);
            conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        progress_rx.recv().await,
        Some(RunnerProgress::ToolFinished { tool_call_id, .. })
            if tool_call_id == call_id(11)
    ));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            reply
                .send(Ok(ack_assistant(&conversation, &draft, &spec)))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::Completed { .. }
        })
    ));
    assert_finished(task.await.unwrap());
    assert_eq!(tool.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn typed_suspension_failures_map_without_fabricating_tool_results() {
    for error in [
        SuspensionError::StaleTurn,
        SuspensionError::InvalidState,
        SuspensionError::RuntimeClosed,
    ] {
        let model = ScriptModel::new(
            4_096,
            vec![ModelBehavior::Events(tool_events(
                &[(12, "search")],
                Usage::default(),
            ))],
        );
        let tool = ScriptTool::new(
            "search",
            vec![ToolBehavior::Complete(ToolOutput::new("x").unwrap())],
        );
        let policy = ScriptPolicy::new(vec![approval()]);
        let spec = session_spec(&["search"], 4);
        let initial = initial_conversation(&spec, 4);
        let (request, mut critical_rx, _progress_rx) = runner_request(
            spec.clone(),
            4,
            session_bindings(model, None, vec![tool], Some(policy)),
            initial.clone(),
        );
        let task = tokio::spawn(run_turn(request));
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::CommitAssistant { draft, reply } => {
                reply
                    .send(Ok(ack_assistant(&initial, &draft, &spec)))
                    .unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::Suspend { suspension } => {
                suspension.resume.send(Err(error)).unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
        assert!(matches!(
            critical_rx.recv().await,
            Some(RunnerEvent::Finish {
                outcome: RunnerOutcome::Failed { .. }
            })
        ));
        assert_finished(task.await.unwrap());
        assert!(critical_rx.try_recv().is_err());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn suspension_cancellation_and_deadline_retain_usage_with_exact_outcomes() {
    let prior_usage = Usage::new(17, 9, 4);
    for error in [
        SuspensionError::Cancelled,
        SuspensionError::DeadlineExceeded,
    ] {
        let model = ScriptModel::new(
            4_096,
            vec![ModelBehavior::Events(tool_events(
                &[(15, "search")],
                prior_usage,
            ))],
        );
        let tool = ScriptTool::new(
            "search",
            vec![ToolBehavior::Complete(ToolOutput::new("unused").unwrap())],
        );
        let policy = ScriptPolicy::new(vec![approval()]);
        let spec = session_spec(&["search"], 4);
        let initial = initial_conversation(&spec, 4);
        let (request, mut critical_rx, _progress_rx) = runner_request(
            spec.clone(),
            4,
            session_bindings(model, None, vec![tool], Some(policy)),
            initial.clone(),
        );
        let task = tokio::spawn(run_turn(request));
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::CommitAssistant { draft, reply } => {
                reply
                    .send(Ok(ack_assistant(&initial, &draft, &spec)))
                    .unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::Suspend { suspension } => {
                suspension.resume.send(Err(error)).unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
        let outcome = match critical_rx.recv().await.unwrap() {
            RunnerEvent::Finish { outcome } => outcome,
            event => panic!("unexpected event: {event:?}"),
        };
        assert_eq!(outcome.usage(), prior_usage);
        assert!(match error {
            SuspensionError::Cancelled => matches!(outcome, RunnerOutcome::Cancelled { .. }),
            SuspensionError::DeadlineExceeded => {
                matches!(outcome, RunnerOutcome::BudgetExceeded { .. })
            }
            _ => false,
        });
        assert_finished(task.await.unwrap());
        assert!(critical_rx.try_recv().is_err());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_suspension_forward_is_cancellable_without_delayed_enqueue() {
    let notify = Arc::new(Notify::new());
    let policy = Arc::new(SignallingApproval {
        notify: Arc::clone(&notify),
    });
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(tool_events(
            &[(13, "search")],
            Usage::default(),
        ))],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("unused").unwrap())],
    );
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let cancellation = CancellationToken::new();
    let (request, mut critical_rx, _progress_rx) = request_with_control(
        spec.clone(),
        4,
        session_bindings(model, None, vec![tool], Some(policy)),
        initial.clone(),
        cancellation.clone(),
        Instant::now() + Duration::from_secs(30),
        1,
    );
    let critical_tx = request.critical_tx.clone();
    let task = tokio::spawn(run_turn(request));
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            critical_tx
                .try_send(RunnerEvent::Finish {
                    outcome: RunnerOutcome::Cancelled {
                        usage: Usage::default(),
                    },
                })
                .unwrap();
            reply
                .send(Ok(ack_assistant(&initial, &draft, &spec)))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    notified.await;
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
        Ok(RunnerEvent::Finish { .. })
    ));
    drop(critical_tx);
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn blocked_suspension_forward_deadline_has_no_delayed_enqueue() {
    let notify = Arc::new(Notify::new());
    let policy = Arc::new(SignallingApproval {
        notify: Arc::clone(&notify),
    });
    let model = ScriptModel::new(
        4_096,
        vec![ModelBehavior::Events(tool_events(
            &[(14, "search")],
            Usage::default(),
        ))],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("unused").unwrap())],
    );
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = request_with_control(
        spec.clone(),
        4,
        session_bindings(model, None, vec![tool], Some(policy)),
        initial.clone(),
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
        1,
    );
    let critical_tx = request.critical_tx.clone();
    let task = tokio::spawn(run_turn(request));
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            critical_tx
                .try_send(RunnerEvent::Finish {
                    outcome: RunnerOutcome::Cancelled {
                        usage: Usage::default(),
                    },
                })
                .unwrap();
            reply
                .send(Ok(ack_assistant(&initial, &draft, &spec)))
                .unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    notified.await;
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
    drop(critical_tx);
    assert!(matches!(
        critical_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}
