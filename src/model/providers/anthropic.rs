//! Model-owned Anthropic Messages provider.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde_json::{Map, Value, json};
use thiserror::Error;

use super::super::provider::{
    CredentialSource, ModelCallContext, ModelFuture, ModelProvider, ProviderEndpointPolicy,
};
use super::super::transport::{
    MAX_RESPONSE_BYTES, SseParser, cancelled, classify_send_error, invalid_provider_response,
    invalid_request_not_sent, is_event_stream, parse_retry_after, read_bounded_envelope,
    transport_read_error,
};
use super::super::types::{
    AssistantPart, DeliveryState, ModelDescriptor, ModelError, ModelErrorKind, ModelEvent,
    ModelFinishReason, ModelMessage, ModelRequest, ModelResponse, ProviderItemId, ReasoningContent,
    ToolCall, ToolSpec, Usage,
};
use crate::ids_v2::ToolCallId;
use crate::tools_v2::ToolName;

const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_JSON_BYTES: usize = 65_536;
const MAX_TEXT_BYTES: usize = 262_144;
const MAX_SIGNATURE_BYTES: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AnthropicProviderError {
    #[error("Anthropic endpoint violates the explicit endpoint policy")]
    InvalidEndpoint,
    #[error("Anthropic version must be non-empty safe opaque ASCII within 64 bytes")]
    InvalidVersion,
    #[error("Anthropic provider requires at least one model descriptor")]
    EmptyModels,
    #[error("Anthropic provider contains duplicate model selections")]
    DuplicateModel,
    #[error("all Anthropic model descriptors must use the same provider id")]
    ProviderMismatch,
    #[error("Anthropic HTTP client construction failed")]
    ClientBuild,
}

pub struct AnthropicMessagesProvider {
    id: super::super::types::ProviderId,
    models: Arc<[ModelDescriptor]>,
    client: reqwest::Client,
    endpoint: reqwest::Url,
    version: Box<str>,
    credential_source: Arc<dyn CredentialSource>,
}

impl AnthropicMessagesProvider {
    pub fn new(
        endpoint: &str,
        endpoint_policy: ProviderEndpointPolicy,
        version: &str,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, AnthropicProviderError> {
        if models.is_empty() {
            return Err(AnthropicProviderError::EmptyModels);
        }
        let id = models[0].selection().provider_id().clone();
        let mut selections = BTreeSet::new();
        for model in &models {
            if model.selection().provider_id() != &id {
                return Err(AnthropicProviderError::ProviderMismatch);
            }
            if !selections.insert(model.selection().clone()) {
                return Err(AnthropicProviderError::DuplicateModel);
            }
        }
        let endpoint = validate_endpoint(endpoint, endpoint_policy)?;
        validate_version(version)?;
        let client = super::super::transport::client_builder()
            .build()
            .map_err(|_| AnthropicProviderError::ClientBuild)?;
        Ok(Self {
            id,
            models: models.into(),
            client,
            endpoint,
            version: version.into(),
            credential_source,
        })
    }

    pub fn new_https(
        endpoint: &str,
        version: &str,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, AnthropicProviderError> {
        Self::new(
            endpoint,
            ProviderEndpointPolicy::HttpsOnly,
            version,
            credential_source,
            models,
        )
    }

    pub fn new_loopback_http(
        endpoint: &str,
        version: &str,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, AnthropicProviderError> {
        Self::new(
            endpoint,
            ProviderEndpointPolicy::AllowLoopbackHttp,
            version,
            credential_source,
            models,
        )
    }

    async fn run(
        &self,
        request: ModelRequest,
        ctx: ModelCallContext,
    ) -> Result<ModelResponse, ModelError> {
        let descriptor = self
            .models
            .iter()
            .find(|model| model.selection() == request.selection())
            .ok_or(ModelError::InvalidRequest)?;
        if !request_limits_fit(request.limits(), descriptor.limits())
            || !descriptor.supports_reasoning(request.reasoning())
        {
            return Err(ModelError::InvalidRequest);
        }
        if ctx.cancellation().is_cancelled() {
            return Err(cancelled(DeliveryState::NotSent));
        }
        let credential = tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => {
                return Err(cancelled(DeliveryState::NotSent));
            }
            credential = self.credential_source.resolve() => credential,
        };
        let Some(credential) = credential else {
            return Err(detailed(
                ModelErrorKind::AuthMissing,
                DeliveryState::NotSent,
                None,
            ));
        };
        if ctx.cancellation().is_cancelled() {
            return Err(cancelled(DeliveryState::NotSent));
        }
        let body = encode_request(&request, descriptor)?;
        let mut api_key = HeaderValue::from_bytes(credential.header().as_bytes())
            .map_err(|_| invalid_request_not_sent())?;
        api_key.set_sensitive(true);
        let http_request = self
            .client
            .post(self.endpoint.clone())
            .header("x-api-key", api_key)
            .header("anthropic-version", self.version.as_ref())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body)
            .build()
            .map_err(|_| invalid_request_not_sent())?;
        if ctx.cancellation().is_cancelled() {
            return Err(cancelled(DeliveryState::NotSent));
        }

        let response = tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => {
                return Err(cancelled(DeliveryState::Unknown));
            }
            response = self.client.execute(http_request) => response,
        }
        .map_err(|error| classify_send_error(&error))?;

        let status = response.status().as_u16();
        let retry_after = parse_retry_after(response.headers());
        if !response.status().is_success() {
            let delivery = if (400..=499).contains(&status) {
                DeliveryState::RejectedBeforeExecution
            } else {
                DeliveryState::Unknown
            };
            let envelope = read_bounded_envelope(response, ctx.cancellation(), delivery).await?;
            return Err(classify_http_error(status, envelope.as_ref(), retry_after));
        }
        if !is_event_stream(response.headers().get(CONTENT_TYPE)) {
            return Err(invalid_provider_response(DeliveryState::AcceptedNoOutput));
        }

        let mut delivery = DeliveryState::AcceptedNoOutput;
        let mut parser = SseParser::new(MAX_RESPONSE_BYTES);
        let mut state = AnthropicStreamState::default();
        let expected_model = descriptor.api_model_name();
        let mut stream = response.bytes_stream();
        loop {
            tokio::select! {
                biased;
                _ = ctx.cancellation().cancelled() => {
                    return Err(cancelled(delivery));
                }
                chunk = stream.next() => {
                    let chunk = match chunk {
                        Some(Ok(chunk)) => chunk,
                        Some(Err(_)) | None => return Err(transport_read_error(delivery)),
                    };
                    let events = parser
                        .feed(&chunk)
                        .map_err(|_| invalid_provider_response(delivery))?;
                    for event in events {
                        match dispatch(
                            &event.data,
                            DispatchInput {
                                ctx: &ctx,
                                delivery: &mut delivery,
                                state: &mut state,
                                expected_model,
                                allowed_tools: request.tools(),
                            },
                        )? {
                            Dispatch::Continue => {}
                            Dispatch::Success(response) => return Ok(response),
                            Dispatch::Failure(error) => return Err(error),
                        }
                    }
                }
            }
        }
    }
}

impl fmt::Debug for AnthropicMessagesProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicMessagesProvider")
            .field("provider_id", &self.id)
            .field("model_count", &self.models.len())
            .field("endpoint", &"<redacted>")
            .field("version", &self.version.as_ref())
            .finish()
    }
}

impl ModelProvider for AnthropicMessagesProvider {
    fn id(&self) -> &super::super::types::ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        &self.models
    }

    fn generate<'a>(&'a self, request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'a> {
        Box::pin(self.run(request, ctx))
    }
}

fn validate_endpoint(
    endpoint: &str,
    policy: ProviderEndpointPolicy,
) -> Result<reqwest::Url, AnthropicProviderError> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| AnthropicProviderError::InvalidEndpoint)?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AnthropicProviderError::InvalidEndpoint);
    }
    match url.scheme() {
        "https" => Ok(url),
        "http"
            if policy == ProviderEndpointPolicy::AllowLoopbackHttp
                && url.host_str().is_some_and(is_numeric_loopback) =>
        {
            Ok(url)
        }
        _ => Err(AnthropicProviderError::InvalidEndpoint),
    }
}

fn is_numeric_loopback(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|address| address == std::net::Ipv6Addr::LOCALHOST)
}

fn validate_version(version: &str) -> Result<(), AnthropicProviderError> {
    if version.is_empty()
        || version.len() > 64
        || version
            .bytes()
            .any(|byte| !(0x21..=0x7e).contains(&byte) || matches!(byte, b'"' | b'\\'))
    {
        Err(AnthropicProviderError::InvalidVersion)
    } else {
        Ok(())
    }
}

fn request_limits_fit(
    requested: &super::super::types::ModelLimits,
    available: &super::super::types::ModelLimits,
) -> bool {
    requested
        .context_window_tokens()
        .zip(available.context_window_tokens())
        .is_none_or(|(requested, available)| requested <= available)
        && requested
            .max_output_tokens()
            .zip(available.max_output_tokens())
            .is_none_or(|(requested, available)| requested <= available)
}

fn detailed(
    kind: ModelErrorKind,
    delivery: DeliveryState,
    retry_after: Option<std::time::Duration>,
) -> ModelError {
    ModelError::detailed(kind, delivery, retry_after).unwrap_or(ModelError::Internal)
}

fn stream_failure(kind: ModelErrorKind, delivery: DeliveryState) -> ModelError {
    if let Ok(error) = ModelError::detailed(kind, delivery, None) {
        return error;
    }
    match delivery {
        DeliveryState::OutputStarted => detailed(
            ModelErrorKind::StreamInterrupted,
            DeliveryState::OutputStarted,
            None,
        ),
        DeliveryState::AcceptedNoOutput | DeliveryState::Unknown => detailed(
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
            None,
        ),
        DeliveryState::NotSent | DeliveryState::RejectedBeforeExecution => {
            detailed(kind, delivery, None)
        }
    }
}

fn unexpected_tool(delivery: DeliveryState) -> ModelError {
    detailed(ModelErrorKind::UnexpectedToolCall, delivery, None)
}

fn classify_http_error(
    status: u16,
    envelope: Option<&Value>,
    retry_after: Option<std::time::Duration>,
) -> ModelError {
    let error = envelope.and_then(|value| value.get("error"));
    let error_type = error
        .and_then(Value::as_object)
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str);
    let error_code = error
        .and_then(Value::as_object)
        .and_then(|object| object.get("code"))
        .and_then(Value::as_str);
    match status {
        400 => detailed(
            if matches!(
                error_code,
                Some("context_length_exceeded")
                    | Some("context_window_exceeded")
                    | Some("prompt_too_long")
            ) {
                ModelErrorKind::ContextOverflow
            } else {
                ModelErrorKind::InvalidRequest
            },
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        401 => detailed(
            ModelErrorKind::AuthRejected,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        402 => detailed(
            ModelErrorKind::QuotaExceeded,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        413 => detailed(
            ModelErrorKind::InvalidRequest,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        429 => detailed(
            ModelErrorKind::RateLimited,
            DeliveryState::RejectedBeforeExecution,
            retry_after,
        ),
        529 if error_type == Some("overloaded_error") => detailed(
            ModelErrorKind::ProviderUnavailable,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        _ => detailed(
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
            None,
        ),
    }
}

fn classify_stream_error(value: &Value, delivery: DeliveryState) -> Result<ModelError, ModelError> {
    let error_type = value
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str);
    let kind = match error_type {
        None | Some("") => return Err(invalid_provider_response(delivery)),
        Some("rate_limit_error") => ModelErrorKind::RateLimited,
        Some("authentication_error") => ModelErrorKind::AuthRejected,
        Some("billing_error") => ModelErrorKind::QuotaExceeded,
        Some("invalid_request_error") | Some("request_too_large") => ModelErrorKind::InvalidRequest,
        Some("timeout_error") => ModelErrorKind::Timeout,
        Some("api_error") | Some("overloaded_error") | Some(_) => {
            ModelErrorKind::ProviderUnavailable
        }
    };
    Ok(stream_failure(kind, delivery))
}

fn mark_semantic(delivery: &mut DeliveryState) {
    if *delivery == DeliveryState::AcceptedNoOutput {
        *delivery = DeliveryState::OutputStarted;
    }
}

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

fn encode_request(
    request: &ModelRequest,
    descriptor: &ModelDescriptor,
) -> Result<Vec<u8>, ModelError> {
    let max_tokens = request
        .limits()
        .max_output_tokens()
        .or_else(|| descriptor.limits().max_output_tokens())
        .ok_or_else(invalid_request_not_sent)?;
    let mut body = Map::new();
    body.insert("model".into(), json!(descriptor.api_model_name()));
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert("stream".into(), json!(true));

    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in request.messages() {
        match message {
            ModelMessage::System(text) => system.push(json!({"type": "text", "text": text})),
            _ => encode_message(message, &mut messages),
        }
    }
    if messages.is_empty() {
        return Err(invalid_request_not_sent());
    }
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools().is_empty() {
        body.insert("tools".into(), Value::Array(encode_tools(request.tools())));
        body.insert("tool_choice".into(), json!({"type": "auto"}));
    }
    let (thinking, effort) = reasoning_wire(request.reasoning());
    if let Some(thinking) = thinking {
        body.insert("thinking".into(), thinking);
    }
    if let Some(effort) = effort {
        body.insert("output_config".into(), json!({"effort": effort}));
    }
    body.insert("service_tier".into(), json!("standard_only"));

    let bytes = serde_json::to_vec(&Value::Object(body)).map_err(|_| invalid_request_not_sent())?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid_request_not_sent());
    }
    Ok(bytes)
}

fn encode_message(message: &ModelMessage, messages: &mut Vec<Value>) {
    match message {
        ModelMessage::User(text) => messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": text}],
        })),
        ModelMessage::Assistant(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part {
                    AssistantPart::Text(text) => blocks.push(json!({"type": "text", "text": text})),
                    AssistantPart::Reasoning(reasoning) => {
                        if let Some(block) = encode_reasoning_block(reasoning) {
                            blocks.push(block);
                        }
                    }
                    AssistantPart::ToolCall(call) => blocks.push(json!({
                        "type": "tool_use",
                        "id": call.tool_call_id().to_string(),
                        "name": call.name().as_str(),
                        "input": call.arguments(),
                    })),
                }
            }
            if !blocks.is_empty() {
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
        }
        ModelMessage::Tool {
            tool_call_id,
            output,
        } => messages.push(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tool_call_id.to_string(),
                "content": [{"type": "text", "text": output.text()}],
                "is_error": output.is_error(),
            }],
        })),
        ModelMessage::System(_) => {}
    }
}

fn encode_reasoning_block(reasoning: &ReasoningContent) -> Option<Value> {
    if let (Some(text), Some(signature)) = (reasoning.text(), reasoning.signature()) {
        return Some(json!({
            "type": "thinking",
            "thinking": text,
            "signature": signature,
        }));
    }
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
            return Some(json!({"type": "redacted_thinking", "data": encrypted}));
        }
    }
    None
}

fn encode_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name().as_str(),
                "description": tool.description(),
                "input_schema": tool.input_schema(),
            })
        })
        .collect()
}

fn reasoning_wire(
    reasoning: super::super::types::ReasoningPreference,
) -> (Option<Value>, Option<&'static str>) {
    match reasoning {
        super::super::types::ReasoningPreference::Auto => (None, None),
        super::super::types::ReasoningPreference::Disabled => {
            (Some(json!({"type": "disabled"})), None)
        }
        super::super::types::ReasoningPreference::Low => {
            (Some(json!({"type": "adaptive"})), Some("low"))
        }
        super::super::types::ReasoningPreference::Medium => {
            (Some(json!({"type": "adaptive"})), Some("medium"))
        }
        super::super::types::ReasoningPreference::High => {
            (Some(json!({"type": "adaptive"})), Some("high"))
        }
    }
}

// ---------------------------------------------------------------------------
// SSE state machine
// ---------------------------------------------------------------------------

enum Dispatch {
    Continue,
    Success(ModelResponse),
    Failure(ModelError),
}

struct DispatchInput<'a> {
    ctx: &'a ModelCallContext,
    delivery: &'a mut DeliveryState,
    state: &'a mut AnthropicStreamState,
    expected_model: &'a str,
    allowed_tools: &'a [ToolSpec],
}

#[derive(Default)]
struct AnthropicStreamState {
    start_seen: bool,
    open: Option<OpenBlock>,
    next_provider_index: u64,
    content: Vec<NormalizedContent>,
    usage: UsageState,
}

enum NormalizedContent {
    Text(String),
    Reasoning(ReasoningContent),
    Tool {
        id: ToolCallId,
        name: ToolName,
        arguments: Value,
    },
}

struct OpenBlock {
    index: u64,
    kind: OpenBlockKind,
}

enum OpenBlockKind {
    Thinking {
        text: String,
        signature: Option<String>,
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
        input: String,
        has_deltas: bool,
    },
}

#[derive(Clone, Copy)]
enum StartError {
    Invalid,
    UnexpectedTool,
}

fn dispatch(data: &str, input: DispatchInput<'_>) -> Result<Dispatch, ModelError> {
    let DispatchInput {
        ctx,
        delivery,
        state,
        expected_model,
        allowed_tools,
    } = input;
    let value: Value =
        serde_json::from_str(data).map_err(|_| invalid_provider_response(*delivery))?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_provider_response(*delivery))?;
    match event_type {
        "message_start" => {
            handle_message_start(&value, state, expected_model)
                .map_err(|_| invalid_provider_response(*delivery))?;
        }
        "content_block_start" => {
            let semantic = value
                .get("content_block")
                .and_then(Value::as_object)
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "tool_use" | "redacted_thinking"));
            let result = handle_content_block_start(&value, state, allowed_tools);
            match result {
                Ok(initial_semantic) => {
                    if semantic || initial_semantic {
                        mark_semantic(delivery);
                    }
                }
                Err(StartError::UnexpectedTool) => return Err(unexpected_tool(*delivery)),
                Err(StartError::Invalid) => {
                    return Err(invalid_provider_response(*delivery));
                }
            }
        }
        "content_block_delta" => {
            handle_content_block_delta(&value, ctx, delivery, state)
                .map_err(|_| invalid_provider_response(*delivery))?;
        }
        "content_block_stop" => {
            handle_content_block_stop(&value, state)
                .map_err(|_| invalid_provider_response(*delivery))?;
        }
        "message_delta" => {
            return Ok(Dispatch::Success(handle_message_delta(
                &value, *delivery, state,
            )?));
        }
        "message_stop" => {
            if !state.start_seen {
                return Err(invalid_provider_response(*delivery));
            }
        }
        "error" => return Ok(Dispatch::Failure(classify_stream_error(&value, *delivery)?)),
        "ping" => {}
        _ => {}
    }
    Ok(Dispatch::Continue)
}

fn handle_message_start(
    value: &Value,
    state: &mut AnthropicStreamState,
    expected_model: &str,
) -> Result<(), ()> {
    if state.start_seen {
        return Err(());
    }
    let message = value.get("message").and_then(Value::as_object).ok_or(())?;
    if message.get("type").and_then(Value::as_str) != Some("message")
        || message.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return Err(());
    }
    if message.get("model").and_then(Value::as_str) != Some(expected_model) {
        return Err(());
    }
    message
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse::<ProviderItemId>().ok())
        .ok_or(())?;
    if !message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        return Err(());
    }
    if message
        .get("stop_reason")
        .is_some_and(|value| !value.is_null())
        || message
            .get("stop_sequence")
            .is_some_and(|value| !value.is_null())
    {
        return Err(());
    }
    state.usage.apply_start(message.get("usage"))?;
    state.start_seen = true;
    Ok(())
}

fn handle_content_block_start(
    value: &Value,
    state: &mut AnthropicStreamState,
    allowed_tools: &[ToolSpec],
) -> Result<bool, StartError> {
    if !state.start_seen || state.open.is_some() {
        return Err(StartError::Invalid);
    }
    let index = value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or(StartError::Invalid)?;
    if index != state.next_provider_index {
        return Err(StartError::Invalid);
    }
    let block = value
        .get("content_block")
        .and_then(Value::as_object)
        .ok_or(StartError::Invalid)?;
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or(StartError::Invalid)?;
    let (kind, semantic) = match block_type {
        "thinking" => {
            let text = block
                .get("thinking")
                .and_then(Value::as_str)
                .ok_or(StartError::Invalid)?
                .to_owned();
            if text.len() > MAX_TEXT_BYTES {
                return Err(StartError::Invalid);
            }
            let signature = match block.get("signature") {
                None => None,
                Some(value) => Some(value.as_str().ok_or(StartError::Invalid)?.to_owned()),
            };
            if signature
                .as_ref()
                .is_some_and(|value| value.len() > MAX_SIGNATURE_BYTES)
            {
                return Err(StartError::Invalid);
            }
            (
                OpenBlockKind::Thinking { text, signature },
                !block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty),
            )
        }
        "redacted_thinking" => {
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .filter(|data| !data.is_empty())
                .ok_or(StartError::Invalid)?
                .to_owned();
            if data.len() > MAX_TEXT_BYTES {
                return Err(StartError::Invalid);
            }
            (OpenBlockKind::RedactedThinking { data }, true)
        }
        "text" => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .ok_or(StartError::Invalid)?
                .to_owned();
            if text.len() > MAX_TEXT_BYTES {
                return Err(StartError::Invalid);
            }
            (OpenBlockKind::Text { text: text.clone() }, !text.is_empty())
        }
        "tool_use" => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<ToolCallId>().ok())
                .ok_or(StartError::Invalid)?;
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<ToolName>().ok())
                .ok_or(StartError::Invalid)?;
            if !allowed_tools.iter().any(|tool| tool.name() == &name) {
                return Err(StartError::UnexpectedTool);
            }
            let input = block
                .get("input")
                .filter(|value| value.is_object())
                .ok_or(StartError::Invalid)?;
            let input = serde_json::to_string(input).map_err(|_| StartError::Invalid)?;
            if input.len() > MAX_JSON_BYTES {
                return Err(StartError::Invalid);
            }
            (
                OpenBlockKind::ToolUse {
                    id,
                    name,
                    input,
                    has_deltas: false,
                },
                true,
            )
        }
        _ => return Err(StartError::Invalid),
    };
    state.next_provider_index = index.checked_add(1).ok_or(StartError::Invalid)?;
    state.open = Some(OpenBlock { index, kind });
    Ok(semantic)
}

fn handle_content_block_delta(
    value: &Value,
    ctx: &ModelCallContext,
    delivery: &mut DeliveryState,
    state: &mut AnthropicStreamState,
) -> Result<(), ()> {
    let index = value.get("index").and_then(Value::as_u64).ok_or(())?;
    let open = state.open.as_mut().ok_or(())?;
    if open.index != index {
        return Err(());
    }
    let delta = value.get("delta").and_then(Value::as_object).ok_or(())?;
    let delta_type = delta.get("type").and_then(Value::as_str).ok_or(())?;
    match (delta_type, &mut open.kind) {
        ("thinking_delta", OpenBlockKind::Thinking { text, .. }) => {
            let piece = delta.get("thinking").and_then(Value::as_str).ok_or(())?;
            append_bounded(text, piece, MAX_TEXT_BYTES)?;
            if !piece.is_empty() {
                mark_semantic(delivery);
                ctx.publish(ModelEvent::ReasoningDelta {
                    delta: piece.to_owned(),
                });
            }
        }
        ("signature_delta", OpenBlockKind::Thinking { signature, .. }) => {
            let piece = delta.get("signature").and_then(Value::as_str).ok_or(())?;
            let signature = signature.get_or_insert_with(String::new);
            append_bounded(signature, piece, MAX_SIGNATURE_BYTES)?;
            if !piece.is_empty() {
                mark_semantic(delivery);
            }
        }
        ("text_delta", OpenBlockKind::Text { text }) => {
            let piece = delta.get("text").and_then(Value::as_str).ok_or(())?;
            append_bounded(text, piece, MAX_TEXT_BYTES)?;
            if !piece.is_empty() {
                mark_semantic(delivery);
                ctx.publish(ModelEvent::TextDelta {
                    delta: piece.to_owned(),
                });
            }
        }
        (
            "input_json_delta",
            OpenBlockKind::ToolUse {
                input, has_deltas, ..
            },
        ) => {
            let piece = delta
                .get("partial_json")
                .and_then(Value::as_str)
                .ok_or(())?;
            if !piece.is_empty() {
                mark_semantic(delivery);
                if !*has_deltas {
                    input.clear();
                    *has_deltas = true;
                }
                append_bounded(input, piece, MAX_JSON_BYTES)?;
            }
        }
        _ => return Err(()),
    }
    Ok(())
}

fn append_bounded(target: &mut String, piece: &str, maximum: usize) -> Result<(), ()> {
    if piece.len() > maximum.saturating_sub(target.len()) {
        return Err(());
    }
    target.push_str(piece);
    Ok(())
}

fn handle_content_block_stop(value: &Value, state: &mut AnthropicStreamState) -> Result<(), ()> {
    let index = value.get("index").and_then(Value::as_u64).ok_or(())?;
    let open = state.open.take().ok_or(())?;
    if open.index != index {
        return Err(());
    }
    let content = match open.kind {
        OpenBlockKind::Thinking { text, signature } => {
            let signature = signature.filter(|value| !value.is_empty());
            if text.is_empty() && signature.is_none() {
                return Ok(());
            }
            NormalizedContent::Reasoning(
                ReasoningContent::new(
                    (!text.is_empty()).then_some(text),
                    None,
                    None,
                    signature,
                    None,
                )
                .map_err(|_| ())?,
            )
        }
        OpenBlockKind::RedactedThinking { data } => NormalizedContent::Reasoning(
            ReasoningContent::new(None, None, Some(data), None, None).map_err(|_| ())?,
        ),
        OpenBlockKind::Text { text } => {
            if text.is_empty() {
                return Ok(());
            }
            NormalizedContent::Text(text)
        }
        OpenBlockKind::ToolUse {
            id,
            name,
            input,
            has_deltas: _,
        } => {
            let arguments: Value = serde_json::from_str(&input).map_err(|_| ())?;
            if !arguments.is_object() {
                return Err(());
            }
            NormalizedContent::Tool {
                id,
                name,
                arguments,
            }
        }
    };
    state.content.push(content);
    Ok(())
}

fn handle_message_delta(
    value: &Value,
    delivery: DeliveryState,
    state: &mut AnthropicStreamState,
) -> Result<ModelResponse, ModelError> {
    if !state.start_seen || state.open.is_some() {
        return Err(invalid_provider_response(delivery));
    }
    let delta = value
        .get("delta")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let stop_reason = delta
        .get("stop_reason")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    match delta.get("stop_sequence") {
        None | Some(Value::Null) | Some(Value::String(_)) => {}
        _ => return Err(invalid_provider_response(delivery)),
    }
    state
        .usage
        .apply_terminal(value.get("usage"))
        .map_err(|_| invalid_provider_response(delivery))?;

    let has_visible = state
        .content
        .iter()
        .any(|content| matches!(content, NormalizedContent::Text(text) if !text.is_empty()));
    let has_tool = state
        .content
        .iter()
        .any(|content| matches!(content, NormalizedContent::Tool { .. }));
    let finish_reason = map_finish_reason(stop_reason);
    match finish_reason {
        ModelFinishReason::ToolCalls => {
            if !has_tool {
                return Err(invalid_provider_response(delivery));
            }
        }
        ModelFinishReason::Refused => {
            if !has_visible || has_tool {
                return Err(invalid_provider_response(delivery));
            }
        }
        ModelFinishReason::Length | ModelFinishReason::ContentFiltered => {
            return Err(detailed(ModelErrorKind::IncompleteResponse, delivery, None));
        }
        ModelFinishReason::Stop => {
            if has_tool {
                return Err(invalid_provider_response(delivery));
            }
            if !has_visible {
                return Err(detailed(ModelErrorKind::IncompleteResponse, delivery, None));
            }
        }
        ModelFinishReason::Unknown => {
            if !has_tool && !has_visible {
                return Err(detailed(ModelErrorKind::IncompleteResponse, delivery, None));
            }
        }
    }

    let mut parts = Vec::new();
    let mut call_index = 0_u32;
    for content in std::mem::take(&mut state.content) {
        match content {
            NormalizedContent::Text(text) => parts.push(AssistantPart::Text(text)),
            NormalizedContent::Reasoning(reasoning) => {
                parts.push(AssistantPart::Reasoning(reasoning))
            }
            NormalizedContent::Tool {
                id,
                name,
                arguments,
            } => {
                let call = ToolCall::new(id, name, arguments, call_index)
                    .map_err(|_| invalid_provider_response(delivery))?;
                call_index = call_index
                    .checked_add(1)
                    .ok_or_else(|| invalid_provider_response(delivery))?;
                parts.push(AssistantPart::ToolCall(call));
            }
        }
    }
    let usage = state.usage.finish();
    ModelResponse::new(parts, finish_reason, usage).map_err(|_| invalid_provider_response(delivery))
}

fn map_finish_reason(stop_reason: &str) -> ModelFinishReason {
    match stop_reason {
        "end_turn" | "stop_sequence" => ModelFinishReason::Stop,
        "tool_use" => ModelFinishReason::ToolCalls,
        "max_tokens" | "model_context_window_exceeded" | "pause_turn" => ModelFinishReason::Length,
        "content_filter" | "content_filtered" => ModelFinishReason::ContentFiltered,
        "refusal" => ModelFinishReason::Refused,
        _ => ModelFinishReason::Unknown,
    }
}

#[derive(Default)]
struct UsageState {
    seen: bool,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

impl UsageState {
    fn apply_start(&mut self, value: Option<&Value>) -> Result<(), ()> {
        let object = Self::required_object(value)?;
        let input = Self::required_u64(object, "input_tokens")?;
        let output = Self::required_u64(object, "output_tokens")?;
        self.seen = true;
        Self::merge_value(&mut self.input_tokens, input)?;
        Self::merge_value(&mut self.output_tokens, output)?;
        self.apply_optional(object)
    }

    fn apply_terminal(&mut self, value: Option<&Value>) -> Result<(), ()> {
        let object = Self::required_object(value)?;
        let output = Self::required_u64(object, "output_tokens")?;
        self.seen = true;
        Self::merge_value(&mut self.output_tokens, output)?;
        self.apply_optional(object)
    }

    fn required_object(value: Option<&Value>) -> Result<&Map<String, Value>, ()> {
        let value = value.ok_or(())?;
        if value.is_null() {
            return Err(());
        }
        value.as_object().ok_or(())
    }

    fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64, ()> {
        object.get(key).and_then(Value::as_u64).ok_or(())
    }

    fn apply_optional(&mut self, object: &Map<String, Value>) -> Result<(), ()> {
        Self::merge_field(object, "input_tokens", &mut self.input_tokens)?;
        Self::merge_field(
            object,
            "cache_read_input_tokens",
            &mut self.cache_read_tokens,
        )?;
        Self::merge_field(
            object,
            "cache_creation_input_tokens",
            &mut self.cache_write_tokens,
        )?;
        match object.get("output_tokens_details") {
            None | Some(Value::Null) => {}
            Some(details) => {
                let details = details.as_object().ok_or(())?;
                Self::merge_field(details, "thinking_tokens", &mut self.reasoning_tokens)?;
            }
        }
        Ok(())
    }

    fn merge_field(
        object: &Map<String, Value>,
        key: &str,
        target: &mut Option<u64>,
    ) -> Result<(), ()> {
        let Some(value) = object.get(key) else {
            return Ok(());
        };
        let Some(value) = value.as_u64() else {
            if value.is_null() {
                return Ok(());
            }
            return Err(());
        };
        Self::merge_value(target, value)
    }

    fn merge_value(target: &mut Option<u64>, value: u64) -> Result<(), ()> {
        if target.is_some_and(|current| value < current) {
            return Err(());
        }
        *target = Some(value);
        Ok(())
    }

    fn finish(&self) -> Option<Usage> {
        self.seen.then(|| {
            Usage::from_optional(self.input_tokens, self.output_tokens, self.reasoning_tokens)
                .with_cache_read_tokens(self.cache_read_tokens)
                .with_cache_write_tokens(self.cache_write_tokens)
        })
    }
}
