use minicore_runtime::runtime_interface::{RuntimeCommand, RuntimeRequest, SessionCommand};
use minicore_runtime::wire::{
    FileUriFamily, IncrementalRuntimeProtocolV1, ProtocolLimits, ProtocolVersion, PublicDecodeCode,
    PublicDecodeStage, RuntimeRequestKind, WireV1Codec,
};

#[test]
fn create_session_fixture_decodes_to_host_neutral_owner_values_and_reencodes_exactly() {
    let raw = fixture();
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let request = protocol
        .decode_request(RuntimeRequestKind::Dispatch, &raw)
        .unwrap();
    let RuntimeRequest::Dispatch(dispatch) = &request else {
        panic!("create did not decode as dispatch");
    };
    let RuntimeCommand::Session(SessionCommand::Create {
        agent_id,
        definition,
        metadata,
    }) = dispatch.command()
    else {
        panic!("create decoded into another command");
    };

    assert_eq!(agent_id.to_string(), "agt_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let root = definition.workspace().primary_root();
    assert_eq!(root.key().as_str(), "repo");
    assert_eq!(root.path().as_str(), "file:///Users/alice/project");
    assert_eq!(root.path().family(), FileUriFamily::Posix);
    assert_eq!(definition.workspace().cwd().relative_path().as_str(), "src");
    assert_eq!(
        definition.model().selection().provider_id().as_str(),
        "openai"
    );
    assert!(definition.prompts().enabled().is_empty());
    assert!(metadata.name().is_none());
    assert!(!format!("{request:?}").contains("/Users/alice/project"));
    assert_eq!(
        protocol.encode_request(&request).unwrap(),
        without_final_lf(&raw)
    );
}

#[test]
fn canonical_file_uri_families_remain_valid_independent_of_the_test_host() {
    let base = fixture();
    for (uri, family) in [
        ("file:///Users/alice/project", FileUriFamily::Posix),
        ("file:///C:/work/project", FileUriFamily::Drive),
        ("file://server/share/project", FileUriFamily::Unc),
    ] {
        let raw = replace_uri(&base, uri);
        let protocol = IncrementalRuntimeProtocolV1::v1_0();
        let request = protocol
            .decode_request(RuntimeRequestKind::Dispatch, &raw)
            .unwrap();
        let RuntimeRequest::Dispatch(dispatch) = &request else {
            panic!("create did not decode as dispatch");
        };
        let RuntimeCommand::Session(SessionCommand::Create { definition, .. }) = dispatch.command()
        else {
            panic!("create decoded into another command");
        };
        assert_eq!(
            definition.workspace().primary_root().path().family(),
            family
        );
        assert_eq!(
            protocol.encode_request(&request).unwrap(),
            without_final_lf(&raw)
        );
    }
}

#[test]
fn create_decode_and_encode_consume_selected_workspace_and_metadata_limits() {
    let raw = fixture();
    let request = IncrementalRuntimeProtocolV1::v1_0()
        .decode_request(RuntimeRequestKind::Dispatch, &raw)
        .unwrap();

    let mut root_limits = ProtocolLimits::v1_0();
    root_limits.workspace.max_workspace_roots = 0;
    let protocol = selected_protocol(root_limits);
    assert_invalid_scalar(
        protocol
            .decode_request(RuntimeRequestKind::Dispatch, &raw)
            .unwrap_err(),
    );
    assert_invalid_scalar(protocol.encode_request(&request).unwrap_err());

    let named = String::from_utf8(raw.clone())
        .unwrap()
        .replace("\"name\":null", "\"name\":\"session\"")
        .into_bytes();
    let mut metadata_limits = ProtocolLimits::v1_0();
    metadata_limits.text.max_display_name_bytes = 6;
    let protocol = selected_protocol(metadata_limits);
    assert_invalid_scalar(
        protocol
            .decode_request(RuntimeRequestKind::Dispatch, &named)
            .unwrap_err(),
    );
}

#[test]
fn create_rejects_duplicate_workspace_roots_unknown_cwd_and_noncanonical_integers() {
    let base = String::from_utf8(fixture()).unwrap();
    let duplicate_root = base.replace(
        "\"additionalRoots\":[]",
        "\"additionalRoots\":[{\"key\":\"repo\",\"path\":\"file:///other\",\"requestedAccess\":\"read_only\",\"sources\":{\"prompt\":false,\"skill\":false}}]",
    );
    let error = IncrementalRuntimeProtocolV1::v1_0()
        .decode_request(RuntimeRequestKind::Dispatch, duplicate_root.as_bytes())
        .unwrap_err();
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::SelectedSchema);
    assert_eq!(fault.code(), PublicDecodeCode::DuplicateValue);

    let unknown_cwd = base.replace("\"root\":\"repo\"", "\"root\":\"missing\"");
    assert_invalid_scalar(
        IncrementalRuntimeProtocolV1::v1_0()
            .decode_request(RuntimeRequestKind::Dispatch, unknown_cwd.as_bytes())
            .unwrap_err(),
    );

    for literal in ["0", "-0", "1e3", "1.0"] {
        let invalid = base.replace(
            "\"maxOutputTokens\":null",
            &format!("\"maxOutputTokens\":{literal}"),
        );
        assert_invalid_scalar(
            IncrementalRuntimeProtocolV1::v1_0()
                .decode_request(RuntimeRequestKind::Dispatch, invalid.as_bytes())
                .unwrap_err(),
        );
    }
}

fn selected_protocol(limits: ProtocolLimits) -> IncrementalRuntimeProtocolV1 {
    IncrementalRuntimeProtocolV1::new(WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap())
}

fn assert_invalid_scalar(error: minicore_runtime::wire::TypedJsonError) {
    let fault = error.public_decode_error().unwrap();
    assert_eq!(fault.stage(), PublicDecodeStage::TypedScalar);
    assert_eq!(fault.code(), PublicDecodeCode::InvalidScalar);
}

fn fixture() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/fixtures/wire-v1/public/valid/create-session-file-uri-command.json"),
    )
    .unwrap()
}

fn without_final_lf(value: &[u8]) -> &[u8] {
    value.strip_suffix(b"\n").unwrap_or(value)
}

fn replace_uri(base: &[u8], uri: &str) -> Vec<u8> {
    String::from_utf8(base.to_vec())
        .unwrap()
        .replace("file:///Users/alice/project", uri)
        .into_bytes()
}
