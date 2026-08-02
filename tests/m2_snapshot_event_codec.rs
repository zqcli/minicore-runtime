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

    let invalid_pending = String::from_utf8(fixture("valid/starting-session-snapshot-frame.json"))
        .unwrap()
        .replace("agt_99999999999999999999999999999999", "agt_NOT_CANONICAL");
    let fault = protocol
        .decode_event_frame(invalid_pending.as_bytes())
        .unwrap_err()
        .public_decode_error()
        .unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::NoncanonicalId);

    let interrupted =
        String::from_utf8(fixture("valid/turn-interrupted-state-frame.json")).unwrap();
    let mismatched_route = interrupted.replacen(
        "ses_22222222222222222222222222222222",
        "ses_55555555555555555555555555555555",
        1,
    );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(mismatched_route.as_bytes())
            .unwrap_err(),
    );

    let runtime = String::from_utf8(fixture("valid/runtime-state-frame.json"))
        .unwrap()
        .replace(
            "\"kind\":\"command_catalog_invalidated\"",
            "\"kind\":\"shared_resources_reloaded\"",
        )
        .replace("\"detail\":null", "\"detail\":{\"unsafe\":true}");
    assert_invalid_scalar(protocol.decode_event_frame(runtime.as_bytes()).unwrap_err());
}

#[test]
fn pending_session_snapshot_validates_queue_and_nested_view_invariants_first() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let starting: serde_json::Value =
        serde_json::from_slice(&fixture("valid/starting-session-snapshot-frame.json")).unwrap();

    let mut too_many = starting.clone();
    let admissions = too_many["data"]["data"]["queues"]["submitAdmissions"]
        .as_array_mut()
        .unwrap();
    let template = admissions[0].clone();
    for index in 2_u8..=17 {
        let mut admission = template.clone();
        admission["commandId"] =
            serde_json::json!(format!("cmd_200000000000000000000000000000{index:02}"));
        admission["state"] = serde_json::json!("queued");
        admissions.push(admission);
    }
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&too_many).unwrap())
            .unwrap_err(),
    );

    let mut two_starting = starting.clone();
    two_starting["data"]["data"]["queues"]["submitAdmissions"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "commandId": "cmd_20000000000000000000000000000002",
            "state": "starting"
        }));
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&two_starting).unwrap())
            .unwrap_err(),
    );

    let mut missing_starting = starting.clone();
    missing_starting["data"]["data"]["queues"]["submitAdmissions"][0]["state"] =
        serde_json::json!("queued");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&missing_starting).unwrap())
            .unwrap_err(),
    );

    let mut starting_not_fifo = starting.clone();
    starting_not_fifo["data"]["data"]["queues"]["submitAdmissions"] = serde_json::json!([
        {"commandId":"cmd_20000000000000000000000000000002","state":"queued"},
        {"commandId":"cmd_20000000000000000000000000000001","state":"starting"}
    ]);
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&starting_not_fifo).unwrap())
            .unwrap_err(),
    );

    let mut archived_loaded = starting.clone();
    archived_loaded["data"]["data"]["lifecycle"] = serde_json::json!("archived");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&archived_loaded).unwrap())
            .unwrap_err(),
    );

    let mut cross_lane_duplicate = starting;
    cross_lane_duplicate["data"]["data"]["queues"]["steers"] = serde_json::json!([{
        "commandId": "cmd_20000000000000000000000000000001",
        "expectedTurnId": "trn_33333333333333333333333333333333"
    }]);
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&cross_lane_duplicate).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::DuplicateValue,
    );

    let active: serde_json::Value =
        serde_json::from_slice(&fixture("valid/active-session-snapshot-frame.json")).unwrap();
    let mut non_ready_running = active.clone();
    non_ready_running["data"]["data"]["readiness"] = serde_json::json!({"type":"preparing"});
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&non_ready_running).unwrap())
            .unwrap_err(),
    );

    let mut unloading_active = active.clone();
    unloading_active["data"]["data"]["loadState"] = serde_json::json!("unloading");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&unloading_active).unwrap())
            .unwrap_err(),
    );
    let mut wrong_steer_turn = active.clone();
    wrong_steer_turn["data"]["data"]["queues"]["steers"][0]["expectedTurnId"] =
        serde_json::json!("trn_44444444444444444444444444444444");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_steer_turn).unwrap())
            .unwrap_err(),
    );

    let mut wrong_question_phase = active.clone();
    wrong_question_phase["data"]["data"]["currentTurn"]["phase"] = serde_json::json!("sampling");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_question_phase).unwrap())
            .unwrap_err(),
    );

    let mut no_input_message = active.clone();
    no_input_message["data"]["data"]["activeItems"][0]["content"]["data"]["source"] =
        serde_json::json!("steer");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&no_input_message).unwrap())
            .unwrap_err(),
    );

    let mut final_while_running = active.clone();
    final_while_running["data"]["data"]["activeItems"][1] = serde_json::json!({
        "itemId":"itm_10000000000000000000000000000003",
        "turnId":"trn_33333333333333333333333333333333",
        "status":"completed",
        "content":{
            "type":"agent_message",
            "data":{"disposition":"final","text":["done"]}
        },
        "createdAt":"2026-07-31T12:00:02.000Z",
        "completedAt":"2026-07-31T12:00:03.000Z"
    });
    final_while_running["data"]["data"]["pendingInteractions"] = serde_json::json!([]);
    final_while_running["data"]["data"]["currentTurn"]["phase"] = serde_json::json!("sampling");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&final_while_running).unwrap())
            .unwrap_err(),
    );

    let mut valid_owner_shapes = active.clone();
    valid_owner_shapes["data"]["data"]["activeItems"][0]["content"]["data"]["contributions"] = serde_json::json!([
        {"type":"skill","data":{"skillId":"code-review"}},
        {"type":"workspace","data":{"rootKey":"repo","relativeLocation":"docs/readme.md"}}
    ]);
    valid_owner_shapes["data"]["data"]["pendingInteractions"][0]["request"]["data"]["title"] =
        serde_json::Value::Null;
    valid_owner_shapes["data"]["data"]["pendingInteractions"][0]["request"]["data"]["questions"]
        [0]["input"] = serde_json::json!({"type":"text","data":{"multiline":true}});
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&valid_owner_shapes).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut duplicate_origin = active.clone();
    duplicate_origin["data"]["data"]["activeItems"][0]["content"]["data"]["contributions"] = serde_json::json!([
        {"type":"skill","data":{"skillId":"code-review"}},
        {"type":"skill","data":{"skillId":"code-review"}}
    ]);
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&duplicate_origin).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::DuplicateValue,
    );

    let mut too_many_contributions = active.clone();
    too_many_contributions["data"]["data"]["activeItems"][0]["content"]["data"]["contributions"] =
        serde_json::Value::Array(
            (0..65)
                .map(|index| {
                    serde_json::json!({
                        "type":"workspace",
                        "data":{
                            "rootKey":"repo",
                            "relativeLocation":format!("part-{index}")
                        }
                    })
                })
                .collect(),
        );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&too_many_contributions).unwrap())
            .unwrap_err(),
    );

    let mut maximum_parts = active.clone();
    maximum_parts["data"]["data"]["activeItems"][0]["content"]["data"]["contributions"] =
        serde_json::Value::Array(
            (0..63)
                .map(|index| {
                    serde_json::json!({
                        "type":"workspace",
                        "data":{
                            "rootKey":"repo",
                            "relativeLocation":format!("part-{index}")
                        }
                    })
                })
                .collect(),
        );
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&maximum_parts).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut empty_user_message = active.clone();
    empty_user_message["data"]["data"]["activeItems"][0]["content"]["data"]["body"] =
        serde_json::Value::Null;
    empty_user_message["data"]["data"]["activeItems"][0]["content"]["data"]["contributions"] =
        serde_json::json!([]);
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&empty_user_message).unwrap())
            .unwrap_err(),
    );

    let mut empty_item = active.clone();
    empty_item["data"]["data"]["activeItems"][0]["content"]["data"] = serde_json::json!({});
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&empty_item).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::MissingRequiredField,
    );

    let mut started_user_message = active.clone();
    started_user_message["data"]["data"]["activeItems"][0]["status"] = serde_json::json!("started");
    started_user_message["data"]["data"]["activeItems"][0]["completedAt"] = serde_json::Value::Null;
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&started_user_message).unwrap())
            .unwrap_err(),
    );

    let mut started_tool_with_result = active.clone();
    started_tool_with_result["data"]["data"]["activeItems"][1]["content"]["data"]["result"] =
        serde_json::json!({"disposition":"succeeded","summary":"done"});
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&started_tool_with_result).unwrap())
            .unwrap_err(),
    );

    let mut interaction_on_message = active.clone();
    interaction_on_message["data"]["data"]["pendingInteractions"][0]["itemId"] =
        serde_json::json!("itm_10000000000000000000000000000001");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&interaction_on_message).unwrap())
            .unwrap_err(),
    );

    let mut wrong_item_turn = active.clone();
    wrong_item_turn["data"]["data"]["activeItems"][0]["turnId"] =
        serde_json::json!("trn_44444444444444444444444444444444");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_item_turn).unwrap())
            .unwrap_err(),
    );

    let mut compaction_item = active;
    compaction_item["data"]["data"]["activeItems"][0]["content"] =
        serde_json::json!({"type":"compaction_summary","data":{"summary":"x"}});
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&compaction_item).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::UnknownOutputVariant,
    );

    let mut empty_interaction: serde_json::Value =
        serde_json::from_slice(&fixture("valid/approval-session-snapshot-frame.json")).unwrap();
    empty_interaction["data"]["data"]["pendingInteractions"][0]["request"]["data"] =
        serde_json::json!({});
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&empty_interaction).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::MissingRequiredField,
    );

    let approval: serde_json::Value =
        serde_json::from_slice(&fixture("valid/approval-session-snapshot-frame.json")).unwrap();
    let mut wrong_approval_phase = approval.clone();
    wrong_approval_phase["data"]["data"]["currentTurn"]["phase"] =
        serde_json::json!("waiting_for_user_input");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_approval_phase).unwrap())
            .unwrap_err(),
    );
    let mut deny_option = approval.clone();
    deny_option["data"]["data"]["pendingInteractions"][0]["request"]["data"]["options"][0]["kind"] =
        serde_json::json!("deny");
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&deny_option).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::UnknownOutputVariant,
    );

    let mut no_options = approval;
    no_options["data"]["data"]["pendingInteractions"][0]["request"]["data"]["options"] =
        serde_json::json!([]);
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&no_options).unwrap())
            .unwrap_err(),
    );
}

#[test]
fn pending_progress_validates_complete_payload_and_route_coherence() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let progress: serde_json::Value =
        serde_json::from_slice(&fixture("valid/progress-frame.json")).unwrap();

    let mut missing_delta = progress.clone();
    missing_delta["data"]["update"]["data"]
        .as_object_mut()
        .unwrap()
        .remove("delta");
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&missing_delta).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::MissingRequiredField,
    );

    let mut wrong_kind = progress;
    wrong_kind["data"]["kind"] = serde_json::json!("tool");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_kind).unwrap())
            .unwrap_err(),
    );

    let retry = |purpose: &str, retry_count: u32| {
        serde_json::json!({
            "type":"progress",
            "data":{
                "timestamp":"2026-07-31T12:00:03.000Z",
                "route":{
                    "type":"turn",
                    "data":{
                        "sessionId":"ses_22222222222222222222222222222222",
                        "turnId":"trn_33333333333333333333333333333333"
                    }
                },
                "kind":"retry",
                "update":{
                    "type":"model_retry_scheduled",
                    "data":{
                        "purpose":purpose,
                        "retryCount":retry_count,
                        "readyAt":"2026-07-31T12:00:04.000Z"
                    }
                }
            }
        })
    };
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&retry("agent_run", 3)).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );
    for invalid in [retry("agent_run", 4), retry("compaction_summary", 2)] {
        assert_invalid_scalar(
            protocol
                .decode_event_frame(&serde_json::to_vec(&invalid).unwrap())
                .unwrap_err(),
        );
    }
}

#[test]
fn pending_state_events_validate_post_mutation_snapshot_correlation() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let active: serde_json::Value =
        serde_json::from_slice(&fixture("valid/active-session-snapshot-frame.json")).unwrap();
    let active_snapshot = active["data"]["data"].clone();
    let item = active_snapshot["activeItems"][0].clone();
    let item_event = session_state_frame(
        active_snapshot.clone(),
        serde_json::json!({
            "type": "item",
            "data": {
                "sessionId": "ses_22222222222222222222222222222222",
                "turnId": "trn_33333333333333333333333333333333",
                "itemId": "itm_10000000000000000000000000000001"
            }
        }),
        "item_completed",
        serde_json::json!({"type":"item_changed","data":{"item":item}}),
    );
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&item_event).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut additive_detail = item_event.clone();
    additive_detail["data"]["msg"]["data"]["detail"]["data"]["item"]
        .as_object_mut()
        .unwrap()
        .insert("futureItemField".into(), serde_json::json!(true));
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&additive_detail).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut mismatched_detail = item_event;
    mismatched_detail["data"]["msg"]["data"]["detail"]["data"]["item"]["content"]["data"]["body"] =
        serde_json::json!("different");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&mismatched_detail).unwrap())
            .unwrap_err(),
    );

    let requested = session_state_frame(
        active_snapshot,
        serde_json::json!({
            "type": "interaction",
            "data": {
                "sessionId": "ses_22222222222222222222222222222222",
                "turnId": "trn_33333333333333333333333333333333",
                "itemId": "itm_10000000000000000000000000000003",
                "requestId": "req_10000000000000000000000000000002"
            }
        }),
        "interaction_requested",
        serde_json::Value::Null,
    );
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&requested).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let approval: serde_json::Value =
        serde_json::from_slice(&fixture("valid/approval-session-snapshot-frame.json")).unwrap();
    let mut resolved_snapshot = approval["data"]["data"].clone();
    resolved_snapshot["pendingInteractions"] = serde_json::json!([]);
    let route = serde_json::json!({
        "type": "interaction",
        "data": {
            "sessionId": "ses_22222222222222222222222222222222",
            "turnId": "trn_33333333333333333333333333333333",
            "itemId": "itm_10000000000000000000000000000002",
            "requestId": "req_10000000000000000000000000000001"
        }
    });
    for reason in [
        "host_cancelled",
        "turn_cancelled",
        "security_revoked",
        "session_unloaded",
        "runtime_closing",
        "turn_terminal",
    ] {
        let event = session_state_frame(
            resolved_snapshot.clone(),
            route.clone(),
            "interaction_resolved",
            serde_json::json!({
                "type":"interaction_resolved",
                "data":{
                    "requestId":"req_10000000000000000000000000000001",
                    "resolution":{"type":"cancelled","data":{"reason":reason}}
                }
            }),
        );
        assert!(
            protocol
                .decode_event_frame(&serde_json::to_vec(&event).unwrap())
                .unwrap_err()
                .is_pending_public_target(),
            "{reason}"
        );
    }

    let bare_cancel_reason = session_state_frame(
        resolved_snapshot.clone(),
        route.clone(),
        "interaction_resolved",
        serde_json::json!({
            "type":"interaction_resolved",
            "data":{
                "requestId":"req_10000000000000000000000000000001",
                "resolution":{"type":"cancelled","data":"host_cancelled"}
            }
        }),
    );
    assert_fault(
        protocol
            .decode_event_frame(&serde_json::to_vec(&bare_cancel_reason).unwrap())
            .unwrap_err(),
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::WrongJsonType,
    );

    let still_pending = session_state_frame(
        approval["data"]["data"].clone(),
        route.clone(),
        "interaction_resolved",
        serde_json::json!({
            "type":"interaction_resolved",
            "data":{
                "requestId":"req_10000000000000000000000000000001",
                "resolution":{"type":"cancelled","data":{"reason":"host_cancelled"}}
            }
        }),
    );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&still_pending).unwrap())
            .unwrap_err(),
    );

    let wrong_resolution_family = session_state_frame(
        resolved_snapshot.clone(),
        route.clone(),
        "interaction_resolved",
        serde_json::json!({
            "type":"interaction_resolved",
            "data":{
                "requestId":"req_10000000000000000000000000000001",
                "resolution":{"type":"user_answer","data":{"answers":[]}}
            }
        }),
    );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_resolution_family).unwrap())
            .unwrap_err(),
    );

    let answers = (0_u32..5)
        .map(|question_index| {
            serde_json::json!({
                "questionIndex": question_index,
                "value": {"type":"text","data":"x".repeat(16_000)}
            })
        })
        .collect::<Vec<_>>();
    let oversized_answer = session_state_frame(
        resolved_snapshot,
        route,
        "interaction_resolved",
        serde_json::json!({
            "type":"interaction_resolved",
            "data":{
                "requestId":"req_10000000000000000000000000000001",
                "resolution":{"type":"user_answer","data":{"answers":answers}}
            }
        }),
    );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&oversized_answer).unwrap())
            .unwrap_err(),
    );

    let mut question_snapshot = active["data"]["data"].clone();
    question_snapshot["pendingInteractions"] = serde_json::json!([]);
    let answer_with_additive_field = session_state_frame(
        question_snapshot,
        serde_json::json!({
            "type":"interaction",
            "data":{
                "sessionId":"ses_22222222222222222222222222222222",
                "turnId":"trn_33333333333333333333333333333333",
                "itemId":"itm_10000000000000000000000000000003",
                "requestId":"req_10000000000000000000000000000002"
            }
        }),
        "interaction_resolved",
        serde_json::json!({
            "type":"interaction_resolved",
            "data":{
                "requestId":"req_10000000000000000000000000000002",
                "resolution":{
                    "type":"user_answer",
                    "data":{
                        "answers":[{"questionIndex":0,"value":{"type":"text","data":"ok"}}],
                        "futureAnswerField":"x".repeat(100_000)
                    }
                }
            }
        }),
    );
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&answer_with_additive_field).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );
}

#[test]
fn pending_terminal_and_runtime_lifecycle_events_match_their_post_mutation_views() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let failed: serde_json::Value =
        serde_json::from_slice(&fixture("valid/turn-failed-state-frame.json")).unwrap();
    let mut finishing = failed.clone();
    finishing["data"]["msg"]["data"]["snapshot"]["execution"] = serde_json::json!("finishing");
    finishing["data"]["msg"]["data"]["snapshot"]["currentTurn"] = serde_json::json!({
        "turnId":"trn_33333333333333333333333333333333",
        "status":{"type":"failed","data":{
            "completedAt":"2026-07-31T12:00:05.000Z",
            "reason":"model"
        }},
        "phase":null,
        "startedAt":"2026-07-31T12:00:01.000Z"
    });
    let active: serde_json::Value =
        serde_json::from_slice(&fixture("valid/active-session-snapshot-frame.json")).unwrap();
    finishing["data"]["msg"]["data"]["snapshot"]["activeItems"] =
        serde_json::json!([active["data"]["data"]["activeItems"][0].clone()]);
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&finishing).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut mismatched_terminal = finishing;
    mismatched_terminal["data"]["msg"]["data"]["snapshot"]["currentTurn"]["status"]["data"]["reason"] =
        serde_json::json!("tool");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&mismatched_terminal).unwrap())
            .unwrap_err(),
    );

    let terminal_phase_event = session_state_frame(
        mismatched_terminal["data"]["msg"]["data"]["snapshot"].clone(),
        serde_json::json!({
            "type":"turn",
            "data":{
                "sessionId":"ses_22222222222222222222222222222222",
                "turnId":"trn_33333333333333333333333333333333"
            }
        }),
        "turn_phase_changed",
        serde_json::Value::Null,
    );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&terminal_phase_event).unwrap())
            .unwrap_err(),
    );

    let runtime: serde_json::Value =
        serde_json::from_slice(&fixture("valid/runtime-state-frame.json")).unwrap();
    let snapshot = runtime["data"]["msg"]["data"]["snapshot"].clone();
    let route = serde_json::json!({
        "type":"session",
        "data":{"sessionId":"ses_22222222222222222222222222222222"}
    });
    let archived_summary = serde_json::json!({
        "sessionId":"ses_22222222222222222222222222222222",
        "definitionRevision":"sdr_1",
        "metadata":{
            "revision":"smr_1",
            "name":null,
            "description":null,
            "updatedAt":"2026-07-31T12:00:00.000Z"
        },
        "lifecycle":"archived",
        "forked":false,
        "createdAt":"2026-07-31T12:00:00.000Z"
    });
    let archived = runtime_state_frame(
        snapshot.clone(),
        route.clone(),
        "session_archived",
        serde_json::json!({
            "type":"session_changed",
            "data":{"session":archived_summary}
        }),
    );
    assert!(
        protocol
            .decode_event_frame(&serde_json::to_vec(&archived).unwrap())
            .unwrap_err()
            .is_pending_public_target()
    );

    let mut wrong_lifecycle = archived;
    wrong_lifecycle["data"]["msg"]["data"]["detail"]["data"]["session"]["lifecycle"] =
        serde_json::json!("open");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&wrong_lifecycle).unwrap())
            .unwrap_err(),
    );

    let mut loaded_archived_snapshot = snapshot.clone();
    loaded_archived_snapshot["loadedSessions"] = serde_json::json!([{
        "sessionId":"ses_22222222222222222222222222222222",
        "readiness":{"type":"ready"},
        "execution":"idle",
        "recording":{"state":"healthy"}
    }]);
    let mut archived_while_loaded = runtime_state_frame(
        loaded_archived_snapshot,
        serde_json::json!({
            "type":"session",
            "data":{"sessionId":"ses_22222222222222222222222222222222"}
        }),
        "session_archived",
        wrong_lifecycle["data"]["msg"]["data"]["detail"].clone(),
    );
    archived_while_loaded["data"]["msg"]["data"]["detail"]["data"]["session"]["lifecycle"] =
        serde_json::json!("archived");
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&archived_while_loaded).unwrap())
            .unwrap_err(),
    );

    let unloaded_as_loaded =
        runtime_state_frame(snapshot, route, "session_loaded", serde_json::Value::Null);
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&unloaded_as_loaded).unwrap())
            .unwrap_err(),
    );

    let deleted_agent = runtime_state_frame(
        runtime["data"]["msg"]["data"]["snapshot"].clone(),
        serde_json::json!({
            "type":"agent",
            "data":{"agentId":"agt_99999999999999999999999999999999"}
        }),
        "agent_metadata_updated",
        serde_json::json!({
            "type":"agent_changed",
            "data":{"agent":{
                "agentId":"agt_99999999999999999999999999999999",
                "definitionRevision":"ar_1",
                "metadata":{
                    "revision":"amr_1",
                    "name":"fixture",
                    "description":null,
                    "updatedAt":"2026-07-31T12:00:00.000Z"
                },
                "status":"deleted",
                "createdAt":"2026-07-31T12:00:00.000Z"
            }}
        }),
    );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&deleted_agent).unwrap())
            .unwrap_err(),
    );

    let base_session: serde_json::Value =
        serde_json::from_slice(&fixture("valid/session-snapshot-frame.json")).unwrap();
    let mut unavailable = base_session["data"]["data"].clone();
    unavailable["readiness"] =
        serde_json::json!({"type":"unavailable","data":"workspace_unavailable"});
    unavailable["queues"]["acceptingInput"] = serde_json::json!(false);
    let workspace_reload = session_state_frame(
        unavailable,
        serde_json::json!({
            "type":"session",
            "data":{"sessionId":"ses_22222222222222222222222222222222"}
        }),
        "session_workspace_reloaded",
        serde_json::Value::Null,
    );
    assert_invalid_scalar(
        protocol
            .decode_event_frame(&serde_json::to_vec(&workspace_reload).unwrap())
            .unwrap_err(),
    );
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
fn m10_recording_usage_and_diagnostic_projection_remains_known_pending() {
    let base = String::from_utf8(fixture("valid/session-snapshot-frame.json")).unwrap();
    for later in [
        base.replace("\"modelCalls\":\"0\"", "\"modelCalls\":\"1\""),
        base.replace(
            "\"diagnostics\":[]",
            "\"diagnostics\":[{\"code\":\"usage_currency_limit_exceeded\",\"message\":\"safe summary\"}]",
        ),
    ] {
        assert!(
            IncrementalRuntimeProtocolV1::v1_0()
                .decode_event_frame(later.as_bytes())
                .unwrap_err()
                .is_pending_public_target()
        );
    }

    let degraded_without_diagnostic = base.replace(
        "\"recording\":{\"state\":\"healthy\"}",
        "\"recording\":{\"state\":\"degraded\"}",
    );
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(degraded_without_diagnostic.as_bytes())
            .unwrap_err(),
    );

    let healthy_with_recording_diagnostic = base.replace(
        "\"diagnostics\":[]",
        "\"diagnostics\":[{\"code\":\"session_recording_append_failed\",\"message\":\"safe summary\"}]",
    );
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(healthy_with_recording_diagnostic.as_bytes())
            .unwrap_err(),
    );

    let degraded_with_diagnostic = degraded_without_diagnostic.replace(
        "\"diagnostics\":[]",
        "\"diagnostics\":[{\"code\":\"session_recording_append_failed\",\"message\":\"safe summary\"}]",
    );
    assert!(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(degraded_with_diagnostic.as_bytes())
            .unwrap_err()
            .is_pending_public_target()
    );

    let unsafe_diagnostic = base.replace(
        "\"diagnostics\":[]",
        "\"diagnostics\":[{\"code\":\"recording_failed\",\"message\":\"unsafe\\u0000detail\"}]",
    );
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_event_frame(unsafe_diagnostic.as_bytes())
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

fn session_state_frame(
    snapshot: serde_json::Value,
    route: serde_json::Value,
    kind: &str,
    detail: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type": "state",
        "data": {
            "timestamp": "2026-07-31T12:00:05.000Z",
            "commandId": null,
            "route": route,
            "msg": {
                "type": "session",
                "data": {"kind":kind,"snapshot":snapshot,"detail":detail}
            }
        }
    })
}

fn runtime_state_frame(
    snapshot: serde_json::Value,
    route: serde_json::Value,
    kind: &str,
    detail: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type":"state",
        "data":{
            "timestamp":"2026-07-31T12:00:05.000Z",
            "commandId":null,
            "route":route,
            "msg":{
                "type":"runtime",
                "data":{"kind":kind,"snapshot":snapshot,"detail":detail}
            }
        }
    })
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
