use super::*;

#[tokio::test]
async fn direct_and_constructed_deltas_enforce_normalized_byte_limits() {
    let limit = crate::model::MAX_MODEL_EVENT_TEXT_BYTES;
    let controls = "\0".repeat(limit / 3);
    let replacements = "�".repeat(limit / 3);
    for (raw, normalized, accepted) in [
        ("\r\n".repeat(40_000), "\n".repeat(40_000), true),
        ("\r\n".repeat(limit), "\n".repeat(limit), true),
        ("\r\n".repeat(limit + 1), "\n".repeat(limit + 1), false),
        (format!("{controls}a"), format!("{replacements}a"), true),
        (format!("{controls}aa"), format!("{replacements}aa"), false),
        ("a".repeat(limit + 1), "a".repeat(limit + 1), false),
    ] {
        for reasoning in [false, true] {
            let delta = BoundedText::new(&raw).unwrap();
            let (direct, constructed) = if reasoning {
                (
                    ModelEvent::ReasoningDelta { delta },
                    ModelEvent::reasoning_delta(&raw),
                )
            } else {
                (
                    ModelEvent::TextDelta { delta },
                    ModelEvent::text_delta(&raw),
                )
            };
            assert_eq!(constructed.is_ok(), accepted);
            assert_eq!(direct.validate().is_ok(), accepted);
            let direct_result = run_events(
                request(),
                vec![Ok(direct), Ok(finish(ModelFinishReason::Stop))],
                ModelDriverConfig::default(),
            )
            .await;
            if accepted {
                let constructed_result = run_events(
                    request(),
                    vec![
                        Ok(constructed.unwrap()),
                        Ok(finish(ModelFinishReason::Stop)),
                    ],
                    ModelDriverConfig::default(),
                )
                .await;
                assert_eq!(direct_result, constructed_result);
                let response = direct_result.unwrap();
                let text = match &response.parts()[0] {
                    AssistantPart::Text(text) => text.as_ref(),
                    AssistantPart::Reasoning(reasoning) => reasoning.text().unwrap(),
                    _ => panic!("expected model text"),
                };
                assert_eq!(text, normalized);
            } else {
                assert_eq!(
                    direct_result.unwrap_err().kind(),
                    ModelErrorKind::InvalidProviderResponse
                );
            }
        }
    }
}

#[tokio::test]
async fn model_text_is_normalized_and_empty_deltas_do_not_emit_progress() {
    let model = ScriptModel::new(
        descriptor(),
        vec![Behavior::Events(vec![
            Ok(ModelEvent::text_delta("").unwrap()),
            Ok(ModelEvent::reasoning_delta("").unwrap()),
            Ok(ModelEvent::TextDelta {
                delta: BoundedText::new("a\r\nb\rc\0\x1b\t").unwrap(),
            }),
            Ok(ModelEvent::reasoning_delta("r\r\n\x7f").unwrap()),
            Ok(start(call_id(1), "search")),
            Ok(args(call_id(1), "")),
            Ok(ModelEvent::ToolCallArgumentsDelta {
                tool_call_id: call_id(1),
                delta: BoundedText::new("{\r\n\"q\":\"a\x1bb\"}").unwrap(),
            }),
            Ok(end(call_id(1))),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ])],
    );
    let driver = model.driver(&ModelDriverConfig::default());
    let (progress, mut receiver) = progress_channel();
    let parent = CancellationToken::new();
    let response = driver
        .run(
            tool_request(),
            context(parent.clone(), Duration::from_secs(30)),
            &progress,
        )
        .await
        .unwrap();
    assert!(
        matches!(response.parts(), [AssistantPart::Text(text), AssistantPart::Reasoning(reason), AssistantPart::ToolCall(call)]
        if text.as_ref() == "a\nb\nc��\t" && reason.text() == Some("r\n�") && call.arguments() == &json!({"q": "a�b"}))
    );
    assert!(
        matches!(receiver.try_recv(), Ok(ModelDriverProgress::TextDelta(text)) if text.as_str() == "a\nb\nc��\t")
    );
    assert!(
        matches!(receiver.try_recv(), Ok(ModelDriverProgress::ReasoningDelta(text)) if text.as_str() == "r\n�")
    );
    assert!(receiver.try_recv().is_err());
    assert!(!parent.is_cancelled());
}

#[tokio::test(flavor = "current_thread")]
async fn assembles_text_reasoning_tool_usage_and_preserves_first_slot_order() {
    let id = call_id(1);
    let events = vec![
        Ok(ModelEvent::reasoning_delta("reason").unwrap()),
        Ok(start(id.clone(), "search")),
        Ok(ModelEvent::text_delta("answer").unwrap()),
        Ok(args(id.clone(), "{\"q\":")),
        Ok(args(id.clone(), "\"rust\"}")),
        Ok(end(id.clone())),
        Ok(ModelEvent::Usage {
            usage: Usage::new(7, 5, 3),
        }),
        Ok(finish(ModelFinishReason::ToolCalls)),
    ];
    let model = ScriptModel::new(descriptor(), vec![Behavior::Events(events)]);
    let driver = model.driver(&with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)));
    let (progress, mut progress_rx) = progress_channel();
    let response = driver
        .run(
            tool_request(),
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
        )
        .await
        .unwrap();

    assert_eq!(response.finish_reason(), ModelFinishReason::ToolCalls);
    assert_eq!(response.usage(), Some(&Usage::new(7, 5, 3)));
    assert!(matches!(
        response.parts(),
        [
            AssistantPart::Reasoning(reasoning),
            AssistantPart::ToolCall(call),
            AssistantPart::Text(text),
        ] if reasoning.text() == Some("reason")
            && call.tool_call_id() == &id
            && call.name().as_str() == "search"
            && call.arguments() == &json!({"q": "rust"})
            && call.call_index() == 0
            && text.as_ref() == "answer"
    ));
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ModelDriverProgress::ReasoningDelta(delta)) if delta.as_str() == "reason"
    ));
    assert!(matches!(
        progress_rx.try_recv(),
        Ok(ModelDriverProgress::TextDelta(delta)) if delta.as_str() == "answer"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn absent_usage_defaults_and_refusal_requires_text() {
    let response = run_events(
        request(),
        text_success("ok"),
        with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
    )
    .await
    .unwrap();
    assert_eq!(response.usage(), Some(&Usage::default()));

    let error = run_events(
        request(),
        vec![Ok(finish(ModelFinishReason::Refused))],
        with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
}

#[tokio::test(flavor = "current_thread")]
async fn tool_call_indices_follow_start_order_not_request_order() {
    let first = call_id(6);
    let second = call_id(7);
    let request = request_with(
        ReasoningPreference::High,
        vec![tool_spec("lookup"), tool_spec("search")],
        Some(64),
    );
    let response = run_events(
        request,
        vec![
            Ok(start(first.clone(), "search")),
            Ok(args(first, "{}")),
            Ok(end(call_id(6))),
            Ok(start(second.clone(), "lookup")),
            Ok(args(second, "{}")),
            Ok(end(call_id(7))),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
    )
    .await
    .unwrap();
    assert!(matches!(
        response.parts(),
        [AssistantPart::ToolCall(first), AssistantPart::ToolCall(second)]
            if first.name().as_str() == "search"
                && first.call_index() == 0
                && second.name().as_str() == "lookup"
                && second.call_index() == 1
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn rejects_every_malformed_stream_sequence() {
    let id = call_id(2);
    let other = call_id(3);
    let usage = ModelEvent::Usage {
        usage: Usage::default(),
    };
    let cases = vec![
        (
            "arguments before start",
            tool_request(),
            vec![Ok(args(id.clone(), "{}"))],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "end before start",
            tool_request(),
            vec![Ok(end(id.clone()))],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "duplicate id",
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(start(id.clone(), "search")),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "end mismatch",
            tool_request(),
            vec![Ok(start(id.clone(), "search")), Ok(end(other.clone()))],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "delta after end",
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), "{}")),
                Ok(end(id.clone())),
                Ok(args(id.clone(), "{}")),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "duplicate end",
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), "{}")),
                Ok(end(id.clone())),
                Ok(end(id.clone())),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "finish with open call",
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(finish(ModelFinishReason::ToolCalls)),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "duplicate usage",
            request(),
            vec![Ok(usage.clone()), Ok(usage.clone())],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "duplicate finish",
            request(),
            vec![
                Ok(ModelEvent::text_delta("x").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "post finish event",
            request(),
            vec![
                Ok(ModelEvent::text_delta("x").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
                Ok(usage.clone()),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "eof before finish",
            request(),
            vec![Ok(ModelEvent::text_delta("x").unwrap())],
            ModelErrorKind::IncompleteResponse,
        ),
        (
            "no parts",
            request(),
            vec![Ok(finish(ModelFinishReason::Stop))],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "unexpected tool",
            tool_request(),
            vec![Ok(start(id.clone(), "other"))],
            ModelErrorKind::UnexpectedToolCall,
        ),
        (
            "bad json",
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), "{")),
                Ok(end(id.clone())),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "empty arguments",
            tool_request(),
            vec![Ok(start(id.clone(), "search")), Ok(end(id.clone()))],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "nonobject json",
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), "[]")),
                Ok(end(id.clone())),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "tool reason without tools",
            request(),
            vec![
                Ok(ModelEvent::text_delta("x").unwrap()),
                Ok(finish(ModelFinishReason::ToolCalls)),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "stop reason with tools",
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), "{}")),
                Ok(end(id.clone())),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
    ];

    for (name, request, events, expected) in cases {
        let error = run_events(
            request,
            events,
            with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
        )
        .await
        .expect_err(name);
        assert_eq!(error.kind(), expected, "{name}");
        assert_eq!(error.delivery(), DeliveryState::Started, "{name}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn tool_arguments_use_centralized_json_depth_and_node_validation() {
    let nested = format!(
        "{}{{}}{}",
        "{\"x\":".repeat(crate::value::MAX_JSON_DEPTH + 1),
        "}".repeat(crate::value::MAX_JSON_DEPTH + 1),
    );
    let many_nodes = serde_json::to_string(&json!({
        "items": vec![Value::Null; crate::value::MAX_JSON_NODES]
    }))
    .unwrap();
    for arguments in [nested, many_nodes] {
        let id = call_id(5);
        let error = run_events(
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), &arguments)),
                Ok(end(id)),
            ],
            with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_text_reasoning_and_arguments_enforce_exact_semantic_boundaries() {
    let limits = LoopLimits {
        max_model_text_bytes: 3,
        max_model_reasoning_bytes: 3,
        max_tool_arguments_bytes: 7,
        ..LoopLimits::default()
    };
    let kernel = with_limits(limits);

    assert!(
        run_events(
            request(),
            vec![
                Ok(ModelEvent::text_delta("abc").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            kernel.clone(),
        )
        .await
        .is_ok()
    );
    assert_eq!(
        run_events(
            request(),
            vec![
                Ok(ModelEvent::text_delta("abcd").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            kernel.clone(),
        )
        .await
        .unwrap_err()
        .kind(),
        ModelErrorKind::InvalidProviderResponse
    );
    assert!(
        run_events(
            request(),
            vec![
                Ok(ModelEvent::reasoning_delta("abc").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            kernel.clone(),
        )
        .await
        .is_ok()
    );
    assert_eq!(
        run_events(
            request(),
            vec![
                Ok(ModelEvent::reasoning_delta("abcd").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            kernel.clone(),
        )
        .await
        .unwrap_err()
        .kind(),
        ModelErrorKind::InvalidProviderResponse
    );

    let id = call_id(4);
    assert!(
        run_events(
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), "{\"a\":1}")),
                Ok(end(id.clone())),
                Ok(finish(ModelFinishReason::ToolCalls)),
            ],
            kernel.clone(),
        )
        .await
        .is_ok()
    );
    assert_eq!(
        run_events(
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), "{\"a\":10}")),
                Ok(end(id)),
                Ok(finish(ModelFinishReason::ToolCalls)),
            ],
            kernel,
        )
        .await
        .unwrap_err()
        .kind(),
        ModelErrorKind::InvalidProviderResponse
    );
}

fn replay_event() -> ModelEvent {
    ModelEvent::ProviderReplay {
        replay: crate::model::ProviderReplay::new("test/v1", json!({"opaque": "secret"})).unwrap(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn replay_is_retained_once_never_forwarded_and_requires_finish() {
    for with_text in [false, true] {
        let mut events = vec![Ok(replay_event())];
        if with_text {
            events.push(Ok(ModelEvent::text_delta("answer").unwrap()));
        }
        events.push(Ok(finish(ModelFinishReason::Stop)));
        let model = ScriptModel::new(descriptor(), vec![Behavior::Events(events)]);
        let driver = model.driver(&with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)));
        let (progress, mut receiver) = progress_channel();
        let response = driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
            .unwrap();
        assert_eq!(
            response.provider_replay().unwrap().payload(),
            &json!({"opaque":"secret"})
        );
        if with_text {
            assert!(matches!(
                receiver.try_recv(),
                Ok(ModelDriverProgress::TextDelta(_))
            ));
        }
        assert!(receiver.try_recv().is_err());
    }
    for (events, kind) in [
        (vec![Ok(replay_event())], ModelErrorKind::IncompleteResponse),
        (
            vec![
                Ok(replay_event()),
                Ok(replay_event()),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            vec![
                Ok(ModelEvent::text_delta("ok").unwrap()),
                Ok(finish(ModelFinishReason::Stop)),
                Ok(replay_event()),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
    ] {
        let error = run_events(
            request(),
            events,
            with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), kind);
    }
}
