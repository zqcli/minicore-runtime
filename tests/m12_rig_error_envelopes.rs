//! M12 slice: real Rig 0.40.0 loopback evidence for provider error
//! envelopes (400/401) and malformed 200 bodies, for OpenAI Responses and
//! Anthropic Messages. A 400/401 JSON error envelope is **preserved**
//! (status + raw body via `provider_response_status` / `provider_response_json`)
//! after exactly one HTTP request on the
//! documented path; a 200 body that cannot be a valid completion
//! (unparseable JSON for OpenAI, well-formed JSON missing every required
//! field for Anthropic) **fails closed** with a parse error after exactly
//! one request; and the Anthropic 400 envelope carries **no machine-readable
//! context-overflow subtype/code** — only human `message` text hints at the
//! cause, so the M12 gate must classify structurally, never by message prose.
//!
//! Classification stays pinned by `docs/fixtures/provider-gate-m12/error-mapping-v1.json`
//! and `tests/m12_provider_error_matrix.rs`; this file only pins what Rig
//! preserves on the wire, via `support::LoopbackServer`, settled via `join()`.

mod support;

use rig::client::CompletionClient;
use rig::completion::{CompletionError, CompletionModel};
use rig::providers::anthropic::Client as AnthropicClient;
use rig::providers::anthropic::completion::CLAUDE_SONNET_4_6;
use rig::providers::openai::{Client as OpenAiClient, GPT_4O_MINI};
use serde_json::Value;

// --- Scripted provider error envelopes ---

/// OpenAI 400: `invalid_request_error` + `code=context_length_exceeded`
/// (matrix `openai_context_length_exceeded`).
const OPENAI_400_CONTEXT_LENGTH_BODY: &str = r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, you requested 200000 tokens (128000 in the messages, 72000 in the tools).","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#;

/// Anthropic 400: `invalid_request_error` + `request_id`, no overflow `code`
/// (matrix `anthropic_invalid_request`).
const ANTHROPIC_400_INVALID_REQUEST_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 200001 tokens > 200000 maximum"},"request_id":"req_01M12ERRORENVELOPE"}"#;

/// OpenAI 401 (`openai_auth_rejected`; matrix pins `errorCode` null).
const OPENAI_401_BODY: &str = r#"{"error":{"message":"Incorrect API key provided: test-key.","type":"authentication_error","param":null,"code":null}}"#;

/// Anthropic 401 (`anthropic_auth_rejected`).
const ANTHROPIC_401_BODY: &str = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_01M12AUTH"}"#;

/// 200 body that is not JSON at all (OpenAI malformed-success case).
const NOT_JSON_BODY: &str = "this is not json";

/// 200: well-formed JSON matching neither an Anthropic `message` (missing
/// type/content/id/model/role/usage) nor an `error` envelope.
const STRUCTURALLY_EMPTY_JSON_BODY: &str = r#"{"foo": "bar"}"#;

// --- Tiny local helpers: clients, error extraction, request shape ---

fn openai_client(base_url: &str) -> OpenAiClient {
    OpenAiClient::builder()
        .api_key("test-key")
        .base_url(base_url)
        .build()
        .expect("openai build")
}

fn anthropic_client(base_url: &str) -> AnthropicClient {
    AnthropicClient::builder()
        .api_key("sk-test-key")
        .base_url(base_url)
        .build()
        .expect("anthropic build")
}

/// Rig 0.40.0 preserves a non-2xx response as `CompletionError::HttpError`
/// wrapping status + raw body: one request on `expected_path`, then `(status, parsed JSON)`.
#[rustfmt::skip]
fn extract_provider_error<T: std::fmt::Debug>(
    requests: Vec<support::CapturedRequest>,
    outcome: Result<T, CompletionError>,
    expected_path: &str,
) -> (u16, Value) {
    assert_eq!(requests.len(), 1, "one completion invocation must make exactly one HTTP request");
    assert_request_shape(&requests[0], expected_path);
    let err = outcome.expect_err("a provider HTTP error envelope must fail the completion");
    assert!(matches!(&err, CompletionError::HttpError(_)), "non-2xx must be HttpError, got {err:?}");
    let status = err.provider_response_status().expect("status preserved").as_u16();
    let body = err.provider_response_json().expect("body is JSON").expect("body present");
    (status, body)
}

/// A malformed 200 body fails closed as `JsonError` with no status/body to
/// recover. Single-request and path evidence was already asserted.
#[rustfmt::skip]
fn assert_failed_closed_parse<T: std::fmt::Debug>(outcome: Result<T, CompletionError>) {
    let err = outcome.expect_err("a malformed 200 body must fail the completion closed");
    assert!(matches!(&err, CompletionError::JsonError(_)), "must be JsonError, got {err:?}");
    assert!(err.provider_response_status().is_none(), "parse failure preserves no status");
    assert!(err.provider_response_body().is_none(), "parse failure preserves no body");
}

/// Transport shape of the single captured request: POST, JSON body of exactly
/// `Content-Length` bytes, on the documented provider path.
fn assert_request_shape(request: &support::CapturedRequest, expected_path: &str) {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), expected_path);
    assert_eq!(request.header("content-type"), Some("application/json"));
    let content_length: usize = request
        .header("content-length")
        .expect("content-length header must be present")
        .parse()
        .expect("content-length must be numeric");
    assert_eq!(content_length, request.body_len());
    assert!(request.json_body().is_object());
}

/// Recursively assert no machine-readable `code` field (any case variant) exists.
fn assert_no_code_field(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, sub) in map {
                assert!(
                    !key.eq_ignore_ascii_case("code"),
                    "the envelope must carry no machine-readable code field, found {key:?}"
                );
                assert_no_code_field(sub);
            }
        }
        Value::Array(items) => {
            for sub in items {
                assert_no_code_field(sub);
            }
        }
        _ => {}
    }
}

// --- Per-protocol case runners (tests below only supply the envelope) ---

/// OpenAI envelope case: exactly one `/responses` request, status preserved.
#[rustfmt::skip]
async fn openai_envelope_case(status: u16, body: &'static str, assert_envelope: impl FnOnce(&Value)) {
    let server = support::LoopbackServer::spawn(&[(status, body)]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let outcome = model.completion(model.completion_request("Hello").build()).await;
    let (got, envelope) = extract_provider_error(server.join(), outcome, "/responses");
    assert_eq!(got, status, "the provider HTTP status must be preserved");
    assert_envelope(&envelope);
}

/// Anthropic envelope case: exactly one `/v1/messages` request, status preserved.
#[rustfmt::skip]
async fn anthropic_envelope_case(status: u16, body: &'static str, assert_envelope: impl FnOnce(&Value)) {
    let server = support::LoopbackServer::spawn(&[(status, body)]);
    let model = anthropic_client(server.base_url()).completion_model(CLAUDE_SONNET_4_6);
    let outcome = model.completion(model.completion_request("hello").max_tokens(16).build()).await;
    let (got, envelope) = extract_provider_error(server.join(), outcome, "/v1/messages");
    assert_eq!(got, status, "the provider HTTP status must be preserved");
    assert_envelope(&envelope);
}

/// OpenAI malformed-200 case: fails closed with a parse error.
#[rustfmt::skip]
async fn openai_malformed_case(body: &'static str) {
    let server = support::LoopbackServer::spawn(&[(200, body)]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let outcome = model.completion(model.completion_request("Hello").build()).await;
    let requests = server.join();
    assert_eq!(requests.len(), 1, "exactly one HTTP request");
    assert_request_shape(&requests[0], "/responses");
    assert_failed_closed_parse(outcome);
}

/// Anthropic malformed-200 case: fails closed with a parse error.
#[rustfmt::skip]
async fn anthropic_malformed_case(body: &'static str) {
    let server = support::LoopbackServer::spawn(&[(200, body)]);
    let model = anthropic_client(server.base_url()).completion_model(CLAUDE_SONNET_4_6);
    let outcome = model.completion(model.completion_request("hello").max_tokens(16).build()).await;
    let requests = server.join();
    assert_eq!(requests.len(), 1, "exactly one HTTP request");
    assert_request_shape(&requests[0], "/v1/messages");
    assert_failed_closed_parse(outcome);
}

// --- 1. OpenAI HTTP 400: invalid_request_error / context_length_exceeded ---

/// Matrix `openai_context_length_exceeded`: fails on a 400 envelope after
/// exactly one `/responses` request, preserving status + `error.type`/`code`.
#[tokio::test(flavor = "current_thread")]
async fn openai_400_context_length_exceeded_envelope_preserved() {
    openai_envelope_case(400, OPENAI_400_CONTEXT_LENGTH_BODY, |envelope| {
        assert_eq!(envelope["error"]["type"], "invalid_request_error");
        assert_eq!(envelope["error"]["code"], "context_length_exceeded");
        assert!(envelope["error"]["message"].is_string());
    })
    .await;
}

// --- 2. Anthropic HTTP 400: invalid_request_error, no overflow code ---

/// Matrix `anthropic_invalid_request`: fails on a 400 envelope after exactly
/// one `/v1/messages` request, preserving status, envelope `type`, `error.type`,
/// and top-level `request_id` — no machine-readable overflow code anywhere,
/// so the M12 gate must not parse message prose.
#[tokio::test(flavor = "current_thread")]
async fn anthropic_400_invalid_request_envelope_preserved_without_machine_readable_code() {
    anthropic_envelope_case(400, ANTHROPIC_400_INVALID_REQUEST_BODY, |envelope| {
        assert_eq!(envelope["type"], "error");
        assert_eq!(envelope["error"]["type"], "invalid_request_error");
        assert_eq!(envelope["request_id"], "req_01M12ERRORENVELOPE");
        assert!(envelope["error"]["message"].is_string());
        assert_no_code_field(envelope);
    })
    .await;
}

// --- 3. HTTP 401 authentication_error for both protocols ---

/// Matrix `openai_auth_rejected` / `anthropic_auth_rejected`: each fails on a
/// 401 envelope after exactly one request on its own path, status preserved.
#[tokio::test(flavor = "current_thread")]
async fn both_protocols_401_authentication_error_envelopes_preserved() {
    openai_envelope_case(401, OPENAI_401_BODY, |envelope| {
        assert_eq!(envelope["error"]["type"], "authentication_error");
        assert!(envelope["error"]["code"].is_null());
    })
    .await;
    anthropic_envelope_case(401, ANTHROPIC_401_BODY, |envelope| {
        assert_eq!(envelope["type"], "error");
        assert_eq!(envelope["error"]["type"], "authentication_error");
        assert!(envelope["request_id"].is_string());
    })
    .await;
}

// --- 4. Malformed 200 bodies fail closed for both protocols ---

/// Matrix `openai_malformed_success_body` / `anthropic_malformed_success_body`
/// (stage `completed_response`): a 200 body that cannot be a valid completion
/// — unparseable JSON for OpenAI, well-formed JSON missing every required
/// field for Anthropic — fails closed as `JsonError` after one request.
#[tokio::test(flavor = "current_thread")]
async fn both_protocols_malformed_200_bodies_fail_closed() {
    openai_malformed_case(NOT_JSON_BODY).await;
    anthropic_malformed_case(STRUCTURALLY_EMPTY_JSON_BODY).await;
}
