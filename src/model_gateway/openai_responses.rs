//! M14 slice: direct private OpenAI Responses production provider adapter.
//!
//! This adapter owns the OpenAI Responses protocol wire mapping directly (no generic
//! HTTP/SSE abstraction — there is only one consumer today). It is a child module of
//! `model_gateway` so it can consume the existing private request/result/error types
//! without widening them.
//!
//! Frozen contracts (ADR 0138/0139, M12 fixture `docs/fixtures/provider-gate-m12`):
//! - one `generate_model_turn` issues at most one POST; the reqwest client is built
//!   with `redirect::Policy::none()`, `retry::never()` and `no_proxy()`;
//! - success is exactly an SSE `response.completed` whose `response.status` is
//!   `completed`; `response.failed` / `response.incomplete` / `error` are non-success;
//!   early EOF is a truthful transport failure, never a synthetic success;
//! - response metadata allowlist is exactly `x-request-id` / `retry-after` /
//!   `openai-processing-ms`; only `x-request-id` (validated) enters success metadata,
//!   only numeric `retry-after` enters typed rate-limit hints, and processing time is
//!   never retained. No other header, and no body field outside the terminal grammar,
//!   can be represented;
//! - error classification is structural (status + typed `error.type`/`error.code` +
//!   retry hint), never human message matching;
//! - progress `content_index` is the zero-based position of the block in the
//!   finalized terminal `content[]`; every non-empty streamed text delta is checked
//!   against the terminal so provider `output_index` and normalized index cannot
//!   silently drift;
//! - cancellation before the request is `Cancelled/NotSent`; cancellation during send
//!   or read aborts by dropping the reqwest future/stream and returns `Cancelled` with
//!   a conservative delivery; a synchronously accepted `response.completed` wins over
//!   any later cancellation.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::model_gateway::provider_transport::{
    SseParser, build_client, cancelled, classify_send_error, invalid_provider_response,
    invalid_request_not_sent, is_event_stream, parse_retry_after, read_bounded_envelope,
    response_byte_limit, transport_read_error,
};
use crate::model_gateway::{
    ModelCallErrorReason, ModelContentDelta, ModelFinishReason, ModelProgressEvent,
    ModelProgressPublisher, ModelReasoningSummary, ModelServiceClass, ModelUsage, OutputContract,
    ProviderAttemptContent, ProviderAttemptError, ProviderAttemptRequest, ProviderAttemptResult,
    ProviderItemId, ProviderRequestDeliveryState, ProviderRequestId, ProviderResponseId,
    ProviderResponseMetadata, ReasoningContent, RedactedProviderCode, StructuredOutputContract,
};
use crate::prompt::{ModelAssistantContentRef, ModelMessage, ModelMessageRef, PromptSection};
use crate::tools::{ToolCallId, ToolName, ToolSpec};
use crate::wire::lexical::validate_opaque_ascii;
use crate::wire::{BoundedJsonObject, ProtocolLimits};

/// Typed, payload-free configuration error. The credential and the rejected endpoint
/// details are never stored, so Debug/Display can never leak them.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by the adjacent M14 model source/catalog slice"
    )
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum OpenAiProviderConfigError {
    #[error("OpenAI endpoint must be an absolute http(s) URL without query, fragment, or userinfo")]
    InvalidEndpoint,
    #[error("OpenAI bearer credential must be non-empty printable ASCII within 256 bytes")]
    InvalidCredential,
    #[error("OpenAI HTTP client construction failed")]
    ClientBuild,
}

/// Direct private OpenAI Responses adapter.
///
/// The endpoint is stored exactly as validated: scheme http/https, no userinfo, no
/// query, no fragment (all rejected at construction), so the stored URL is already
/// safe for Debug. The bearer credential is stored but never printed.
pub(crate) struct OpenAiResponsesProviderAdapter {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    credential: Box<str>,
}

impl OpenAiResponsesProviderAdapter {
    /// Builds the adapter against an explicit full `/responses` endpoint URL with an
    /// explicit bearer credential. No environment or home-directory lookup ever runs.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "constructed by the adjacent M14 model source/catalog slice"
        )
    )]
    pub(crate) fn new(endpoint: &str, credential: &str) -> Result<Self, OpenAiProviderConfigError> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| OpenAiProviderConfigError::InvalidEndpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(OpenAiProviderConfigError::InvalidEndpoint);
        }
        validate_opaque_ascii(credential, 256)
            .map_err(|_| OpenAiProviderConfigError::InvalidCredential)?;
        // The locked-down client (no redirects, no retries, no ambient proxy) is the
        // shared production constructor; the typed ClientBuild mapping is preserved.
        let client = build_client().map_err(|_| OpenAiProviderConfigError::ClientBuild)?;
        Ok(Self {
            client,
            endpoint,
            credential: credential.into(),
        })
    }
}

impl fmt::Debug for OpenAiResponsesProviderAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesProviderAdapter")
            .field("endpoint", &self.endpoint.as_str())
            .field("credential", &"<redacted>")
            .finish()
    }
}

impl crate::model_gateway::ProviderAdapter for OpenAiResponsesProviderAdapter {
    fn execute(
        &self,
        request: ProviderAttemptRequest,
        progress: ModelProgressPublisher,
        cancel: CancellationToken,
    ) -> crate::model_gateway::ProviderAttemptFuture<'_> {
        Box::pin(self.run(request, progress, cancel))
    }
}

impl OpenAiResponsesProviderAdapter {
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
        let http_request = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&*self.credential)
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
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<ProviderRequestId>().ok());
        let retry_after = parse_retry_after(response.headers());
        if !status.is_success() {
            let delivery = match status.as_u16() {
                400 | 401 | 429 => ProviderRequestDeliveryState::RejectedBeforeExecution,
                _ => ProviderRequestDeliveryState::Unknown,
            };
            let envelope = read_bounded_envelope(response, &cancel, delivery).await?;
            return Err(classify_http_error(
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
        // Every non-empty streamed text delta's validated output index, correlated
        // with the terminal output array when response.completed arrives.
        let mut streamed_text_indexes = Vec::new();
        let mut parser = SseParser::new(response_byte_limit());
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
                            &mut streamed_text_indexes,
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

/// Shared structural mapping from a typed OpenAI error envelope to the closed domain
/// taxonomy. Only the machine-readable `error.type`/`error.code` are inspected — the
/// human `error.message` is never read. Unknown tuples return `None` so each caller
/// applies its own status-specific fallback (provider unavailable/unknown stays the
/// caller default).
fn classify_error_tuple(
    error_type: Option<&str>,
    error_code: Option<&str>,
) -> Option<ModelCallErrorReason> {
    if error_code == Some("context_length_exceeded") {
        Some(ModelCallErrorReason::ContextOverflow)
    } else if error_type == Some("insufficient_quota")
        || error_code == Some("insufficient_quota")
        || error_code == Some("credit_balance_exhausted")
    {
        Some(ModelCallErrorReason::QuotaExceeded)
    } else if error_type == Some("rate_limit_error") || error_code == Some("rate_limit_exceeded") {
        Some(ModelCallErrorReason::RateLimited)
    } else if error_type == Some("authentication_error")
        || error_code == Some("authentication_error")
        || error_code == Some("invalid_api_key")
    {
        Some(ModelCallErrorReason::AuthRejected)
    } else {
        None
    }
}

/// HTTP error classification is structural per the M12 fixture: status plus typed
/// `error.type`/`error.code`; human `error.message` is never inspected. 400/401/429 are
/// provider-declared pre-execution rejections; 5xx and every other status fail
/// conservatively with Unknown delivery (no retry-safe proof). The shared tuple
/// mapping supplies the known typed reasons while each status keeps its fallback:
/// a 400 that is not context overflow stays InvalidRequest, a 401 is always
/// AuthRejected, and a 429 that is not quota stays RateLimited.
fn classify_http_error(
    status: u16,
    envelope: Option<&Value>,
    retry_after: Option<Duration>,
) -> ProviderAttemptError {
    let (error_type, error_code) = match envelope.and_then(|value| value.get("error")) {
        Some(error) => (
            error.get("type").and_then(Value::as_str),
            error.get("code").and_then(Value::as_str),
        ),
        None => (None, None),
    };
    let reason = match (status, classify_error_tuple(error_type, error_code)) {
        (400, Some(ModelCallErrorReason::ContextOverflow)) => ModelCallErrorReason::ContextOverflow,
        (400, _) => ModelCallErrorReason::InvalidRequest,
        (401, _) => ModelCallErrorReason::AuthRejected,
        (429, Some(ModelCallErrorReason::QuotaExceeded)) => ModelCallErrorReason::QuotaExceeded,
        (429, _) => ModelCallErrorReason::RateLimited,
        _ => ModelCallErrorReason::ProviderUnavailable,
    };
    let delivery = match status {
        400 | 401 | 429 => ProviderRequestDeliveryState::RejectedBeforeExecution,
        _ => ProviderRequestDeliveryState::Unknown,
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

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

/// Encodes the exact provider request body. The model name comes from the exact
/// `ModelDefinitionRef` bound to the request; the adapter carries no model name.
fn encode_request(request: &ProviderAttemptRequest) -> Result<Vec<u8>, ProviderAttemptError> {
    let call = request.call();
    let input = call.input();

    let mut body = Map::new();
    body.insert(
        "model".into(),
        json!(call.model.definition().model_id().as_str()),
    );
    body.insert(
        "max_output_tokens".into(),
        json!(request.effective_max_output_tokens.get()),
    );
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));

    let system = input.system();
    if !system.is_empty() {
        let instructions = system.iter().map(PromptSection::text).collect::<Vec<_>>();
        body.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    let items = encode_input_items(input.messages());
    if items.is_empty() {
        // A Responses request without `input` is never meaningful (for example, a
        // transcript whose only items were orphaned reasoning with no item id).
        return Err(invalid_request_not_sent());
    }
    body.insert("input".into(), Value::Array(items));
    if !input.tools_empty() {
        body.insert("tools".into(), Value::Array(encode_tools(input.tools())));
    }
    // tool_choice: "none" whenever the output contract forbids tool calls — NoToolCalls
    // is reachable for Compaction with zero tools (no `tools` array on the wire) and
    // Structured output is validated tool-free, with "none" as the conservative
    // fail-closed choice if that invariant ever loosens; ordinary toolful calls use
    // "auto"; a request with neither contract nor tools omits tool_choice entirely.
    match input.output_contract() {
        Some(OutputContract::NoToolCalls) | Some(OutputContract::Structured(_)) => {
            body.insert("tool_choice".into(), json!("none"));
        }
        None if !input.tools_empty() => {
            body.insert("tool_choice".into(), json!("auto"));
        }
        None => {}
    }
    if let Some(reasoning) = reasoning_parameters(call.model.generation().reasoning()) {
        body.insert("reasoning".into(), reasoning);
    }
    if call.model.generation().service_class() == ModelServiceClass::Priority {
        body.insert("service_tier".into(), json!("priority"));
    }
    if let Some(OutputContract::Structured(contract)) = input.output_contract() {
        let name = contract.name().unwrap_or("response_schema");
        body.insert(
            "text".into(),
            json!({
                "format": {
                    "type": "json_schema",
                    "name": name,
                    "strict": true,
                    "schema": sanitize_structured_schema(contract),
                }
            }),
        );
    }

    let bytes = serde_json::to_vec(&Value::Object(body)).map_err(|_| invalid_request_not_sent())?;
    let maximum =
        usize::try_from(ProtocolLimits::v1_0().transport.max_request_bytes).unwrap_or(usize::MAX);
    if bytes.len() > maximum {
        return Err(invalid_request_not_sent());
    }
    Ok(bytes)
}

fn reasoning_parameters(reasoning: ModelReasoningSummary) -> Option<Value> {
    match reasoning {
        ModelReasoningSummary::ProviderDefault => None,
        ModelReasoningSummary::Disabled => Some(json!({"effort": "none"})),
        ModelReasoningSummary::Low => Some(json!({"effort": "low", "summary": "auto"})),
        ModelReasoningSummary::Medium => Some(json!({"effort": "medium", "summary": "auto"})),
        ModelReasoningSummary::High => Some(json!({"effort": "high", "summary": "auto"})),
    }
}

/// Encodes the ordered transcript as Responses `input` items. Reasoning is replayable
/// only when it carries a provider item id; its signature is never an OpenAI field and
/// is never emitted. Every replayed reasoning item carries the official-required
/// `summary` field — one `summary_text` entry when a summary exists, `[]` otherwise —
/// with `content`/`encrypted_content` staying optional as today. Function calls use
/// the domain `ToolCallId` as `call_id`, and tool results join their Tools-owned text
/// parts deterministically with `\n`.
fn encode_input_items(messages: &[ModelMessage]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        match message.as_ref() {
            ModelMessageRef::User { content } => {
                let parts: Vec<Value> = content
                    .iter()
                    .map(|part| json!({"type": "input_text", "text": part.as_text()}))
                    .collect();
                items.push(json!({"type": "message", "role": "user", "content": parts}));
            }
            ModelMessageRef::Assistant { content } => {
                for block in content {
                    match block.as_ref() {
                        ModelAssistantContentRef::Text(text) => {
                            items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}],
                            }));
                        }
                        ModelAssistantContentRef::Reasoning(reasoning) => {
                            let Some(item_id) = reasoning.provider_item_id() else {
                                continue;
                            };
                            let mut item = Map::new();
                            item.insert("type".into(), json!("reasoning"));
                            item.insert("id".into(), json!(item_id.as_str()));
                            // The official ReasoningItem requires `summary`: always emit
                            // the field, with a single summary_text entry when a summary
                            // exists and an empty array otherwise.
                            let summary = reasoning.summary().map_or_else(Vec::new, |summary| {
                                vec![json!({"type": "summary_text", "text": summary})]
                            });
                            item.insert("summary".into(), Value::Array(summary));
                            if let Some(text) = reasoning.text() {
                                item.insert(
                                    "content".into(),
                                    json!([{"type": "reasoning_text", "text": text}]),
                                );
                            }
                            if let Some(encrypted) = reasoning.encrypted() {
                                item.insert("encrypted_content".into(), json!(encrypted));
                            }
                            items.push(Value::Object(item));
                        }
                        ModelAssistantContentRef::ToolCall {
                            tool_call_id,
                            name,
                            arguments,
                        } => {
                            items.push(json!({
                                "type": "function_call",
                                "call_id": tool_call_id.as_str(),
                                "name": name.as_str(),
                                "arguments": arguments.canonical_json(),
                                "status": "completed",
                            }));
                        }
                    }
                }
            }
            ModelMessageRef::Tool {
                tool_call_id,
                content,
            } => {
                let output = content
                    .parts()
                    .iter()
                    .map(|part| part.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id.as_str(),
                    "output": output,
                    "status": "completed",
                }));
            }
        }
    }
    items
}

/// ToolSpec function definitions preserve name, description, and the canonical bounded
/// schema verbatim; no speculative hosted-tool fields are emitted.
fn encode_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|spec| {
            let parameters: Value = serde_json::from_str(spec.input_schema().canonical_json())
                .expect("bounded schema canonical JSON is always valid JSON");
            json!({
                "type": "function",
                "name": spec.name().as_str(),
                "description": spec.description(),
                "parameters": parameters,
            })
        })
        .collect()
}

/// Minimal recursive OpenAI strict-mode sanitizer over the already-validated local
/// subset: only object nodes (`type:"object"` or `properties` present) get
/// `additionalProperties:false`, `properties` (an empty map when absent), and
/// `required` forced to all property names (an empty array when there are none);
/// scalar and array nodes keep only type/description/enum/const and never gain
/// `additionalProperties`. Arrays recurse into `items`, objects into `properties`.
/// The source schema is never mutated; a fresh provider copy is built (and `$schema`
/// is dropped from the wire copy).
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
// SSE event dispatch
// ---------------------------------------------------------------------------

enum Dispatch {
    Continue,
    Success(Box<ProviderAttemptResult>),
    Failure(ProviderAttemptError),
}

/// Dispatches one completed SSE frame. Event identity comes from the JSON `type`
/// field (OpenAI Responses events carry their type in the payload). Unknown valid
/// event types are ignored; a malformed JSON event or a malformed required event
/// field fails closed as `InvalidProviderResponse` with the current delivery.
/// Non-empty text deltas record their validated output index in
/// `streamed_text_indexes` so the terminal can verify the published `content_index`
/// contract.
fn dispatch(
    data: &str,
    progress: &ModelProgressPublisher,
    request_id: &Option<ProviderRequestId>,
    delivery: &mut ProviderRequestDeliveryState,
    streamed_text_indexes: &mut Vec<u32>,
) -> Result<Dispatch, ProviderAttemptError> {
    let value: Value =
        serde_json::from_str(data).map_err(|_| invalid_provider_response(*delivery))?;
    let Some(event_type) = value.get("type").and_then(Value::as_str) else {
        return Err(invalid_provider_response(*delivery));
    };
    match event_type {
        "response.output_text.delta" => {
            let output_index = required_u64(&value, "output_index")
                .map_err(|_| invalid_provider_response(*delivery))?;
            let delta =
                required_str(&value, "delta").map_err(|_| invalid_provider_response(*delivery))?;
            let content_index =
                u32::try_from(output_index).map_err(|_| invalid_provider_response(*delivery))?;
            // An empty delta is validated but publishes no progress and does not
            // advance delivery: only real semantic bytes mark output started, and
            // only real deltas are correlated with the terminal.
            if !delta.is_empty() {
                streamed_text_indexes.push(content_index);
                mark_semantic(delivery);
                progress.publish(ModelProgressEvent::ContentDelta {
                    content_index,
                    delta: ModelContentDelta::Text(Arc::from(delta)),
                });
            }
            Ok(Dispatch::Continue)
        }
        // Semantic deltas the current progress enum cannot carry: they are validated
        // structurally (required fields present), advance delivery truth, and are never
        // published — hidden raw reasoning text must not leak.
        "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta"
        | "response.refusal.delta"
        | "response.function_call_arguments.delta" => {
            let _ = required_u64(&value, "output_index")
                .map_err(|_| invalid_provider_response(*delivery))?;
            let delta =
                required_str(&value, "delta").map_err(|_| invalid_provider_response(*delivery))?;
            if !delta.is_empty() {
                mark_semantic(delivery);
            }
            Ok(Dispatch::Continue)
        }
        "response.output_item.done" => {
            let item = value
                .get("item")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            // Delivery advances only on real semantic payload: a message with a
            // non-empty output_text/refusal, reasoning with a non-empty
            // summary_text/reasoning_text/encrypted_content string, or any function
            // call. A merely-present content array or a null/empty encrypted_content
            // is not output.
            match item_type {
                "message" => {
                    if item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                match part.get("type").and_then(Value::as_str) {
                                    Some("output_text") => part
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .is_some_and(|text| !text.is_empty()),
                                    Some("refusal") => part
                                        .get("refusal")
                                        .and_then(Value::as_str)
                                        .is_some_and(|text| !text.is_empty()),
                                    _ => false,
                                }
                            })
                        })
                    {
                        mark_semantic(delivery);
                    }
                }
                "reasoning" => {
                    let has_non_empty_entry = ["summary", "content"].iter().any(|key| {
                        item.get(*key)
                            .and_then(Value::as_array)
                            .is_some_and(|entries| {
                                entries.iter().any(|entry| {
                                    matches!(
                                        entry.get("type").and_then(Value::as_str),
                                        Some("summary_text") | Some("reasoning_text")
                                    ) && entry
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .is_some_and(|text| !text.is_empty())
                                })
                            })
                    });
                    if has_non_empty_entry
                        || item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.is_empty())
                    {
                        mark_semantic(delivery);
                    }
                }
                "function_call" => mark_semantic(delivery),
                _ => {}
            }
            Ok(Dispatch::Continue)
        }
        "response.completed" => {
            let response = value
                .get("response")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            let status = response
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            if status != "completed" {
                // A completed-typed terminal that is not completed is contradictory.
                return Err(invalid_provider_response(*delivery));
            }
            let result =
                normalize_terminal(response, request_id, *delivery, streamed_text_indexes)?;
            Ok(Dispatch::Success(Box::new(result)))
        }
        "response.failed" => {
            let response = value
                .get("response")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            Ok(Dispatch::Failure(classify_response_failed(
                response, *delivery,
            )))
        }
        "response.incomplete" => {
            let _ = value
                .get("response")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            Ok(Dispatch::Failure(ProviderAttemptError {
                reason: ModelCallErrorReason::IncompleteResponse,
                retry_after: None,
                delivery: *delivery,
            }))
        }
        "error" => Ok(Dispatch::Failure(classify_stream_error(&value, *delivery))),
        _ => Ok(Dispatch::Continue),
    }
}

fn required_u64(value: &Value, key: &str) -> Result<u64, ()> {
    value.get(key).and_then(Value::as_u64).ok_or(())
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, ()> {
    value.get(key).and_then(Value::as_str).ok_or(())
}

fn mark_semantic(delivery: &mut ProviderRequestDeliveryState) {
    if *delivery == ProviderRequestDeliveryState::AcceptedNoOutput {
        *delivery = ProviderRequestDeliveryState::OutputStarted;
    }
}

/// SSE `error` event classification: structural on the machine-readable `code` field
/// only. The event's own `type` is the frame discriminator ("error"), never an error
/// envelope type, so the shared tuple mapping is applied with the code alone. Unknown
/// codes (including `server_error`) fail conservatively as `ProviderUnavailable` with
/// the current delivery.
fn classify_stream_error(
    event: &Value,
    delivery: ProviderRequestDeliveryState,
) -> ProviderAttemptError {
    let error_code = event.get("code").and_then(Value::as_str);
    let reason =
        classify_error_tuple(None, error_code).unwrap_or(ModelCallErrorReason::ProviderUnavailable);
    ProviderAttemptError {
        reason,
        retry_after: None,
        delivery,
    }
}

/// `response.failed` classification from the embedded `response.error` typed
/// envelope, with the current delivery (the request was already accepted). Unknown
/// tuples stay `ProviderUnavailable`.
fn classify_response_failed(
    response: &Map<String, Value>,
    delivery: ProviderRequestDeliveryState,
) -> ProviderAttemptError {
    let (error_type, error_code) = match response.get("error") {
        Some(error) => (
            error.get("type").and_then(Value::as_str),
            error.get("code").and_then(Value::as_str),
        ),
        None => (None, None),
    };
    let reason = classify_error_tuple(error_type, error_code)
        .unwrap_or(ModelCallErrorReason::ProviderUnavailable);
    ProviderAttemptError {
        reason,
        retry_after: None,
        delivery,
    }
}

// ---------------------------------------------------------------------------
// Terminal normalization
// ---------------------------------------------------------------------------

/// Normalizes the `response.completed` terminal in provider order: one domain content
/// block per output item — message text (ordered `output_text` parts merged), refusal
/// text, reasoning (provider item id/summary/text/encrypted; no signature exists on
/// this protocol), and typed function calls. Completed messages must be
/// assistant/completed; reasoning and function_call statuses are optional but must be
/// `completed` when present and non-null; optional nullable fields (usage,
/// service_tier, encrypted content, function item id) accept explicit null. Unknown
/// output items, malformed ids/names/arguments, and impossible mixed refusal/text/tool
/// shapes fail closed.
///
/// The progress `content_index` contract is verified here: every non-empty streamed
/// text `output_index` must name a terminal message item with non-empty `output_text`
/// whose block lands at exactly that normalized content index. Because empty
/// message/reasoning items normalize to zero blocks, such empty items are accepted
/// only as a trailing suffix (e.g. the M12 trailing empty reasoning echo); an empty
/// item before any representable item would silently diverge provider `output_index`
/// from `content[]` and fails closed.
fn normalize_terminal(
    response: &Map<String, Value>,
    request_id: &Option<ProviderRequestId>,
    delivery: ProviderRequestDeliveryState,
    streamed_text_indexes: &[u32],
) -> Result<ProviderAttemptResult, ProviderAttemptError> {
    let response_id: ProviderResponseId = response
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_provider_response(delivery))?;

    let mut content: Vec<ProviderAttemptContent> = Vec::new();
    // Terminal output positions of message items with non-empty output_text, paired
    // with the normalized content index of their (single) text block.
    let mut text_block_content_indexes: Vec<(usize, usize)> = Vec::new();
    let mut has_refusal = false;
    let mut has_non_refusal_visible = false;
    let mut has_tool_call = false;
    // Once an item normalized to zero blocks is seen, only further empty items may
    // follow; a representable item after an empty one would make provider
    // output_index and content[] indices diverge.
    let mut empty_tail_started = false;

    for (position, item) in output.iter().enumerate() {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_provider_response(delivery))?;
        let blocks_before = content.len();
        match item_type {
            "message" => normalize_message_item(
                item,
                delivery,
                &mut content,
                &mut has_refusal,
                &mut has_non_refusal_visible,
            )?,
            "reasoning" => normalize_reasoning_item(item, delivery, &mut content)?,
            "function_call" => {
                normalize_function_call_item(item, delivery, &mut content)?;
                has_tool_call = true;
            }
            _ => return Err(invalid_provider_response(delivery)),
        }
        if content.len() > blocks_before {
            if empty_tail_started {
                return Err(invalid_provider_response(delivery));
            }
            if item_type == "message" && message_has_non_empty_output_text(item) {
                text_block_content_indexes.push((position, blocks_before));
            }
        } else {
            empty_tail_started = true;
        }
    }
    // Every streamed text delta must resolve to the terminal text block it claimed:
    // the output position and the final normalized content index both equal the
    // streamed index.
    for &streamed in streamed_text_indexes {
        let streamed = usize::try_from(streamed).expect("u32 always fits usize");
        if !text_block_content_indexes
            .iter()
            .any(|&(position, content_index)| position == streamed && content_index == streamed)
        {
            return Err(invalid_provider_response(delivery));
        }
    }
    if has_refusal && (has_tool_call || has_non_refusal_visible) {
        return Err(invalid_provider_response(delivery));
    }
    let finish_reason = if has_tool_call {
        ModelFinishReason::ToolCalls
    } else if has_refusal {
        ModelFinishReason::Refused
    } else {
        ModelFinishReason::Stop
    };

    let usage = normalize_usage(response.get("usage"), delivery)?;
    // service_tier is optional: both absent and explicit null mean "no tier".
    let service_tier = match response.get("service_tier") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|tier| tier.parse::<RedactedProviderCode>().ok())
                .ok_or_else(|| invalid_provider_response(delivery))?,
        ),
    };

    Ok(ProviderAttemptResult {
        response_id: Some(response_id),
        content: content.into(),
        finish_reason,
        usage,
        metadata: ProviderResponseMetadata::new(
            request_id.clone(),
            Some(
                "completed"
                    .parse::<RedactedProviderCode>()
                    .expect("literal is valid opaque ASCII"),
            ),
            service_tier,
        ),
    })
}

fn normalize_message_item(
    item: &Value,
    delivery: ProviderRequestDeliveryState,
    content: &mut Vec<ProviderAttemptContent>,
    has_refusal: &mut bool,
    has_non_refusal_visible: &mut bool,
) -> Result<(), ProviderAttemptError> {
    // For a completed message our frozen contract requires exactly assistant/completed.
    if item.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(invalid_provider_response(delivery));
    }
    if item.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(invalid_provider_response(delivery));
    }
    // The message item id must be a well-formed provider item id; it is validated but
    // not retained (the domain has no message-item carrier).
    item.get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse::<ProviderItemId>().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let parts = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let mut text_parts = Vec::new();
    let mut refusal_parts = Vec::new();
    for part in parts {
        let part_type = part
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_provider_response(delivery))?;
        match part_type {
            "output_text" => {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_provider_response(delivery))?;
                text_parts.push(text);
            }
            "refusal" => {
                // OpenAI Responses refusal parts carry the text in `refusal`, not `text`.
                let text = part
                    .get("refusal")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_provider_response(delivery))?;
                refusal_parts.push(text);
            }
            _ => return Err(invalid_provider_response(delivery)),
        }
    }
    if !text_parts.is_empty() && !refusal_parts.is_empty() {
        return Err(invalid_provider_response(delivery));
    }
    if !text_parts.is_empty() {
        *has_non_refusal_visible = true;
        content.push(ProviderAttemptContent::Text(Arc::from(text_parts.join(""))));
    }
    if !refusal_parts.is_empty() {
        *has_refusal = true;
        content.push(ProviderAttemptContent::Text(Arc::from(
            refusal_parts.join("\n"),
        )));
    }
    Ok(())
}

/// True when the terminal message item contains at least one `output_text` part with
/// a non-empty `text` string — the only shape a streamed text delta may claim.
fn message_has_non_empty_output_text(item: &Value) -> bool {
    item.get("content")
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                part.get("type").and_then(Value::as_str) == Some("output_text")
                    && part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
            })
        })
}

fn normalize_reasoning_item(
    item: &Value,
    delivery: ProviderRequestDeliveryState,
    content: &mut Vec<ProviderAttemptContent>,
) -> Result<(), ProviderAttemptError> {
    // The reasoning item id is required (reasoning replays depend on it); status is
    // optional — when present and non-null it must be "completed".
    let id: ProviderItemId = item
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    match item.get("status") {
        None | Some(Value::Null) => {}
        Some(status) => {
            if status.as_str() != Some("completed") {
                return Err(invalid_provider_response(delivery));
            }
        }
    }
    let mut summaries = Vec::new();
    if let Some(summary) = item.get("summary") {
        let entries = summary
            .as_array()
            .ok_or_else(|| invalid_provider_response(delivery))?;
        for entry in entries {
            let entry_type = entry
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_provider_response(delivery))?;
            if entry_type != "summary_text" {
                return Err(invalid_provider_response(delivery));
            }
            let text = entry
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_provider_response(delivery))?;
            summaries.push(text);
        }
    }
    let mut texts = Vec::new();
    if let Some(content_value) = item.get("content") {
        let entries = content_value
            .as_array()
            .ok_or_else(|| invalid_provider_response(delivery))?;
        for entry in entries {
            let entry_type = entry
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_provider_response(delivery))?;
            if entry_type != "reasoning_text" {
                return Err(invalid_provider_response(delivery));
            }
            let text = entry
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_provider_response(delivery))?;
            texts.push(text);
        }
    }
    // encrypted_content is optional: both absent and explicit null mean "no encrypted
    // artifact" (the include[] flag is advisory, not guaranteed).
    let encrypted = match item.get("encrypted_content") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_provider_response(delivery))?,
        ),
    };
    if summaries.is_empty() && texts.is_empty() && encrypted.is_none() {
        // The provider emitted a reasoning item with no representable artifact
        // (the completed response may echo an empty reasoning item); it contributes
        // no domain content block.
        return Ok(());
    }
    let reasoning = ReasoningContent::new(
        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        },
        if summaries.is_empty() {
            None
        } else {
            Some(summaries.join("\n"))
        },
        encrypted,
        None,
        Some(id),
    )
    .map_err(|_| invalid_provider_response(delivery))?;
    content.push(ProviderAttemptContent::Reasoning(reasoning));
    Ok(())
}

fn normalize_function_call_item(
    item: &Value,
    delivery: ProviderRequestDeliveryState,
    content: &mut Vec<ProviderAttemptContent>,
) -> Result<(), ProviderAttemptError> {
    // The item id is optional in the official Responses schema: when present and
    // non-null it must be a well-formed provider item id, but it is never retained;
    // absent or null is accepted. `call_id`, `name`, and `arguments` stay required.
    // status is also optional — when present and non-null it must be "completed".
    match item.get("id") {
        None | Some(Value::Null) => {}
        Some(id) => {
            if id
                .as_str()
                .and_then(|id| id.parse::<ProviderItemId>().ok())
                .is_none()
            {
                return Err(invalid_provider_response(delivery));
            }
        }
    }
    match item.get("status") {
        None | Some(Value::Null) => {}
        Some(status) => {
            if status.as_str() != Some("completed") {
                return Err(invalid_provider_response(delivery));
            }
        }
    }
    let tool_call_id: ToolCallId = item
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let name: ToolName = item
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| name.parse().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let arguments: BoundedJsonObject = item
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|arguments| arguments.parse().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    content.push(ProviderAttemptContent::ToolCall {
        tool_call_id,
        name,
        arguments,
    });
    Ok(())
}

/// Normalizes usage to the closed domain shape: input/output/reasoning/cache-read/total.
/// Cache-write and cost are not representable on this protocol. Usage is optional —
/// both absent and explicit null mean "no usage" — and any present field must be a
/// non-negative integer; the details objects are likewise optional/nullable.
fn normalize_usage(
    value: Option<&Value>,
    delivery: ProviderRequestDeliveryState,
) -> Result<Option<ModelUsage>, ProviderAttemptError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let usage = value
        .as_object()
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let input_tokens = optional_usage_u64(usage, "input_tokens")
        .map_err(|_| invalid_provider_response(delivery))?;
    let output_tokens = optional_usage_u64(usage, "output_tokens")
        .map_err(|_| invalid_provider_response(delivery))?;
    let total_tokens = optional_usage_u64(usage, "total_tokens")
        .map_err(|_| invalid_provider_response(delivery))?;
    let cached_tokens = match usage.get("input_tokens_details") {
        None | Some(Value::Null) => None,
        Some(details) => optional_usage_u64(
            details
                .as_object()
                .ok_or_else(|| invalid_provider_response(delivery))?,
            "cached_tokens",
        )
        .map_err(|_| invalid_provider_response(delivery))?,
    };
    let reasoning_tokens = match usage.get("output_tokens_details") {
        None | Some(Value::Null) => None,
        Some(details) => optional_usage_u64(
            details
                .as_object()
                .ok_or_else(|| invalid_provider_response(delivery))?,
            "reasoning_tokens",
        )
        .map_err(|_| invalid_provider_response(delivery))?,
    };
    Ok(Some(ModelUsage::new(
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cached_tokens,
        None,
        total_tokens,
        None,
    )))
}

fn optional_usage_u64(usage: &Map<String, Value>, key: &str) -> Result<Option<u64>, ()> {
    match usage.get(key) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(()),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::model_gateway::provider_transport::loopback::{
        CapturedRequest, LoopbackServer, ScriptedResponse,
    };
    use crate::model_gateway::provider_transport::{SseParseError, drain_bounded};
    use crate::model_gateway::tests::{
        request_for_model, request_for_model_with_tools, request_with_output_contract,
        resolve_request, scripted_tool_set, structured_definition, structured_request,
        structured_schema, text_definition,
    };
    use crate::model_gateway::{
        EffectiveModelLimits, FinalizedAssistantContent, ModelCapabilities, ModelDefinition,
        ModelDefinitionVersion, ModelGateway, ModelGenerationDefaults, ModelSelection,
        ModelSourceAdapter, ModelSourceFuture, OutputContract, ProviderAdapter,
        ReasoningCapabilities, StructuredOutputContract, TokenEstimateRate, TurnModelSnapshot,
    };
    use crate::prompt::{ModelAssistantContent, ModelMessage};
    use crate::tools::ToolResultContent;

    struct SingleModelSource {
        definitions: std::sync::Mutex<Vec<ModelDefinition>>,
    }

    impl SingleModelSource {
        fn new(definition: ModelDefinition) -> Self {
            Self {
                definitions: std::sync::Mutex::new(vec![definition]),
            }
        }
    }

    impl ModelSourceAdapter for SingleModelSource {
        fn discover(&self) -> ModelSourceFuture<'_> {
            let definitions = self.definitions.lock().unwrap().clone();
            Box::pin(async move { Ok(definitions) })
        }
    }

    async fn gateway_and_model(
        definition: ModelDefinition,
    ) -> (Arc<ModelGateway>, Arc<TurnModelSnapshot>) {
        let source = Arc::new(SingleModelSource::new(definition));
        let source_adapter: Arc<dyn ModelSourceAdapter> = source;
        let gateway = Arc::new(ModelGateway::new(vec![source_adapter]));
        let model = gateway
            .resolve_for_turn(gateway.initialize().await.unwrap(), resolve_request(None))
            .unwrap();
        (gateway, model)
    }

    /// Definition with explicit Low reasoning and Priority service class so the request
    /// encoder's reasoning/service mapping is exercised end to end.
    fn priority_low_reasoning_definition(adapter: Arc<dyn ProviderAdapter>) -> ModelDefinition {
        ModelDefinition::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            ModelDefinitionVersion::new(NonZeroU64::new(1).unwrap()),
            ModelCapabilities::text_only(ReasoningCapabilities::all(), true),
            EffectiveModelLimits::new(NonZeroU32::new(128_000), NonZeroU32::new(8_192)),
            TokenEstimateRate::new(NonZeroU32::new(3).unwrap(), 1).unwrap(),
            ModelGenerationDefaults::new(
                NonZeroU32::new(4_096).unwrap(),
                ModelReasoningSummary::Low,
                ModelServiceClass::Priority,
            ),
            adapter,
        )
        .unwrap()
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

    fn output_text_delta(output_index: u64, delta: &str) -> Value {
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": output_index,
            "content_index": 0,
            "sequence_number": 1,
            "delta": delta,
        })
    }

    fn completed_simple(text: &str) -> Value {
        json!({
            "type": "response.completed",
            "sequence_number": 1,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1752000000,
                "status": "completed",
                "model": "gpt-5",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text}],
                }],
                "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8},
            }
        })
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

    fn assert_request_shape(request: &CapturedRequest, credential: &str) {
        assert_eq!(request.method(), "POST");
        assert_eq!(request.path(), "/responses");
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {credential}").as_str())
        );
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
    // Pure parser/encoder/config unit tests
    // ------------------------------------------------------------------

    #[test]
    fn sse_parser_handles_fragmentation_line_endings_comments_and_multi_data() {
        let body = "event: response.created\r\ndata: {\"type\":\"response.created\"}\r\n\r\n\
                    : keep-alive\r\ndata: {\"type\":\"one\"}\ndata: {\"type\":\"two\"}\n\n\
                    id: 7\ndata: {\"type\":\"three\"}\r\n\r\ndata: {\"type\":\"tail\"}";
        let mut parser = SseParser::new(response_byte_limit());
        let mut events = Vec::new();
        // Arbitrary byte-by-byte fragmentation.
        for byte in body.bytes() {
            events.extend(parser.feed(&[byte]).expect("fragmented feed must parse"));
        }
        assert_eq!(
            events
                .iter()
                .map(|event| event.data.as_str())
                .collect::<Vec<_>>(),
            [
                r#"{"type":"response.created"}"#,
                "{\"type\":\"one\"}\n{\"type\":\"two\"}",
                r#"{"type":"three"}"#,
            ],
            "event/id fields and comments are ignored, multi-data lines join with newline, \
             and a trailing frame without a blank line is dropped on EOF"
        );

        // CR-only line endings parse identically.
        let mut parser = SseParser::new(response_byte_limit());
        let events = parser
            .feed(b"data: {\"a\":1}\r\rdata: {\"b\":2}\r\r")
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.data.as_str())
                .collect::<Vec<_>>(),
            [r#"{"a":1}"#, r#"{"b":2}"#]
        );

        // Mixed LF/CRLF within one feed.
        let mut parser = SseParser::new(response_byte_limit());
        let events = parser
            .feed(b"data: {\"c\":3}\r\n\r\ndata: {\"d\":4}\n\n")
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.data.as_str())
                .collect::<Vec<_>>(),
            [r#"{"c":3}"#, r#"{"d":4}"#]
        );
    }

    #[test]
    fn sse_parser_enforces_line_and_cumulative_byte_bounds() {
        let maximum = response_byte_limit();

        // A single line beyond the bound fails closed.
        let mut parser = SseParser::new(maximum);
        let mut chunk = vec![b'x'; maximum + 1];
        chunk.push(b'\n');
        assert_eq!(parser.feed(&chunk), Err(SseParseError::LimitExceeded));

        // The cumulative response-byte bound applies across arbitrary chunks.
        let mut parser = SseParser::new(maximum);
        parser
            .feed(&vec![b' '; maximum])
            .expect("exact bound is accepted");
        assert_eq!(parser.feed(b"x"), Err(SseParseError::LimitExceeded));

        // Invalid UTF-8 data fails closed.
        let mut parser = SseParser::new(maximum);
        assert_eq!(
            parser.feed(b"data: \xff\xfe\n\n"),
            Err(SseParseError::InvalidUtf8)
        );
    }

    #[test]
    fn reasoning_parameters_map_closed_levels() {
        assert_eq!(
            reasoning_parameters(ModelReasoningSummary::ProviderDefault),
            None
        );
        assert_eq!(
            reasoning_parameters(ModelReasoningSummary::Disabled),
            Some(json!({"effort": "none"}))
        );
        assert_eq!(
            reasoning_parameters(ModelReasoningSummary::Low),
            Some(json!({"effort": "low", "summary": "auto"}))
        );
        assert_eq!(
            reasoning_parameters(ModelReasoningSummary::Medium),
            Some(json!({"effort": "medium", "summary": "auto"}))
        );
        assert_eq!(
            reasoning_parameters(ModelReasoningSummary::High),
            Some(json!({"effort": "high", "summary": "auto"}))
        );
    }

    #[test]
    fn encode_input_items_maps_ordered_history_without_leaking_signature() {
        let user =
            ModelMessage::unstamped_user_text(Arc::from("What's the weather in Paris?")).unwrap();
        let replayable = ReasoningContent::new(
            Some("reasoning text".to_owned()),
            Some("reasoning summary".to_owned()),
            Some("encrypted".to_owned()),
            Some("SECRET-SIGNATURE".to_owned()),
            Some("rs_1".parse().unwrap()),
        )
        .unwrap();
        // Reasoning without a provider item id is not replayable and is skipped.
        let orphan =
            ReasoningContent::new(Some("orphan".to_owned()), None, None, None, None).unwrap();
        // Replayable reasoning with no summary must still emit the official-required
        // `summary` field as an empty array.
        let summary_less = ReasoningContent::new(
            Some("more reasoning".to_owned()),
            None,
            None,
            None,
            Some("rs_2".parse().unwrap()),
        )
        .unwrap();
        let assistant = ModelMessage::assistant(Arc::from([
            ModelAssistantContent::reasoning(replayable),
            ModelAssistantContent::reasoning(orphan),
            ModelAssistantContent::reasoning(summary_less),
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
            ToolResultContent::from_text_parts(vec![
                "22°C and sunny".to_owned(),
                "feels like 20°C".to_owned(),
            ])
            .unwrap(),
        );

        let items = encode_input_items(&[user, assistant, tool]);
        let wire = serde_json::to_string(&Value::Array(items.clone())).unwrap();
        assert!(
            !wire.contains("SECRET-SIGNATURE"),
            "signature must never be an OpenAI field"
        );
        assert!(
            !wire.contains("orphan"),
            "reasoning without item id must not be replayed"
        );
        assert_eq!(
            Value::Array(items),
            json!([
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "What's the weather in Paris?"}]},
                {"type": "reasoning", "id": "rs_1",
                 "summary": [{"type": "summary_text", "text": "reasoning summary"}],
                 "content": [{"type": "reasoning_text", "text": "reasoning text"}],
                 "encrypted_content": "encrypted"},
                {"type": "reasoning", "id": "rs_2",
                 "summary": [],
                 "content": [{"type": "reasoning_text", "text": "more reasoning"}]},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "assistant says"}]},
                {"type": "function_call", "call_id": "call_abc", "name": "get_weather",
                 "arguments": r#"{"city":"Paris"}"#, "status": "completed"},
                {"type": "function_call_output", "call_id": "call_abc",
                 "output": "22°C and sunny\nfeels like 20°C", "status": "completed"},
            ])
        );
    }

    #[test]
    fn structured_sanitizer_forces_object_strictness_without_touching_scalars() {
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
        let sanitized = sanitize_structured_schema(&contract);
        // Object nodes get additionalProperties:false, forced required, and a
        // properties map; scalar and array nodes are untouched beyond their supported
        // keywords, so no additionalProperties leaks onto them.
        assert_eq!(
            sanitized,
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
        // The source schema is never mutated.
        assert!(
            contract
                .schema()
                .canonical_json()
                .contains("\"required\":[\"status\"]")
        );
        assert!(
            !contract
                .schema()
                .canonical_json()
                .contains("\"required\":[\"status\",\"meta\"]")
        );

        // An object with no properties keeps strict shape: properties {} and required [].
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

        // A nested node with properties but no explicit type is still an object.
        let implicit = StructuredOutputContract::new(
            &model,
            None,
            structured_schema(
                r#"{"type":"object","properties":{"loose":{"properties":{"x":{"type":"string"}}}}}"#,
            ),
        )
        .unwrap();
        assert_eq!(
            sanitize_structured_schema(&implicit),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["loose"],
                "properties": {
                    "loose": {
                        "additionalProperties": false,
                        "required": ["x"],
                        "properties": {"x": {"type": "string"}},
                    }
                },
            })
        );

        // An array of scalars gains no additionalProperties anywhere.
        let array_of_scalars = StructuredOutputContract::new(
            &model,
            None,
            structured_schema(
                r#"{"type":"object","properties":{"nums":{"type":"array","items":{"type":"number"}}}}"#,
            ),
        )
        .unwrap();
        assert_eq!(
            sanitize_structured_schema(&array_of_scalars),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["nums"],
                "properties": {
                    "nums": {"type": "array", "items": {"type": "number"}},
                },
            })
        );
    }

    #[test]
    fn adapter_configuration_rejects_unsafe_endpoints_and_credentials() {
        for endpoint in [
            "ftp://api.openai.com/v1/responses",
            "ws://api.openai.com/v1/responses",
            "https://api.openai.com/v1/responses?key=SECRET",
            "https://api.openai.com/v1/responses?x=1",
            "https://api.openai.com/v1/responses#fragment",
            "https://user:pass@api.openai.com/v1/responses",
            "https://user@api.openai.com/v1/responses",
            "not a url",
            "",
        ] {
            assert_eq!(
                OpenAiResponsesProviderAdapter::new(endpoint, "sk-test").unwrap_err(),
                OpenAiProviderConfigError::InvalidEndpoint,
                "endpoint {endpoint:?} was accepted"
            );
        }
        let oversize = "x".repeat(257);
        for credential in ["", "has space", "bad\ncredential", oversize.as_str()] {
            assert_eq!(
                OpenAiResponsesProviderAdapter::new(
                    "https://api.openai.com/v1/responses",
                    credential
                )
                .unwrap_err(),
                OpenAiProviderConfigError::InvalidCredential,
                "credential {credential:?} was accepted"
            );
        }
        let adapter = OpenAiResponsesProviderAdapter::new(
            "https://api.openai.com/v1/responses",
            "sk-SECRET-CREDENTIAL",
        )
        .unwrap();
        let debug = format!("{adapter:?}");
        assert!(
            !debug.contains("sk-SECRET-CREDENTIAL"),
            "credential leaked: {debug}"
        );
        assert!(debug.contains("api.openai.com"));
        // Loopback http endpoints are accepted.
        assert!(
            OpenAiResponsesProviderAdapter::new("http://127.0.0.1:1234/responses", "sk-ok").is_ok()
        );
    }

    // ------------------------------------------------------------------
    // End-to-end loopback contract tests through ModelGateway
    // ------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_rich_request_and_terminal_mapping() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![("x-request-id".to_owned(), "req-123".to_owned())],
            body: sse(&[
                json!({"type": "response.created", "sequence_number": 0,
                       "response": {"id": "resp_123", "object": "response", "created_at": 1752000000,
                                    "status": "in_progress", "model": "gpt-5"}}),
                json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1",
                       "output_index": 1, "summary_index": 0, "sequence_number": 1,
                       "delta": "Let me think"}),
                output_text_delta(0, "The weather"),
                output_text_delta(0, " in Paris is 22°C."),
                json!({"type": "response.output_item.done", "item_id": "msg_456", "output_index": 0,
                       "sequence_number": 2,
                       "item": {"type": "message", "id": "msg_456", "role": "assistant",
                                "status": "completed",
                                "content": [{"type": "output_text", "text": "The weather in Paris is 22°C."}]}}),
                json!({"type": "response.output_item.done", "item_id": "rs_1", "output_index": 1,
                       "sequence_number": 3,
                       "item": {"type": "reasoning", "id": "rs_1",
                                "summary": [{"type": "summary_text", "text": "Hidden chain-of-thought"}],
                                "content": [{"type": "reasoning_text", "text": "Full reasoning"}],
                                "status": "completed"}}),
                json!({"type": "response.completed", "sequence_number": 4,
                "response": {
                    "id": "resp_123", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [
                        {"type": "message", "id": "msg_456", "role": "assistant",
                         "status": "completed",
                         "content": [{"type": "output_text", "text": "The weather in Paris is 22°C."}]},
                        {"type": "reasoning", "id": "rs_1",
                         "summary": [{"type": "summary_text", "text": "Hidden chain-of-thought"}],
                         "content": [{"type": "reasoning_text", "text": "Full reasoning"}],
                         "encrypted_content": "ZW5jcnlwdGVk", "status": "completed"},
                        {"type": "function_call", "id": "fc_999", "call_id": "call_xyz",
                         "name": "echo", "arguments": "{}", "status": "completed"},
                    ],
                    "usage": {
                        "input_tokens": 42,
                        "input_tokens_details": {"cached_tokens": 7},
                        "output_tokens": 11,
                        "output_tokens_details": {"reasoning_tokens": 4},
                        "total_tokens": 53,
                    },
                    "service_tier": "priority",
                }}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test-credential")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(priority_low_reasoning_definition(provider)).await;
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
        assert_request_shape(&requests[0], "sk-test-credential");
        let wire: Value = requests[0].json_body();

        // --- Rich request mapping. ---
        assert_eq!(wire["model"], "gpt-5");
        assert_eq!(
            wire["instructions"],
            "SECRET required system\n\nSECRET base system"
        );
        assert_eq!(wire["max_output_tokens"], 4096);
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["store"], false);
        assert_eq!(wire["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(
            wire["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );
        assert_eq!(wire["service_tier"], "priority");
        assert_eq!(wire["tool_choice"], "auto");
        assert_eq!(
            wire["input"],
            json!([
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "SECRET live user input"}]}
            ])
        );
        assert_eq!(
            wire["tools"],
            json!([
                {"type": "function", "name": "echo",
                 "description": "Echo a bounded JSON value", "parameters": {}}
            ])
        );

        // --- Progress: only the representable text deltas, normalized content_index. ---
        assert_eq!(
            drain(&mut progress_rx),
            [
                ModelProgressEvent::ContentDelta {
                    content_index: 0,
                    delta: ModelContentDelta::Text(Arc::from("The weather")),
                },
                ModelProgressEvent::ContentDelta {
                    content_index: 0,
                    delta: ModelContentDelta::Text(Arc::from(" in Paris is 22°C.")),
                },
            ]
        );

        // --- Terminal mapping: provider order text -> reasoning -> tool call, matching
        // the streamed output_index values (text 0, reasoning 1). ---
        let response = result.response();
        assert_eq!(response.model().model_id().as_str(), "gpt-5");
        assert_eq!(response.model().reasoning(), ModelReasoningSummary::Low);
        assert_eq!(
            response.model().service_class(),
            ModelServiceClass::Priority
        );
        assert_eq!(response.finish_reason(), ModelFinishReason::ToolCalls);
        assert_eq!(response.response_id().unwrap().as_str(), "resp_123");
        assert_eq!(response.effective_max_output_tokens().get(), 4096);
        assert_eq!(
            response.metadata().provider_request_id().unwrap().as_str(),
            "req-123"
        );
        assert_eq!(
            response.metadata().raw_finish_code().unwrap().as_str(),
            "completed"
        );
        assert_eq!(
            response.metadata().service_tier().unwrap().as_str(),
            "priority"
        );
        assert_eq!(response.content().len(), 3);
        match &response.content()[0] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, "The weather in Paris is 22°C.");
            }
            other => panic!("expected merged text first, got {other:?}"),
        }
        match &response.content()[1] {
            FinalizedAssistantContent::Reasoning(reasoning) => {
                assert_eq!(reasoning.summary(), Some("Hidden chain-of-thought"));
                assert_eq!(reasoning.text(), Some("Full reasoning"));
                assert_eq!(reasoning.encrypted(), Some("ZW5jcnlwdGVk"));
                assert_eq!(reasoning.provider_item_id().unwrap().as_str(), "rs_1");
                assert!(reasoning.signature().is_none());
            }
            other => panic!("expected reasoning second, got {other:?}"),
        }
        match &response.content()[2] {
            FinalizedAssistantContent::ToolCall {
                tool_call_id,
                name,
                arguments,
            } => {
                assert_eq!(tool_call_id.as_str(), "call_xyz");
                assert_eq!(name.as_str(), "echo");
                assert_eq!(arguments.canonical_json(), "{}");
            }
            other => panic!("expected typed tool call third, got {other:?}"),
        }
        let usage = response.usage().unwrap();
        assert_eq!(usage.input_tokens(), Some(42));
        assert_eq!(usage.output_tokens(), Some(11));
        assert_eq!(usage.reasoning_tokens(), Some(4));
        assert_eq!(usage.cache_read_tokens(), Some(7));
        assert_eq!(usage.cache_write_tokens(), None);
        assert_eq!(usage.provider_total_tokens(), Some(53));
        assert_eq!(usage.reported_cost(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_structured_request_maps_sanitized_schema_and_validates_terminal() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                output_text_delta(0, r#"{"summary":"SECRET hello","tags":["a","b"]}"#),
                json!({"type": "response.completed", "sequence_number": 1,
                "response": {
                    "id": "resp_2", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{
                        "type": "message", "id": "msg_2", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text",
                                     "text": r#"{"summary":"SECRET hello","tags":["a","b"]}"#}],
                    }],
                    "usage": {"input_tokens": 9, "output_tokens": 7, "total_tokens": 16},
                }}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let definition =
            structured_definition(1, 4_096, NonZeroU32::new(65_536).unwrap(), provider);
        let (gateway, model) = gateway_and_model(definition).await;
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
        assert_request_shape(&requests[0], "sk-test");
        let wire: Value = requests[0].json_body();
        assert_eq!(wire["model"], "gpt-5");
        assert_eq!(
            wire["text"]["format"],
            json!({
                "type": "json_schema",
                "name": "response_schema",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["summary", "tags"],
                    "properties": {
                        "summary": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                    },
                },
            })
        );
        assert!(
            wire.get("tools").is_none(),
            "structured requests are tool-free"
        );
        assert_eq!(
            wire["tool_choice"], "none",
            "a tool-free structured contract still encodes tool_choice none"
        );
        assert!(
            wire.get("reasoning").is_none(),
            "ProviderDefault omits reasoning"
        );
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["store"], false);

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
    async fn loopback_early_eof_before_output_maps_to_request_outcome_unknown() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[json!({"type": "response.created", "sequence_number": 0,
                               "response": {"id": "resp_1", "object": "response",
                                            "created_at": 1752000000, "status": "in_progress",
                                            "model": "gpt-5"}})]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
                output_text_delta(0, "partial "),
                json!({"type": "response.output_item.done", "item_id": "msg_early",
                       "output_index": 0, "sequence_number": 1,
                       "item": {"type": "message", "id": "msg_early", "role": "assistant",
                                "status": "completed",
                                "content": [{"type": "output_text", "text": "partial "}]}}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
    async fn loopback_stream_error_event_server_error_maps_to_stream_interrupted() {
        for body in [
            sse(&[
                output_text_delta(0, "partial "),
                json!({"type": "error", "code": "server_error", "message": "boom"}),
            ]),
            sse(&[
                output_text_delta(0, "partial "),
                json!({"type": "response.failed", "sequence_number": 2,
                       "response": {"id": "resp_f", "object": "response", "created_at": 1752000000,
                                    "status": "failed", "model": "gpt-5",
                                    "error": {"code": "server_error", "message": "boom",
                                              "param": null, "type": "server_error"}}}),
            ]),
        ] {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 200,
                content_type: "text/event-stream",
                headers: vec![],
                body,
                gate: 0,
            }]);
            let adapter = Arc::new(
                OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
                "a provider-declared stream failure after output must carry the current delivery"
            );
            assert_eq!(
                error.delivery(),
                ProviderRequestDeliveryState::OutputStarted
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_http_400_context_length_exceeded_maps_to_context_overflow() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 400,
            content_type: "application/json",
            headers: vec![],
            body: r#"{"error":{"message":"context too long","type":"invalid_request_error",
                        "param":"messages","code":"context_length_exceeded"}}"#
                .to_owned(),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
        assert_eq!(error.reason(), ModelCallErrorReason::ContextOverflow);
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::RejectedBeforeExecution
        );
        assert_eq!(error.retry_after(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_http_429_rate_limit_carries_numeric_retry_after() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 429,
            content_type: "application/json",
            headers: vec![("retry-after".to_owned(), "17".to_owned())],
            body: r#"{"error":{"message":"rate limited","type":"rate_limit_error",
                        "code":"rate_limit_exceeded","param":null}}"#
                .to_owned(),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
        assert_eq!(error.reason(), ModelCallErrorReason::RateLimited);
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::RejectedBeforeExecution
        );
        assert_eq!(error.retry_after(), Some(Duration::from_secs(17)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_http_500_single_attempt_maps_to_request_outcome_unknown() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 500,
            content_type: "application/json",
            headers: vec![],
            body: r#"{"error":{"message":"provider exploded","type":"server_error"}}"#.to_owned(),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
            "a provider 5xx must make exactly one HTTP request with no automatic retry"
        );
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::RequestOutcomeUnknown,
            "HTTP 500 must not create retry-safe proof"
        );
        assert_eq!(error.delivery(), ProviderRequestDeliveryState::Unknown);
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
                OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
    async fn loopback_metadata_allowlist_keeps_request_id_and_never_retains_canary() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![
                ("x-request-id".to_owned(), "req-canary-probe".to_owned()),
                ("x-canary-secret".to_owned(), "CANARY-TOP-SECRET".to_owned()),
                ("openai-processing-ms".to_owned(), "42".to_owned()),
                ("retry-after".to_owned(), "99".to_owned()),
                ("set-cookie".to_owned(), "session=SECRET".to_owned()),
            ],
            body: sse(&[completed_simple("hello")]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test-credential")
                .unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
            "the allowlisted x-request-id enters validated metadata"
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
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_cancellation_before_send_returns_not_sent_without_any_request() {
        // A cancellation that is already observable before the adapter sends must
        // be Cancelled/NotSent, and no POST may ever be issued. The second
        // is_cancelled check after encoding/building closes the synchronous
        // race where a cancellation lands in that window: with no await between
        // the two checks it cannot be observed deterministically from a test
        // without sleeps, so this deterministic pre-send regression pins the
        // surrounding contract and the branch itself is guarded by review.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[completed_simple("ok")]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (_gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
        let request = request_for_model(model).await;
        let cancel = CancellationToken::new();
        cancel.cancel();

        // Drive the adapter directly (bypassing the gateway's own pre-cancel
        // short-circuit) so the adapter's pre-send NotSent contract is proven.
        let attempt = ProviderAttemptRequest {
            effective_max_output_tokens: request.effective_max_output_tokens(),
            call: Arc::clone(&request),
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
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                output_text_delta(0, "partial "),
                completed_simple("partial "),
            ]),
            gate: 1,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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

        // Deterministic ordering: the first delta is published only after the server
        // wrote it; cancellation happens before the server is released to write more.
        let first = progress_rx
            .recv()
            .await
            .expect("first delta must be published before cancellation");
        assert!(matches!(
            first,
            ModelProgressEvent::ContentDelta {
                content_index: 0,
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
    async fn loopback_no_tool_calls_contract_encodes_tool_choice_none_without_tools() {
        // NoToolCalls is reachable for Compaction with zero tools: the wire must still
        // forbid tool calls via tool_choice "none" even though no `tools` array exists.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[completed_simple("compact")]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
        let request =
            request_with_output_contract(&model, Some(&OutputContract::NoToolCalls)).await;

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
        assert_request_shape(&requests[0], "sk-test");
        let wire: Value = requests[0].json_body();
        assert!(wire.get("tools").is_none(), "no tools are declared");
        assert_eq!(
            wire["tool_choice"], "none",
            "a NoToolCalls contract without tools still encodes tool_choice none"
        );
        assert_eq!(result.response().finish_reason(), ModelFinishReason::Stop);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_refusal_only_terminal_completes_as_refused() {
        // OpenAI Responses refusal parts carry their text in `refusal`, never `text`.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[json!({
                "type": "response.completed",
                "sequence_number": 1,
                "response": {
                    "id": "resp_ref", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{
                        "type": "message", "id": "msg_ref", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "refusal", "refusal": "I cannot answer that."}],
                    }],
                },
            })]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
            result.response().finish_reason(),
            ModelFinishReason::Refused
        );
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, "I cannot answer that.");
            }
            other => panic!("expected refusal text, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_explicit_null_optionals_do_not_fail_a_completed_response() {
        // service_tier, usage, usage details, and reasoning encrypted_content are all
        // optional: explicit null must read exactly like absence.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[json!({
                "type": "response.completed",
                "sequence_number": 1,
                "response": {
                    "id": "resp_null", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "service_tier": null,
                    "output": [
                        {"type": "reasoning", "id": "rs_null",
                         "summary": [{"type": "summary_text", "text": "hidden"}],
                         "encrypted_content": null},
                        {"type": "message", "id": "msg_null", "role": "assistant",
                         "status": "completed",
                         "content": [{"type": "output_text", "text": "fine"}]},
                    ],
                    "usage": {
                        "input_tokens": 5, "input_tokens_details": null,
                        "output_tokens": 3, "output_tokens_details": null,
                        "total_tokens": 8,
                    },
                },
            })]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
        let response = result.response();
        assert_eq!(response.finish_reason(), ModelFinishReason::Stop);
        assert_eq!(
            response.metadata().service_tier(),
            None,
            "explicit null service_tier is absence"
        );
        let usage = response.usage().unwrap();
        assert_eq!(usage.input_tokens(), Some(5));
        assert_eq!(usage.output_tokens(), Some(3));
        assert_eq!(usage.provider_total_tokens(), Some(8));
        assert_eq!(
            usage.reasoning_tokens(),
            None,
            "null output_tokens_details is absence"
        );
        assert_eq!(usage.cache_read_tokens(), None);
        match &response.content()[0] {
            FinalizedAssistantContent::Reasoning(reasoning) => {
                assert_eq!(reasoning.summary(), Some("hidden"));
                assert_eq!(
                    reasoning.encrypted(),
                    None,
                    "null encrypted_content is absence"
                );
            }
            other => panic!("expected reasoning first, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_function_call_without_id_and_optional_statuses_is_accepted() {
        // The function_call item id is optional in the official Responses schema;
        // absent/null is accepted. reasoning/function_call statuses are optional too:
        // absent/null is accepted, any other value fails closed.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[json!({
                "type": "response.completed",
                "sequence_number": 1,
                "response": {
                    "id": "resp_fc", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [
                        {"type": "reasoning", "id": "rs_fc",
                         "summary": [{"type": "summary_text", "text": "hidden"}]},
                        {"type": "message", "id": "msg_fc", "role": "assistant",
                         "status": "completed",
                         "content": [{"type": "output_text", "text": "let me check"}]},
                        {"type": "function_call", "status": null,
                         "call_id": "call_opt", "name": "echo", "arguments": "{}"},
                        {"type": "function_call", "id": null,
                         "call_id": "call_null_id", "name": "echo", "arguments": "{}"},
                    ],
                },
            })]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
        let request = request_for_model_with_tools(Arc::clone(&model), scripted_tool_set()).await;

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
            result.response().finish_reason(),
            ModelFinishReason::ToolCalls
        );
        assert_eq!(result.response().content().len(), 4);
        for (index, expected_call_id) in [(2, "call_opt"), (3, "call_null_id")] {
            match &result.response().content()[index] {
                FinalizedAssistantContent::ToolCall {
                    tool_call_id,
                    name,
                    arguments,
                } => {
                    assert_eq!(tool_call_id.as_str(), expected_call_id);
                    assert_eq!(name.as_str(), "echo");
                    assert_eq!(arguments.canonical_json(), "{}");
                }
                other => panic!("expected typed tool call at {index}, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_malformed_terminal_item_semantics_fail_closed() {
        // Completed messages require role assistant and status completed; reasoning and
        // function_call statuses must be "completed" when present and non-null.
        let terminals: Vec<(Value, bool)> = vec![
            // (terminal response JSON, whether the request declares tools)
            (
                json!({
                    "id": "resp_bad", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "message", "id": "m", "role": "user",
                                 "status": "completed",
                                 "content": [{"type": "output_text", "text": "hi"}]}],
                }),
                false,
            ),
            (
                json!({
                    "id": "resp_bad", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "message", "id": "m", "status": "completed",
                                 "content": [{"type": "output_text", "text": "hi"}]}],
                }),
                false,
            ),
            (
                json!({
                    "id": "resp_bad", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "message", "id": "m", "role": "assistant",
                                 "status": "in_progress",
                                 "content": [{"type": "output_text", "text": "hi"}]}],
                }),
                false,
            ),
            (
                json!({
                    "id": "resp_bad", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "message", "id": "m", "role": "assistant",
                                 "status": null,
                                 "content": [{"type": "output_text", "text": "hi"}]}],
                }),
                false,
            ),
            (
                json!({
                    "id": "resp_bad", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "message", "id": "m", "role": "assistant",
                                 "content": [{"type": "output_text", "text": "hi"}]}],
                }),
                false,
            ),
            (
                json!({
                    "id": "resp_bad", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "reasoning", "id": "r", "status": "in_progress",
                                 "summary": [{"type": "summary_text", "text": "s"}]},
                                {"type": "message", "id": "m", "role": "assistant",
                                 "status": "completed",
                                 "content": [{"type": "output_text", "text": "hi"}]}],
                }),
                false,
            ),
            (
                json!({
                    "id": "resp_bad", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "function_call", "id": "fc_bad",
                                 "status": "in_progress",
                                 "call_id": "call_bad", "name": "echo", "arguments": "{}"}],
                }),
                true,
            ),
        ];
        for (index, (terminal, toolful)) in terminals.into_iter().enumerate() {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 200,
                content_type: "text/event-stream",
                headers: vec![],
                body: sse(&[json!({"type": "response.completed", "sequence_number": 1,
                                   "response": terminal})]),
                gate: 0,
            }]);
            let adapter = Arc::new(
                OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
            let request = if toolful {
                request_for_model_with_tools(Arc::clone(&model), scripted_tool_set()).await
            } else {
                request_for_model(model).await
            };

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
                "malformed terminal semantics (case {index}) must fail closed"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_empty_text_delta_publishes_nothing_and_keeps_accepted_no_output() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[output_text_delta(0, "")]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
        let request = request_for_model(model).await;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let progress = ModelProgressPublisher::new(move |event| {
            let _ = progress_tx.send(event);
        });

        let error = gateway
            .generate_model_turn(request, progress, CancellationToken::new())
            .await
            .unwrap_err();
        let requests = server.join();

        assert_eq!(requests.len(), 1);
        assert!(
            drain(&mut progress_rx).is_empty(),
            "an empty delta must never publish progress"
        );
        assert_eq!(
            error.reason(),
            ModelCallErrorReason::RequestOutcomeUnknown,
            "empty deltas do not advance delivery, so EOF stays AcceptedNoOutput"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::AcceptedNoOutput
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_output_item_done_advances_only_on_real_semantic_payload() {
        // output_item.done is an echo, not a promise: delivery advances only for a
        // message with non-empty output_text/refusal, reasoning with a non-empty
        // summary_text/reasoning_text/encrypted_content string, or a function call.
        // A merely-present content array or a null/empty encrypted_content stays
        // AcceptedNoOutput — the existing empty-delta test alone cannot prove this.
        let cases: &[(Value, ProviderRequestDeliveryState)] = &[
            (
                json!({"type": "message", "id": "m1", "role": "assistant",
                       "status": "completed", "content": []}),
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ),
            (
                json!({"type": "message", "id": "m2", "role": "assistant",
                       "status": "completed",
                       "content": [{"type": "output_text", "text": ""}]}),
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ),
            (
                json!({"type": "message", "id": "m3", "role": "assistant",
                       "status": "completed",
                       "content": [{"type": "refusal", "refusal": ""}]}),
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ),
            (
                json!({"type": "reasoning", "id": "r1", "status": "completed",
                       "summary": [{"type": "summary_text", "text": ""}]}),
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ),
            (
                json!({"type": "reasoning", "id": "r2", "status": "completed",
                       "content": [{"type": "reasoning_text", "text": ""}]}),
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ),
            (
                json!({"type": "reasoning", "id": "r3", "status": "completed",
                       "encrypted_content": null}),
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ),
            (
                json!({"type": "reasoning", "id": "r4", "status": "completed",
                       "encrypted_content": ""}),
                ProviderRequestDeliveryState::AcceptedNoOutput,
            ),
            (
                json!({"type": "message", "id": "m4", "role": "assistant",
                       "status": "completed",
                       "content": [{"type": "output_text", "text": "hi"}]}),
                ProviderRequestDeliveryState::OutputStarted,
            ),
            (
                json!({"type": "message", "id": "m5", "role": "assistant",
                       "status": "completed",
                       "content": [{"type": "refusal", "refusal": "no"}]}),
                ProviderRequestDeliveryState::OutputStarted,
            ),
            (
                json!({"type": "reasoning", "id": "r5", "status": "completed",
                       "summary": [{"type": "summary_text", "text": "s"}]}),
                ProviderRequestDeliveryState::OutputStarted,
            ),
            (
                json!({"type": "reasoning", "id": "r6", "status": "completed",
                       "encrypted_content": "x"}),
                ProviderRequestDeliveryState::OutputStarted,
            ),
        ];
        for (index, (item, expected)) in cases.iter().enumerate() {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 200,
                content_type: "text/event-stream",
                headers: vec![],
                body: sse(
                    &[json!({"type": "response.output_item.done", "item_id": "i",
                                   "output_index": 0, "sequence_number": 1, "item": item})],
                ),
                gate: 0,
            }]);
            let adapter = Arc::new(
                OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
                error.delivery(),
                *expected,
                "done item case {index} must leave delivery {expected:?}"
            );
            let expected_reason = if *expected == ProviderRequestDeliveryState::OutputStarted {
                ModelCallErrorReason::StreamInterrupted
            } else {
                ModelCallErrorReason::RequestOutcomeUnknown
            };
            assert_eq!(
                error.reason(),
                expected_reason,
                "done item case {index} must map through the delivery truth"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_streamed_text_index_mismatching_terminal_fails_closed() {
        // The text deltas claim output_index 1, but the terminal message sits at
        // output position 0: the streamed index names no terminal text block, so the
        // published content_index could not be the block's position in content[].
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                output_text_delta(1, "hello"),
                json!({"type": "response.completed", "sequence_number": 2,
                "response": {
                    "id": "resp_mismatch", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [{"type": "message", "id": "msg_0", "role": "assistant",
                                "status": "completed",
                                "content": [{"type": "output_text", "text": "hello"}]}],
                }}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
            "a streamed text index that cannot resolve to the terminal text block must fail closed"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::OutputStarted,
            "the failing terminal keeps the already-advanced delivery truth"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_empty_leading_item_before_later_output_fails_closed() {
        // An empty reasoning echo at output position 0 normalizes to zero blocks, so
        // the later message text would land at content index 0 while the provider
        // numbers it 1: silent index drift must fail closed even when no text delta
        // was streamed (the empty item must be a trailing suffix only).
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                json!({"type": "response.output_item.done", "item_id": "rs_empty",
                       "output_index": 0, "sequence_number": 1,
                       "item": {"type": "reasoning", "id": "rs_empty", "status": "completed"}}),
                json!({"type": "response.completed", "sequence_number": 2,
                "response": {
                    "id": "resp_lead", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [
                        {"type": "reasoning", "id": "rs_empty", "status": "completed"},
                        {"type": "message", "id": "msg_1", "role": "assistant",
                         "status": "completed",
                         "content": [{"type": "output_text", "text": "later text"}]},
                    ],
                }}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
            "an empty item before any representable item must fail closed"
        );
        assert_eq!(
            error.delivery(),
            ProviderRequestDeliveryState::AcceptedNoOutput
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_trailing_empty_reasoning_echo_is_accepted() {
        // M12 terminal shape: an empty reasoning echo after the text block is a valid
        // trailing suffix — it normalizes to zero blocks after the representable ones,
        // so output_index and content[] indices stay aligned.
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![],
            body: sse(&[
                output_text_delta(0, "hello"),
                json!({"type": "response.output_item.done", "item_id": "rs_empty",
                       "output_index": 1, "sequence_number": 1,
                       "item": {"type": "reasoning", "id": "rs_empty", "status": "completed"}}),
                json!({"type": "response.completed", "sequence_number": 2,
                "response": {
                    "id": "resp_tail", "object": "response", "created_at": 1752000000,
                    "status": "completed", "model": "gpt-5",
                    "output": [
                        {"type": "message", "id": "msg_0", "role": "assistant",
                         "status": "completed",
                         "content": [{"type": "output_text", "text": "hello"}]},
                        {"type": "reasoning", "id": "rs_empty", "status": "completed"},
                    ],
                }}),
            ]),
            gate: 0,
        }]);
        let adapter = Arc::new(
            OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test").unwrap(),
        );
        let provider: Arc<dyn ProviderAdapter> = adapter.clone();
        let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
        assert_eq!(result.response().finish_reason(), ModelFinishReason::Stop);
        assert_eq!(result.response().content().len(), 1);
        match &result.response().content()[0] {
            FinalizedAssistantContent::Text { text } => {
                assert_eq!(&**text, "hello");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_bounded_cancellation_after_first_chunk_is_cancelled_conservative() {
        // Deterministic cancellation mid-drain: the controlled poll_fn stream yields
        // one body chunk, then on its next poll notifies the test and parks forever.
        // The notification can only fire after the drain consumed the first chunk and
        // re-entered the read, so the test cancels exactly inside that read and the
        // biased select resolves to the typed Cancelled — no sleep, timeout,
        // yield_now, or blind polling.
        use futures_util::stream::poll_fn;
        use std::task::Poll;
        use tokio::sync::Notify;

        for delivery in [
            ProviderRequestDeliveryState::RejectedBeforeExecution,
            ProviderRequestDeliveryState::Unknown,
        ] {
            let probe = Arc::new(Notify::new());
            let stream_probe = Arc::clone(&probe);
            let mut first = true;
            let stream = poll_fn(move |_cx| {
                if first {
                    first = false;
                    Poll::Ready(Some(Ok::<&'static [u8], &'static str>(
                        b"{\"error\":{\"type\":\"server_error\"}}",
                    )))
                } else {
                    // Parked on the read after the first chunk; only cancellation
                    // may wake this drain.
                    stream_probe.notify_one();
                    Poll::Pending
                }
            });
            let cancel = CancellationToken::new();
            let drain_cancel = cancel.clone();
            let drain = tokio::spawn(async move {
                drain_bounded(stream, &drain_cancel, usize::MAX, delivery).await
            });

            probe.notified().await;
            cancel.cancel();
            let error = drain
                .await
                .expect("drain task must settle")
                .expect_err("cancellation must fail the drain");

            assert_eq!(error.reason, ModelCallErrorReason::Cancelled);
            assert_eq!(
                error.delivery, delivery,
                "cancellation delivery must be the conservative caller-supplied state"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_http_429_quota_classification_is_structural() {
        // 429 quota classification recognizes `insufficient_quota` in either error.type
        // or error.code, plus code `credit_balance_exhausted`; message text is ignored.
        for body in [
            r#"{"error":{"message":"pay up","type":"insufficient_quota","code":null}}"#,
            r#"{"error":{"message":"pay up","type":"server_error","code":"insufficient_quota"}}"#,
            r#"{"error":{"message":"pay up","type":"rate_limit_error","code":"credit_balance_exhausted"}}"#,
        ] {
            let server = LoopbackServer::spawn(vec![ScriptedResponse {
                status: 429,
                content_type: "application/json",
                headers: vec![("retry-after".to_owned(), "17".to_owned())],
                body: body.to_owned(),
                gate: 0,
            }]);
            let adapter = Arc::new(
                OpenAiResponsesProviderAdapter::new(&server.responses_endpoint(), "sk-test")
                    .unwrap(),
            );
            let provider: Arc<dyn ProviderAdapter> = adapter.clone();
            let (gateway, model) = gateway_and_model(text_definition(1, 4_096, provider)).await;
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
            assert_eq!(error.reason(), ModelCallErrorReason::QuotaExceeded);
            assert_eq!(
                error.delivery(),
                ProviderRequestDeliveryState::RejectedBeforeExecution
            );
            assert_eq!(
                error.retry_after(),
                None,
                "quota is not a retryable rate limit"
            );
        }
    }
}
