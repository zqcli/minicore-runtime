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
        with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
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
        with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
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
        ModelDriverConfig::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
    assert_eq!(error.delivery(), DeliveryState::Started);
    let mut events = Vec::new();
    for index in 0..65 {
        let id = call_id(index);
        events.extend([
            Ok(start(id.clone(), "search")),
            Ok(args(id.clone(), "{}")),
            Ok(end(id)),
        ]);
    }
    events.push(Ok(finish(ModelFinishReason::ToolCalls)));
    let response = run_events(tool_request(), events, ModelDriverConfig::default())
        .await
        .unwrap();
    assert_eq!(response.parts().len(), 65);
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_text_and_reasoning_aggregate_in_first_occurrence_order() {
    let id = call_id(26);
    let response = run_events(
        tool_request(),
        vec![
            Ok(ModelEvent::text_delta("answer-")),
            Ok(start(id.clone(), "search")),
            Ok(ModelEvent::reasoning_delta("reason-")),
            Ok(ModelEvent::text_delta("continued")),
            Ok(args(id.clone(), "{}")),
            Ok(ModelEvent::reasoning_delta("continued")),
            Ok(end(id.clone())),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
    )
    .await
    .unwrap();

    assert!(matches!(
        response.parts(),
        [
            AssistantPart::Text(text),
            AssistantPart::ToolCall(call),
            AssistantPart::Reasoning(reasoning),
        ] if text.as_ref() == "answer-continued"
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
                Ok(ModelEvent::text_delta("provider message")),
                Ok(finish(reason)),
            ],
            with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
        )
        .await
        .unwrap();
        assert_eq!(response.finish_reason(), reason);
        assert_text(&response, "provider message");

        let error = run_events(
            request(),
            vec![
                Ok(ModelEvent::reasoning_delta("internal reason")),
                Ok(finish(reason)),
            ],
            with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
        assert_eq!(error.delivery(), DeliveryState::Started);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_tool_specs_allow_text_response_when_limit_is_one() {
    let model = ScriptModel::new(descriptor(), vec![Behavior::Events(text_success("done"))]);
    let driver = model.driver(&ModelDriverConfig::default());
    let (progress, _receiver) = progress_channel();
    let request = request_with(
        ReasoningPreference::High,
        vec![tool_spec("lookup"), tool_spec("search")],
        Some(64),
    );
    let response = driver
        .run(
            request,
            context(CancellationToken::new(), Duration::from_secs(30)),
            &progress,
        )
        .await
        .expect("registered specs do not restrict response shape");
    assert_text(&response, "done");
    assert_eq!(model.starts(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_tool_specs_allow_single_tool_call_when_limit_is_one() {
    let id = call_id(30);
    let request = request_with(
        ReasoningPreference::High,
        vec![tool_spec("lookup"), tool_spec("search")],
        Some(64),
    );
    let response = run_events(
        request,
        vec![
            Ok(start(id.clone(), "lookup")),
            Ok(args(id.clone(), "{\"query\":\"rust\"}")),
            Ok(end(id.clone())),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        ModelDriverConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(response.finish_reason(), ModelFinishReason::ToolCalls);
    assert!(matches!(
        response.parts(),
        [AssistantPart::ToolCall(call)]
            if call.tool_call_id() == &id
                && call.call_index() == 0
                && call.arguments() == &json!({"query": "rust"})
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_tool_specs_reject_second_tool_call_when_limit_is_one() {
    let first = call_id(31);
    let second = call_id(32);
    let request = request_with(
        ReasoningPreference::High,
        vec![tool_spec("lookup"), tool_spec("search")],
        Some(64),
    );
    let response = run_events(
        request,
        vec![
            Ok(start(first.clone(), "lookup")),
            Ok(args(first.clone(), "{}")),
            Ok(end(first)),
            Ok(start(second.clone(), "search")),
            Ok(args(second.clone(), "{}")),
            Ok(end(second)),
            Ok(finish(ModelFinishReason::ToolCalls)),
        ],
        ModelDriverConfig::default(),
    )
    .await
    .unwrap();
    assert_eq!(response.parts().len(), 2);
}
