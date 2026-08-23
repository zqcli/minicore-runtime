use super::*;

async fn validate(
    candidate: CompactionCandidate,
    proposed: CompactionProposal,
    max_summary_bytes: usize,
) -> Result<ValidatedCompactionProposal, CompactionError> {
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Proposal(proposed)]);
    run(
        &driver(Some(strategy_port(&strategy)), max_summary_bytes),
        candidate,
        10,
        Duration::from_secs(30),
        CancellationToken::new(),
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn proposal_must_use_an_exact_newer_completed_boundary_within_snapshot() {
    for (candidate, through) in [
        (active_candidate(), 2),
        (active_candidate(), 4),
        (active_candidate(), 5),
        (summarized_candidate(true), 3),
    ] {
        assert_eq!(
            validate(candidate, proposal(through, "summary"), 64).await,
            Err(CompactionError::InvalidRequest)
        );
    }

    let before_open_turn = validate(active_candidate(), proposal(3, "summary"), 64)
        .await
        .unwrap();
    assert_eq!(before_open_turn.snapshot_head(), ConversationSeq::new(4));
    assert_eq!(before_open_turn.through_seq(), ConversationSeq::new(3));
}

#[tokio::test(flavor = "current_thread")]
async fn summary_must_be_safe_nonempty_and_within_the_semantic_limit() {
    let empty = CompactionProposal {
        through_seq: ConversationSeq::new(3),
        summary: BoundedText::new("").unwrap(),
    };
    assert_eq!(
        validate(completed_candidate(), empty, 3).await,
        Err(CompactionError::InvalidRequest)
    );

    let control = CompactionProposal {
        through_seq: ConversationSeq::new(3),
        summary: BoundedText::new("a\0b").unwrap(),
    };
    assert_eq!(
        validate(completed_candidate(), control, 3).await,
        Err(CompactionError::InvalidRequest)
    );

    let exact = validate(completed_candidate(), proposal(3, "abc"), 3)
        .await
        .unwrap();
    assert_eq!(exact.summary().as_str(), "abc");
    assert_eq!(
        validate(completed_candidate(), proposal(3, "abcd"), 3).await,
        Err(CompactionError::InvalidRequest)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn forged_candidate_boundary_indexes_are_rejected_before_strategy() {
    let completed = completed_candidate();
    let summarized = summarized_candidate(true);
    let forged = [
        CompactionCandidate::forge_for_test(
            completed.entries().to_vec().into(),
            completed.head(),
            completed.latest_summary_through(),
            vec![ConversationSeq::new(2), ConversationSeq::new(3)].into(),
        ),
        CompactionCandidate::forge_for_test(
            completed.entries().to_vec().into(),
            completed.head(),
            completed.latest_summary_through(),
            vec![ConversationSeq::new(3), ConversationSeq::new(3)].into(),
        ),
        CompactionCandidate::forge_for_test(
            summarized.entries().to_vec().into(),
            summarized.head(),
            summarized.latest_summary_through(),
            vec![ConversationSeq::new(7), ConversationSeq::new(3)].into(),
        ),
        CompactionCandidate::forge_for_test(
            completed.entries().to_vec().into(),
            completed.head(),
            completed.latest_summary_through(),
            vec![ConversationSeq::new(3), ConversationSeq::new(9)].into(),
        ),
        CompactionCandidate::forge_for_test(
            vec![
                assistant(2, 1, "out of order"),
                user(1, 1, "out of order"),
                terminal(3, 1),
            ]
            .into(),
            ConversationSeq::new(3),
            None,
            vec![ConversationSeq::new(3)].into(),
        ),
        CompactionCandidate::forge_for_test(
            vec![user(1, 1, "gap"), terminal(3, 1)].into(),
            ConversationSeq::new(3),
            None,
            vec![ConversationSeq::new(3)].into(),
        ),
        CompactionCandidate::forge_for_test(
            completed.entries().to_vec().into(),
            ConversationSeq::new(2),
            completed.latest_summary_through(),
            Arc::<[ConversationSeq]>::from([]),
        ),
    ];
    for candidate in forged {
        let strategy =
            ScriptStrategy::new(vec![StrategyBehavior::Proposal(proposal(3, "summary"))]);
        assert_eq!(
            run(
                &driver(Some(strategy_port(&strategy)), 64),
                candidate,
                10,
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .await,
            Err(CompactionError::InvalidRequest)
        );
        assert_eq!(strategy.calls(), 0);
    }
}

#[test]
fn proposal_validation_rechecks_that_the_selected_boundary_is_terminal() {
    let completed = completed_candidate();
    let forged = CompactionCandidate::forge_for_test(
        completed.entries().to_vec().into(),
        completed.head(),
        completed.latest_summary_through(),
        vec![ConversationSeq::new(2)].into(),
    );
    assert_eq!(
        validate_proposal(forged, proposal(2, "summary"), 64),
        Err(CompactionError::InvalidRequest)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn many_canonical_turns_preserve_linear_candidate_shape() {
    let mut entries = Vec::new();
    let mut seq = 1_u64;
    for turn in 1_u8..=64 {
        entries.push(user(seq, turn, "input"));
        seq += 1;
        entries.push(assistant(seq, turn, "answer"));
        seq += 1;
        entries.push(terminal(seq, turn));
        seq += 1;
    }
    let through = seq - 1;
    let candidate = canonical_candidate(entries);
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Proposal(proposal(
        through, "summary",
    ))]);
    let validated = run(
        &driver(Some(strategy_port(&strategy)), 64),
        candidate,
        10,
        Duration::from_secs(30),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(validated.through_seq(), ConversationSeq::new(through));
    assert_eq!(strategy.calls(), 1);
}
