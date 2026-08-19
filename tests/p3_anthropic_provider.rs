use std::collections::{BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use minicore_runtime::{
    AnthropicMessagesProvider, AnthropicProviderError, AssistantPart, CredentialSource,
    CredentialSourceFuture, DeliveryState, ModelCallContext, ModelDescriptor, ModelErrorKind,
    ModelEvent, ModelFinishReason, ModelGateway, ModelLimits, ModelMessage, ModelRequest,
    ModelSelection, ProviderRegistryBuilder, ReasoningContent, ReasoningPreference, ToolCall,
    ToolName, ToolOutput, ToolSpec, fixed_credential_source,
};
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
    fn single(status: u16, content_type: &str, body: impl Into<String>) -> Self {
        Self::multi(vec![ScriptedResponse {
            status,
            content_type: content_type.to_owned(),
            headers: Vec::new(),
            body: body.into(),
            fragmented: false,
            gate: None,
        }])
    }

    fn fragmented(body: impl Into<String>) -> Self {
        Self::multi(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: Vec::new(),
            body: body.into(),
            fragmented: true,
            gate: None,
        }])
    }

    fn gated(body: impl Into<String>) -> (Self, Receiver<()>, Sender<()>) {
        let (ready_tx, ready_rx) = channel();
        let (release_tx, release_rx) = channel();
        let server = Self::multi(vec![ScriptedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: Vec::new(),
            body: body.into(),
            fragmented: false,
            gate: Some((ready_tx, release_rx)),
        }]);
        (server, ready_rx, release_tx)
    }

    fn multi(responses: Vec<ScriptedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let address = listener.local_addr().expect("loopback address");
        let endpoint = format!("http://{address}/v1/messages");
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
                let mut body = bytes[header_end + 4..].to_vec();
                while body.len() < content_length {
                    let count = stream.read(&mut scratch).expect("read request body");
                    assert!(count > 0, "request ended before body");
                    body.extend_from_slice(&scratch[..count]);
                }
                body.truncate(content_length);
                captured
                    .lock()
                    .expect("capture lock")
                    .push(CapturedRequest { headers, body });

                let mut scripted = responses.next().expect("unexpected extra request");
                let reason = match scripted.status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    402 => "Payment Required",
                    413 => "Payload Too Large",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    529 => "Unknown Error",
                    _ => "Test",
                };
                let mut response_head = format!(
                    "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    scripted.status,
                    scripted.content_type,
                    scripted.body.len()
                );
                for (name, value) in &scripted.headers {
                    response_head.push_str(name);
                    response_head.push_str(": ");
                    response_head.push_str(value);
                    response_head.push_str("\r\n");
                }
                response_head.push_str("\r\n");
                stream
                    .write_all(response_head.as_bytes())
                    .expect("write response head");
                if let Some((ready, release)) = scripted.gate.take() {
                    let mut boundary = 0;
                    for _ in 0..3 {
                        let offset = scripted.body[boundary..]
                            .find("\n\n")
                            .expect("gated SSE body must have an event boundary");
                        boundary += offset + 2;
                    }
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
            .expect("loopback server must not panic");
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
        ModelSelection::new(
            "anthropic".parse().unwrap(),
            "stable-model".parse().unwrap(),
        ),
        "claude-sonnet-api",
        ModelLimits::new(Some(128_000), Some(4_096)).unwrap(),
        BTreeSet::from([
            ReasoningPreference::Auto,
            ReasoningPreference::Disabled,
            ReasoningPreference::Low,
            ReasoningPreference::Medium,
            ReasoningPreference::High,
        ]),
    )
    .unwrap()
}

fn request(reasoning: ReasoningPreference) -> ModelRequest {
    ModelRequest::new(
        ModelSelection::new(
            "anthropic".parse().unwrap(),
            "stable-model".parse().unwrap(),
        ),
        vec![
            ModelMessage::system("system one").unwrap(),
            ModelMessage::system("system two").unwrap(),
            ModelMessage::user("hello").unwrap(),
        ],
        Vec::new(),
        ModelLimits::new(Some(128_000), Some(123)).unwrap(),
        reasoning,
    )
    .unwrap()
}

fn request_with_tools(tools: Vec<ToolSpec>) -> ModelRequest {
    ModelRequest::new(
        ModelSelection::new(
            "anthropic".parse().unwrap(),
            "stable-model".parse().unwrap(),
        ),
        vec![
            ModelMessage::system("system one").unwrap(),
            ModelMessage::user("hello").unwrap(),
        ],
        tools,
        ModelLimits::new(Some(128_000), Some(123)).unwrap(),
        ReasoningPreference::Auto,
    )
    .unwrap()
}

fn provider(
    endpoint: &str,
    credential_source: Arc<dyn CredentialSource>,
) -> AnthropicMessagesProvider {
    AnthropicMessagesProvider::new_loopback_http(
        endpoint,
        "2023-06-01",
        credential_source,
        vec![descriptor()],
    )
    .unwrap()
}

fn make_gateway(provider: AnthropicMessagesProvider) -> ModelGateway {
    let mut registry = ProviderRegistryBuilder::default();
    registry.register(provider).unwrap();
    ModelGateway::new(registry.build())
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {}\n\n", event))
        .collect()
}

fn message_start(usage: Option<Value>) -> Value {
    let mut message = json!({
        "type": "message",
        "id": "msg_1",
        "role": "assistant",
        "model": "claude-sonnet-api",
        "content": [],
        "stop_reason": null,
        "stop_sequence": null,
    });
    message["usage"] = usage.unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0}));
    json!({"type": "message_start", "message": message})
}

fn message_start_without_usage() -> Value {
    let mut event = message_start(None);
    event["message"].as_object_mut().unwrap().remove("usage");
    event
}

fn text_start(index: u64) -> Value {
    json!({
        "type": "content_block_start",
        "index": index,
        "content_block": {"type": "text", "text": ""}
    })
}

fn thinking_start(index: u64, signature: Option<&str>) -> Value {
    let mut block = json!({"type": "thinking", "thinking": ""});
    if let Some(signature) = signature {
        block["signature"] = json!(signature);
    }
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn tool_start(index: u64, id: &str, name: &str) -> Value {
    json!({
        "type": "content_block_start",
        "index": index,
        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
    })
}

fn block_delta(index: u64, delta: Value) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": delta})
}

fn block_stop(index: u64) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

fn message_delta(stop_reason: &str, usage: Option<Value>) -> Value {
    let mut event = json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null}
    });
    event["usage"] = usage.unwrap_or_else(|| json!({"output_tokens": 0}));
    event
}

fn message_delta_without_usage(stop_reason: &str) -> Value {
    let mut event = message_delta(stop_reason, None);
    event.as_object_mut().unwrap().remove("usage");
    event
}

fn text_terminal(text: &str, usage: Option<Value>) -> String {
    sse(&[
        message_start(Some(json!({"input_tokens": 10, "output_tokens": 0}))),
        text_start(0),
        block_delta(0, json!({"type": "text_delta", "text": text})),
        block_stop(0),
        message_delta("end_turn", usage),
    ])
}

async fn run_success(
    body: String,
    request: ModelRequest,
) -> (minicore_runtime::ModelResponse, Vec<CapturedRequest>) {
    let server = LoopbackServer::single(200, "text/event-stream", body);
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(16).unwrap();
    let result = gateway
        .generate(
            request,
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    (result, server.join())
}

async fn run_error(
    body: String,
    content_type: &str,
    request: ModelRequest,
) -> (minicore_runtime::ModelError, Vec<CapturedRequest>) {
    let server = LoopbackServer::single(200, content_type, body);
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(16).unwrap();
    let error = gateway
        .generate(
            request,
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap_err();
    (error, server.join())
}

#[test]
fn endpoint_version_and_provider_debug_are_checked_and_redacted() {
    let source = fixed_credential_source("sk-SECRET").unwrap();
    assert!(
        AnthropicMessagesProvider::new_https(
            "https://api.anthropic.example/v1/messages",
            "2023-06-01",
            source.clone(),
            vec![descriptor()],
        )
        .is_ok()
    );
    assert!(
        AnthropicMessagesProvider::new_https(
            "http://127.0.0.1:1234/v1/messages",
            "2023-06-01",
            source.clone(),
            vec![descriptor()],
        )
        .is_err()
    );
    assert!(
        AnthropicMessagesProvider::new_loopback_http(
            "http://127.0.0.1:1234/v1/messages",
            "2023-06-01",
            source.clone(),
            vec![descriptor()],
        )
        .is_ok()
    );
    for endpoint in [
        "http://localhost:1234/v1/messages",
        "http://127.0.0.2:1234/v1/messages",
        "https://user@example.com/v1/messages?x=1",
        "https://example.com/v1/messages#fragment",
    ] {
        assert!(
            AnthropicMessagesProvider::new_https(
                endpoint,
                "2023-06-01",
                source.clone(),
                vec![descriptor()],
            )
            .is_err()
        );
    }
    assert_eq!(
        AnthropicMessagesProvider::new_https(
            "https://api.anthropic.example/v1/messages",
            "",
            source.clone(),
            vec![descriptor()],
        )
        .unwrap_err(),
        AnthropicProviderError::InvalidVersion
    );
    assert_eq!(
        AnthropicMessagesProvider::new_https(
            "https://api.anthropic.example/v1/messages",
            &"x".repeat(65),
            source.clone(),
            vec![descriptor()],
        )
        .unwrap_err(),
        AnthropicProviderError::InvalidVersion
    );
    let provider = AnthropicMessagesProvider::new_https(
        "https://api.anthropic.example/v1/messages",
        "2023-06-01",
        source,
        vec![descriptor()],
    )
    .unwrap();
    let debug = format!("{provider:?}");
    assert!(debug.contains("2023-06-01"));
    assert!(!debug.contains("api.anthropic.example"));
    assert!(!debug.contains("claude-sonnet-api"));
    assert!(!debug.contains("SECRET"));
}

#[tokio::test(flavor = "current_thread")]
async fn exact_headers_body_and_reasoning_wire_match_old_messages_contract() {
    let (result, requests) = run_success(
        text_terminal(
            "answer",
            Some(json!({"output_tokens": 4, "output_tokens_details": {"thinking_tokens": 2}})),
        ),
        request(ReasoningPreference::Low),
    )
    .await;
    assert_eq!(result.finish_reason(), ModelFinishReason::Stop);
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.header("x-api-key"), Some("sk-test"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert!(request.header("anthropic-beta").is_none());
    assert!(request.header("cache-control").is_none());
    let wire = request.json();
    assert_eq!(wire["model"], "claude-sonnet-api");
    assert_eq!(wire["max_tokens"], 123);
    assert_eq!(wire["stream"], true);
    assert_eq!(
        wire["system"],
        json!([{"type": "text", "text": "system one"}, {"type": "text", "text": "system two"}])
    );
    assert_eq!(
        wire["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
    );
    assert_eq!(wire["thinking"], json!({"type": "adaptive"}));
    assert_eq!(wire["output_config"], json!({"effort": "low"}));
    let encoded = serde_json::to_string(&wire).unwrap();
    assert_eq!(wire["service_tier"], "standard_only");
    for forbidden in [
        "cache_control",
        "anthropic-beta",
        "previous_response",
        "continuation",
    ] {
        assert!(!encoded.contains(forbidden), "forbidden field {forbidden}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reasoning_modes_match_old_anthropic_mapping() {
    for (reasoning, thinking, output_config) in [
        (ReasoningPreference::Auto, None, None),
        (
            ReasoningPreference::Disabled,
            Some(json!({"type": "disabled"})),
            None,
        ),
        (
            ReasoningPreference::Medium,
            Some(json!({"type": "adaptive"})),
            Some(json!({"effort": "medium"})),
        ),
        (
            ReasoningPreference::High,
            Some(json!({"type": "adaptive"})),
            Some(json!({"effort": "high"})),
        ),
    ] {
        let server = LoopbackServer::single(200, "text/event-stream", text_terminal("ok", None));
        let gateway = make_gateway(provider(
            &server.endpoint,
            fixed_credential_source("sk-test").unwrap(),
        ));
        let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
        gateway
            .generate(
                request(reasoning),
                ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
            )
            .await
            .unwrap();
        let requests = server.join();
        let wire = requests[0].json();
        assert_eq!(wire.get("thinking"), thinking.as_ref());
        assert_eq!(wire.get("output_config"), output_config.as_ref());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn request_replay_preserves_ordered_tools_results_and_thinking_rules() {
    let server = LoopbackServer::single(200, "text/event-stream", text_terminal("next", None));
    let call = ToolCall::new(
        "call_1".parse().unwrap(),
        ToolName::from_str("read_file").unwrap(),
        json!({"path": "README.md"}),
        0,
    )
    .unwrap();
    let signed = ReasoningContent::new(
        Some("prior thought".to_owned()),
        Some("summary from another provider".to_owned()),
        Some("openai-encrypted".to_owned()),
        Some("anthropic-signature".to_owned()),
        Some("rs_openai".parse().unwrap()),
    )
    .unwrap();
    let unsigned =
        ReasoningContent::new(Some("do not replay".to_owned()), None, None, None, None).unwrap();
    let foreign_encrypted = ReasoningContent::new(
        None,
        None,
        Some("foreign-encrypted".to_owned()),
        None,
        Some("rs_foreign".parse().unwrap()),
    )
    .unwrap();
    let redacted = ReasoningContent::new(
        None,
        None,
        Some("anthropic-redacted".to_owned()),
        None,
        None,
    )
    .unwrap();
    let request = ModelRequest::new(
        ModelSelection::new(
            "anthropic".parse().unwrap(),
            "stable-model".parse().unwrap(),
        ),
        vec![
            ModelMessage::user("first").unwrap(),
            ModelMessage::assistant(vec![
                AssistantPart::Reasoning(signed),
                AssistantPart::Reasoning(unsigned),
                AssistantPart::Reasoning(foreign_encrypted),
                AssistantPart::Reasoning(redacted),
                AssistantPart::Text("checking".to_owned()),
                AssistantPart::ToolCall(call.clone()),
            ])
            .unwrap(),
            ModelMessage::tool(
                call.tool_call_id().clone(),
                ToolOutput::failure("not found").unwrap(),
            )
            .unwrap(),
            ModelMessage::user("continue").unwrap(),
        ],
        vec![
            ToolSpec::new(
                ToolName::from_str("read_file").unwrap(),
                "Read one file",
                json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            )
            .unwrap(),
        ],
        ModelLimits::new(Some(128_000), Some(123)).unwrap(),
        ReasoningPreference::Auto,
    )
    .unwrap();
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
    gateway
        .generate(
            request,
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    let wire = server.join()[0].json();
    assert_eq!(
        wire["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "first"}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "prior thought", "signature": "anthropic-signature"},
                {"type": "redacted_thinking", "data": "anthropic-redacted"},
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "README.md"}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": [{"type": "text", "text": "not found"}], "is_error": true}]},
            {"role": "user", "content": [{"type": "text", "text": "continue"}]}
        ])
    );
    let encoded = serde_json::to_string(&wire).unwrap();
    assert!(!encoded.contains("do not replay"));
    assert!(!encoded.contains("foreign-encrypted"));
    assert!(!encoded.contains("summary from another provider"));
    assert_eq!(wire["tools"][0]["name"], "read_file");
    assert_eq!(wire["tool_choice"], json!({"type": "auto"}));
}

#[tokio::test(flavor = "current_thread")]
async fn rich_terminal_publishes_reasoning_text_and_normalizes_tool_calls_and_usage() {
    let body = sse(&[
        message_start(Some(json!({
            "input_tokens": 10,
            "output_tokens": 1,
            "output_tokens_details": {"thinking_tokens": 1},
            "cache_read_input_tokens": 2,
            "cache_creation_input_tokens": 3,
        }))),
        thinking_start(0, None),
        block_delta(0, json!({"type": "thinking_delta", "thinking": "ponder"})),
        block_delta(
            0,
            json!({"type": "signature_delta", "signature": "sig_late"}),
        ),
        block_stop(0),
        text_start(1),
        block_delta(1, json!({"type": "text_delta", "text": "answer"})),
        block_stop(1),
        tool_start(2, "toolu_1", "read_file"),
        block_delta(
            2,
            json!({"type": "input_json_delta", "partial_json": "{\"path\":"}),
        ),
        block_delta(
            2,
            json!({"type": "input_json_delta", "partial_json": "\"x\"}"}),
        ),
        block_stop(2),
        message_delta(
            "tool_use",
            Some(json!({
                "input_tokens": 10,
                "output_tokens": 7,
                "output_tokens_details": {"thinking_tokens": 2},
                "cache_read_input_tokens": 4,
                "cache_creation_input_tokens": 5,
            })),
        ),
    ]);
    let server = LoopbackServer::single(200, "text/event-stream", body);
    let tool = ToolSpec::new(ToolName::from_str("read_file").unwrap(), "read", json!({})).unwrap();
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let (sink, mut events) = minicore_runtime::ModelEventSink::channel(16).unwrap();
    let result = gateway
        .generate(
            ModelRequest::new(
                request(ReasoningPreference::Auto).selection().clone(),
                request(ReasoningPreference::Auto).messages().to_vec(),
                vec![tool],
                request(ReasoningPreference::Auto).limits().to_owned(),
                ReasoningPreference::Auto,
            )
            .unwrap(),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::ReasoningDelta {
            delta: "ponder".to_owned()
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
    match &result.parts()[0] {
        AssistantPart::Reasoning(reasoning) => {
            assert_eq!(reasoning.text(), Some("ponder"));
            assert_eq!(reasoning.signature(), Some("sig_late"));
            assert_eq!(reasoning.provider_item_id(), None);
        }
        other => panic!("expected reasoning, got {other:?}"),
    }
    assert!(matches!(&result.parts()[1], AssistantPart::Text(text) if text == "answer"));
    match &result.parts()[2] {
        AssistantPart::ToolCall(call) => {
            assert_eq!(call.call_index(), 0);
            assert_eq!(call.name().as_str(), "read_file");
            assert_eq!(call.arguments(), &json!({"path": "x"}));
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    let usage = result.usage().unwrap();
    assert_eq!(usage.input_tokens(), Some(10));
    assert_eq!(usage.output_tokens(), Some(7));
    assert_eq!(usage.reasoning_tokens(), Some(2));
    assert_eq!(usage.cache_read_tokens(), Some(4));
    assert_eq!(usage.cache_write_tokens(), Some(5));
    assert_eq!(usage.provider_total_tokens(), None);
    assert_eq!(server.join().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn unsigned_and_signed_thinking_only_follow_frozen_old_behavior() {
    let signed = sse(&[
        message_start(None),
        thinking_start(0, Some("sig_only")),
        block_stop(0),
        message_delta("end_turn", None),
    ]);
    let (error, _) = run_error(
        signed,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
    assert_eq!(error.delivery(), DeliveryState::AcceptedNoOutput);

    let unsigned = sse(&[
        message_start(None),
        thinking_start(0, None),
        block_delta(0, json!({"type": "thinking_delta", "thinking": "hidden"})),
        block_stop(0),
        message_delta("end_turn", None),
    ]);
    let (error, _) = run_error(
        unsigned,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
    assert_eq!(error.delivery(), DeliveryState::OutputStarted);
}

#[tokio::test(flavor = "current_thread")]
async fn finish_reasons_and_terminal_proof_are_structural() {
    for (stop_reason, expected) in [
        ("end_turn", ModelFinishReason::Stop),
        ("stop_sequence", ModelFinishReason::Stop),
        ("custom_stop", ModelFinishReason::Unknown),
        ("refusal", ModelFinishReason::Refused),
    ] {
        let body = sse(&[
            message_start(None),
            text_start(0),
            block_delta(0, json!({"type": "text_delta", "text": "visible"})),
            block_stop(0),
            message_delta(stop_reason, None),
        ]);
        let (result, _) = run_success(body, request(ReasoningPreference::Auto)).await;
        assert_eq!(result.finish_reason(), expected, "{stop_reason}");
    }

    for stop_reason in [
        "max_tokens",
        "model_context_window_exceeded",
        "pause_turn",
        "content_filter",
    ] {
        let body = sse(&[
            message_start(None),
            text_start(0),
            block_delta(0, json!({"type": "text_delta", "text": "visible"})),
            block_stop(0),
            message_delta(stop_reason, None),
        ]);
        let (error, _) = run_error(
            body,
            "text/event-stream",
            request(ReasoningPreference::Auto),
        )
        .await;
        assert_eq!(
            error.kind(),
            ModelErrorKind::IncompleteResponse,
            "{stop_reason}"
        );
        assert_eq!(
            error.delivery(),
            DeliveryState::OutputStarted,
            "{stop_reason}"
        );
    }

    let message_stop = sse(&[message_start(None), json!({"type": "message_stop"})]);
    let (error, _) = run_error(
        message_stop,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::RequestOutcomeUnknown);
    assert_eq!(error.delivery(), DeliveryState::Unknown);

    let malformed = "data: {not-json}\n\n".to_owned();
    let (error, _) = run_error(
        malformed,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_content_shapes_follow_stop_reason_contract() {
    let tool = ToolSpec::new(ToolName::from_str("read_file").unwrap(), "read", json!({})).unwrap();
    let tool_request = || request_with_tools(vec![tool.clone()]);

    for stop_reason in ["end_turn", "stop_sequence"] {
        let body = sse(&[
            message_start(None),
            tool_start(0, "toolu_shape", "read_file"),
            block_stop(0),
            message_delta(stop_reason, None),
        ]);
        let (error, _) = run_error(body, "text/event-stream", tool_request()).await;
        assert_eq!(
            error.kind(),
            ModelErrorKind::InvalidProviderResponse,
            "{stop_reason}"
        );
    }

    for stop_reason in ["max_tokens", "content_filter"] {
        let body = sse(&[
            message_start(None),
            tool_start(0, "toolu_shape", "read_file"),
            block_stop(0),
            message_delta(stop_reason, None),
        ]);
        let (error, _) = run_error(body, "text/event-stream", tool_request()).await;
        assert_eq!(
            error.kind(),
            ModelErrorKind::IncompleteResponse,
            "{stop_reason}"
        );
    }

    let refusal_without_text = sse(&[message_start(None), message_delta("refusal", None)]);
    let (error, _) = run_error(
        refusal_without_text,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);

    let refusal_with_tool = sse(&[
        message_start(None),
        text_start(0),
        block_delta(0, json!({"type": "text_delta", "text": "refused"})),
        block_stop(0),
        tool_start(1, "toolu_shape", "read_file"),
        block_stop(1),
        message_delta("refusal", None),
    ]);
    let (error, _) = run_error(refusal_with_tool, "text/event-stream", tool_request()).await;
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);

    let tool_without_block = sse(&[message_start(None), message_delta("tool_use", None)]);
    let (error, _) = run_error(
        tool_without_block,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);

    let tool_only = sse(&[
        message_start(None),
        tool_start(0, "toolu_shape", "read_file"),
        block_stop(0),
        message_delta("tool_use", None),
    ]);
    let (result, _) = run_success(tool_only, tool_request()).await;
    assert_eq!(result.finish_reason(), ModelFinishReason::ToolCalls);

    let unknown_tool = sse(&[
        message_start(None),
        tool_start(0, "toolu_shape", "read_file"),
        block_stop(0),
        message_delta("custom_stop", None),
    ]);
    let (result, _) = run_success(unknown_tool, tool_request()).await;
    assert_eq!(result.finish_reason(), ModelFinishReason::Unknown);

    for stop_reason in ["max_tokens", "content_filter"] {
        let body = sse(&[message_start(None), message_delta(stop_reason, None)]);
        let (error, _) = run_error(
            body,
            "text/event-stream",
            request(ReasoningPreference::Auto),
        )
        .await;
        assert_eq!(
            error.kind(),
            ModelErrorKind::IncompleteResponse,
            "{stop_reason}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stream_error_codes_are_structural_and_delivery_safe() {
    let before_output = sse(&[
        message_start(None),
        json!({"type": "error", "error": {"type": "rate_limit_error", "message": "context_length_exceeded"}}),
    ]);
    let (error, _) = run_error(
        before_output,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::RequestOutcomeUnknown);
    assert_eq!(error.delivery(), DeliveryState::Unknown);

    let after_output = sse(&[
        message_start(None),
        text_start(0),
        block_delta(0, json!({"type": "text_delta", "text": "partial"})),
        json!({"type": "error", "error": {"type": "api_error", "message": "not inspected"}}),
    ]);
    let (error, _) = run_error(
        after_output,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::StreamInterrupted);
    assert_eq!(error.delivery(), DeliveryState::OutputStarted);

    let malformed = sse(&[
        message_start(None),
        json!({"type": "error", "error": {"message": "missing code"}}),
    ]);
    let (error, _) = run_error(
        malformed,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
    assert_eq!(error.delivery(), DeliveryState::AcceptedNoOutput);
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_ordering_wrong_content_type_eof_and_tool_arguments_fail_closed() {
    for body in [
        sse(&[text_start(0)]),
        sse(&[message_start(None), block_stop(0)]),
        {
            let mut start = message_start(None);
            start["message"]["model"] = json!("wrong-model");
            sse(&[start])
        },
        sse(&[
            message_start(None),
            text_start(0),
            block_delta(1, json!({"type": "text_delta", "text": "bad index"})),
        ]),
    ] {
        let (error, _) = run_error(
            body,
            "text/event-stream",
            request(ReasoningPreference::Auto),
        )
        .await;
        assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
    }
    let body = sse(&[
        message_start(None),
        text_start(0),
        block_delta(0, json!({"type": "text_delta", "text": "partial"})),
        block_stop(0),
    ]);
    let (error, _) = run_error(
        body,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::StreamInterrupted);
    assert_eq!(error.delivery(), DeliveryState::OutputStarted);

    let body = sse(&[
        message_start(None),
        tool_start(0, "toolu_1", "read_file"),
        block_delta(0, json!({"type": "input_json_delta", "partial_json": "[]"})),
        block_stop(0),
        message_delta("tool_use", None),
    ]);
    let (error, _) = run_error(
        body,
        "text/event-stream",
        request_with_tools(vec![
            ToolSpec::new(ToolName::from_str("read_file").unwrap(), "read", json!({})).unwrap(),
        ]),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);

    let (error, _) = run_error(
        text_terminal("ok", None),
        "application/json",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
}

#[tokio::test(flavor = "current_thread")]
async fn usage_grammar_requires_numeric_start_and_terminal_counters() {
    let mut invalid_starts = vec![message_start_without_usage()];
    for usage in [
        Value::Null,
        json!({}),
        json!({"input_tokens": 10}),
        json!({"input_tokens": "10", "output_tokens": 0}),
    ] {
        let mut start = message_start(None);
        start["message"]["usage"] = usage;
        invalid_starts.push(start);
    }
    for start in invalid_starts {
        let (error, _) = run_error(
            sse(&[start]),
            "text/event-stream",
            request(ReasoningPreference::Auto),
        )
        .await;
        assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
        assert_eq!(error.delivery(), DeliveryState::AcceptedNoOutput);
    }

    let mut invalid_terminals = vec![message_delta_without_usage("end_turn")];
    for usage in [
        Value::Null,
        json!({}),
        json!({"input_tokens": 10}),
        json!({"output_tokens": "3"}),
    ] {
        let mut terminal = message_delta("end_turn", None);
        terminal["usage"] = usage;
        invalid_terminals.push(terminal);
    }
    for terminal in invalid_terminals {
        let events = [
            message_start(Some(json!({"input_tokens": 10, "output_tokens": 1}))),
            text_start(0),
            block_delta(0, json!({"type": "text_delta", "text": "ok"})),
            block_stop(0),
            terminal,
        ];
        let (error, _) = run_error(
            sse(&events),
            "text/event-stream",
            request(ReasoningPreference::Auto),
        )
        .await;
        assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
        assert_eq!(error.delivery(), DeliveryState::OutputStarted);
    }

    let valid_terminal = message_delta(
        "end_turn",
        Some(json!({
            "output_tokens": 3,
            "input_tokens": null,
            "cache_read_input_tokens": null,
            "output_tokens_details": null,
        })),
    );
    let events = [
        message_start(Some(json!({
            "input_tokens": 10,
            "output_tokens": 1,
            "cache_read_input_tokens": null,
            "cache_creation_input_tokens": null,
            "output_tokens_details": null,
        }))),
        text_start(0),
        block_delta(0, json!({"type": "text_delta", "text": "ok"})),
        block_stop(0),
        valid_terminal,
    ];
    let (result, _) = run_success(sse(&events), request(ReasoningPreference::Auto)).await;
    let usage = result.usage().expect("valid usage is present");
    assert_eq!(usage.input_tokens(), Some(10));
    assert_eq!(usage.output_tokens(), Some(3));
    assert_eq!(usage.reasoning_tokens(), None);
    assert_eq!(usage.provider_total_tokens(), None);

    for terminal_usage in [
        json!({"output_tokens": 0}),
        json!({"output_tokens": 3, "input_tokens": 9}),
        json!({"output_tokens": 3, "output_tokens_details": {"thinking_tokens": 0}}),
    ] {
        let events = [
            message_start(Some(json!({
                "input_tokens": 10,
                "output_tokens": 1,
                "output_tokens_details": {"thinking_tokens": 1},
            }))),
            text_start(0),
            block_delta(0, json!({"type": "text_delta", "text": "ok"})),
            block_stop(0),
            message_delta("end_turn", Some(terminal_usage)),
        ];
        let (error, _) = run_error(
            sse(&events),
            "text/event-stream",
            request(ReasoningPreference::Auto),
        )
        .await;
        assert_eq!(error.kind(), ModelErrorKind::InvalidProviderResponse);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unexpected_tools_and_http_errors_are_structural_and_non_retrying() {
    let body = sse(&[message_start(None), tool_start(0, "toolu_1", "write_file")]);
    let (error, _) = run_error(
        body,
        "text/event-stream",
        request(ReasoningPreference::Auto),
    )
    .await;
    assert_eq!(error.kind(), ModelErrorKind::UnexpectedToolCall);
    assert_eq!(error.delivery(), DeliveryState::AcceptedNoOutput);

    let cases = [
        (
            400,
            r#"{"error":{"type":"invalid_request_error","message":"context length exceeded"}}"#,
            ModelErrorKind::InvalidRequest,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            400,
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"ordinary"}}"#,
            ModelErrorKind::ContextOverflow,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            401,
            r#"{"error":{"type":"authentication_error","message":"context_length_exceeded"}}"#,
            ModelErrorKind::AuthRejected,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            402,
            r#"{"error":{"type":"billing_error","message":"context_length_exceeded"}}"#,
            ModelErrorKind::QuotaExceeded,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            413,
            r#"{"error":{"type":"request_too_large","message":"ordinary"}}"#,
            ModelErrorKind::InvalidRequest,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            500,
            r#"{"error":{"type":"api_error","message":"context_length_exceeded"}}"#,
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
            None,
        ),
        (
            529,
            r#"{"error":{"type":"overloaded_error","message":"busy"}}"#,
            ModelErrorKind::ProviderUnavailable,
            DeliveryState::RejectedBeforeExecution,
            None,
        ),
        (
            529,
            r#"{"error":{"type":"api_error","message":"overloaded_error"}}"#,
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
            None,
        ),
    ];
    for (status, body, kind, delivery, retry) in cases {
        let server = LoopbackServer::single(status, "application/json", body);
        let gateway = make_gateway(provider(
            &server.endpoint,
            fixed_credential_source("sk-test").unwrap(),
        ));
        let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
        let error = gateway
            .generate(
                request(ReasoningPreference::Auto),
                ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), kind, "HTTP {status}");
        assert_eq!(error.delivery(), delivery, "HTTP {status}");
        assert_eq!(error.retry_after(), retry, "HTTP {status}");
        assert_eq!(server.join().len(), 1, "HTTP {status} must not retry");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn numeric_retry_after_is_preserved_without_retry() {
    let server = LoopbackServer::multi(vec![ScriptedResponse {
        status: 429,
        content_type: "application/json".to_owned(),
        headers: vec![("retry-after".to_owned(), "17".to_owned())],
        body: r#"{"error":{"type":"rate_limit_error"}}"#.to_owned(),
        fragmented: false,
        gate: None,
    }]);
    let endpoint = server.endpoint.clone();
    let provider = provider(&endpoint, fixed_credential_source("sk-test").unwrap());
    let gateway = make_gateway(provider);
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
    let error = gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::RateLimited);
    assert_eq!(error.retry_after(), Some(Duration::from_secs(17)));
    assert_eq!(server.join().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn retry_after_is_bounded_and_dynamic_credentials_are_resolved_per_attempt() {
    let body = text_terminal("ok", None);
    let server = LoopbackServer::multi(vec![
        ScriptedResponse {
            status: 429,
            content_type: "application/json".to_owned(),
            headers: vec![("retry-after".to_owned(), u64::MAX.to_string())],
            body: r#"{"error":{"type":"rate_limit_error"}}"#.to_owned(),
            fragmented: false,
            gate: None,
        },
        ScriptedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: Vec::new(),
            body,
            fragmented: false,
            gate: None,
        },
    ]);
    let credentials = Arc::new(Mutex::new(VecDeque::from([
        "sk-one".to_owned(),
        "sk-two".to_owned(),
    ])));
    let source: Arc<dyn CredentialSource> = Arc::new(RotatingCredentialSource(credentials));
    let gateway = make_gateway(provider(&server.endpoint, source));
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
    let first = gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(first.kind(), ModelErrorKind::RateLimited);
    assert_eq!(first.retry_after(), None);
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
    gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("x-api-key"), Some("sk-one"));
    assert_eq!(requests[1].header("x-api-key"), Some("sk-two"));
}

struct RotatingCredentialSource(Arc<Mutex<VecDeque<String>>>);

impl CredentialSource for RotatingCredentialSource {
    fn resolve(&self) -> CredentialSourceFuture<'_> {
        let value = self.0.lock().unwrap().pop_front();
        Box::pin(async move { value.and_then(|value| value.parse().ok()) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_send_and_after_delta_preserves_delivery() {
    let server = LoopbackServer::single(200, "text/event-stream", text_terminal("never", None));
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
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

    let body = sse(&[
        message_start(None),
        text_start(0),
        block_delta(0, json!({"type": "text_delta", "text": "partial"})),
        block_stop(0),
        message_delta("end_turn", None),
    ]);
    let (server, ready, release) = LoopbackServer::gated(body);
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let (sink, mut events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let gateway = gateway.clone();
        let cancellation = cancellation.clone();
        async move {
            gateway
                .generate(
                    request(ReasoningPreference::Auto),
                    ModelCallContext::new(cancellation, sink).unwrap(),
                )
                .await
        }
    });
    loop {
        match ready.try_recv() {
            Ok(()) => break,
            Err(TryRecvError::Empty) => tokio::task::yield_now().await,
            Err(TryRecvError::Disconnected) => panic!("server exited before event"),
        }
    }
    assert_eq!(
        events.recv().await,
        Some(ModelEvent::TextDelta {
            delta: "partial".to_owned()
        })
    );
    cancellation.cancel();
    release.send(()).unwrap();
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    assert_eq!(error.delivery(), DeliveryState::OutputStarted);
    assert_eq!(server.join().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn fragmented_stream_and_request_bound_are_safe() {
    let body = text_terminal("fragmented", None);
    let server = LoopbackServer::fragmented(body);
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
    gateway
        .generate(
            request(ReasoningPreference::Auto),
            ModelCallContext::new(CancellationToken::new(), sink).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(server.join().len(), 1);

    let server = LoopbackServer::single(200, "text/event-stream", text_terminal("never", None));
    let gateway = make_gateway(provider(
        &server.endpoint,
        fixed_credential_source("sk-test").unwrap(),
    ));
    let long = "x".repeat(262_144);
    let oversized = ModelRequest::new(
        ModelSelection::new(
            "anthropic".parse().unwrap(),
            "stable-model".parse().unwrap(),
        ),
        vec![
            ModelMessage::system(long.clone()).unwrap(),
            ModelMessage::system(long.clone()).unwrap(),
            ModelMessage::system(long.clone()).unwrap(),
            ModelMessage::system(long).unwrap(),
            ModelMessage::user("end").unwrap(),
        ],
        Vec::new(),
        ModelLimits::new(Some(128_000), Some(123)).unwrap(),
        ReasoningPreference::Auto,
    )
    .unwrap();
    let (sink, _events) = minicore_runtime::ModelEventSink::channel(8).unwrap();
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

#[test]
fn new_anthropic_provider_stays_model_owned_and_bounded() {
    let source = std::fs::read_to_string("src/model/providers/anthropic.rs").unwrap();
    for forbidden in [
        "crate::prompt",
        "crate::session",
        "crate::runtime",
        "crate::wire",
        "crate::tools::",
        "crate::http_transport",
        "crate::model_gateway",
        "tokio::spawn",
        "std::thread::spawn",
        "allow(dead_code",
        "set_hook",
    ] {
        assert!(
            !source.contains(forbidden),
            "forbidden dependency {forbidden}"
        );
    }
    assert!(source.lines().count() <= 2_000);
}
