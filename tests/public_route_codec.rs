use minicore_runtime::runtime_interface::{
    QueryResult, RuntimeCommand, RuntimeDispatchError, RuntimeLifecycleCommand, RuntimeQuery,
    RuntimeQueryResult, RuntimeReadQuery, RuntimeRequest, SnapshotRequest, SubscriptionScope,
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
