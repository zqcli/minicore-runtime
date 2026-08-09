use minicore_runtime::runtime_interface::{
    AgentQuery, AgentQueryResult, CommandCompletion, CommandOutcome, EventFrame, EventRoute,
    QueryErrorCode, QueryResult, RuntimeCommand, RuntimeDispatchError, RuntimeEventDetail,
    RuntimeLifecycleCommand, RuntimeQuery, RuntimeQueryResult, RuntimeReadQuery, RuntimeRequest,
    RuntimeStateEventKind, SessionCommand, SessionLifecycleView, SessionQuery, SessionQueryResult,
    SessionSummary, SnapshotRequest, StateEvent, StateEventMsg, SubscriptionScope,
};
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

#[test]
fn selected_v1_vectors_in_later_slices_are_not_reported_as_unknown_variants() {
    let input =
        include_bytes!("../docs/fixtures/wire-v1/public/valid/resolve-interaction-command.json");
    assert!(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_request(RuntimeRequestKind::Dispatch, input)
            .unwrap_err()
            .is_pending_public_target()
    );
}

fn without_lf(input: &[u8]) -> Vec<u8> {
    input.strip_suffix(b"\n").unwrap_or(input).to_vec()
}
