use super::*;

#[test]
fn enabled_tool_specs_must_match_exact_sorted_unique_set() {
    let spec = session_spec("", &["alpha", "beta"]);
    let limits = SemanticLimits::default();
    assert!(
        PromptBuilder::new(
            &spec,
            vec![tool_spec("alpha"), tool_spec("beta")],
            limits.clone(),
        )
        .is_ok()
    );
    for tools in [
        vec![tool_spec("beta"), tool_spec("alpha")],
        vec![tool_spec("alpha"), tool_spec("alpha")],
        vec![tool_spec("alpha")],
        vec![tool_spec("alpha"), tool_spec("beta"), tool_spec("gamma")],
    ] {
        assert!(matches!(
            PromptBuilder::new(&spec, tools, limits.clone()),
            Err(PromptError::InvalidTools)
        ));
    }
}

#[test]
fn tool_count_name_description_and_schema_semantic_boundaries_are_checked() {
    let count_limits = SemanticLimits {
        max_tool_count: 1,
        ..SemanticLimits::default()
    };
    assert!(matches!(
        PromptBuilder::new(
            &session_spec("", &["alpha", "beta"]),
            vec![tool_spec("alpha"), tool_spec("beta")],
            count_limits,
        ),
        Err(PromptError::InvalidTools)
    ));

    let name_limits = SemanticLimits {
        max_tool_name_bytes: 3,
        ..SemanticLimits::default()
    };
    assert!(matches!(
        PromptBuilder::new(
            &session_spec("", &["tool"]),
            vec![tool_spec("tool")],
            name_limits,
        ),
        Err(PromptError::InvalidTools)
    ));

    let description_limits = SemanticLimits {
        max_tool_schema_bytes: 8,
        ..SemanticLimits::default()
    };
    let spec = session_spec("", &["tool"]);
    let exact = ToolSpec::new("tool".parse().unwrap(), "12345678", json!({})).unwrap();
    assert!(PromptBuilder::new(&spec, vec![exact], description_limits.clone()).is_ok());
    let mut oversized = tool_spec("tool");
    oversized.description = BoundedText::new("123456789").unwrap();
    assert!(matches!(
        PromptBuilder::new(&spec, vec![oversized], description_limits),
        Err(PromptError::InvalidTools)
    ));

    let schema_limits = SemanticLimits {
        max_tool_schema_bytes: 7,
        ..SemanticLimits::default()
    };
    let exact = ToolSpec::new("tool".parse().unwrap(), "tool", json!({"a": 1})).unwrap();
    assert!(PromptBuilder::new(&spec, vec![exact], schema_limits.clone()).is_ok());
    let oversized = ToolSpec::new("tool".parse().unwrap(), "tool", json!({"a": 10})).unwrap();
    assert!(matches!(
        PromptBuilder::new(&spec, vec![oversized], schema_limits),
        Err(PromptError::InvalidTools)
    ));
}

#[test]
fn built_request_uses_exact_frozen_tools_reasoning_and_limits() {
    let builder = builder("", &["alpha", "beta"]);
    let limits = ModelLimits::new(Some(4096), Some(64)).unwrap();
    let request = builder
        .build(&ConversationView::empty(), &empty_context(), limits)
        .unwrap();
    assert_eq!(request.tools(), &[tool_spec("alpha"), tool_spec("beta")]);
    assert_eq!(request.reasoning(), ReasoningPreference::High);
    assert_eq!(request.limits(), &limits);
}
