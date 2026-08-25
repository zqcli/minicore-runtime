use super::*;

#[tokio::test(flavor = "current_thread")]
async fn provider_bundle_is_validated_and_sorted_canonically() {
    let provider = ScriptProvider::new(vec![ProviderBehavior::Bundle(bundle(vec![
        block("turn", ContextSlot::TurnContext, 9, "turn"),
        block("knowledge-b", ContextSlot::RetrievedKnowledge, 1, "b"),
        block("project-low", ContextSlot::ProjectInstructions, 1, "low"),
        block("knowledge-a", ContextSlot::RetrievedKnowledge, 1, "a"),
        block("project-high", ContextSlot::ProjectInstructions, 5, "high"),
    ]))]);
    let driver = ContextDriver::new(
        Some(provider_port(&provider)),
        Duration::from_secs(5),
        limits(),
    )
    .unwrap();
    let result = driver
        .provide(request(Duration::from_secs(30), CancellationToken::new()))
        .await
        .unwrap();
    assert_eq!(
        result
            .blocks()
            .iter()
            .map(|block| block.source.as_str())
            .collect::<Vec<_>>(),
        vec![
            "project-high",
            "project-low",
            "knowledge-a",
            "knowledge-b",
            "turn",
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_count_and_byte_limits_return_exact_context_errors() {
    let cases = vec![
        (
            limits(),
            bundle(vec![
                block("same", ContextSlot::TurnContext, 0, "a"),
                block("same", ContextSlot::ProjectInstructions, 0, "b"),
            ]),
            ContextError::DuplicateSource,
        ),
        (
            SemanticLimits {
                max_context_blocks: 1,
                ..limits()
            },
            bundle(vec![
                block("a", ContextSlot::TurnContext, 0, ""),
                block("b", ContextSlot::TurnContext, 0, ""),
            ]),
            ContextError::TooManyBlocks,
        ),
        (
            SemanticLimits {
                max_context_bytes: 2,
                ..limits()
            },
            bundle(vec![block("a", ContextSlot::TurnContext, 0, "abc")]),
            ContextError::BlockTooLarge,
        ),
        (
            SemanticLimits {
                max_context_bytes: 3,
                ..limits()
            },
            bundle(vec![
                block("a", ContextSlot::TurnContext, 0, "ab"),
                block("b", ContextSlot::TurnContext, 0, "cd"),
            ]),
            ContextError::TotalTooLarge,
        ),
    ];
    for (limits, bundle, expected) in cases {
        let provider = ScriptProvider::new(vec![ProviderBehavior::Bundle(bundle)]);
        let driver = ContextDriver::new(
            Some(provider_port(&provider)),
            Duration::from_secs(5),
            limits,
        )
        .unwrap();
        assert_eq!(
            driver
                .provide(request(Duration::from_secs(30), CancellationToken::new(),))
                .await,
            Err(expected)
        );
    }
}

#[test]
fn invalid_timeout_or_limits_fail_before_provider_ownership() {
    let provider = ScriptProvider::new(Vec::new());
    assert!(matches!(
        ContextDriver::new(Some(provider_port(&provider)), Duration::ZERO, limits()),
        Err(ContextError::InvalidLimits)
    ));
    let invalid = SemanticLimits {
        max_context_blocks: 0,
        ..limits()
    };
    assert!(matches!(
        ContextDriver::new(
            Some(provider_port(&provider)),
            Duration::from_secs(1),
            invalid,
        ),
        Err(ContextError::InvalidLimits)
    ));
    assert_eq!(provider.calls(), 0);
}
