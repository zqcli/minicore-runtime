use super::*;

#[test]
fn empty_view_produces_the_empty_compaction_candidate() {
    let candidate = ConversationView::empty()
        .validated_compaction_candidate(&spec(), &SemanticLimits::default())
        .unwrap();
    assert_eq!(candidate, crate::compaction::CompactionCandidate::empty());
}

#[test]
fn candidate_retains_active_turn_and_sorted_completed_boundaries() {
    let first = TurnId::new().unwrap();
    let second = TurnId::new().unwrap();
    let active = TurnId::new().unwrap();
    let conversation = view(vec![
        user(1, first, 4),
        assistant(
            2,
            first,
            Some("first answer"),
            Vec::new(),
            ModelFinishReason::Stop,
        ),
        terminal(3, first),
        user(4, second, 4),
        assistant(
            5,
            second,
            Some("second answer"),
            Vec::new(),
            ModelFinishReason::Stop,
        ),
        terminal(6, second),
        summary(7, 3, "sensitive summary"),
        user(8, active, 1),
    ]);
    let candidate = conversation
        .validated_compaction_candidate(&spec(), &SemanticLimits::default())
        .unwrap();
    assert_eq!(candidate.head(), ConversationSeq::new(8));
    assert_eq!(
        candidate.latest_summary_through(),
        Some(ConversationSeq::new(3))
    );
    assert_eq!(
        candidate.completed_boundaries(),
        &[ConversationSeq::new(3), ConversationSeq::new(6)]
    );
    assert_eq!(candidate.entries(), conversation.entries());
    assert!(
        !candidate
            .completed_boundaries()
            .contains(&ConversationSeq::new(8))
    );
    let debug = format!("{candidate:?}");
    assert!(!debug.contains("first answer"));
    assert!(!debug.contains("sensitive summary"));
}

#[test]
fn malformed_compaction_views_return_exact_canonical_errors() {
    let turn = TurnId::new().unwrap();
    assert_eq!(
        view(vec![user(2, turn, 4)])
            .validated_compaction_candidate(&spec(), &SemanticLimits::default())
            .unwrap_err(),
        ConversationValidationError::SequenceGap
    );
    assert_eq!(
        view(vec![
            user(1, turn, 4),
            assistant(
                2,
                TurnId::new().unwrap(),
                Some("wrong turn"),
                Vec::new(),
                ModelFinishReason::Stop,
            ),
        ])
        .validated_compaction_candidate(&spec(), &SemanticLimits::default())
        .unwrap_err(),
        ConversationValidationError::TurnMismatch
    );
    let wrong_head = ConversationView::from_confirmed(
        ConversationSeq::new(2),
        Arc::from(vec![user(1, turn, 1)]),
    );
    assert_eq!(
        wrong_head
            .validated_compaction_candidate(&spec(), &SemanticLimits::default())
            .unwrap_err(),
        ConversationValidationError::SequenceGap
    );
}
