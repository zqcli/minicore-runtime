use super::*;

#[test]
fn constructor_rejects_invalid_session_text_and_limits() {
    let mut spec = session_spec("valid", &[]);
    spec.system_prompt = BoundedText::new("bad\0prompt").unwrap();
    assert!(matches!(
        PromptBuilder::new(&spec, Vec::new(), SemanticLimits::default()),
        Err(PromptError::InvalidConfiguration)
    ));
    let invalid_limits = SemanticLimits {
        max_system_prompt_bytes: 0,
        ..SemanticLimits::default()
    };
    assert!(matches!(
        PromptBuilder::new(&session_spec("", &[]), Vec::new(), invalid_limits),
        Err(PromptError::InvalidConfiguration)
    ));
}

#[test]
fn malformed_view_head_sequence_and_incomplete_tool_exchange_are_rejected() {
    let builder = builder("", &["search"]);
    let empty_context = ContextBundle { blocks: Vec::new() };
    assert_eq!(
        builder.build(
            &view_with_head(2, vec![user(1, 1, "question")]),
            &empty_context,
            ModelLimits::default(),
        ),
        Err(PromptError::InvalidConversation)
    );
    assert_eq!(
        builder.build(
            &view_with_head(1, vec![user(1, 1, "first"), user(1, 2, "duplicate")],),
            &empty_context,
            ModelLimits::default(),
        ),
        Err(PromptError::InvalidConversation)
    );
    let incomplete = view(vec![
        user(1, 1, "question"),
        assistant(2, 1, None, None, vec![call(0, 1, "search")]),
    ]);
    assert_eq!(
        builder.build(&incomplete, &empty_context, ModelLimits::default()),
        Err(PromptError::InvalidConversation)
    );
}

#[test]
fn unsorted_duplicate_and_malformed_context_are_rejected_not_reordered() {
    let builder = builder("", &[]);
    let conversation = ConversationView::empty();
    let unsorted = ContextBundle {
        blocks: vec![
            context_block("turn", ContextSlot::TurnContext, 0, "turn"),
            context_block("project", ContextSlot::ProjectInstructions, 0, "project"),
        ],
    };
    assert_eq!(
        builder.build(&conversation, &unsorted, ModelLimits::default()),
        Err(PromptError::InvalidContext)
    );
    let duplicate = ContextBundle {
        blocks: vec![
            context_block("same", ContextSlot::TurnContext, 0, "one"),
            context_block("same", ContextSlot::TurnContext, 0, "two"),
        ],
    };
    assert_eq!(
        builder.build(&conversation, &duplicate, ModelLimits::default()),
        Err(PromptError::InvalidContext)
    );
    let malformed = ContextBundle {
        blocks: vec![ContextBlock {
            source: "bad".parse().unwrap(),
            slot: ContextSlot::TurnContext,
            priority: 0,
            content: BoundedText::new("bad\0context").unwrap(),
        }],
    };
    assert_eq!(
        builder.build(&conversation, &malformed, ModelLimits::default()),
        Err(PromptError::InvalidContext)
    );

    let hard_limit = ContextBundle {
        blocks: vec![context_block(
            "large",
            ContextSlot::TurnContext,
            0,
            &"x".repeat(BoundedText::MAX_BYTES),
        )],
    };
    assert_eq!(
        builder.build(&conversation, &hard_limit, ModelLimits::default()),
        Err(PromptError::InvalidContext)
    );
}

#[test]
fn assistant_text_and_reasoning_follow_semantic_round_limits() {
    let limits = SemanticLimits {
        max_model_text_bytes_per_round: 3,
        max_model_reasoning_bytes_per_round: 3,
        ..SemanticLimits::default()
    };
    let builder = PromptBuilder::new(&session_spec("", &[]), Vec::new(), limits).unwrap();
    let context = ContextBundle { blocks: Vec::new() };
    let exact = view(vec![
        user(1, 1, "question"),
        assistant(2, 1, Some("abc"), Some("abc"), Vec::new()),
    ]);
    assert!(
        builder
            .build(&exact, &context, ModelLimits::default())
            .is_ok()
    );
    for conversation in [
        view(vec![
            user(1, 1, "question"),
            assistant(2, 1, Some("abcd"), Some("abc"), Vec::new()),
        ]),
        view(vec![
            user(1, 1, "question"),
            assistant(2, 1, Some("abc"), Some("abcd"), Vec::new()),
        ]),
    ] {
        assert_eq!(
            builder.build(&conversation, &context, ModelLimits::default()),
            Err(PromptError::InvalidConversation)
        );
    }

    let mut oversized_summary_entry = summary(4, 3, "abc");
    if let ConversationEntry::Summary(entry) = &mut oversized_summary_entry {
        entry.summary = BoundedText::new("abcd").unwrap();
    }
    let oversized_summary = view(vec![
        user(1, 1, "question"),
        assistant(2, 1, None, Some("abc"), Vec::new()),
        terminal(3, 1),
        oversized_summary_entry,
    ]);
    assert_eq!(
        builder.build(&oversized_summary, &context, ModelLimits::default()),
        Err(PromptError::InvalidConversation)
    );
}

#[test]
fn exact_request_serialization_drives_budget_and_includes_reasoning_and_limits() {
    let builder = builder("session", &["search"]);
    let conversation = view(vec![user(1, 1, "question")]);
    let context = ContextBundle { blocks: Vec::new() };
    let output = 7_u32;
    let mut exact_window = 512_u32;
    let mut independently_built = None;
    for _ in 0..16 {
        let limits = ModelLimits::new(Some(exact_window), Some(output)).unwrap();
        let request = ModelRequest::new(
            builder.compose_messages(&conversation, &context).unwrap(),
            builder.tools.to_vec(),
            limits,
            builder.spec.reasoning,
        )
        .unwrap();
        let derived = u32::try_from(direct_request_tokens(&request) + u64::from(output)).unwrap();
        if derived == exact_window {
            independently_built = Some(request);
            break;
        }
        exact_window = derived;
    }
    let independently_built = independently_built.unwrap();
    let exact = ModelLimits::new(Some(exact_window), Some(output)).unwrap();
    let direct = direct_request_tokens(&independently_built);
    assert_eq!(direct + u64::from(output), u64::from(exact_window));
    assert_eq!(
        builder
            .remaining_context_budget(&conversation, exact)
            .unwrap(),
        0
    );
    let returned = builder.build(&conversation, &context, exact).unwrap();
    assert_eq!(returned, independently_built);
    assert_eq!(estimate_request(&returned).unwrap(), direct);
    assert_eq!(
        direct,
        u64::try_from(serde_json::to_vec(&returned).unwrap().len().div_ceil(4)).unwrap()
    );

    let below = ModelLimits::new(Some(exact_window - 1), Some(output)).unwrap();
    let below_request = ModelRequest::new(
        builder.compose_messages(&conversation, &context).unwrap(),
        builder.tools.to_vec(),
        below,
        builder.spec.reasoning,
    )
    .unwrap();
    assert!(
        direct_request_tokens(&below_request) + u64::from(output) > u64::from(exact_window - 1)
    );
    assert_eq!(
        builder.remaining_context_budget(&conversation, below),
        Err(PromptError::ContextOverflow)
    );
    assert_eq!(
        builder.build(&conversation, &context, below),
        Err(PromptError::ContextOverflow)
    );
    let unknown = ModelLimits::new(None, Some(output)).unwrap();
    assert_eq!(
        builder
            .remaining_context_budget(&conversation, unknown)
            .unwrap(),
        u64::MAX
    );

    let disabled = ModelRequest::new(
        returned.messages().to_vec(),
        returned.tools().to_vec(),
        exact,
        ReasoningPreference::Disabled,
    )
    .unwrap();
    assert!(direct_request_tokens(&disabled) > direct);
    let numeric_limits = ModelLimits::new(Some(123_456), Some(123_456)).unwrap();
    let numeric = ModelRequest::new(
        returned.messages().to_vec(),
        returned.tools().to_vec(),
        numeric_limits,
        returned.reasoning(),
    )
    .unwrap();
    let absent = ModelRequest::new(
        returned.messages().to_vec(),
        returned.tools().to_vec(),
        ModelLimits::default(),
        returned.reasoning(),
    )
    .unwrap();
    assert!(direct_request_tokens(&numeric) > direct_request_tokens(&absent));

    assert_eq!(estimate_tokens(usize::MAX), Err(PromptError::TokenOverflow));
    assert_eq!(
        remaining_budget(1, 1, ModelLimits::new(Some(1), Some(1)).unwrap()),
        Err(PromptError::ContextOverflow)
    );
}

fn direct_request_tokens(request: &ModelRequest) -> u64 {
    let bytes = serde_json::to_vec(request).unwrap().len();
    u64::try_from(bytes.div_ceil(4)).unwrap()
}
