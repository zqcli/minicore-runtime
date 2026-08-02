use std::path::{Path, PathBuf};
use std::str::FromStr;

use minicore_runtime::wire::{
    CanonicalFileUri, FileUriFamily, ProtocolBootstrapResponse, ProtocolBootstrapRouter,
    ProtocolRejectReason, ProtocolVersion, RuntimeCapabilities, TypedJsonError,
    decode_protocol_bootstrap_response_v1, decode_protocol_hello_v1,
    encode_protocol_bootstrap_response_v1, encode_protocol_hello_v1,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicManifest {
    version: u32,
    vectors: Vec<PublicVector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicVector {
    path: String,
    target: String,
    direction: VectorDirection,
    status: VectorStatus,
    slice: VectorSlice,
    expected: PublicExpectation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum VectorDirection {
    ClientToRuntime,
    RuntimeToClient,
    Bidirectional,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VectorStatus {
    Active,
    Pending,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VectorSlice {
    Foundation,
    M1,
    M2Initial,
    M8,
    M9,
    M10,
    M11,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code, reason = "fields mirror the public manifest contract")]
struct PublicExpectation {
    decode: String,
    canonical_reencode: Option<String>,
    valid_round_trip: Option<String>,
    invalid: Option<String>,
    assert: Option<String>,
    stage: Option<String>,
    code: Option<String>,
    ignored_json_pointers: Option<Vec<String>>,
    canonical_reencode_path: Option<String>,
    runtime_encoder: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileUriVectors {
    version: u32,
    target: String,
    valid: Vec<ValidFileUri>,
    invalid: Vec<InvalidFileUri>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ValidFileUri {
    wire: String,
    family: String,
    authority: Option<String>,
    decoded_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidFileUri {
    wire: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NegotiationVectors {
    runtime_supported_versions: Vec<ProtocolVersion>,
    runtime_capabilities: Vec<String>,
    cases: Vec<NegotiationCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NegotiationCase {
    hello_path: String,
    expected_response_path: String,
    expected_selected_version: Option<ProtocolVersion>,
    expected_capabilities: Option<Vec<String>>,
    expected_reject_reason: Option<String>,
}

#[test]
fn every_active_public_manifest_vector_uses_an_exported_production_seam() {
    let manifest: PublicManifest = read_json(&fixture_root().join("public/manifest.json"));
    assert_eq!(manifest.version, 2);

    let mut active = 0_usize;
    for vector in &manifest.vectors {
        match vector.direction {
            VectorDirection::ClientToRuntime
            | VectorDirection::RuntimeToClient
            | VectorDirection::Bidirectional => {}
        }
        match vector.slice {
            VectorSlice::Foundation
            | VectorSlice::M1
            | VectorSlice::M2Initial
            | VectorSlice::M8
            | VectorSlice::M9
            | VectorSlice::M10
            | VectorSlice::M11 => {}
        }
        match vector.status {
            VectorStatus::Active => {
                active += 1;
                run_active_vector(vector);
            }
            VectorStatus::Pending => {}
        }
    }
    assert!(active > 0);
}

fn run_active_vector(vector: &PublicVector) {
    match vector.target.as_str() {
        "ProtocolHello" => run_protocol_hello(vector),
        "ProtocolBootstrapResponse" => run_bootstrap_response(vector),
        "CanonicalFileUriVectorSet" => run_file_uri_vectors(vector),
        "ProtocolNegotiationCaseSet" => run_negotiation_vectors(vector),
        target => panic!("active manifest target has no Rust handler: {target}"),
    }
}

fn run_protocol_hello(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::ClientToRuntime);
    let raw = read_fixture(&vector.path);
    match vector.expected.decode.as_str() {
        "accepted" => {
            let hello = decode_protocol_hello_v1(&raw).unwrap();
            let encoded = encode_protocol_hello_v1(&hello).unwrap();
            assert_eq!(encoded, canonical_target(vector, &raw), "{}", vector.path);
        }
        "rejected" => {
            let error = decode_protocol_hello_v1(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported ProtocolHello expectation {decode}"),
    }
}

fn run_bootstrap_response(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    assert_eq!(vector.expected.decode, "accepted");
    let raw = read_fixture(&vector.path);
    let response = decode_protocol_bootstrap_response_v1(&raw).unwrap();
    let encoded = encode_protocol_bootstrap_response_v1(&response).unwrap();
    assert_eq!(encoded, canonical_target(vector, &raw), "{}", vector.path);
}

fn run_file_uri_vectors(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::Bidirectional);
    assert_eq!(vector.expected.decode, "vector_set");
    assert_eq!(
        vector.expected.valid_round_trip.as_deref(),
        Some("same_wire")
    );
    assert_eq!(vector.expected.invalid.as_deref(), Some("typed_reject"));
    let vectors: FileUriVectors = read_json(&fixture_root().join("public").join(&vector.path));
    assert_eq!(vectors.version, 1);
    assert_eq!(vectors.target, "CanonicalFileUri");
    for case in vectors.valid {
        let uri = CanonicalFileUri::from_str(&case.wire).unwrap();
        let family = match case.family.as_str() {
            "posix" => FileUriFamily::Posix,
            "drive" => FileUriFamily::Drive,
            "unc" => FileUriFamily::Unc,
            family => panic!("unknown file URI family {family}"),
        };
        assert_eq!(uri.family(), family, "{}", case.wire);
        assert_eq!(uri.authority(), case.authority.as_deref(), "{}", case.wire);
        assert_eq!(uri.decoded_path(), case.decoded_path, "{}", case.wire);
        assert_eq!(uri.to_string(), case.wire);
    }
    for case in vectors.invalid {
        assert!(
            CanonicalFileUri::from_str(&case.wire).is_err(),
            "accepted {} ({})",
            case.wire,
            case.reason,
        );
    }
}

fn run_negotiation_vectors(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::Bidirectional);
    assert_eq!(vector.expected.decode, "case_set");
    assert_eq!(
        vector.expected.assert.as_deref(),
        Some("highest_exact_version_and_capability_intersection")
    );
    let vectors: NegotiationVectors = read_json(&fixture_root().join("public").join(&vector.path));
    assert_eq!(vectors.runtime_supported_versions, [ProtocolVersion::V1_0]);
    let capabilities = RuntimeCapabilities::for_v1(
        vectors
            .runtime_capabilities
            .iter()
            .map(|value| value.parse().unwrap())
            .collect(),
    )
    .unwrap();
    let router = ProtocolBootstrapRouter::new("minicore-runtime", "0.1.0", capabilities).unwrap();

    for case in vectors.cases {
        let route = router.route(&read_fixture(&case.hello_path)).unwrap();
        let expected_bytes = read_fixture(&case.expected_response_path);
        let expected = decode_protocol_bootstrap_response_v1(&expected_bytes).unwrap();
        assert_eq!(route.response(), &expected, "{}", case.hello_path);
        assert_eq!(
            encode_protocol_bootstrap_response_v1(route.response()).unwrap(),
            without_final_lf(&expected_bytes),
            "{}",
            case.hello_path,
        );
        match route.response() {
            ProtocolBootstrapResponse::Welcome(welcome) => {
                assert!(route.codec().is_some());
                assert_eq!(
                    Some(welcome.selected_version()),
                    case.expected_selected_version
                );
                assert_eq!(
                    welcome
                        .capabilities()
                        .values()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                    case.expected_capabilities.unwrap(),
                );
                assert!(case.expected_reject_reason.is_none());
            }
            ProtocolBootstrapResponse::Reject(reject) => {
                assert!(route.codec().is_none());
                assert_eq!(
                    reject.reason(),
                    ProtocolRejectReason::UnsupportedProtocolVersion
                );
                assert_eq!(
                    case.expected_reject_reason.as_deref(),
                    Some("unsupported_protocol_version")
                );
            }
        }
    }
}

fn assert_manifest_fault(vector: &PublicVector, error: TypedJsonError) {
    let fault = error.public_decode_error().unwrap_or_else(|| {
        panic!(
            "unclassified public decode error for {}: {error:?}",
            vector.path
        )
    });
    assert_eq!(
        Some(fault.stage().as_str()),
        vector.expected.stage.as_deref()
    );
    assert_eq!(Some(fault.code().as_str()), vector.expected.code.as_deref());
}

fn canonical_target(vector: &PublicVector, raw: &[u8]) -> Vec<u8> {
    if vector.expected.canonical_reencode.as_deref() == Some("same_bytes") {
        return without_final_lf(raw);
    }
    let path = vector
        .expected
        .canonical_reencode_path
        .as_deref()
        .unwrap_or_else(|| panic!("missing canonical target for {}", vector.path));
    let target = read_fixture(path);
    let pointers = vector
        .expected
        .ignored_json_pointers
        .as_deref()
        .unwrap_or_else(|| panic!("missing ignored pointers for {}", vector.path));
    assert_eq!(
        vector.expected.runtime_encoder.as_deref(),
        Some("must_not_send_in_1_0"),
        "{}",
        vector.path,
    );
    let mut stripped: Value = serde_json::from_slice(raw).unwrap();
    for pointer in pointers {
        remove_json_pointer(&mut stripped, pointer);
    }
    let expected: Value = serde_json::from_slice(&target).unwrap();
    assert_eq!(
        stripped, expected,
        "stale ignored pointers for {}",
        vector.path
    );
    without_final_lf(&target)
}

fn remove_json_pointer(value: &mut Value, pointer: &str) {
    let (parent_pointer, token) = pointer
        .rsplit_once('/')
        .unwrap_or_else(|| panic!("invalid JSON pointer {pointer}"));
    let token = token.replace("~1", "/").replace("~0", "~");
    let parent = value
        .pointer_mut(parent_pointer)
        .unwrap_or_else(|| panic!("missing JSON pointer parent {parent_pointer}"));
    match parent {
        Value::Object(object) => {
            assert!(
                object.remove(&token).is_some(),
                "missing JSON pointer {pointer}"
            );
        }
        Value::Array(array) => {
            let index = token
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid array JSON pointer {pointer}"));
            assert!(index < array.len(), "missing JSON pointer {pointer}");
            array.remove(index);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            panic!("non-container JSON pointer parent {parent_pointer}")
        }
    }
}

fn read_fixture(relative: &str) -> Vec<u8> {
    std::fs::read(fixture_root().join("public").join(relative)).unwrap()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn without_final_lf(bytes: &[u8]) -> Vec<u8> {
    bytes.strip_suffix(b"\n").unwrap_or(bytes).to_vec()
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fixtures/wire-v1")
}
