#[test]
fn compaction_driver_is_private_single_strategy_and_owner_neutral() {
    let module = include_str!("../src/compaction/mod.rs");
    assert!(module.contains("mod driver;"));
    assert!(
        module.contains("pub(crate) use driver::{CompactionDriver, ValidatedCompactionProposal};")
    );
    assert!(!module.contains("pub use driver"));

    let driver = include_str!("../src/compaction/driver.rs");
    for required in [
        "pub(crate) struct CompactionDriver",
        "strategy: Option<Arc<dyn CompactionStrategy>>",
        "pub(crate) async fn run(",
        "effective_deadline(turn_deadline, self.timeout)?",
        "catch_unwind(AssertUnwindSafe(|| strategy.compact(request)))",
        "AssertUnwindSafe(future).catch_unwind()",
        "let child_cancellation = cancellation.child_token();",
        "child_cancellation.cancel();",
        "let has_newer_completed_boundary = validate_candidate(&candidate)?;",
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
        "#[allow",
        "#[expect",
    ] {
        assert!(
            !driver.contains(forbidden),
            "compaction driver contains {forbidden}"
        );
    }
    assert!(driver.lines().count() < 500);
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
    let proof_end = driver.find("impl CompactionDriver").unwrap();
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
    for name in ["CompactionDriver", "ValidatedCompactionProposal"] {
        assert!(!root.contains(name));
    }
    assert!(!module.contains("pub use driver::"));
}
