//! Model-owned OpenAI Responses provider implementation.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use super::super::provider::{
    CredentialSource, ModelCallContext, ModelFuture, ModelProvider, OpenAiReasoningProgress,
};
use super::super::transport::{
    MAX_RESPONSE_BYTES, SseParser, cancelled, classify_send_error, invalid_provider_response,
    invalid_request_not_sent, is_event_stream, parse_retry_after, read_bounded_envelope,
    transport_read_error,
};
use super::super::types::{
    AssistantPart, DeliveryState, ModelDescriptor, ModelError, ModelErrorKind, ModelEvent,
    ModelFinishReason, ModelMessage, ModelRequest, ModelResponse, ProviderItemId, ReasoningContent,
    ReasoningPreference, ToolCall, ToolSpec, Usage,
};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Map, Value, json};
use thiserror::Error;

const MAX_REQUEST_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum OpenAiProviderError {
    #[error("OpenAI endpoint violates the explicit endpoint policy")]
    InvalidEndpoint,
    #[error("OpenAI provider requires at least one model descriptor")]
    EmptyModels,
    #[error("OpenAI provider contains duplicate model selections")]
    DuplicateModel,
    #[error("all OpenAI model descriptors must use the same provider id")]
    ProviderMismatch,
    #[error("OpenAI HTTP client construction failed")]
    ClientBuild,
}

pub struct OpenAiResponsesProvider {
    id: super::super::types::ProviderId,
    models: Arc<[ModelDescriptor]>,
    client: reqwest::Client,
    endpoint: reqwest::Url,
    credential_source: Arc<dyn CredentialSource>,
    reasoning_progress: OpenAiReasoningProgress,
}

impl OpenAiResponsesProvider {
    pub fn new(
        endpoint: &str,
        endpoint_policy: super::super::provider::ProviderEndpointPolicy,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, OpenAiProviderError> {
        Self::new_with_reasoning_progress(
            endpoint,
            endpoint_policy,
            OpenAiReasoningProgress::SummaryOnly,
            credential_source,
            models,
        )
    }

    pub fn new_with_reasoning_progress(
        endpoint: &str,
        endpoint_policy: super::super::provider::ProviderEndpointPolicy,
        reasoning_progress: OpenAiReasoningProgress,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, OpenAiProviderError> {
        if models.is_empty() {
            return Err(OpenAiProviderError::EmptyModels);
        }
        let id = models[0].selection().provider_id().clone();
        let mut selections = BTreeSet::new();
        for model in &models {
            if model.selection().provider_id() != &id {
                return Err(OpenAiProviderError::ProviderMismatch);
            }
            if !selections.insert(model.selection().clone()) {
                return Err(OpenAiProviderError::DuplicateModel);
            }
        }
        let endpoint = validate_endpoint(endpoint, endpoint_policy)?;
        let client = super::super::transport::client_builder()
            .build()
            .map_err(|_| OpenAiProviderError::ClientBuild)?;
        Ok(Self {
            id,
            models: models.into(),
            client,
            endpoint,
            credential_source,
            reasoning_progress,
        })
    }

    pub fn new_https(
        endpoint: &str,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, OpenAiProviderError> {
        Self::new(
            endpoint,
            super::super::provider::ProviderEndpointPolicy::HttpsOnly,
            credential_source,
            models,
        )
    }

    pub fn new_loopback_http(
        endpoint: &str,
        credential_source: Arc<dyn CredentialSource>,
        models: Vec<ModelDescriptor>,
    ) -> Result<Self, OpenAiProviderError> {
        Self::new(
            endpoint,
            super::super::provider::ProviderEndpointPolicy::AllowLoopbackHttp,
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
        let http_request = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(credential.header())
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

        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        if !status.is_success() {
            let delivery = if matches!(status.as_u16(), 400 | 401 | 429) {
                DeliveryState::RejectedBeforeExecution
            } else {
                DeliveryState::Unknown
            };
            let envelope = read_bounded_envelope(response, ctx.cancellation(), delivery).await?;
            return Err(classify_http_error(
                status.as_u16(),
                envelope.as_ref(),
                retry_after,
            ));
        }
        if !is_event_stream(response.headers().get(CONTENT_TYPE)) {
            return Err(invalid_provider_response(DeliveryState::AcceptedNoOutput));
        }

        let mut delivery = DeliveryState::AcceptedNoOutput;
        let mut parser = SseParser::new(MAX_RESPONSE_BYTES);
        let mut streamed_text_indexes = Vec::new();
        let mut streamed_reasoning_indexes = Vec::new();
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
                    let events = parser.feed(&chunk)
                        .map_err(|_| invalid_provider_response(delivery))?;
                    for event in events {
                        match dispatch(
                            &event.data,
                            DispatchInput {
                                ctx: &ctx,
                                delivery: &mut delivery,
                                streamed_text_indexes: &mut streamed_text_indexes,
                                streamed_reasoning_indexes: &mut streamed_reasoning_indexes,
                                allowed_tools: request.tools(),
                            },
                            self.reasoning_progress,
                            expected_model,
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

impl fmt::Debug for OpenAiResponsesProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesProvider")
            .field("provider_id", &self.id)
            .field("model_count", &self.models.len())
            .field("endpoint", &"<redacted>")
            .field("reasoning_progress", &self.reasoning_progress)
            .finish()
    }
}

impl ModelProvider for OpenAiResponsesProvider {
    fn id(&self) -> &super::super::types::ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        &self.models
    }

    fn generate(&self, request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'_> {
        Box::pin(self.run(request, ctx))
    }
}

fn validate_endpoint(
    endpoint: &str,
    policy: super::super::provider::ProviderEndpointPolicy,
) -> Result<reqwest::Url, OpenAiProviderError> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| OpenAiProviderError::InvalidEndpoint)?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(OpenAiProviderError::InvalidEndpoint);
    }
    match url.scheme() {
        "https" => Ok(url),
        "http"
            if policy == super::super::provider::ProviderEndpointPolicy::AllowLoopbackHttp
                && url.host_str().is_some_and(is_numeric_loopback) =>
        {
            Ok(url)
        }
        _ => Err(OpenAiProviderError::InvalidEndpoint),
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

fn encode_request(
    request: &ModelRequest,
    descriptor: &ModelDescriptor,
) -> Result<Vec<u8>, ModelError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(descriptor.api_model_name()));
    if let Some(max_output_tokens) = request
        .limits()
        .max_output_tokens()
        .or_else(|| descriptor.limits().max_output_tokens())
    {
        body.insert("max_output_tokens".into(), json!(max_output_tokens));
    }
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));

    let instructions = request
        .messages()
        .iter()
        .filter_map(|message| match message {
            ModelMessage::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    let input = encode_input(request.messages());
    if input.is_empty() {
        return Err(invalid_request_not_sent());
    }
    body.insert("input".into(), Value::Array(input));
    if !request.tools().is_empty() {
        body.insert("tools".into(), Value::Array(encode_tools(request.tools())));
        body.insert("tool_choice".into(), json!("auto"));
    }
    if let Some(reasoning) = reasoning_parameters(request.reasoning()) {
        body.insert("reasoning".into(), reasoning);
    }

    let bytes = serde_json::to_vec(&Value::Object(body)).map_err(|_| invalid_request_not_sent())?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid_request_not_sent());
    }
    Ok(bytes)
}

fn reasoning_parameters(reasoning: ReasoningPreference) -> Option<Value> {
    match reasoning {
        ReasoningPreference::Auto => None,
        ReasoningPreference::Disabled => Some(json!({"effort": "none"})),
        ReasoningPreference::Low => Some(json!({"effort": "low", "summary": "auto"})),
        ReasoningPreference::Medium => Some(json!({"effort": "medium", "summary": "auto"})),
        ReasoningPreference::High => Some(json!({"effort": "high", "summary": "auto"})),
    }
}

fn encode_input(messages: &[ModelMessage]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        match message {
            ModelMessage::System(_) => {}
            ModelMessage::User(text) => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            })),
            ModelMessage::Assistant(parts) => {
                for part in parts {
                    match part {
                        AssistantPart::Text(text) => input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                        })),
                        AssistantPart::Reasoning(reasoning) => {
                            let Some(provider_item_id) = reasoning.provider_item_id() else {
                                continue;
                            };
                            let mut item = Map::new();
                            item.insert("type".into(), json!("reasoning"));
                            item.insert("id".into(), json!(provider_item_id.as_str()));
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
                            input.push(Value::Object(item));
                        }
                        AssistantPart::ToolCall(call) => input.push(json!({
                            "type": "function_call",
                            "call_id": call.tool_call_id().as_str(),
                            "name": call.name().as_str(),
                            "arguments": serde_json::to_string(call.arguments()).expect("JSON value serializes"),
                            "status": "completed",
                        })),
                    }
                }
            }
            ModelMessage::Tool {
                tool_call_id,
                output,
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": tool_call_id.to_string(),
                "output": output.text(),
                "status": "completed",
            })),
        }
    }
    input
}

fn encode_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name().as_str(),
                "description": tool.description(),
                "parameters": tool.input_schema(),
            })
        })
        .collect()
}

fn classify_http_error(
    status: u16,
    envelope: Option<&Value>,
    retry_after: Option<std::time::Duration>,
) -> ModelError {
    let (error_type, error_code) = match envelope.and_then(|value| value.get("error")) {
        Some(error) => (
            error.get("type").and_then(Value::as_str),
            error.get("code").and_then(Value::as_str),
        ),
        None => (None, None),
    };
    match status {
        400 => detailed(
            if classify_error_tuple(error_type, error_code) == Some(ModelErrorKind::ContextOverflow)
            {
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
        429 => {
            let quota =
                classify_error_tuple(error_type, error_code) == Some(ModelErrorKind::QuotaExceeded);
            detailed(
                if quota {
                    ModelErrorKind::QuotaExceeded
                } else {
                    ModelErrorKind::RateLimited
                },
                DeliveryState::RejectedBeforeExecution,
                if quota { None } else { retry_after },
            )
        }
        _ => detailed(
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
            None,
        ),
    }
}

fn classify_error_tuple(
    error_type: Option<&str>,
    error_code: Option<&str>,
) -> Option<ModelErrorKind> {
    if error_code == Some("context_length_exceeded") {
        Some(ModelErrorKind::ContextOverflow)
    } else if error_type == Some("insufficient_quota")
        || error_code == Some("insufficient_quota")
        || error_code == Some("credit_balance_exhausted")
    {
        Some(ModelErrorKind::QuotaExceeded)
    } else if error_type == Some("rate_limit_error") || error_code == Some("rate_limit_exceeded") {
        Some(ModelErrorKind::RateLimited)
    } else if error_type == Some("authentication_error")
        || error_code == Some("authentication_error")
        || error_code == Some("invalid_api_key")
    {
        Some(ModelErrorKind::AuthRejected)
    } else {
        None
    }
}

fn stream_failure(kind: ModelErrorKind, delivery: DeliveryState) -> ModelError {
    if let Ok(error) = ModelError::detailed(kind, delivery, None) {
        return error;
    }
    match delivery {
        DeliveryState::OutputStarted => ModelError::detailed(
            ModelErrorKind::StreamInterrupted,
            DeliveryState::OutputStarted,
            None,
        )
        .unwrap_or(ModelError::StreamInterrupted),
        DeliveryState::AcceptedNoOutput | DeliveryState::Unknown => ModelError::detailed(
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
            None,
        )
        .unwrap_or(ModelError::RequestOutcomeUnknown),
        DeliveryState::NotSent | DeliveryState::RejectedBeforeExecution => {
            ModelError::detailed(kind, delivery, None).unwrap_or(ModelError::RequestOutcomeUnknown)
        }
    }
}

fn unexpected_tool_call(delivery: DeliveryState) -> ModelError {
    ModelError::detailed(ModelErrorKind::UnexpectedToolCall, delivery, None)
        .unwrap_or(ModelError::UnexpectedToolCall)
}

enum Dispatch {
    Continue,
    Success(ModelResponse),
    Failure(ModelError),
}

struct DispatchInput<'a> {
    ctx: &'a ModelCallContext,
    delivery: &'a mut DeliveryState,
    streamed_text_indexes: &'a mut Vec<u32>,
    streamed_reasoning_indexes: &'a mut Vec<u32>,
    allowed_tools: &'a [ToolSpec],
}

fn dispatch(
    data: &str,
    input: DispatchInput<'_>,
    reasoning_progress: OpenAiReasoningProgress,
    expected_model: &str,
) -> Result<Dispatch, ModelError> {
    let DispatchInput {
        ctx,
        delivery,
        streamed_text_indexes,
        streamed_reasoning_indexes,
        allowed_tools,
    } = input;
    let value: Value =
        serde_json::from_str(data).map_err(|_| invalid_provider_response(*delivery))?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_provider_response(*delivery))?;
    match event_type {
        "response.output_text.delta" => {
            let index = required_index(&value, "output_index", *delivery)?;
            let delta = required_string(&value, "delta", *delivery)?;
            if !delta.is_empty() {
                mark_semantic(delivery);
                streamed_text_indexes.push(index);
                ctx.publish(ModelEvent::TextDelta {
                    delta: delta.to_owned(),
                });
            }
            Ok(Dispatch::Continue)
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let index = required_index(&value, "output_index", *delivery)?;
            let delta = required_string(&value, "delta", *delivery)?;
            if !delta.is_empty() {
                mark_semantic(delivery);
                let selected = matches!(
                    (reasoning_progress, event_type),
                    (
                        OpenAiReasoningProgress::SummaryOnly,
                        "response.reasoning_summary_text.delta"
                    ) | (
                        OpenAiReasoningProgress::RawText,
                        "response.reasoning_text.delta"
                    )
                );
                if selected {
                    streamed_reasoning_indexes.push(index);
                    ctx.publish(ModelEvent::ReasoningDelta {
                        delta: delta.to_owned(),
                    });
                }
            }
            Ok(Dispatch::Continue)
        }
        "response.refusal.delta" | "response.function_call_arguments.delta" => {
            let _ = required_index(&value, "output_index", *delivery)?;
            let delta = required_string(&value, "delta", *delivery)?;
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
            let semantic = match item_type {
                "message" => item
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            (part.get("type").and_then(Value::as_str) == Some("output_text")
                                && part
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| !text.is_empty()))
                                || (part.get("type").and_then(Value::as_str) == Some("refusal")
                                    && part
                                        .get("refusal")
                                        .and_then(Value::as_str)
                                        .is_some_and(|text| !text.is_empty()))
                        })
                    }),
                "reasoning" => {
                    item.get("summary")
                        .and_then(Value::as_array)
                        .is_some_and(|entries| {
                            entries.iter().any(|entry| {
                                entry.get("type").and_then(Value::as_str) == Some("summary_text")
                                    && entry
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .is_some_and(|text| !text.is_empty())
                            })
                        })
                        || item
                            .get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|entries| {
                                entries.iter().any(|entry| {
                                    entry.get("type").and_then(Value::as_str)
                                        == Some("reasoning_text")
                                        && entry
                                            .get("text")
                                            .and_then(Value::as_str)
                                            .is_some_and(|text| !text.is_empty())
                                })
                            })
                        || item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                }
                "function_call" => true,
                _ => false,
            };
            if semantic {
                mark_semantic(delivery);
            }
            Ok(Dispatch::Continue)
        }
        "response.completed" => {
            let response = value
                .get("response")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            if response.get("model").and_then(Value::as_str) != Some(expected_model)
                || response.get("status").and_then(Value::as_str) != Some("completed")
            {
                return Err(invalid_provider_response(*delivery));
            }
            Ok(Dispatch::Success(normalize_terminal(
                response,
                *delivery,
                streamed_text_indexes,
                streamed_reasoning_indexes,
                reasoning_progress,
                allowed_tools,
            )?))
        }
        "response.incomplete" => {
            value
                .get("response")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            Ok(Dispatch::Failure(detailed(
                ModelErrorKind::IncompleteResponse,
                *delivery,
                None,
            )))
        }
        "response.failed" => {
            let response = value
                .get("response")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid_provider_response(*delivery))?;
            let (error_type, error_code) = response
                .get("error")
                .and_then(Value::as_object)
                .map(|error| {
                    (
                        error.get("type").and_then(Value::as_str),
                        error.get("code").and_then(Value::as_str),
                    )
                })
                .unwrap_or((None, None));
            Ok(Dispatch::Failure(stream_failure(
                classify_error_tuple(error_type, error_code)
                    .unwrap_or(ModelErrorKind::ProviderUnavailable),
                *delivery,
            )))
        }
        "error" => {
            let error_code = value.get("code").and_then(Value::as_str);
            Ok(Dispatch::Failure(stream_failure(
                classify_error_tuple(None, error_code)
                    .unwrap_or(ModelErrorKind::ProviderUnavailable),
                *delivery,
            )))
        }
        _ => Ok(Dispatch::Continue),
    }
}

fn required_index(value: &Value, key: &str, delivery: DeliveryState) -> Result<u32, ModelError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| invalid_provider_response(delivery))
}

fn required_string<'a>(
    value: &'a Value,
    key: &str,
    delivery: DeliveryState,
) -> Result<&'a str, ModelError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_provider_response(delivery))
}

fn mark_semantic(delivery: &mut DeliveryState) {
    if *delivery == DeliveryState::AcceptedNoOutput {
        *delivery = DeliveryState::OutputStarted;
    }
}

fn normalize_terminal(
    response: &Map<String, Value>,
    delivery: DeliveryState,
    streamed_text_indexes: &[u32],
    streamed_reasoning_indexes: &[u32],
    reasoning_progress: OpenAiReasoningProgress,
    allowed_tools: &[ToolSpec],
) -> Result<ModelResponse, ModelError> {
    response
        .get("id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<ProviderItemId>().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let mut parts = Vec::new();
    let mut text_indexes = Vec::new();
    let mut reasoning_indexes = Vec::new();
    let mut empty_tail = false;
    let mut has_refusal = false;
    let mut has_visible = false;
    let mut has_tool_call = false;
    let mut next_call_index = 0_u32;

    for (position, item) in output.iter().enumerate() {
        let before = parts.len();
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_provider_response(delivery))?;
        match item_type {
            "message" => normalize_message(
                item,
                delivery,
                &mut parts,
                &mut has_refusal,
                &mut has_visible,
            )?,
            "reasoning" => {
                if let Some(reasoning) = normalize_reasoning(item, delivery)? {
                    if (reasoning_progress == OpenAiReasoningProgress::SummaryOnly
                        && reasoning.summary().is_some_and(|value| !value.is_empty()))
                        || (reasoning_progress == OpenAiReasoningProgress::RawText
                            && reasoning.text().is_some_and(|value| !value.is_empty()))
                    {
                        reasoning_indexes.push((position, before));
                    }
                    parts.push(AssistantPart::Reasoning(reasoning));
                }
            }
            "function_call" => {
                let call = normalize_function_call(item, delivery, next_call_index, allowed_tools)?;
                next_call_index = next_call_index
                    .checked_add(1)
                    .ok_or_else(|| invalid_provider_response(delivery))?;
                parts.push(AssistantPart::ToolCall(call));
                has_tool_call = true;
            }
            _ => return Err(invalid_provider_response(delivery)),
        }
        if item_type == "message" && parts.len() > before && message_has_text(item) {
            text_indexes.push((position, before));
        }
        if parts.len() == before {
            empty_tail = true;
        } else if empty_tail {
            return Err(invalid_provider_response(delivery));
        }
    }
    for index in streamed_text_indexes {
        let index = usize::try_from(*index).expect("u32 fits usize");
        if !text_indexes
            .iter()
            .any(|(position, content_index)| *position == index && *content_index == index)
        {
            return Err(invalid_provider_response(delivery));
        }
    }
    for index in streamed_reasoning_indexes {
        let index = usize::try_from(*index).expect("u32 fits usize");
        if !reasoning_indexes
            .iter()
            .any(|(position, content_index)| *position == index && *content_index == index)
        {
            return Err(invalid_provider_response(delivery));
        }
    }
    if has_refusal && (has_visible || has_tool_call) {
        return Err(invalid_provider_response(delivery));
    }
    if !has_visible && !has_refusal && !has_tool_call {
        return Err(detailed(ModelErrorKind::IncompleteResponse, delivery, None));
    }
    let finish = if has_tool_call {
        ModelFinishReason::ToolCalls
    } else if has_refusal {
        ModelFinishReason::Refused
    } else {
        ModelFinishReason::Stop
    };
    let usage = normalize_usage(response.get("usage"), delivery)?;
    ModelResponse::new(parts, finish, usage).map_err(|_| invalid_provider_response(delivery))
}

fn normalize_message(
    item: &Value,
    delivery: DeliveryState,
    parts: &mut Vec<AssistantPart>,
    has_refusal: &mut bool,
    has_visible: &mut bool,
) -> Result<(), ModelError> {
    if item.get("role").and_then(Value::as_str) != Some("assistant")
        || item.get("status").and_then(Value::as_str) != Some("completed")
    {
        return Err(invalid_provider_response(delivery));
    }
    item.get("id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<ProviderItemId>().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let mut texts = Vec::new();
    let mut refusals = Vec::new();
    for part in content {
        match part.get("type").and_then(Value::as_str) {
            Some("output_text") => {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_provider_response(delivery))?;
                if !text.is_empty() {
                    texts.push(text);
                }
            }
            Some("refusal") => {
                let refusal = part
                    .get("refusal")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_provider_response(delivery))?;
                if !refusal.is_empty() {
                    refusals.push(refusal);
                }
            }
            _ => return Err(invalid_provider_response(delivery)),
        }
    }
    if !texts.is_empty() && !refusals.is_empty() {
        return Err(invalid_provider_response(delivery));
    }
    if !texts.is_empty() {
        *has_visible = true;
        parts.push(AssistantPart::Text(texts.concat()));
    }
    if !refusals.is_empty() {
        *has_refusal = true;
        parts.push(AssistantPart::Text(refusals.join("\n")));
    }
    Ok(())
}

fn message_has_text(item: &Value) -> bool {
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

fn normalize_reasoning(
    item: &Value,
    delivery: DeliveryState,
) -> Result<Option<ReasoningContent>, ModelError> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<ProviderItemId>().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    if let Some(status) = item.get("status") {
        if !status.is_null() && status.as_str() != Some("completed") {
            return Err(invalid_provider_response(delivery));
        }
    }
    let summaries = parse_reasoning_parts(item.get("summary"), "summary_text", delivery)?;
    let texts = parse_reasoning_parts(item.get("content"), "reasoning_text", delivery)?;
    let encrypted = match item.get("encrypted_content") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| invalid_provider_response(delivery))?
                .to_owned(),
        ),
    };
    if summaries.is_empty() && texts.is_empty() && encrypted.is_none() {
        return Ok(None);
    }
    ReasoningContent::new(
        (!texts.is_empty()).then(|| texts.join("\n")),
        (!summaries.is_empty()).then(|| summaries.join("\n")),
        encrypted,
        None,
        Some(id),
    )
    .map(Some)
    .map_err(|_| invalid_provider_response(delivery))
}

fn parse_reasoning_parts(
    value: Option<&Value>,
    expected_type: &str,
    delivery: DeliveryState,
) -> Result<Vec<String>, ModelError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| invalid_provider_response(delivery))?;
    entries
        .iter()
        .map(|entry| {
            if entry.get("type").and_then(Value::as_str) != Some(expected_type) {
                return Err(invalid_provider_response(delivery));
            }
            entry
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| invalid_provider_response(delivery))
        })
        .collect()
}

fn normalize_function_call(
    item: &Value,
    delivery: DeliveryState,
    call_index: u32,
    allowed_tools: &[ToolSpec],
) -> Result<ToolCall, ModelError> {
    if let Some(id) = item.get("id") {
        if !id.is_null()
            && id
                .as_str()
                .and_then(|value| value.parse::<ProviderItemId>().ok())
                .is_none()
        {
            return Err(invalid_provider_response(delivery));
        }
    }
    if let Some(status) = item.get("status") {
        if !status.is_null() && status.as_str() != Some("completed") {
            return Err(invalid_provider_response(delivery));
        }
    }
    let tool_call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    if !allowed_tools.iter().any(|tool| tool.name() == &name) {
        return Err(unexpected_tool_call(delivery));
    }
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .ok_or_else(|| invalid_provider_response(delivery))?;
    ToolCall::new(tool_call_id, name, arguments, call_index)
        .map_err(|_| invalid_provider_response(delivery))
}

fn normalize_usage(
    value: Option<&Value>,
    delivery: DeliveryState,
) -> Result<Option<Usage>, ModelError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| invalid_provider_response(delivery))?;
    let input = optional_u64(object, "input_tokens", delivery)?;
    let output = optional_u64(object, "output_tokens", delivery)?;
    let total = optional_u64(object, "total_tokens", delivery)?;
    let reasoning = match object.get("output_tokens_details") {
        None | Some(Value::Null) => None,
        Some(details) => optional_u64(
            details
                .as_object()
                .ok_or_else(|| invalid_provider_response(delivery))?,
            "reasoning_tokens",
            delivery,
        )?,
    };
    let cache_read = match object.get("input_tokens_details") {
        None | Some(Value::Null) => None,
        Some(details) => optional_u64(
            details
                .as_object()
                .ok_or_else(|| invalid_provider_response(delivery))?,
            "cached_tokens",
            delivery,
        )?,
    };
    let cache_write = optional_u64(object, "cache_write_tokens", delivery)?;
    Ok(Some(
        Usage::from_optional(input, output, reasoning)
            .with_cache_read_tokens(cache_read)
            .with_cache_write_tokens(cache_write)
            .with_provider_total_tokens(total),
    ))
}

fn optional_u64(
    object: &Map<String, Value>,
    key: &str,
    delivery: DeliveryState,
) -> Result<Option<u64>, ModelError> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid_provider_response(delivery)),
    }
}
