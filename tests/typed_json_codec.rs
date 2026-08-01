use std::path::{Path, PathBuf};

use minicore_runtime::wire::{
    BoundedJsonError, ProtocolBootstrapResponse, ProtocolLimits, ProtocolRejectReason,
    ProtocolVersion, ProtocolWelcome, PublicJsonKind, RuntimeCapabilities, RuntimeInfo,
    TypedJsonError, WireV1Codec, decode_protocol_bootstrap_response_v1, decode_protocol_hello_v1,
    encode_protocol_bootstrap_response_v1, encode_protocol_hello_v1,
};
use serde_json::Value;

#[test]
fn protocol_hello_fixtures_decode_and_reencode_exactly() {
    for name in [
        "protocol-hello.json",
        "protocol-hello-capability-subset.json",
        "protocol-hello-unsupported-version.json",
    ] {
        let bytes = read_fixture(&format!("public/valid/{name}"));
        let hello = decode_protocol_hello_v1(&bytes).unwrap();
        assert_eq!(encode_protocol_hello_v1(&hello).unwrap(), bytes, "{name}");
    }
}

#[test]
fn protocol_hello_is_strict_after_bounded_duplicate_aware_preflight() {
    let unknown = read_fixture("public/invalid/input/hello-unknown-field.json");
    assert_eq!(
        decode_protocol_hello_v1(&unknown),
        Err(TypedJsonError::TypedShape)
    );

    let duplicate = br#"{"supportedVersions":[{"major":1,"minor":0}],"client":{"name":"a","\u006eame":"b","version":"1"},"capabilities":{"values":[]}}"#;
    assert_eq!(
        decode_protocol_hello_v1(duplicate),
        Err(TypedJsonError::Json(BoundedJsonError::DuplicateKey))
    );

    for noncanonical_integer in ["1e0", "1.0", "-0"] {
        let input = format!(
            r#"{{"supportedVersions":[{{"major":{noncanonical_integer},"minor":0}}],"client":{{"name":"a","version":"1"}},"capabilities":{{"values":[]}}}}"#
        );
        assert_eq!(
            decode_protocol_hello_v1(input.as_bytes()),
            Err(TypedJsonError::TypedShape)
        );
    }

    for nested_unknown in [
        br#"{"supportedVersions":[{"major":1,"minor":0,"future":1}],"client":{"name":"a","version":"1"},"capabilities":{"values":[]}}"#.as_slice(),
        br#"{"supportedVersions":[{"major":1,"minor":0}],"client":{"name":"a","version":"1","future":1},"capabilities":{"values":[]}}"#.as_slice(),
        br#"{"supportedVersions":[{"major":1,"minor":0}],"client":{"name":"a","version":"1"},"capabilities":{"values":[],"future":1}}"#.as_slice(),
    ] {
        assert_eq!(
            decode_protocol_hello_v1(nested_unknown),
            Err(TypedJsonError::TypedShape)
        );
    }
}

#[test]
fn bootstrap_response_fixtures_decode_and_reencode_exactly() {
    for name in [
        "protocol-welcome.json",
        "protocol-welcome-capability-intersection.json",
        "protocol-reject.json",
    ] {
        let bytes = read_fixture(&format!("public/valid/{name}"));
        let response = decode_protocol_bootstrap_response_v1(&bytes).unwrap();
        assert_eq!(
            encode_protocol_bootstrap_response_v1(&response).unwrap(),
            bytes,
            "{name}"
        );
    }

    let reject =
        decode_protocol_bootstrap_response_v1(&read_fixture("public/valid/protocol-reject.json"))
            .unwrap();
    assert!(matches!(
        reject,
        ProtocolBootstrapResponse::Reject(ref value)
            if value.reason() == ProtocolRejectReason::UnsupportedProtocolVersion
    ));
}

#[test]
fn runtime_output_ignores_unknown_fields_but_never_unknown_variants() {
    let canonical = read_fixture("public/valid/protocol-welcome.json");
    let mut compatible: Value = serde_json::from_slice(&canonical).unwrap();
    compatible
        .as_object_mut()
        .unwrap()
        .insert("futureRoot".to_owned(), Value::Bool(true));
    compatible["data"]
        .as_object_mut()
        .unwrap()
        .insert("futureData".to_owned(), Value::String("ignored".to_owned()));
    compatible["data"]["runtime"]
        .as_object_mut()
        .unwrap()
        .insert("futureRuntime".to_owned(), Value::Null);
    compatible["data"]["limits"]["transport"]
        .as_object_mut()
        .unwrap()
        .insert("futureLimit".to_owned(), Value::from(1));

    let decoded =
        decode_protocol_bootstrap_response_v1(&serde_json::to_vec(&compatible).unwrap()).unwrap();
    assert_eq!(
        encode_protocol_bootstrap_response_v1(&decoded).unwrap(),
        canonical
    );

    assert_eq!(
        decode_protocol_bootstrap_response_v1(br#"{"type":"future","data":{}}"#),
        Err(TypedJsonError::TypedShape)
    );
}

#[test]
fn public_frame_preflight_uses_selected_v1_limits() {
    let codec = WireV1Codec::v1_0();
    let maximum = ProtocolLimits::v1_0().transport.max_progress_event_bytes as usize;
    let boundary = format!("{{}}{}", " ".repeat(maximum - 2));
    codec
        .preflight(PublicJsonKind::ProgressEvent, boundary.as_bytes())
        .unwrap();

    let oversized = format!("{boundary} ");
    assert_eq!(
        codec.preflight(PublicJsonKind::ProgressEvent, oversized.as_bytes()),
        Err(TypedJsonError::Json(BoundedJsonError::RawInputTooLarge))
    );
    assert_eq!(
        WireV1Codec::new(ProtocolVersion::new(1, 1), ProtocolLimits::v1_0()),
        Err(TypedJsonError::UnsupportedSelectedVersion)
    );

    let mut operational = ProtocolLimits::v1_0();
    operational.transport.max_request_bytes = 64;
    operational.transport.max_response_bytes = 96;
    let codec = WireV1Codec::new(ProtocolVersion::V1_0, operational).unwrap();
    for (kind, maximum) in [
        (PublicJsonKind::Request, 64_usize),
        (PublicJsonKind::Response, 96_usize),
    ] {
        let boundary = format!("{{}}{}", " ".repeat(maximum - 2));
        codec.preflight(kind, boundary.as_bytes()).unwrap();
        assert_eq!(
            codec.preflight(kind, format!("{boundary} ").as_bytes()),
            Err(TypedJsonError::Json(BoundedJsonError::RawInputTooLarge))
        );
    }
}

#[test]
fn effective_limits_may_shrink_but_never_exceed_v1_hard_maxima() {
    let mut inflated = ProtocolLimits::v1_0();
    inflated.transport.max_array_items += 1;
    assert_eq!(
        WireV1Codec::new(ProtocolVersion::V1_0, inflated),
        Err(TypedJsonError::InvalidProtocolLimits)
    );

    let canonical = read_fixture("public/valid/protocol-welcome.json");
    let mut document: Value = serde_json::from_slice(&canonical).unwrap();
    document["data"]["limits"]["transport"]["maxResponseBytes"] =
        Value::from(u64::from(ProtocolLimits::v1_0().transport.max_response_bytes) + 1);
    assert_eq!(
        decode_protocol_bootstrap_response_v1(&serde_json::to_vec(&document).unwrap()),
        Err(TypedJsonError::InvalidProtocolLimits)
    );

    let invalid = ProtocolBootstrapResponse::Welcome(ProtocolWelcome::new(
        ProtocolVersion::V1_0,
        RuntimeInfo::new(ProtocolVersion::V1_0, "minicore-runtime", "0.1.0"),
        RuntimeCapabilities::new(Vec::new()),
        inflated,
    ));
    assert_eq!(
        encode_protocol_bootstrap_response_v1(&invalid),
        Err(TypedJsonError::InvalidProtocolLimits)
    );
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fixtures/wire-v1")
}

fn read_fixture(relative: &str) -> Vec<u8> {
    let mut bytes = std::fs::read(fixture_root().join(relative)).unwrap();
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}
