use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use minicore_runtime::model::{
    AssistantPart, CredentialSource, CredentialSourceFuture, DeliveryState, ModelCallContext,
    ModelDescriptor, ModelError, ModelErrorKind, ModelEvent, ModelEventSink, ModelFinishReason,
    ModelGateway, ModelLimits, ModelMessage, ModelRequest, ModelResponse, ModelSelection,
    OpenAiReasoningProgress, OpenAiResponsesProvider, ProviderCredential, ProviderEndpointPolicy,
    ProviderRegistryBuilder, ReasoningContent, ReasoningPreference, ToolCall,
    fixed_credential_source,
};
use minicore_runtime::tools::{ToolName, ToolOutput, ToolResultOutcome, ToolSpec};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct CapturedRequest {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body must be JSON")
    }
}

struct ScriptedResponse {
    status: u16,
    content_type: String,
    headers: Vec<(String, String)>,
    body: String,
    fragmented: bool,
    gate: Option<(Sender<()>, Receiver<()>)>,
}

struct LoopbackServer {
    address: std::net::SocketAddr,
    endpoint: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    fn single(status: u16, content_type: &str, body: String) -> Self {
        Self::multi(vec![ScriptedResponse {
            status,
            content_type: content_type.to_owned(),
            headers: Vec::new(),
            body,
            fragmented: false,
            gate: None,
        }])
    }

    fn fragmented(body: String) -> Self {
        Self::multi(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: Vec::new(),
            body,
            fragmented: true,
            gate: None,
        }])
    }

    fn gated(body: String) -> (Self, Receiver<()>, Sender<()>) {
        let (ready_tx, ready_rx) = channel();
        let (release_tx, release_rx) = channel();
        let server = Self::multi(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: Vec::new(),
            body,
            fragmented: false,
            gate: Some((ready_tx, release_rx)),
        }]);
        (server, ready_rx, release_tx)
    }

    fn multi(responses: Vec<ScriptedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let address = listener.local_addr().expect("loopback address");
        let endpoint = format!("http://{address}/responses");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let mut responses = responses.into_iter();
            loop {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut bytes = Vec::new();
                let mut scratch = [0_u8; 4096];
                let header_end = loop {
                    let count = stream.read(&mut scratch).expect("read request");
                    if count == 0 || (bytes.is_empty() && scratch[0] == 0) {
                        return;
                    }
                    bytes.extend_from_slice(&scratch[..count]);
                    if let Some(position) =
                        bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        break position;
                    }
                };
                let head = String::from_utf8_lossy(&bytes[..header_end]);
                let mut lines = head.split("\r\n");
                let _request_line = lines.next().expect("request line");
                let headers = lines
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        let (name, value) = line.split_once(':').expect("header pair");
                        (name.trim().to_owned(), value.trim().to_owned())
                    })
                    .collect::<Vec<_>>();
                let content_length = headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.parse::<usize>().ok())
                    .expect("content length");
                let mut request_body = bytes[header_end + 4..].to_vec();
                while request_body.len() < content_length {
                    let count = stream.read(&mut scratch).expect("read request body");
                    assert!(count > 0, "request ended before body");
                    request_body.extend_from_slice(&scratch[..count]);
                }
                request_body.truncate(content_length);
                captured
                    .lock()
                    .expect("capture lock")
                    .push(CapturedRequest {
                        headers,
                        body: request_body,
                    });
                let mut scripted = responses.next().expect("unexpected extra request");
                let reason = match scripted.status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    _ => "Test",
                };
                let mut head = format!(
                    "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    scripted.status,
                    scripted.content_type,
                    scripted.body.len()
                );
                for (name, value) in &scripted.headers {
                    head.push_str(name);
                    head.push_str(": ");
                    head.push_str(value);
                    head.push_str("\r\n");
                }
                head.push_str("\r\n");
                stream
                    .write_all(head.as_bytes())
                    .expect("write response head");
                if let Some((ready, release)) = scripted.gate.take() {
                    let boundary = scripted
                        .body
                        .find("\n\n")
                        .map(|offset| offset + 2)
                        .expect("gated SSE body must have an event boundary");
                    let (first, rest) = scripted.body.split_at(boundary);
                    stream
                        .write_all(first.as_bytes())
                        .expect("write gated event");
                    ready.send(()).expect("notify gated event");
                    let _ = release.recv();
                    let _ = stream.write_all(rest.as_bytes());
                } else if scripted.fragmented {
                    for byte in scripted.body.bytes() {
                        stream.write_all(&[byte]).expect("write fragment");
                    }
                } else {
                    stream
                        .write_all(scripted.body.as_bytes())
                        .expect("write response body");
                }
                let _ = stream.shutdown(Shutdown::Write);
            }
        });
        Self {
            address,
            endpoint,
            requests,
            handle: Some(handle),
        }
    }

    fn join(mut self) -> Vec<CapturedRequest> {
        if let Ok(mut poison) = TcpStream::connect(self.address) {
            let _ = poison.write_all(&[0]);
            let _ = poison.shutdown(Shutdown::Write);
        }
        self.handle
            .take()
            .expect("server join once")
            .join()
            .expect("server thread must not panic");
        let requests = std::mem::replace(&mut self.requests, Arc::new(Mutex::new(Vec::new())));
        Arc::try_unwrap(requests)
            .expect("capture owner")
            .into_inner()
            .expect("capture lock")
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Ok(mut poison) = TcpStream::connect(self.address) {
                let _ = poison.write_all(&[0]);
                let _ = poison.shutdown(Shutdown::Write);
            }
            let _ = handle.join();
        }
    }
}

fn descriptor() -> ModelDescriptor {
    ModelDescriptor::new(
        ModelSelection::new("openai".parse().unwrap(), "stable-model".parse().unwrap()),
        "gpt-5-api",
        ModelLimits::new(Some(128_000), Some(4_096)).unwrap(),
        BTreeSet::from([
            ReasoningPreference::Auto,
            ReasoningPreference::Low,
            ReasoningPreference::Medium,
            ReasoningPreference::High,
            ReasoningPreference::Disabled,
        ]),
    )
    .unwrap()
}

fn request(reasoning: ReasoningPreference) -> ModelRequest {
    request_with_tools(reasoning, Vec::new())
}

fn request_with_tools(reasoning: ReasoningPreference, tools: Vec<ToolSpec>) -> ModelRequest {
    ModelRequest::new(
        ModelSelection::new("openai".parse().unwrap(), "stable-model".parse().unwrap()),
        vec![
            ModelMessage::system("system one").unwrap(),
            ModelMessage::system("system two").unwrap(),
            ModelMessage::user("hello").unwrap(),
        ],
        tools,
        ModelLimits::new(Some(128_000), Some(123)).unwrap(),
        reasoning,
    )
    .unwrap()
}

fn completed_text(text: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "model": "gpt-5-api",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text}]
                }],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 4,
                    "output_tokens_details": {"reasoning_tokens": 2},
                    "input_tokens_details": {"cached_tokens": 3},
                    "total_tokens": 14
                }
            }
        })
    )
}

#[tokio::test(flavor = "current_thread")]
async fn openai_provider_replays_reasoning_and_tool_items_without_private_artifact_leaks() {
    let server = LoopbackServer::single(200, "text/event-stream", completed_text("next"));
    let replay = ReasoningContent::new(
        Some("prior raw".to_owned()),
        Some("prior summary".to_owned()),
        Some("prior-encrypted".to_owned()),
        Some("PRIVATE_SIGNATURE".to_owned()),
        Some("rs_1".parse().unwrap()),
    )
    .unwrap();
    let call = ToolCall::new(
        "call_1".parse().unwrap(),
        ToolName::from_str("read_file").unwrap(),
        json!({"path": "README.md"}),
        0,
    )
    .unwrap();
    let tool = ToolSpec::new(
        ToolName::from_str("read_file").unwrap(),
        "Read one file",
        json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    )
    .unwrap();
    let selection = ModelSelection::new("openai".parse().unwrap(), "stable-model".parse().unwrap());
    let request = ModelRequest::new(
        selection,
        vec![
            ModelMessage::user("first").unwrap(),
            ModelMessage::assistant(vec![
                AssistantPart::Reasoning(replay),
                AssistantPart::Text("checking".to_owned()),
                AssistantPart::ToolCall(call.clone()),
            ])
            .unwrap(),
            ModelMessage::tool_with_outcome(
                call.tool_call_id().clone(),
                ToolOutput::new("exact output").unwrap(),
                ToolResultOutcome::Success,
            )
            .unwrap(),
            ModelMessage::user("continue").unwrap(),
        ],
        vec![tool],
        ModelLimits::new(Some(128_000), Some(123)).unwrap(),
        ReasoningPreference::Auto,
    )
    .unwrap();
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    gateway
        .generate(
            request,
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    let requests = server.join();
    let wire = requests[0].json();
    assert_eq!(wire["tool_choice"], "auto");
    assert!(wire.get("reasoning").is_none(), "Auto reasoning is omitted");
    assert!(
        !serde_json::to_string(&wire)
            .unwrap()
            .contains("PRIVATE_SIGNATURE")
    );
    assert_eq!(
        wire["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "first"}]},
            {"type": "reasoning", "id": "rs_1",
             "summary": [{"type": "summary_text", "text": "prior summary"}],
             "content": [{"type": "reasoning_text", "text": "prior raw"}],
             "encrypted_content": "prior-encrypted"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
            {"type": "function_call", "call_id": "call_1", "name": "read_file",
             "arguments": "{\"path\":\"README.md\"}", "status": "completed"},
            {"type": "function_call_output", "call_id": "call_1", "output": "exact output", "status": "completed"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
        ])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reasoning_progress_mode_and_rich_terminal_preserve_provider_order() {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 1,
            "delta": "safe summary"
        }),
        json!({
            "type": "response.reasoning_text.delta",
            "output_index": 1,
            "delta": "raw hidden"
        }),
        json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "answer"
        }),
        json!({
            "type": "response.output_item.done",
            "item": {"type": "function_call"}
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_rich",
                "status": "completed",
                "model": "gpt-5-api",
                "output": [
                    {"type": "message", "id": "msg_rich", "role": "assistant", "status": "completed",
                     "content": [{"type": "output_text", "text": "answer"}]},
                    {"type": "reasoning", "id": "rs_rich", "summary": [{"type": "summary_text", "text": "safe summary"}],
                     "content": [{"type": "reasoning_text", "text": "raw hidden"}], "encrypted_content": "enc"},
                    {"type": "function_call", "call_id": "call_rich", "name": "read_file", "arguments": "{\"path\":\"x\"}", "status": "completed"}
                ],
                "usage": {"input_tokens": 5, "output_tokens": 7, "output_tokens_details": {"reasoning_tokens": 3}, "total_tokens": 12}
            }
        })
    );
    let server = LoopbackServer::single(200, "text/event-stream", body);
    let enabled_tool =
        ToolSpec::new(ToolName::from_str("read_file").unwrap(), "read", json!({})).unwrap();
    let provider = OpenAiResponsesProvider::new_with_reasoning_progress(
        &server.endpoint,
        ProviderEndpointPolicy::AllowLoopbackHttp,
        OpenAiReasoningProgress::SummaryOnly,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, mut events) = ModelEventSink::channel(8).unwrap();
    let result = gateway
        .generate(
            request_with_tools(ReasoningPreference::Low, vec![enabled_tool]),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::ReasoningDelta {
            delta: "safe summary".to_owned()
        })
    );
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::TextDelta {
            delta: "answer".to_owned()
        })
    );
    assert!(events.try_recv().is_err());
    assert_eq!(result.finish_reason(), ModelFinishReason::ToolCalls);
    assert_eq!(result.parts().len(), 3);
    assert!(matches!(result.parts()[0], AssistantPart::Text(ref text) if text == "answer"));
    match &result.parts()[1] {
        AssistantPart::Reasoning(reasoning) => {
            assert_eq!(reasoning.summary(), Some("safe summary"));
            assert_eq!(reasoning.text(), Some("raw hidden"));
            assert_eq!(reasoning.encrypted(), Some("enc"));
            assert_eq!(reasoning.provider_item_id().unwrap().as_str(), "rs_rich");
        }
        other => panic!("expected reasoning, got {other:?}"),
    }
    match &result.parts()[2] {
        AssistantPart::ToolCall(call) => {
            assert_eq!(call.call_index(), 0);
            assert_eq!(call.name().as_str(), "read_file");
            assert_eq!(call.arguments(), &json!({"path": "x"}));
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    assert_eq!(result.usage().unwrap().provider_total_tokens(), Some(12));
    assert_eq!(server.join().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn raw_reasoning_progress_requires_explicit_opt_in() {
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "summary hidden"}),
        json!({"type": "response.reasoning_text.delta", "output_index": 0, "delta": "raw visible"}),
    ) + &format!(
        "data: {}\n\n",
        json!({"type": "response.completed", "response": {"id": "resp_raw", "status": "completed", "model": "gpt-5-api",
        "output": [
            {"type": "reasoning", "id": "rs_raw", "summary": [{"type": "summary_text", "text": "summary hidden"}],
             "content": [{"type": "reasoning_text", "text": "raw visible"}]},
            {"type": "message", "id": "msg_raw", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "answer"}]}
        ]}})
    );
    let server = LoopbackServer::single(200, "text/event-stream", body);
    let provider = OpenAiResponsesProvider::new_with_reasoning_progress(
        &server.endpoint,
        ProviderEndpointPolicy::AllowLoopbackHttp,
        OpenAiReasoningProgress::RawText,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, mut events) = ModelEventSink::channel(8).unwrap();
    gateway
        .generate(
            request(ReasoningPreference::Low),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::ReasoningDelta {
            delta: "raw visible".to_owned()
        })
    );
    assert!(events.try_recv().is_err());
    assert_eq!(server.join().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn fragmented_sse_terminal_is_normalized_without_synthetic_success() {
    let server = LoopbackServer::fragmented(completed_text("fragmented"));
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let result = gateway
        .generate(
            request(ReasoningPreference::Disabled),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        result.parts(),
        &[AssistantPart::Text("fragmented".to_owned())]
    );
    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].json()["reasoning"],
        json!({"effort": "none"}),
        "Disabled must emit the explicit provider none effort"
    );
}

async fn run_http_error(script: ScriptedResponse) -> ModelError {
    let server = LoopbackServer::multi(vec![script]);
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let error = gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(server.join().len(), 1);
    error
}

#[tokio::test(flavor = "current_thread")]
async fn provider_function_arguments_use_the_same_json_shape_bounds() {
    let mut arguments = json!({});
    for _ in 0..33 {
        arguments = json!({"nested": arguments});
    }
    let body = format!(
        "data: {}\n\n",
        json!({"type": "response.completed", "response": {
            "id": "resp_args", "status": "completed", "model": "gpt-5-api",
            "output": [{"type": "function_call", "call_id": "call_args", "name": "read_file",
                        "arguments": serde_json::to_string(&arguments).unwrap(), "status": "completed"}]
        }})
    );
    let server = LoopbackServer::single(200, "text/event-stream", body);
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let enabled_tool =
        ToolSpec::new(ToolName::from_str("read_file").unwrap(), "read", json!({})).unwrap();
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let error = gateway
        .generate(
            request_with_tools(ReasoningPreference::Auto, vec![enabled_tool]),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
    assert_eq!(error.delivery(), DeliveryState::AcceptedNoOutput);
    assert_eq!(server.join().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn structural_http_errors_are_typed_without_message_matching_or_retry() {
    let cases = [
        (
            400,
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"not inspected"}}"#,
            Vec::new(),
            ModelErrorKind::ContextOverflow,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            400,
            r#"{"error":{"type":"invalid_request_error","message":"context_length_exceeded"}}"#,
            Vec::new(),
            ModelErrorKind::InvalidRequest,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            401,
            r#"{"error":{"type":"authentication_error","message":"secret"}}"#,
            Vec::new(),
            ModelErrorKind::AuthRejected,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            429,
            r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
            vec![("retry-after".to_owned(), "7".to_owned())],
            ModelErrorKind::RateLimited,
            DeliveryState::RejectedBeforeExecution,
            Some(Duration::from_secs(7)),
        ),
        (
            429,
            r#"{"error":{"type":"insufficient_quota","code":"insufficient_quota"}}"#,
            vec![("retry-after".to_owned(), "7".to_owned())],
            ModelErrorKind::QuotaExceeded,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            429,
            r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
            vec![("retry-after".to_owned(), u64::MAX.to_string())],
            ModelErrorKind::RateLimited,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            500,
            r#"{"error":{"type":"server_error","message":"context_length_exceeded"}}"#,
            Vec::new(),
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
            None,
        ),
    ];
    for (status, body, headers, kind, delivery, retry_after) in cases {
        let error = run_http_error(ScriptedResponse {
            status,
            content_type: "application/json".to_owned(),
            headers,
            body: body.to_owned(),
            fragmented: false,
            gate: None,
        })
        .await;
        assert_eq!(error.kind(), kind, "status {status}");
        assert_eq!(error.delivery(), delivery, "status {status}");
        assert_eq!(error.retry_after(), retry_after, "status {status}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn refusal_is_text_with_refused_finish_and_cannot_mix_tool_output() {
    let body = format!(
        "data: {}\n\n",
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_refused",
                "status": "completed",
                "model": "gpt-5-api",
                "output": [{"type": "message", "id": "msg_refused", "role": "assistant", "status": "completed",
                            "content": [{"type": "refusal", "refusal": "cannot comply"}]}]
            }
        })
    );
    let server = LoopbackServer::single(200, "text/event-stream", body);
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let result = gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.finish_reason(), ModelFinishReason::Refused);
    assert_eq!(
        result.parts(),
        &[AssistantPart::Text("cannot comply".to_owned())]
    );
    assert_eq!(server.join().len(), 1);
}

async fn run_success_stream(body: String) -> ModelResponse {
    let server = LoopbackServer::single(200, "text/event-stream", body);
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let result = gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(server.join().len(), 1);
    result
}

async fn run_stream_error(body: String, content_type: &str) -> ModelError {
    let server = LoopbackServer::single(200, content_type, body);
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let error = gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(server.join().len(), 1);
    error
}

#[tokio::test(flavor = "current_thread")]
async fn stream_failure_codes_are_structural_and_delivery_safe() {
    let failed_after_output = run_stream_error(
        format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "partial"}),
            json!({"type": "response.failed", "response": {"error": {
                "type": "insufficient_quota", "code": "insufficient_quota", "message": "not inspected"
            }}})
        ),
        "text/event-stream",
    )
    .await;
    assert_eq!(
        failed_after_output.kind(),
        ModelErrorKind::StreamInterrupted
    );
    assert_eq!(failed_after_output.delivery(), DeliveryState::OutputStarted);

    let error_before_output = run_stream_error(
        format!(
            "data: {}\n\n",
            json!({"type": "error", "code": "rate_limit_exceeded", "message": "not inspected"})
        ),
        "text/event-stream",
    )
    .await;
    assert_eq!(
        error_before_output.kind(),
        ModelErrorKind::RequestOutcomeUnknown
    );
    assert_eq!(error_before_output.delivery(), DeliveryState::Unknown);
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_reasoning_or_empty_output_is_incomplete() {
    for output in [
        json!([{"type": "reasoning", "id": "rs_only", "summary": [{"type": "summary_text", "text": "hidden"}]}]),
        json!([]),
    ] {
        let body = format!(
            "data: {}\n\n",
            json!({"type": "response.completed", "response": {
                "id": "resp_incomplete", "status": "completed", "model": "gpt-5-api", "output": output
            }})
        );
        let error = run_stream_error(body, "text/event-stream").await;
        assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
        assert_eq!(error.delivery(), DeliveryState::AcceptedNoOutput);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn absent_or_null_usage_stays_absent() {
    for usage in [None, Some(Value::Null)] {
        let mut terminal = json!({"type": "response.completed", "response": {
            "id": "resp_usage_none", "status": "completed", "model": "gpt-5-api",
            "output": [{"type": "message", "id": "msg_usage_none", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": "ok"}]}]
        }});
        if let Some(usage) = usage {
            terminal["response"]["usage"] = usage;
        }
        let result = run_success_stream(format!("data: {}\n\n", terminal)).await;
        assert_eq!(result.usage(), None);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn partial_usage_preserves_missing_reported_counts() {
    for (usage, input, output, reasoning) in [
        (json!({}), None, None, None),
        (json!({"input_tokens": 10}), Some(10), None, None),
    ] {
        let body = format!(
            "data: {}\n\n",
            json!({"type": "response.completed", "response": {
                "id": "resp_usage_partial", "status": "completed", "model": "gpt-5-api",
                "output": [{"type": "message", "id": "msg_usage_partial", "role": "assistant", "status": "completed",
                            "content": [{"type": "output_text", "text": "ok"}]}],
                "usage": usage
            }})
        );
        let result = run_success_stream(body).await;
        let usage = result.usage().expect("present usage object");
        assert_eq!(usage.input_tokens(), input);
        assert_eq!(usage.output_tokens(), output);
        assert_eq!(usage.reasoning_tokens(), reasoning);
        assert_eq!(usage.total_tokens(), None);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_or_disabled_function_calls_are_unexpected_tool_calls() {
    let terminal = |name: &str| {
        format!(
            "data: {}\n\n",
            json!({"type": "response.completed", "response": {
                "id": "resp_tool", "status": "completed", "model": "gpt-5-api",
                "output": [{"type": "function_call", "call_id": "call_tool", "name": name,
                            "arguments": "{}", "status": "completed"}]
            }})
        )
    };
    for (tools, name) in [
        (Vec::new(), "read_file"),
        (
            vec![
                ToolSpec::new(
                    ToolName::from_str("write_file").unwrap(),
                    "write",
                    json!({}),
                )
                .unwrap(),
            ],
            "read_file",
        ),
    ] {
        let server = LoopbackServer::single(200, "text/event-stream", terminal(name));
        let provider = OpenAiResponsesProvider::new_loopback_http(
            &server.endpoint,
            fixed_credential_source("sk-test").unwrap(),
            vec![descriptor()],
        )
        .unwrap();
        let mut registry = ProviderRegistryBuilder::default();
        registry.register(provider).unwrap();
        let gateway = ModelGateway::new(registry.build());
        let request = ModelRequest::new(
            ModelSelection::new("openai".parse().unwrap(), "stable-model".parse().unwrap()),
            vec![ModelMessage::user("call a tool").unwrap()],
            tools,
            ModelLimits::new(Some(128_000), Some(123)).unwrap(),
            ReasoningPreference::Auto,
        )
        .unwrap();
        let (sink, _events) = ModelEventSink::channel(8).unwrap();
        let error = gateway
            .generate(
                request,
                ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ModelErrorKind::UnexpectedToolCall);
        assert_eq!(error.delivery(), DeliveryState::AcceptedNoOutput);
        assert_eq!(server.join().len(), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn early_eof_and_malformed_successes_never_become_success() {
    let before_output = run_stream_error(
        format!(
            "data: {}\n\n",
            json!({"type": "response.created", "response": {"status": "in_progress"}})
        ),
        "text/event-stream",
    )
    .await;
    assert_eq!(before_output.kind(), ModelErrorKind::RequestOutcomeUnknown);
    assert_eq!(before_output.delivery(), DeliveryState::Unknown);

    let after_output = run_stream_error(
        format!(
            "data: {}\n\n",
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "partial"})
        ),
        "text/event-stream",
    )
    .await;
    assert_eq!(after_output.kind(), ModelErrorKind::StreamInterrupted);
    assert_eq!(after_output.delivery(), DeliveryState::OutputStarted);

    let malformed_json =
        run_stream_error("data: not-json\n\n".to_owned(), "text/event-stream").await;
    assert_eq!(
        malformed_json.kind(),
        ModelErrorKind::InvalidProviderResponse
    );
    assert_eq!(malformed_json.delivery(), DeliveryState::AcceptedNoOutput);

    let wrong_content_type = run_stream_error("{}".to_owned(), "application/json").await;
    assert_eq!(
        wrong_content_type.kind(),
        ModelErrorKind::InvalidProviderResponse
    );
    assert_eq!(
        wrong_content_type.delivery(),
        DeliveryState::AcceptedNoOutput
    );
}

struct RotatingCredentialSource {
    values: Mutex<Vec<ProviderCredential>>,
}

impl CredentialSource for RotatingCredentialSource {
    fn resolve(&self) -> CredentialSourceFuture<'_> {
        let value = self.values.lock().unwrap().pop();
        Box::pin(async move { value })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn credential_source_is_resolved_fresh_for_each_attempt() {
    let server = LoopbackServer::multi(vec![
        ScriptedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: Vec::new(),
            body: completed_text("one"),
            fragmented: false,
            gate: None,
        },
        ScriptedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: Vec::new(),
            body: completed_text("two"),
            fragmented: false,
            gate: None,
        },
    ]);
    let source: Arc<dyn CredentialSource> = Arc::new(RotatingCredentialSource {
        values: Mutex::new(vec![
            "second-secret".parse().unwrap(),
            "first-secret".parse().unwrap(),
        ]),
    });
    let provider =
        OpenAiResponsesProvider::new_loopback_http(&server.endpoint, source, vec![descriptor()])
            .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    for _ in 0..2 {
        let (sink, _events) = ModelEventSink::channel(8).unwrap();
        gateway
            .generate(
                request(ReasoningPreference::Auto),
                ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
            )
            .await
            .unwrap();
    }
    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer first-secret")
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer second-secret")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_full_request_is_rejected_before_post() {
    let server = LoopbackServer::single(200, "text/event-stream", completed_text("never"));
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let messages = (0..5)
        .map(|_| ModelMessage::user("x".repeat(262_144)).unwrap())
        .collect();
    let oversized = ModelRequest::new(
        ModelSelection::new("openai".parse().unwrap(), "stable-model".parse().unwrap()),
        messages,
        Vec::new(),
        ModelLimits::new(None, Some(123)).unwrap(),
        ReasoningPreference::Auto,
    )
    .unwrap();
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let error = gateway
        .generate(
            oversized,
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::InvalidRequest);
    assert_eq!(error.delivery(), DeliveryState::NotSent);
    assert!(server.join().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn pre_send_cancellation_makes_no_post_and_is_not_sent() {
    let server = LoopbackServer::single(200, "text/event-stream", completed_text("never"));
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(cancellation, sink).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::NotSent);
    assert!(server.join().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn post_delta_cancellation_preserves_output_started() {
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "partial"}),
        json!({"type": "response.completed", "response": {"id": "resp_cancel", "status": "completed", "model": "gpt-5-api",
            "output": [{"type": "message", "id": "msg_cancel", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": "partial"}]}]}})
    );
    let (server, ready, release) = LoopbackServer::gated(body);
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, mut events) = ModelEventSink::channel(8).unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let gateway = gateway.clone();
        let context = ModelCallContext::new(cancellation.clone(), sink).unwrap();
        async move {
            gateway
                .generate(request(ReasoningPreference::Auto), context)
                .await
        }
    });
    loop {
        match ready.try_recv() {
            Ok(()) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("server exited before the first event")
            }
        }
    }
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::TextDelta {
            delta: "partial".to_owned()
        })
    );
    cancellation.cancel();
    release.send(()).expect("release server");
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::OutputStarted);
    assert_eq!(server.join().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn in_flight_cancellation_before_semantic_output_is_conservative() {
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type": "response.created", "response": {"status": "in_progress"}}),
        json!({"type": "response.completed", "response": {"id": "resp_cancel_2", "status": "completed", "model": "gpt-5-api",
            "output": [{"type": "message", "id": "msg_cancel_2", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": "late"}]}]}})
    );
    let (server, ready, release) = LoopbackServer::gated(body);
    let provider = OpenAiResponsesProvider::new_loopback_http(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let gateway = gateway.clone();
        let context = ModelCallContext::new(cancellation.clone(), sink).unwrap();
        async move {
            gateway
                .generate(request(ReasoningPreference::Auto), context)
                .await
        }
    });
    loop {
        match ready.try_recv() {
            Ok(()) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("server exited before the response event")
            }
        }
    }
    cancellation.cancel();
    release.send(()).expect("release server");
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    // After HTTP success but before semantic output, the provider may win with
    // precise AcceptedNoOutput or the Gateway's outer cancellation may win
    // conservatively with Unknown. Both are unsafe to replay.
    assert!(matches!(
        error.delivery(),
        DeliveryState::AcceptedNoOutput | DeliveryState::Unknown
    ));
    assert_eq!(server.join().len(), 1);
}

#[test]
fn endpoint_policy_and_provider_debug_are_redacted() {
    let source = fixed_credential_source("sk-SECRET").unwrap();
    let models = vec![descriptor()];
    assert!(
        OpenAiResponsesProvider::new(
            "https://api.openai.com/v1/responses",
            ProviderEndpointPolicy::HttpsOnly,
            Arc::clone(&source),
            models.clone(),
        )
        .is_ok()
    );
    assert!(
        OpenAiResponsesProvider::new(
            "http://127.0.0.1:1234/responses",
            ProviderEndpointPolicy::AllowLoopbackHttp,
            Arc::clone(&source),
            models.clone(),
        )
        .is_ok()
    );
    for endpoint in [
        "http://api.openai.com/v1/responses",
        "http://localhost:1234/responses",
        "https://user:pass@api.openai.com/v1/responses",
        "https://api.openai.com/v1/responses?secret=TOP",
        "https://api.openai.com/v1/responses#secret",
    ] {
        assert!(
            OpenAiResponsesProvider::new(
                endpoint,
                ProviderEndpointPolicy::AllowLoopbackHttp,
                Arc::clone(&source),
                models.clone(),
            )
            .is_err()
        );
    }
    let provider = OpenAiResponsesProvider::new(
        "https://api.openai.com/v1/responses",
        ProviderEndpointPolicy::HttpsOnly,
        source,
        models,
    )
    .unwrap();
    let debug = format!("{provider:?}");
    assert!(!debug.contains("api.openai.com"));
    assert!(!debug.contains("sk-SECRET"));
}

#[tokio::test(flavor = "current_thread")]
async fn openai_provider_encodes_full_request_and_normalizes_terminal() {
    let server = LoopbackServer::single(200, "text/event-stream", completed_text("done"));
    let credential_source = fixed_credential_source("sk-test").unwrap();
    let provider = OpenAiResponsesProvider::new(
        &server.endpoint,
        ProviderEndpointPolicy::AllowLoopbackHttp,
        credential_source,
        vec![descriptor()],
    )
    .unwrap();
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    let gateway = ModelGateway::new(registry.build());
    let (sink, _events) = ModelEventSink::channel(8).unwrap();

    let result = gateway
        .generate(
            request(ReasoningPreference::Low),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    let requests = server.join();
    assert_eq!(requests.len(), 1);
    let captured = &requests[0];
    assert_eq!(captured.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(captured.header("content-type"), Some("application/json"));
    assert_eq!(captured.header("accept"), Some("text/event-stream"));
    let wire = captured.json();
    assert_eq!(wire["model"], "gpt-5-api");
    assert_eq!(wire["instructions"], "system one\n\nsystem two");
    assert_eq!(wire["max_output_tokens"], 123);
    assert_eq!(wire["stream"], true);
    assert_eq!(wire["store"], false);
    assert_eq!(wire["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(
        wire["reasoning"],
        json!({"effort": "low", "summary": "auto"})
    );
    assert!(wire.get("tools").is_none());
    assert!(wire.get("tool_choice").is_none());
    assert!(wire.get("previous_response_id").is_none());
    assert_eq!(
        wire["input"],
        json!([{"type": "message", "role": "user", "content":
                 [{"type": "input_text", "text": "hello"}]}])
    );
    assert_eq!(result.finish_reason(), ModelFinishReason::Stop);
    assert_eq!(result.parts(), &[AssistantPart::Text("done".to_owned())]);
    assert_eq!(result.usage().unwrap().input_tokens(), Some(10));
    assert_eq!(result.usage().unwrap().output_tokens(), Some(4));
    assert_eq!(result.usage().unwrap().reasoning_tokens(), Some(2));
    assert_eq!(result.usage().unwrap().cache_read_tokens(), Some(3));
    assert_eq!(result.usage().unwrap().provider_total_tokens(), Some(14));
}

#[test]
fn credential_and_reasoning_artifacts_are_checked_and_redacted() {
    let credential: ProviderCredential = "sk-SECRET".parse().unwrap();
    let credential_debug = format!("{credential:?}");
    assert!(!credential_debug.contains("SECRET"));
    assert!("x".repeat(256).parse::<ProviderCredential>().is_ok());
    assert!("x".repeat(257).parse::<ProviderCredential>().is_err());
    assert!("has space".parse::<ProviderCredential>().is_err());

    let item_id = "rs_SECRET".parse().unwrap();
    let reasoning = ReasoningContent::new(
        Some("visible reasoning".to_owned()),
        Some("safe summary".to_owned()),
        Some("ENCRYPTED_SECRET".to_owned()),
        Some("SIGNATURE_SECRET".to_owned()),
        Some(item_id),
    )
    .unwrap();
    let encoded = serde_json::to_value(&reasoning).unwrap();
    let decoded: ReasoningContent = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, reasoning);

    let debug = format!("{reasoning:?}");
    for secret in ["ENCRYPTED_SECRET", "SIGNATURE_SECRET", "rs_SECRET"] {
        assert!(!debug.contains(secret), "reasoning Debug leaked {secret}");
    }
    assert!(ReasoningContent::new(None, None, None, None, None).is_err());
    assert!(
        ReasoningContent::new(None, None, None, None, Some("rs_only".parse().unwrap()),).is_err()
    );

    let assistant = AssistantPart::Reasoning(reasoning);
    assert!(assistant.validate().is_ok());
}

#[test]
fn new_provider_module_stays_model_owned_and_private() {
    let source = include_str!("../src/model/providers/openai.rs");
    for forbidden in [
        "crate::prompt",
        "crate::session",
        "crate::runtime",
        "crate::wire",
        "crate::model_gateway",
        "crate::http_transport",
        "allow(dead_code",
        "tokio::spawn",
        "std::thread::spawn",
        "std::thread::sleep",
        "tokio::time::sleep",
        "tokio::time::timeout",
        "set_hook",
    ] {
        assert!(
            !source.contains(forbidden),
            "found forbidden import/tool {forbidden}"
        );
    }
    assert!(
        source.len() < 200_000,
        "provider source must remain split-sized"
    );
}

#[test]
fn assistant_reasoning_json_is_ordinary_checked_persistence() {
    let reasoning = ReasoningContent::new(
        None,
        Some("summary".to_owned()),
        None,
        None,
        Some("rs_1".parse().unwrap()),
    )
    .unwrap();
    let assistant = AssistantPart::Reasoning(reasoning);
    let json = serde_json::to_string(&assistant).unwrap();
    assert!(json.contains("reasoning"));
    let decoded: AssistantPart = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, assistant);
}
