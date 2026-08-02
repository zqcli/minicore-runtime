use minicore_runtime::wire::{
    CapabilityToken, ProtocolBootstrapResponse, ProtocolBootstrapRouter,
    ProtocolBootstrapRouterError, ProtocolLimits, ProtocolRejectReason, ProtocolVersion,
    PublicDecodeCode, PublicDecodeStage, RuntimeCapabilities,
};

#[test]
fn bootstrap_advertises_only_runtime_implemented_capabilities() {
    let capabilities =
        RuntimeCapabilities::for_v1(vec!["state_events".parse::<CapabilityToken>().unwrap()])
            .unwrap();
    let router = ProtocolBootstrapRouter::new("minicore-runtime", "0.1.0", capabilities).unwrap();
    let route = router
        .route(include_bytes!(
            "../docs/fixtures/wire-v1/public/valid/protocol-hello.json"
        ))
        .unwrap();

    let ProtocolBootstrapResponse::Welcome(welcome) = route.response() else {
        panic!("supported V1 Hello was rejected");
    };
    assert_eq!(welcome.selected_version(), ProtocolVersion::V1_0);
    assert_eq!(
        welcome
            .capabilities()
            .values()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["state_events"],
    );
    assert_eq!(welcome.limits(), ProtocolLimits::v1_0());
    let codec = route.codec().expect("selected route must carry its codec");
    assert_eq!(codec.selected_version(), ProtocolVersion::V1_0);
    assert_eq!(codec.limits(), ProtocolLimits::v1_0());
}

#[test]
fn bootstrap_separates_schema_errors_from_semantically_invalid_hello() {
    let router = ProtocolBootstrapRouter::new(
        "minicore-runtime",
        "0.1.0",
        RuntimeCapabilities::for_v1(Vec::new()).unwrap(),
    )
    .unwrap();
    let schema_error = router
        .route(include_bytes!(
            "../docs/fixtures/wire-v1/public/invalid/input/hello-unknown-field.json"
        ))
        .unwrap_err()
        .public_decode_error()
        .unwrap();
    assert_eq!(schema_error.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(schema_error.code(), PublicDecodeCode::UnknownInputField);

    let duplicate_version = br#"{"supportedVersions":[{"major":1,"minor":0},{"major":1,"minor":0}],"client":{"name":"host","version":"1"},"capabilities":{"values":[]}}"#;
    let route = router.route(duplicate_version).unwrap();
    let ProtocolBootstrapResponse::Reject(reject) = route.response() else {
        panic!("semantically invalid Hello was selected");
    };
    assert_eq!(reject.reason(), ProtocolRejectReason::InvalidHello);
    assert!(route.codec().is_none());
}

#[test]
fn bootstrap_runtime_identity_is_safe_and_nonempty() {
    let capabilities = RuntimeCapabilities::for_v1(Vec::new()).unwrap();
    for invalid in ["", "bad\ridentity", "bad\0identity"] {
        assert_eq!(
            ProtocolBootstrapRouter::new(invalid, "1", capabilities.clone()),
            Err(ProtocolBootstrapRouterError::InvalidRuntimeIdentity),
        );
        assert_eq!(
            ProtocolBootstrapRouter::new("runtime", invalid, capabilities.clone()),
            Err(ProtocolBootstrapRouterError::InvalidRuntimeIdentity),
        );
    }
}
