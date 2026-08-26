#[test]
fn compaction_driver_is_private_single_strategy_and_owner_neutral() {
    let module = include_str!("../src/compaction/mod.rs");
    let compact_module: String = module.split_whitespace().collect();
    assert!(module.contains("mod driver;"));
    assert!(compact_module.contains("pub(crate)usedriver::{CompactionDriver"));
    assert!(!module.contains("pub use driver"));

    let driver = include_str!("../src/compaction/driver.rs");
    for required in [
        "pub(crate) struct CompactionDriver",
        "strategy: Option<Arc<dyn CompactionStrategy>>",
        "pub(crate) async fn run(",
        "pub(crate) async fn run_detailed(",
        "pub(crate) struct CompactionDriverFailure",
        "deadline_source: Option<DeadlineSource>",
        "CompactionDriverFailure::deadline(source)",
        "run_port_call(",
        "PortCallOutcome::Returned(Ok(Some(proposal)))",
        "PortCallOutcome::Returned(Ok(None))",
        "PortCallOutcome::Cancelled",
        "PortCallOutcome::DeadlineExceeded(source)",
        "PortCallOutcome::InvalidDeadline(_)",
        "PortCallOutcome::Panicked",
        "validate_candidate(&candidate_for_call).map_err(CompactionDriverFailure::ordinary)?",
        "fn validate_candidate(candidate: &CompactionCandidate) -> Result<bool, CompactionError>",
        "if candidate.entries().is_empty()",
        "candidate.head() == ConversationSeq::ZERO",
        "let expected = previous_entry",
        ".next()",
        "if entry_seq != expected",
        "for entry in candidate.entries()",
        "boundaries.get(boundary_index).copied()",
        "previous_boundary.is_some_and(|previous| previous >= boundary)",
        "previous_entry != candidate.head()",
        "boundary_index != boundaries.len()",
        "completed_boundaries()",
        "binary_search(&proposal.through_seq)",
        "binary_search_by_key(&proposal.through_seq, ConversationEntry::seq)",
        "matches!(entry, ConversationEntry::TurnTerminal(_))",
        "ValidatedCompactionProposal",
        "snapshot_head: candidate.head()",
    ] {
        assert!(
            driver.contains(required),
            "compaction driver misses {required}"
        );
    }
    assert_eq!(
        driver.matches("for entry in candidate.entries()").count(),
        1
    );
    let detailed_start = driver.find("pub(crate) async fn run_detailed(").unwrap();
    let candidate_start = driver.find("fn validate_candidate(").unwrap();
    let detailed = &driver[detailed_start..candidate_start];
    let call = detailed.find("run_port_call(").unwrap();
    let target = detailed.find("if target_tokens == 0").unwrap();
    let candidate = detailed
        .find("validate_candidate(&candidate_for_call).map_err(CompactionDriverFailure::ordinary)?")
        .unwrap();
    let pause = detailed
        .find("tests::pause_after_candidate(turn_id).await")
        .unwrap();
    assert!(target < candidate);
    assert!(candidate < pause);
    assert!(pause < call);
    let closure_start = detailed
        .find("move |child_cancellation, deadline| async move")
        .unwrap();
    let closure = &detailed[closure_start..];
    let availability = closure
        .find("let Some(strategy) = strategy.filter(|_| has_newer_completed_boundary)")
        .unwrap();
    let request = closure.find("let request = CompactionRequest").unwrap();
    assert!(availability < request);
    assert!(!closure.contains("target_tokens == 0"));
    assert!(!closure.contains("validate_candidate"));
    assert!(!closure.contains("pause_after_candidate"));
    assert!(closure.contains("return Ok(None)"));
    assert!(closure.contains("strategy.compact(request).await.map(Some)"));
    assert!(!driver.contains("fn check_control("));
    assert!(!driver.contains("effective_deadline("));
    assert!(!driver.contains("tokio::select!"));
    assert!(!driver.contains("catch_unwind"));
    for removed in [
        "fn is_terminal_boundary(",
        "candidate.entries().iter().any(",
        "for boundary in candidate.completed_boundaries()",
    ] {
        assert!(
            !driver.contains(removed),
            "quadratic candidate scan remains: {removed}"
        );
    }
    for forbidden in [
        "SessionRuntime",
        "SessionHandle",
        "ConversationLog",
        "SessionLog",
        "append",
        "ContextProvider",
        "Workspace",
        "Store",
        "tokio::spawn",
        "retry",
        "serde",
        "unsafe",
        "#[allow",
        "#[expect",
    ] {
        assert!(
            !driver.contains(forbidden),
            "compaction driver contains {forbidden}"
        );
    }
    assert!(driver.lines().count() < 500);
    let deadline_tests = include_str!("../src/compaction/driver/tests/deadline.rs");
    assert!(
        deadline_tests
            .contains("turn_control_after_candidate_validation_precedes_strategy_availability")
    );
    assert!(
        deadline_tests.contains("cancellation_after_candidate_validation_reaches_helper_precheck")
    );
    assert!(deadline_tests.contains("deadline_after_candidate_validation_reaches_helper_precheck"));
    let test_support = include_str!("../src/compaction/driver/tests.rs");
    for required in [
        "OnceLock<Mutex<BTreeMap<TurnId, CandidateControlHook>>>",
        "install_candidate_control_hook(turn_id: TurnId)",
        ".insert(turn_id, hook.clone())",
        ".remove(&turn_id)",
        "Arc::new(Barrier::new(2))",
    ] {
        assert!(test_support.contains(required));
    }
    assert!(!test_support.contains("static mut"));
}

#[test]
fn conversation_owns_the_canonical_compaction_candidate_proof() {
    let module = include_str!("../src/conversation/mod.rs");
    let state = include_str!("../src/conversation/state.rs");
    let view = include_str!("../src/conversation/view.rs");
    let candidate = include_str!("../src/conversation/compaction_candidate.rs");
    assert!(module.contains("pub(crate) mod compaction_candidate;"));
    for required in [
        "pub(crate) fn validated_compaction_candidate(",
        "self.validated_state(spec, limits)?.compaction_candidate()",
        "ConversationState::new(spec.clone(), limits.clone())?",
        ".candidate(self.entries())?",
        "state.head() != self.head",
    ] {
        assert!(view.contains(required), "candidate proof misses {required}");
    }
    for required in [
        "pub(crate) fn compaction_candidate(&self) -> CompactionCandidate",
        "self.projection.entries().to_vec().into()",
        ".terminal_boundaries()",
        ".latest_summary_through()",
        "CompactionCandidate::from_confirmed(",
    ] {
        assert!(
            state.contains(required),
            "candidate builder misses {required}"
        );
    }
    for required in [
        "pub struct CompactionCandidate",
        "entries: Arc<[ConversationEntry]>",
        "completed_boundaries: Arc<[ConversationSeq]>",
        "pub(super) fn from_confirmed(",
        "#[cfg(test)]\n    pub(crate) fn forge_for_test(",
    ] {
        assert!(
            candidate.contains(required),
            "candidate DTO misses {required}"
        );
    }
    assert!(!module.contains("pub use compaction_candidate"));
    assert!(!candidate.contains("serde"));
    assert!(!candidate.contains("pub fn from_confirmed("));
    assert!(!candidate.contains("pub(crate) fn from_confirmed("));
    let driver = include_str!("../src/compaction/driver.rs");
    let strategy = include_str!("../src/compaction/strategy.rs");
    for source in [view, driver, strategy] {
        assert!(!source.contains("CompactionCandidate::from_confirmed("));
    }
    assert!(candidate.lines().count() < 500);
    assert!(view.lines().count() < 500);
}

#[test]
fn validated_proposal_is_a_stale_head_proof_without_commit_authority() {
    let driver = include_str!("../src/compaction/driver.rs");
    let proof_start = driver
        .find("pub(crate) struct ValidatedCompactionProposal")
        .unwrap();
    let proof_end = proof_start
        + driver[proof_start..]
            .find("impl CompactionDriver {")
            .unwrap();
    let proof = &driver[proof_start..proof_end];
    for required in [
        "snapshot_head: ConversationSeq",
        "through_seq: ConversationSeq",
        "summary: BoundedText",
        "pub(crate) const fn snapshot_head(",
        "pub(crate) const fn through_seq(",
        "pub(crate) const fn summary(",
        "current_head == snapshot_head before CommitSummary",
        ".field(\"summary_bytes\"",
    ] {
        assert!(
            driver.contains(required),
            "validated proof misses {required}"
        );
    }
    for forbidden in [
        "pub struct ValidatedCompactionProposal",
        "pub(crate) fn new(",
        "fn commit(",
        "fn append(",
    ] {
        assert!(
            !proof.contains(forbidden),
            "validated proof exposes {forbidden}"
        );
    }
}

#[test]
fn public_compaction_port_exports_remain_adapter_only() {
    let module = include_str!("../src/compaction/mod.rs");
    let root = include_str!("../src/lib.rs");
    assert!(
        module.contains("pub use crate::conversation::compaction_candidate::CompactionCandidate;")
    );
    for name in [
        "CompactionDriver",
        "CompactionDriverFailure",
        "ValidatedCompactionProposal",
    ] {
        assert!(!root.contains(name));
    }
    assert!(!module.contains("pub use driver::"));
}

#[test]
fn conversation_pending_tools_use_an_indexed_slice_without_remove_zero() {
    let validator = include_str!("../src/conversation/validator.rs");
    for required in [
        "pending_tools: Vec<PendingToolCall>",
        "pending_index: usize",
        "&self.pending_tools[self.pending_index..]",
        "self.pending_index += 1",
        "self.pending_tools.clear()",
        "self.pending_index = 0",
    ] {
        assert!(
            validator.contains(required),
            "pending index misses {required}"
        );
    }
    assert!(!validator.contains("remove(0)"));
}

#[test]
fn summary_semantics_preserve_active_turns_but_require_prior_terminal_boundaries() {
    let validator = include_str!("../src/conversation/validator.rs");
    let tests = include_str!("../src/conversation/validator/tests/summary.rs");
    let projection = include_str!("../src/conversation/view/tests/compaction.rs");
    assert!(!validator.contains("SummaryDuringActiveTurn"));
    let apply = &validator[validator.find("fn apply_summary(").unwrap()..];
    let apply = &apply[..apply.find("fn validate_user_input(").unwrap()];
    for required in [
        "entry.through > self.head",
        "self.terminal_boundaries.contains(&entry.through)",
        "entry.through <= last",
        "self.last_summary_through = Some(entry.through)",
    ] {
        assert!(
            apply.contains(required),
            "summary validator misses {required}"
        );
    }
    for forbidden in [
        "self.active_turn =",
        "self.active_phase =",
        "self.pending_tools",
        "self.seen_tool_call_ids",
    ] {
        assert!(!apply.contains(forbidden));
    }
    for required in [
        "summary_during_awaiting_assistant_preserves_the_active_turn",
        "summary_during_tool_results_preserves_pending_calls_and_continuation",
        "summary_during_final_assistant_preserves_terminal_eligibility",
        "active_turn_summary_cannot_cross_a_nonterminal_or_fake_boundary",
        "ConversationValidationError::DuplicateToolCallId",
    ] {
        assert!(
            tests.contains(required),
            "summary evidence misses {required}"
        );
    }
    assert!(
        projection.contains("active_turn_summary_keeps_the_current_turn_in_the_prompt_projection")
    );
}
