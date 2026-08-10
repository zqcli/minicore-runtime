use minicore_runtime::runtime_interface::{
    CommandCompletion, CommandError, CommandErrorCode, CommandOutcome, CommandOutput,
    CommandResponse, CommandValueError, PublicCancelTarget, PublicIngressLane, PublicSubject,
    RetryAdvice,
};
use minicore_runtime::skills::SkillId;
use minicore_runtime::wire::{
    AgentId, CommandId, Duration, IncrementalRuntimeProtocolV1, ItemId, ProtocolLimits,
    ProtocolVersion, PublicDecodeCode, PublicDecodeStage, RequestId, SessionId, TurnId,
    WireV1Codec,
};

#[test]
fn turn_started_and_rejected_responses_round_trip_as_typed_completions() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let started_bytes = fixture("valid/turn-started-response.json");
    let started = protocol.decode_command_response(&started_bytes).unwrap();
    let CommandCompletion::Completed { outcome, output } = started.completion() else {
        panic!("TurnStarted response was not completed");
    };
    let CommandOutcome::TurnStarted { turn_id } = outcome else {
        panic!("TurnStarted response used another outcome");
    };
    assert_eq!(turn_id.to_string(), "trn_33333333333333333333333333333333");
    assert!(output.is_none());
    assert_eq!(
        protocol.encode_command_response(&started).unwrap(),
        without_lf(&started_bytes),
    );

    let rejected_bytes = fixture("valid/rejected-command-response.json");
    let rejected = protocol.decode_command_response(&rejected_bytes).unwrap();
    let CommandCompletion::Rejected(error) = rejected.completion() else {
        panic!("rejected response was not rejected");
    };
    assert_eq!(error.code(), CommandErrorCode::SessionBusy);
    assert_eq!(error.retry(), RetryAdvice::RefreshAndRetry);
    assert!(matches!(error.subject(), Some(PublicSubject::Session(_))));
    assert_eq!(error.message(), "session is busy");
    assert_eq!(
        protocol.encode_command_response(&rejected).unwrap(),
        without_lf(&rejected_bytes),
    );
    let debug = format!("{rejected:?}");
    assert!(!debug.contains("session is busy"));
}

#[test]
fn retry_with_backoff_response_round_trips_with_typed_duration() {
    let bytes = fixture("valid/retry-with-backoff-response.json");
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let response = protocol.decode_command_response(&bytes).unwrap();
    let CommandCompletion::Rejected(error) = response.completion() else {
        panic!("retry response was not rejected");
    };
    assert_eq!(
        error.retry(),
        RetryAdvice::RetryWithBackoff {
            retry_after: Some(Duration::new(2_000).unwrap()),
        }
    );
    assert_eq!(
        protocol.encode_command_response(&response).unwrap(),
        without_lf(&bytes),
    );
}

#[test]
fn session_definition_updated_response_is_an_active_typed_outcome() {
    let bytes = fixture("valid/session-definition-updated-response.json");
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let response = protocol.decode_command_response(&bytes).unwrap();
    assert!(matches!(
        response.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDefinitionUpdated { definition_revision },
            output: None,
        } if definition_revision.get() == 2
    ));
    assert_eq!(
        protocol.encode_command_response(&response).unwrap(),
        without_lf(&bytes)
    );
}

#[test]
fn queued_command_outcomes_round_trip_with_their_typed_shapes() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let command_id: CommandId = "cmd_11111111111111111111111111111111".parse().unwrap();
    let turn_id: TurnId = "trn_33333333333333333333333333333333".parse().unwrap();
    for outcome in [
        CommandOutcome::SubmitCancelled,
        CommandOutcome::SteerQueued { turn_id },
        CommandOutcome::FollowUpQueued,
        CommandOutcome::QueuedMessageCancelled,
        CommandOutcome::CancelAccepted {
            target: PublicCancelTarget::Turn(turn_id),
            cancel_epoch: 7,
        },
    ] {
        let response = CommandResponse::new(
            command_id,
            CommandCompletion::Completed {
                outcome,
                output: None,
            },
        )
        .unwrap();
        let bytes = protocol.encode_command_response(&response).unwrap();
        let decoded = protocol.decode_command_response(&bytes).unwrap();
        assert_eq!(decoded, response);
    }
}

#[test]
fn command_response_uses_selected_message_limits_and_rejects_mismatched_output() {
    let bytes = fixture("valid/rejected-command-response.json");
    let response = IncrementalRuntimeProtocolV1::v1_0()
        .decode_command_response(&bytes)
        .unwrap();
    let mut limits = ProtocolLimits::v1_0();
    limits.text.max_diagnostic_message_bytes = 4;
    let protocol =
        IncrementalRuntimeProtocolV1::new(WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap());
    assert_invalid_scalar(protocol.decode_command_response(&bytes).unwrap_err());
    assert_invalid_scalar(protocol.encode_command_response(&response).unwrap_err());

    let output_bytes = fixture("valid/command-output-response.json");
    let output_response = IncrementalRuntimeProtocolV1::v1_0()
        .decode_command_response(&output_bytes)
        .unwrap();
    let mut output_limits = ProtocolLimits::v1_0();
    output_limits.text.max_command_output_bytes = 4;
    let protocol = IncrementalRuntimeProtocolV1::new(
        WireV1Codec::new(ProtocolVersion::V1_0, output_limits).unwrap(),
    );
    assert_invalid_scalar(protocol.decode_command_response(&output_bytes).unwrap_err());
    assert_invalid_scalar(
        protocol
            .encode_command_response(&output_response)
            .unwrap_err(),
    );

    let command_id = "cmd_11111111111111111111111111111111"
        .parse::<CommandId>()
        .unwrap();
    let turn_id = "trn_33333333333333333333333333333333"
        .parse::<TurnId>()
        .unwrap();
    assert_eq!(
        CommandResponse::new(
            command_id,
            CommandCompletion::Completed {
                outcome: CommandOutcome::TurnStarted { turn_id },
                output: Some(CommandOutput::new("unexpected").unwrap()),
            },
        ),
        Err(CommandValueError::InvalidCompletion),
    );
    assert_eq!(
        CommandResponse::new(
            command_id,
            CommandCompletion::Completed {
                outcome: CommandOutcome::CommandOutput,
                output: None,
            },
        ),
        Err(CommandValueError::InvalidCompletion),
    );
}

#[test]
fn command_error_code_and_retry_form_one_machine_contract() {
    assert_eq!(
        CommandError::new(
            CommandErrorCode::SessionBusy,
            "busy",
            RetryAdvice::DoNotRetry,
            None,
        ),
        Err(CommandValueError::InvalidErrorContract),
    );
    assert_eq!(
        CommandError::new(
            CommandErrorCode::IngressLaneFull {
                lane: PublicIngressLane::TurnAdmission,
            },
            "full",
            RetryAdvice::RefreshAndRetry,
            None,
        ),
        Err(CommandValueError::InvalidErrorContract),
    );
    assert!(
        CommandError::new(
            CommandErrorCode::SessionBusy,
            "busy",
            RetryAdvice::RefreshAndRetry,
            None,
        )
        .is_ok()
    );
    assert!(
        CommandError::new(
            CommandErrorCode::RuntimeClosing,
            "closing",
            RetryAdvice::RetryWithBackoff { retry_after: None },
            None,
        )
        .is_ok()
    );
}

#[test]
fn command_text_values_enforce_hard_boundaries() {
    let limits = ProtocolLimits::v1_0().text;
    assert!(CommandOutput::new("").is_err());
    assert!(CommandOutput::new("unsafe\0output").is_err());
    assert!(CommandOutput::new("x".repeat(limits.max_command_output_bytes as usize)).is_ok());
    assert!(CommandOutput::new("x".repeat(limits.max_command_output_bytes as usize + 1)).is_err());

    assert!(
        CommandError::new(
            CommandErrorCode::SessionBusy,
            "x".repeat(limits.max_diagnostic_message_bytes as usize),
            RetryAdvice::RefreshAndRetry,
            None,
        )
        .is_ok()
    );
    assert!(
        CommandError::new(
            CommandErrorCode::SessionBusy,
            "x".repeat(limits.max_diagnostic_message_bytes as usize + 1),
            RetryAdvice::RefreshAndRetry,
            None,
        )
        .is_err()
    );
}

#[test]
fn representable_error_codes_and_all_subjects_round_trip_exhaustively() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let command_id = "cmd_11111111111111111111111111111111"
        .parse::<CommandId>()
        .unwrap();
    let cases = [
        (CommandErrorCode::InvalidArgument, RetryAdvice::DoNotRetry),
        (CommandErrorCode::NotFound, RetryAdvice::RefreshAndRetry),
        (CommandErrorCode::CommandConflict, RetryAdvice::DoNotRetry),
        (
            CommandErrorCode::StaleRevision,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::AgentDisabled,
            RetryAdvice::UserActionRequired,
        ),
        (CommandErrorCode::AgentDeleted, RetryAdvice::DoNotRetry),
        (
            CommandErrorCode::SessionArchived,
            RetryAdvice::UserActionRequired,
        ),
        (CommandErrorCode::SessionDeleted, RetryAdvice::DoNotRetry),
        (
            CommandErrorCode::SessionNotLoaded,
            RetryAdvice::UserActionRequired,
        ),
        (
            CommandErrorCode::SessionNotReady,
            RetryAdvice::UserActionRequired,
        ),
        (CommandErrorCode::SessionBusy, RetryAdvice::RefreshAndRetry),
        (
            CommandErrorCode::ReloadValidationFailed,
            RetryAdvice::UserActionRequired,
        ),
        (
            CommandErrorCode::QueuedMessageNotQueued,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::SubmitNotCancellable,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::ExpectedTurnMismatch,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::TurnNotRunning,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::TurnCancelling,
            RetryAdvice::RefreshAndRetry,
        ),
        (CommandErrorCode::TurnTerminal, RetryAdvice::RefreshAndRetry),
        (
            CommandErrorCode::InteractionNotFound,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::InteractionAlreadyResolved,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::InteractionFamilyMismatch,
            RetryAdvice::DoNotRetry,
        ),
        (
            CommandErrorCode::InvalidForkAnchor,
            RetryAdvice::RefreshAndRetry,
        ),
        (
            CommandErrorCode::Unauthorized,
            RetryAdvice::UserActionRequired,
        ),
        (
            CommandErrorCode::Unavailable,
            RetryAdvice::RetryWithBackoff { retry_after: None },
        ),
        (
            CommandErrorCode::DurableStateCorrupt,
            RetryAdvice::UserActionRequired,
        ),
        (
            CommandErrorCode::DurableStateTooLarge,
            RetryAdvice::UserActionRequired,
        ),
        (
            CommandErrorCode::RuntimeClosing,
            RetryAdvice::RetryWithBackoff {
                retry_after: Some(Duration::new(2_000).unwrap()),
            },
        ),
    ];
    for (code, retry) in cases {
        let response = CommandResponse::new(
            command_id,
            CommandCompletion::Rejected(CommandError::new(code, "safe", retry, None).unwrap()),
        )
        .unwrap();
        let encoded = protocol.encode_command_response(&response).unwrap();
        let decoded = protocol.decode_command_response(&encoded).unwrap();
        let CommandCompletion::Rejected(error) = decoded.completion() else {
            panic!("error response decoded as completed");
        };
        assert_eq!(error.code(), code);
        assert_eq!(error.retry(), retry);
    }

    let session_id = "ses_22222222222222222222222222222222"
        .parse::<SessionId>()
        .unwrap();
    let turn_id = "trn_33333333333333333333333333333333"
        .parse::<TurnId>()
        .unwrap();
    let item_id = "itm_44444444444444444444444444444444"
        .parse::<ItemId>()
        .unwrap();
    let request_id = "req_55555555555555555555555555555555"
        .parse::<RequestId>()
        .unwrap();
    let subjects = [
        PublicSubject::Runtime,
        PublicSubject::Command(command_id),
        PublicSubject::Agent(
            "agt_66666666666666666666666666666666"
                .parse::<AgentId>()
                .unwrap(),
        ),
        PublicSubject::Session(session_id),
        PublicSubject::Turn {
            session_id,
            turn_id,
        },
        PublicSubject::Item {
            session_id,
            turn_id,
            item_id,
        },
        PublicSubject::Interaction {
            session_id,
            turn_id,
            item_id,
            request_id,
        },
        PublicSubject::Skill("code-review".parse::<SkillId>().unwrap()),
    ];
    for subject in subjects {
        let response = CommandResponse::new(
            command_id,
            CommandCompletion::Rejected(
                CommandError::new(
                    CommandErrorCode::InvalidArgument,
                    "safe",
                    RetryAdvice::DoNotRetry,
                    Some(subject.clone()),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        let decoded = protocol
            .decode_command_response(&protocol.encode_command_response(&response).unwrap())
            .unwrap();
        let CommandCompletion::Rejected(error) = decoded.completion() else {
            panic!("subject response decoded as completed");
        };
        assert_eq!(error.subject(), Some(&subject));
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

fn without_lf(input: &[u8]) -> Vec<u8> {
    input.strip_suffix(b"\n").unwrap_or(input).to_vec()
}
