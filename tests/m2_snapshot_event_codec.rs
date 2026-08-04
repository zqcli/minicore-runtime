use minicore_runtime::runtime_interface::{
    EventFrame, EventRoute, RuntimeStateEventKind, RuntimeStatusView, SessionEventDetail,
    SessionStateEventKind, SnapshotResponse, StateEventMsg, TurnFailureView, TurnTerminalView,
};
use minicore_runtime::wire::{
    BoundedJsonError, IncrementalRuntimeProtocolV1, ProtocolLimits, ProtocolVersion,
    PublicDecodeCode, PublicDecodeStage, TypedJsonError, WireV1Codec,
};

#[test]
fn idle_session_snapshot_frame_round_trips_through_semantic_owners() {
    let raw = fixture("valid/session-snapshot-frame.json");
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let frame = protocol.decode_event_frame(&raw).unwrap();
    let EventFrame::Snapshot(SnapshotResponse::Session(snapshot)) = &frame else {
        panic!("fixture did not decode as a Session snapshot frame");
    };
    assert_eq!(
        snapshot.session_id().to_string(),
        "ses_22222222222222222222222222222222"
    );
    assert_eq!(snapshot.metadata().revision().get(), 1);
    assert_eq!(snapshot.definition().revision().get(), 1);
    assert_eq!(snapshot.definition().workspace().roots().len(), 1);
    assert_eq!(
        snapshot.definition().workspace().roots()[0].key().as_str(),
        "repo"
    );
    assert_eq!(
        snapshot
            .definition()
            .model()
            .selection()
            .provider_id()
            .as_str(),
        "openai"
    );
    assert!(snapshot.definition().prompts().enabled().is_empty());
    assert_eq!(snapshot.usage().unwrap().model_calls(), 0);
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(&raw)
    );
}

#[test]
fn session_snapshot_output_ignores_additive_model_fields_but_not_unknown_variants() {
    let canonical = fixture("valid/session-snapshot-frame.json");
    let mut value: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    let model = value["data"]["data"]["definition"]["model"]
        .as_object_mut()
        .unwrap();
    model.insert("futureModelField".into(), serde_json::json!({"x": 1}));
    model["selection"]
        .as_object_mut()
        .unwrap()
        .insert("futureSelectionField".into(), serde_json::json!(true));
    let compatible = serde_json::to_vec(&value).unwrap();
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let frame = protocol.decode_event_frame(&compatible).unwrap();
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(&canonical)
    );

    value["data"]["data"]["definition"]["model"]["reasoning"] =
        serde_json::json!("future_reasoning");
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::UnknownOutputVariant);
}

#[test]
fn runtime_and_terminal_state_frames_round_trip_with_coherent_routes() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();

    let runtime_raw = fixture("valid/runtime-state-frame.json");
    let runtime = protocol.decode_event_frame(&runtime_raw).unwrap();
    let EventFrame::State(runtime_event) = &runtime else {
        panic!("runtime fixture did not decode as state");
    };
    assert_eq!(runtime_event.route(), EventRoute::Runtime);
    let StateEventMsg::Runtime { kind, snapshot } = runtime_event.msg() else {
        panic!("runtime fixture did not decode as runtime message");
    };
    assert_eq!(*kind, RuntimeStateEventKind::CommandCatalogInvalidated);
    assert_eq!(snapshot.runtime().status(), RuntimeStatusView::Running);
    assert!(snapshot.loaded_sessions().is_empty());
    assert_eq!(
        protocol.encode_event_frame(&runtime).unwrap(),
        without_lf(&runtime_raw)
    );

    for (path, expected_kind, expected_failure) in [
        (
            "valid/turn-completed-state-frame.json",
            SessionStateEventKind::TurnCompleted,
            None,
        ),
        (
            "valid/turn-failed-state-frame.json",
            SessionStateEventKind::TurnFailed,
            Some(TurnFailureView::Model),
        ),
    ] {
        let raw = fixture(path);
        let frame = protocol.decode_event_frame(&raw).unwrap();
        let EventFrame::State(event) = &frame else {
            panic!("{path} did not decode as state");
        };
        let EventRoute::Turn { turn_id, .. } = event.route() else {
            panic!("{path} did not use a Turn route");
        };
        let StateEventMsg::Session { kind, detail, .. } = event.msg() else {
            panic!("{path} did not decode as session message");
        };
        assert_eq!(*kind, expected_kind);
        let Some(SessionEventDetail::TurnTerminal {
            turn_id: detail_turn_id,
            terminal,
        }) = detail
        else {
            panic!("{path} did not carry terminal detail");
        };
        assert_eq!(turn_id, *detail_turn_id);
        match (terminal, expected_failure) {
            (TurnTerminalView::Completed { .. }, None) => {}
            (TurnTerminalView::Failed { reason, .. }, Some(expected)) => {
                assert_eq!(*reason, expected);
            }
            value => panic!("unexpected terminal {value:?}"),
        }
        assert_eq!(
            protocol.encode_event_frame(&frame).unwrap(),
            without_lf(&raw)
        );
    }
}

#[test]
fn later_event_slices_are_known_pending_and_unknown_frames_are_protocol_errors() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    for path in [
        "valid/starting-session-snapshot-frame.json",
        "valid/active-session-snapshot-frame.json",
        "valid/approval-session-snapshot-frame.json",
        "valid/turn-interrupted-state-frame.json",
        "valid/progress-frame.json",
        "valid/closed-frame.json",
    ] {
        assert!(
            protocol
                .decode_event_frame(&fixture(path))
                .unwrap_err()
                .is_pending_public_target(),
            "{path}"
        );
    }

    let error = protocol
        .decode_event_frame(&fixture("invalid/output/unknown-event-frame-variant.json"))
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::UnknownOutputVariant);

    let runtime = String::from_utf8(fixture("valid/runtime-state-frame.json"))
        .unwrap()
        .replace(
            "\"kind\":\"command_catalog_invalidated\"",
            "\"kind\":\"shared_resources_reloaded\"",
        );
    assert!(
        protocol
            .decode_event_frame(runtime.as_bytes())
            .unwrap_err()
            .is_pending_public_target()
    );
}

#[test]
fn known_pending_observations_do_not_validate_future_payloads() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    for path in [
        "valid/starting-session-snapshot-frame.json",
        "valid/active-session-snapshot-frame.json",
        "valid/approval-session-snapshot-frame.json",
    ] {
        assert!(
            protocol
                .decode_event_frame(&fixture(path))
                .unwrap_err()
                .is_pending_public_target(),
            "{path}"
        );
    }

    let mut starting: serde_json::Value =
        serde_json::from_slice(&fixture("valid/starting-session-snapshot-frame.json")).unwrap();
    starting["data"]["data"]["definition"]["agent"]["agentId"] =
        serde_json::json!("agt_NOT_CANONICAL");
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&starting).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut active: serde_json::Value =
        serde_json::from_slice(&fixture("valid/active-session-snapshot-frame.json")).unwrap();
    active["data"]["data"]["activeItems"][0]["content"]["data"] = serde_json::json!({});
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&active).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut interrupted: serde_json::Value =
        serde_json::from_slice(&fixture("valid/turn-interrupted-state-frame.json")).unwrap();
    interrupted["data"]["route"]["data"]["sessionId"] =
        serde_json::json!("ses_55555555555555555555555555555555");
    interrupted["data"]["msg"]["data"]["detail"]["data"]["terminal"]["data"]["reason"] =
        serde_json::json!("future-detail-is-not-validated");
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&interrupted).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut wrong_future_route: serde_json::Value =
        serde_json::from_slice(&fixture("valid/turn-interrupted-state-frame.json")).unwrap();
    wrong_future_route["data"]["route"] = serde_json::json!({"type": "runtime"});
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_future_route).unwrap())
            .unwrap_err(),
    );

    let mut progress: serde_json::Value =
        serde_json::from_slice(&fixture("valid/progress-frame.json")).unwrap();
    progress["data"]["update"]["data"]
        .as_object_mut()
        .unwrap()
        .remove("delta");
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&progress).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );
}

#[test]
fn active_terminal_contract_is_checked_before_pending_snapshot_classification() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    for (path, wrong_terminal_type) in [
        ("valid/turn-completed-state-frame.json", "failed"),
        ("valid/turn-failed-state-frame.json", "completed"),
    ] {
        for pending_snapshot in ["usage", "recording"] {
            let mut legal: serde_json::Value =
                serde_json::from_slice(&fixture(path)).expect("terminal fixture is JSON");
            if pending_snapshot == "usage" {
                legal["data"]["msg"]["data"]["snapshot"]["usage"]["modelCalls"] =
                    serde_json::json!("1");
            } else {
                legal["data"]["msg"]["data"]["snapshot"]["recording"]["state"] =
                    serde_json::json!("degraded");
            }
            assert!(
                protocol
                    .decode_event_frame(&serde_json::to_vec(&legal).unwrap())
                    .unwrap_err()
                    .is_pending_public_target(),
                "a legal terminal with a {pending_snapshot} snapshot is pending"
            );

            let mut missing_detail = legal.clone();
            missing_detail["data"]["msg"]["data"]
                .as_object_mut()
                .unwrap()
                .remove("detail");
            assert_fault(
                protocol
                    .decode_event_frame(&serde_json::to_vec(&missing_detail).unwrap())
                    .unwrap_err(),
                PublicDecodeStage::SelectedSchema,
                PublicDecodeCode::MissingRequiredField,
            );

            let mut wrong_terminal = legal.clone();
            wrong_terminal["data"]["msg"]["data"]["detail"]["data"]["terminal"]["type"] =
                serde_json::json!(wrong_terminal_type);
            assert!(
                !protocol
                    .decode_event_frame(&serde_json::to_vec(&wrong_terminal).unwrap())
                    .unwrap_err()
                    .is_pending_public_target(),
                "wrong terminal kind must remain an active contract error"
            );

            let mut wrong_route = legal;
            wrong_route["data"]["route"] = serde_json::json!({
                "type": "session",
                "data": {
                    "sessionId": "ses_22222222222222222222222222222222"
                }
            });
            assert_invalid_scalar(
                protocol
                    .decode_event_frame(&serde_json::to_vec(&wrong_route).unwrap())
                    .unwrap_err(),
            );
        }
    }
}

#[test]
fn state_event_cross_field_mismatches_fail_as_invalid_scalars() {
    let completed = String::from_utf8(fixture("valid/turn-completed-state-frame.json")).unwrap();
    let route_mismatch = completed.replacen(
        "\"turnId\":\"trn_33333333333333333333333333333333\"",
        "\"turnId\":\"trn_44444444444444444444444444444444\"",
        1,
    );
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(route_mismatch.as_bytes())
            .unwrap_err(),
    );

    let terminal_mismatch =
        completed.replace("\"kind\":\"turn_completed\"", "\"kind\":\"turn_failed\"");
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(terminal_mismatch.as_bytes())
            .unwrap_err(),
    );

    let snapshot = String::from_utf8(fixture("valid/session-snapshot-frame.json")).unwrap();
    let definition_mismatch = snapshot.replace(
        "\"definition\":{\"sessionId\":\"ses_22222222222222222222222222222222\"",
        "\"definition\":{\"sessionId\":\"ses_55555555555555555555555555555555\"",
    );
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(definition_mismatch.as_bytes())
            .unwrap_err(),
    );

    let runtime = String::from_utf8(fixture("valid/runtime-state-frame.json")).unwrap();
    let impossible_loaded_state = runtime.replace(
        "\"loadedSessions\":[]",
        "\"loadedSessions\":[{\"sessionId\":\"ses_22222222222222222222222222222222\",\"readiness\":{\"type\":\"preparing\"},\"execution\":\"running\",\"recording\":{\"state\":\"healthy\"}}]",
    );
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(impossible_loaded_state.as_bytes())
            .unwrap_err(),
    );
}

#[test]
fn progress_frame_validates_only_its_closed_outer_contract() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let canonical = fixture("valid/progress-frame.json");

    let mut empty_data = serde_json::json!({
        "type": "progress",
        "data": {}
    });
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&empty_data).unwrap())
        .unwrap_err();
    assert_fault(
        error,
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::MissingRequiredField,
    );

    let mut unknown_kind: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    unknown_kind["data"]["kind"] = serde_json::json!("future");
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&unknown_kind).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::UnknownOutputVariant,
    );

    let mut unknown_update: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    unknown_update["data"]["update"]["type"] = serde_json::json!("future_update");
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&unknown_update).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::UnknownOutputVariant,
    );

    let mut missing_update_data: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    missing_update_data["data"]["update"]
        .as_object_mut()
        .unwrap()
        .remove("data");
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&missing_update_data).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::MissingRequiredField,
    );

    let mut empty_update_data: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    empty_update_data["data"]["update"]["data"] = serde_json::json!({});
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&empty_update_data).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    empty_data["data"] = serde_json::json!({
        "timestamp": "2026-07-31T12:00:01.250Z",
        "route": {"type": "runtime"},
        "kind": "model",
        "update": {"type": "item_delta", "data": {}}
    });
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&empty_data).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );
}

#[test]
fn future_session_snapshot_projection_remains_known_pending() {
    let base = String::from_utf8(fixture("valid/session-snapshot-frame.json")).unwrap();
    for later in [
        base.replace("\"modelCalls\":\"0\"", "\"modelCalls\":\"1\""),
        base.replace(
            "\"diagnostics\":[]",
            "\"diagnostics\":[{\"code\":\"usage_currency_limit_exceeded\",\"message\":\"safe summary\"}]",
        ),
        base.replace(
            "\"recording\":{\"state\":\"healthy\"}",
            "\"recording\":{\"state\":\"degraded\"}",
        ),
    ] {
        assert!(
            IncrementalRuntimeProtocolV1::v1_0()
                .decode_event_frame(later.as_bytes())
                .unwrap_err()
                .is_pending_public_target()
        );
    }

    let unsafe_diagnostic = base.replace(
        "\"diagnostics\":[]",
        "\"diagnostics\":[{\"code\":\"recording_failed\",\"message\":\"unsafe\\u0000detail\"}]",
    );
    assert!(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(unsafe_diagnostic.as_bytes())
            .unwrap_err()
            .is_pending_public_target()
    );
}

#[test]
fn future_snapshot_classification_defers_owner_caps_and_usage_payloads() {
    let mut limits = ProtocolLimits::v1_0();
    limits.observation.max_active_items = 0;
    limits.observation.max_pending_interactions = 0;
    limits.observation.max_snapshot_diagnostics = 0;
    limits.queues.max_submit_admissions = 0;
    limits.queues.max_steers = 0;
    limits.queues.max_follow_ups = 0;
    let protocol =
        IncrementalRuntimeProtocolV1::new(WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap());

    let active = fixture("valid/active-session-snapshot-frame.json");
    assert!(
        protocol
            .decode_event_frame(&active)
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut active_usage: serde_json::Value = serde_json::from_slice(&active).unwrap();
    active_usage["data"]["data"]["usage"] = serde_json::json!({
        "modelCalls": "1"
    });
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&active_usage).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut token_usage: serde_json::Value = serde_json::from_slice(&active).unwrap();
    token_usage["data"]["data"]["usage"] = serde_json::json!({
        "modelCalls": "0",
        "compactionCalls": "0",
        "inputTokens": {"future": true}
    });
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&token_usage).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut active_lifecycle = active_usage;
    active_lifecycle["data"]["data"]["lifecycle"] = serde_json::json!("archived");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&active_lifecycle).unwrap())
            .unwrap_err(),
    );

    let mut future_state: serde_json::Value =
        serde_json::from_slice(&fixture("valid/turn-interrupted-state-frame.json")).unwrap();
    future_state["data"]["msg"]["data"]["snapshot"]["lifecycle"] = serde_json::json!("deleted");
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(&serde_json::to_vec(&future_state).unwrap())
            .unwrap_err(),
    );
}

#[test]
fn event_decode_and_encode_use_variant_specific_effective_byte_caps() {
    for (path, shrink) in [
        ("valid/session-snapshot-frame.json", SnapshotLimit::Session),
        ("valid/runtime-state-frame.json", SnapshotLimit::State),
    ] {
        let raw = fixture(path);
        let frame = IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(&raw)
            .unwrap();
        let mut limits = ProtocolLimits::v1_0();
        let maximum = u32::try_from(without_lf(&raw).len() - 1).unwrap();
        match shrink {
            SnapshotLimit::Session => limits.transport.max_session_snapshot_bytes = maximum,
            SnapshotLimit::State => limits.transport.max_state_event_bytes = maximum,
        }
        let protocol = IncrementalRuntimeProtocolV1::new(
            WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap(),
        );
        assert_eq!(
            protocol.decode_event_frame(&raw).unwrap_err(),
            TypedJsonError::Json(BoundedJsonError::RawInputTooLarge),
            "{path}"
        );
        assert_eq!(
            protocol.encode_event_frame(&frame).unwrap_err(),
            TypedJsonError::FrameTooLarge,
            "{path}"
        );
    }
}

#[test]
fn event_discriminator_is_order_independent_and_selects_pending_progress_cap() {
    let canonical = fixture("valid/runtime-state-frame.json");
    let value: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    let reordered = serde_json::to_vec(&serde_json::json!({
        "data": value["data"].clone(),
        "type": "state",
    }))
    .unwrap();
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let frame = protocol.decode_event_frame(&reordered).unwrap();
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(&canonical)
    );

    let escaped_type =
        String::from_utf8(canonical.clone())
            .unwrap()
            .replacen("\"type\"", "\"\\u0074ype\"", 1);
    assert!(protocol.decode_event_frame(escaped_type.as_bytes()).is_ok());

    let progress = fixture("valid/progress-frame.json");
    let mut limits = ProtocolLimits::v1_0();
    limits.transport.max_progress_event_bytes =
        u32::try_from(without_lf(&progress).len() - 1).unwrap();
    let protocol =
        IncrementalRuntimeProtocolV1::new(WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap());
    assert_eq!(
        protocol.decode_event_frame(&progress).unwrap_err(),
        TypedJsonError::Json(BoundedJsonError::RawInputTooLarge)
    );

    let runtime_state: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    let runtime_snapshot = serde_json::json!({
        "type": "snapshot",
        "data": {
            "type": "runtime",
            "data": runtime_state["data"]["msg"]["data"]["snapshot"].clone()
        }
    });
    let runtime_snapshot = serde_json::to_vec(&runtime_snapshot).unwrap();
    let runtime_snapshot_len = u32::try_from(runtime_snapshot.len()).unwrap();
    let mut runtime_limits = ProtocolLimits::v1_0();
    runtime_limits.transport.max_response_bytes = runtime_snapshot_len;
    runtime_limits.transport.max_session_snapshot_bytes = runtime_snapshot_len;
    runtime_limits.transport.max_state_event_bytes = runtime_snapshot_len;
    runtime_limits.transport.max_progress_event_bytes = runtime_snapshot_len;
    runtime_limits.transport.max_runtime_snapshot_bytes = runtime_snapshot_len - 1;
    let runtime_protocol = IncrementalRuntimeProtocolV1::new(
        WireV1Codec::new(ProtocolVersion::V1_0, runtime_limits).unwrap(),
    );
    assert_eq!(
        runtime_protocol
            .decode_event_frame(&runtime_snapshot)
            .unwrap_err(),
        TypedJsonError::Json(BoundedJsonError::RawInputTooLarge)
    );

    let closed = fixture("valid/closed-frame.json");
    let closed_len = u32::try_from(without_lf(&closed).len()).unwrap();
    let closed_input_len = u32::try_from(closed.len()).unwrap();
    let mut closed_limits = ProtocolLimits::v1_0();
    closed_limits.transport.max_runtime_snapshot_bytes = closed_input_len;
    closed_limits.transport.max_session_snapshot_bytes = closed_input_len;
    closed_limits.transport.max_state_event_bytes = closed_input_len;
    closed_limits.transport.max_progress_event_bytes = closed_input_len;
    closed_limits.transport.max_response_bytes = closed_len - 1;
    let closed_protocol = IncrementalRuntimeProtocolV1::new(
        WireV1Codec::new(ProtocolVersion::V1_0, closed_limits).unwrap(),
    );
    assert_eq!(
        closed_protocol.decode_event_frame(&closed).unwrap_err(),
        TypedJsonError::Json(BoundedJsonError::RawInputTooLarge)
    );

    let duplicate = format!(
        "{{\"type\":\"progress\",\"data\":{{\"padding\":\"{}\"}},\"t\\u0079pe\":\"state\"}}",
        "x".repeat(70_000)
    );
    assert_eq!(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(duplicate.as_bytes())
            .unwrap_err(),
        TypedJsonError::Json(BoundedJsonError::DuplicateKey)
    );

    let non_string_duplicate = br#"{"type":null,"t\u0079pe":"state","data":{}}"#;
    assert_eq!(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(non_string_duplicate)
            .unwrap_err(),
        TypedJsonError::Json(BoundedJsonError::DuplicateKey)
    );

    let duplicate_data = format!(
        "{{\"type\":\"progress\",\"data\":{{}},\"padding\":\"{}\",\"d\\u0061ta\":{{}}}}",
        "x".repeat(70_000)
    );
    assert_eq!(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(duplicate_data.as_bytes())
            .unwrap_err(),
        TypedJsonError::Json(BoundedJsonError::DuplicateKey)
    );

    let wrong_root = IncrementalRuntimeProtocolV1::v1_0()
        .decode_event_frame(b"[]")
        .unwrap_err();
    assert_eq!(
        wrong_root.public_decode_error().unwrap().code(),
        PublicDecodeCode::WrongJsonType
    );

    let mut state: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    state["data"]["type"] = serde_json::json!(1);
    assert!(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(&serde_json::to_vec(&state).unwrap())
            .is_ok()
    );
}

#[derive(Clone, Copy)]
enum SnapshotLimit {
    Session,
    State,
}

fn assert_invalid_scalar(error: TypedJsonError) {
    assert_fault(
        error,
        PublicDecodeStage::TypedScalar,
        PublicDecodeCode::InvalidScalar,
    );
}

fn assert_fault(error: TypedJsonError, stage: PublicDecodeStage, code: PublicDecodeCode) {
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), stage);
    assert_eq!(fault.code(), code);
}

fn fixture(relative: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/fixtures/wire-v1/public")
            .join(relative),
    )
    .unwrap()
}

fn without_lf(value: &[u8]) -> &[u8] {
    value.strip_suffix(b"\n").unwrap_or(value)
}
