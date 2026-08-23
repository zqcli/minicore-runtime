use super::*;

#[tokio::test(flavor = "current_thread")]
async fn allow_executes_once_with_exact_invocation_and_frozen_policy_spec() {
    let expected = invocation("search", 1, json!({"query": "rust"}));
    let tool = ScriptTool::mutating(
        "search",
        vec![ToolBehavior::Complete(ToolOutput::new("done").unwrap())],
    );
    let policy = allow_policy();
    let driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let (suspensions, _suspension_rx, progress, mut progress_rx) = channels();
    let turn_deadline = deadline_after(Duration::from_secs(2));
    let result = driver
        .run(
            expected.clone(),
            turn_deadline,
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();

    assert_outcome(&result, ToolResultOutcome::Success, "done");
    assert_eq!(tool.calls(), 1);
    assert_eq!(tool.invocations(), vec![expected.clone()]);
    let requests = policy.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].invocation, expected);
    assert_eq!(requests[0].spec.description().as_str(), "frozen spec");
    assert_eq!(requests[0].deadline, turn_deadline);
    assert_eq!(tool.deadlines(), vec![turn_deadline]);
    assert_eq!(tool.spec_calls(), 1);
    assert_eq!(tool.spec().description().as_str(), "mutated spec");
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ToolDriverProgress::Started { tool_call_id, tool_name })
            if tool_call_id == call_id(1) && tool_name.as_str() == "search"
    ));
    assert!(progress_rx.try_recv().is_err());
}

#[test]
fn enabled_tools_require_policy_and_configuration_is_checked() {
    let tools = ToolSet::default();
    let enabled = BTreeSet::from(["search".parse().unwrap()]);
    assert!(matches!(
        ToolDriver::new(tools.clone(), enabled, None, config()),
        Err(ToolDriverBuildError::MissingPolicy)
    ));
    let invalid = config_with(Duration::ZERO, Duration::from_secs(1), 1, 1);
    assert!(matches!(
        ToolDriver::new(tools, BTreeSet::new(), None, invalid),
        Err(ToolDriverBuildError::InvalidConfiguration)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn missing_disabled_and_mismatched_tool_paths_fail_without_policy_or_execution() {
    let policy = allow_policy();
    let missing = ToolDriver::new(
        ToolSet::default(),
        BTreeSet::from(["missing".parse().unwrap()]),
        Some(policy_port(&policy)),
        config(),
    )
    .unwrap();
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let result = missing
        .run(
            invocation("missing", 2, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, ToolResultOutcome::Failed);
    assert_eq!(policy.calls(), 0);

    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(
            ToolOutput::new("unexpected").unwrap(),
        )],
    );
    let mut builder = ToolSet::builder();
    let registered = Arc::clone(&tool);
    let registered: Arc<dyn Tool> = registered;
    builder.register_arc(registered);
    let disabled =
        ToolDriver::new(builder.build().unwrap(), BTreeSet::new(), None, config()).unwrap();
    let result = disabled
        .run(
            invocation("search", 3, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, ToolResultOutcome::Failed);
    assert_eq!(tool.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_and_semantically_oversized_arguments_fail_preflight() {
    let tool = ScriptTool::new(
        "search",
        vec![
            ToolBehavior::Complete(ToolOutput::new("exact").unwrap()),
            ToolBehavior::Complete(ToolOutput::new("unexpected").unwrap()),
        ],
    );
    let policy = allow_policy();
    let tool_driver = driver(
        Arc::clone(&tool),
        Some(policy_port(&policy)),
        config_with(
            Duration::from_secs(5),
            Duration::from_secs(10),
            7,
            BoundedText::MAX_BYTES,
        ),
    );
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let exact = tool_driver
        .run(
            invocation("search", 4, json!({"a": 1})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_outcome(&exact, ToolResultOutcome::Success, "exact");

    let oversized = tool_driver
        .run(
            invocation("search", 5, json!({"a": 10})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(oversized.outcome, ToolResultOutcome::Failed);

    let mut malformed = invocation("search", 6, json!({}));
    malformed.arguments = json!([]);
    let malformed = tool_driver
        .run(
            malformed,
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(malformed.outcome, ToolResultOutcome::Failed);

    let shape_driver = driver(Arc::clone(&tool), Some(policy_port(&policy)), config());
    let mut deep_arguments = json!({});
    for _ in 0..=crate::value::MAX_JSON_DEPTH {
        deep_arguments = json!({"nested": deep_arguments});
    }
    let mut deep = invocation("search", 9, json!({}));
    deep.arguments = deep_arguments;
    let deep = shape_driver
        .run(
            deep,
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(deep.outcome, ToolResultOutcome::Failed);

    let mut node_heavy = invocation("search", 10, json!({}));
    node_heavy.arguments = json!({
        "items": vec![Value::Null; crate::value::MAX_JSON_NODES]
    });
    let node_heavy = shape_driver
        .run(
            node_heavy,
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(node_heavy.outcome, ToolResultOutcome::Failed);
    assert_eq!(tool.calls(), 1);
    assert_eq!(policy.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn denial_uses_checked_reason_or_bounded_static_fallback() {
    let tool = ScriptTool::new("search", Vec::new());
    let policy = ScriptPolicy::new(vec![
        PolicyBehavior::Decision(ToolDecision::deny("no").unwrap()),
        PolicyBehavior::Decision(ToolDecision::deny("private long reason").unwrap()),
    ]);
    let driver = driver(
        Arc::clone(&tool),
        Some(policy_port(&policy)),
        config_with(
            Duration::from_secs(5),
            Duration::from_secs(10),
            MAX_JSON_BYTES,
            4,
        ),
    );
    let (suspensions, _suspension_rx, progress, _progress_rx) = channels();
    let exact = driver
        .run(
            invocation("search", 7, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_outcome(&exact, ToolResultOutcome::Denied, "no");

    let fallback = driver
        .run(
            invocation("search", 8, json!({})),
            deadline_after(Duration::from_secs(30)),
            CancellationToken::new(),
            &suspensions,
            &progress,
        )
        .await
        .unwrap();
    assert_outcome(&fallback, ToolResultOutcome::Denied, "tool");
    assert_eq!(tool.calls(), 0);
}
