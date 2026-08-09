use minicore_runtime::prompt::{PromptBodyIntent, PromptIntent, TextIntent};
use minicore_runtime::runtime_interface::{
    CommandRequest, RuntimeCommand, RuntimeRequest, TurnCommand,
};
use minicore_runtime::wire::{
    CommandId, IncrementalRuntimeProtocolV1, ProtocolLimits, ProtocolVersion, PublicDecodeCode,
    PublicDecodeStage, RuntimeRequestKind, SessionId, TurnId, WireV1Codec,
};

#[test]
fn submit_reuses_prompt_owner_values_and_has_redacted_debug() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let RuntimeRequest::Dispatch(request) = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &fixture("valid/submit-command.json"),
        )
        .unwrap()
    else {
        panic!("submit did not decode as dispatch");
    };
    let RuntimeCommand::Turn(TurnCommand::Submit { session_id, intent }) = request.command() else {
        panic!("submit decoded into another command");
    };
    assert_eq!(
        session_id.to_string(),
        "ses_22222222222222222222222222222222"
    );
    let PromptBodyIntent::Text(text) = intent.body() else {
        panic!("submit body was not text");
    };
    assert_eq!(text.text(), "hello");
    assert_eq!(intent.skills().len(), 1);
    assert_eq!(intent.skills()[0].skill_id().as_str(), "code-review");
    let debug = format!("{request:?}");
    assert!(!debug.contains("hello"));
    assert!(!debug.contains("code-review"));
}

#[test]
fn submit_nested_unknown_field_uses_the_manifest_classification() {
    let error = IncrementalRuntimeProtocolV1::v1_0()
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &fixture("invalid/input/unknown-input-field.json"),
        )
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::UnknownInputField);
}

#[test]
fn submit_uses_selected_prompt_limits_for_decode_and_encode() {
    let bytes = fixture("valid/submit-command.json");
    let request = IncrementalRuntimeProtocolV1::v1_0()
        .decode_request(RuntimeRequestKind::Dispatch, &bytes)
        .unwrap();

    let mut text_limits = ProtocolLimits::v1_0();
    text_limits.text.max_text_intent_bytes = 4;
    let protocol = IncrementalRuntimeProtocolV1::new(
        WireV1Codec::new(ProtocolVersion::V1_0, text_limits).unwrap(),
    );
    assert_invalid_scalar(
        protocol
            .decode_request(RuntimeRequestKind::Dispatch, &bytes)
            .unwrap_err(),
    );
    assert_invalid_scalar(protocol.encode_request(&request).unwrap_err());

    let mut skill_limits = ProtocolLimits::v1_0();
    skill_limits.prompt.max_skills_per_intent = 0;
    let protocol = IncrementalRuntimeProtocolV1::new(
        WireV1Codec::new(ProtocolVersion::V1_0, skill_limits).unwrap(),
    );
    assert_invalid_scalar(
        protocol
            .decode_request(RuntimeRequestKind::Dispatch, &bytes)
            .unwrap_err(),
    );
    assert_invalid_scalar(protocol.encode_request(&request).unwrap_err());
}

#[test]
fn prompt_decode_errors_do_not_expose_user_values() {
    let invalid = br#"{"commandId":"cmd_11111111111111111111111111111111","command":{"type":"turn","data":{"type":"submit","data":{"sessionId":"ses_22222222222222222222222222222222","intent":{"body":{"type":"text","data":{"text":"SECRET\u0000TEXT"}},"skills":[]}}}}}"#;
    let error = IncrementalRuntimeProtocolV1::v1_0()
        .decode_request(RuntimeRequestKind::Dispatch, invalid)
        .unwrap_err();
    assert!(!format!("{error:?}").contains("SECRET"));
    assert!(!error.to_string().contains("SECRET"));
}

#[test]
fn queued_turn_commands_round_trip_through_the_public_codec() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let session_id: SessionId = "ses_22222222222222222222222222222222".parse().unwrap();
    let turn_id: TurnId = "trn_33333333333333333333333333333333".parse().unwrap();
    let target_command_id: CommandId = "cmd_44444444444444444444444444444444".parse().unwrap();
    let intent = PromptIntent::new(
        PromptBodyIntent::Text(TextIntent::new("queued input").unwrap()),
        Vec::new(),
    )
    .unwrap();
    let commands = [
        RuntimeCommand::Turn(TurnCommand::Steer {
            session_id,
            expected_turn_id: turn_id,
            intent: intent.clone(),
        }),
        RuntimeCommand::Turn(TurnCommand::FollowUp { session_id, intent }),
        RuntimeCommand::Turn(TurnCommand::CancelQueuedMessage {
            session_id,
            target_command_id,
        }),
    ];

    for (index, command) in commands.into_iter().enumerate() {
        let command_id: CommandId = format!("cmd_{:032x}", index + 1).parse().unwrap();
        let request = RuntimeRequest::Dispatch(CommandRequest::new(command_id, command));
        let bytes = protocol.encode_request(&request).unwrap();
        let decoded = protocol
            .decode_request(RuntimeRequestKind::Dispatch, &bytes)
            .unwrap();
        assert_eq!(decoded, request);
    }
}

fn assert_invalid_scalar(error: minicore_runtime::wire::TypedJsonError) {
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);
}

fn fixture(relative: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/fixtures/wire-v1/public")
            .join(relative),
    )
    .unwrap()
}
