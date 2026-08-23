use super::*;

#[tokio::test(flavor = "current_thread")]
async fn complete_tool_response_accepts_unknown_finish_reason() {
    let id = call_id(20);
    let response = run_events(
        tool_request(),
        vec![
            Ok(start(id.clone(), "search")),
            Ok(args(id.clone(), "{\"query\":\"rust\"}")),
            Ok(end(id.clone())),
            Ok(finish(ModelFinishReason::Unknown)),
        ],
        kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()),
    )
    .await
    .unwrap();

    assert_eq!(response.finish_reason(), ModelFinishReason::Unknown);
    assert!(matches!(
        response.parts(),
        [AssistantPart::ToolCall(call)]
            if call.tool_call_id() == &id
                && call.call_index() == 0
                && call.arguments() == &json!({"query": "rust"})
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn interleaved_open_tools_allow_out_of_order_matching_ends() {
    let first = call_id(21);
    let second = call_id(22);
    let request = request_with(
        ReasoningPreference::High,
        vec![tool_spec("lookup"), tool_spec("search")],
        Some(64),
    );
    let response = run_events(
        request,
        vec![
            Ok(start(first.clone(), "search")),
            Ok(start(second.clone(), "lookup")),
            Ok(args(first.clone(), "{\"first\":")),
            Ok(args(second.clone(), "{\"second\":")),
            Ok(args(first.clone(), "1}")),
            Ok(args(second.clone(), "2}")),
            Ok(end(second.clone())),
            Ok(end(first.clone())),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()),
    )
    .await
    .unwrap();

    assert!(matches!(
        response.parts(),
        [AssistantPart::ToolCall(first_call), AssistantPart::ToolCall(second_call)]
            if first_call.tool_call_id() == &first
                && first_call.call_index() == 0
                && first_call.arguments() == &json!({"first": 1})
                && second_call.tool_call_id() == &second
                && second_call.call_index() == 1
                && second_call.arguments() == &json!({"second": 2})
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn eof_with_open_tool_and_provider_tool_count_overflow_are_rejected() {
    let open = call_id(23);
    let error = run_events(
        tool_request(),
        vec![Ok(start(open.clone(), "search")), Ok(args(open, "{}"))],
        kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
    assert_eq!(error.delivery(), DeliveryState::Started);

    let limits = SemanticLimits {
        max_tool_count: 1,
        ..SemanticLimits::default()
    };
    let first = call_id(24);
    let second = call_id(25);
    let error = run_events(
        tool_request(),
        vec![
            Ok(start(first.clone(), "search")),
            Ok(args(first.clone(), "{}")),
            Ok(end(first)),
            Ok(start(second, "search")),
        ],
        limits_kernel(limits),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
    assert_eq!(error.delivery(), DeliveryState::Started);
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_text_and_reasoning_aggregate_in_first_occurrence_order() {
    let id = call_id(26);
    let response = run_events(
        tool_request(),
        vec![
            Ok(ModelEvent::text_delta("answer-").unwrap()),
            Ok(start(id.clone(), "search")),
            Ok(ModelEvent::reasoning_delta("reason-").unwrap()),
            Ok(ModelEvent::text_delta("continued").unwrap()),
            Ok(args(id.clone(), "{}")),
            Ok(ModelEvent::reasoning_delta("continued").unwrap()),
            Ok(end(id.clone())),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()),
    )
    .await
    .unwrap();

    assert!(matches!(
        response.parts(),
        [
            AssistantPart::Text(text),
            AssistantPart::ToolCall(call),
            AssistantPart::Reasoning(reasoning),
        ] if text == "answer-continued"
            && call.tool_call_id() == &id
            && reasoning.text() == Some("reason-continued")
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn refused_and_filtered_require_nonempty_text_not_reasoning_only() {
    for reason in [
        ModelFinishReason::Refused,
        ModelFinishReason::ContentFiltered,
    ] {
        let response = run_events(
            request(),
            vec![
                Ok(ModelEvent::text_delta("provider message").unwrap()),
                Ok(finish(reason)),
            ],
            kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(response.finish_reason(), reason);
        assert_text(&response, "provider message");

        let error = run_events(
            request(),
            vec![
                Ok(ModelEvent::reasoning_delta("internal reason").unwrap()),
                Ok(finish(reason)),
            ],
            kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
        assert_eq!(error.delivery(), DeliveryState::Started);
    }
}
