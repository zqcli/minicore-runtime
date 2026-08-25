use std::sync::atomic::AtomicUsize;

use super::super::support::UsageAccumulator;
use super::*;
use tokio::sync::Notify;

#[test]
fn usage_accumulation_is_checked_and_missing_fields_remain_unknown() {
    let mut usage = UsageAccumulator::default();
    assert_eq!(usage.current(), Usage::default());
    usage.add(Usage::new(1, 2, 3)).unwrap();
    assert_eq!(usage.current(), Usage::new(1, 2, 3));
    usage.add(Usage::new(4, 5, 6)).unwrap();
    assert_eq!(usage.finish(), Usage::new(5, 7, 9));

    let mut partial = UsageAccumulator::default();
    partial
        .add(Usage::from_optional(Some(1), None, Some(3)))
        .unwrap();
    partial.add(Usage::new(4, 5, 6)).unwrap();
    let partial = partial.finish();
    assert_eq!(partial.input_tokens(), Some(5));
    assert_eq!(partial.output_tokens(), None);
    assert_eq!(partial.reasoning_tokens(), Some(9));

    let mut overflow = UsageAccumulator::default();
    overflow
        .add(Usage::from_optional(Some(u64::MAX), Some(1), Some(1)))
        .unwrap();
    assert_eq!(overflow.add(Usage::new(1, 1, 1)), Err(()));
    assert_eq!(
        overflow.current(),
        Usage::from_optional(Some(u64::MAX), Some(1), Some(1))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn context_model_and_prompt_failures_map_to_bounded_finish_diagnostics() {
    let cases = ["context", "model", "prompt"];
    for case in cases {
        let model = ScriptModel::new(
            if case == "prompt" { 1 } else { 4_096 },
            if case == "model" {
                vec![ModelBehavior::Error(test_model_error(
                    ModelErrorKind::ContextOverflow,
                ))]
            } else {
                vec![ModelBehavior::Events(final_events(
                    "unused",
                    Usage::default(),
                ))]
            },
        );
        let context = if case == "context" {
            Some(ScriptContext::new(vec![Err(ContextError::Unavailable)]))
        } else {
            None
        };
        let spec = session_spec(&[], 4);
        let initial = initial_conversation(&spec, 4);
        let (request, _critical_rx, _progress_rx) = runner_request(
            spec,
            4,
            session_bindings(model, context, Vec::new(), None),
            initial,
        );
        let task = tokio::spawn(run_turn(request));
        let diagnostic = joined_outcome(task).await.diagnostic().unwrap().clone();
        assert_eq!(
            diagnostic.category,
            if case == "context" {
                crate::error::DiagnosticCategory::Context
            } else {
                crate::error::DiagnosticCategory::Compaction
            }
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn context_deadline_and_model_timeout_have_exact_failure_diagnostics() {
    for case in ["context_deadline", "model_timeout"] {
        let model = ScriptModel::new(
            4_096,
            if case == "model_timeout" {
                vec![
                    ModelBehavior::Error(retryable_not_started_model_error(
                        ModelErrorKind::Timeout,
                    )),
                    ModelBehavior::Error(retryable_not_started_model_error(
                        ModelErrorKind::Timeout,
                    )),
                    ModelBehavior::Error(retryable_not_started_model_error(
                        ModelErrorKind::Timeout,
                    )),
                ]
            } else {
                Vec::new()
            },
        );
        let context = if case == "context_deadline" {
            Some(ScriptContext::new(vec![Err(
                ContextError::DeadlineExceeded,
            )]))
        } else {
            None
        };
        let spec = session_spec(&[], 4);
        let initial = initial_conversation(&spec, 4);
        let (request, _critical_rx, _progress_rx) = runner_request(
            spec,
            4,
            session_bindings(model, context, Vec::new(), None),
            initial,
        );
        let task = tokio::spawn(run_turn(request));
        let outcome = joined_outcome(task).await;
        assert_eq!(outcome.usage(), Usage::default());
        let diagnostic = outcome.diagnostic().unwrap();
        assert_eq!(
            (diagnostic.code, diagnostic.category),
            if case == "context_deadline" {
                (
                    crate::error::DiagnosticCode::ContextFailed,
                    crate::error::DiagnosticCategory::Context,
                )
            } else {
                (
                    crate::error::DiagnosticCode::ModelTimeout,
                    crate::error::DiagnosticCategory::Model,
                )
            }
        );
    }
}

struct PendingContext {
    started: Arc<Notify>,
}

impl ContextProvider for PendingContext {
    fn provide<'a>(&'a self, _request: ContextRequest) -> ContextFuture<'a> {
        self.started.notify_waiters();
        Box::pin(std::future::pending())
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn absolute_turn_deadline_during_context_is_budget_exceeded() {
    let started = Arc::new(Notify::new());
    let context = Arc::new(PendingContext {
        started: Arc::clone(&started),
    });
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let mut bindings = session_bindings(model, None, Vec::new(), None);
    let context: Arc<dyn ContextProvider> = context;
    bindings.context = Some(context);
    let (request, _critical_rx, _progress_rx) = request_with_control(
        spec,
        4,
        bindings,
        initial,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
        4,
    );
    let notified = started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = tokio::spawn(run_turn(request));
    notified.await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        joined_outcome(task).await,
        RunnerOutcome::BudgetExceeded { usage } if usage == Usage::default()
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn model_and_context_failures_retain_usage_from_the_prior_round() {
    let prior_usage = Usage::new(13, 8, 5);
    for case in ["context", "model"] {
        let model = ScriptModel::new(
            4_096,
            if case == "model" {
                vec![
                    ModelBehavior::Events(tool_events(&[(31, "search")], prior_usage)),
                    ModelBehavior::Error(retryable_not_started_model_error(
                        ModelErrorKind::Timeout,
                    )),
                    ModelBehavior::Error(retryable_not_started_model_error(
                        ModelErrorKind::Timeout,
                    )),
                    ModelBehavior::Error(retryable_not_started_model_error(
                        ModelErrorKind::Timeout,
                    )),
                ]
            } else {
                vec![ModelBehavior::Events(tool_events(
                    &[(31, "search")],
                    prior_usage,
                ))]
            },
        );
        let context = if case == "context" {
            Some(ScriptContext::new(vec![
                Ok(ContextBundle { blocks: Vec::new() }),
                Err(ContextError::Unavailable),
            ]))
        } else {
            None
        };
        let tool = ScriptTool::new(
            "search",
            vec![ToolBehavior::Complete(ToolOutput::new("result").unwrap())],
        );
        let spec = session_spec(&["search"], 4);
        let initial = initial_conversation(&spec, 4);
        let (request, mut critical_rx, _progress_rx) = runner_request(
            spec.clone(),
            4,
            session_bindings(
                Arc::clone(&model),
                context,
                vec![tool],
                Some(ScriptPolicy::new(vec![ToolDecision::Allow])),
            ),
            initial.clone(),
        );
        let task = tokio::spawn(run_turn(request));
        let mut conversation = initial;
        loop {
            match critical_rx.recv().await {
                None => break,
                Some(RunnerEvent::CommitAssistant { draft, reply }) => {
                    let acknowledgement = ack_assistant(&conversation, &draft, &spec);
                    conversation = acknowledgement.conversation.clone();
                    reply.send(Ok(acknowledgement)).unwrap();
                }
                Some(RunnerEvent::CommitToolResult { draft, reply }) => {
                    let acknowledgement = ack_tool(&conversation, &draft, &spec);
                    conversation = acknowledgement.conversation.clone();
                    reply.send(Ok(acknowledgement)).unwrap();
                }
                Some(event) => panic!("unexpected event: {event:?}"),
            }
        }
        let outcome = joined_outcome(task).await;
        assert_eq!(outcome.usage(), prior_usage);
        let diagnostic = outcome.diagnostic().unwrap();
        assert_eq!(
            (diagnostic.code, diagnostic.category),
            if case == "context" {
                (
                    crate::error::DiagnosticCode::ContextFailed,
                    crate::error::DiagnosticCategory::Context,
                )
            } else {
                (
                    crate::error::DiagnosticCode::ModelTimeout,
                    crate::error::DiagnosticCategory::Model,
                )
            }
        );
        assert_eq!(
            model.requests().len(),
            if case == "context" { 1 } else { 4 }
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn usage_overflow_reports_internal_failure_with_the_prior_conservative_usage() {
    let prior_usage = Usage::from_optional(Some(u64::MAX), None, Some(3));
    let model = ScriptModel::new(
        4_096,
        vec![
            ModelBehavior::Events(tool_events(&[(32, "search")], prior_usage)),
            ModelBehavior::Events(final_events("overflow", Usage::new(1, 1, 1))),
        ],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("result").unwrap())],
    );
    let spec = session_spec(&["search"], 4);
    let initial = initial_conversation(&spec, 4);
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(
            model,
            None,
            vec![tool],
            Some(ScriptPolicy::new(vec![ToolDecision::Allow])),
        ),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));
    let mut conversation = initial;
    for expected in ["assistant", "tool"] {
        match critical_rx.recv().await.unwrap() {
            RunnerEvent::CommitAssistant { draft, reply } if expected == "assistant" => {
                let acknowledgement = ack_assistant(&conversation, &draft, &spec);
                conversation = acknowledgement.conversation.clone();
                reply.send(Ok(acknowledgement)).unwrap();
            }
            RunnerEvent::CommitToolResult { draft, reply } if expected == "tool" => {
                let acknowledgement = ack_tool(&conversation, &draft, &spec);
                conversation = acknowledgement.conversation.clone();
                reply.send(Ok(acknowledgement)).unwrap();
            }
            event => panic!("unexpected event: {event:?}"),
        }
    }
    let outcome = joined_outcome(task).await;
    assert_eq!(outcome.usage(), prior_usage);
    let diagnostic = outcome.diagnostic().unwrap();
    assert_eq!(diagnostic.code, crate::error::DiagnosticCode::Internal);
    assert_eq!(
        diagnostic.category,
        crate::error::DiagnosticCategory::Internal
    );
    assert!(critical_rx.try_recv().is_err());
}

struct UnusedCompaction {
    calls: AtomicUsize,
}

impl crate::compaction::CompactionStrategy for UnusedCompaction {
    fn compact<'a>(
        &'a self,
        _request: crate::compaction::CompactionRequest,
    ) -> crate::compaction::CompactionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(crate::compaction::CompactionError::Unavailable) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn enabled_compaction_without_boundary_fails_overflow_without_strategy_call() {
    let model = ScriptModel::new(
        1,
        vec![ModelBehavior::Events(final_events(
            "unused",
            Usage::default(),
        ))],
    );
    let mut spec = session_spec(&[], 4);
    spec.compaction = CompactionConfig::Enabled {
        trigger_tokens: 100,
        target_tokens: 50,
    };
    let initial = initial_conversation(&spec, 4);
    let compaction = Arc::new(UnusedCompaction {
        calls: AtomicUsize::new(0),
    });
    let mut bindings = session_bindings(model, None, Vec::new(), None);
    bindings.compaction = Some(compaction.clone());
    let (request, _critical_rx, _progress_rx) = runner_request(spec, 4, bindings, initial);
    let task = tokio::spawn(run_turn(request));
    let outcome = joined_outcome(task).await;
    assert!(matches!(
        outcome,
        RunnerOutcome::Failed { diagnostic, .. }
            if diagnostic.category == crate::error::DiagnosticCategory::Compaction
    ));
    assert_eq!(compaction.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn request_validation_rejects_invalid_effective_rounds() {
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let conversation = initial_conversation(&spec, 4);
    let kernel = KernelConfig::default_checked().unwrap();
    let bindings = session_bindings(model, None, Vec::new(), None);
    let environment = SessionEnvironment::build(&kernel, &spec, &bindings).unwrap();
    let (critical_tx, _critical_rx) = mpsc::channel(1);
    let (progress_tx, _progress_rx) = mpsc::channel(1);
    let control = TurnRunnerControl {
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
        critical_tx,
        progress_tx,
    };
    assert!(matches!(
        TurnRunnerRequest::new(
            TurnRunnerIdentity {
                session_id: session_id(),
                instance_id: instance_id(),
                turn_id: turn_id(),
            },
            environment,
            0,
            conversation,
            control,
        ),
        Err(TurnRunnerRequestError::Configuration)
    ));
}

#[test]
fn request_validation_rejects_a_different_canonical_active_turn() {
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let mut entries = initial_conversation(&spec, 4).entries().to_vec();
    let other_turn = "trn_00000000000000000000000000000082".parse().unwrap();
    if let ConversationEntry::UserMessage(entry) = &mut entries[0] {
        entry.turn_id = other_turn;
    }
    let conversation = ConversationView::from_confirmed(ConversationSeq::new(1), entries.into());
    let (critical_tx, _critical_rx) = mpsc::channel(1);
    let (progress_tx, _progress_rx) = mpsc::channel(1);
    let kernel = KernelConfig::default_checked().unwrap();
    let bindings = session_bindings(model, None, Vec::new(), None);
    let environment = SessionEnvironment::build(&kernel, &spec, &bindings).unwrap();
    assert!(matches!(
        TurnRunnerRequest::new(
            TurnRunnerIdentity {
                session_id: session_id(),
                instance_id: instance_id(),
                turn_id: turn_id(),
            },
            environment,
            4,
            conversation,
            TurnRunnerControl {
                cancellation: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(30),
                critical_tx,
                progress_tx,
            },
        ),
        Err(TurnRunnerRequestError::Conversation)
    ));
}
