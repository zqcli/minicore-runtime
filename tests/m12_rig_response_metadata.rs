//! M12 metadata spike: prove a future private `RigProviderAdapter` can
//! preserve allowlisted response metadata (request ID / Retry-After) that
//! rig-core 0.40.0's default error path otherwise discards, while Rig still
//! reports its normal status/body error.
//!
//! Rig's stock `reqwest::Client` unary `send` converts any non-2xx response
//! straight into `Error::InvalidStatusCodeWithMessage(status, body)` after
//! reading the body — every response header is thrown away before the
//! providers (or any adapter) could see it. Status and body survive as
//! `CompletionError::HttpError`, but a request ID or Retry-After hint does
//! not.
//!
//! This test injects a small [`MetadataHttpClient`] (around
//! [`ReqwestClient`]) through the public `ClientBuilder::http_client()` seam.
//! Its unary `send` inspects status / headers / body, copies only a closed
//! per-protocol allowlist into a shared metadata record, and then converts
//! exactly like rig's default: 2xx becomes a `Response<LazyBody<U>>` (status,
//! headers, lazy body), anything else becomes
//! `InvalidStatusCodeWithMessage(status, body)`. `send_multipart` and
//! `send_streaming` delegate unchanged — this slice covers unary metadata
//! only.
//!
//! Closed allowlist (nothing else is ever stored — no auth, no cookies, no
//! arbitrary headers, no body content):
//! - OpenAI: `x-request-id`, `retry-after`, `openai-processing-ms`
//! - Anthropic: `request-id`, `retry-after`
//!
//! No production adapter exists yet; this is evidence for the M12 gate only.

mod support;

use std::future::Future;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::{
    AssistantContent, CompletionError, CompletionModel, CompletionRequest, Message,
};
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, ReqwestClient, Response,
    StreamingResponse,
};
use rig::providers::anthropic::completion::{ANTHROPIC_VERSION_LATEST, CLAUDE_SONNET_4_6};
use rig::providers::openai::GPT_4O_MINI;
use rig::wasm_compat::WasmCompatSend;

// ---------------------------------------------------------------------------
// Metadata allowlist + capture
// ---------------------------------------------------------------------------

/// Which provider the wrapped client is talking to; selects the closed header
/// allowlist. Local to this test only — a future production adapter would
/// read its protocol from the domain model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Openai,
    Anthropic,
}

/// Captured allowlisted response metadata for one HTTP response.
///
/// The struct is closed: there is no field for any header outside the
/// allowlist, so a canary header is structurally unrepresentable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ResponseMetadata {
    request_id: Option<String>,
    retry_after: Option<String>,
    /// `openai-processing-ms`; on the OpenAI allowlist only, so it is always
    /// `None` for Anthropic.
    processing_ms: Option<String>,
}

impl ResponseMetadata {
    /// Every allowlisted value present, for canary-exclusion assertions.
    fn values(&self) -> impl Iterator<Item = &str> {
        [
            self.request_id.as_deref(),
            self.retry_after.as_deref(),
            self.processing_ms.as_deref(),
        ]
        .into_iter()
        .flatten()
    }
}

/// Test-only [`HttpClientExt`] over [`ReqwestClient`]: unary `send` is
/// reimplemented so status / headers / body can be inspected and the
/// allowlisted metadata captured before the response is converted exactly as
/// rig's default client converts it; multipart and streaming delegate.
#[derive(Clone, Debug)]
struct MetadataHttpClient {
    inner: ReqwestClient,
    protocol: Protocol,
    captured: Arc<Mutex<Vec<ResponseMetadata>>>,
}

impl MetadataHttpClient {
    fn new(inner: ReqwestClient, protocol: Protocol) -> Self {
        Self {
            inner,
            protocol,
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

// Rig's `CompletionModel` impls require `H: Default` on the client backend.
impl Default for MetadataHttpClient {
    fn default() -> Self {
        Self::new(ReqwestClient::new(), Protocol::Openai)
    }
}

impl MetadataHttpClient {
    /// Copy only the allowlisted response headers. Anything else (auth,
    /// cookies, canaries, arbitrary headers) is never stored, and neither is
    /// any body content.
    fn capture_metadata(&self, headers: &http_client::HeaderMap) {
        let mut metadata = ResponseMetadata::default();
        for (name, value) in headers {
            let Ok(value) = value.to_str() else { continue };
            match (self.protocol, name.as_str()) {
                (Protocol::Openai, "x-request-id") | (Protocol::Anthropic, "request-id") => {
                    metadata.request_id = Some(value.to_string());
                }
                (_, "retry-after") => metadata.retry_after = Some(value.to_string()),
                (Protocol::Openai, "openai-processing-ms") => {
                    metadata.processing_ms = Some(value.to_string());
                }
                _ => {}
            }
        }
        self.captured
            .lock()
            .expect("metadata mutex must not be poisoned")
            .push(metadata);
    }

    fn metadata(&self) -> Vec<ResponseMetadata> {
        self.captured
            .lock()
            .expect("metadata mutex must not be poisoned")
            .clone()
    }
}

impl HttpClientExt for MetadataHttpClient {
    // Not `async fn`: the trait's `impl Future + WasmCompatSend + 'static`
    // return bound cannot be expressed with the `async fn` sugar.
    #[allow(clippy::manual_async_fn)]
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let inner = self.inner.clone();
        let this = self.clone();
        // Convert T outside the async block so the captured request is 'static
        // even though the trait's `T` bound carries no lifetime.
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        async move {
            let req = inner
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body);
            let response = req
                .send()
                .await
                .map_err(|error| http_client::Error::Instance(Box::new(error)))?;

            // Metadata is copied from the headers before the body is touched,
            // so it survives both conversions below.
            this.capture_metadata(response.headers());

            if !response.status().is_success() {
                // Exactly rig's default error conversion (which drops every
                // header): status + exact body are preserved for Rig, and the
                // allowlisted metadata is already captured above.
                let status = response.status();
                let message = response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
                return Err(http_client::Error::InvalidStatusCodeWithMessage(
                    status, message,
                ));
            }

            // Exactly rig's default success conversion: status, headers, and
            // a lazy body, so Rig's normal successful output is unchanged.
            let mut res = Response::builder().status(response.status());
            if let Some(headers) = res.headers_mut() {
                *headers = response.headers().clone();
            }
            let body: LazyBody<U> = Box::pin(async move {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| http_client::Error::Instance(Box::new(error)))?;
                Ok(U::from(bytes))
            });
            res.body(body).map_err(http_client::Error::Protocol)
        }
    }

    // Not `async fn`: same `impl Future + WasmCompatSend + 'static` bound.
    #[allow(clippy::manual_async_fn)]
    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let inner = self.inner.clone();
        async move { inner.send_multipart(req).await }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        // Delegated unchanged: this slice proves unary metadata only.
        let inner = self.inner.clone();
        async move { inner.send_streaming(req).await }
    }
}

// ---------------------------------------------------------------------------
// Scripted provider responses
// ---------------------------------------------------------------------------

/// OpenAI 429 rate-limit error with `error.code` = `rate_limit_exceeded`.
const OPENAI_429_BODY: &str = r#"{
  "error": {
    "code": "rate_limit_exceeded",
    "message": "You are sending requests too quickly.",
    "type": "rate_limit_error"
  }
}"#;

/// Anthropic 529 overloaded error carrying a `request_id` in the body.
const ANTHROPIC_529_BODY: &str = r#"{
  "type": "error",
  "error": {
    "type": "overloaded_error",
    "message": "Overloaded"
  },
  "request_id": "req_m12_anthropic_529"
}"#;

/// OpenAI Responses success payload with a response ID and real usage.
const OPENAI_SUCCESS_BODY: &str = r#"{
  "id": "resp_m12_ok",
  "object": "response",
  "created_at": 1752000000,
  "status": "completed",
  "model": "gpt-4o-mini",
  "output": [
    {
      "type": "message",
      "id": "msg_m12_ok",
      "role": "assistant",
      "status": "completed",
      "content": [
        { "type": "output_text", "text": "Hello from the metadata loopback." }
      ]
    }
  ],
  "usage": { "input_tokens": 4, "output_tokens": 6, "total_tokens": 10 }
}"#;

/// Header value that is on neither allowlist; it must never appear in
/// captured metadata.
const CANARY_VALUE: &str = "m12-canary-never-stored";

// ---------------------------------------------------------------------------
// Client builders + request fixtures
// ---------------------------------------------------------------------------

/// Real rig OpenAI client with the metadata wrapper injected through the
/// public builder seam: `builder().http_client(...)`.
fn wrapped_openai(
    base_url: &str,
    http_client: &MetadataHttpClient,
) -> rig::providers::openai::Client<MetadataHttpClient> {
    rig::providers::openai::Client::builder()
        .api_key("test-key")
        .base_url(base_url)
        .http_client(http_client.clone())
        .build()
        .expect("openai client must build with the metadata http client")
}

/// Real rig Anthropic client with the metadata wrapper injected.
fn wrapped_anthropic(
    base_url: &str,
    http_client: &MetadataHttpClient,
) -> rig::providers::anthropic::Client<MetadataHttpClient> {
    rig::providers::anthropic::Client::builder()
        .api_key("sk-m12-metadata")
        .base_url(base_url)
        .http_client(http_client.clone())
        .build()
        .expect("anthropic client must build with the metadata http client")
}

/// The unary OpenAI Responses request: `stream` stays off (rig defaults it).
fn openai_request() -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::one(Message::user("Hello")),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
    }
}

/// Assert no captured metadata value equals the canary header value.
fn assert_canary_excluded(metadata: &[ResponseMetadata]) {
    for record in metadata {
        for value in record.values() {
            assert_ne!(
                value, CANARY_VALUE,
                "the non-allowlisted canary header must never be stored"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// OpenAI 429: rig's default error path would discard `x-request-id` /
/// `retry-after` / `openai-processing-ms`; the wrapper keeps them while Rig
/// still reports exactly status 429 + the exact body.
#[tokio::test(flavor = "current_thread")]
async fn openai_429_keeps_rig_error_and_captures_only_allowlisted_metadata() {
    let headers: &[(&str, &str)] = &[
        ("x-request-id", "req_m12_openai_429"),
        ("retry-after", "17"),
        ("openai-processing-ms", "41"),
        ("x-canary", CANARY_VALUE),
    ];
    let server = support::LoopbackServer::spawn_with_headers(&[(429, OPENAI_429_BODY, headers)]);
    let http_client = MetadataHttpClient::new(ReqwestClient::new(), Protocol::Openai);
    let model = wrapped_openai(server.base_url(), &http_client).completion_model(GPT_4O_MINI);

    let outcome = model.completion(openai_request()).await;

    // Settle the server thread before asserting (poison + join), so it can
    // never outlive the test whatever the outcome.
    let requests = server.join();
    let error = outcome.expect_err("a provider 429 must surface as a completion error");

    // Exactly one HTTP request for one completion invocation.
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method(), "POST");
    assert_eq!(requests[0].path(), "/responses");
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert_eq!(requests[0].header("authorization"), Some("Bearer test-key"));
    let wire = requests[0].json_body();
    assert_eq!(wire["model"], GPT_4O_MINI);
    assert!(
        wire.get("stream").is_none(),
        "unary completion must not set stream"
    );
    assert!(requests[0].body_len() > 0);

    // Rig keeps its normal error: status 429 + the exact body.
    assert!(matches!(error, CompletionError::HttpError(_)));
    assert_eq!(
        error
            .provider_response_status()
            .map(|status| status.as_u16()),
        Some(429)
    );
    assert_eq!(error.provider_response_body(), Some(OPENAI_429_BODY));
    let body = error
        .provider_response_json()
        .expect("error body must parse as JSON")
        .expect("error body must be present");
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");

    // Metadata: only the allowlisted fields, canary excluded.
    let metadata = http_client.metadata();
    assert_eq!(metadata.len(), 1);
    assert_eq!(
        metadata[0].request_id.as_deref(),
        Some("req_m12_openai_429")
    );
    assert_eq!(metadata[0].retry_after.as_deref(), Some("17"));
    assert_eq!(metadata[0].processing_ms.as_deref(), Some("41"));
    assert_canary_excluded(&metadata);
}

/// Anthropic 529: the header `request-id` and the body's `request_id` carry
/// the same value, proving an adapter can correlate them; only allowlisted
/// fields survive (no `openai-processing-ms` on the Anthropic allowlist).
#[tokio::test(flavor = "current_thread")]
async fn anthropic_529_metadata_request_id_matches_body_request_id() {
    let headers: &[(&str, &str)] = &[
        ("request-id", "req_m12_anthropic_529"),
        ("retry-after", "3"),
        ("x-canary", CANARY_VALUE),
    ];
    let server = support::LoopbackServer::spawn_with_headers(&[(529, ANTHROPIC_529_BODY, headers)]);
    let http_client = MetadataHttpClient::new(ReqwestClient::new(), Protocol::Anthropic);
    let model =
        wrapped_anthropic(server.base_url(), &http_client).completion_model(CLAUDE_SONNET_4_6);

    let outcome = model
        .completion(model.completion_request("hello").max_tokens(16).build())
        .await;

    let requests = server.join();
    let error = outcome.expect_err("a provider 529 must surface as a completion error");

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method(), "POST");
    assert_eq!(requests[0].path(), "/v1/messages");
    assert_eq!(
        requests[0].header("anthropic-version"),
        Some(ANTHROPIC_VERSION_LATEST)
    );
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert_eq!(requests[0].json_body()["model"], CLAUDE_SONNET_4_6);
    assert!(requests[0].body_len() > 0);

    // Rig keeps its normal error: status 529 + the exact body.
    assert!(matches!(error, CompletionError::HttpError(_)));
    assert_eq!(
        error
            .provider_response_status()
            .map(|status| status.as_u16()),
        Some(529)
    );
    assert_eq!(error.provider_response_body(), Some(ANTHROPIC_529_BODY));
    let body = error
        .provider_response_json()
        .expect("error body must parse as JSON")
        .expect("error body must be present");
    assert_eq!(body["error"]["type"], "overloaded_error");
    let body_request_id = body["request_id"]
        .as_str()
        .expect("body request_id must be a string");

    // Metadata: only the allowlisted fields, canary excluded.
    let metadata = http_client.metadata();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].request_id.as_deref(), Some(body_request_id));
    assert_eq!(metadata[0].retry_after.as_deref(), Some("3"));
    assert_eq!(
        metadata[0].processing_ms, None,
        "openai-processing-ms is not on the Anthropic allowlist"
    );
    assert_canary_excluded(&metadata);
}

/// OpenAI 200: the response body ID and the header request ID are both
/// available, and Rig's normal successful output is unchanged.
#[tokio::test(flavor = "current_thread")]
async fn openai_200_body_id_and_header_request_id_both_available() {
    let headers: &[(&str, &str)] = &[
        ("x-request-id", "req_m12_openai_ok"),
        ("openai-processing-ms", "12"),
        ("x-canary", CANARY_VALUE),
    ];
    let server =
        support::LoopbackServer::spawn_with_headers(&[(200, OPENAI_SUCCESS_BODY, headers)]);
    let http_client = MetadataHttpClient::new(ReqwestClient::new(), Protocol::Openai);
    let model = wrapped_openai(server.base_url(), &http_client).completion_model(GPT_4O_MINI);

    let outcome = model.completion(openai_request()).await;

    let requests = server.join();
    let result = outcome.expect("completion must succeed");

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path(), "/responses");
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert_eq!(requests[0].json_body()["model"], GPT_4O_MINI);
    assert!(requests[0].json_body().get("stream").is_none());
    assert!(requests[0].body_len() > 0);

    // Rig's normal successful output is untouched...
    assert_eq!(result.raw_response.id, "resp_m12_ok");
    let mut items = result.choice.iter();
    match items.next().expect("first response item") {
        AssistantContent::Text(text) => {
            assert_eq!(text.text, "Hello from the metadata loopback.");
        }
        other => panic!("expected assistant text first, got {other:?}"),
    }
    assert!(items.next().is_none(), "exactly one output item");
    assert_eq!(result.usage.input_tokens, 4);
    assert_eq!(result.usage.output_tokens, 6);
    assert_eq!(result.usage.total_tokens, 10);

    // ...and the header request ID is available alongside the body ID.
    let metadata = http_client.metadata();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].request_id.as_deref(), Some("req_m12_openai_ok"));
    assert_eq!(metadata[0].processing_ms.as_deref(), Some("12"));
    assert_eq!(metadata[0].retry_after, None);
    assert_canary_excluded(&metadata);
}
