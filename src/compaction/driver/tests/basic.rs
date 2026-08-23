use super::*;

#[test]
fn constructor_rejects_invalid_timeout_and_summary_limit() {
    for (timeout, max_summary_bytes) in [
        (Duration::ZERO, 1),
        (Duration::from_secs(24 * 60 * 60 + 1), 1),
        (Duration::from_secs(1), 0),
        (Duration::from_secs(1), BoundedText::MAX_BYTES + 1),
    ] {
        assert!(matches!(
            CompactionDriver::new(None, timeout, limits(max_summary_bytes)),
            Err(CompactionError::InvalidRequest)
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_strategy_boundary_newer_boundary_and_target_preflight_are_fixed() {
    assert_eq!(
        run(
            &driver(None, 64),
            completed_candidate(),
            10,
            Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await,
        Err(CompactionError::Unavailable)
    );

    let strategy = ScriptStrategy::new(Vec::new());
    let compact = driver(Some(strategy_port(&strategy)), 64);
    assert_eq!(
        run(
            &compact,
            CompactionCandidate::empty(),
            10,
            Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await,
        Err(CompactionError::Unavailable)
    );
    assert_eq!(
        run(
            &compact,
            summarized_candidate(false),
            10,
            Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await,
        Err(CompactionError::Unavailable)
    );
    assert_eq!(
        run(
            &compact,
            completed_candidate(),
            0,
            Duration::from_secs(30),
            CancellationToken::new(),
        )
        .await,
        Err(CompactionError::InvalidRequest)
    );
    assert_eq!(strategy.calls(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn success_delivers_exact_candidate_request_deadline_and_validated_proof() {
    let original = completed_candidate();
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Proposal(proposal(
        3,
        "private summary",
    ))]);
    let compact = CompactionDriver::new(
        Some(strategy_port(&strategy)),
        Duration::from_secs(2),
        limits(64),
    )
    .unwrap();
    let expected_deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(2))
        .unwrap()
        .into_std();
    let validated = run(
        &compact,
        original.clone(),
        123,
        Duration::from_secs(30),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(validated.snapshot_head(), ConversationSeq::new(3));
    assert_eq!(validated.through_seq(), ConversationSeq::new(3));
    assert_eq!(validated.summary().as_str(), "private summary");
    let debug = format!("{validated:?}");
    assert!(debug.contains("summary_bytes"));
    assert!(!debug.contains("private summary"));

    let requests = strategy.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.session_id, session_id());
    assert_eq!(request.turn_id, turn_id(9));
    assert_eq!(request.candidate, original);
    assert_eq!(request.target_tokens, 123);
    assert_eq!(request.deadline, expected_deadline);
    assert!(request.cancellation.is_cancelled());
}

#[tokio::test(flavor = "current_thread")]
async fn shorter_turn_deadline_is_passed_exactly() {
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Proposal(proposal(3, "summary"))]);
    let compact = driver(Some(strategy_port(&strategy)), 64);
    let expected = Instant::now() + Duration::from_secs(2);
    compact
        .run(
            session_id(),
            turn_id(9),
            completed_candidate(),
            10,
            expected,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(strategy.requests()[0].deadline, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn latest_summary_must_advance_to_a_newer_completed_boundary() {
    let strategy = ScriptStrategy::new(vec![StrategyBehavior::Proposal(proposal(
        7,
        "advanced summary",
    ))]);
    let validated = run(
        &driver(Some(strategy_port(&strategy)), 64),
        summarized_candidate(true),
        10,
        Duration::from_secs(30),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(validated.snapshot_head(), ConversationSeq::new(7));
    assert_eq!(validated.through_seq(), ConversationSeq::new(7));
    assert_eq!(validated.summary().as_str(), "advanced summary");
}
