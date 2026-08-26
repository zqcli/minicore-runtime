fn compact(source: &str) -> String {
    source.split_whitespace().collect()
}

#[test]
fn shared_port_call_is_private_bounded_and_excludes_core_protocols() {
    let root = include_str!("../src/lib.rs");
    let helper = include_str!("../src/port_call.rs");
    assert!(root.contains("mod port_call;"));
    assert!(!root.contains("pub mod port_call;"));
    assert!(!root.contains("pub use port_call"));
    for required in [
        "pub(crate) enum PortCallOutcome<T, E>",
        "Returned(Result<T, E>)",
        "Cancelled",
        "DeadlineExceeded(DeadlineSource)",
        "InvalidDeadline(DeadlineOverflow)",
        "Panicked",
        "pub(crate) async fn run_port_call<F, Fut, T, E>(",
        "effective_deadline(turn_deadline, port_timeout)",
        "parent_cancellation.is_cancelled()",
        "parent_cancellation.child_token()",
        "catch_unwind(AssertUnwindSafe(||",
        ".catch_unwind()",
        "biased;",
        "child_cancellation.cancel();",
    ] {
        assert!(helper.contains(required), "port helper misses {required}");
    }
    let production = helper
        .split_once("#[cfg(test)]\nmod tests")
        .map(|(production, _)| production)
        .unwrap();
    for forbidden in [
        "ModelDriver",
        "SessionLog",
        "ConversationLog",
        "Interaction",
        "RunnerEvent",
        "retry",
        "durability",
        "ServiceLocator",
        "Hook",
        "tokio::spawn",
    ] {
        assert!(
            !production.contains(forbidden),
            "port helper contains {forbidden}"
        );
    }
    assert!(production.lines().count() < 100);
    assert_eq!(
        include_str!("../src/context/driver.rs")
            .matches("run_port_call(")
            .count(),
        1
    );
    assert_eq!(
        include_str!("../src/compaction/driver.rs")
            .matches("run_port_call(")
            .count(),
        1
    );
    assert_eq!(
        include_str!("../src/agent/tool_driver.rs")
            .matches("run_port_call(")
            .count(),
        2
    );
    for source in [
        include_str!("../src/model/driver.rs"),
        include_str!("../src/conversation/log.rs"),
        include_str!("../src/conversation/session_log.rs"),
        include_str!("../src/agent/runner.rs"),
    ] {
        assert!(!source.contains("run_port_call"));
    }
}

#[test]
fn final_turn_runner_is_private_no_spawn_and_owner_neutral() {
    let module = include_str!("../src/agent/mod.rs");
    for required in [
        "mod runner;",
        "mod environment;",
        "mod runner_protocol;",
        "mod tool_driver;",
        "mod turn_context;",
        "pub(crate) use runner::run_turn;",
        "TurnRunnerRequest",
        "RunnerEvent",
        "RunnerProgress",
    ] {
        assert!(module.contains(required), "agent module misses {required}");
    }

    let runner = include_str!("../src/agent/runner.rs");
    let compaction = include_str!("../src/agent/runner/compaction.rs");
    let support = include_str!("../src/agent/runner/support.rs");
    let diagnostics = include_str!("../src/agent/runner/diagnostics.rs");
    let environment = include_str!("../src/agent/environment.rs");
    let context = include_str!("../src/agent/turn_context.rs");
    let protocol = include_str!("../src/agent/runner_protocol.rs");
    let production = format!(
        "{runner}\n{compaction}\n{support}\n{diagnostics}\n{environment}\n{context}\n{protocol}"
    );
    let compact_production = compact(&production);
    for required in [
        "pub(crate) async fn run_turn(",
        "async fn run_ordinary_loop(",
        "RunnerEvent::CommitAssistant",
        "RunnerEvent::CommitToolResult",
        "RunnerEvent::CommitSummary",
        "RunnerEvent::Suspend { suspension }",
        "UsageAccumulator",
    ] {
        assert!(
            production.contains(required),
            "turn runner misses {required}"
        );
    }
    for required in [
        "ContextRequest{",
        "context.environment.prompt.plan(",
        "plan.finish(&validated_context)",
        ".provide_detailed(ContextRequest{",
        "letvalidated_context=matchcontext.environment.context.provide_detailed(ContextRequest{",
        "letrun=context.environment.model.run_detailed(",
        "letrun=context.environment.tools.run(",
        "prepare_model_request(",
        ".run_detailed(",
        "context.conversation=conversation;",
        "ToolInvocation::new(",
        "try_send(event)",
        "control.sender.send(event)",
        "AssertUnwindSafe(task).catch_unwind()",
    ] {
        assert!(
            compact_production.contains(required),
            "turn runner misses compact pattern {required}"
        );
    }
    assert!(compact_production.contains("Ok(outcome)=>TurnRunnerExit::Finished{outcome}"));
    assert!(compact_production.contains("Err(_)=>TurnRunnerExit::Panicked"));
    assert!(!production.contains("RunnerEvent::Finish"));
    assert!(!production.contains("FinishControl"));
    assert!(!production.contains("finish_outcome"));
    assert!(!production.contains("ProtocolClosed"));
    for forbidden in [
        "SessionLog",
        "ConversationLog",
        ".append(",
        "SessionRuntime",
        "SessionHandle",
        "Handle",
        "Workspace",
        "Store",
        "tokio::spawn",
        "spawn_blocking",
        "serde",
        "Legacy",
        "legacy_",
        "#[allow",
        "#[expect",
    ] {
        assert!(
            !production.contains(forbidden),
            "turn runner contains {forbidden}"
        );
    }
    assert!(production.contains("CompactionDriver"));
    for source in [
        runner,
        compaction,
        support,
        diagnostics,
        environment,
        context,
        protocol,
    ] {
        assert!(source.lines().count() < 500);
    }
    let immediate = support.find("control.sender.try_send(event)").unwrap();
    let blocked = support.find("control.sender.send(event)").unwrap();
    assert!(immediate < blocked);
    let compact_support = compact(support);
    for required in [
        "previous_head != context.conversation.head()",
        "update.previous_head != previous_head",
        "previous_head.next() != Some(update.entry.seq())",
        "update.conversation.head() != update.entry.seq()",
        "update.conversation.entries().last() != Some(&update.entry)",
        "before.turn_id != after.turn_id || before.execution != after.execution",
    ] {
        assert!(
            support.contains(required),
            "ack validation misses {required}"
        );
    }
    assert!(compact_support.contains(
        "update.conversation.is_validated_for(&context.environment.spec,&context.environment.limits)"
    ));
    let provenance = compact_support
        .find("if!update.conversation.is_validated_for(")
        .unwrap();
    let shape = compact_support
        .find("ifprevious_head!=context.conversation.head()")
        .unwrap();
    assert!(provenance < shape);
    for forbidden in [
        "current_entries",
        "replacement_entries",
        "get(..current_entries.len())",
        ".zip(",
    ] {
        assert!(
            !support.contains(forbidden),
            "ack validation retains {forbidden}"
        );
    }
    let run_tool = &runner[runner.find("async fn run_tool(").unwrap()..];
    let result = run_tool.find("result = &mut run =>").unwrap();
    let tool_progress = run_tool[result..]
        .find("value = progress_rx.recv()")
        .unwrap();
    let suspension = run_tool[result..]
        .find("value = suspension_rx.recv()")
        .unwrap();
    assert!(tool_progress < suspension);
    assert!(runner.contains("while let Ok(value) = progress_rx.try_recv()"));
    assert!(!diagnostics.contains("deadline_failure"));
    assert!(diagnostics.contains("ContextError::DeadlineExceeded => failed("));
    assert!(diagnostics.contains("DiagnosticCode::ContextFailed"));
    assert!(diagnostics.contains("DiagnosticCategory::Context"));
    assert!(diagnostics.contains("RunnerOutcome::BudgetExceeded { usage }"));
    assert!(diagnostics.contains("failure.deadline_source() == Some(DeadlineSource::Turn)"));
    assert!(!runner.contains("Err(crate::context::ContextError::DeadlineExceeded)"));
}

#[test]
fn runner_protocol_has_exact_critical_ack_outcome_and_progress_roles() {
    let protocol = include_str!("../src/agent/runner_protocol.rs");
    let compact_protocol = compact(protocol);
    let conversation = include_str!("../src/conversation/mod.rs");
    assert!(conversation.contains("AssistantMessageDraft"));
    assert!(conversation.contains("ToolResultDraft"));
    for required in [
        "pub(crate)enumRunnerEvent{",
        concat!(
            "CommitAssistant{draft:AssistantMessageDraft,",
            "reply:oneshot::Sender<Result<CommittedUpdate,RunnerCommitError>>,}"
        ),
        concat!(
            "CommitToolResult{draft:ToolResultDraft,",
            "reply:oneshot::Sender<Result<CommittedUpdate,RunnerCommitError>>,}"
        ),
        concat!(
            "CommitSummary{snapshot_head:ConversationSeq,draft:SummaryDraft,",
            "reply:oneshot::Sender<Result<CommittedUpdate,RunnerCommitError>>,}"
        ),
        "Suspend{suspension:TurnSuspension,}",
        concat!(
            "pub(crate)structCommittedUpdate{",
            "pub(crate)previous_head:ConversationSeq,",
            "pub(crate)entry:ConversationEntry,",
            "pub(crate)conversation:ConversationView,}"
        ),
        "pub(crate)enumRunnerCommitError{",
        "Stale,",
        "Degraded,",
        "DurabilityUnavailable,",
        "DurabilityUnknown,",
        "RuntimeClosed,",
        "pub(crate)enumRunnerOutcome{",
        "Completed{usage:Usage,}",
        "Failed{diagnostic:DiagnosticSummary,usage:Usage,}",
        "Cancelled{usage:Usage,}",
        "BudgetExceeded{usage:Usage,}",
        "pub(crate)constfnusage(&self)->Usage",
        "pub(crate)constfndiagnostic(&self)->Option<&DiagnosticSummary>",
        "pub(crate)enumRunnerProgress{",
        "ModelStarted{model_round:u16,}",
        "ModelProgress{model_round:u16,progress:ModelDriverProgress,}",
        "ModelFinished{model_round:u16,usage:Usage,}",
        "ToolStarted{tool_call_id:ToolCallId,tool_name:ToolName,}",
        "ToolProgress{tool_call_id:ToolCallId,progress:ToolProgressValue,}",
        concat!(
            "ToolFinished{tool_call_id:ToolCallId,tool_name:ToolName,",
            "outcome:ToolResultOutcome,content_bytes:usize,}"
        ),
        "pub(crate)enumTurnRunnerExit{",
        "Finished{outcome:RunnerOutcome}",
        "Panicked,",
    ] {
        assert!(
            compact_protocol.contains(required),
            "runner protocol misses compact shape {required}"
        );
    }
    assert!(!protocol.contains("take_resume_for_actor"));
    assert!(!protocol.contains("take_commit_reply_for_actor"));
    assert!(!protocol.contains("RunnerEvent::Finish"));
    assert!(!protocol.contains("ProtocolClosed"));
    let actor = include_str!("../src/session/actor/runner.rs");
    assert!(actor.contains("let TurnSuspension {"));
    assert!(actor.contains("RunnerEvent::CommitSummary"));
    assert!(!actor.contains("RunnerEvent::Finish"));
    assert!(actor.contains("active.forced_outcome.take().or(joined)"));
    assert!(actor.contains("TurnRunnerExit::Finished { outcome }"));
    assert!(!protocol.contains("serde"));
    assert!(!protocol.contains("reply: Box"));
}

#[test]
fn summary_commit_is_stale_head_checked_redacted_and_actor_owned() {
    let protocol = include_str!("../src/agent/runner_protocol.rs");
    let compact_protocol = compact(protocol);
    let runner_compaction = include_str!("../src/agent/runner/compaction.rs");
    let support = include_str!("../src/agent/runner/support.rs");
    let actor_evidence = include_str!("../src/session/actor/runner.rs");
    for required in [
        "let snapshot_head = proposal.snapshot_head();",
        "context.conversation.head() != snapshot_head",
        "commit_summary(context, snapshot_head, draft.clone())",
        "validate_summary_ack(",
        "critical_failure(error, usage)",
        "pub(super) fn turn_control_outcome(",
        "context.cancellation.is_cancelled()",
        "TokioInstant::now() >= TokioInstant::from_std(context.deadline)",
    ] {
        assert!(
            runner_compaction.contains(required),
            "summary orchestration misses {required}"
        );
    }
    assert!(!runner_compaction.contains("fn commit_failure("));
    assert!(
        runner_compaction
            .matches("turn_control_outcome(context, usage)")
            .count()
            >= 10
    );
    let summary_variant = concat!(
        "CommitSummary{snapshot_head:ConversationSeq,draft:SummaryDraft,",
        "reply:oneshot::Sender<Result<CommittedUpdate,RunnerCommitError>>,}"
    );
    assert!(
        compact_protocol.contains(summary_variant),
        "summary protocol misses compact shape {summary_variant}"
    );
    for required in [
        ".field(\"through\", &draft.through)",
        ".field(\"summary_bytes\", &draft.summary.byte_len())",
    ] {
        assert!(
            protocol.contains(required),
            "summary protocol misses {required}"
        );
    }
    let debug = &protocol[protocol.find("Self::CommitSummary").unwrap()..];
    let debug = &debug[..debug.find("Self::Suspend").unwrap()];
    assert!(!debug.contains(".field(\"draft\""));
    assert!(!debug.contains("draft.summary.as_str()"));
    for required in [
        "before.turn_id != after.turn_id || before.execution != after.execution",
        "ConversationEntry::Summary(entry)",
        "entry.through == draft.through",
        "entry.summary == draft.summary",
    ] {
        assert!(support.contains(required), "summary ack misses {required}");
    }
    let stale = actor_evidence
        .find("if self.conversation.head() != snapshot_head")
        .unwrap();
    let append = actor_evidence
        .find("self.commit_one(UnsequencedEntry::Summary(draft))")
        .unwrap();
    assert!(stale < append);
    assert!(actor_evidence.contains("return Err(RunnerCommitError::Stale)"));
}

#[test]
fn request_context_reuses_the_frozen_environment_without_owner_authority() {
    let runner = include_str!("../src/agent/runner.rs");
    let context = include_str!("../src/agent/turn_context.rs");
    let environment = include_str!("../src/agent/environment.rs");
    let compact_context = compact(context);
    for required in [
        "pub(crate) struct TurnRunnerRequest",
        "pub(crate) session_id: SessionId",
        "pub(crate) instance_id: SessionInstanceId",
        "pub(crate) turn_id: TurnId",
        "pub(super) environment: Arc<SessionEnvironment>",
        "pub(super) fn from_request(request: TurnRunnerRequest) -> Self",
        "pub(crate) effective_max_tool_rounds: u16",
        "pub(crate) conversation: ConversationView",
        "pub(crate) cancellation: CancellationToken",
        "pub(crate) deadline: Instant",
        "pub(crate) critical_tx: mpsc::Sender<RunnerEvent>",
        "pub(crate) progress_tx: mpsc::Sender<RunnerProgress>",
    ] {
        assert!(context.contains(required), "turn context misses {required}");
    }
    for required in [
        "environment.spec.max_tool_rounds",
        "environment.limits.max_tool_rounds",
        ".validated_active_turn(&environment.spec,&environment.limits)",
        "active.turn_id!=Some(identity.turn_id)",
        concat!(
            "active.execution.as_ref().is_none_or(|execution|",
            "execution.max_tool_rounds!=effective_max_tool_rounds)"
        ),
    ] {
        assert!(
            compact_context.contains(required),
            "turn context misses compact pattern {required}"
        );
    }
    for forbidden in [
        "TurnRunnerKernel",
        "SessionBindings",
        "PromptBuilder::new(",
        "ContextDriver::new(",
        "CompactionDriver::new(",
        "ModelDriver::new(",
        "ToolDriver::new(",
        "descriptor()",
    ] {
        assert!(
            !context.contains(forbidden),
            "turn context retains {forbidden}"
        );
    }
    let compact_environment = compact(environment);
    for required in [
        "bindings.freeze(spec,&kernel.limits)",
        "PromptBuilder::new(",
        "ContextDriver::new(",
        "CompactionDriver::new(",
        "ModelDriver::from_validated(",
        "ToolDriver::from_enabled(",
        "SessionChannelCapacities",
        "pub(crate)fnsession_inputs(",
    ] {
        assert!(
            compact_environment.contains(required),
            "environment misses {required}"
        );
    }
    assert!(!environment.contains("pub(super) kernel"));
    assert!(!environment.contains("kernel: KernelConfig"));
    assert!(!context.contains("panic_after_context_creation"));
    assert!(
        runner.contains("#[cfg(test)]\n    if tests::take_scripted_turn_panic(context.turn_id)")
    );
}

#[test]
fn detailed_deadline_provenance_is_wired_without_clock_inference() {
    let runner = include_str!("../src/agent/runner.rs");
    let runner_compaction = include_str!("../src/agent/runner/compaction.rs");
    let diagnostics = include_str!("../src/agent/runner/diagnostics.rs");
    let context = include_str!("../src/context/driver.rs");
    let model = include_str!("../src/model/driver.rs");
    let compaction = include_str!("../src/compaction/driver.rs");
    let combined =
        format!("{runner}\n{runner_compaction}\n{diagnostics}\n{context}\n{model}\n{compaction}");
    for required in [
        ".provide_detailed(ContextRequest {",
        "context.environment.model.run_detailed(",
        "ContextDriverFailure",
        "ModelDriverFailure",
        "CompactionDriverFailure",
        "Some(DeadlineSource::Turn)",
    ] {
        assert!(
            combined.contains(required),
            "deadline wiring misses {required}"
        );
    }
    assert!(!runner.contains("now() >= context.deadline"));
    assert!(!runner.contains("DeadlineExceeded) if TokioInstant::now()"));
}

#[test]
fn reviewer_regressions_have_deterministic_private_evidence() {
    let acknowledgements = include_str!("../src/agent/runner/tests/acknowledgements.rs");
    let deadlines = include_str!("../src/agent/runner/tests/deadline_provenance.rs");
    let request = include_str!("../src/agent/runner/tests/request_validation.rs");
    let panic = include_str!("../src/agent/runner/tests/panic.rs");
    let panic_support = include_str!("../src/agent/runner/tests/panic_support.rs");
    let interactions = include_str!("../src/agent/runner/tests/interactions.rs");
    let usage = include_str!("../src/agent/runner/tests/usage_errors.rs");
    let compaction = include_str!("../src/agent/runner/tests/compaction.rs");
    let compaction_usage = include_str!("../src/agent/runner/tests/compaction/usage.rs");
    let compaction_ack = include_str!("../src/agent/runner/tests/compaction_acknowledgements.rs");
    let compaction_control = include_str!("../src/agent/runner/tests/compaction_control.rs");
    let control = include_str!("../src/agent/runner/tests/control.rs");
    let actor_scheduling = include_str!("../src/session/actor/tests/scheduling.rs");
    let compaction_priority =
        include_str!("../src/agent/runner/tests/compaction_control/priority.rs");
    for required in [
        "assistant_ack_rejects_semantically_valid_untrusted_early_prefix_replacement",
        "tool_result_ack_rejects_each_draft_field_mismatch",
        "committed_update_acknowledges_a_ten_thousand_entry_history",
        "sequence_mismatch",
    ] {
        assert!(acknowledgements.contains(required));
    }
    assert!(
        control.contains(
            "closed_critical_channel_returns_a_joined_failure_without_orphaning_the_turn"
        )
    );
    assert!(actor_scheduling.contains("panicked_runner_join_persists_a_durable_internal_terminal"));
    for required in [
        "request_rejects_supplied_rounds_above_the_durable_active_turn_value",
        "request_rejects_supplied_rounds_below_the_durable_active_turn_value",
        "request_accepts_the_exact_durable_lower_round_value",
    ] {
        assert!(request.contains(required));
    }
    for required in [
        "panic_returns_panicked_without_a_finish_event",
        "panic_with_closed_critical_channel_returns_panicked_without_retry",
    ] {
        assert!(panic.contains(required));
    }
    assert!(panic_support.contains("Mutex<HashSet<TurnId>>"));
    assert!(panic_support.contains("AtomicU64::new(900)"));
    assert!(panic_support.contains(".remove(&turn_id)"));
    assert!(!panic_support.contains("static mut"));
    assert!(interactions.contains(
        "tool_input_orders_started_before_suspend_and_finished_after_commit_without_reexecution"
    ));
    assert!(interactions.contains("ToolStarted { tool_call_id, tool_name }"));
    assert!(interactions.contains("ToolFinished { tool_call_id, .. }"));
    assert!(usage.contains("model_and_context_failures_retain_usage_from_the_prior_round"));
    assert!(
        usage.contains("usage_overflow_reports_internal_failure_with_the_prior_conservative_usage")
    );
    for required in [
        "context_turn_deadline_is_budget_exceeded",
        "configured_model_timeout_keeps_model_timeout_diagnostic",
        "policy_turn_deadline_has_no_tool_result_commit",
        "configured_policy_timeout_commits_denied_result_and_continues",
        "tool_turn_deadline_has_no_tool_result_commit",
        "configured_tool_timeout_commits_failed_result_and_continues",
    ] {
        assert!(deadlines.contains(required));
    }
    for required in [
        "proactive_compaction_commits_before_context_and_model_use_the_new_view",
        "forced_final_build_overflow_commits_then_retries_the_same_model_round",
        "forced_retry_overflow_fails_without_a_second_strategy_call_or_loop",
        "proactive_trigger_equality_and_same_head_suppression_are_exact",
    ] {
        assert!(compaction.contains(required));
    }
    for required in [
        "stale_summary_snapshot_is_rejected_before_actor_append",
        "summary_ack_must_end_with_the_exact_committed_summary",
        "every_summary_commit_error_uses_the_existing_critical_taxonomy",
    ] {
        assert!(compaction_ack.contains(required));
    }
    for required in [
        "forced_compaction_turn_deadline_is_budget_exceeded",
        "forced_compaction_configured_timeout_is_a_compaction_failure",
        "full_summary_send_is_cancellable_without_a_delayed_commit",
        "deadline_while_waiting_for_summary_ack_has_no_model_continuation",
        "proactive_configured_timeout_is_skipped_and_model_continues",
        "proactive_parent_cancellation_is_terminal_without_model_continuation",
        "proactive_turn_deadline_is_terminal_without_model_continuation",
    ] {
        assert!(compaction_control.contains(required));
    }
    for required in [
        "provider_cancellation_after_success_does_not_cancel_the_parent_turn",
        "expired_turn_after_context_success_wins_without_strategy_or_boundary",
    ] {
        assert!(compaction_priority.contains(required));
    }
    for required in [
        "proactive_commit_failure_after_model_usage_preserves_usage",
        "forced_failure_after_model_usage_preserves_usage",
    ] {
        assert!(compaction_usage.contains(required));
    }
    for source in [acknowledgements, deadlines, request, panic, panic_support] {
        assert!(source.lines().count() < 500);
    }
}

#[test]
fn tool_driver_started_progress_is_execution_only_and_lifecycle_typed() {
    let driver = include_str!("../src/agent/tool_driver.rs");
    let support = include_str!("../src/agent/tool_driver/support.rs");
    assert!(driver.contains("pub(crate) enum ToolDriverProgress"));
    assert!(driver.contains("Started {"));
    assert!(driver.contains("tool_name: ToolName"));
    assert!(driver.contains("Update {"));
    let start = driver.find("ToolDriverProgress::Started").unwrap();
    let execute = driver.find("tool.execute(invocation, context)").unwrap();
    assert!(start < execute);
    assert_eq!(driver.matches("ToolDriverProgress::Started").count(), 1);
    assert!(support.contains("ToolDriverProgress::Update"));
    assert!(driver.lines().count() < 500);
}
