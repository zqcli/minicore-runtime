use std::fmt::Debug;
use std::str::FromStr;

use minicore_runtime::{
    AssistantPart, DeliveryState, InteractionId, ModelEvent, ModelFinishReason, ModelId,
    ModelLimits, ModelMessage, ModelRequest, ModelResponse, ModelSelection, ProviderId,
    ReasoningPreference, SessionEvent, SessionEventKind, SessionId, SessionSnapshot, SessionStatus,
    SnapshotHistory, ToolCall, ToolCallId, ToolCallSummary, ToolName, ToolOutput, ToolResultStatus,
    ToolResultSummary, ToolSpec, TurnId, TurnSummary, Usage, UserAnswer, UserQuestion,
};

fn assert_json_round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + Eq + Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let decoded = serde_json::from_str::<T>(&json).unwrap();
    assert_eq!(&decoded, value);
}

fn assert_invalid<T>(value: serde_json::Value)
where
    T: serde::de::DeserializeOwned,
{
    assert!(
        serde_json::from_value::<T>(value).is_err(),
        "malformed JSON unexpectedly deserialized"
    );
}

fn call(id: &str, index: u32) -> ToolCall {
    ToolCall::new(
        ToolCallId::from_str(id).unwrap(),
        ToolName::from_str("read_file").unwrap(),
        serde_json::json!({"path": "README.md"}),
        index,
    )
    .unwrap()
}

#[test]
fn model_and_tool_dtos_round_trip_through_checked_ordinary_json() {
    let provider_id = ProviderId::from_str("openai").unwrap();
    let model_id = ModelId::from_str("responses/gpt-5").unwrap();
    let selection = ModelSelection::new(provider_id, model_id);
    let tool_name = ToolName::from_str("read_file").unwrap();
    let call_id = ToolCallId::from_str("provider/call:1!").unwrap();
    let tool_spec = ToolSpec::new(
        tool_name.clone(),
        "Read one text file",
        serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
    )
    .unwrap();
    let tool_call = ToolCall::new(
        call_id.clone(),
        tool_name.clone(),
        serde_json::json!({"path": "README.md"}),
        0,
    )
    .unwrap();
    let output = ToolOutput::new("file contents", false).unwrap();
    let interaction_id = InteractionId::new().unwrap();
    let question = UserQuestion::new(
        interaction_id,
        "Continue?",
        Some(vec!["yes".into(), "no".into()]),
    )
    .unwrap();
    let answer = UserAnswer::new("yes").unwrap();
    let call_summary = ToolCallSummary::new(call_id.clone(), tool_name.clone(), 0).unwrap();
    let result_summary = ToolResultSummary::new(call_id, ToolResultStatus::Succeeded).unwrap();

    let messages = vec![
        ModelMessage::system("You are a coding agent").unwrap(),
        ModelMessage::user("Read README.md").unwrap(),
        ModelMessage::assistant(vec![AssistantPart::ToolCall(tool_call.clone())]).unwrap(),
        ModelMessage::tool(tool_call.tool_call_id().clone(), output.clone()).unwrap(),
    ];
    let request = ModelRequest::new(
        selection,
        messages,
        vec![tool_spec.clone()],
        ModelLimits::new(Some(128_000), Some(8_192)).unwrap(),
        ReasoningPreference::Auto,
    )
    .unwrap();
    let response = ModelResponse::new(
        vec![AssistantPart::Text("done".into())],
        ModelFinishReason::Stop,
        Usage::new(12, 3, 0),
        DeliveryState::Delivered,
    )
    .unwrap();
    let events = vec![
        ModelEvent::TextDelta { delta: "do".into() },
        ModelEvent::ReasoningDelta {
            delta: "briefly".into(),
        },
    ];

    assert_eq!(tool_call.call_index(), 0);
    assert_eq!(call_summary.call_index(), 0);
    assert_eq!(question.interaction_id(), interaction_id);
    assert_json_round_trip(&tool_spec);
    assert_json_round_trip(&tool_call);
    assert_json_round_trip(&question);
    assert_json_round_trip(&answer);
    assert_json_round_trip(&call_summary);
    assert_json_round_trip(&result_summary);
    assert_json_round_trip(&request);
    assert_json_round_trip(&response);
    assert_json_round_trip(&events);
}

#[test]
fn constrained_dto_deserializers_reject_malformed_json_and_nested_invalid_values() {
    let interaction_id = InteractionId::new().unwrap();
    assert_invalid::<ToolSpec>(serde_json::json!({
        "name": "read_file",
        "description": "",
        "input_schema": {}
    }));
    assert_invalid::<ToolOutput>(serde_json::json!({
        "text": "bad\u{0001}text",
        "is_error": false
    }));
    assert_invalid::<UserQuestion>(serde_json::json!({
        "question": "Continue?",
        "choices": null
    }));
    assert_invalid::<UserQuestion>(serde_json::json!({
        "interaction_id": interaction_id.to_string(),
        "question": "",
        "choices": null
    }));
    assert_invalid::<UserAnswer>(serde_json::json!({"text": ""}));
    assert_invalid::<ModelLimits>(serde_json::json!({
        "context_window_tokens": 0,
        "max_output_tokens": 1
    }));
    assert_invalid::<ToolCall>(serde_json::json!({
        "tool_call_id": "call_1!",
        "name": "read_file",
        "arguments": [],
        "call_index": 0
    }));
    assert_invalid::<AssistantPart>(serde_json::json!({
        "type": "text",
        "data": ""
    }));
    assert_invalid::<ModelMessage>(serde_json::json!({
        "role": "user",
        "content": ""
    }));

    let selection = ModelSelection::new(
        ProviderId::from_str("openai").unwrap(),
        ModelId::from_str("responses/gpt-5").unwrap(),
    );
    let valid_request = ModelRequest::new(
        selection,
        vec![ModelMessage::user("hello").unwrap()],
        Vec::new(),
        ModelLimits::default(),
        ReasoningPreference::Auto,
    )
    .unwrap();
    let mut invalid_request = serde_json::to_value(valid_request).unwrap();
    invalid_request["messages"] = serde_json::json!([]);
    assert_invalid::<ModelRequest>(invalid_request);
    let mut nested_invalid_request = serde_json::to_value(
        ModelRequest::new(
            ModelSelection::new(
                ProviderId::from_str("openai").unwrap(),
                ModelId::from_str("responses/gpt-5").unwrap(),
            ),
            vec![ModelMessage::user("hello").unwrap()],
            Vec::new(),
            ModelLimits::default(),
            ReasoningPreference::Auto,
        )
        .unwrap(),
    )
    .unwrap();
    nested_invalid_request["messages"] = serde_json::json!([{
        "role": "assistant",
        "content": []
    }]);
    assert_invalid::<ModelRequest>(nested_invalid_request);

    let valid_response = ModelResponse::new(
        vec![AssistantPart::Text("done".into())],
        ModelFinishReason::Stop,
        Usage::default(),
        DeliveryState::Delivered,
    )
    .unwrap();
    let mut invalid_response = serde_json::to_value(valid_response).unwrap();
    invalid_response["parts"] = serde_json::json!([]);
    assert_invalid::<ModelResponse>(invalid_response);
    let nested_invalid_response = serde_json::json!({
        "parts": [{"type": "text", "data": ""}],
        "finish_reason": "stop",
        "usage": {"input_tokens": 0, "output_tokens": 0, "reasoning_tokens": 0},
        "delivery_state": "delivered"
    });
    assert_invalid::<ModelResponse>(nested_invalid_response);
}

#[test]
fn tool_calls_are_ordered_and_duplicate_ids_or_indices_are_rejected() {
    let first = call("call_1!", 0);
    let second = call("call_2!", 1);
    let assistant = ModelMessage::assistant(vec![
        AssistantPart::ToolCall(first.clone()),
        AssistantPart::ToolCall(second.clone()),
    ])
    .unwrap();
    let ModelMessage::Assistant(parts) = assistant else {
        panic!("expected assistant message");
    };
    assert_eq!(parts[0].as_tool_call().unwrap().call_index(), 0);
    assert_eq!(parts[1].as_tool_call().unwrap().call_index(), 1);

    let duplicate_id = ModelMessage::assistant(vec![
        AssistantPart::ToolCall(first.clone()),
        AssistantPart::ToolCall(call("call_1!", 1)),
    ]);
    assert!(duplicate_id.is_err());

    let duplicate_index = ModelMessage::assistant(vec![
        AssistantPart::ToolCall(first.clone()),
        AssistantPart::ToolCall(call("call_2!", 0)),
    ]);
    assert!(duplicate_index.is_err());

    assert!(
        ModelResponse::new(
            vec![
                AssistantPart::ToolCall(first.clone()),
                AssistantPart::ToolCall(call("call_2!", 0)),
            ],
            ModelFinishReason::ToolCalls,
            Usage::default(),
            DeliveryState::Delivered,
        )
        .is_err()
    );

    let duplicate_json = serde_json::json!({
        "role": "assistant",
        "content": [
            {"type": "tool_call", "data": {
                "tool_call_id": "call_1!", "name": "read_file", "arguments": {}, "call_index": 0
            }},
            {"type": "tool_call", "data": {
                "tool_call_id": "call_1!", "name": "read_file", "arguments": {}, "call_index": 1
            }}
        ]
    });
    assert_invalid::<ModelMessage>(duplicate_json);
}

#[test]
fn model_request_scopes_call_indices_and_ids_to_each_assistant_round() {
    let selection = ModelSelection::new(
        ProviderId::from_str("openai").unwrap(),
        ModelId::from_str("responses/gpt-5").unwrap(),
    );
    let request = ModelRequest::new(
        selection,
        vec![
            ModelMessage::assistant(vec![AssistantPart::ToolCall(call("round_1!", 0))]).unwrap(),
            ModelMessage::assistant(vec![AssistantPart::ToolCall(call("round_2!", 0))]).unwrap(),
        ],
        Vec::new(),
        ModelLimits::default(),
        ReasoningPreference::Auto,
    );
    assert!(request.is_ok());
}

#[test]
fn one_assistant_round_requires_contiguous_tool_call_indices_in_order() {
    assert!(
        ModelMessage::assistant(vec![
            AssistantPart::ToolCall(call("out_of_order_1!", 1)),
            AssistantPart::ToolCall(call("out_of_order_0!", 0)),
        ])
        .is_err()
    );
    assert!(
        ModelMessage::assistant(vec![
            AssistantPart::ToolCall(call("gap_0!", 0)),
            AssistantPart::ToolCall(call("gap_2!", 2)),
        ])
        .is_err()
    );
    assert!(
        ModelMessage::assistant(vec![
            AssistantPart::ToolCall(call("duplicate_index_0!", 0)),
            AssistantPart::ToolCall(call("duplicate_index_0b!", 0)),
        ])
        .is_err()
    );
}

#[test]
fn session_snapshot_has_checked_private_fields_and_a_complete_four_state_matrix() {
    let session_id = SessionId::new().unwrap();
    let turn_id = TurnId::new().unwrap();
    let interaction_id = InteractionId::new().unwrap();
    let question = UserQuestion::new(interaction_id, "Pick one", None).unwrap();
    let history = SnapshotHistory::new(None, None);

    let idle = SessionSnapshot::new(
        session_id,
        SessionStatus::Idle,
        None,
        None,
        Usage::default(),
        history.clone(),
        42,
    )
    .unwrap();
    assert_eq!(idle.session_id(), session_id);
    assert_eq!(idle.status(), SessionStatus::Idle);
    assert_eq!(idle.active_turn(), None);
    assert_eq!(idle.pending_question(), None);
    assert_eq!(idle.conversation_seq(), 42);

    let running = SessionSnapshot::new(
        session_id,
        SessionStatus::Running { turn_id },
        Some(TurnSummary::new(turn_id)),
        None,
        Usage::default(),
        history.clone(),
        0,
    )
    .unwrap();
    assert_eq!(running.active_turn().unwrap().turn_id, turn_id);

    let waiting = SessionSnapshot::new(
        session_id,
        SessionStatus::WaitingForInput {
            turn_id,
            interaction_id,
        },
        Some(TurnSummary::new(turn_id)),
        Some(question.clone()),
        Usage::default(),
        history.clone(),
        0,
    )
    .unwrap();
    assert_eq!(
        waiting.pending_question().unwrap().interaction_id(),
        interaction_id
    );

    let closing = SessionSnapshot::new(
        session_id,
        SessionStatus::Closing,
        Some(TurnSummary::new(turn_id)),
        None,
        Usage::default(),
        history.clone(),
        0,
    )
    .unwrap();
    assert_eq!(closing.status(), SessionStatus::Closing);

    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::Idle,
            Some(TurnSummary::new(turn_id)),
            None,
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::Idle,
            None,
            Some(question.clone()),
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::Running { turn_id },
            None,
            None,
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::Running { turn_id },
            Some(TurnSummary::new(TurnId::new().unwrap())),
            None,
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::Running { turn_id },
            Some(TurnSummary::new(turn_id)),
            Some(question.clone()),
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::WaitingForInput {
                turn_id,
                interaction_id,
            },
            None,
            Some(question.clone()),
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::WaitingForInput {
                turn_id,
                interaction_id,
            },
            Some(TurnSummary::new(turn_id)),
            None,
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::WaitingForInput {
                turn_id,
                interaction_id,
            },
            Some(TurnSummary::new(turn_id)),
            Some(UserQuestion::new(InteractionId::new().unwrap(), "wrong", None).unwrap()),
            Usage::default(),
            history.clone(),
            0,
        )
        .is_err()
    );
    assert!(
        SessionSnapshot::new(
            session_id,
            SessionStatus::Closing,
            Some(TurnSummary::new(turn_id)),
            Some(question),
            Usage::default(),
            history,
            0,
        )
        .is_err()
    );
}

#[test]
fn session_snapshot_deserialize_is_checked_and_event_json_is_canonical_and_safe() {
    let session_id = SessionId::new().unwrap();
    let turn_id = TurnId::new().unwrap();
    let interaction_id = InteractionId::new().unwrap();
    let wrong_interaction_id = InteractionId::new().unwrap();
    let question = UserQuestion::new(interaction_id, "Pick one", None).unwrap();
    let snapshot = SessionSnapshot::new(
        session_id,
        SessionStatus::WaitingForInput {
            turn_id,
            interaction_id,
        },
        Some(TurnSummary::new(turn_id)),
        Some(question.clone()),
        Usage::default(),
        SnapshotHistory::new(None, None),
        0,
    )
    .unwrap();
    assert_json_round_trip(&snapshot);

    let mut wrong_question_id = serde_json::to_value(&snapshot).unwrap();
    wrong_question_id["pending_question"]["interaction_id"] =
        serde_json::json!(wrong_interaction_id.to_string());
    assert_invalid::<SessionSnapshot>(wrong_question_id);

    let result = ToolResultSummary::new(
        ToolCallId::from_str("call_1!").unwrap(),
        ToolResultStatus::Succeeded,
    )
    .unwrap();
    let summary_json = serde_json::to_value(&result).unwrap();
    assert_eq!(
        summary_json,
        serde_json::json!({
            "tool_call_id": "call_1!",
            "status": "succeeded"
        })
    );
    let snapshot_event = SessionEvent::Snapshot(snapshot.clone());
    let snapshot_json = serde_json::to_value(&snapshot_event).unwrap();
    assert_eq!(
        snapshot_json,
        serde_json::json!({
            "type": "snapshot",
            "data": serde_json::to_value(&snapshot).unwrap()
        })
    );
    assert_eq!(snapshot_event.kind(), SessionEventKind::Snapshot);

    let secret = "ORIGINAL-TOOL-OUTPUT-SECRET";
    let safe_result = ToolResultSummary::new(
        ToolCallId::from_str("call_1!").unwrap(),
        ToolResultStatus::Succeeded,
    )
    .unwrap();
    let events = vec![
        SessionEvent::Snapshot(snapshot),
        SessionEvent::ToolFinished {
            turn_id,
            result: result.clone(),
        },
        SessionEvent::InputRequested { turn_id, question },
        SessionEvent::ResyncRequired,
        SessionEvent::Closed,
    ];
    let json = serde_json::to_string(&events).unwrap();
    assert!(!json.contains(secret));
    let input_json = serde_json::to_value(&events[2]).unwrap();
    assert!(input_json["data"].get("interaction_id").is_none());
    assert!(
        !serde_json::to_string(&safe_result)
            .unwrap()
            .contains(secret)
    );
    assert_eq!(events[1].kind(), SessionEventKind::ToolFinished);
    assert_eq!(events[2].kind(), SessionEventKind::InputRequested);
    assert_eq!(events[3].kind(), SessionEventKind::ResyncRequired);
    assert_eq!(events[4].kind(), SessionEventKind::Closed);
    assert_eq!(
        serde_json::from_str::<Vec<SessionEvent>>(&json).unwrap(),
        events
    );
}

#[test]
fn model_message_and_assistant_parts_expose_minimal_validation() {
    assert!(AssistantPart::Text(String::new()).validate().is_err());
    assert!(AssistantPart::Reasoning(String::new()).validate().is_err());
    assert!(ModelMessage::User(String::new()).validate().is_err());
    assert!(ModelMessage::Assistant(Vec::new()).validate().is_err());
}
