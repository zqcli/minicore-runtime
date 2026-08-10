use minicore_runtime::runtime_interface::{
    AgentQuery, AgentQueryResult, CommandCompletion, CommandOutcome, EventFrame, EventRoute,
    ItemProgressContentKind, ModelCallPurpose, ObservationValueError, ProgressEvent,
    ProgressEventKind, ProgressUpdate, QueryErrorCode, QueryResult, RuntimeCommand,
    RuntimeDispatchError, RuntimeEventDetail, RuntimeLifecycleCommand, RuntimeQuery,
    RuntimeQueryResult, RuntimeReadQuery, RuntimeRequest, RuntimeStateEventKind, SessionCommand,
    SessionLifecycleView, SessionQuery, SessionQueryResult, SessionStateEventKind, SessionSummary,
    SnapshotRequest, StateEvent, StateEventMsg, SubscriptionClosed, SubscriptionScope,
};
use minicore_runtime::tools::ToolCallId;
use minicore_runtime::wire::{
    BoundedJsonError, IncrementalRuntimeProtocolV1, ProtocolLimits, ProtocolVersion,
    PublicDecodeCode, PublicDecodeStage, RuntimeRequestKind, TypedJsonError, WireV1Codec,
};

#[test]
fn four_public_request_roots_route_without_a_generic_json_envelope() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let cases = [
        (
            RuntimeRequestKind::Dispatch,
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/reload-shared-resources-command.json"
            )
            .as_slice(),
        ),
        (
            RuntimeRequestKind::Query,
            include_bytes!("../docs/fixtures/wire-v1/public/valid/runtime-capabilities-query.json")
                .as_slice(),
        ),
        (
            RuntimeRequestKind::Snapshot,
            include_bytes!("../docs/fixtures/wire-v1/public/valid/session-snapshot-request.json")
                .as_slice(),
        ),
        (
            RuntimeRequestKind::Subscribe,
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/session-subscription-request.json"
            )
            .as_slice(),
        ),
    ];

    for (kind, input) in cases {
        let request = protocol.decode_request(kind, input).unwrap();
        assert_eq!(
            protocol.encode_request(&request).unwrap(),
            without_lf(input)
        );
    }

    let RuntimeRequest::Dispatch(request) =
        protocol.decode_request(cases[0].0, cases[0].1).unwrap()
    else {
        panic!("dispatch root was misrouted");
    };
    assert_eq!(
        request.command(),
        &RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources)
    );

    let RuntimeRequest::Query(RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities)) =
        protocol.decode_request(cases[1].0, cases[1].1).unwrap()
    else {
        panic!("query root was misrouted");
    };

    let RuntimeRequest::Snapshot(SnapshotRequest::Session { session_id }) =
        protocol.decode_request(cases[2].0, cases[2].1).unwrap()
    else {
        panic!("snapshot root was misrouted");
    };
    assert_eq!(
        session_id.to_string(),
        "ses_22222222222222222222222222222222"
    );

    let RuntimeRequest::Subscribe(request) =
        protocol.decode_request(cases[3].0, cases[3].1).unwrap()
    else {
        panic!("subscription root was misrouted");
    };
    assert!(request.include_progress());
    assert!(matches!(request.scope(), SubscriptionScope::Session { .. }));
}

#[test]
fn runtime_scope_unit_variants_have_one_canonical_shape() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let runtime_snapshot =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/runtime-snapshot-request.json");
    let snapshot = protocol
        .decode_request(RuntimeRequestKind::Snapshot, runtime_snapshot)
        .unwrap();
    assert_eq!(
        protocol.encode_request(&snapshot).unwrap(),
        without_lf(runtime_snapshot),
    );
    let runtime_subscription =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/runtime-subscription-request.json");
    let subscription = protocol
        .decode_request(RuntimeRequestKind::Subscribe, runtime_subscription)
        .unwrap();
    assert_eq!(
        protocol.encode_request(&subscription).unwrap(),
        without_lf(runtime_subscription),
    );
    assert!(
        protocol
            .decode_request(
                RuntimeRequestKind::Snapshot,
                include_bytes!(
                    "../docs/fixtures/wire-v1/public/invalid/input/runtime-snapshot-null-data.json"
                ),
            )
            .is_err()
    );
}

#[test]
fn session_lifecycle_commands_and_completions_round_trip_as_typed_values() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let command =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/archive-session-command.json");
    let request = protocol
        .decode_request(RuntimeRequestKind::Dispatch, command)
        .unwrap();
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("Archive fixture decodes as a dispatch request");
    };
    assert!(matches!(
        dispatch.command(),
        RuntimeCommand::Session(SessionCommand::Archive { .. })
    ));
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(command)
    );

    let response =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/session-archived-response.json");
    let decoded = protocol.decode_command_response(response).unwrap();
    assert!(matches!(
        decoded.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionArchived,
            output: None,
        }
    ));
    assert_eq!(
        protocol.encode_command_response(&decoded).unwrap(),
        without_lf(response)
    );

    let unarchive =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/unarchive-session-command.json");
    let request = protocol
        .decode_request(RuntimeRequestKind::Dispatch, unarchive)
        .unwrap();
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("Unarchive fixture decodes as a dispatch request");
    };
    assert!(matches!(
        dispatch.command(),
        RuntimeCommand::Session(SessionCommand::Unarchive { .. })
    ));
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(unarchive)
    );

    let delete =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/delete-session-command.json");
    let request = protocol
        .decode_request(RuntimeRequestKind::Dispatch, delete)
        .unwrap();
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("Delete fixture decodes as a dispatch request");
    };
    assert!(matches!(
        dispatch.command(),
        RuntimeCommand::Session(SessionCommand::Delete { .. })
    ));
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(delete)
    );

    for (bytes, expected) in [
        (
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/session-unarchived-response.json"
            )
            .as_slice(),
            CommandOutcome::SessionUnarchived,
        ),
        (
            include_bytes!("../docs/fixtures/wire-v1/public/valid/session-deleted-response.json")
                .as_slice(),
            CommandOutcome::SessionDeleted,
        ),
        (
            include_bytes!("../docs/fixtures/wire-v1/public/valid/no-change-response.json")
                .as_slice(),
            CommandOutcome::NoChange,
        ),
    ] {
        let decoded = protocol.decode_command_response(bytes).unwrap();
        assert!(matches!(
            decoded.completion(),
            CommandCompletion::Completed { outcome, output: None } if outcome == &expected
        ));
        assert_eq!(
            protocol.encode_command_response(&decoded).unwrap(),
            without_lf(bytes)
        );
    }
}

#[test]
fn session_lifecycle_runtime_events_round_trip_with_matching_safe_summaries() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    for (bytes, expected_kind, expected_lifecycle) in [
        (
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/session-archived-state-frame.json"
            )
            .as_slice(),
            RuntimeStateEventKind::SessionArchived,
            SessionLifecycleView::Archived,
        ),
        (
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/session-unarchived-state-frame.json"
            )
            .as_slice(),
            RuntimeStateEventKind::SessionUnarchived,
            SessionLifecycleView::Open,
        ),
        (
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/session-deleted-state-frame.json"
            )
            .as_slice(),
            RuntimeStateEventKind::SessionDeleted,
            SessionLifecycleView::Deleted,
        ),
    ] {
        let frame = protocol.decode_event_frame(bytes).unwrap();
        let EventFrame::State(event) = &frame else {
            panic!("the lifecycle fixture decodes as a StateEvent");
        };
        let StateEventMsg::Runtime {
            kind,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } = event.msg()
        else {
            panic!("the lifecycle event carries one SessionChanged detail");
        };
        assert_eq!(*kind, expected_kind);
        assert_eq!(session.lifecycle(), expected_lifecycle);
        assert_eq!(
            protocol.encode_event_frame(&frame).unwrap(),
            without_lf(bytes)
        );
    }
}

#[test]
fn remaining_public_manifest_fixtures_round_trip_through_selected_v1() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();

    let resolve =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/resolve-interaction-command.json");
    let request = protocol
        .decode_request(RuntimeRequestKind::Dispatch, resolve)
        .unwrap();
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(resolve)
    );

    let updated = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/session-definition-updated-response.json"
    );
    let response = protocol.decode_command_response(updated).unwrap();
    assert_eq!(
        protocol.encode_command_response(&response).unwrap(),
        without_lf(updated)
    );

    for frame in [
        include_bytes!("../docs/fixtures/wire-v1/public/valid/progress-frame.json").as_slice(),
        include_bytes!("../docs/fixtures/wire-v1/public/valid/closed-frame.json").as_slice(),
    ] {
        let decoded = protocol.decode_event_frame(frame).unwrap();
        assert_eq!(
            protocol.encode_event_frame(&decoded).unwrap(),
            without_lf(frame)
        );
    }
}

#[test]
fn progress_and_closed_frames_round_trip_all_public_variants() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let timestamp = "2026-07-31T12:00:01.250Z".parse().unwrap();
    let session_id = "ses_22222222222222222222222222222222".parse().unwrap();
    let turn_id = "trn_33333333333333333333333333333333".parse().unwrap();
    let item_id = "itm_88888888888888888888888888888888".parse().unwrap();
    let item_route = EventRoute::Item {
        session_id,
        turn_id,
        item_id,
    };
    let turn_route = EventRoute::Turn {
        session_id,
        turn_id,
    };
    let frames = [
        EventFrame::Progress(
            ProgressEvent::new(
                timestamp,
                item_route,
                ProgressEventKind::Model,
                ProgressUpdate::item_started(item_id, 0, ItemProgressContentKind::AssistantText),
            )
            .unwrap(),
        ),
        EventFrame::Progress(
            ProgressEvent::new(
                timestamp,
                item_route,
                ProgressEventKind::Tool,
                ProgressUpdate::tool_output_delta(
                    item_id,
                    "call_1".parse::<ToolCallId>().unwrap(),
                    "SECRET-TOOL-DELTA",
                )
                .unwrap(),
            )
            .unwrap(),
        ),
        EventFrame::Progress(
            ProgressEvent::new(
                timestamp,
                turn_route,
                ProgressEventKind::Retry,
                ProgressUpdate::model_retry_scheduled(
                    ModelCallPurpose::AgentRun,
                    1,
                    "2026-07-31T12:00:02.250Z".parse().unwrap(),
                ),
            )
            .unwrap(),
        ),
        EventFrame::Progress(
            ProgressEvent::new(
                timestamp,
                turn_route,
                ProgressEventKind::Compaction,
                ProgressUpdate::operation_status("SECRET-OPERATION-STATUS").unwrap(),
            )
            .unwrap(),
        ),
        EventFrame::Closed(SubscriptionClosed::Backpressure),
        EventFrame::Closed(SubscriptionClosed::RuntimeClosing),
        EventFrame::Closed(SubscriptionClosed::PublisherRestarted),
    ];

    for frame in frames {
        assert!(!format!("{frame:?}").contains("SECRET-"));
        let encoded = protocol.encode_event_frame(&frame).unwrap();
        assert_eq!(protocol.decode_event_frame(&encoded).unwrap(), frame);
    }

    let other_item_id = "itm_99999999999999999999999999999999".parse().unwrap();
    assert_eq!(
        ProgressEvent::new(
            timestamp,
            item_route,
            ProgressEventKind::Model,
            ProgressUpdate::item_started(other_item_id, 0, ItemProgressContentKind::Reasoning,),
        ),
        Err(ObservationValueError::InconsistentProgressEvent)
    );
}

#[test]
fn runtime_capabilities_query_response_and_dispatch_error_are_typed_outputs() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let response_bytes = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/runtime-capabilities-query-response.json"
    );
    let response = protocol.decode_query_response(response_bytes).unwrap();
    let QueryResult::Runtime(RuntimeQueryResult::Capabilities(capabilities)) = response.data()
    else {
        panic!("capabilities response decoded into another result family");
    };
    assert_eq!(capabilities.values().len(), 8);
    assert_eq!(
        protocol.encode_query_response(&response).unwrap(),
        without_lf(response_bytes)
    );

    let error_bytes =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/runtime-dispatch-error.json");
    let error = protocol.decode_runtime_dispatch_error(error_bytes).unwrap();
    assert_eq!(error, RuntimeDispatchError::RequestTooLarge);
    assert_eq!(
        protocol.encode_runtime_dispatch_error(error).unwrap(),
        without_lf(error_bytes)
    );
}

#[test]
fn agent_catalog_page_queries_round_trip_through_the_selected_v1_codec() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    for (bytes, expects_cursor) in [
        (
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/list-agents-first-page-query.json"
            )
            .as_slice(),
            false,
        ),
        (
            include_bytes!(
                "../docs/fixtures/wire-v1/public/valid/list-agents-next-page-query.json"
            )
            .as_slice(),
            true,
        ),
    ] {
        let request = protocol
            .decode_request(RuntimeRequestKind::Query, bytes)
            .unwrap();
        let RuntimeRequest::Query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page,
            include_deleted,
        })) = &request
        else {
            panic!("the fixture decodes as ListAgents");
        };
        assert_eq!(page.cursor().is_some(), expects_cursor);
        assert_eq!(page.limit().get(), 50);
        assert!(!include_deleted);
        assert_eq!(
            protocol.encode_request(&request).unwrap(),
            without_lf(bytes)
        );
    }
}

#[test]
fn durable_catalog_query_results_and_stale_cursor_error_round_trip() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();

    let agents =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/list-agents-query-response.json");
    let response = protocol.decode_query_response(agents).unwrap();
    let QueryResult::Agent(AgentQueryResult::Agents(page)) = response.data() else {
        panic!("the Agent result fixture decodes as an Agent page");
    };
    assert_eq!(page.items()[0].metadata().name(), "Planner");
    assert_eq!(
        protocol.encode_query_response(&response).unwrap(),
        without_lf(agents)
    );

    let sessions =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/list-sessions-query-response.json");
    let response = protocol.decode_query_response(sessions).unwrap();
    let QueryResult::Session(SessionQueryResult::Sessions(page)) = response.data() else {
        panic!("the Session result fixture decodes as a Session page");
    };
    assert!(page.items()[0].forked());
    assert!(page.next_cursor().is_some());
    assert_eq!(
        protocol.encode_query_response(&response).unwrap(),
        without_lf(sessions)
    );

    let provenance = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/session-fork-provenance-query-response.json"
    );
    let response = protocol.decode_query_response(provenance).unwrap();
    assert!(matches!(
        response.data(),
        QueryResult::Session(SessionQueryResult::ForkProvenance(Some(_)))
    ));
    assert_eq!(
        protocol.encode_query_response(&response).unwrap(),
        without_lf(provenance)
    );

    let error_bytes =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/stale-cursor-query-error.json");
    let error = protocol.decode_query_error(error_bytes).unwrap();
    assert_eq!(error.code(), QueryErrorCode::StaleCursor);
    assert_eq!(
        protocol.encode_query_error(&error).unwrap(),
        without_lf(error_bytes)
    );
}

#[test]
fn session_forked_runtime_state_event_round_trips_with_safe_summary() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let bytes =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/session-forked-state-frame.json");
    let frame = protocol.decode_event_frame(bytes).unwrap();
    let EventFrame::State(event) = &frame else {
        panic!("the fixture decodes as a StateEvent");
    };
    assert!(matches!(event.route(), EventRoute::Session { .. }));
    let StateEventMsg::Runtime {
        kind,
        snapshot,
        detail: Some(RuntimeEventDetail::SessionChanged { session }),
    } = event.msg()
    else {
        panic!("the fixture carries one Runtime SessionChanged detail");
    };
    assert_eq!(*kind, RuntimeStateEventKind::SessionForked);
    assert!(session.forked());
    assert_eq!(
        session.session_id().to_string(),
        "ses_33333333333333333333333333333333"
    );
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(bytes)
    );

    let invalid_summary = SessionSummary::new(
        session.session_id(),
        session.definition_revision(),
        session.metadata().clone(),
        session.lifecycle(),
        false,
        session.created_at(),
    );
    let invalid = EventFrame::State(StateEvent::session_forked(
        event.timestamp(),
        event.command_id(),
        snapshot.clone(),
        invalid_summary,
    ));
    let error = protocol.encode_event_frame(&invalid).unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);
}

#[test]
fn session_catalog_queries_round_trip_through_the_selected_v1_codec() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let list =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/list-sessions-first-page-query.json");
    let request = protocol
        .decode_request(RuntimeRequestKind::Query, list)
        .unwrap();
    assert!(matches!(
        request,
        RuntimeRequest::Query(RuntimeQuery::Session(SessionQuery::ListSessions { .. }))
    ));
    assert_eq!(protocol.encode_request(&request).unwrap(), without_lf(list));

    let provenance = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/get-session-fork-provenance-query.json"
    );
    let request = protocol
        .decode_request(RuntimeRequestKind::Query, provenance)
        .unwrap();
    assert!(matches!(
        request,
        RuntimeRequest::Query(RuntimeQuery::Session(
            SessionQuery::GetSessionForkProvenance { .. }
        ))
    ));
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(provenance)
    );
}

#[test]
fn query_output_ignores_unknown_fields_but_rejects_unknown_variants() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let compatible = include_bytes!(
        "../docs/fixtures/wire-v1/public/compat/runtime-capabilities-query-response-unknown-fields.json"
    );
    let response = protocol.decode_query_response(compatible).unwrap();
    assert_eq!(
        protocol.encode_query_response(&response).unwrap(),
        without_lf(include_bytes!(
            "../docs/fixtures/wire-v1/public/valid/runtime-capabilities-query-response.json"
        )),
    );

    let unknown_variant = include_bytes!(
        "../docs/fixtures/wire-v1/public/invalid/output/unknown-query-result-variant.json"
    );
    let fault = protocol
        .decode_query_response(unknown_variant)
        .unwrap_err()
        .public_decode_error()
        .unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::UnknownOutputVariant);
}

#[test]
fn response_roots_use_the_selected_effective_limits() {
    let bytes = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/runtime-capabilities-query-response.json"
    );
    let response = IncrementalRuntimeProtocolV1::v1_0()
        .decode_query_response(bytes)
        .unwrap();
    let mut limits = ProtocolLimits::v1_0();
    limits.transport.max_response_bytes = 64;
    let protocol =
        IncrementalRuntimeProtocolV1::new(WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap());
    assert_eq!(
        protocol.decode_query_response(bytes),
        Err(TypedJsonError::Json(BoundedJsonError::RawInputTooLarge)),
    );
    assert_eq!(
        protocol.encode_query_response(&response),
        Err(TypedJsonError::FrameTooLarge),
    );
}

#[test]
fn catalog_pages_and_cursors_use_the_selected_effective_limits() {
    let page_bytes =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/list-agents-query-response.json");
    let page = IncrementalRuntimeProtocolV1::v1_0()
        .decode_query_response(page_bytes)
        .unwrap();
    let mut limits = ProtocolLimits::v1_0();
    limits.paging.max_page_size = 0;
    let protocol =
        IncrementalRuntimeProtocolV1::new(WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap());
    assert!(protocol.decode_query_response(page_bytes).is_err());
    assert!(protocol.encode_query_response(&page).is_err());

    let cursor_bytes =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/list-agents-next-page-query.json");
    let cursor = IncrementalRuntimeProtocolV1::v1_0()
        .decode_request(RuntimeRequestKind::Query, cursor_bytes)
        .unwrap();
    let mut limits = ProtocolLimits::v1_0();
    limits.paging.max_page_cursor_bytes = 1;
    let protocol =
        IncrementalRuntimeProtocolV1::new(WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap());
    assert!(
        protocol
            .decode_request(RuntimeRequestKind::Query, cursor_bytes)
            .is_err()
    );
    assert!(protocol.encode_request(&cursor).is_err());
}

#[test]
fn command_root_reports_manifest_stable_decode_faults() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    for (path, stage, code) in [
        (
            "docs/fixtures/wire-v1/public/invalid/input/duplicate-key.json",
            PublicDecodeStage::JsonStructure,
            PublicDecodeCode::DuplicateKey,
        ),
        (
            "docs/fixtures/wire-v1/public/invalid/input/unknown-envelope-field.json",
            PublicDecodeStage::SelectedSchema,
            PublicDecodeCode::UnknownInputField,
        ),
        (
            "docs/fixtures/wire-v1/public/invalid/input/unknown-command-variant.json",
            PublicDecodeStage::SelectedSchema,
            PublicDecodeCode::UnknownInputVariant,
        ),
        (
            "docs/fixtures/wire-v1/public/invalid/input/id-number-not-string.json",
            PublicDecodeStage::TypedScalar,
            PublicDecodeCode::WrongJsonType,
        ),
        (
            "docs/fixtures/wire-v1/public/invalid/input/noncanonical-id.json",
            PublicDecodeStage::TypedScalar,
            PublicDecodeCode::NoncanonicalId,
        ),
    ] {
        let input =
            std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap();
        let fault = protocol
            .decode_request(RuntimeRequestKind::Dispatch, &input)
            .unwrap_err()
            .public_decode_error()
            .unwrap();
        assert_eq!(fault.stage(), stage, "{path}");
        assert_eq!(fault.code(), code, "{path}");
    }
}

fn without_lf(input: &[u8]) -> Vec<u8> {
    input.strip_suffix(b"\n").unwrap_or(input).to_vec()
}

#[test]
fn session_metadata_updated_events_reject_mismatched_contracts_and_round_trip_valid_frames() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();

    let runtime_frame = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/session-metadata-updated-runtime-state-frame.json"
    );
    let frame = protocol.decode_event_frame(runtime_frame).unwrap();
    let EventFrame::State(event) = &frame else {
        panic!("the Runtime metadata fixture decodes as a StateEvent");
    };
    assert!(matches!(
        event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionMetadataUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { .. }),
            ..
        }
    ));
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(runtime_frame)
    );

    let session_frame = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/session-metadata-updated-session-state-frame.json"
    );
    let frame = protocol.decode_event_frame(session_frame).unwrap();
    let EventFrame::State(event) = &frame else {
        panic!("the Session metadata fixture decodes as a StateEvent");
    };
    assert_eq!(
        event.route(),
        EventRoute::Session {
            session_id: "ses_22222222222222222222222222222222".parse().unwrap()
        }
    );
    assert_eq!(
        event.msg().session_kind(),
        Some(minicore_runtime::runtime_interface::SessionStateEventKind::SessionMetadataUpdated)
    );
    assert!(event.msg().session_detail().is_none());
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(session_frame)
    );

    // The Runtime-scope contract rejects a Deleted lifecycle summary.
    let mut value: serde_json::Value =
        serde_json::from_slice(runtime_frame).expect("the fixture is JSON");
    value["data"]["msg"]["data"]["detail"]["data"]["session"]["lifecycle"] =
        serde_json::json!("deleted");
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);

    // The Runtime-scope contract rejects a missing SessionChanged detail.
    let mut value: serde_json::Value =
        serde_json::from_slice(runtime_frame).expect("the fixture is JSON");
    value["data"]["msg"]["data"]["detail"] = serde_json::Value::Null;
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::WrongJsonType);

    // The Session-scope contract rejects a populated detail.
    let mut value: serde_json::Value =
        serde_json::from_slice(session_frame).expect("the fixture is JSON");
    value["data"]["msg"]["data"]["detail"] = serde_json::json!({
        "type": "turn_terminal",
        "data": {
            "turnId": "trn_33333333333333333333333333333333",
            "terminal": {
                "type": "completed",
                "data": { "completedAt": "2026-08-03T10:02:00.123Z" },
            },
        },
    });
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);

    // The Session-scope contract rejects a Turn route.
    let mut value: serde_json::Value =
        serde_json::from_slice(session_frame).expect("the fixture is JSON");
    value["data"]["route"] = serde_json::json!({
        "type": "turn",
        "data": {
            "sessionId": "ses_22222222222222222222222222222222",
            "turnId": "trn_33333333333333333333333333333333",
        },
    });
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);

    // The Runtime-scope metadata encoder rejects a Deleted summary; Archived metadata updates
    // remain valid by contract.
    let event = protocol.decode_event_frame(runtime_frame).unwrap();
    let EventFrame::State(event) = &event else {
        panic!("the Runtime metadata fixture decodes as a StateEvent");
    };
    let StateEventMsg::Runtime {
        snapshot,
        detail: Some(RuntimeEventDetail::SessionChanged { session }),
        ..
    } = event.msg()
    else {
        panic!("the Runtime metadata fixture carries one SessionChanged detail");
    };
    let invalid_summary = SessionSummary::new(
        session.session_id(),
        session.definition_revision(),
        session.metadata().clone(),
        SessionLifecycleView::Deleted,
        session.forked(),
        session.created_at(),
    );
    let invalid = EventFrame::State(StateEvent::session_metadata_updated(
        event.timestamp(),
        event.command_id(),
        snapshot.clone(),
        invalid_summary,
    ));
    let error = protocol.encode_event_frame(&invalid).unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);
}

#[test]
fn session_update_definition_command_round_trips_with_canonical_optional_replacement_fields() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let command = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/update-session-definition-command.json"
    );
    let request = protocol
        .decode_request(RuntimeRequestKind::Dispatch, command)
        .unwrap();
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("UpdateDefinition fixture decodes as a dispatch request");
    };
    let RuntimeCommand::Session(SessionCommand::UpdateDefinition {
        session_id,
        expected_revision,
        patch,
    }) = dispatch.command()
    else {
        panic!("the fixture decodes as Session UpdateDefinition");
    };
    assert_eq!(
        session_id.to_string(),
        "ses_22222222222222222222222222222222"
    );
    assert_eq!(expected_revision.get(), 1);
    let workspace = patch
        .workspace()
        .expect("the fixture replaces the Workspace");
    assert_eq!(workspace.primary_root().key().as_str(), "repo");
    assert_eq!(
        workspace.primary_root().path().as_str(),
        "file:///Users/alice/project"
    );
    assert_eq!(
        patch
            .model()
            .expect("the fixture replaces the model")
            .reasoning(),
        minicore_runtime::model_gateway::ReasoningPreference::High
    );
    assert_eq!(
        patch
            .prompts()
            .expect("the fixture replaces the prompts")
            .enabled()
            .len(),
        1
    );
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(command)
    );

    // An empty replacement object is a valid explicit empty patch (all fields null).
    let mut empty: serde_json::Value =
        serde_json::from_slice(command).expect("the fixture is JSON");
    empty["command"]["data"]["data"]["patch"] = serde_json::json!({
        "workspace": null,
        "model": null,
        "prompts": null,
    });
    let request = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&empty).unwrap(),
        )
        .expect("an all-null patch decodes");
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("the empty patch decodes as a dispatch request");
    };
    let RuntimeCommand::Session(SessionCommand::UpdateDefinition { patch, .. }) =
        dispatch.command()
    else {
        panic!("the empty patch decodes as Session UpdateDefinition");
    };
    assert!(patch.workspace().is_none());
    assert!(patch.model().is_none());
    assert!(patch.prompts().is_none());
    let canonical_empty = br#"{"commandId":"cmd_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","command":{"type":"session","data":{"type":"update_definition","data":{"sessionId":"ses_22222222222222222222222222222222","expectedRevision":"sdr_1","patch":{"workspace":null,"model":null,"prompts":null}}}}}"#;
    assert_eq!(protocol.encode_request(&request).unwrap(), canonical_empty);

    // Unknown fields inside the patch are rejected.
    let mut unknown: serde_json::Value =
        serde_json::from_slice(command).expect("the fixture is JSON");
    unknown["command"]["data"]["data"]["patch"]["unknownField"] = serde_json::json!(true);
    let error = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&unknown).unwrap(),
        )
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::UnknownInputField);
}

#[test]
fn session_agent_upgrade_command_round_trips_with_canonical_null_target() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let current = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/upgrade-session-agent-current-command.json"
    );
    let exact = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/upgrade-session-agent-exact-command.json"
    );
    for (bytes, expected_target) in [
        (current.as_slice(), None),
        (
            exact.as_slice(),
            Some(("agt_11111111111111111111111111111111", 2_u64)),
        ),
    ] {
        let request = protocol
            .decode_request(RuntimeRequestKind::Dispatch, bytes)
            .expect("the Agent upgrade fixture decodes");
        let RuntimeRequest::Dispatch(dispatch) = &request else {
            panic!("the Agent upgrade fixture decodes as a dispatch request");
        };
        let RuntimeCommand::Session(SessionCommand::UpgradeAgentRevision {
            session_id,
            expected_revision,
            target,
        }) = dispatch.command()
        else {
            panic!("the fixture decodes as Session UpgradeAgentRevision");
        };
        assert_eq!(
            session_id.to_string(),
            "ses_22222222222222222222222222222222"
        );
        assert_eq!(expected_revision.get(), 1);
        match (target, expected_target) {
            (None, None) => {}
            (Some(target), Some((agent_id, revision))) => {
                assert_eq!(target.agent_id().to_string(), agent_id);
                assert_eq!(target.revision().get(), revision);
            }
            (target, expected) => {
                panic!("target mismatch: {target:?} vs {expected:?}")
            }
        }
        assert_eq!(
            protocol.encode_request(&request).unwrap(),
            without_lf(bytes)
        );
    }

    // An omitted target decodes as None and the canonical reencode includes target:null,
    // matching the serde Option convention.
    let mut omitted: serde_json::Value = serde_json::from_slice(current).expect("fixture is JSON");
    omitted["command"]["data"]["data"]
        .as_object_mut()
        .expect("the command payload is an object")
        .remove("target");
    let request = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&omitted).expect("the omitted-target command encodes"),
        )
        .expect("an omitted target decodes as None");
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("the omitted-target command decodes as a dispatch request");
    };
    assert!(matches!(
        dispatch.command(),
        RuntimeCommand::Session(SessionCommand::UpgradeAgentRevision { target: None, .. })
    ));
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(current),
        "canonical reencode restores target:null"
    );

    // Unknown fields inside the target object are rejected by the selected schema.
    let mut unknown: serde_json::Value = serde_json::from_slice(exact).expect("fixture is JSON");
    unknown["command"]["data"]["data"]["target"]["unknownField"] = serde_json::json!(true);
    let error = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&unknown).expect("the unknown-field command encodes"),
        )
        .expect_err("an unknown target field is rejected");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::UnknownInputField);

    // A target that is not null or an object is rejected.
    let mut wrong_shape: serde_json::Value =
        serde_json::from_slice(current).expect("fixture is JSON");
    wrong_shape["command"]["data"]["data"]["target"] = serde_json::json!("ar_1");
    let error = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&wrong_shape).expect("the wrong-shape command encodes"),
        )
        .expect_err("a non-object target is rejected");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::WrongJsonType);

    // Wrong target ID and revision types are rejected as typed scalars.
    let mut wrong_id: serde_json::Value = serde_json::from_slice(exact).expect("fixture is JSON");
    wrong_id["command"]["data"]["data"]["target"]["agentId"] = serde_json::json!(123);
    let error = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&wrong_id).expect("the wrong-ID command encodes"),
        )
        .expect_err("a non-string agentId is rejected");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);

    let mut wrong_revision: serde_json::Value =
        serde_json::from_slice(exact).expect("fixture is JSON");
    wrong_revision["command"]["data"]["data"]["target"]["revision"] = serde_json::json!(1);
    let error = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&wrong_revision).expect("the wrong-revision command encodes"),
        )
        .expect_err("a non-string revision is rejected");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
}

#[test]
fn session_definition_updated_events_reject_mismatched_contracts_and_round_trip_valid_frames() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();

    let runtime_frame = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/session-definition-updated-runtime-state-frame.json"
    );
    let frame = protocol.decode_event_frame(runtime_frame).unwrap();
    let EventFrame::State(event) = &frame else {
        panic!("the Runtime definition fixture decodes as a StateEvent");
    };
    assert!(matches!(
        event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionDefinitionUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { .. }),
            ..
        }
    ));
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(runtime_frame)
    );

    let session_frame = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/session-definition-updated-session-state-frame.json"
    );
    let frame = protocol.decode_event_frame(session_frame).unwrap();
    let EventFrame::State(event) = &frame else {
        panic!("the Session definition fixture decodes as a StateEvent");
    };
    assert_eq!(
        event.route(),
        EventRoute::Session {
            session_id: "ses_22222222222222222222222222222222".parse().unwrap()
        }
    );
    assert_eq!(
        event.msg().session_kind(),
        Some(SessionStateEventKind::SessionDefinitionUpdated)
    );
    assert!(event.msg().session_detail().is_none());
    assert_eq!(
        event
            .msg()
            .session_snapshot()
            .unwrap()
            .definition()
            .revision()
            .get(),
        2
    );
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(session_frame)
    );

    // The Runtime-scope contract rejects a Deleted lifecycle summary.
    let mut value: serde_json::Value =
        serde_json::from_slice(runtime_frame).expect("the fixture is JSON");
    value["data"]["msg"]["data"]["detail"]["data"]["session"]["lifecycle"] =
        serde_json::json!("deleted");
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);

    // The Session-scope contract rejects a populated detail.
    let mut value: serde_json::Value =
        serde_json::from_slice(session_frame).expect("the fixture is JSON");
    value["data"]["msg"]["data"]["detail"] = serde_json::json!({
        "type": "turn_terminal",
        "data": {
            "turnId": "trn_33333333333333333333333333333333",
            "terminal": {
                "type": "completed",
                "data": { "completedAt": "2026-08-03T10:02:00.123Z" },
            },
        },
    });
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);

    // The Runtime-scope definition encoder rejects Archived as well as Deleted because ordinary
    // definition updates are only valid for Open Sessions.
    let event = protocol.decode_event_frame(runtime_frame).unwrap();
    let EventFrame::State(event) = &event else {
        panic!("the Runtime definition fixture decodes as a StateEvent");
    };
    let StateEventMsg::Runtime {
        snapshot,
        detail: Some(RuntimeEventDetail::SessionChanged { session }),
        ..
    } = event.msg()
    else {
        panic!("the Runtime definition fixture carries one SessionChanged detail");
    };
    let invalid_summary = SessionSummary::new(
        session.session_id(),
        session.definition_revision(),
        session.metadata().clone(),
        SessionLifecycleView::Archived,
        session.forked(),
        session.created_at(),
    );
    let invalid = EventFrame::State(StateEvent::session_definition_updated(
        event.timestamp(),
        event.command_id(),
        snapshot.clone(),
        invalid_summary,
    ));
    let error = protocol.encode_event_frame(&invalid).unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);
}

#[test]
fn reload_workspace_command_round_trips_strictly_and_rejects_unknown_payload_fields() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let fixture = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/reload-session-workspace-command.json"
    );
    let request = protocol
        .decode_request(RuntimeRequestKind::Dispatch, fixture)
        .expect("the reload fixture decodes");
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("the reload fixture decodes as a dispatch request");
    };
    let RuntimeCommand::Session(SessionCommand::ReloadWorkspace { session_id }) =
        dispatch.command()
    else {
        panic!("the fixture decodes as Session ReloadWorkspace");
    };
    assert_eq!(
        session_id.to_string(),
        "ses_22222222222222222222222222222222"
    );
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_lf(fixture)
    );

    // An unknown payload field is rejected by the selected schema even though the typed
    // payload itself is a strict single-sessionId object.
    let mut unknown: serde_json::Value = serde_json::from_slice(fixture).expect("fixture is JSON");
    unknown["command"]["data"]["data"]["expectedRevision"] = serde_json::json!("sdr_1");
    let error = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&unknown).expect("the unknown-field command encodes"),
        )
        .expect_err("an unknown reload payload field is rejected");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::UnknownInputField);

    // A non-object payload and a non-canonical sessionId are rejected.
    let mut null_data: serde_json::Value =
        serde_json::from_slice(fixture).expect("fixture is JSON");
    null_data["command"]["data"]["data"] = serde_json::json!(null);
    let error = protocol
        .decode_request(
            RuntimeRequestKind::Dispatch,
            &serde_json::to_vec(&null_data).expect("the null-data command encodes"),
        )
        .expect_err("a null reload payload is rejected");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::WrongJsonType);
}

#[test]
fn workspace_reloaded_outcome_round_trips_strictly_and_rejects_populated_data() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let fixture =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/workspace-reloaded-response.json");
    let response = protocol
        .decode_command_response(fixture)
        .expect("the reload completion fixture decodes");
    assert!(matches!(
        response.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::WorkspaceReloaded,
            output: None,
        }
    ));
    assert_eq!(
        protocol.encode_command_response(&response).unwrap(),
        without_lf(fixture)
    );

    // A WorkspaceReloaded outcome with populated data is rejected: the selected schema activates
    // it as a strict typed unit, not a pending unit.
    let mut populated: serde_json::Value =
        serde_json::from_slice(fixture).expect("fixture is JSON");
    populated["completion"]["data"]["outcome"]["data"] = serde_json::json!({});
    let error = protocol
        .decode_command_response(&serde_json::to_vec(&populated).unwrap())
        .expect_err("a populated WorkspaceReloaded outcome is rejected");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::WrongJsonType);

    // The semantic encoder rejects a completion that pairs the unit outcome with output text.
    let mut with_output: serde_json::Value =
        serde_json::from_slice(fixture).expect("fixture is JSON");
    with_output["completion"]["data"]["output"] = serde_json::json!({ "text": "reloaded" });
    let error = protocol
        .decode_command_response(&serde_json::to_vec(&with_output).unwrap())
        .expect_err("a unit completion cannot carry output text");
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);
}

#[test]
fn session_workspace_reloaded_state_frame_round_trips_and_rejects_broken_contracts() {
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let fixture = include_bytes!(
        "../docs/fixtures/wire-v1/public/valid/session-workspace-reloaded-session-state-frame.json"
    );
    let frame = protocol.decode_event_frame(fixture).unwrap();
    let EventFrame::State(event) = &frame else {
        panic!("the reload fixture decodes as a StateEvent");
    };
    assert_eq!(
        event.route(),
        EventRoute::Session {
            session_id: "ses_22222222222222222222222222222222".parse().unwrap()
        }
    );
    assert_eq!(
        event.msg().session_kind(),
        Some(SessionStateEventKind::SessionWorkspaceReloaded)
    );
    assert!(event.msg().session_detail().is_none());
    let snapshot = event.msg().session_snapshot().unwrap();
    assert_eq!(
        snapshot.session_id().to_string(),
        "ses_22222222222222222222222222222222"
    );
    assert_eq!(snapshot.definition().revision().get(), 2);
    assert_eq!(
        snapshot.execution(),
        minicore_runtime::runtime_interface::SessionExecutionView::Idle
    );
    assert_eq!(
        protocol.encode_event_frame(&frame).unwrap(),
        without_lf(fixture)
    );

    // The Session-scope contract rejects a populated detail.
    let mut value: serde_json::Value = serde_json::from_slice(fixture).expect("fixture is JSON");
    value["data"]["msg"]["data"]["detail"] = serde_json::json!({
        "type": "turn_terminal",
        "data": {
            "turnId": "trn_33333333333333333333333333333333",
            "terminal": {
                "type": "completed",
                "data": { "completedAt": "2026-08-03T10:02:00.123Z" },
            },
        },
    });
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);

    // A Runtime route with the same message is rejected.
    let mut wrong_route: serde_json::Value =
        serde_json::from_slice(fixture).expect("fixture is JSON");
    wrong_route["data"]["route"] = serde_json::json!({ "type": "runtime" });
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&wrong_route).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);

    // A mismatched snapshot Session identity is rejected.
    let mut wrong_session: serde_json::Value =
        serde_json::from_slice(fixture).expect("fixture is JSON");
    wrong_session["data"]["route"]["data"]["sessionId"] =
        serde_json::json!("ses_33333333333333333333333333333333");
    let error = protocol
        .decode_event_frame(&serde_json::to_vec(&wrong_session).unwrap())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);
}
