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
    let empty_context = empty_context();
    assert_eq!(
        finish(
            &builder,
            &view_with_head(2, vec![user(1, 1, "question")]),
            &empty_context,
            ModelLimits::default(),
        ),
        Err(PromptError::InvalidConversation)
    );
    assert_eq!(
        finish(
            &builder,
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
        finish(
            &builder,
            &incomplete,
            &empty_context,
            ModelLimits::default(),
        ),
        Err(PromptError::InvalidConversation)
    );
}

#[test]
fn context_block_formatting_failure_is_reported() {
    let builder = builder("", &[]);
    let conversation = ConversationView::empty();
    let malformed = checked_context(vec![ContextBlock {
        source: "bad".parse().unwrap(),
        slot: ContextSlot::TurnContext,
        priority: 0,
        content: BoundedText::new("bad\0context").unwrap(),
    }]);
    assert_eq!(
        finish(&builder, &conversation, &malformed, ModelLimits::default(),),
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
    let context = empty_context();
    let exact = view(vec![
        user(1, 1, "question"),
        assistant(2, 1, Some("abc"), Some("abc"), Vec::new()),
    ]);
    assert!(finish(&builder, &exact, &context, ModelLimits::default()).is_ok());
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
            finish(&builder, &conversation, &context, ModelLimits::default()),
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
        finish(
            &builder,
            &oversized_summary,
            &context,
            ModelLimits::default(),
        ),
        Err(PromptError::InvalidConversation)
    );
}

#[test]
fn exact_request_serialization_drives_budget_and_includes_reasoning_and_limits() {
    let builder = builder("session", &["search"]);
    let conversation = view(vec![user(1, 1, "question")]);
    let context = checked_context(vec![context_block(
        "test-context",
        ContextSlot::TurnContext,
        0,
        "context",
    )]);
    let output = 7_u32;
    let mut fixed_window = 512_u32;
    let independently_built = loop {
        let limits = ModelLimits::new(Some(fixed_window), Some(output)).unwrap();
        let expected = expected_request(limits, false);
        let derived = u32::try_from(direct_request_tokens(&expected) + u64::from(output)).unwrap();
        if derived == fixed_window {
            break expected;
        }
        fixed_window = derived;
    };
    let fixed_limits = *independently_built.limits();
    let fixed_direct = direct_request_tokens(&independently_built);
    let fixed_plan = builder.plan(&conversation, fixed_limits).unwrap();
    assert_eq!(fixed_plan.fixed_input_tokens(), fixed_direct);
    assert_eq!(fixed_plan.remaining_context_budget(), Ok(0));

    let mut exact_window = fixed_window;
    let expected_final = loop {
        let limits = ModelLimits::new(Some(exact_window), Some(output)).unwrap();
        let expected = expected_request(limits, true);
        let derived = u32::try_from(direct_request_tokens(&expected) + u64::from(output)).unwrap();
        if derived == exact_window {
            break expected;
        }
        exact_window = derived;
    };
    let exact = ModelLimits::new(Some(exact_window), Some(output)).unwrap();
    assert_eq!(expected_final, expected_request(exact, true));
    let expected_fixed = expected_request(exact, false);
    let direct = direct_request_tokens(&expected_fixed);
    let plan = builder.plan(&conversation, exact).unwrap();
    assert_eq!(plan.fixed_input_tokens(), direct);
    assert_eq!(
        plan.remaining_context_budget(),
        Ok(u64::from(exact_window) - u64::from(output) - direct)
    );
    let returned = plan.finish(&context).unwrap();
    assert_eq!(returned, expected_final);
    assert_eq!(
        direct_request_tokens(&returned) + u64::from(output),
        u64::from(exact_window)
    );
    assert_eq!(
        estimate_request(&returned).unwrap(),
        u64::try_from(serde_json::to_vec(&returned).unwrap().len().div_ceil(4)).unwrap()
    );
    assert_eq!(direct, direct_request_tokens(&expected_fixed));

    let below = ModelLimits::new(Some(exact_window - 1), Some(output)).unwrap();
    let below_request = expected_request(below, true);
    assert!(
        direct_request_tokens(&below_request) + u64::from(output) > u64::from(exact_window - 1)
    );
    assert!(
        builder
            .plan(&conversation, below)
            .unwrap()
            .remaining_context_budget()
            .is_ok()
    );
    assert_eq!(
        finish(&builder, &conversation, &context, below),
        Err(PromptError::ContextOverflow)
    );
    let unknown = ModelLimits::new(None, Some(output)).unwrap();
    assert_eq!(
        builder
            .plan(&conversation, unknown)
            .unwrap()
            .remaining_context_budget()
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

fn expected_request(limits: ModelLimits, include_context: bool) -> ModelRequest {
    let mut messages = vec![
        ModelMessage::system(KERNEL_INVARIANT).unwrap(),
        ModelMessage::system("session").unwrap(),
    ];
    if include_context {
        messages.push(
            ModelMessage::system(
                "[minicore-context slot=turn_context source=test-context]\ncontext",
            )
            .unwrap(),
        );
    }
    messages.push(ModelMessage::user("question").unwrap());
    ModelRequest::new(
        messages,
        vec![tool_spec("search")],
        limits,
        ReasoningPreference::High,
    )
    .unwrap()
}

fn direct_request_tokens(request: &ModelRequest) -> u64 {
    let bytes = serde_json::to_vec(request).unwrap().len();
    u64::try_from(bytes.div_ceil(4)).unwrap()
}
