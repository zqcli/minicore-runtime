//! M14 slice: direct private Anthropic Messages production provider adapter.
//!
//! This adapter owns the Anthropic Messages protocol wire mapping directly and
//! shares only protocol-neutral transport/framing with the OpenAI adapter (see
//! `provider_transport`); there is no generic provider response/event parser.
//! It is a child module of `model_gateway` so it can consume the existing
//! private request/result/error types without widening them.
//!
//! Frozen contracts (ADR 0138/0139, M12 fixture `docs/fixtures/provider-gate-m12`):
//! - one `generate_model_turn` issues at most one POST; the reqwest client is
//!   built with `redirect::Policy::none()`, `retry::never()` and `no_proxy()`;
//! - success is exactly an SSE `message_delta` whose `delta.stop_reason` is a
//!   non-empty string, preceded by a valid `message_start` whose `message.model`
//!   is a non-empty string exactly equal to the pinned private API model name of
//!   the request's definition (mismatch, missing, non-string, or empty fails
//!   closed as `InvalidProviderResponse`) and with all content blocks closed;
//!   `message_stop` alone is never success, and early EOF is a truthful transport
//!   failure, never a synthetic success;
//! - request headers are exactly `x-api-key`, `anthropic-version`,
//!   `Content-Type: application/json` and `Accept: text/event-stream`; the
//!   metadata header allowlist is `request-id` / `retry-after` only;
//! - error classification is structural (status or typed `error.type`), never
//!   human message matching;
//! - progress `content_index` is the eventual normalized `content[]` position
//!   of the block (provider block indexes are tracked strictly in order, but an
//!   empty preceding hidden block must never create index drift); hidden
//!   thinking is never published;
//! - cancellation before the request is `Cancelled/NotSent`; cancellation
//!   during send or read aborts by dropping the reqwest future/stream and
//!   returns `Cancelled` with a conservative delivery; a synchronously accepted
//!   `message_delta` terminal wins over any later cancellation.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::model_gateway::provider_transport::{
    SseParser, build_client, cancelled, classify_send_error, invalid_provider_response,
    invalid_request_not_sent, is_event_stream, parse_retry_after, read_bounded_envelope,
    response_byte_limit, transport_read_error,
};
use crate::model_gateway::{
    ApiModelName, ModelCallErrorReason, ModelContentDelta, ModelFinishReason, ModelProgressEvent,
    ModelProgressPublisher, ModelReasoningSummary, ModelServiceClass, ModelUsage, OutputContract,
    ProviderAttemptContent, ProviderAttemptError, ProviderAttemptRequest, ProviderAttemptResult,
    ProviderRequestDeliveryState, ProviderRequestId, ProviderResponseId, ProviderResponseMetadata,
    ReasoningContent, RedactedProviderCode, StructuredOutputContract,
};
use crate::prompt::{ModelAssistantContentRef, ModelMessage, ModelMessageRef};
use crate::tools::{ToolCallId, ToolName, ToolSpec};
use crate::wire::lexical::validate_opaque_ascii;
use crate::wire::{BoundedJsonObject, ProtocolLimits};

/// Typed, payload-free configuration error. The version and the rejected endpoint
/// details are never stored, so Debug/Display can never leak them. The API key is
/// not part of adapter configuration: it is resolved per attempt by the gateway
/// and owned by the attempt request.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by the adjacent M14 model source/catalog slice"
    )
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AnthropicProviderConfigError {
    #[error(
        "Anthropic endpoint must be an absolute http(s) URL without query, fragment, or userinfo"
    )]
    InvalidEndpoint,
    #[error("Anthropic version header must be non-empty printable ASCII within 64 bytes")]
    InvalidVersion,
    #[error("Anthropic HTTP client construction failed")]
    ClientBuild,
}

/// Direct private Anthropic Messages adapter.
///
/// The endpoint is stored exactly as validated: scheme http/https, no userinfo, no
/// query, and no fragment (all rejected at construction), but remains private and
/// is always redacted from Debug. The adapter owns no API key: the gateway resolves
/// the credential source on every attempt and the attempt request carries it for
/// header injection; the validated `anthropic-version` header is public metadata
/// and prints in Debug.
pub(crate) struct AnthropicMessagesProviderAdapter {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    version: Box<str>,
}

impl AnthropicMessagesProviderAdapter {
    /// Builds the adapter against an explicit full `/v1/messages` endpoint URL with
    /// an explicit `anthropic-version` header value. No environment or
    /// home-directory lookup ever runs.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "constructed by the adjacent M14 model source/catalog slice"
        )
    )]
    pub(crate) fn new(endpoint: &str, version: &str) -> Result<Self, AnthropicProviderConfigError> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| AnthropicProviderConfigError::InvalidEndpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(AnthropicProviderConfigError::InvalidEndpoint);
        }
        validate_opaque_ascii(version, 64)
            .map_err(|_| AnthropicProviderConfigError::InvalidVersion)?;
        let client = build_client().map_err(|_| AnthropicProviderConfigError::ClientBuild)?;
        Ok(Self {
            client,
            endpoint,
            version: version.into(),
        })
    }
}

impl fmt::Debug for AnthropicMessagesProviderAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The endpoint path is private route metadata and never prints: only a
        // redacted marker is reported. The validated anthropic-version header is
        // public metadata and continues to print.
        formatter
            .debug_struct("AnthropicMessagesProviderAdapter")
            .field("endpoint", &"<redacted>")
            .field("version", &self.version.as_ref())
            .finish()
    }
}

impl crate::model_gateway::ProviderAdapter for AnthropicMessagesProviderAdapter {
    fn execute(
        &self,
        request: ProviderAttemptRequest,
        progress: ModelProgressPublisher,
        cancel: CancellationToken,
    ) -> crate::model_gateway::ProviderAttemptFuture<'_> {
        Box::pin(self.run(request, progress, cancel))
    }
}

impl AnthropicMessagesProviderAdapter {
    async fn run(
        &self,
        request: ProviderAttemptRequest,
        progress: ModelProgressPublisher,
        cancel: CancellationToken,
    ) -> Result<ProviderAttemptResult, ProviderAttemptError> {
        // Cancellation before the request exists: NotSent.
        if cancel.is_cancelled() {
            return Err(cancelled(ProviderRequestDeliveryState::NotSent));
        }

        let body = encode_request(&request)?;
        let api_key = sensitive_api_key_header(request.credential().for_header())?;
        let http_request = self
            .client
            .post(self.endpoint.clone())
            .header("x-api-key", api_key)
            .header("anthropic-version", &*self.version)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body)
            .build()
            .map_err(|_| invalid_request_not_sent())?;

        // Second cancellation check after encoding/building but before the send
        // select: the request provably never left, so a cancellation that landed
        // during that synchronous window stays NotSent. Races once the send select
        // is polled keep the conservative Unknown below.
        if cancel.is_cancelled() {
            return Err(cancelled(ProviderRequestDeliveryState::NotSent));
        }

        // Cancellation while the POST is in flight: the request may have been received,
        // so the conservative delivery is Unknown.
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(cancelled(ProviderRequestDeliveryState::Unknown));
            }
            response = self.client.execute(http_request) => response,
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => return Err(classify_send_error(&error)),
        };

        let status = response.status();
        let request_id = response
            .headers()
            .get("request-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<ProviderRequestId>().ok());
        let retry_after = parse_retry_after(response.headers());
        if !status.is_success() {
            let delivery = http_delivery(status.as_u16());
            // The error envelope is drained bounded and cancellation-aware and its
            // typed `error.type` participates in classification where the frozen
            // matrix requires it (529). The human `error.message` is never read.
            let envelope = read_bounded_envelope(response, &cancel, delivery).await?;
            return Err(classify_http_status(
                status.as_u16(),
                envelope.as_ref(),
                retry_after,
            ));
        }
        if !is_event_stream(response.headers().get(CONTENT_TYPE)) {
            return Err(invalid_provider_response(
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ));
        }

        let mut delivery = ProviderRequestDeliveryState::AcceptedNoOutput;
        let mut parser = SseParser::new(response_byte_limit());
        let mut state = AnthropicStreamState::default();
        let expected_model = request.call().model.api_model_name();
        let mut stream = response.bytes_stream();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(cancelled(delivery));
                }
                chunk = stream.next() => {
                    let chunk = match chunk {
                        Some(Ok(chunk)) => chunk,
                        Some(Err(_)) => {
                            return Err(transport_read_error(delivery));
                        }
                        None => {
                            // Truthful early EOF: never a synthetic success.
                            return Err(transport_read_error(delivery));
                        }
                    };
                    let events = parser
                        .feed(&chunk)
                        .map_err(|_| invalid_provider_response(delivery))?;
                    for event in events {
                        match dispatch(
                            &event.data,
                            &progress,
                            &request_id,
                            &mut delivery,
                            &mut state,
                            expected_model,
                        )? {
                            Dispatch::Continue => {}
                            Dispatch::Success(result) => return Ok(*result),
                            Dispatch::Failure(error) => return Err(error),
                        }
                    }
                }
            }
        }
    }
}

/// Builds the Anthropic credential header as an explicitly sensitive value so
/// reqwest request/debug instrumentation cannot render the secret. Credential
/// validation already guarantees header-safe opaque ASCII; conversion still
/// fails closed as an unsent invalid request if that invariant is ever violated.
fn sensitive_api_key_header(value: &str) -> Result<HeaderValue, ProviderAttemptError> {
    let mut header =
        HeaderValue::from_bytes(value.as_bytes()).map_err(|_| invalid_request_not_sent())?;
    header.set_sensitive(true);
    Ok(header)
}

/// Provider-declared pre-execution rejection statuses for the Anthropic HTTP
/// matrix: every 4xx (the closed 400/401/402/413/429 plus the conservative
/// other-4xx fallback); every other status is conservatively Unknown. The 529
/// overload status is NOT declared here — its rejection proof requires the
/// complete typed in-bound envelope, so it is decided in `classify_http_status`.
fn http_delivery(status: u16) -> ProviderRequestDeliveryState {
    match status {
        400..=499 => ProviderRequestDeliveryState::RejectedBeforeExecution,
        _ => ProviderRequestDeliveryState::Unknown,
    }
}

/// HTTP error classification is structural per the M12 fixture: each status maps
/// to its closed reason and delivery — 400/413 invalid request, 401 auth rejected,
/// 402 quota exceeded, 429 rate limited (numeric Retry-After only when rate
/// limited), 529 provider unavailable with `RejectedBeforeExecution` granted only
/// when the complete in-bound JSON envelope declares `error.type ==
/// "overloaded_error"` (otherwise Unknown, never logical-retry safe), 500/504
/// provider-declared unknown, other 4xx conservatively invalid request rejected,
/// and every other 5xx/status provider unavailable unknown. The human
/// `error.message` is never inspected.
fn classify_http_status(
    status: u16,
    envelope: Option<&Value>,
    retry_after: Option<Duration>,
) -> ProviderAttemptError {
    let reason = match status {
        400 | 413 => ModelCallErrorReason::InvalidRequest,
        401 => ModelCallErrorReason::AuthRejected,
        402 => ModelCallErrorReason::QuotaExceeded,
        429 => ModelCallErrorReason::RateLimited,
        529 => ModelCallErrorReason::ProviderUnavailable,
        504 => ModelCallErrorReason::Timeout,
        500..=599 => ModelCallErrorReason::ProviderUnavailable,
        400..=499 => ModelCallErrorReason::InvalidRequest,
        _ => ModelCallErrorReason::ProviderUnavailable,
    };
    let delivery = if status == 529 {
        // RejectedBeforeExecution is granted only on a complete, in-bound envelope
        // whose typed error is exactly overloaded_error; a missing, malformed, or
        // mismatched type stays Unknown so it can never be logical-retry safe.
        let overloaded = envelope
            .and_then(|value| value.get("error"))
            .and_then(Value::as_object)
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
            == Some("overloaded_error");
        if overloaded {
            ProviderRequestDeliveryState::RejectedBeforeExecution
        } else {
            ProviderRequestDeliveryState::Unknown
        }
    } else {
        http_delivery(status)
    };
    ProviderAttemptError {
        reason,
        retry_after: if reason == ModelCallErrorReason::RateLimited {
            retry_after
        } else {
            None
        },
        delivery,
    }
}

/// SSE `error` event classification: structural on the machine-readable
/// `error.type` only. A malformed event — missing error object, or a missing,
/// non-string, or empty `error.type` — fails closed as InvalidProviderResponse
/// with the current delivery. Unknown non-empty future types (including
/// `overloaded_error`'s conservative fallback) map to ProviderUnavailable.
fn classify_stream_error(
    event: &Value,
    delivery: ProviderRequestDeliveryState,
) -> Result<ProviderAttemptError, ProviderAttemptError> {
    let error_type = event
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str);
    let reason = match error_type {
        None | Some("") => return Err(invalid_provider_response(delivery)),
        Some("rate_limit_error") => ModelCallErrorReason::RateLimited,
        Some("authentication_error") => ModelCallErrorReason::AuthRejected,
        Some("billing_error") => ModelCallErrorReason::QuotaExceeded,
        Some("invalid_request_error") | Some("request_too_large") => {
            ModelCallErrorReason::InvalidRequest
        }
        Some("timeout_error") => ModelCallErrorReason::Timeout,
        Some("api_error") | Some("overloaded_error") | Some(_) => {
            ModelCallErrorReason::ProviderUnavailable
        }
    };
    Ok(ProviderAttemptError {
        reason,
        retry_after: None,
        delivery,
    })
}

fn mark_semantic(delivery: &mut ProviderRequestDeliveryState) {
    if *delivery == ProviderRequestDeliveryState::AcceptedNoOutput {
        *delivery = ProviderRequestDeliveryState::OutputStarted;
    }
}

fn required_u64(value: &Value, key: &str) -> Result<u64, ()> {
    value.get(key).and_then(Value::as_u64).ok_or(())
}

/// Optional numeric usage field: absent and explicit null both mean absence; a
/// present non-null value must be a number.
fn optional_u64(usage: &Map<String, Value>, key: &str) -> Result<Option<u64>, ()> {
    match usage.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(()),
    }
}

/// Required numeric usage field: present non-null numbers only; absent, null,
/// or non-numeric values are malformed.
fn required_u64_field(usage: &Map<String, Value>, key: &str) -> Result<u64, ()> {
    usage.get(key).and_then(Value::as_u64).ok_or(())
}

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

/// Encodes the exact provider request body. The wire model name is the private
/// provider API model name of the exact definition bound to the request (never the
/// stable `ModelId`); the adapter carries no model name. No cache_control, beta,
/// temperature, store/include, or speculative fields are ever emitted.
fn encode_request(request: &ProviderAttemptRequest) -> Result<Vec<u8>, ProviderAttemptError> {
    let call = request.call();
    let input = call.input();

    let mut body = Map::new();
    body.insert("model".into(), json!(call.model.api_model_name().as_str()));
    body.insert(
        "max_tokens".into(),
        json!(request.effective_max_output_tokens.get()),
    );
    body.insert("stream".into(), json!(true));

    let system = input.system();
    if !system.is_empty() {
        body.insert(
            "system".into(),
            Value::Array(
                system
                    .iter()
                    .map(|section| json!({"type": "text", "text": section.text()}))
                    .collect(),
            ),
        );
    }

    let messages = encode_messages(input.messages())?;
    body.insert("messages".into(), Value::Array(messages));

    // Tools as name/description/input_schema; tool_choice is emitted only on an
    // ordinary toolful call. Tool-free calls (including NoToolCalls/Structured
    // contracts) omit both fields.
    if !input.tools_empty() {
        body.insert("tools".into(), Value::Array(encode_tools(input.tools())));
        body.insert("tool_choice".into(), json!({"type": "auto"}));
    }

    // One output_config object: structured output format and/or adaptive-thinking
    // effort live in the same object when both are present.
    let mut output_config = Map::new();
    if let Some(OutputContract::Structured(contract)) = input.output_contract() {
        output_config.insert(
            "format".into(),
            json!({
                "type": "json_schema",
                "schema": sanitize_structured_schema(contract),
            }),
        );
    }
    let (thinking, effort) = thinking_and_effort(call.model.generation().reasoning());
    if let Some(thinking) = thinking {
        body.insert("thinking".into(), thinking);
    }
    if let Some(effort) = effort {
        output_config.insert("effort".into(), json!(effort));
    }
    if !output_config.is_empty() {
        body.insert("output_config".into(), Value::Object(output_config));
    }

    body.insert(
        "service_tier".into(),
        json!(match call.model.generation().service_class() {
            ModelServiceClass::Standard => "standard_only",
            ModelServiceClass::Priority => "auto",
        }),
    );

    let bytes = serde_json::to_vec(&Value::Object(body)).map_err(|_| invalid_request_not_sent())?;
    let maximum =
        usize::try_from(ProtocolLimits::v1_0().transport.max_request_bytes).unwrap_or(usize::MAX);
    if bytes.len() > maximum {
        return Err(invalid_request_not_sent());
    }
    Ok(bytes)
}

/// Reasoning mapping: ProviderDefault omits thinking and effort entirely;
/// Disabled requests `thinking {type:disabled}`; Low/Medium/High request adaptive
/// thinking plus an `output_config.effort` level. The effort value lands in the
/// same output_config object that carries the structured format when both are
/// present.
fn thinking_and_effort(reasoning: ModelReasoningSummary) -> (Option<Value>, Option<&'static str>) {
    match reasoning {
        ModelReasoningSummary::ProviderDefault => (None, None),
        ModelReasoningSummary::Disabled => (Some(json!({"type": "disabled"})), None),
        ModelReasoningSummary::Low => (Some(json!({"type": "adaptive"})), Some("low")),
        ModelReasoningSummary::Medium => (Some(json!({"type": "adaptive"})), Some("medium")),
        ModelReasoningSummary::High => (Some(json!({"type": "adaptive"})), Some("high")),
    }
}

/// Encodes the ordered transcript as Anthropic Messages. Assistant blocks keep
/// their frozen domain order; only provider-incompatible reasoning is skipped
/// (never fabricated into a Claude block), and an assistant message whose blocks
/// all become unrepresentable is dropped whole rather than emitted empty. Fails
/// `NotSent InvalidRequest` when no representable message remains.
fn encode_messages(messages: &[ModelMessage]) -> Result<Vec<Value>, ProviderAttemptError> {
    let mut encoded = Vec::new();
    for message in messages {
        match message.as_ref() {
            ModelMessageRef::User { content } => {
                let parts: Vec<Value> = content
                    .iter()
                    .map(|part| json!({"type": "text", "text": part.as_text()}))
                    .collect();
                encoded.push(json!({"role": "user", "content": parts}));
            }
            ModelMessageRef::Assistant { content } => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in content {
                    match block.as_ref() {
                        ModelAssistantContentRef::Text(text) => {
                            blocks.push(json!({"type": "text", "text": text}));
                        }
                        ModelAssistantContentRef::Reasoning(reasoning) => {
                            if let Some(block) = encode_reasoning_block(reasoning) {
                                blocks.push(block);
                            }
                        }
                        ModelAssistantContentRef::ToolCall {
                            tool_call_id,
                            name,
                            arguments,
                        } => {
                            let input: Value = serde_json::from_str(arguments.canonical_json())
                                .expect("bounded object canonical JSON is always valid JSON");
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tool_call_id.as_str(),
                                "name": name.as_str(),
                                "input": input,
                            }));
                        }
                    }
                }
                if !blocks.is_empty() {
                    encoded.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            ModelMessageRef::Tool {
                tool_call_id,
                content,
            } => {
                // Tool output parts become the ordered array of Anthropic text
                // blocks — one block per domain part, never a newline-joined string.
                let output: Vec<Value> = content
                    .parts()
                    .iter()
                    .map(|part| json!({"type": "text", "text": part.as_text()}))
                    .collect();
                encoded.push(json!({
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": tool_call_id.as_str(),
                                 "content": output}],
                }));
            }
        }
    }
    if encoded.is_empty() {
        return Err(invalid_request_not_sent());
    }
    Ok(encoded)
}

/// Protocol-truthful reasoning replay: a Claude thinking block requires the exact
/// text with its original signature; a signature-only block (text None — the
/// normalized form of a hidden/display-omitted adaptive thinking block) replays
/// as an empty thinking block carrying the exact signature; a redacted block may
/// replay only from an unambiguously Anthropic opaque artifact (encrypted content
/// with no OpenAI provider item id, signature, text, or summary). Anything else is
/// skipped.
fn encode_reasoning_block(reasoning: &ReasoningContent) -> Option<Value> {
    if let (Some(text), Some(signature)) = (reasoning.text(), reasoning.signature()) {
        return Some(json!({
            "type": "thinking",
            "thinking": text,
            "signature": signature,
        }));
    }
    // Signature-only: the exact normalized shape of an adaptive thinking block
    // whose display text was omitted (empty thinking string, exact signature).
    // Only the pure normalized shape replays; mixed artifacts stay excluded.
    if reasoning.text().is_none()
        && reasoning.summary().is_none()
        && reasoning.encrypted().is_none()
    {
        if let Some(signature) = reasoning.signature() {
            return Some(json!({
                "type": "thinking",
                "thinking": "",
                "signature": signature,
            }));
        }
    }
    if let Some(encrypted) = reasoning.encrypted() {
        if reasoning.provider_item_id().is_none()
            && reasoning.signature().is_none()
            && reasoning.text().is_none()
            && reasoning.summary().is_none()
        {
            return Some(json!({
                "type": "redacted_thinking",
                "data": encrypted,
            }));
        }
    }
    None
}

/// ToolSpec definitions preserve name, description, and the canonical bounded
/// schema verbatim as `input_schema`; no speculative fields are emitted.
fn encode_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|spec| {
            let input_schema: Value = serde_json::from_str(spec.input_schema().canonical_json())
                .expect("bounded schema canonical JSON is always valid JSON");
            json!({
                "name": spec.name().as_str(),
                "description": spec.description(),
                "input_schema": input_schema,
            })
        })
        .collect()
}

/// Minimal recursive strict sanitizer over the already-validated local schema
/// subset (the same strictness the OpenAI adapter applies, kept private per
/// provider): only object nodes (`type:"object"` or `properties` present) get
/// `additionalProperties:false`, `properties` (an empty map when absent), and
/// `required` forced to all property names (an empty array when there are none);
/// scalar and array nodes keep only type/description/enum/const and never gain
/// `additionalProperties`. Arrays recurse into `items`, objects into `properties`.
/// The source schema is never mutated; a fresh provider copy is built (and
/// `$schema` is dropped from the wire copy).
fn sanitize_structured_schema(contract: &StructuredOutputContract) -> Value {
    let schema: Value = serde_json::from_str(contract.schema().canonical_json())
        .expect("bounded schema canonical JSON is always valid JSON");
    sanitize_schema_node(&schema)
}

fn sanitize_schema_node(node: &Value) -> Value {
    let Some(object) = node.as_object() else {
        return node.clone();
    };
    let mut sanitized = Map::new();
    for key in ["type", "description", "enum", "const"] {
        if let Some(value) = object.get(key) {
            sanitized.insert(key.into(), value.clone());
        }
    }
    if object.get("type").and_then(Value::as_str) == Some("object")
        || object.contains_key("properties")
    {
        let mut sanitized_properties = Map::new();
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            for (name, child) in properties {
                sanitized_properties.insert(name.clone(), sanitize_schema_node(child));
            }
        }
        let required: Vec<Value> = sanitized_properties
            .keys()
            .map(|name| json!(name))
            .collect();
        sanitized.insert("required".into(), Value::Array(required));
        sanitized.insert("properties".into(), Value::Object(sanitized_properties));
        sanitized.insert("additionalProperties".into(), json!(false));
    }
    if let Some(items) = object.get("items") {
        sanitized.insert("items".into(), sanitize_schema_node(items));
    }
    Value::Object(sanitized)
}

// ---------------------------------------------------------------------------
// SSE event dispatch / stream state machine
// ---------------------------------------------------------------------------

enum Dispatch {
    Continue,
    Success(Box<ProviderAttemptResult>),
    Failure(ProviderAttemptError),
}

/// Stream state over one attempt: message_start identity/usage, the ordered
/// closed content blocks, the single currently open block, and strict provider
/// block index tracking. The normalized `content[]` position of the open block
/// is `content.len()` — an empty preceding hidden block never shifts it, because
/// such a block contributes no domain block.
#[derive(Default)]
struct AnthropicStreamState {
    start_seen: bool,
    response_id: Option<ProviderResponseId>,
    service_tier: Option<RedactedProviderCode>,
    usage: UsageState,
    content: Vec<ProviderAttemptContent>,
    open: Option<OpenBlock>,
    next_provider_index: u64,
}

struct OpenBlock {
    index: u64,
    kind: OpenBlockKind,
}

enum OpenBlockKind {
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    Text {
        text: String,
    },
    ToolUse {
        id: ToolCallId,
        name: ToolName,
        /// Serialized accumulated input JSON. The `content_block_start` `input`
        /// object seeds the value and is used only when no non-empty
        /// `input_json_delta` ever arrives (per the official streaming contract
        /// the start input is an empty placeholder and the deltas carry the
        /// full JSON).
        input: String,
        has_deltas: bool,
    },
}

/// Dispatches one completed SSE frame. Event identity comes from the JSON `type`
/// field. Unknown valid event types and `ping` are ignored; a malformed JSON
/// event or a malformed required event field fails closed as
/// `InvalidProviderResponse` with the current delivery. Success is only a
/// `message_delta` with a non-empty `delta.stop_reason` after a valid
/// `message_start` whose `message.model` exactly equals the pinned private API
/// model name of the request's definition, with all content blocks closed —
/// `message_stop` is never required and never sufficient.
fn dispatch(
    data: &str,
    progress: &ModelProgressPublisher,
    request_id: &Option<ProviderRequestId>,
    delivery: &mut ProviderRequestDeliveryState,
    state: &mut AnthropicStreamState,
    expected_model: &ApiModelName,
) -> Result<Dispatch, ProviderAttemptError> {
    let value: Value =
        serde_json::from_str(data).map_err(|_| invalid_provider_response(*delivery))?;
    let Some(event_type) = value.get("type").and_then(Value::as_str) else {
        return Err(invalid_provider_response(*delivery));
    };
    match event_type {
        "message_start" => {
            handle_message_start(&value, state, expected_model)
                .map_err(|_| invalid_provider_response(*delivery))?;
        }
        "content_block_start" => {
            // A tool_use block or a redacted_thinking block is semantic output the
            // moment it starts (identity/name and the artifact are already present).
            let semantic = value
                .get("content_block")
                .and_then(Value::as_object)
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "tool_use" | "redacted_thinking"));
            handle_content_block_start(&value, state)
                .map_err(|_| invalid_provider_response(*delivery))?;
            if semantic {
                mark_semantic(delivery);
            }
        }
        "content_block_delta" => {
            handle_content_block_delta(&value, progress, delivery, state)
                .map_err(|_| invalid_provider_response(*delivery))?;
        }
        "content_block_stop" => {
            handle_content_block_stop(&value, state)
                .map_err(|_| invalid_provider_response(*delivery))?;
        }
        "message_delta" => {
            let result = handle_message_delta(&value, request_id, *delivery, state)?;
            return Ok(Dispatch::Success(Box::new(result)));
        }
        "message_stop" => {
            // Never a success proof by itself; a stop before any start is malformed.
            if !state.start_seen {
                return Err(invalid_provider_response(*delivery));
            }
        }
        "error" => {
            return Ok(Dispatch::Failure(classify_stream_error(&value, *delivery)?));
        }
        "ping" => {}
        _ => {}
    }
    Ok(Dispatch::Continue)
}

/// Parses and freezes the required `message_start` identity: exactly one valid
/// start per attempt, `message.type`/`message.role` exact, `message.model` a
/// non-empty string exactly equal to the pinned private API model name of the
/// request's definition (mismatch, missing, non-string, or empty fails), empty
/// content, null stop fields, a valid response id, and the required usage base.
fn handle_message_start(
    value: &Value,
    state: &mut AnthropicStreamState,
    expected_model: &ApiModelName,
) -> Result<(), ()> {
    if state.start_seen {
        return Err(());
    }
    let message = value.get("message").and_then(Value::as_object).ok_or(())?;
    // Frozen start grammar: the message shape is exact, and the usage object is
    // required with numeric input_tokens and output_tokens.
    if message.get("type").and_then(Value::as_str) != Some("message") {
        return Err(());
    }
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(());
    }
    // Bind the provider-reported identity to the pinned private API model name:
    // the start must carry a non-empty string exactly equal to the requested API
    // model. Missing, non-string, empty, or mismatched fails closed.
    let model = message.get("model").and_then(Value::as_str).ok_or(())?;
    if model.is_empty() || model != expected_model.as_str() {
        return Err(());
    }
    let content = message.get("content").and_then(Value::as_array).ok_or(())?;
    if !content.is_empty() {
        return Err(());
    }
    if message.get("stop_reason") != Some(&Value::Null)
        || message.get("stop_sequence") != Some(&Value::Null)
    {
        return Err(());
    }
    let response_id: ProviderResponseId = message
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or(())?;
    // The actual service tier is `message.usage.service_tier` — optional: absent
    // and explicit null mean "no tier"; present non-null must be a valid opaque
    // provider code. A top-level `message.service_tier` is not the actual tier
    // and is never read.
    let usage = message.get("usage").and_then(Value::as_object).ok_or(())?;
    let service_tier = match usage.get("service_tier") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|tier| tier.parse::<RedactedProviderCode>().ok())
                .ok_or(())?,
        ),
    };
    state.start_seen = true;
    state.response_id = Some(response_id);
    state.service_tier = service_tier;
    state.usage = UsageState::from_start(usage)?;
    Ok(())
}

fn handle_content_block_start(value: &Value, state: &mut AnthropicStreamState) -> Result<(), ()> {
    if !state.start_seen || state.open.is_some() {
        return Err(());
    }
    let index = required_u64(value, "index")?;
    if index != state.next_provider_index {
        return Err(());
    }
    let block = value
        .get("content_block")
        .and_then(Value::as_object)
        .ok_or(())?;
    let block_type = block.get("type").and_then(Value::as_str).ok_or(())?;
    let kind = match block_type {
        "thinking" => {
            // The official streaming contract always carries both strings on the
            // start block; empty is accepted (a hidden/adaptive-omitted block),
            // missing is malformed.
            let thinking = block
                .get("thinking")
                .and_then(Value::as_str)
                .ok_or(())?
                .to_owned();
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .ok_or(())?
                .to_owned();
            OpenBlockKind::Thinking {
                thinking,
                signature,
            }
        }
        "redacted_thinking" => {
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .ok_or(())?
                .to_owned();
            // Empty redacted data is rejected here, before dispatch can mark the
            // block semantic: the delivery must stay AcceptedNoOutput rather than
            // silently claim OutputStarted for an artifact that can never be
            // represented.
            if data.is_empty() {
                return Err(());
            }
            OpenBlockKind::RedactedThinking { data }
        }
        "text" => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .ok_or(())?
                .to_owned();
            OpenBlockKind::Text { text }
        }
        "tool_use" => {
            let id: ToolCallId = block
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse().ok())
                .ok_or(())?;
            let name: ToolName = block
                .get("name")
                .and_then(Value::as_str)
                .and_then(|name| name.parse().ok())
                .ok_or(())?;
            // The official streaming contract always carries a placeholder JSON
            // object on `input` (the real arguments arrive as input_json_delta
            // fragments). The seed must be present and an object: missing, null,
            // or a non-object seed fails closed, and the object seed is used only
            // when no non-empty delta ever arrives.
            let input = match block.get("input") {
                Some(value @ Value::Object(_)) => serde_json::to_string(value).map_err(|_| ())?,
                _ => return Err(()),
            };
            OpenBlockKind::ToolUse {
                id,
                name,
                input,
                has_deltas: false,
            }
        }
        _ => return Err(()),
    };
    state.next_provider_index = index.checked_add(1).ok_or(())?;
    state.open = Some(OpenBlock { index, kind });
    Ok(())
}

fn handle_content_block_delta(
    value: &Value,
    progress: &ModelProgressPublisher,
    delivery: &mut ProviderRequestDeliveryState,
    state: &mut AnthropicStreamState,
) -> Result<(), ()> {
    let index = required_u64(value, "index")?;
    let Some(open) = state.open.as_mut() else {
        return Err(());
    };
    if open.index != index {
        return Err(());
    }
    let delta = value.get("delta").and_then(Value::as_object).ok_or(())?;
    let delta_type = delta.get("type").and_then(Value::as_str).ok_or(())?;
    match (delta_type, &mut open.kind) {
        ("thinking_delta", OpenBlockKind::Thinking { thinking, .. }) => {
            let text = delta.get("thinking").and_then(Value::as_str).ok_or(())?;
            if !text.is_empty() {
                mark_semantic(delivery);
            }
            thinking.push_str(text);
        }
        ("signature_delta", OpenBlockKind::Thinking { signature, .. }) => {
            let signature_delta = delta.get("signature").and_then(Value::as_str).ok_or(())?;
            if !signature_delta.is_empty() {
                mark_semantic(delivery);
            }
            signature.push_str(signature_delta);
        }
        ("text_delta", OpenBlockKind::Text { text }) => {
            let piece = delta.get("text").and_then(Value::as_str).ok_or(())?;
            if !piece.is_empty() {
                mark_semantic(delivery);
                // The open block's eventual normalized content[] position is the
                // number of already-finalized domain blocks — never the raw
                // provider index, so an empty preceding hidden block cannot drift.
                let content_index = u32::try_from(state.content.len()).map_err(|_| ())?;
                progress.publish(ModelProgressEvent::ContentDelta {
                    content_index,
                    delta: ModelContentDelta::Text(Arc::from(piece)),
                });
            }
            text.push_str(piece);
        }
        (
            "input_json_delta",
            OpenBlockKind::ToolUse {
                input, has_deltas, ..
            },
        ) => {
            let partial = delta
                .get("partial_json")
                .and_then(Value::as_str)
                .ok_or(())?;
            if !partial.is_empty() {
                mark_semantic(delivery);
                // The start `input` object is an empty placeholder per the official
                // streaming contract: once a real delta arrives the accumulated
                // concatenation replaces the seed entirely.
                if !*has_deltas {
                    input.clear();
                    *has_deltas = true;
                }
                input.push_str(partial);
            }
        }
        _ => return Err(()),
    }
    Ok(())
}

fn handle_content_block_stop(value: &Value, state: &mut AnthropicStreamState) -> Result<(), ()> {
    let index = required_u64(value, "index")?;
    let Some(open) = state.open.take() else {
        return Err(());
    };
    if open.index != index {
        return Err(());
    }
    let block = match open.kind {
        OpenBlockKind::Thinking {
            thinking,
            signature,
        } => {
            if thinking.is_empty() && signature.is_empty() {
                // Empty hidden thinking contributes no domain block.
                return Ok(());
            }
            // Visible thinking without its signature is malformed: Claude replay
            // requires the exact signature for the exact text.
            if !thinking.is_empty() && signature.is_empty() {
                return Err(());
            }
            // Signature-only stays representable (adaptive display omission).
            ProviderAttemptContent::Reasoning(
                ReasoningContent::new(
                    (!thinking.is_empty()).then_some(thinking),
                    None,
                    None,
                    (!signature.is_empty()).then_some(signature),
                    None,
                )
                .map_err(|_| ())?,
            )
        }
        OpenBlockKind::RedactedThinking { data } => {
            // Defense in depth: the start handler already rejects empty data, so
            // an empty artifact can never reach normalization.
            if data.is_empty() {
                return Err(());
            }
            ProviderAttemptContent::Reasoning(
                ReasoningContent::new(None, None, Some(data), None, None).map_err(|_| ())?,
            )
        }
        OpenBlockKind::Text { text } => {
            if text.is_empty() {
                return Ok(());
            }
            ProviderAttemptContent::Text(Arc::from(text))
        }
        OpenBlockKind::ToolUse {
            id,
            name,
            input,
            has_deltas: _,
        } => {
            // With deltas the accumulated concatenation is the full input JSON;
            // without any delta the start `input` object is the argument value.
            let arguments: BoundedJsonObject = input.parse().map_err(|_| ())?;
            ProviderAttemptContent::ToolCall {
                tool_call_id: id,
                name,
                arguments,
            }
        }
    };
    state.content.push(block);
    Ok(())
}

fn handle_message_delta(
    value: &Value,
    request_id: &Option<ProviderRequestId>,
    delivery: ProviderRequestDeliveryState,
    state: &mut AnthropicStreamState,
) -> Result<ProviderAttemptResult, ProviderAttemptError> {
    if !state.start_seen || state.open.is_some() {
        return Err(invalid_provider_response(delivery));
    }
    let delta = value
        .get("delta")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_provider_response(delivery))?;
    // Success proof: a non-empty stop_reason. message_stop is never required.
    let stop_reason = delta
        .get("stop_reason")
        .and_then(Value::as_str)
        .filter(|stop_reason| !stop_reason.is_empty())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    // Frozen terminal grammar: stop_sequence is null or a string, and the
    // required usage object carries the required numeric cumulative
    // output_tokens; optional cumulative input/cache fields and
    // output_tokens_details may be absent or explicit null.
    match delta.get("stop_sequence") {
        None | Some(Value::Null) | Some(Value::String(_)) => {}
        _ => return Err(invalid_provider_response(delivery)),
    }
    let usage = value
        .get("usage")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_provider_response(delivery))?;
    state
        .usage
        .merge(usage)
        .map_err(|_| invalid_provider_response(delivery))?;
    // Usage finalization is fallible: a checked provider-total overflow is a
    // malformed provider response with the current delivery, never a silent
    // `usage: None`.
    let usage = state
        .usage
        .finish()
        .map_err(|_| invalid_provider_response(delivery))?;
    let raw_finish_code: RedactedProviderCode = stop_reason
        .parse()
        .map_err(|_| invalid_provider_response(delivery))?;
    Ok(ProviderAttemptResult {
        response_id: state.response_id.clone(),
        content: std::mem::take(&mut state.content).into(),
        finish_reason: map_stop_reason(stop_reason),
        usage: Some(usage),
        metadata: ProviderResponseMetadata::new(
            request_id.clone(),
            Some(raw_finish_code),
            state.service_tier.clone(),
        ),
    })
}

/// Official stop reasons: end_turn/stop_sequence complete as Stop, tool_use as
/// ToolCalls, refusal as Refused, the three incomplete reasons as Length (so the
/// ModelGateway yields IncompleteResponse), and any other non-empty reason as
/// Unknown. The caller guarantees a non-empty stop_reason.
fn map_stop_reason(stop_reason: &str) -> ModelFinishReason {
    match stop_reason {
        "end_turn" | "stop_sequence" => ModelFinishReason::Stop,
        "tool_use" => ModelFinishReason::ToolCalls,
        "refusal" => ModelFinishReason::Refused,
        "max_tokens" | "model_context_window_exceeded" | "pause_turn" => ModelFinishReason::Length,
        _ => ModelFinishReason::Unknown,
    }
}

/// Usage accumulation: message_start provides the required base (numeric
/// input_tokens and output_tokens); message_delta fields are cumulative and
/// override the base only when present and non-null, with output_tokens required
/// at the terminal. Every present non-null cumulative counter must never decrease
/// from the current value (a decrease is a malformed provider response).
/// provider_total_tokens is the checked sum of input + cache read + cache write +
/// output and its overflow fails the terminal closed; cost is never representable
/// on this protocol.
#[derive(Default)]
struct UsageState {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    thinking_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

impl UsageState {
    /// The frozen start grammar requires a usage object with numeric
    /// input_tokens and output_tokens; cache fields, output_tokens_details, and
    /// service_tier are optional (absent or explicit null).
    fn from_start(usage: &Map<String, Value>) -> Result<Self, ()> {
        let state = Self {
            input_tokens: Some(required_u64_field(usage, "input_tokens")?),
            output_tokens: Some(required_u64_field(usage, "output_tokens")?),
            cache_read_input_tokens: optional_u64(usage, "cache_read_input_tokens")?,
            cache_creation_input_tokens: optional_u64(usage, "cache_creation_input_tokens")?,
            thinking_tokens: match usage.get("output_tokens_details") {
                None | Some(Value::Null) => None,
                Some(details) => optional_u64(details.as_object().ok_or(())?, "thinking_tokens")?,
            },
        };
        Ok(state)
    }

    fn merge(&mut self, usage: &Map<String, Value>) -> Result<(), ()> {
        // Cumulative output_tokens is required at the terminal and must never
        // decrease from the start/current cumulative value.
        let output_tokens = required_u64_field(usage, "output_tokens")?;
        if self
            .output_tokens
            .is_some_and(|current| output_tokens < current)
        {
            return Err(());
        }
        self.output_tokens = Some(output_tokens);
        // The other cumulative fields stay optional and override the start base
        // only when present and non-null; a present non-null value must never
        // decrease from the current value. Null means no override.
        if let Some(value) = optional_u64(usage, "input_tokens")? {
            if self.input_tokens.is_some_and(|current| value < current) {
                return Err(());
            }
            self.input_tokens = Some(value);
        }
        if let Some(value) = optional_u64(usage, "cache_read_input_tokens")? {
            if self
                .cache_read_input_tokens
                .is_some_and(|current| value < current)
            {
                return Err(());
            }
            self.cache_read_input_tokens = Some(value);
        }
        if let Some(value) = optional_u64(usage, "cache_creation_input_tokens")? {
            if self
                .cache_creation_input_tokens
                .is_some_and(|current| value < current)
            {
                return Err(());
            }
            self.cache_creation_input_tokens = Some(value);
        }
        match usage.get("output_tokens_details") {
            None | Some(Value::Null) => {}
            Some(details) => {
                let details = details.as_object().ok_or(())?;
                if let Some(value) = optional_u64(details, "thinking_tokens")? {
                    if self.thinking_tokens.is_some_and(|current| value < current) {
                        return Err(());
                    }
                    self.thinking_tokens = Some(value);
                }
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<ModelUsage, ()> {
        let total = match (
            self.input_tokens,
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
            self.output_tokens,
        ) {
            (None, None, None, None) => None,
            (input, read, write, output) => Some(
                input
                    .unwrap_or(0)
                    .checked_add(read.unwrap_or(0))
                    .and_then(|sum| sum.checked_add(write.unwrap_or(0)))
                    .and_then(|sum| sum.checked_add(output.unwrap_or(0)))
                    .ok_or(())?,
            ),
        };
        Ok(ModelUsage::new(
            self.input_tokens,
            self.output_tokens,
            self.thinking_tokens,
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
            total,
            None,
        ))
    }
}

// ---------------------------------------------------------------------------
// Default offline contract tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::model_gateway::provider_transport::loopback::{
        CapturedRequest, LoopbackServer, ScriptedResponse,
    };
    use crate::model_gateway::tests::{
        request_for_model, request_for_model_with_tools, scripted_tool_set, structured_request,
        structured_schema,
    };
    use crate::model_gateway::{
        EffectiveModelLimits, FinalizedAssistantContent, ModelCapabilities, ModelDefinition,
        ModelDefinitionVersion, ModelGateway, ModelGenerationDefaults, ModelSelection,
        ModelSourceAdapter, ModelSourceFuture, ProviderAdapter, ReasoningCapabilities,
        ReasoningPreference, ResolveTurnModelRequest, StructuredOutputContract, TokenEstimateRate,
        TurnModelSnapshot, fixed_credential_source,
    };
    use crate::prompt::{ModelAssistantContent, ModelMessage};
    use crate::tools::ToolResultContent;

    struct SingleModelSource {
        definitions: Mutex<Vec<ModelDefinition>>,
    }

    impl SingleModelSource {
        fn new(definition: ModelDefinition) -> Self {
            Self {
                definitions: Mutex::new(vec![definition]),
            }
        }
    }

    impl ModelSourceAdapter for SingleModelSource {
        fn discover(&self) -> ModelSourceFuture<'_> {
            let definitions = self.definitions.lock().unwrap().clone();
            Box::pin(async move { Ok(definitions) })
        }
    }

    fn anthropic_selection() -> ModelSelection {
        ModelSelection::new(
            "anthropic".parse().unwrap(),
            "claude-sonnet-4-6".parse().unwrap(),
        )
    }

    fn definition_with(
        reasoning: ModelReasoningSummary,
        service_class: ModelServiceClass,
        max_schema_bytes: Option<NonZeroU32>,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> ModelDefinition {
        definition_with_credential(
            reasoning,
            service_class,
            max_schema_bytes,
            adapter,
            "sk-test",
        )
    }

    fn definition_with_credential(
        reasoning: ModelReasoningSummary,
        service_class: ModelServiceClass,
        max_schema_bytes: Option<NonZeroU32>,
        adapter: Arc<dyn ProviderAdapter>,
        credential: &str,
    ) -> ModelDefinition {
        let capabilities = ModelCapabilities::text_only(ReasoningCapabilities::all(), true);
        let limits = match max_schema_bytes {
            Some(maximum) => {
                EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192))
                    .with_max_schema_bytes(maximum)
            }
            None => EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192)),
        };
        ModelDefinition::new(
            anthropic_selection(),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            "claude-sonnet-4-6".parse().unwrap(),
            match max_schema_bytes {
                Some(_) => capabilities.with_structured_json_schema(),
                None => capabilities,
            },
            limits,
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(NonZeroU32::new(4_096).unwrap(), reasoning, service_class),
            adapter,
            fixed_credential_source(credential),
        )
        .unwrap()
    }

    async fn gateway_and_model(
        definition: ModelDefinition,
    ) -> (Arc<ModelGateway>, Arc<TurnModelSnapshot>) {
        let source = Arc::new(SingleModelSource::new(definition));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = Arc::new(ModelGateway::new(vec![source_adapter]));
        let model = gateway
            .resolve_for_turn(
                gateway.initialize().await.unwrap(),
                ResolveTurnModelRequest::new(
                    anthropic_selection(),
                    ReasoningPreference::Auto,
                    None,
                ),
            )
            .unwrap();
        (gateway, model)
    }

    fn sse(events: &[Value]) -> String {
        let mut body = String::new();
        for event in events {
            body.push_str("data: ");
            body.push_str(&event.to_string());
            body.push_str("\n\n");
        }
        body
    }

    fn message_start(id: &str, service_tier: Option<&str>, input_tokens: u64) -> Value {
        message_start_with_model(id, service_tier, input_tokens, "claude-sonnet-4-6")
    }

    fn message_start_with_model(
        id: &str,
        service_tier: Option<&str>,
        input_tokens: u64,
        model: &str,
    ) -> Value {
        let mut message = serde_json::Map::new();
        message.insert("id".into(), json!(id));
        message.insert("type".into(), json!("message"));
        message.insert("role".into(), json!("assistant"));
        message.insert("model".into(), json!(model));
        message.insert("content".into(), Value::Array(Vec::new()));
        message.insert("stop_reason".into(), Value::Null);
        message.insert("stop_sequence".into(), Value::Null);
        // The actual service tier lives on the usage object, not the message.
        let mut usage = serde_json::Map::new();
        usage.insert("input_tokens".into(), json!(input_tokens));
        usage.insert("cache_creation_input_tokens".into(), json!(0));
        usage.insert("cache_read_input_tokens".into(), json!(0));
        usage.insert("output_tokens".into(), json!(1));
        if let Some(tier) = service_tier {
            usage.insert("service_tier".into(), json!(tier));
        }
        message.insert("usage".into(), Value::Object(usage));
        json!({"type": "message_start", "message": Value::Object(message)})
    }

    /// The pinned private API model name every fixture definition installs.
    fn expected_api_model() -> ApiModelName {
        "claude-sonnet-4-6".parse().unwrap()
    }

    /// Removes `message.model` from a message_start event entirely.
    fn without_start_model(start: Value) -> Value {
        let mut start = start;
        start["message"]
            .as_object_mut()
            .expect("the start message is an object")
            .remove("model");
        start
    }

    fn content_block_start(index: u64, block: Value) -> Value {
        json!({"type": "content_block_start", "index": index, "content_block": block})
    }

    fn content_block_delta(index: u64, delta: Value) -> Value {
        json!({"type": "content_block_delta", "index": index, "delta": delta})
    }

    fn content_block_stop(index: u64) -> Value {
        json!({"type": "content_block_stop", "index": index})
    }

    fn thinking_block_start(index: u64) -> Value {
        content_block_start(
            index,
            json!({"type": "thinking", "thinking": "", "signature": ""}),
        )
    }

    fn text_block_start(index: u64) -> Value {
        content_block_start(index, json!({"type": "text", "text": ""}))
    }

    fn tool_use_block_start(index: u64, id: &str, name: &str) -> Value {
        content_block_start(
            index,
            json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
        )
    }

    fn thinking_delta(index: u64, text: &str) -> Value {
        content_block_delta(index, json!({"type": "thinking_delta", "thinking": text}))
    }

    fn signature_delta(index: u64, signature: &str) -> Value {
        content_block_delta(
            index,
            json!({"type": "signature_delta", "signature": signature}),
        )
    }

    fn text_delta(index: u64, text: &str) -> Value {
        content_block_delta(index, json!({"type": "text_delta", "text": text}))
    }

    fn input_json_delta(index: u64, partial: &str) -> Value {
        content_block_delta(
            index,
            json!({"type": "input_json_delta", "partial_json": partial}),
        )
    }

    fn message_delta(stop_reason: &str, output_tokens: u64) -> Value {
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": output_tokens},
        })
    }

    /// Full terminal Anthropic sequence: thinking (deltas + signature), text,
    /// tool_use (input_json deltas), and a message_delta with a `tool_use`
    /// stop_reason and cumulative usage — deliberately WITHOUT message_stop.
    fn rich_terminal_events() -> Vec<Value> {
        vec![
            message_start("msg_rich", Some("standard"), 25),
            thinking_block_start(0),
            thinking_delta(0, "Let me think about this."),
            signature_delta(0, "sig_rich"),
            content_block_stop(0),
            text_block_start(1),
            text_delta(1, "The weather"),
            text_delta(1, " in Paris is 22°C."),
            content_block_stop(1),
            tool_use_block_start(2, "toolu_1", "echo"),
            input_json_delta(2, "{\"city\": \"Ber"),
            input_json_delta(2, "lin\"}"),
            content_block_stop(2),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {
                    "output_tokens": 19,
                    "cache_creation_input_tokens": 11,
                    "cache_read_input_tokens": 9,
                    "output_tokens_details": {"thinking_tokens": 6},
                },
            }),
        ]
    }

    fn drain(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ModelProgressEvent>,
    ) -> Vec<ModelProgressEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn assert_request_shape(request: &CapturedRequest, credential: &str, version: &str) {
        assert_eq!(request.method(), "POST");
        assert_eq!(request.path(), "/v1/messages");
        assert_eq!(request.header("x-api-key"), Some(credential));
        assert_eq!(request.header("anthropic-version"), Some(version));
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        let content_length: usize = request
            .header("content-length")
            .expect("content-length header must be present")
            .parse()
            .expect("content-length must be numeric");
        assert_eq!(
            content_length,
            request.body_len(),
            "the server must read exactly Content-Length body bytes"
        );
    }

    // ------------------------------------------------------------------
    // Pure config/encoder/parser unit tests
    // ------------------------------------------------------------------

    #[test]
    fn adapter_configuration_rejects_unsafe_endpoints_and_versions() {
        for endpoint in [
            "ftp://api.anthropic.com/v1/messages",
            "ws://api.anthropic.com/v1/messages",
            "https://api.anthropic.com/v1/messages?key=SECRET",
            "https://api.anthropic.com/v1/messages?x=1",
            "https://api.anthropic.com/v1/messages#fragment",
            "https://user:pass@api.anthropic.com/v1/messages",
            "https://user@api.anthropic.com/v1/messages",
            "not a url",
            "",
        ] {
            assert_eq!(
                AnthropicMessagesProviderAdapter::new(endpoint, "2023-06-01").unwrap_err(),
                AnthropicProviderConfigError::InvalidEndpoint,
                "endpoint {endpoint:?} was accepted"
            );
        }
        let oversize_version = "x".repeat(65);
        for version in ["", "has space", "bad\nversion", oversize_version.as_str()] {
            assert_eq!(
                AnthropicMessagesProviderAdapter::new(
                    "https://api.anthropic.com/v1/messages",
                    version,
                )
                .unwrap_err(),
                AnthropicProviderConfigError::InvalidVersion,
                "version {version:?} was accepted"
            );
        }
        let adapter = AnthropicMessagesProviderAdapter::new(
            "https://api.anthropic.com/v1/messages",
            "2023-06-01",
        )
        .unwrap();
        let debug = format!("{adapter:?}");
        assert!(
            !debug.contains("sk-"),
            "the adapter must never store or print an API key: {debug}"
        );
        assert!(
            debug.contains("2023-06-01"),
            "the validated anthropic-version is public metadata and must be printed: {debug}"
        );
        assert!(
            !debug.contains("api.anthropic.com") && !debug.contains("/v1/messages"),
            "the adapter Debug must redact the endpoint path: {debug}"
        );
        // Loopback http endpoints are accepted by the adapter; the explicit endpoint
        // policy lives at the provider-installation boundary.
        assert!(
            AnthropicMessagesProviderAdapter::new(
                "http://127.0.0.1:1234/v1/messages",
                "2023-06-01",
            )
            .is_ok()
        );
    }

    #[test]
    fn api_key_header_is_sensitive_and_request_debug_redacts_it() {
        let secret = "sk-ANTHROPIC-SECRET";
        let header = sensitive_api_key_header(secret).expect("the test API key is header-safe");
        assert!(
            header.is_sensitive(),
            "the Anthropic credential header must be explicitly sensitive"
        );
        let request = build_client()
            .expect("the locked-down test client builds")
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", header)
            .build()
            .expect("the test request builds");
        assert!(
            request
                .headers()
                .get("x-api-key")
                .is_some_and(HeaderValue::is_sensitive),
            "request construction must preserve the sensitive marker"
        );
        let debug = format!("{request:?}");
        assert!(
            !debug.contains(secret),
            "reqwest Request Debug leaked the Anthropic credential: {debug}"
        );
    }

    #[test]
    fn thinking_and_effort_map_closed_levels() {
        assert_eq!(
            thinking_and_effort(ModelReasoningSummary::ProviderDefault),
            (None, None)
        );
        assert_eq!(
            thinking_and_effort(ModelReasoningSummary::Disabled),
            (Some(json!({"type": "disabled"})), None)
        );
        assert_eq!(
            thinking_and_effort(ModelReasoningSummary::Low),
            (Some(json!({"type": "adaptive"})), Some("low"))
        );
        assert_eq!(
            thinking_and_effort(ModelReasoningSummary::Medium),
            (Some(json!({"type": "adaptive"})), Some("medium"))
        );
        assert_eq!(
            thinking_and_effort(ModelReasoningSummary::High),
            (Some(json!({"type": "adaptive"})), Some("high"))
        );
    }

    #[test]
    fn encode_messages_applies_anthropic_replay_rules() {
        let user = ModelMessage::unstamped_user_text(Arc::from("What's the weather?")).unwrap();
        // Exact text + original signature replays as a Claude thinking block (the
        // OpenAI item id is irrelevant for Anthropic replay).
        let replayable = ReasoningContent::new(
            Some("thought text".to_owned()),
            Some("summary".to_owned()),
            Some("encrypted".to_owned()),
            Some("sig_1".to_owned()),
            Some("rs_1".parse().unwrap()),
        )
        .unwrap();
        // Orphan reasoning (text only, no signature) is provider-incompatible and
        // must be skipped, never fabricated into a Claude block.
        let orphan =
            ReasoningContent::new(Some("orphan".to_owned()), None, None, None, None).unwrap();
        // Encrypted content with an OpenAI provider item id is an OpenAI artifact:
        // not unambiguously Anthropic, so it is skipped too.
        let openai_encrypted = ReasoningContent::new(
            None,
            None,
            Some("encrypted".to_owned()),
            None,
            Some("rs_2".parse().unwrap()),
        )
        .unwrap();
        // Encrypted content with no item id/signature/text/summary is an
        // unambiguously Anthropic opaque artifact: replays as redacted_thinking.
        let anthropic_redacted =
            ReasoningContent::new(None, None, Some("redacted-data".to_owned()), None, None)
                .unwrap();
        // Summary-only reasoning has no Claude representation and is skipped.
        let summary_only =
            ReasoningContent::new(None, Some("summary".to_owned()), None, None, None).unwrap();
        let assistant = ModelMessage::assistant(Arc::from([
            ModelAssistantContent::reasoning(replayable),
            ModelAssistantContent::reasoning(orphan.clone()),
            ModelAssistantContent::reasoning(openai_encrypted),
            ModelAssistantContent::reasoning(anthropic_redacted),
            ModelAssistantContent::reasoning(summary_only),
            ModelAssistantContent::text(Arc::from("assistant says")).unwrap(),
            ModelAssistantContent::tool_call(
                "call_abc".parse().unwrap(),
                "get_weather".parse().unwrap(),
                r#"{"city":"Paris"}"#.parse().unwrap(),
            ),
        ]))
        .unwrap();
        let tool = ModelMessage::tool_result(
            "call_abc".parse().unwrap(),
            ToolResultContent::from_text_parts(vec!["22°C and sunny".to_owned()]).unwrap(),
        );

        let messages = encode_messages(&[user, assistant, tool]).unwrap();
        let wire = serde_json::to_string(&Value::Array(messages.clone())).unwrap();
        assert!(
            !wire.contains("orphan"),
            "orphan reasoning must never be replayed"
        );
        assert!(
            !wire.contains("summary"),
            "summary-only reasoning must never be replayed"
        );
        assert_eq!(
            Value::Array(messages),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "What's the weather?"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "thought text", "signature": "sig_1"},
                    {"type": "redacted_thinking", "data": "redacted-data"},
                    {"type": "text", "text": "assistant says"},
                    {"type": "tool_use", "id": "call_abc", "name": "get_weather",
                     "input": {"city": "Paris"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_abc",
                     "content": [{"type": "text", "text": "22°C and sunny"}]},
                ]},
            ])
        );

        // An assistant message whose only block is unrepresentable is dropped
        // whole; a transcript with no representable message at all fails
        // NotSent InvalidRequest.
        let orphan_only = ModelMessage::assistant(Arc::from([ModelAssistantContent::reasoning(
            orphan.clone(),
        )]))
        .unwrap();
        assert_eq!(
            encode_messages(&[orphan_only]).unwrap_err().reason,
            ModelCallErrorReason::InvalidRequest
        );
        assert_eq!(
            encode_messages(&[]).unwrap_err().reason,
            ModelCallErrorReason::InvalidRequest
        );
    }

    #[test]
    fn signature_only_thinking_round_trips_receive_normalize_encode() {
        // Receive: a hidden adaptive thinking block whose display text is omitted
        // (empty thinking string, exact signature delivered) normalizes to
        // signature-only ReasoningContent (text None, signature Some).
        let received = dispatch_events(&[
            message_start("msg_rt", None, 5),
            content_block_start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            signature_delta(0, "sig_rt"),
            content_block_stop(0),
            text_block_start(1),
            text_delta(1, "visible"),
            content_block_stop(1),
            message_delta("end_turn", 3),
        ])
        .unwrap()
        .unwrap();
        let mut reasoning = None;
        for content in received.content.iter() {
            if let ProviderAttemptContent::Reasoning(block) = content {
                reasoning = Some(block);
            }
        }
        let reasoning =
            reasoning.expect("signature-only thinking must normalize to a reasoning block");
        assert_eq!(reasoning.text(), None);
        assert_eq!(reasoning.signature(), Some("sig_rt"));
        assert_eq!(reasoning.encrypted(), None);

        // Encode: the normalized form replays as an empty thinking block carrying
        // the exact signature.
        let assistant = ModelMessage::assistant(Arc::from([
            ModelAssistantContent::reasoning(reasoning.clone()),
            ModelAssistantContent::text(Arc::from("visible")).unwrap(),
        ]))
        .unwrap();
        let user = ModelMessage::unstamped_user_text(Arc::from("continue")).unwrap();
        let wire = Value::Array(encode_messages(&[user, assistant]).unwrap());
        assert_eq!(
            wire,
            json!([
                {"role": "user", "content": [{"type": "text", "text": "continue"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig_rt"},
                    {"type": "text", "text": "visible"},
                ]},
            ])
        );

        // Visible thinking also round-trips through the same receive path with its
        // exact text + signature (never an empty-thinking rewrite).
        let rich = dispatch_events(&rich_terminal_events()).unwrap().unwrap();
        let visible = match &rich.content[0] {
            ProviderAttemptContent::Reasoning(block) => block.clone(),
            other => panic!("expected visible thinking first, got {other:?}"),
        };
        assert_eq!(visible.text(), Some("Let me think about this."));
        assert_eq!(visible.signature(), Some("sig_rich"));
        assert_eq!(
            encode_reasoning_block(&visible),
            Some(json!({
                "type": "thinking",
                "thinking": "Let me think about this.",
                "signature": "sig_rich",
            }))
        );
    }

    #[test]
    fn encode_messages_tool_result_parts_are_ordered_text_blocks_not_joined() {
        let tool = ModelMessage::tool_result(
            "call_two_parts".parse().unwrap(),
            ToolResultContent::from_text_parts(vec![
                "first part".to_owned(),
                "second part".to_owned(),
            ])
            .unwrap(),
        );
        let wire = Value::Array(encode_messages(&[tool]).unwrap());
        assert_eq!(
            wire,
            json!([
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_two_parts",
                     "content": [
                        {"type": "text", "text": "first part"},
                        {"type": "text", "text": "second part"},
                     ]},
                ]},
            ]),
            "each domain part must be its own Anthropic text block"
        );
        // The ordered-array shape is exactly what distinguishes ["a","b"] from
        // ["a\nb"]: the newline-joined string form must never be emitted.
        assert_ne!(
            wire,
            json!([
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_two_parts",
                     "content": "first part\nsecond part"},
                ]},
            ]),
            "tool result parts must not be newline-joined"
        );
    }

    #[test]
    fn structured_sanitizer_forces_strict_object_shape() {
        let model =
            TurnModelSnapshot::test_fixture_with_structured(None, NonZeroU32::new(65_536).unwrap());
        let contract = StructuredOutputContract::new(
            &model,
            None,
            structured_schema(
                r#"{
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "description": "report",
                    "required": ["status"],
                    "properties": {
                        "status": {"type": "string", "enum": ["ok", "bad"]},
                        "meta": {
                            "type": "object",
                            "properties": {"nested": {"type": "array", "items": {"type": "integer"}}}
                        }
                    }
                }"#,
            ),
        )
        .unwrap();
        assert_eq!(
            sanitize_structured_schema(&contract),
            json!({
                "type": "object",
                "description": "report",
                "additionalProperties": false,
                "required": ["meta", "status"],
                "properties": {
                    "status": {"type": "string", "enum": ["ok", "bad"]},
                    "meta": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["nested"],
                        "properties": {
                            "nested": {"type": "array", "items": {"type": "integer"}}
                        }
                    }
                }
            })
        );
        let empty =
            StructuredOutputContract::new(&model, None, structured_schema(r#"{"type":"object"}"#))
                .unwrap();
        assert_eq!(
            sanitize_structured_schema(&empty),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": [],
                "properties": {},
            })
        );
    }

    #[test]
    fn fragmented_sse_terminal_without_message_stop_is_success() {
        let body = sse(&rich_terminal_events());
        let progress_events = Arc::new(Mutex::new(Vec::new()));
        let captured_progress = Arc::clone(&progress_events);
        let progress = ModelProgressPublisher::new(move |event| {
            captured_progress.lock().unwrap().push(event);
        });
        let mut parser = SseParser::new(response_byte_limit());
        let mut state = AnthropicStreamState::default();
        let mut delivery = ProviderRequestDeliveryState::AcceptedNoOutput;
        let mut terminal = None;
        // Arbitrary byte-by-byte fragmentation through the shared SSE framing.
        for byte in body.bytes() {
            for event in parser.feed(&[byte]).expect("fragmented feed must parse") {
                match dispatch(
                    &event.data,
                    &progress,
                    &None,
                    &mut delivery,
                    &mut state,
                    &expected_api_model(),
                )
                .expect("fragmented terminal events must dispatch")
                {
                    Dispatch::Continue => {}
                    Dispatch::Success(result) => terminal = Some(*result),
                    Dispatch::Failure(error) => {
                        panic!("fragmented terminal failed: {error:?}")
                    }
                }
            }
        }
        let terminal = terminal.expect("message_delta with stop_reason is the success proof");

        assert_eq!(terminal.finish_reason, ModelFinishReason::ToolCalls);
        assert_eq!(terminal.response_id.unwrap().as_str(), "msg_rich");
        assert_eq!(
            terminal.metadata.raw_finish_code.unwrap().as_str(),
            "tool_use"
        );
        assert_eq!(terminal.metadata.service_tier.unwrap().as_str(), "standard");
        assert_eq!(terminal.metadata.provider_request_id, None);
        assert_eq!(terminal.content.len(), 3);
        match &terminal.content[0] {
            ProviderAttemptContent::Reasoning(reasoning) => {
                assert_eq!(reasoning.text(), Some("Let me think about this."));
                assert_eq!(reasoning.signature(), Some("sig_rich"));
                assert_eq!(reasoning.encrypted(), None);
                assert_eq!(reasoning.provider_item_id(), None);
            }
            other => panic!("expected thinking first, got {other:?}"),
        }
        match &terminal.content[1] {
            ProviderAttemptContent::Text(text) => {
                assert_eq!(&**text, "The weather in Paris is 22°C.");
            }
            other => panic!("expected text second, got {other:?}"),
        }
        match &terminal.content[2] {
            ProviderAttemptContent::ToolCall {
                tool_call_id,
                name,
                arguments,
            } => {
                assert_eq!(tool_call_id.as_str(), "toolu_1");
                assert_eq!(name.as_str(), "echo");
                assert_eq!(arguments.canonical_json(), r#"{"city":"Berlin"}"#);
            }
            other => panic!("expected tool call third, got {other:?}"),
        }
        let usage = terminal.usage.unwrap();
        assert_eq!(usage.input_tokens(), Some(25));
        assert_eq!(usage.output_tokens(), Some(19));
        assert_eq!(usage.reasoning_tokens(), Some(6));
        assert_eq!(usage.cache_read_tokens(), Some(9));
        assert_eq!(usage.cache_write_tokens(), Some(11));
        assert_eq!(usage.provider_total_tokens(), Some(25 + 9 + 11 + 19));
        assert_eq!(usage.reported_cost(), None);
        assert_eq!(delivery, ProviderRequestDeliveryState::OutputStarted);

        // Only non-empty text deltas publish, at the eventual normalized
        // content[] position (the thinking block already occupies position 0).
        assert_eq!(
            *progress_events.lock().unwrap(),
            [
                ModelProgressEvent::ContentDelta {
                    content_index: 1,
                    delta: ModelContentDelta::Text(Arc::from("The weather")),
                },
                ModelProgressEvent::ContentDelta {
                    content_index: 1,
                    delta: ModelContentDelta::Text(Arc::from(" in Paris is 22°C.")),
                },
            ]
        );
    }

    #[test]
    fn http_status_529_requires_typed_overloaded_envelope() {
        // The complete in-bound envelope declaring overloaded_error grants
        // RejectedBeforeExecution; the reason stays ProviderUnavailable and
        // Retry-After is never claimed for it.
        let overloaded = json!({"type": "error", "error": {"type": "overloaded_error"}});
        let error = classify_http_status(529, Some(&overloaded), Some(Duration::from_secs(17)));
        assert_eq!(error.reason, ModelCallErrorReason::ProviderUnavailable);
        assert_eq!(
            error.delivery,
            ProviderRequestDeliveryState::RejectedBeforeExecution
        );
        assert_eq!(
            error.retry_after, None,
            "Retry-After is only exposed for RateLimited"
        );

        // Missing, malformed, or mismatched envelopes stay ProviderUnavailable
        // with Unknown delivery and never claim Retry-After.
        for (label, envelope) in [
            ("unparseable", None),
            (
                "missing type",
                Some(json!({"type": "error", "error": {"message": "boom"}})),
            ),
            (
                "mismatched type",
                Some(json!({"type": "error", "error": {"type": "api_error"}})),
            ),
            (
                "non-string type",
                Some(json!({"type": "error", "error": {"type": 42}})),
            ),
            ("missing error object", Some(json!({"type": "error"}))),
        ] {
            let error = classify_http_status(529, envelope.as_ref(), Some(Duration::from_secs(17)));
            assert_eq!(
                error.reason,
                ModelCallErrorReason::ProviderUnavailable,
                "529 {label}"
            );
            assert_eq!(
                error.delivery,
                ProviderRequestDeliveryState::Unknown,
                "529 {label} must not be logical-retry safe"
            );
            assert_eq!(
                error.retry_after, None,
                "529 {label} must never claim Retry-After"
            );
        }
    }

    #[test]
    fn stream_error_classification_is_structural_on_error_type() {
        let cases: &[(&str, ModelCallErrorReason)] = &[
            ("rate_limit_error", ModelCallErrorReason::RateLimited),
            ("authentication_error", ModelCallErrorReason::AuthRejected),
            ("billing_error", ModelCallErrorReason::QuotaExceeded),
            (
                "invalid_request_error",
                ModelCallErrorReason::InvalidRequest,
            ),
            ("request_too_large", ModelCallErrorReason::InvalidRequest),
            ("timeout_error", ModelCallErrorReason::Timeout),
            ("api_error", ModelCallErrorReason::ProviderUnavailable),
            (
                "overloaded_error",
                ModelCallErrorReason::ProviderUnavailable,
            ),
            (
                "some_future_error_type",
                ModelCallErrorReason::ProviderUnavailable,
            ),
        ];
        for (error_type, expected) in cases {
            let event = json!({"type": "error",
                               "error": {"type": error_type, "message": "boom"}});
            let error = classify_stream_error(&event, ProviderRequestDeliveryState::OutputStarted)
                .expect("typed stream error must classify");
            assert_eq!(error.reason, *expected, "error.type {error_type:?}");
            assert_eq!(
                error.delivery,
                ProviderRequestDeliveryState::OutputStarted,
                "error.type {error_type:?} must keep the current delivery"
            );
        }

        // Malformed error events fail closed as InvalidProviderResponse with the
        // current delivery: missing error object, missing type, non-string type,
        // and empty type are all structurally malformed, never provider-declared.
        for (index, event) in [
            json!({"type": "error"}),
            json!({"type": "error", "error": {"message": "boom"}}),
            json!({"type": "error", "error": {"type": 42, "message": "boom"}}),
            json!({"type": "error", "error": {"type": "", "message": "boom"}}),
        ]
        .iter()
        .enumerate()
        {
            let error = classify_stream_error(event, ProviderRequestDeliveryState::OutputStarted)
                .expect_err("malformed stream error must fail closed");
            assert_eq!(
                error.reason,
                ModelCallErrorReason::InvalidProviderResponse,
                "malformed case {index}"
            );
            assert_eq!(
                error.delivery,
                ProviderRequestDeliveryState::OutputStarted,
                "malformed case {index} must keep the current delivery"
            );
        }
    }

    /// Runs events through the real dispatch path, returning the terminal result
    /// (or the first failure).
    fn dispatch_events(
        events: &[Value],
    ) -> Result<Option<ProviderAttemptResult>, ProviderAttemptError> {
        let progress = ModelProgressPublisher::discard();
        let mut state = AnthropicStreamState::default();
        let mut delivery = ProviderRequestDeliveryState::AcceptedNoOutput;
        let mut terminal = None;
        for event in events {
            match dispatch(
                &event.to_string(),
                &progress,
                &None,
                &mut delivery,
                &mut state,
                &expected_api_model(),
            ) {
                Ok(Dispatch::Continue) => {}
                Ok(Dispatch::Success(result)) => terminal = Some(*result),
                Ok(Dispatch::Failure(error)) | Err(error) => return Err(error),
            }
        }
        Ok(terminal)
    }

    /// Dispatches events expecting a failure and returns the error together with
    /// the delivery at the exact failure point.
    fn dispatch_expect_error(
        events: &[Value],
    ) -> (ProviderAttemptError, ProviderRequestDeliveryState) {
        let progress = ModelProgressPublisher::discard();
        let mut state = AnthropicStreamState::default();
        let mut delivery = ProviderRequestDeliveryState::AcceptedNoOutput;
        for event in events {
            match dispatch(
                &event.to_string(),
                &progress,
                &None,
                &mut delivery,
                &mut state,
                &expected_api_model(),
            ) {
                Ok(Dispatch::Continue) => {}
                Ok(Dispatch::Success(_)) => panic!("expected an error, got a terminal success"),
                Ok(Dispatch::Failure(error)) | Err(error) => return (error, delivery),
            }
        }
        panic!("expected an error, stream completed without one")
    }

    #[test]
    fn frozen_message_start_grammar_fails_closed() {
        fn start_with(message: serde_json::Map<String, Value>) -> Value {
            json!({"type": "message_start", "message": Value::Object(message)})
        }
        fn base_message() -> serde_json::Map<String, Value> {
            let mut message = serde_json::Map::new();
            message.insert("id".into(), json!("msg_grammar"));
            message.insert("type".into(), json!("message"));
            message.insert("role".into(), json!("assistant"));
            message.insert("model".into(), json!("claude-sonnet-4-6"));
            message.insert("content".into(), Value::Array(Vec::new()));
            message.insert("stop_reason".into(), Value::Null);
            message.insert("stop_sequence".into(), Value::Null);
            message.insert(
                "usage".into(),
                json!({"input_tokens": 5, "output_tokens": 1}),
            );
            message
        }

        let cases: Vec<(&str, Value)> = vec![
            (
                "type is not message",
                start_with({
                    let mut message = base_message();
                    message.insert("type".into(), json!("error"));
                    message
                }),
            ),
            (
                "role is not assistant",
                start_with({
                    let mut message = base_message();
                    message.insert("role".into(), json!("user"));
                    message
                }),
            ),
            (
                "model missing",
                start_with({
                    let mut message = base_message();
                    message.remove("model");
                    message
                }),
            ),
            (
                "model empty",
                start_with({
                    let mut message = base_message();
                    message.insert("model".into(), json!(""));
                    message
                }),
            ),
            (
                "model mismatched",
                start_with({
                    let mut message = base_message();
                    message.insert("model".into(), json!("claude-sonnet-4-5"));
                    message
                }),
            ),
            (
                "model non-string",
                start_with({
                    let mut message = base_message();
                    message.insert("model".into(), json!(42));
                    message
                }),
            ),
            (
                "content non-empty",
                start_with({
                    let mut message = base_message();
                    message.insert(
                        "content".into(),
                        json!([{"type": "text", "text": "prefilled"}]),
                    );
                    message
                }),
            ),
            (
                "stop_reason non-null",
                start_with({
                    let mut message = base_message();
                    message.insert("stop_reason".into(), json!("end_turn"));
                    message
                }),
            ),
            (
                "stop_sequence non-null",
                start_with({
                    let mut message = base_message();
                    message.insert("stop_sequence".into(), json!("seq"));
                    message
                }),
            ),
            (
                "usage missing",
                start_with({
                    let mut message = base_message();
                    message.remove("usage");
                    message
                }),
            ),
            (
                "usage input_tokens missing",
                start_with({
                    let mut message = base_message();
                    message.insert("usage".into(), json!({"output_tokens": 1}));
                    message
                }),
            ),
            (
                "usage output_tokens missing",
                start_with({
                    let mut message = base_message();
                    message.insert("usage".into(), json!({"input_tokens": 5}));
                    message
                }),
            ),
            (
                "usage output_tokens non-numeric",
                start_with({
                    let mut message = base_message();
                    message.insert(
                        "usage".into(),
                        json!({"input_tokens": 5, "output_tokens": "x"}),
                    );
                    message
                }),
            ),
            (
                "usage service_tier invalid",
                start_with({
                    let mut message = base_message();
                    message.insert(
                        "usage".into(),
                        json!({"input_tokens": 5, "output_tokens": 1,
                               "service_tier": "bad tier!"}),
                    );
                    message
                }),
            ),
        ];
        for (label, start) in cases {
            let error = dispatch_events(&[start]).expect_err(&format!("{label} must fail closed"));
            assert_eq!(
                error.reason,
                ModelCallErrorReason::InvalidProviderResponse,
                "{label}"
            );
        }

        // A second message_start is malformed: exactly one valid start.
        let error = dispatch_events(&[
            message_start("msg_a", None, 5),
            message_start("msg_b", None, 5),
        ])
        .expect_err("a second message_start must fail closed");
        assert_eq!(error.reason, ModelCallErrorReason::InvalidProviderResponse);
    }

    #[test]
    fn service_tier_is_read_from_usage_not_from_top_level_message() {
        fn start_with_usage(usage: Value, top_level_tier: Option<&str>) -> Value {
            let mut message = serde_json::Map::new();
            message.insert("id".into(), json!("msg_tier"));
            message.insert("type".into(), json!("message"));
            message.insert("role".into(), json!("assistant"));
            message.insert("model".into(), json!("claude-sonnet-4-6"));
            message.insert("content".into(), Value::Array(Vec::new()));
            message.insert("stop_reason".into(), Value::Null);
            message.insert("stop_sequence".into(), Value::Null);
            if let Some(tier) = top_level_tier {
                message.insert("service_tier".into(), json!(tier));
            }
            message.insert("usage".into(), usage);
            json!({"type": "message_start", "message": Value::Object(message)})
        }
        fn terminal_with(start: Value) -> ProviderAttemptResult {
            dispatch_events(&[
                start,
                text_block_start(0),
                text_delta(0, "ok"),
                content_block_stop(0),
                message_delta("end_turn", 3),
            ])
            .unwrap()
            .unwrap()
        }

        // A top-level message.service_tier must never be mistaken for the actual
        // tier: with the tier only top-level, metadata reports None.
        let result = terminal_with(start_with_usage(
            json!({"input_tokens": 5, "output_tokens": 1}),
            Some("standard"),
        ));
        assert_eq!(
            result.metadata.service_tier, None,
            "top-level message.service_tier is not the actual tier"
        );

        // The actual tier lives at message.usage.service_tier.
        let result = terminal_with(start_with_usage(
            json!({"input_tokens": 5, "output_tokens": 1, "service_tier": "standard"}),
            None,
        ));
        assert_eq!(
            result.metadata.service_tier.unwrap().as_str(),
            "standard",
            "usage.service_tier is the actual tier"
        );

        // Explicit null service_tier means no tier.
        let result = terminal_with(start_with_usage(
            json!({"input_tokens": 5, "output_tokens": 1, "service_tier": null}),
            None,
        ));
        assert_eq!(result.metadata.service_tier, None);
    }

    #[test]
    fn frozen_terminal_grammar_fails_closed() {
        let baseline = vec![
            message_start("msg_term", None, 5),
            text_block_start(0),
            text_delta(0, "ok"),
            content_block_stop(0),
            message_delta("end_turn", 3),
        ];
        let result = dispatch_events(&baseline)
            .unwrap()
            .expect("valid start+terminal must succeed");
        assert!(
            result.usage.is_some(),
            "a valid start+terminal always yields Some usage"
        );

        let cases: Vec<(&str, Value)> = vec![
            (
                "delta missing",
                json!({"type": "message_delta", "usage": {"output_tokens": 3}}),
            ),
            (
                "stop_reason missing",
                json!({"type": "message_delta",
                       "delta": {"stop_sequence": null},
                       "usage": {"output_tokens": 3}}),
            ),
            (
                "stop_reason empty",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "", "stop_sequence": null},
                       "usage": {"output_tokens": 3}}),
            ),
            (
                "stop_sequence numeric",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": 3},
                       "usage": {"output_tokens": 3}}),
            ),
            (
                "usage missing",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": null}}),
            ),
            (
                "usage null",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                       "usage": null}),
            ),
            (
                "usage output_tokens missing",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                       "usage": {"input_tokens": 5}}),
            ),
            (
                "usage output_tokens non-numeric",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                       "usage": {"output_tokens": "x"}}),
            ),
        ];
        for (label, terminal) in cases {
            let mut events = vec![
                message_start("msg_term", None, 5),
                text_block_start(0),
                text_delta(0, "ok"),
                content_block_stop(0),
            ];
            events.push(terminal);
            let error = dispatch_events(&events).expect_err(&format!("{label} must fail closed"));
            assert_eq!(
                error.reason,
                ModelCallErrorReason::InvalidProviderResponse,
                "{label}"
            );
        }

        // A string stop_sequence is accepted at the terminal.
        let mut events = vec![
            message_start("msg_term", None, 5),
            text_block_start(0),
            text_delta(0, "ok"),
            content_block_stop(0),
        ];
        events.push(json!({"type": "message_delta",
                            "delta": {"stop_reason": "stop_sequence", "stop_sequence": "my_seq"},
                            "usage": {"output_tokens": 3}}));
        let result = dispatch_events(&events).unwrap().unwrap();
        assert_eq!(result.finish_reason, ModelFinishReason::Stop);
    }

    #[test]
    fn malformed_content_blocks_fail_closed() {
        // Missing required start strings fail closed (empty strings stay accepted
        // via the usual block-start helpers).
        for (label, start) in [
            (
                "thinking missing thinking",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "thinking", "signature": "sig"}}),
            ),
            (
                "thinking missing signature",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "thinking", "thinking": ""}}),
            ),
            (
                "text missing text",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "text"}}),
            ),
            (
                "redacted_thinking missing data",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "redacted_thinking"}}),
            ),
            (
                "tool_use missing input",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "tool_use", "id": "toolu_1",
                                          "name": "echo"}}),
            ),
            (
                "tool_use null input",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "tool_use", "id": "toolu_1",
                                          "name": "echo", "input": null}}),
            ),
            (
                "tool_use string input",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "tool_use", "id": "toolu_1",
                                          "name": "echo", "input": "{\"city\":\"Paris\"}"}}),
            ),
            (
                "tool_use array input",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "tool_use", "id": "toolu_1",
                                          "name": "echo", "input": [1, 2]}}),
            ),
            (
                "tool_use scalar input",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "tool_use", "id": "toolu_1",
                                          "name": "echo", "input": 42}}),
            ),
        ] {
            let error = dispatch_events(&[message_start("msg_cb", None, 5), start])
                .expect_err(&format!("{label} must fail closed"));
            assert_eq!(
                error.reason,
                ModelCallErrorReason::InvalidProviderResponse,
                "{label}"
            );
        }

        // Non-empty thinking without a signature by block stop is malformed.
        let error = dispatch_events(&[
            message_start("msg_cb", None, 5),
            thinking_block_start(0),
            thinking_delta(0, "visible thought"),
            content_block_stop(0),
        ])
        .expect_err("non-empty thinking without a signature must fail closed");
        assert_eq!(error.reason, ModelCallErrorReason::InvalidProviderResponse);

        // Signature-only thinking stays representable (adaptive display omission).
        let result = dispatch_events(&[
            message_start("msg_cb", None, 5),
            thinking_block_start(0),
            signature_delta(0, "sig_only"),
            content_block_stop(0),
            text_block_start(1),
            text_delta(1, "ok"),
            content_block_stop(1),
            message_delta("end_turn", 3),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(result.content.len(), 2);
        match &result.content[0] {
            ProviderAttemptContent::Reasoning(reasoning) => {
                assert_eq!(reasoning.text(), None);
                assert_eq!(reasoning.signature(), Some("sig_only"));
            }
            other => panic!("expected signature-only reasoning, got {other:?}"),
        }

        // An empty redacted_thinking is rejected at content_block_start — before
        // dispatch can mark semantic output — and fails closed.
        let error = dispatch_events(&[
            message_start("msg_cb", None, 5),
            content_block_start(0, json!({"type": "redacted_thinking", "data": ""})),
            content_block_stop(0),
        ])
        .expect_err("empty redacted_thinking must fail closed");
        assert_eq!(error.reason, ModelCallErrorReason::InvalidProviderResponse);

        // Non-empty redacted_thinking stays representable.
        let result = dispatch_events(&[
            message_start("msg_cb", None, 5),
            content_block_start(0, json!({"type": "redacted_thinking", "data": "opaque"})),
            content_block_stop(0),
            message_delta("end_turn", 3),
        ])
        .unwrap()
        .unwrap();
        match &result.content[0] {
            ProviderAttemptContent::Reasoning(reasoning) => {
                assert_eq!(reasoning.encrypted(), Some("opaque"));
            }
            other => panic!("expected redacted reasoning, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_start_input_requires_present_object_seed() {
        // The valid empty placeholder object: with no input_json_delta it becomes
        // the empty tool arguments.
        let result = dispatch_events(&[
            message_start("msg_tool_seed", None, 5),
            tool_use_block_start(0, "toolu_empty", "echo"),
            content_block_stop(0),
            message_delta("tool_use", 3),
        ])
        .unwrap()
        .unwrap();
        match &result.content[0] {
            ProviderAttemptContent::ToolCall {
                tool_call_id,
                name,
                arguments,
            } => {
                assert_eq!(tool_call_id.as_str(), "toolu_empty");
                assert_eq!(name.as_str(), "echo");
                assert_eq!(arguments.canonical_json(), "{}");
            }
            other => panic!("expected empty-object tool call, got {other:?}"),
        }

        // A non-empty object seed is the argument value verbatim when no
        // input_json_delta ever arrives.
        let result = dispatch_events(&[
            message_start("msg_tool_seed", None, 5),
            content_block_start(
                0,
                json!({"type": "tool_use", "id": "toolu_seed", "name": "echo",
                       "input": {"city": "Paris"}}),
            ),
            content_block_stop(0),
            message_delta("tool_use", 3),
        ])
        .unwrap()
        .unwrap();
        match &result.content[0] {
            ProviderAttemptContent::ToolCall { arguments, .. } => {
                assert_eq!(arguments.canonical_json(), r#"{"city":"Paris"}"#);
            }
            other => panic!("expected object-seed tool call, got {other:?}"),
        }

        // Every malformed seed shape fails closed as InvalidProviderResponse:
        // missing, null, string, array, and scalar.
        for (label, block) in [
            (
                "missing",
                json!({"type": "tool_use", "id": "toolu_bad", "name": "echo"}),
            ),
            (
                "null",
                json!({"type": "tool_use", "id": "toolu_bad", "name": "echo",
                       "input": null}),
            ),
            (
                "string",
                json!({"type": "tool_use", "id": "toolu_bad", "name": "echo",
                       "input": "{\"city\":\"Paris\"}"}),
            ),
            (
                "array",
                json!({"type": "tool_use", "id": "toolu_bad", "name": "echo",
                       "input": [1, 2]}),
            ),
            (
                "scalar",
                json!({"type": "tool_use", "id": "toolu_bad", "name": "echo",
                       "input": 42}),
            ),
        ] {
            let error = dispatch_events(&[
                message_start("msg_tool_seed", None, 5),
                content_block_start(0, block),
            ])
            .expect_err(&format!("{label} tool_use input must fail closed"));
            assert_eq!(
                error.reason,
                ModelCallErrorReason::InvalidProviderResponse,
                "{label} tool_use input"
            );
        }
    }

    #[test]
    fn cumulative_usage_must_be_monotonic() {
        fn start_with_usage(usage: Value) -> Value {
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_usage",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-sonnet-4-6",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": usage,
                },
            })
        }
        fn terminal_with_usage(usage: Value) -> Value {
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": usage,
            })
        }
        // Every decrease case fails InvalidProviderResponse with the current
        // (output-started) delivery preserved.
        let base_usage = json!({
            "input_tokens": 25,
            "output_tokens": 19,
            "cache_read_input_tokens": 9,
            "cache_creation_input_tokens": 11,
            "output_tokens_details": {"thinking_tokens": 6},
        });
        for (label, terminal_usage) in [
            ("output_tokens decrease", json!({"output_tokens": 18})),
            (
                "input_tokens decrease",
                json!({"input_tokens": 24, "output_tokens": 19}),
            ),
            (
                "cache_read_input_tokens decrease",
                json!({"cache_read_input_tokens": 8, "output_tokens": 19}),
            ),
            (
                "cache_creation_input_tokens decrease",
                json!({"cache_creation_input_tokens": 10, "output_tokens": 19}),
            ),
            (
                "thinking_tokens decrease",
                json!({
                    "output_tokens": 19,
                    "output_tokens_details": {"thinking_tokens": 5},
                }),
            ),
        ] {
            let (error, delivery) = dispatch_expect_error(&[
                start_with_usage(base_usage.clone()),
                text_block_start(0),
                text_delta(0, "ok"),
                content_block_stop(0),
                terminal_with_usage(terminal_usage),
            ]);
            assert_eq!(
                error.reason,
                ModelCallErrorReason::InvalidProviderResponse,
                "{label}"
            );
            assert_eq!(
                delivery,
                ProviderRequestDeliveryState::OutputStarted,
                "{label} must keep the current delivery"
            );
        }

        // Equal cumulative counters stay accepted.
        let equal = dispatch_events(&[
            start_with_usage(base_usage.clone()),
            text_block_start(0),
            text_delta(0, "ok"),
            content_block_stop(0),
            terminal_with_usage(json!({
                "output_tokens": 19,
                "input_tokens": 25,
                "cache_read_input_tokens": 9,
                "cache_creation_input_tokens": 11,
                "output_tokens_details": {"thinking_tokens": 6},
            })),
        ])
        .unwrap()
        .unwrap();
        let usage = equal.usage.unwrap();
        assert_eq!(usage.input_tokens(), Some(25));
        assert_eq!(usage.output_tokens(), Some(19));
        assert_eq!(usage.reasoning_tokens(), Some(6));
        assert_eq!(usage.cache_read_tokens(), Some(9));
        assert_eq!(usage.cache_write_tokens(), Some(11));

        // A strict increase of every counter stays accepted.
        let increased = dispatch_events(&[
            start_with_usage(base_usage.clone()),
            text_block_start(0),
            text_delta(0, "ok"),
            content_block_stop(0),
            terminal_with_usage(json!({
                "output_tokens": 21,
                "input_tokens": 27,
                "cache_read_input_tokens": 10,
                "cache_creation_input_tokens": 12,
                "output_tokens_details": {"thinking_tokens": 8},
            })),
        ])
        .unwrap()
        .unwrap();
        let usage = increased.usage.unwrap();
        assert_eq!(usage.input_tokens(), Some(27));
        assert_eq!(usage.output_tokens(), Some(21));
        assert_eq!(usage.reasoning_tokens(), Some(8));
        assert_eq!(usage.cache_read_tokens(), Some(10));
        assert_eq!(usage.cache_write_tokens(), Some(12));

        // Null is absence: it never overrides the start base and never trips the
        // monotonic check, so the terminal stays accepted.
        let nulls = dispatch_events(&[
            start_with_usage(base_usage.clone()),
            text_block_start(0),
            text_delta(0, "ok"),
            content_block_stop(0),
            terminal_with_usage(json!({
                "output_tokens": 20,
                "input_tokens": null,
                "cache_read_input_tokens": null,
                "cache_creation_input_tokens": null,
                "output_tokens_details": null,
            })),
        ])
        .unwrap()
        .unwrap();
        let usage = nulls.usage.unwrap();
        assert_eq!(usage.input_tokens(), Some(25));
        assert_eq!(usage.output_tokens(), Some(20));
        assert_eq!(usage.reasoning_tokens(), Some(6));
        assert_eq!(usage.cache_read_tokens(), Some(9));
        assert_eq!(usage.cache_write_tokens(), Some(11));
    }

    // ------------------------------------------------------------------
    // End-to-end loopback contract tests through ModelGateway
    // ------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_rich_request_and_terminal_without_message_stop() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![("request-id".to_owned(), "req-rich".to_owned())],
            body: sse(&rich_terminal_events()),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with_credential(
            ModelReasoningSummary::Low,
            ModelServiceClass::Priority,
            None,
            provider,
            "sk-test-credential",
        ))
        .await;
        let request = request_for_model_with_tools(Arc::clone(&model), scripted_tool_set()).await;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let progress = ModelProgressPublisher::new(move |event| {
            let _ = progress_tx.send(event);
        });

        let result = gateway
            .generate_model_turn(request, progress, CancellationToken::new())
            .await
            .unwrap();
        let requests = server.join();

        // --- Exactly one POST with the exact transport shape. ---
        assert_eq!(
            requests.len(),
            1,
            "one attempt must make exactly one HTTP request"
        );
        assert_request_shape(&requests[0], "sk-test-credential", "2023-06-01");
        let wire: Value = requests[0].json_body();

        // --- Rich request mapping. ---
        assert_eq!(wire["model"], "claude-sonnet-4-6");
        assert_eq!(wire["max_tokens"], 4096);
        assert_eq!(wire["stream"], true);
        assert_eq!(
            wire["system"],
            json!([
                {"type": "text", "text": "SECRET required system"},
                {"type": "text", "text": "SECRET base system"},
            ])
        );
        assert_eq!(
            wire["messages"],
            json!([
                {"role": "user",
                 "content": [{"type": "text", "text": "SECRET live user input"}]}
            ])
        );
        assert_eq!(
            wire["tools"],
            json!([
                {"name": "echo", "description": "Echo a bounded JSON value",
                 "input_schema": {}}
            ])
        );
        assert_eq!(wire["tool_choice"], json!({"type": "auto"}));
        assert_eq!(wire["thinking"], json!({"type": "adaptive"}));
        assert_eq!(wire["output_config"], json!({"effort": "low"}));
        assert_eq!(wire["service_tier"], "auto");
        for forbidden in ["temperature", "cache_control", "beta", "store", "include"] {
            assert!(
                wire.get(forbidden).is_none(),
                "speculative field {forbidden} must never be sent"
            );
        }

        // --- Progress: only non-empty text deltas, at the normalized index 1. ---
        assert_eq!(
            drain(&mut progress_rx),
            [
                ModelProgressEvent::ContentDelta {
                    content_index: 1,
                    delta: ModelContentDelta::Text(Arc::from("The weather")),
                },
                ModelProgressEvent::ContentDelta {
                    content_index: 1,
                    delta: ModelContentDelta::Text(Arc::from(" in Paris is 22°C.")),
                },
            ]
        );

        // --- Terminal mapping without message_stop. ---
        let response = result.response();
        assert_eq!(response.model().model_id().as_str(), "claude-sonnet-4-6");
        assert_eq!(response.model().reasoning(), ModelReasoningSummary::Low);
        assert_eq!(
            response.model().service_class(),
            ModelServiceClass::Priority
        );
        assert_eq!(response.finish_reason(), ModelFinishReason::ToolCalls);
        assert_eq!(response.response_id().unwrap().as_str(), "msg_rich");
        assert_eq!(
            response.metadata().provider_request_id().unwrap().as_str(),
            "req-rich"
        );
        assert_eq!(
            response.metadata().raw_finish_code().unwrap().as_str(),
            "tool_use"
        );
        assert_eq!(
            response.metadata().service_tier().unwrap().as_str(),
            "standard",
            "the actual response service tier is captured, not the requested one"
        );
        assert_eq!(response.content().len(), 3);
        match &response.content()[0] {
            FinalizedAssistantContent::Reasoning(reasoning) => {
                assert_eq!(reasoning.text(), Some("Let me think about this."));
                assert_eq!(reasoning.signature(), Some("sig_rich"));
                assert_eq!(reasoning.encrypted(), None);
                assert_eq!(reasoning.provider_item_id(), None);
            }
            other => panic!("expected thinking first, got {other:?}"),
        }
        match &response.content()[1] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, "The weather in Paris is 22°C.");
            }
            other => panic!("expected text second, got {other:?}"),
        }
        match &response.content()[2] {
            FinalizedAssistantContent::ToolCall {
                tool_call_id,
                name,
                arguments,
            } => {
                assert_eq!(tool_call_id.as_str(), "toolu_1");
                assert_eq!(name.as_str(), "echo");
                assert_eq!(arguments.canonical_json(), r#"{"city":"Berlin"}"#);
            }
            other => panic!("expected tool call third, got {other:?}"),
        }
        let usage = response.usage().unwrap();
        assert_eq!(usage.input_tokens(), Some(25));
        assert_eq!(usage.output_tokens(), Some(19));
        assert_eq!(usage.reasoning_tokens(), Some(6));
        assert_eq!(usage.cache_read_tokens(), Some(9));
        assert_eq!(usage.cache_write_tokens(), Some(11));
        assert_eq!(usage.provider_total_tokens(), Some(25 + 9 + 11 + 19));
        assert_eq!(usage.reported_cost(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_structured_merges_effort_into_one_output_config() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_struct", None, 9),
                text_block_start(0),
                text_delta(0, r#"{"summary":"SECRET hello","tags":["a","b"]}"#),
                content_block_stop(0),
                message_delta("end_turn", 7),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::Low,
            ModelServiceClass::Standard,
            Some(NonZeroU32::new(65_536).unwrap()),
            provider,
        ))
        .await;
        let contract = StructuredOutputContract::new(
            &model,
            None,
            structured_schema(
                r#"{"type":"object","required":["summary"],"additionalProperties":false,
                    "properties":{
                        "summary":{"type":"string"},
                        "tags":{"type":"array","items":{"type":"string"}}
                    }}"#,
            ),
        )
        .unwrap();
        let request = structured_request(&model, &contract).await;

        let result = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let requests = server.join();

        assert_eq!(
            requests.len(),
            1,
            "one attempt must make exactly one HTTP request"
        );
        assert_request_shape(&requests[0], "sk-test", "2023-06-01");
        let wire: Value = requests[0].json_body();
        // One output_config object carries both the structured format and the
        // adaptive-thinking effort.
        assert_eq!(
            wire["output_config"],
            json!({
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["summary", "tags"],
                        "properties": {
                            "summary": {"type": "string"},
                            "tags": {"type": "array", "items": {"type": "string"}},
                        },
                    },
                },
                "effort": "low",
            })
        );
        assert_eq!(wire["thinking"], json!({"type": "adaptive"}));
        assert_eq!(wire["service_tier"], "standard_only");
        assert!(
            wire.get("tools").is_none(),
            "structured requests are tool-free and omit tool_choice"
        );
        assert!(wire.get("tool_choice").is_none());

        // The gateway re-validates the exact terminal against the contract.
        assert_eq!(result.response().finish_reason(), ModelFinishReason::Stop);
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, r#"{"summary":"SECRET hello","tags":["a","b"]}"#);
            }
            other => panic!("expected structured text, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_disabled_and_provider_default_thinking_mapping() {
        for (reasoning, expected_thinking, expected_effort) in [
            (
                ModelReasoningSummary::Disabled,
                Some(json!({"type": "disabled"})),
                None::<&str>,
            ),
            (
                ModelReasoningSummary::ProviderDefault,
                None::<Value>,
                None::<&str>,
            ),
        ] {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 200,
                content_type: "text/event-stream",
                headers: vec![],
                body: sse(&[
                    message_start("msg_think", None, 5),
                    text_block_start(0),
                    text_delta(0, "ok"),
                    content_block_stop(0),
                    message_delta("end_turn", 3),
                ]),
                gate: 0,
            }]);
            let adapter = Arc::new(
                AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(definition_with(
                reasoning,
                ModelServiceClass::Standard,
                None,
                provider,
            ))
            .await;
            let request = request_for_model(model).await;

            let result = gateway
                .generate_model_turn(
                    request,
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let requests = server.join();

            assert_eq!(requests.len(), 1);
            let wire: Value = requests[0].json_body();
            match expected_thinking {
                Some(thinking) => assert_eq!(wire["thinking"], thinking),
                None => assert!(wire.get("thinking").is_none()),
            }
            match expected_effort {
                Some(effort) => assert_eq!(wire["output_config"]["effort"], effort),
                None => assert!(wire.get("output_config").is_none()),
            }
            assert_eq!(wire["service_tier"], "standard_only");
            assert_eq!(result.response().finish_reason(), ModelFinishReason::Stop);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_early_eof_before_output_maps_to_request_outcome_unknown() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_eof", None, 10),
                thinking_block_start(0),
                content_block_stop(0),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let request = request_for_model(model).await;

        let error = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = server.join();

        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::RequestOutcomeUnknown,
            "early EOF before output must not be a success or a retryable reason"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::AcceptedNoOutput
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_early_eof_after_output_maps_to_stream_interrupted() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_eof2", None, 10),
                text_block_start(0),
                text_delta(0, "partial "),
                content_block_stop(0),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let request = request_for_model(model).await;

        let error = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = server.join();

        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::StreamInterrupted,
            "EOF after semantic output must map through the delivery truth"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_stream_error_carries_current_delivery() {
        // Provider-declared overload before any output (matrix
        // `anthropic_stream_overloaded_before_output`).
        let before = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_err", None, 10),
                json!({"type": "error",
                       "error": {"type": "overloaded_error", "message": "overloaded"}}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&before.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = before.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::RequestOutcomeUnknown,
            "overloaded before output normalizes through the delivery truth"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::AcceptedNoOutput
        );

        // A typed authentication error after output keeps its typed reason with
        // the current (output-started) delivery.
        let after = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_err2", None, 10),
                text_block_start(0),
                text_delta(0, "partial "),
                content_block_stop(0),
                json!({"type": "error",
                       "error": {"type": "authentication_error", "message": "key expired"}}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&after.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = after.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(error.reason(), ModelCallErrorReason::AuthRejected);
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_http_error_matrix_maps_statuses_structurally() {
        let cases: &[(
            u16,
            &str,
            ModelCallErrorReason,
            ProviderRequestDeliveryState,
            Option<Duration>,
        )] = &[
            (
                400,
                "invalid_request_error",
                ModelCallErrorReason::InvalidRequest,
                ProviderRequestDeliveryState::RejectedBeforeExecution,
                None,
            ),
            (
                401,
                "authentication_error",
                ModelCallErrorReason::AuthRejected,
                ProviderRequestDeliveryState::RejectedBeforeExecution,
                None,
            ),
            (
                402,
                "billing_error",
                ModelCallErrorReason::QuotaExceeded,
                ProviderRequestDeliveryState::RejectedBeforeExecution,
                None,
            ),
            (
                413,
                "request_too_large",
                ModelCallErrorReason::InvalidRequest,
                ProviderRequestDeliveryState::RejectedBeforeExecution,
                None,
            ),
            (
                429,
                "rate_limit_error",
                ModelCallErrorReason::RateLimited,
                ProviderRequestDeliveryState::RejectedBeforeExecution,
                Some(Duration::from_secs(17)),
            ),
            (
                529,
                "overloaded_error",
                ModelCallErrorReason::ProviderUnavailable,
                ProviderRequestDeliveryState::RejectedBeforeExecution,
                None,
            ),
            (
                500,
                "api_error",
                ModelCallErrorReason::RequestOutcomeUnknown,
                ProviderRequestDeliveryState::Unknown,
                None,
            ),
            (
                504,
                "timeout_error",
                ModelCallErrorReason::RequestOutcomeUnknown,
                ProviderRequestDeliveryState::Unknown,
                None,
            ),
            (
                403,
                "permission_error",
                ModelCallErrorReason::InvalidRequest,
                ProviderRequestDeliveryState::RejectedBeforeExecution,
                None,
            ),
            (
                503,
                "api_error",
                ModelCallErrorReason::RequestOutcomeUnknown,
                ProviderRequestDeliveryState::Unknown,
                None,
            ),
        ];
        for (status, error_type, expected_reason, expected_delivery, expected_retry_after) in cases
        {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: *status,
                content_type: "application/json",
                headers: if *status == 429 || *status == 529 {
                    vec![("retry-after".to_owned(), "17".to_owned())]
                } else {
                    vec![]
                },
                body: format!(
                    r#"{{"type":"error","error":{{"type":"{error_type}","message":"boom"}},"request_id":"req_{status}"}}"#
                ),
                gate: 0,
            }]);
            let adapter = Arc::new(
                AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(definition_with(
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
                None,
                provider,
            ))
            .await;
            let request = request_for_model(model).await;

            let error = gateway
                .generate_model_turn(
                    request,
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            let requests = server.join();

            assert_eq!(
                requests.len(),
                1,
                "HTTP {status} must make exactly one request with no retry"
            );
            assert_eq!(
                error.reason(),
                *expected_reason,
                "HTTP {status} classification"
            );
            assert_eq!(
                error.delivery(),
                *expected_delivery,
                "HTTP {status} delivery"
            );
            assert_eq!(
                error.retry_after(),
                *expected_retry_after,
                "HTTP {status} retry-after"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_529_requires_typed_overloaded_envelope() {
        // A 529 grants RejectedBeforeExecution only when the complete in-bound
        // JSON envelope declares error.type == "overloaded_error"; missing,
        // mismatched, or unparseable envelopes stay ProviderUnavailable/Unknown
        // (never logical-retry safe). Retry-After is never claimed for
        // ProviderUnavailable.
        let server = LoopbackServer::spawn(vec![
            ScriptedResponse {
                status: 529,
                content_type: "application/json",
                headers: vec![("retry-after".to_owned(), "17".to_owned())],
                body: r#"{"type":"error","error":{"message":"boom"}}"#.to_owned(),
                gate: 0,
            },
            ScriptedResponse {
                status: 529,
                content_type: "application/json",
                headers: vec![("retry-after".to_owned(), "17".to_owned())],
                body: r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#
                    .to_owned(),
                gate: 0,
            },
            ScriptedResponse {
                status: 529,
                content_type: "application/json",
                headers: vec![("retry-after".to_owned(), "17".to_owned())],
                body: "this is not json".to_owned(),
                gate: 0,
            },
        ]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;

        for (round, label) in ["missing type", "mismatched type", "malformed body"]
            .iter()
            .enumerate()
        {
            let error = gateway
                .generate_model_turn(
                    request_for_model(Arc::clone(&model)).await,
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.reason(),
                ModelCallErrorReason::RequestOutcomeUnknown,
                "529 {label} (round {round}) normalizes through the Unknown delivery"
            );
            assert_eq!(
                error.delivery(),
                ProviderRequestDeliveryState::Unknown,
                "529 {label} (round {round}) must not be logical-retry safe"
            );
            assert_eq!(
                error.retry_after(),
                None,
                "529 {label} (round {round}) must never claim Retry-After"
            );
        }
        let requests = server.join();
        assert_eq!(requests.len(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_connect_refused_maps_to_transport_unavailable_not_sent() {
        // Port 0 is never a valid connect target — no process can listen on it —
        // so the connect fails deterministically in the connect phase with no
        // timeout, no external net, and no bind/drop port-reuse residual. Verified
        // empirically on this toolchain: reqwest 0.13.4 accepts the port-0 URL and
        // hyper classifies the failure as a connect error (`is_connect()`), which
        // `classify_send_error` maps to TransportUnavailable/NotSent.
        let endpoint = "http://127.0.0.1:0/v1/messages";

        let adapter =
            Arc::new(AnthropicMessagesProviderAdapter::new(endpoint, "2023-06-01").unwrap());
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let request = request_for_model(model).await;

        let error = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.reason(), ModelCallErrorReason::TransportUnavailable);
        assert_eq!(error.delivery(), ProviderRequestDeliveryState::NotSent);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_malformed_200_fails_closed_as_invalid_provider_response() {
        for (content_type, body) in [
            ("text/event-stream", "data: this is not json\n\n".to_owned()),
            ("application/json", "{}".to_owned()),
        ] {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 200,
                content_type,
                headers: vec![],
                body,
                gate: 0,
            }]);
            let adapter = Arc::new(
                AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(definition_with(
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
                None,
                provider,
            ))
            .await;
            let request = request_for_model(model).await;

            let error = gateway
                .generate_model_turn(
                    request,
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            let requests = server.join();

            assert_eq!(requests.len(), 1);
            assert_eq!(
                error.reason(),
                ModelCallErrorReason::InvalidProviderResponse,
                "malformed success ({content_type}) must fail closed"
            );
            assert_eq!(
                error.delivery(),
                ProviderRequestDeliveryState::AcceptedNoOutput
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_redacted_thinking_empty_rejected_before_semantic_non_empty_representable() {
        // Empty redacted data is rejected at content_block_start, before dispatch
        // can mark semantic output: InvalidProviderResponse AND the delivery stays
        // AcceptedNoOutput (never a phantom OutputStarted).
        let empty = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_red0", None, 5),
                content_block_start(0, json!({"type": "redacted_thinking", "data": ""})),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&empty.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = empty.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::InvalidProviderResponse,
            "empty redacted_thinking must fail closed"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::AcceptedNoOutput,
            "rejection must happen before dispatch marks semantic output"
        );

        // Non-empty redacted_thinking stays representable in finalized content
        // (with the visible text the gateway's terminal contract requires for a
        // Stop finish).
        let non_empty = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_red1", None, 5),
                content_block_start(0, json!({"type": "redacted_thinking", "data": "opaque"})),
                content_block_stop(0),
                text_block_start(1),
                text_delta(1, "visible"),
                content_block_stop(1),
                message_delta("end_turn", 3),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&non_empty.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let result = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let requests = non_empty.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(result.response().finish_reason(), ModelFinishReason::Stop);
        match &result.response().content()[0] {
            FinalizedAssistantContent::Reasoning(reasoning) => {
                assert_eq!(reasoning.encrypted(), Some("opaque"));
                assert_eq!(reasoning.text(), None);
                assert_eq!(reasoning.signature(), None);
            }
            other => panic!("expected redacted reasoning, got {other:?}"),
        }

        // A non-empty redacted block marks semantic output at start: an EOF after
        // it (before any terminal) is StreamInterrupted with OutputStarted.
        let semantic = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_red2", None, 5),
                content_block_start(0, json!({"type": "redacted_thinking", "data": "opaque"})),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&semantic.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = semantic.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(error.reason(), ModelCallErrorReason::StreamInterrupted);
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted,
            "a non-empty redacted block must stay semantic at start"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_metadata_allowlist_keeps_request_id_and_never_retains_canary() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![
                ("request-id".to_owned(), "req-canary-probe".to_owned()),
                ("x-canary-secret".to_owned(), "CANARY-TOP-SECRET".to_owned()),
                ("retry-after".to_owned(), "99".to_owned()),
                ("set-cookie".to_owned(), "session=SECRET".to_owned()),
            ],
            body: sse(&[
                message_start("msg_canary", None, 5),
                text_block_start(0),
                text_delta(0, "hello"),
                content_block_stop(0),
                message_delta("end_turn", 3),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with_credential(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
            "sk-test-credential",
        ))
        .await;
        let request = request_for_model(model).await;

        let result = gateway
            .generate_model_turn(
                request,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let requests = server.join();

        assert_eq!(requests.len(), 1);
        assert_eq!(
            result
                .response()
                .metadata()
                .provider_request_id()
                .unwrap()
                .as_str(),
            "req-canary-probe",
            "the allowlisted request-id enters validated metadata"
        );
        for debug in [format!("{result:?}"), format!("{adapter:?}")] {
            assert!(
                !debug.contains("CANARY-TOP-SECRET"),
                "canary header leaked: {debug}"
            );
            assert!(
                !debug.contains("sk-test-credential"),
                "credential leaked: {debug}"
            );
            assert!(
                !debug.contains("req-canary-probe"),
                "request id must be redacted: {debug}"
            );
            assert!(
                !debug.contains("session=SECRET"),
                "unlisted header leaked: {debug}"
            );
        }
        // The validated anthropic-version is not secret: it prints in the adapter
        // Debug (the gateway result never carries it).
        assert!(
            !format!("{result:?}").contains("2023-06-01"),
            "the result debug never carries the version"
        );
        assert!(
            format!("{adapter:?}").contains("2023-06-01"),
            "the adapter debug must print the validated anthropic-version"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_malformed_stream_error_fails_closed_with_current_delivery() {
        // A malformed error event after output: InvalidProviderResponse with the
        // current (output-started) delivery — never a provider-declared reason.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_err3", None, 10),
                text_block_start(0),
                text_delta(0, "partial "),
                content_block_stop(0),
                json!({"type": "error", "error": {"message": "boom"}}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = server.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::InvalidProviderResponse,
            "a malformed stream error must fail closed"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted,
            "the current delivery is preserved through the malformed error event"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_message_start_model_mismatch_fails_closed_without_success() {
        // The pinned API model is "claude-sonnet-4-6", but the start reports a
        // different model (or no model): the attempt must fail closed as
        // InvalidProviderResponse before any block, never succeed, and never
        // produce provenance.
        for start in [
            message_start_with_model("msg_mismatch", None, 5, "claude-sonnet-4-5"),
            without_start_model(message_start_with_model(
                "msg_mismatch",
                None,
                5,
                "claude-sonnet-4-6",
            )),
        ] {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 200,
                content_type: "text/event-stream",
                headers: vec![],
                body: sse(&[start, message_delta("end_turn", 1)]),
                gate: 0,
            }]);
            let adapter = Arc::new(
                AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(definition_with(
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
                None,
                provider,
            ))
            .await;
            let error = gateway
                .generate_model_turn(
                    request_for_model(model).await,
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            let requests = server.join();

            assert_eq!(
                requests.len(),
                1,
                "a mismatched start must make exactly one HTTP request"
            );
            assert_eq!(
                error.reason(),
                ModelCallErrorReason::InvalidProviderResponse,
                "a start that does not echo the pinned API model must fail closed"
            );
            assert_eq!(
                error.delivery(),
                ProviderRequestDeliveryState::AcceptedNoOutput,
                "a start mismatch fails at the current delivery, never success"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_usage_total_overflow_fails_closed_with_current_delivery() {
        // The checked provider-total sum overflows (u64::MAX input + 1 output):
        // InvalidProviderResponse with the current delivery, never a silent
        // `usage: None` success.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_overflow", None, u64::MAX),
                text_block_start(0),
                text_delta(0, "ok"),
                content_block_stop(0),
                message_delta("end_turn", 1),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = server.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::InvalidProviderResponse,
            "provider-total overflow must fail the terminal"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_explicit_null_usage_fields_are_absence() {
        // Explicit JSON null optional usage fields at start and terminal are
        // absence: the terminal nulls never override the start base, and success
        // usage is always Some.
        let mut message = serde_json::Map::new();
        message.insert("id".into(), json!("msg_null"));
        message.insert("type".into(), json!("message"));
        message.insert("role".into(), json!("assistant"));
        message.insert("model".into(), json!("claude-sonnet-4-6"));
        message.insert("content".into(), Value::Array(Vec::new()));
        message.insert("stop_reason".into(), Value::Null);
        message.insert("stop_sequence".into(), Value::Null);
        message.insert(
            "usage".into(),
            json!({
                "input_tokens": 25,
                "output_tokens": 1,
                "cache_read_input_tokens": null,
                "cache_creation_input_tokens": null,
                "output_tokens_details": null,
            }),
        );
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                json!({"type": "message_start", "message": Value::Object(message)}),
                text_block_start(0),
                text_delta(0, "ok"),
                content_block_stop(0),
                json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {
                    "output_tokens": 19,
                    "input_tokens": null,
                    "cache_read_input_tokens": null,
                    "cache_creation_input_tokens": null,
                    "output_tokens_details": null,
                }}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let result = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let requests = server.join();
        assert_eq!(requests.len(), 1);
        let usage = result
            .response()
            .usage()
            .expect("valid start+terminal always yields usage");
        assert_eq!(
            usage.input_tokens(),
            Some(25),
            "terminal null input_tokens must not override the start base"
        );
        assert_eq!(usage.output_tokens(), Some(19));
        assert_eq!(usage.cache_read_tokens(), None);
        assert_eq!(usage.cache_write_tokens(), None);
        assert_eq!(usage.reasoning_tokens(), None);
        assert_eq!(usage.provider_total_tokens(), Some(25 + 19));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_cancellation_before_send_returns_not_sent_without_any_request() {
        // A cancellation that is already observable before the adapter sends must
        // be Cancelled/NotSent, and no POST may ever be issued.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_cancel", None, 5),
                text_block_start(0),
                text_delta(0, "ok"),
                content_block_stop(0),
                message_delta("end_turn", 3),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (_gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let request = request_for_model(model).await;
        let cancel = CancellationToken::new();
        cancel.cancel();

        // Drive the adapter directly (bypassing the gateway's own pre-cancel
        // short-circuit) so the adapter's pre-send NotSent contract is proven.
        let attempt = ProviderAttemptRequest {
            effective_max_output_tokens: request.effective_max_output_tokens(),
            call: Arc::clone(&request),
            credential: "sk-test".parse().unwrap(),
        };
        let error = adapter
            .execute(attempt, ModelProgressPublisher::discard(), cancel)
            .await
            .unwrap_err();
        let requests = server.join();
        assert!(
            requests.is_empty(),
            "a pre-send cancellation must never issue the POST"
        );
        assert_eq!(error.reason, ModelCallErrorReason::Cancelled);
        assert_eq!(error.delivery, ProviderRequestDeliveryState::NotSent);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_cancellation_after_first_delta_returns_cancelled_with_output_started() {
        // The gated body writes message_start, the thinking block, and the first
        // text delta before holding; cancellation happens only after the delta is
        // published, so the delivery truth is OutputStarted.
        let body = sse(&[
            message_start("msg_cancel", None, 10),
            thinking_block_start(0),
            thinking_delta(0, "thinking"),
            signature_delta(0, "sig_cancel"),
            content_block_stop(0),
            text_block_start(1),
            text_delta(1, "partial "),
            content_block_stop(1),
            message_delta("end_turn", 5),
        ]);
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body,
            gate: 7,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let request = request_for_model(model).await;

        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let progress = ModelProgressPublisher::new(move |event| {
            let _ = progress_tx.send(event);
        });
        let cancel = CancellationToken::new();
        let gateway_task = Arc::clone(&gateway);
        let request_task = Arc::clone(&request);
        let cancel_task = cancel.clone();
        let generation = tokio::spawn(async move {
            gateway_task
                .generate_model_turn(request_task, progress, cancel_task)
                .await
        });

        // Deterministic ordering: the first text delta is published only after the
        // server wrote it; cancellation happens before the server is released.
        let first = progress_rx
            .recv()
            .await
            .expect("first delta must be published before cancellation");
        assert!(matches!(
            first,
            ModelProgressEvent::ContentDelta {
                content_index: 1,
                delta: ModelContentDelta::Text(delta),
            } if &*delta == "partial "
        ));
        cancel.cancel();
        let _ = server.release().send(());

        let error = generation
            .await
            .expect("generation task must settle")
            .expect_err("cancellation must fail the attempt");
        let requests = server.join();

        assert_eq!(requests.len(), 1, "cancellation must not re-issue the POST");
        assert_eq!(error.reason(), ModelCallErrorReason::Cancelled);
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted,
            "cancellation after semantic output keeps the conservative delivery truth"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_empty_hidden_block_before_text_publishes_normalized_index() {
        // An empty thinking block at provider index 0 normalizes to zero content
        // blocks, so the text at provider index 1 must publish content_index 0 —
        // the eventual normalized content[] position, never the raw provider index.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_hidden", None, 10),
                thinking_block_start(0),
                content_block_stop(0),
                text_block_start(1),
                text_delta(1, "hello"),
                content_block_stop(1),
                message_delta("end_turn", 5),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let request = request_for_model(model).await;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let progress = ModelProgressPublisher::new(move |event| {
            let _ = progress_tx.send(event);
        });

        let result = gateway
            .generate_model_turn(request, progress, CancellationToken::new())
            .await
            .unwrap();
        let requests = server.join();

        assert_eq!(requests.len(), 1);
        assert_eq!(
            drain(&mut progress_rx),
            [ModelProgressEvent::ContentDelta {
                content_index: 0,
                delta: ModelContentDelta::Text(Arc::from("hello")),
            }]
        );
        assert_eq!(result.response().finish_reason(), ModelFinishReason::Stop);
        assert_eq!(result.response().content().len(), 1);
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => assert_eq!(&**text, "hello"),
            other => panic!("empty hidden block must not leak into content, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_stop_reason_variants_and_terminal_consistency() {
        // (stop_reason, toolful request, expected gateway reason, expected finish
        // reason on success, whether the body is the malformed-empty terminal)
        let cases: &[(
            &str,
            bool,
            Option<ModelCallErrorReason>,
            Option<ModelFinishReason>,
        )] = &[
            ("end_turn", false, None, Some(ModelFinishReason::Stop)),
            ("stop_sequence", false, None, Some(ModelFinishReason::Stop)),
            ("tool_use", true, None, Some(ModelFinishReason::ToolCalls)),
            // A tool_use stop reason without any tool block is terminal-inconsistent.
            (
                "tool_use",
                false,
                Some(ModelCallErrorReason::InvalidProviderResponse),
                None,
            ),
            ("refusal", false, None, Some(ModelFinishReason::Refused)),
            // A refusal stop reason without visible text is invalid.
            (
                "refusal",
                false,
                Some(ModelCallErrorReason::InvalidProviderResponse),
                None,
            ),
            (
                "max_tokens",
                false,
                Some(ModelCallErrorReason::IncompleteResponse),
                None,
            ),
            (
                "model_context_window_exceeded",
                false,
                Some(ModelCallErrorReason::IncompleteResponse),
                None,
            ),
            (
                "pause_turn",
                false,
                Some(ModelCallErrorReason::IncompleteResponse),
                None,
            ),
            // Unknown non-empty stop reasons are preserved as Unknown.
            ("custom_stop", false, None, Some(ModelFinishReason::Unknown)),
        ];
        for (index, (stop_reason, toolful, expected_error, expected_finish)) in
            cases.iter().enumerate()
        {
            let events = if *stop_reason == "refusal" && index == 4 {
                // Refused with visible refusal text.
                vec![
                    message_start("msg_stop", None, 5),
                    text_block_start(0),
                    text_delta(0, "I cannot answer that."),
                    content_block_stop(0),
                    message_delta("refusal", 3),
                ]
            } else if *toolful {
                vec![
                    message_start("msg_stop", None, 5),
                    tool_use_block_start(0, "toolu_stop", "echo"),
                    input_json_delta(0, "{}"),
                    content_block_stop(0),
                    message_delta("tool_use", 3),
                ]
            } else if *stop_reason == "tool_use" || *stop_reason == "refusal" {
                // Terminal-inconsistent: tool_use/refusal stop reason without any
                // representable block. The refusal variant genuinely sends
                // stop_reason "refusal" with no visible text.
                vec![
                    message_start("msg_stop", None, 5),
                    message_delta(stop_reason, 3),
                ]
            } else {
                vec![
                    message_start("msg_stop", None, 5),
                    text_block_start(0),
                    text_delta(0, "some words"),
                    content_block_stop(0),
                    message_delta(stop_reason, 3),
                ]
            };
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 200,
                content_type: "text/event-stream",
                headers: vec![],
                body: sse(&events),
                gate: 0,
            }]);
            let adapter = Arc::new(
                AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(definition_with(
                ModelReasoningSummary::ProviderDefault,
                ModelServiceClass::Standard,
                None,
                provider,
            ))
            .await;
            let request = if *toolful {
                request_for_model_with_tools(Arc::clone(&model), scripted_tool_set()).await
            } else {
                request_for_model(model).await
            };

            let outcome = gateway
                .generate_model_turn(
                    request,
                    ModelProgressPublisher::discard(),
                    CancellationToken::new(),
                )
                .await;
            let requests = server.join();
            assert_eq!(requests.len(), 1, "stop-reason case {index}");

            match (expected_error, expected_finish) {
                (Some(expected_error), _) => {
                    let error = outcome.expect_err("case must fail at the gateway");
                    assert_eq!(
                        &error.reason(),
                        expected_error,
                        "stop-reason case {index} ({stop_reason})"
                    );
                }
                (None, Some(expected_finish)) => {
                    let result = outcome.expect("case must succeed at the gateway");
                    assert_eq!(
                        &result.response().finish_reason(),
                        expected_finish,
                        "stop-reason case {index} ({stop_reason})"
                    );
                }
                _ => panic!("case must declare an expectation"),
            }
        }

        // An empty stop_reason is a malformed terminal: InvalidProviderResponse.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_empty", None, 5),
                text_block_start(0),
                text_delta(0, "words"),
                content_block_stop(0),
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "", "stop_sequence": null},
                       "usage": {"output_tokens": 3}}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = server.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::InvalidProviderResponse,
            "an empty stop_reason is a malformed terminal"
        );

        // message_stop alone (no message_delta) is never success: EOF stays a
        // truthful transport failure.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                message_start("msg_stop_only", None, 5),
                text_block_start(0),
                text_delta(0, "words"),
                content_block_stop(0),
                json!({"type": "message_stop"}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            AnthropicMessagesProviderAdapter::new(&server.messages_endpoint(), "2023-06-01")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(definition_with(
            ModelReasoningSummary::ProviderDefault,
            ModelServiceClass::Standard,
            None,
            provider,
        ))
        .await;
        let error = gateway
            .generate_model_turn(
                request_for_model(model).await,
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let requests = server.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::StreamInterrupted,
            "message_stop alone must never synthesize success"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted
        );
    }
}
