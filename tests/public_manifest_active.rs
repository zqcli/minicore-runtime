use std::path::{Path, PathBuf};
use std::str::FromStr;

use minicore_runtime::prompt::PromptBodyIntent;
use minicore_runtime::runtime_interface::{
    CommandCompletion, CommandErrorCode, CommandOutcome, EventFrame, EventRoute,
    PublicCancelTarget, PublicSubject, RetryAdvice, RuntimeCommand, RuntimeRequest,
    RuntimeStateEventKind, SessionCommand, SessionEventDetail, SessionStateEventKind,
    SnapshotResponse, StateEventMsg, TurnCommand, TurnFailureView, TurnTerminalView,
};
use minicore_runtime::wire::{
    CanonicalFileUri, FileUriFamily, IncrementalRuntimeProtocolV1, ProtocolBootstrapResponse,
    ProtocolBootstrapRouter, ProtocolRejectReason, ProtocolVersion, RuntimeCapabilities,
    RuntimeCapability, RuntimeRequestKind, TypedJsonError, decode_protocol_bootstrap_response_v1,
    decode_protocol_hello_v1, encode_protocol_bootstrap_response_v1, encode_protocol_hello_v1,
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
        "CommandRequest" => run_request(vector, RuntimeRequestKind::Dispatch),
        "CommandResponse" => run_command_response(vector),
        "RuntimeQuery" => run_request(vector, RuntimeRequestKind::Query),
        "SnapshotRequest" => run_request(vector, RuntimeRequestKind::Snapshot),
        "SubscriptionRequest" => run_request(vector, RuntimeRequestKind::Subscribe),
        "QueryResponse" => run_query_response(vector),
        "RuntimeDispatchError" => run_runtime_dispatch_error(vector),
        "EventFrame" => run_event_frame(vector),
        "CanonicalFileUriVectorSet" => run_file_uri_vectors(vector),
        "ProtocolNegotiationCaseSet" => run_negotiation_vectors(vector),
        target => panic!("active manifest target has no Rust handler: {target}"),
    }
}

fn run_event_frame(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let frame = protocol.decode_event_frame(&raw).unwrap();
            assert_event_frame_semantics(vector, &frame);
            assert_eq!(
                protocol.encode_event_frame(&frame).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "protocol_error" => {
            assert_eq!(
                vector.expected.runtime_encoder.as_deref(),
                Some("must_not_send")
            );
            let error = protocol.decode_event_frame(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported EventFrame expectation {decode}"),
    }
}

fn assert_event_frame_semantics(vector: &PublicVector, frame: &EventFrame) {
    match (vector.expected.assert.as_deref(), frame) {
        (
            Some("idle_session_snapshot"),
            EventFrame::Snapshot(SnapshotResponse::Session(snapshot)),
        ) => {
            assert_eq!(
                snapshot.session_id().to_string(),
                "ses_22222222222222222222222222222222"
            );
            assert_eq!(snapshot.definition().revision().get(), 1);
            assert_eq!(snapshot.definition().workspace().roots().len(), 1);
            assert_eq!(snapshot.usage().unwrap().model_calls(), 0);
        }
        (Some("runtime_catalog_invalidated_state"), EventFrame::State(event)) => {
            assert_eq!(event.route(), EventRoute::Runtime);
            let StateEventMsg::Runtime { kind, snapshot } = event.msg() else {
                panic!("runtime state assertion requires a Runtime message");
            };
            assert_eq!(*kind, RuntimeStateEventKind::CommandCatalogInvalidated);
            assert!(snapshot.loaded_sessions().is_empty());
        }
        (Some("turn_completed_state"), EventFrame::State(event)) => {
            assert_turn_terminal_event(event.msg(), SessionStateEventKind::TurnCompleted, None);
        }
        (Some("turn_failed_model_state"), EventFrame::State(event)) => {
            assert_turn_terminal_event(
                event.msg(),
                SessionStateEventKind::TurnFailed,
                Some(TurnFailureView::Model),
            );
        }
        (assertion, frame) => panic!("event assertion {assertion:?} does not match {frame:?}"),
    }
}

fn assert_turn_terminal_event(
    msg: &StateEventMsg,
    expected_kind: SessionStateEventKind,
    expected_failure: Option<TurnFailureView>,
) {
    let StateEventMsg::Session { kind, detail, .. } = msg else {
        panic!("turn terminal assertion requires a Session message");
    };
    assert_eq!(*kind, expected_kind);
    let Some(SessionEventDetail::TurnTerminal { terminal, .. }) = detail else {
        panic!("turn terminal assertion requires terminal detail");
    };
    match (terminal, expected_failure) {
        (TurnTerminalView::Completed { .. }, None) => {}
        (TurnTerminalView::Failed { reason, .. }, Some(expected)) => {
            assert_eq!(*reason, expected);
        }
        value => panic!("unexpected terminal {value:?}"),
    }
}

fn run_command_response(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let response = protocol.decode_command_response(&raw).unwrap();
            assert_command_response_semantics(vector, &response);
            assert_eq!(
                protocol.encode_command_response(&response).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "protocol_error" => {
            assert_eq!(
                vector.expected.runtime_encoder.as_deref(),
                Some("must_not_send")
            );
            let error = protocol.decode_command_response(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported CommandResponse expectation {decode}"),
    }
}

fn assert_command_response_semantics(
    vector: &PublicVector,
    response: &minicore_runtime::runtime_interface::CommandResponse,
) {
    match (vector.expected.assert.as_deref(), response.completion()) {
        (
            Some("turn_started_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::TurnStarted { turn_id },
                output: None,
            },
        ) => {
            assert_eq!(turn_id.to_string(), "trn_33333333333333333333333333333333");
        }
        (Some("session_busy_rejection"), CommandCompletion::Rejected(error)) => {
            assert_eq!(error.code(), CommandErrorCode::SessionBusy);
            assert_eq!(error.retry(), RetryAdvice::RefreshAndRetry);
            assert!(matches!(error.subject(), Some(PublicSubject::Session(_))));
        }
        (
            Some("command_output_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::CommandOutput,
                output: Some(output),
            },
        ) => {
            assert_eq!(output.text(), "status ok");
        }
        (assertion, completion) => {
            panic!("response assertion {assertion:?} does not match {completion:?}")
        }
    }
}

fn run_request(vector: &PublicVector, kind: RuntimeRequestKind) {
    assert_eq!(vector.direction, VectorDirection::ClientToRuntime);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let request = protocol.decode_request(kind, &raw).unwrap();
            assert_request_semantics(vector, &request);
            assert_eq!(
                protocol.encode_request(&request).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "rejected" => {
            let error = protocol.decode_request(kind, &raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported request expectation {decode}"),
    }
}

fn assert_request_semantics(vector: &PublicVector, request: &RuntimeRequest) {
    let Some(assertion) = vector.expected.assert.as_deref() else {
        return;
    };
    let RuntimeRequest::Dispatch(request) = request else {
        panic!("request assertion {assertion} requires a dispatch root");
    };
    match (assertion, request.command()) {
        (
            "runtime_reload_shared_resources",
            RuntimeCommand::Runtime(
                minicore_runtime::runtime_interface::RuntimeLifecycleCommand::ReloadSharedResources,
            ),
        ) => {}
        (
            "session_create_file_uri",
            RuntimeCommand::Session(SessionCommand::Create {
                agent_id,
                definition,
                metadata,
            }),
        ) => {
            assert_eq!(agent_id.to_string(), "agt_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
            let root = definition.workspace().primary_root();
            assert_eq!(root.key().as_str(), "repo");
            assert_eq!(root.path().as_str(), "file:///Users/alice/project");
            assert_eq!(root.path().family(), FileUriFamily::Posix);
            assert_eq!(definition.workspace().cwd().relative_path().as_str(), "src");
            assert!(metadata.name().is_none());
        }
        ("session_load", RuntimeCommand::Session(SessionCommand::Load { session_id })) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
        }
        ("session_unload", RuntimeCommand::Session(SessionCommand::Unload { session_id })) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
        }
        (
            "turn_cancel_turn",
            RuntimeCommand::Turn(TurnCommand::Cancel {
                target: PublicCancelTarget::Turn(turn_id),
                ..
            }),
        ) => {
            assert_eq!(turn_id.to_string(), "trn_66666666666666666666666666666666");
        }
        (
            "turn_cancel_submit",
            RuntimeCommand::Turn(TurnCommand::Cancel {
                target: PublicCancelTarget::Submit(command_id),
                ..
            }),
        ) => {
            assert_eq!(
                command_id.to_string(),
                "cmd_88888888888888888888888888888888"
            );
        }
        ("turn_submit_text_skill", RuntimeCommand::Turn(TurnCommand::Submit { intent, .. })) => {
            let PromptBodyIntent::Text(text) = intent.body() else {
                panic!("submit assertion requires text body");
            };
            assert_eq!(text.text(), "hello");
            assert_eq!(intent.skills().len(), 1);
            assert_eq!(intent.skills()[0].skill_id().as_str(), "code-review");
        }
        (assertion, command) => {
            panic!("request assertion {assertion} does not match {command:?}")
        }
    }
}

fn run_query_response(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let response = protocol.decode_query_response(&raw).unwrap();
            assert_eq!(
                protocol.encode_query_response(&response).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "protocol_error" => {
            assert_eq!(
                vector.expected.runtime_encoder.as_deref(),
                Some("must_not_send")
            );
            let error = protocol.decode_query_response(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported QueryResponse expectation {decode}"),
    }
}

fn run_runtime_dispatch_error(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    assert_eq!(vector.expected.decode, "accepted");
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let error = protocol.decode_runtime_dispatch_error(&raw).unwrap();
    assert_eq!(
        protocol.encode_runtime_dispatch_error(error).unwrap(),
        canonical_target(vector, &raw),
        "{}",
        vector.path,
    );
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
        assert_eq!(uri.as_str(), case.wire);
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
    let fixture_capabilities = vectors
        .runtime_capabilities
        .iter()
        .map(|value| runtime_capability(value))
        .collect::<Vec<_>>();
    let capabilities = RuntimeCapabilities::for_v1(fixture_capabilities.clone()).unwrap();
    assert_eq!(fixture_capabilities, capabilities.values());
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
                    welcome.capabilities().values().to_vec(),
                    case.expected_capabilities
                        .unwrap()
                        .iter()
                        .map(|value| runtime_capability(value))
                        .collect::<Vec<_>>(),
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

fn runtime_capability(value: &str) -> RuntimeCapability {
    match value {
        "state_events" => RuntimeCapability::StateEvents,
        "progress_events" => RuntimeCapability::ProgressEvents,
        "runtime_snapshot" => RuntimeCapability::RuntimeSnapshot,
        "session_snapshot" => RuntimeCapability::SessionSnapshot,
        "paged_queries" => RuntimeCapability::PagedQueries,
        "command_catalog" => RuntimeCapability::CommandCatalog,
        "interaction_resolution" => RuntimeCapability::InteractionResolution,
        "session_fork" => RuntimeCapability::SessionFork,
        capability => panic!("unknown fixture runtime capability {capability}"),
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
    let Some(pointers) = vector.expected.ignored_json_pointers.as_deref() else {
        assert!(vector.expected.runtime_encoder.is_none(), "{}", vector.path);
        return without_final_lf(&target);
    };
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
