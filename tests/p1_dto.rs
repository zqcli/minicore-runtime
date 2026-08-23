use std::fmt::Debug;
use std::str::FromStr;

use minicore_runtime::model::{
    AssistantPart, ModelFinishReason, ModelId, ModelLimits, ModelMessage, ModelRequest,
    ModelResponse, ModelSelection, ProviderId, Usage,
};
use minicore_runtime::tools::{
    ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest, ToolInvocation, ToolName, ToolOutput,
    ToolProgress, ToolResultOutcome, ToolSet, ToolSpec,
};
use minicore_runtime::value::{MAX_JSON_BYTES, MAX_JSON_DEPTH, MAX_JSON_NODES, MAX_TEXT_BYTES};
use minicore_runtime::{BoundedText, SessionId, SessionInstanceId, ToolCallId, TurnId};
use serde_json::json;

fn assert_json_round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + Eq + Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let decoded = serde_json::from_str::<T>(&json).unwrap();
    assert_eq!(&decoded, value);
}

fn id_values() -> (SessionId, SessionInstanceId, TurnId, ToolCallId) {
    (
        "ses_00000000000000000000000000000001".parse().unwrap(),
        "ins_00000000000000000000000000000001".parse().unwrap(),
        "trn_00000000000000000000000000000001".parse().unwrap(),
        "call_00000000000000000000000000000001".parse().unwrap(),
    )
}

#[test]
fn final_tool_dtos_round_trip_and_redact_payloads() {
    let (session_id, instance_id, turn_id, call_id) = id_values();
    let name = ToolName::from_str("read_file").unwrap();
    let spec = ToolSpec::new(name.clone(), "Read one file", json!({"type": "object"})).unwrap();
    let invocation = ToolInvocation::new(
        session_id,
        instance_id,
        turn_id,
        call_id,
        name,
        json!({"secret": "do-not-print"}),
    )
    .unwrap();
    let output = ToolOutput::new("safe output").unwrap();
    assert_json_round_trip(&spec);
    assert_json_round_trip(&invocation);
    assert_json_round_trip(&output);
    assert_eq!(
        serde_json::to_value(&output).unwrap(),
        json!({"content": "safe output"})
    );
    assert!(!format!("{invocation:?}").contains("do-not-print"));
    assert!(!format!("{output:?}").contains("safe output"));
    assert!(serde_json::from_value::<ToolOutput>(json!({"content": "x", "extra": true})).is_err());
}

#[test]
fn public_model_tool_wire_has_content_and_outcome_only() {
    let (_, _, _, call_id) = id_values();
    let message = ModelMessage::tool_with_outcome(
        call_id,
        ToolOutput::new("visible result").unwrap(),
        ToolResultOutcome::Success,
    )
    .unwrap();
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(
        value,
        json!({
            "role": "tool",
            "content": {
                "tool_call_id": "call_00000000000000000000000000000001",
                "output": {"content": "visible result"},
                "outcome": "success"
            }
        })
    );
    assert!(value["content"].get("text").is_none());
    assert!(value["content"]["output"].get("is_error").is_none());
    assert!(
        serde_json::from_value::<ModelMessage>(json!({
            "role": "tool",
            "content": {
                "tool_call_id": "call_00000000000000000000000000000001",
                "output": {"content": "visible result"},
                "outcome": "success",
                "extra": true
            }
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ModelMessage>(json!({
            "role": "tool",
            "content": {
                "tool_call_id": "call_00000000000000000000000000000001",
                "output": {"text": "legacy", "is_error": true},
                "outcome": "failed"
            }
        }))
        .is_err()
    );

    let failed = ModelMessage::tool_with_outcome(
        "call_00000000000000000000000000000001".parse().unwrap(),
        ToolOutput::new("failed result").unwrap(),
        ToolResultOutcome::Failed,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(failed).unwrap()["content"]["outcome"],
        "failed"
    );
}

fn object_json_exact_bytes(target: usize) -> serde_json::Value {
    let length = target.checked_sub(14).expect("object JSON key overhead");
    let value = json!({"padding": "x".repeat(length)});
    assert_eq!(serde_json::to_vec(&value).unwrap().len(), target);
    value
}

fn nested_object(depth: usize) -> serde_json::Value {
    let mut value = json!({});
    for _ in 0..depth {
        value = json!({"nested": value});
    }
    value
}

fn object_with_nodes(nodes: usize) -> serde_json::Value {
    json!({"items": vec![serde_json::Value::Null; nodes - 2]})
}

#[test]
fn tool_schema_and_invocation_share_json_byte_depth_and_node_bounds() {
    let (_, instance_id, turn_id, call_id) = id_values();
    let name: ToolName = "bounded".parse().unwrap();
    let make_invocation = |arguments| {
        ToolInvocation::new(
            id_values().0,
            instance_id,
            turn_id,
            call_id.clone(),
            name.clone(),
            arguments,
        )
    };

    let exact_bytes = object_json_exact_bytes(MAX_JSON_BYTES);
    assert!(ToolSpec::new(name.clone(), "schema", exact_bytes.clone()).is_ok());
    assert!(
        ToolSpec::new(
            name.clone(),
            "schema",
            object_json_exact_bytes(MAX_JSON_BYTES + 1),
        )
        .is_err()
    );
    assert!(make_invocation(exact_bytes).is_ok());
    assert!(make_invocation(object_json_exact_bytes(MAX_JSON_BYTES + 1)).is_err());

    let exact_depth = nested_object(MAX_JSON_DEPTH);
    assert!(ToolSpec::new(name.clone(), "schema", exact_depth.clone()).is_ok());
    assert!(ToolSpec::new(name.clone(), "schema", nested_object(MAX_JSON_DEPTH + 1)).is_err());
    assert!(make_invocation(exact_depth).is_ok());
    assert!(make_invocation(nested_object(MAX_JSON_DEPTH + 1)).is_err());

    let exact_nodes = object_with_nodes(MAX_JSON_NODES);
    assert!(ToolSpec::new(name.clone(), "schema", exact_nodes.clone()).is_ok());
    assert!(
        ToolSpec::new(
            name.clone(),
            "schema",
            object_with_nodes(MAX_JSON_NODES + 1)
        )
        .is_err()
    );
    assert!(make_invocation(exact_nodes).is_ok());
    assert!(make_invocation(object_with_nodes(MAX_JSON_NODES + 1)).is_err());
}

#[test]
fn tool_spec_debug_redacts_description_and_schema_payload() {
    let spec = ToolSpec::new(
        "secret_tool".parse().unwrap(),
        "description-secret-value",
        json!({
            "secret_property_name": {
                "description": "schema-secret-value",
                "default": "schema-secret-default"
            }
        }),
    )
    .unwrap();
    let debug = format!("{spec:?}");
    assert!(debug.contains("secret_tool"));
    assert!(debug.contains("description_bytes: 24"));
    assert!(debug.contains("schema_object_keys: 1"));
    let schema_bytes = serde_json::to_vec(spec.input_schema()).unwrap().len();
    assert!(debug.contains(&format!("schema_bytes: {schema_bytes}")));
    for secret in [
        "description-secret-value",
        "secret_property_name",
        "schema-secret-value",
        "schema-secret-default",
    ] {
        assert!(!debug.contains(secret), "debug leaked {secret}");
    }
}

#[test]
fn tool_output_and_input_request_use_exact_boundary_values() {
    assert!(ToolOutput::new("x".repeat(MAX_TEXT_BYTES)).is_ok());
    assert!(ToolOutput::new("x".repeat(MAX_TEXT_BYTES + 1)).is_err());

    let exact_choices = (0..32)
        .map(|_| BoundedText::new("choice").unwrap())
        .collect();
    assert!(
        ToolInputRequest::new("prompt", exact_choices, ToolInputAnswerKind::SingleChoice,).is_ok()
    );
    let too_many_choices = (0..33)
        .map(|_| BoundedText::new("choice").unwrap())
        .collect();
    assert!(
        ToolInputRequest::new(
            "prompt",
            too_many_choices,
            ToolInputAnswerKind::SingleChoice,
        )
        .is_err()
    );
    assert!(
        ToolInputRequest::new(
            "x".repeat(8_192),
            vec![BoundedText::new("choice").unwrap()],
            ToolInputAnswerKind::SingleChoice,
        )
        .is_ok()
    );
    assert!(
        ToolInputRequest::new(
            "x".repeat(8_193),
            vec![BoundedText::new("choice").unwrap()],
            ToolInputAnswerKind::SingleChoice,
        )
        .is_err()
    );
    assert!(
        ToolInputRequest::new(
            "prompt",
            vec![BoundedText::new("x".repeat(1_024)).unwrap()],
            ToolInputAnswerKind::SingleChoice,
        )
        .is_ok()
    );
    assert!(
        ToolInputRequest::new(
            "prompt",
            vec![BoundedText::new("x".repeat(1_025)).unwrap()],
            ToolInputAnswerKind::SingleChoice,
        )
        .is_err()
    );
}

#[test]
fn input_request_and_answer_validation_is_strict() {
    let request = ToolInputRequest::new(
        "Choose a file",
        vec![
            BoundedText::new("first").unwrap(),
            BoundedText::new("second").unwrap(),
        ],
        ToolInputAnswerKind::SingleChoice,
    )
    .unwrap();
    assert_json_round_trip(&request);
    let answer = ToolInputAnswer::Choice { index: 1 };
    assert_json_round_trip(&answer);
    assert!(answer.validate(&request).is_ok());
    assert!(
        ToolInputAnswer::Choice { index: 2 }
            .validate(&request)
            .is_err()
    );
    assert!(
        ToolInputAnswer::Text(BoundedText::new("answer").unwrap())
            .validate(&request)
            .is_err()
    );

    for invalid in [
        json!({"prompt": "x", "choices": [], "answer_kind": "single_choice", "extra": 1}),
        json!({"prompt": 1, "choices": [], "answer_kind": "text"}),
        json!({"prompt": "", "choices": [], "answer_kind": "text"}),
        json!({"prompt": "bad\u{0001}", "choices": [], "answer_kind": "text"}),
        json!({"prompt": "x", "choices": [""], "answer_kind": "single_choice"}),
        json!({"prompt": "x", "choices": ["x".repeat(1_025)], "answer_kind": "single_choice"}),
        json!({"prompt": "x".repeat(8_193), "choices": [], "answer_kind": "text"}),
    ] {
        assert!(serde_json::from_value::<ToolInputRequest>(invalid).is_err());
    }
    assert!(
        serde_json::from_value::<ToolInputAnswer>(json!({
            "kind": "choice",
            "data": {"index": 0, "extra": true}
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolInputAnswer>(json!({
            "kind": "choice",
            "data": "wrong"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolInputAnswer>(json!({
            "kind": "text",
            "data": ""
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolInputAnswer>(json!({
            "kind": "text",
            "data": "bad\u{0001}"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolInputAnswer>(json!({
            "kind": "unknown",
            "data": "x"
        }))
        .is_err()
    );
    assert!(
        ToolInputAnswer::Choice { index: 0 }
            .validate(&request)
            .is_ok()
    );
}

#[test]
fn model_and_progress_values_remain_checked() {
    let selection = ModelSelection::new(
        ProviderId::from_str("openai").unwrap(),
        ModelId::from_str("responses/gpt-5").unwrap(),
    );
    let request = ModelRequest::new(
        selection,
        vec![ModelMessage::user("hello").unwrap()],
        Vec::new(),
        ModelLimits::default(),
        minicore_runtime::model::ReasoningPreference::Auto,
    )
    .unwrap();
    let response = ModelResponse::new(
        vec![AssistantPart::Text("done".to_owned())],
        ModelFinishReason::Stop,
        Usage::new(2, 1, 0),
    )
    .unwrap();
    assert_json_round_trip(&request);
    assert_json_round_trip(&response);
    assert!(ToolProgress::new(None, Some(2), Some(1)).is_err());
    assert!(ToolSet::default().specs_for(&Default::default()).is_empty());
}
