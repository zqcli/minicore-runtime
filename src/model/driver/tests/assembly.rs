use super::*;

#[tokio::test]
async fn eof_delivery_depends_on_whether_an_event_was_observed() {
    for (events, delivery) in [
        (vec![], DeliveryState::Unknown),
        (
            vec![Ok(ModelEvent::text_delta("partial"))],
            DeliveryState::Started,
        ),
    ] {
        let model = ScriptModel::new(descriptor(), vec![Behavior::Events(events)]);
        let driver = model.driver(&ModelDriverConfig::default());
        let (progress, _receiver) = progress_channel();
        let error = driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
        assert_eq!(error.delivery(), delivery);
        assert_eq!(*error.retry_hint(), RetryHint::Never);
        assert_eq!(model.starts(), 1);
    }
}

#[test]
fn split_crlf_matches_whole_text_in_each_channel_and_progress() {
    let raw = "é\r\nA\rB\r\r\n\tend\r";
    let expected = "é\nA\nB\n\n\tend\n";
    assert_eq!(crate::value::normalize_model_text(raw), expected);
    for split in (0..=raw.len()).filter(|&split| raw.is_char_boundary(split)) {
        for direct in [false, true] {
            let mut assembler = Assembler::new(&[]);
            let mut text = String::new();
            let mut reasoning = String::new();
            // Empty deltas and the other channel must not consume a pending CR.
            for (index, chunk) in [&raw[..split], "", &raw[split..]].into_iter().enumerate() {
                let events = if direct {
                    [
                        ModelEvent::TextDelta {
                            delta: Text::new(chunk),
                        },
                        ModelEvent::ReasoningDelta {
                            delta: Text::new(chunk),
                        },
                    ]
                } else {
                    [
                        ModelEvent::text_delta(chunk),
                        ModelEvent::reasoning_delta(chunk),
                    ]
                };
                for event in events {
                    match assembler.push(event).unwrap() {
                        Some(ModelDriverProgress::TextDelta(delta)) => {
                            assert!(!delta.is_empty());
                            text.push_str(delta.as_str());
                        }
                        Some(ModelDriverProgress::ReasoningDelta(delta)) => {
                            assert!(!delta.is_empty());
                            reasoning.push_str(delta.as_str());
                        }
                        None => {}
                    }
                }
                if index == 0 {
                    // A trailing CR is emitted immediately, not buffered until the next event.
                    assert_eq!(text, crate::value::normalize_model_text(chunk));
                    assert_eq!(reasoning, text);
                }
            }
            assert_eq!(text, expected, "split={split}, direct={direct}");
            assert_eq!(reasoning, expected, "split={split}, direct={direct}");
            assembler.push(finish(ModelFinishReason::Stop)).unwrap();
            let response = assembler.finish().unwrap();
            assert!(
                matches!(response.parts(), [AssistantPart::Text(text), AssistantPart::Reasoning(reasoning)]
                if text.as_ref() == expected && reasoning.text() == Some(expected))
            );
        }
    }
}

#[tokio::test]
async fn direct_and_constructed_deltas_enforce_normalized_byte_limits() {
    let limit = 64 * 1024;
    for (raw, normalized) in [
        ("\0".repeat(limit), " ".repeat(limit)),
        ("\x1b".repeat(limit), " ".repeat(limit)),
        ("\u{85}".repeat(limit / 2), " ".repeat(limit / 2)),
        ("\r\n".repeat(limit / 2), "\n".repeat(limit / 2)),
        ("a".repeat(limit), "a".repeat(limit)),
        ("\0".repeat(limit + 1), " ".repeat(limit + 1)),
        ("\r\n".repeat(limit / 2 + 1), "\n".repeat(limit / 2 + 1)),
        ("a".repeat(limit + 1), "a".repeat(limit + 1)),
    ] {
        assert!(normalized.len() <= raw.len());
        for reasoning in [false, true] {
            let delta = Text::new(&raw);
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
            assert_eq!(constructed, direct);
            let direct_result = run_events(
                request(),
                vec![Ok(direct), Ok(finish(ModelFinishReason::Stop))],
                ModelDriverConfig::default(),
            )
            .await;
            let constructed_result = run_events(
                request(),
                vec![Ok(constructed), Ok(finish(ModelFinishReason::Stop))],
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
        }
    }
}

#[tokio::test]
async fn model_text_is_normalized_and_empty_deltas_do_not_emit_progress() {
    let model = ScriptModel::new(
        descriptor(),
        vec![Behavior::Events(vec![
            Ok(ModelEvent::text_delta("")),
            Ok(ModelEvent::reasoning_delta("")),
            Ok(ModelEvent::TextDelta {
                delta: Text::new("a\r\nb\rc\0\x1b\t"),
            }),
            Ok(ModelEvent::reasoning_delta("r\r\n\x7f")),
            Ok(start(call_id(1), "search")),
            Ok(args(call_id(1), "")),
            Ok(ModelEvent::ToolCallArgumentsDelta {
                tool_call_id: call_id(1),
                delta: Text::new("{\r\n\"q\":\"a\x1bb\"}"),
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
        if text.as_ref() == "a\nb\nc  \t" && reason.text() == Some("r\n ") && call.arguments() == &json!({"q": "a b"}))
    );
    assert!(
        matches!(receiver.try_recv(), Ok(ModelDriverProgress::TextDelta(text)) if text.as_str() == "a\nb\nc  \t")
    );
    assert!(
        matches!(receiver.try_recv(), Ok(ModelDriverProgress::ReasoningDelta(text)) if text.as_str() == "r\n ")
    );
    assert!(receiver.try_recv().is_err());
    assert!(!parent.is_cancelled());
}

#[tokio::test(flavor = "current_thread")]
async fn assembles_text_reasoning_tool_usage_and_preserves_first_slot_order() {
    let id = call_id(1);
    let events = vec![
        Ok(ModelEvent::reasoning_delta("reason")),
        Ok(start(id.clone(), "search")),
        Ok(ModelEvent::text_delta("answer")),
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
                Ok(ModelEvent::text_delta("x")),
                Ok(finish(ModelFinishReason::Stop)),
                Ok(finish(ModelFinishReason::Stop)),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "post finish event",
            request(),
            vec![
                Ok(ModelEvent::text_delta("x")),
                Ok(finish(ModelFinishReason::Stop)),
                Ok(usage.clone()),
            ],
            ModelErrorKind::InvalidProviderResponse,
        ),
        (
            "eof before finish",
            request(),
            vec![Ok(ModelEvent::text_delta("x"))],
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
                Ok(ModelEvent::text_delta("x")),
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
        "{}0{}",
        "{\"x\":".repeat(crate::value::MAX_JSON_NESTING + 1),
        "}".repeat(crate::value::MAX_JSON_NESTING + 1)
    );
    let id = call_id(5);
    let error = run_events(
        tool_request(),
        vec![
            Ok(start(id.clone(), "search")),
            Ok(args(id.clone(), &nested)),
            Ok(end(id.clone())),
        ],
        ModelDriverConfig::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
    for containers in [33, crate::value::MAX_JSON_NESTING] {
        let nested = format!(
            "{}0{}",
            "{\"x\":".repeat(containers),
            "}".repeat(containers)
        );
        let response = run_events(
            tool_request(),
            vec![
                Ok(start(id.clone(), "search")),
                Ok(args(id.clone(), &nested)),
                Ok(end(id.clone())),
                Ok(finish(ModelFinishReason::ToolCalls)),
            ],
            ModelDriverConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            response.parts()[0].as_tool_call().unwrap().arguments(),
            &serde_json::from_str::<Value>(&nested).unwrap()
        );
    }
    let arguments = json!({"items": vec![Value::Null; 4097]});
    let response = run_events(
        tool_request(),
        vec![
            Ok(start(id.clone(), "search")),
            Ok(args(id.clone(), &arguments.to_string())),
            Ok(end(id)),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        ModelDriverConfig::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        response.parts()[0].as_tool_call().unwrap().arguments(),
        &arguments
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_text_reasoning_and_arguments_enforce_exact_semantic_boundaries() {
    let text = "x".repeat(262_145);
    let arguments = json!({"large": "a".repeat(65_537)});
    let id = call_id(6);
    let response = run_events(
        tool_request(),
        vec![
            Ok(ModelEvent::text_delta(&text[..131_072])),
            Ok(ModelEvent::reasoning_delta(&text[..131_072])),
            Ok(start(id.clone(), "search")),
            Ok(args(id.clone(), &arguments.to_string())),
            Ok(end(id)),
            Ok(ModelEvent::text_delta(&text[131_072..])),
            Ok(ModelEvent::reasoning_delta(&text[131_072..])),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        ModelDriverConfig::default(),
    )
    .await
    .unwrap();
    assert!(
        matches!(response.parts(), [AssistantPart::Text(actual), AssistantPart::Reasoning(reasoning), AssistantPart::ToolCall(call)]
        if actual.as_ref() == text && reasoning.text() == Some(text.as_str()) && call.arguments() == &arguments)
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
            events.push(Ok(ModelEvent::text_delta("answer")));
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
                Ok(ModelEvent::text_delta("ok")),
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
