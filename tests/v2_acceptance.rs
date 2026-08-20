use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use minicore_runtime::{
    AnthropicMessagesProvider, AssistantPart, DeliveryState, InteractionId, ListDirectoryTool,
    ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind, ModelEvent, ModelFinishReason,
    ModelFuture, ModelLimits, ModelMessage, ModelProvider, ModelRequest, ModelResponse,
    ModelSelection, OpenAiResponsesProvider, ProcessPolicy, ProgramPolicy, ProviderEndpointPolicy,
    ProviderId, ProviderRegistry, ReadFileTool, ReasoningPreference, RetryPolicy, RunCommandTool,
    Runtime, RuntimeConfig, SessionConfig, SessionError, SessionEvent, SessionEventStream,
    SessionId, SessionStatus, Tool, ToolCallId, ToolContext, ToolError, ToolFuture, ToolName,
    ToolOutput, ToolRegistry, TurnOutcome, Usage, UserAnswer, WriteFileTool,
    fixed_credential_source,
};
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

const ACCEPTANCE_CASES: [&str; 20] = [
    "AT-01 Model-only Turn",
    "AT-02 Read file",
    "AT-03 Edit file",
    "AT-04 Run tests",
    "AT-05 Multi-round tools",
    "AT-06 Ask user",
    "AT-07 Cancel model",
    "AT-08 Cancel process",
    "AT-09 Runtime restart",
    "AT-10 Partial JSONL",
    "AT-11 Compaction",
    "AT-12 Workspace security",
    "AT-13 Provider conformance",
    "AT-14 Session isolation",
    "AT-15 Event lag",
    "AT-16 Busy rule",
    "AT-17 Close",
    "AT-18 Custom Tool",
    "AT-19 Secret env",
    "AT-20 No legacy coupling",
];

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct ProviderState {
    steps: Arc<Mutex<VecDeque<ScriptStep>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    pending_started: Arc<AtomicBool>,
    cancellations: Arc<AtomicUsize>,
}

enum ScriptStep {
    Text {
        text: String,
        usage: Option<Usage>,
        deltas: Vec<String>,
    },
    Tool {
        name: ToolName,
        arguments: Value,
    },
    Error(ModelError),
    Pending,
    Stubborn,
}

struct ScriptedProvider {
    id: ProviderId,
    descriptor: ModelDescriptor,
    state: ProviderState,
    summary: Option<String>,
}

struct CancellationDropGuard {
    cancellation: CancellationToken,
    state: ProviderState,
    recorded: bool,
}

impl CancellationDropGuard {
    fn record(&mut self) {
        if !self.recorded {
            self.state.cancellations.fetch_add(1, Ordering::SeqCst);
            self.recorded = true;
        }
    }
}

impl Drop for CancellationDropGuard {
    fn drop(&mut self) {
        if self.cancellation.is_cancelled() {
            self.record();
        }
    }
}

impl ModelProvider for ScriptedProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        std::slice::from_ref(&self.descriptor)
    }

    fn generate(&self, request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'_> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request.clone());
        let last_user = request
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message {
                ModelMessage::User(text) => Some(text.as_str()),
                _ => None,
            });
        let step = if request.reasoning() == ReasoningPreference::Disabled
            && last_user
                == Some("Summarize the preceding conversation. Return only the summary text.")
        {
            self.summary
                .as_ref()
                .map(|text| ScriptStep::Text {
                    text: text.clone(),
                    usage: Some(Usage::new(1, 1, 0)),
                    deltas: Vec::new(),
                })
                .unwrap_or_else(|| ScriptStep::Error(ModelError::Internal))
        } else {
            self.state
                .steps
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
                .unwrap_or_else(|| ScriptStep::Text {
                    text: "fallback".to_owned(),
                    usage: Some(Usage::new(1, 1, 0)),
                    deltas: Vec::new(),
                })
        };
        let state = self.state.clone();
        Box::pin(async move {
            match step {
                ScriptStep::Text {
                    text,
                    usage,
                    deltas,
                } => {
                    for delta in deltas {
                        let _ = ctx.publish(ModelEvent::TextDelta { delta });
                    }
                    Ok(ModelResponse::new(
                        vec![AssistantPart::Text(text)],
                        ModelFinishReason::Stop,
                        usage,
                    )
                    .map_err(|_| ModelError::Internal)?)
                }
                ScriptStep::Tool { name, arguments } => {
                    let call = minicore_runtime::ToolCall::new(
                        ToolCallId::new("call-0").map_err(|_| ModelError::Internal)?,
                        name,
                        arguments,
                        0,
                    )
                    .map_err(|_| ModelError::InvalidRequest)?;
                    Ok(ModelResponse::new(
                        vec![AssistantPart::ToolCall(call)],
                        ModelFinishReason::ToolCalls,
                        Some(Usage::new(2, 1, 0)),
                    )
                    .map_err(|_| ModelError::Internal)?)
                }
                ScriptStep::Error(error) => Err(error),
                ScriptStep::Pending => {
                    state.pending_started.store(true, Ordering::SeqCst);
                    let mut guard = CancellationDropGuard {
                        cancellation: ctx.cancellation().clone(),
                        state,
                        recorded: false,
                    };
                    ctx.cancellation().cancelled().await;
                    guard.record();
                    Err(ModelError::detailed(
                        ModelErrorKind::Cancelled,
                        DeliveryState::NotSent,
                        None,
                    )
                    .unwrap_or(ModelError::Cancelled))
                }
                ScriptStep::Stubborn => {
                    state.pending_started.store(true, Ordering::SeqCst);
                    std::future::pending::<Result<ModelResponse, ModelError>>().await
                }
            }
        })
    }
}

fn selection(provider: &str, model: &str) -> ModelSelection {
    ModelSelection::new(provider.parse().unwrap(), model.parse().unwrap())
}

fn descriptor(selection: ModelSelection) -> ModelDescriptor {
    ModelDescriptor::new(
        selection,
        "scripted-api-model",
        ModelLimits::new(Some(1_024), Some(128)).unwrap(),
        BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
    )
    .unwrap()
}

fn text_step(text: &str) -> ScriptStep {
    ScriptStep::Text {
        text: text.to_owned(),
        usage: Some(Usage::new(3, 2, 1)),
        deltas: Vec::new(),
    }
}

fn delta_step(text: &str, deltas: &[&str]) -> ScriptStep {
    ScriptStep::Text {
        text: text.to_owned(),
        usage: Some(Usage::new(3, 2, 1)),
        deltas: deltas.iter().map(|delta| (*delta).to_owned()).collect(),
    }
}

fn tool_step(name: &str, arguments: Value) -> ScriptStep {
    ScriptStep::Tool {
        name: name.parse().unwrap(),
        arguments,
    }
}

fn provider(
    id: &str,
    steps: Vec<ScriptStep>,
    summary: Option<&str>,
) -> (ScriptedProvider, ProviderState) {
    let state = ProviderState {
        steps: Arc::new(Mutex::new(steps.into_iter().collect())),
        requests: Arc::new(Mutex::new(Vec::new())),
        pending_started: Arc::new(AtomicBool::new(false)),
        cancellations: Arc::new(AtomicUsize::new(0)),
    };
    let scripted = ScriptedProvider {
        id: id.parse().unwrap(),
        descriptor: descriptor(selection(id, "model")),
        state: state.clone(),
        summary: summary.map(str::to_owned),
    };
    (scripted, state)
}

fn provider_registry(providers: Vec<ScriptedProvider>) -> ProviderRegistry {
    let mut builder = ProviderRegistry::builder();
    for provider in providers {
        builder.register(provider).unwrap();
    }
    builder.build()
}

fn root(label: &str) -> (PathBuf, PathBuf) {
    let number = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "minicore-v2-acceptance-{label}-{}-{number}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    (root, workspace)
}

fn runtime_config(root: &Path, providers: ProviderRegistry, tools: ToolRegistry) -> RuntimeConfig {
    RuntimeConfig::new(
        root.to_path_buf(),
        providers,
        tools,
        "coding instructions",
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    )
    .unwrap()
}

fn runtime_config_with(
    root: &Path,
    providers: ProviderRegistry,
    tools: ToolRegistry,
    event_capacity: usize,
    shutdown_timeout: Duration,
) -> RuntimeConfig {
    RuntimeConfig::builder(
        root.to_path_buf(),
        providers,
        tools,
        "coding instructions",
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    )
    .capacities(event_capacity, 64, 64)
    .shutdown_timeout(shutdown_timeout)
    .build()
    .unwrap()
}

fn session_config(workspace: &Path, selection: ModelSelection, tools: &[&str]) -> SessionConfig {
    SessionConfig::new(
        workspace.to_path_buf(),
        selection,
        "system prompt",
        tools.iter().map(|tool| (*tool).parse().unwrap()).collect(),
        1_000,
        900,
        8,
    )
    .unwrap()
}

fn compact_session_config(
    workspace: &Path,
    selection: ModelSelection,
    trigger: u64,
    target: u64,
) -> SessionConfig {
    SessionConfig::new(
        workspace.to_path_buf(),
        selection,
        "system",
        BTreeSet::new(),
        trigger,
        target,
        4,
    )
    .unwrap()
}

async fn stream_for(runtime: &Runtime, id: SessionId) -> SessionEventStream {
    let mut stream = runtime.subscribe(id).unwrap();
    assert!(
        matches!(stream.recv().await, Some(SessionEvent::Snapshot(snapshot)) if snapshot.status() == SessionStatus::Idle)
    );
    stream
}

async fn event_matching<F>(stream: &mut SessionEventStream, mut matches: F) -> SessionEvent
where
    F: FnMut(&SessionEvent) -> bool,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = stream.recv().await.expect("session stream closed early");
            if matches(&event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out waiting for session event")
}

async fn finished(stream: &mut SessionEventStream) -> TurnOutcome {
    finished_event(stream).await.1
}

async fn finished_event(
    stream: &mut SessionEventStream,
) -> (minicore_runtime::TurnId, TurnOutcome) {
    match event_matching(stream, |event| {
        matches!(event, SessionEvent::TurnFinished { .. })
    })
    .await
    {
        SessionEvent::TurnFinished {
            turn_id, outcome, ..
        } => (turn_id, outcome),
        _ => unreachable!(),
    }
}

async fn wait_flag(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for provider state");
}

async fn wait_count(count: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while count.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for provider cancellations");
}

struct EchoTool;

struct ReadOnlyWriteProbe;

impl Tool for ReadOnlyWriteProbe {
    fn spec(&self) -> minicore_runtime::ToolSpec {
        minicore_runtime::ToolSpec::new(
            "write_probe".parse().unwrap(),
            "Attempt a write through the current workspace authority",
            json!({"type": "object"}),
        )
        .unwrap()
    }

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let path =
                minicore_runtime::RelativePath::new("new.txt").map_err(|_| ToolError::Internal)?;
            match ctx.workspace().write_text(&path, "forbidden").await {
                Ok(()) => ToolOutput::success("unexpected write success"),
                Err(_) => ToolOutput::failure("read-only workspace"),
            }
            .map_err(|_| ToolError::Internal)
        })
    }
}

impl Tool for EchoTool {
    fn spec(&self) -> minicore_runtime::ToolSpec {
        minicore_runtime::ToolSpec::new(
            "echo".parse().unwrap(),
            "Echo one JSON value",
            json!({"type": "object"}),
        )
        .unwrap()
    }

    fn execute<'a>(&'a self, _ctx: ToolContext<'a>, args: Value) -> ToolFuture<'a> {
        Box::pin(
            async move { ToolOutput::success(args.to_string()).map_err(|_| ToolError::Internal) },
        )
    }
}

fn enabled_tools(tools: &[&str]) -> ToolRegistry {
    let mut builder = ToolRegistry::builder();
    for tool in tools {
        match *tool {
            "read_file" => builder.register(ReadFileTool::new()).unwrap(),
            "list_directory" => builder.register(ListDirectoryTool::new()).unwrap(),
            "write_file" => builder.register(WriteFileTool::new()).unwrap(),
            "echo" => builder.register(EchoTool).unwrap(),
            "write_probe" => builder.register(ReadOnlyWriteProbe).unwrap(),
            _ => panic!("test tool is not registered: {tool}"),
        }
    }
    builder.build()
}

fn process_tools(policy: Arc<ProcessPolicy>) -> ToolRegistry {
    let mut builder = ToolRegistry::builder();
    builder.register(RunCommandTool::new(policy)).unwrap();
    builder.build()
}

fn recorded_requests(state: &ProviderState) -> Vec<ModelRequest> {
    state
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn request_tool_outputs(request: &ModelRequest) -> Vec<&str> {
    request
        .messages()
        .iter()
        .filter_map(|message| match message {
            ModelMessage::Tool { output, .. } => Some(output.text()),
            _ => None,
        })
        .collect()
}

fn request_tool_call_count(request: &ModelRequest) -> usize {
    request
        .messages()
        .iter()
        .flat_map(|message| match message {
            ModelMessage::Assistant(parts) => parts.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .filter(|part| matches!(part, AssistantPart::ToolCall(_)))
        .count()
}

fn request_user_texts(request: &ModelRequest) -> Vec<&str> {
    request
        .messages()
        .iter()
        .filter_map(|message| match message {
            ModelMessage::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result(entries: &[minicore_runtime::TranscriptEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            minicore_runtime::TranscriptEntry::ToolResult { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn last_assistant_text(entries: &[minicore_runtime::TranscriptEntry]) -> Option<&str> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            minicore_runtime::TranscriptEntry::Assistant { text, .. } => text.as_deref(),
            _ => None,
        })
        .next_back()
}

fn openai_sse(text: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_acceptance",
                "status": "completed",
                "model": "openai-model",
                "output": [{
                    "type": "message",
                    "id": "message_acceptance",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text}]
                }],
                "usage": {
                    "input_tokens": 3,
                    "output_tokens": 2,
                    "output_tokens_details": {"reasoning_tokens": 1},
                    "total_tokens": 5
                }
            }
        })
    )
}

fn openai_tool_sse() -> String {
    format!(
        "data: {}\n\n",
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_tool_acceptance",
                "status": "completed",
                "model": "openai-model",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_openai_tool",
                    "name": "echo",
                    "arguments": "{\"value\":\"provider\"}",
                    "status": "completed"
                }],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }
        })
    )
}

fn openai_cancel_sse() -> String {
    format!(
        "data: {}\n\n{}",
        json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "partial"
        }),
        openai_sse("cancelled")
    )
}

fn anthropic_sse(text: &str) -> String {
    let events = [
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_acceptance",
                "type": "message",
                "role": "assistant",
                "model": "anthropic-model",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 3, "output_tokens": 0}
            }
        }),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": text}
        }),
        json!({"type": "content_block_stop", "index": 0}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 2}
        }),
    ];
    let mut output = String::new();
    for event in events {
        write!(&mut output, "data: {event}\n\n").unwrap();
    }
    output
}

fn anthropic_tool_sse() -> String {
    let events = [
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_tool_acceptance",
                "type": "message",
                "role": "assistant",
                "model": "anthropic-model",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 3, "output_tokens": 0}
            }
        }),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "tool_use",
                "id": "call_anthropic_tool",
                "name": "echo",
                "input": {"value": "provider"}
            }
        }),
        json!({"type": "content_block_stop", "index": 0}),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use", "stop_sequence": null},
            "usage": {"output_tokens": 2}
        }),
    ];
    let mut output = String::new();
    for event in events {
        write!(&mut output, "data: {event}\n\n").unwrap();
    }
    output
}

#[derive(Clone)]
struct CapturedHttpRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

type RequestLog = Arc<Mutex<Vec<CapturedHttpRequest>>>;

fn capture_http_request(stream: &mut std::net::TcpStream) -> Option<CapturedHttpRequest> {
    const MAX_REQUEST_BYTES: usize = 1_048_576;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..count]);
        if request.len() > MAX_REQUEST_BYTES {
            return None;
        }
    };
    let head = std::str::from_utf8(&request[..header_end - 4]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next()?.to_owned();
    let target = request_parts.next()?.to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())?;
    let body_end = header_end.checked_add(content_length)?;
    if body_end > MAX_REQUEST_BYTES {
        return None;
    }
    while request.len() < body_end {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..count]);
        if request.len() > MAX_REQUEST_BYTES {
            return None;
        }
    }
    Some(CapturedHttpRequest {
        method,
        target,
        headers,
        body: request[header_end..body_end].to_vec(),
    })
}

fn request_log(server: &LoopbackServer) -> Vec<CapturedHttpRequest> {
    server
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn request_json(request: &CapturedHttpRequest) -> Value {
    serde_json::from_slice(&request.body).expect("captured provider body is JSON")
}

fn assert_captured_request_shape(request: &CapturedHttpRequest) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/messages");
    assert_eq!(
        request
            .headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok()),
        Some(request.body.len())
    );
    assert!(request.headers.contains_key("content-type"));
}

fn assert_header(request: &CapturedHttpRequest, name: &str, expected: &str) {
    assert!(
        request
            .headers
            .get(name)
            .is_some_and(|actual| actual == expected),
        "unexpected {name} header"
    );
}

fn assert_no_request_keys(value: &Value, forbidden: &[&str]) {
    match value {
        Value::Object(object) => {
            for key in forbidden {
                assert!(
                    !object.contains_key(*key),
                    "forbidden provider field: {key}"
                );
            }
            for value in object.values() {
                assert_no_request_keys(value, forbidden);
            }
        }
        Value::Array(values) => {
            for value in values {
                assert_no_request_keys(value, forbidden);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

struct LoopbackServer {
    address: std::net::SocketAddr,
    handle: Option<JoinHandle<()>>,
    release: Option<std::sync::mpsc::Sender<()>>,
    requests: RequestLog,
}

impl LoopbackServer {
    fn sequence(bodies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let Some(request) = capture_http_request(&mut stream) else {
                    return;
                };
                captured
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.shutdown(Shutdown::Write);
            }
        });
        Self {
            address,
            handle: Some(handle),
            release: None,
            requests,
        }
    }

    fn status(status: u16, content_type: &str, body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let content_type = content_type.to_owned();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Some(request) = capture_http_request(&mut stream) else {
                return;
            };
            captured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.shutdown(Shutdown::Write);
        });
        Self {
            address,
            handle: Some(handle),
            release: None,
            requests,
        }
    }

    fn gated(body: String) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Some(request) = capture_http_request(&mut stream) else {
                return;
            };
            captured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = ready_tx.send(());
            let _ = release_rx.recv();
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.shutdown(Shutdown::Write);
        });
        (
            Self {
                address,
                handle: Some(handle),
                release: Some(release_tx),
                requests,
            },
            ready_rx,
        )
    }

    fn release(&mut self) {
        if let Some(sender) = self.release.take() {
            let _ = sender.send(());
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/messages", self.address)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.release();
        if let Some(handle) = self.handle.take() {
            let _ = std::net::TcpStream::connect(self.address);
            let _ = handle.join();
        }
    }
}

#[test]
fn acceptance_inventory_is_complete() {
    assert_eq!(ACCEPTANCE_CASES.len(), 20);
    for (index, case) in ACCEPTANCE_CASES.iter().enumerate() {
        let expected = format!("AT-{:02}", index + 1);
        assert!(
            case.starts_with(&expected),
            "missing acceptance case: {expected}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_01_model_only_turn() {
    let (root, workspace) = root("at01");
    let (scripted, _) = provider("scripted", vec![delta_step("done", &["de", "lta"])], None);
    let config = runtime_config(
        &root,
        provider_registry(vec![scripted]),
        ToolRegistry::default(),
    );
    let reopen_config = runtime_config(
        &root,
        provider_registry(vec![provider("scripted", Vec::new(), None).0]),
        ToolRegistry::default(),
    );
    let runtime = Runtime::open(config, Handle::current()).await.unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "hello".to_owned()).await.unwrap();
    assert!(matches!(
        event_matching(&mut stream, |event| matches!(event, SessionEvent::TextDelta { .. })).await,
        SessionEvent::TextDelta { delta, .. } if delta == "de"
    ));
    assert!(matches!(
        event_matching(&mut stream, |event| matches!(event, SessionEvent::TextDelta { .. })).await,
        SessionEvent::TextDelta { delta, .. } if delta == "lta"
    ));
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert!(page.entries().iter().any(|entry| matches!(
        entry,
        minicore_runtime::TranscriptEntry::Assistant { text: Some(text), .. } if text == "done"
    )));
    drop(stream);
    drop(page);
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let reopened = Runtime::open(reopen_config, Handle::current())
        .await
        .unwrap();
    reopened.load_session(id).await.unwrap();
    assert_eq!(
        reopened
            .transcript(id, None, 200)
            .await
            .unwrap()
            .entries()
            .len(),
        3
    );
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_02_read_file() {
    let (root, workspace) = root("at02");
    fs::write(workspace.join("note.txt"), "exact file content").unwrap();
    let (scripted, state) = provider(
        "scripted",
        vec![
            tool_step("read_file", json!({"path": "note.txt"})),
            text_step("read complete"),
        ],
        None,
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            enabled_tools(&["read_file"]),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["read_file"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "read it".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert!(tool_result(page.entries()).contains(&"exact file content"));
    assert_eq!(last_assistant_text(page.entries()), Some("read complete"));
    let requests = recorded_requests(&state);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        request_tool_outputs(&requests[1]),
        vec!["exact file content"]
    );
    assert_eq!(request_tool_call_count(&requests[0]), 0);
    assert_eq!(request_tool_call_count(&requests[1]), 1);
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_03_edit_file() {
    let (root, workspace) = root("at03");
    let outside = root.join("outside.txt");
    fs::write(&outside, "outside-original").unwrap();
    let (scripted, _) = provider(
        "scripted",
        vec![
            tool_step(
                "write_file",
                json!({"path": "edited.txt", "content": "new bytes"}),
            ),
            text_step("write complete"),
            tool_step(
                "write_file",
                json!({"path": "../outside.txt", "content": "escape"}),
            ),
            text_step("escape rejected"),
        ],
        None,
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            enabled_tools(&["write_file"]),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["write_file"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "edit".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    runtime.submit(id, "try escape".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    assert_eq!(
        fs::read_to_string(workspace.join("edited.txt")).unwrap(),
        "new bytes"
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside-original");
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert!(page.entries().iter().any(|entry| {
        matches!(
            entry,
            minicore_runtime::TranscriptEntry::ToolResult { is_error: true, .. }
        )
    }));
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_04_run_tests() {
    let (root, workspace) = root("at04");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"acceptance_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "#[test]\nfn works() { println!(\"acceptance cargo evidence\"); }\n",
    )
    .unwrap();
    let (scripted, _) = provider(
        "scripted",
        vec![
            tool_step(
                "run_command",
                json!({"program": "cargo", "args": ["test", "--quiet", "--", "--nocapture"]}),
            ),
            text_step("tests complete"),
        ],
        None,
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            process_tools(Arc::new(ProcessPolicy::coding_agent_local())),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["run_command"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "run tests".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    let output = tool_result(page.entries())
        .into_iter()
        .find(|text| text.contains("exit_code"))
        .expect("run_command output");
    let output: Value = serde_json::from_str(output).unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["timed_out"], false);
    assert!(output["stdout"].is_string());
    assert!(output["stderr"].is_string());
    assert!(
        output["stdout"]
            .as_str()
            .unwrap()
            .contains("acceptance cargo evidence")
    );
    assert_eq!(output["stderr"].as_str(), Some(""));
    assert_eq!(last_assistant_text(page.entries()), Some("tests complete"));
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_05_multi_round_tools() {
    let (root, workspace) = root("at05");
    let (scripted, state) = provider(
        "scripted",
        vec![
            tool_step("echo", json!({"round": 1})),
            tool_step("echo", json!({"round": 2})),
            text_step("all rounds complete"),
        ],
        None,
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            enabled_tools(&["echo"]),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["echo"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "two rounds".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    let results = tool_result(page.entries());
    assert_eq!(results.len(), 2);
    assert!(results[0].contains("\"round\":1"));
    assert!(results[1].contains("\"round\":2"));
    assert_eq!(
        last_assistant_text(page.entries()),
        Some("all rounds complete")
    );
    let requests = recorded_requests(&state);
    assert_eq!(requests.len(), 3);
    assert_eq!(request_tool_call_count(&requests[0]), 0);
    assert_eq!(request_tool_call_count(&requests[1]), 1);
    assert_eq!(request_tool_outputs(&requests[1]), vec!["{\"round\":1}"]);
    assert_eq!(request_tool_call_count(&requests[2]), 2);
    assert_eq!(
        request_tool_outputs(&requests[2]),
        vec!["{\"round\":1}", "{\"round\":2}"]
    );
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_06_ask_user() {
    let (root, workspace) = root("at06");
    let (scripted, state) = provider(
        "scripted",
        vec![
            tool_step("ask_user", json!({"question": "Allow?"})),
            text_step("approved"),
        ],
        None,
    );
    let mut tools = ToolRegistry::builder();
    tools.register(minicore_runtime::AskUserTool).unwrap();
    let runtime = Runtime::open(
        runtime_config(&root, provider_registry(vec![scripted]), tools.build()),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["ask_user"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "ask".to_owned()).await.unwrap();
    let question = event_matching(&mut stream, |event| {
        matches!(event, SessionEvent::InputRequested { .. })
    })
    .await;
    let interaction_id = match question {
        SessionEvent::InputRequested { question, .. } => question.interaction_id(),
        _ => unreachable!(),
    };
    assert_eq!(
        runtime
            .answer(
                id,
                InteractionId::new().unwrap(),
                UserAnswer::new("allow").unwrap(),
            )
            .await,
        Err(SessionError::InteractionMismatch)
    );
    runtime
        .answer(id, interaction_id, UserAnswer::new("allow").unwrap())
        .await
        .unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert_eq!(last_assistant_text(page.entries()), Some("approved"));
    let requests = recorded_requests(&state);
    assert_eq!(requests.len(), 2);
    assert_eq!(request_tool_call_count(&requests[0]), 0);
    assert_eq!(request_tool_call_count(&requests[1]), 1);
    assert_eq!(request_tool_outputs(&requests[1]), vec!["allow"]);
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_07_cancel_model() {
    let (root, workspace) = root("at07");
    let (scripted, state) = provider("scripted", vec![ScriptStep::Pending], None);
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "cancel me".to_owned()).await.unwrap();
    wait_flag(&state.pending_started).await;
    runtime.cancel(id).unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Cancelled);
    wait_count(&state.cancellations, 1).await;
    assert_eq!(runtime.snapshot(id).unwrap().status(), SessionStatus::Idle);
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

fn helper_policy(allowed_env: &[&str]) -> Arc<ProcessPolicy> {
    Arc::new(
        ProcessPolicy::new(
            true,
            ProgramPolicy::allow_list([std::env::current_exe().unwrap().to_string_lossy()])
                .unwrap(),
            false,
            allowed_env
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        )
        .unwrap(),
    )
}

fn helper_args(mode: &str, marker: Option<&Path>) -> Value {
    let mut env = serde_json::Map::new();
    env.insert("MINICORE_V2_HELPER_MODE".to_owned(), json!(mode));
    if let Some(marker) = marker {
        env.insert(
            "MINICORE_V2_HELPER_MARKER".to_owned(),
            json!(marker.to_string_lossy()),
        );
    }
    json!({
        "program": std::env::current_exe().unwrap().to_string_lossy(),
        "args": ["--exact", "at_08_cancel_process", "--nocapture"],
        "env": env,
    })
}

fn present_host_environment() -> (&'static str, String) {
    let candidates = if cfg!(windows) {
        ["SYSTEMROOT", "PATH", "TEMP", "TMP", "HOME"]
    } else {
        ["HOME", "PATH", "TMPDIR", "USER", "SHELL"]
    };
    candidates
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| (key, value))
        })
        .expect("at least one standard host environment variable must be present")
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/NH"])
        .output()
        .is_ok_and(|output| !String::from_utf8_lossy(&output.stdout).contains("No tasks"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_08_cancel_process() {
    if let Ok(mode) = std::env::var("MINICORE_V2_HELPER_MODE") {
        match mode.as_str() {
            "sleep" => {
                if let Ok(marker) = std::env::var("MINICORE_V2_HELPER_MARKER") {
                    let _ = fs::write(marker, std::process::id().to_string());
                }
                std::thread::sleep(Duration::from_secs(30));
            }
            "env" => {
                let key = std::env::var("MINICORE_V2_HOST_KEY")
                    .expect("host environment selector is present");
                println!("HOST_KEY={key:?};HOST_VALUE={:?}", std::env::var(&key));
            }
            _ => {}
        }
        return;
    }
    let (root, workspace) = root("at08");
    let marker = root.join("child-started");
    let (scripted, _) = provider(
        "scripted",
        vec![
            tool_step("run_command", helper_args("sleep", Some(&marker))),
            text_step("unreachable"),
        ],
        None,
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            process_tools(helper_policy(&[
                "MINICORE_V2_HELPER_MODE",
                "MINICORE_V2_HELPER_MARKER",
            ])),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["run_command"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime
        .submit(id, "run and cancel".to_owned())
        .await
        .unwrap();
    event_matching(&mut stream, |event| {
        matches!(event, SessionEvent::ToolStarted { .. })
    })
    .await;
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = fs::read_to_string(&marker) {
                if let Ok(pid) = value.parse::<u32>() {
                    break pid;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child did not start");
    runtime.cancel(id).unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Cancelled);
    tokio::time::timeout(Duration::from_secs(5), async {
        while process_is_alive(pid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("direct child remained alive after cancellation");
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_09_runtime_restart() {
    let (root, workspace) = root("at09");
    let (scripted, _state) = provider(
        "scripted",
        vec![
            text_step("first history"),
            text_step("second history"),
            text_step("third history"),
        ],
        Some("COMPACT SUMMARY"),
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(compact_session_config(
            &workspace,
            selection("scripted", "model"),
            300,
            250,
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    for input in [
        "first history ".repeat(40),
        "second history ".repeat(40),
        "third history ".repeat(40),
    ] {
        runtime.submit(id, input).await.unwrap();
        assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    }
    let original_page = runtime.transcript(id, None, 200).await.unwrap();
    let original_entries = original_page.entries().to_vec();
    assert!(original_entries.iter().any(|entry| {
        matches!(entry, minicore_runtime::TranscriptEntry::Summary { text, .. } if text == "COMPACT SUMMARY")
    }));
    let original_terminals = original_entries
        .iter()
        .filter_map(|entry| match entry {
            minicore_runtime::TranscriptEntry::Terminal {
                turn_id, outcome, ..
            } => Some((*turn_id, outcome.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(original_terminals.len(), 3);
    assert!(
        original_terminals
            .iter()
            .all(|(_, outcome)| *outcome == TurnOutcome::Completed)
    );
    let original_usage = *runtime.snapshot(id).unwrap().usage();
    let original_seq = runtime.snapshot(id).unwrap().conversation_seq();
    assert_eq!(
        original_seq,
        original_entries.last().map_or(0, |entry| match entry {
            minicore_runtime::TranscriptEntry::Terminal { seq, .. }
            | minicore_runtime::TranscriptEntry::Summary { seq, .. }
            | minicore_runtime::TranscriptEntry::User { seq, .. }
            | minicore_runtime::TranscriptEntry::Assistant { seq, .. }
            | minicore_runtime::TranscriptEntry::ToolResult { seq, .. }
            | minicore_runtime::TranscriptEntry::Interaction { seq, .. } => *seq,
        })
    );
    drop(stream);
    drop(original_page);
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let (scripted, _) = provider("scripted", vec![text_step("unused")], None);
    let reopened = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    reopened.load_session(id).await.unwrap();
    let reopened_page = reopened.transcript(id, None, 200).await.unwrap();
    assert_eq!(reopened_page.entries(), original_entries.as_slice());
    let reopened_snapshot = reopened.snapshot(id).unwrap();
    assert_eq!(reopened_snapshot.usage(), &original_usage);
    assert_eq!(reopened_snapshot.conversation_seq(), original_seq);
    let reopened_terminals = reopened_page
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            minicore_runtime::TranscriptEntry::Terminal {
                turn_id, outcome, ..
            } => Some((*turn_id, outcome.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reopened_terminals, original_terminals);
    assert!(reopened_page.entries().iter().any(|entry| {
        matches!(entry, minicore_runtime::TranscriptEntry::Summary { text, .. } if text == "COMPACT SUMMARY")
    }));
    drop(reopened_page);
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn at_10_partial_jsonl() {
    let (root_path, workspace) = root("at10");
    let (scripted, _) = provider("scripted", vec![text_step("durable")], None);
    let runtime = Runtime::open(
        runtime_config(
            &root_path,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "durable".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let original_page = runtime.transcript(id, None, 200).await.unwrap();
    let original_entries = original_page.entries().to_vec();
    drop(stream);
    drop(original_page);
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let conversation = root_path
        .join("sessions")
        .join(id.to_string())
        .join("conversation.jsonl");
    let original_bytes = fs::read(&conversation).unwrap();
    assert!(original_bytes.ends_with(b"\n"));
    OpenOptions::new()
        .append(true)
        .open(&conversation)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    assert!(
        fs::read(&conversation)
            .unwrap()
            .starts_with(&original_bytes)
    );
    let (scripted, _) = provider("scripted", Vec::new(), None);
    let reopened = Runtime::open(
        runtime_config(
            &root_path,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    reopened.load_session(id).await.unwrap();
    assert_eq!(fs::read(&conversation).unwrap(), original_bytes);
    assert_eq!(
        reopened.transcript(id, None, 200).await.unwrap().entries(),
        original_entries.as_slice()
    );
    reopened.shutdown().await.unwrap();
    drop(reopened);

    let (root_two, workspace_two) = root("at10-corrupt");
    let (scripted, _) = provider("scripted", vec![text_step("corruptible")], None);
    let runtime_two = Runtime::open(
        runtime_config(
            &root_two,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let corrupt_id = runtime_two
        .create_session(session_config(
            &workspace_two,
            selection("scripted", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime_two, corrupt_id).await;
    runtime_two
        .submit(corrupt_id, "corrupt".to_owned())
        .await
        .unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    drop(stream);
    runtime_two.shutdown().await.unwrap();
    drop(runtime_two);
    let corrupt_file = root_two
        .join("sessions")
        .join(corrupt_id.to_string())
        .join("conversation.jsonl");
    let original = fs::read_to_string(&corrupt_file).unwrap();
    let mut lines = original.lines();
    let first = lines.next().unwrap();
    let rest = lines.collect::<Vec<_>>().join("\n");
    fs::write(&corrupt_file, format!("{first}\n{{bad}}\n{rest}\n")).unwrap();
    let (scripted, _) = provider("scripted", Vec::new(), None);
    let reopened = Runtime::open(
        runtime_config(
            &root_two,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened.load_session(corrupt_id).await,
        Err(SessionError::Internal)
    );
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root_path).unwrap();
    fs::remove_dir_all(root_two).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_11_compaction() {
    let (root, workspace) = root("at11");
    let (scripted, state) = provider(
        "scripted",
        vec![
            text_step("first history"),
            text_step("second history"),
            text_step("after summary"),
        ],
        Some("COMPACT SUMMARY"),
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(compact_session_config(
            &workspace,
            selection("scripted", "model"),
            300,
            250,
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    for input in [
        "first history ".repeat(40),
        "second history ".repeat(40),
        "trigger compaction ".repeat(40),
    ] {
        runtime.submit(id, input).await.unwrap();
        assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    }
    let requests = state
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let summary_index = requests
        .iter()
        .position(|request| {
            request.reasoning() == ReasoningPreference::Disabled
                && request.messages().iter().any(|message| {
                    matches!(message, ModelMessage::User(text) if text.contains("Summarize the preceding"))
                })
        })
        .expect("compaction summary request");
    let ordinary_after = requests[summary_index + 1..]
        .iter()
        .find(|request| request.reasoning() == ReasoningPreference::Auto)
        .expect("ordinary request after summary");
    assert!(ordinary_after.messages().iter().any(|message| {
        matches!(message, ModelMessage::User(text) if text == "COMPACT SUMMARY")
    }));
    assert!(!ordinary_after.messages().iter().any(|message| {
        matches!(message, ModelMessage::User(text) if text.starts_with("first history"))
    }));
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert!(
        page.entries()
            .iter()
            .filter(|entry| { matches!(entry, minicore_runtime::TranscriptEntry::User { .. }) })
            .count()
            >= 3
    );
    assert!(page.entries().iter().any(|entry| {
        matches!(entry, minicore_runtime::TranscriptEntry::Summary { text, .. } if text == "COMPACT SUMMARY")
    }));
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[cfg(any(unix, windows))]
fn symlink_file(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(target, link).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_12_workspace_security() {
    let (root, workspace) = root("at12");
    let outside = root.join("outside.txt");
    fs::write(&outside, "secret outside").unwrap();
    #[cfg(any(unix, windows))]
    symlink_file(&outside, &workspace.join("link.txt"));
    let (scripted, _) = provider(
        "scripted",
        vec![
            tool_step("read_file", json!({"path": "../outside.txt"})),
            text_step("traversal checked"),
            tool_step("read_file", json!({"path": "/tmp/outside.txt"})),
            text_step("absolute checked"),
            tool_step("read_file", json!({"path": "//outside.txt"})),
            text_step("double slash checked"),
            tool_step("read_file", json!({"path": "link.txt"})),
            text_step("symlink checked"),
            tool_step("write_probe", json!({})),
            text_step("read-only checked"),
        ],
        None,
    );
    let runtime = Runtime::open(
        runtime_config(&root, provider_registry(vec![scripted]), {
            let mut tools = ToolRegistry::builder();
            tools.register(ReadFileTool::new()).unwrap();
            tools.register(ReadOnlyWriteProbe).unwrap();
            tools.build()
        }),
        Handle::current(),
    )
    .await
    .unwrap();
    let readonly_id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["read_file", "write_probe"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, readonly_id).await;
    runtime
        .submit(readonly_id, "security".to_owned())
        .await
        .unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    runtime
        .submit(readonly_id, "absolute".to_owned())
        .await
        .unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    runtime
        .submit(readonly_id, "double slash".to_owned())
        .await
        .unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    runtime
        .submit(readonly_id, "symlink".to_owned())
        .await
        .unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    runtime
        .submit(readonly_id, "write readonly".to_owned())
        .await
        .unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    assert!(!workspace.join("new.txt").exists());
    let page = runtime.transcript(readonly_id, None, 200).await.unwrap();
    let security_results = page
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            minicore_runtime::TranscriptEntry::ToolResult { text, is_error, .. } => {
                Some((text.as_str(), *is_error))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(security_results.len(), 5);
    assert!(security_results.iter().all(|(_, is_error)| *is_error));
    assert_eq!(
        security_results,
        vec![
            ("tool arguments are invalid", true),
            ("tool arguments are invalid", true),
            ("tool arguments are invalid", true),
            ("file could not be read", true),
            ("read-only workspace", true),
        ]
    );
    assert!(
        security_results
            .iter()
            .all(|(text, _)| !text.contains("secret outside"))
    );
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_13_provider_conformance() {
    let openai_server = LoopbackServer::sequence(vec![
        openai_sse("openai text"),
        openai_tool_sse(),
        openai_sse("openai after tool"),
    ]);
    let anthropic_server = LoopbackServer::sequence(vec![
        anthropic_sse("anthropic text"),
        anthropic_tool_sse(),
        anthropic_sse("anthropic after tool"),
    ]);
    let openai_selection = selection("openai", "model");
    let anthropic_selection = selection("anthropic", "model");
    let openai_descriptor = ModelDescriptor::new(
        openai_selection.clone(),
        "openai-model",
        ModelLimits::new(Some(1_024), Some(128)).unwrap(),
        BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
    )
    .unwrap();
    let anthropic_descriptor = ModelDescriptor::new(
        anthropic_selection.clone(),
        "anthropic-model",
        ModelLimits::new(Some(1_024), Some(128)).unwrap(),
        BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
    )
    .unwrap();
    let openai = OpenAiResponsesProvider::new(
        &openai_server.endpoint(),
        ProviderEndpointPolicy::AllowLoopbackHttp,
        fixed_credential_source("openai-secret").unwrap(),
        vec![openai_descriptor],
    )
    .unwrap();
    let anthropic = AnthropicMessagesProvider::new(
        &anthropic_server.endpoint(),
        ProviderEndpointPolicy::AllowLoopbackHttp,
        "2023-06-01",
        fixed_credential_source("anthropic-secret").unwrap(),
        vec![anthropic_descriptor],
    )
    .unwrap();
    let mut providers = ProviderRegistry::builder();
    providers.register(openai).unwrap();
    providers.register(anthropic).unwrap();
    let (root_path, workspace) = root("at13");
    let runtime = Runtime::open(
        runtime_config(&root_path, providers.build(), enabled_tools(&["echo"])),
        Handle::current(),
    )
    .await
    .unwrap();
    let openai_id = runtime
        .create_session(session_config(&workspace, openai_selection, &[]))
        .await
        .unwrap();
    let mut openai_events = stream_for(&runtime, openai_id).await;
    runtime
        .submit(openai_id, "openai".to_owned())
        .await
        .unwrap();
    assert_eq!(finished(&mut openai_events).await, TurnOutcome::Completed);
    assert!(
        runtime
            .snapshot(openai_id)
            .unwrap()
            .usage()
            .input_tokens()
            .is_some()
    );
    assert!(
        runtime
            .snapshot(openai_id)
            .unwrap()
            .usage()
            .output_tokens()
            .is_some()
    );
    let anthropic_workspace = root_path.join("anthropic-workspace");
    fs::create_dir_all(&anthropic_workspace).unwrap();
    let anthropic_id = runtime
        .create_session(session_config(
            &anthropic_workspace,
            anthropic_selection,
            &[],
        ))
        .await
        .unwrap();
    let mut anthropic_events = stream_for(&runtime, anthropic_id).await;
    runtime
        .submit(anthropic_id, "anthropic".to_owned())
        .await
        .unwrap();
    assert_eq!(
        finished(&mut anthropic_events).await,
        TurnOutcome::Completed
    );
    assert!(
        runtime
            .snapshot(anthropic_id)
            .unwrap()
            .usage()
            .input_tokens()
            .is_some()
    );
    assert!(
        runtime
            .snapshot(anthropic_id)
            .unwrap()
            .usage()
            .output_tokens()
            .is_some()
    );
    assert!(runtime
        .transcript(openai_id, None, 200)
        .await
        .unwrap()
        .entries()
        .iter()
        .any(|entry| matches!(entry, minicore_runtime::TranscriptEntry::Assistant { text: Some(text), .. } if text == "openai text")));
    assert!(runtime
        .transcript(anthropic_id, None, 200)
        .await
        .unwrap()
        .entries()
        .iter()
        .any(|entry| matches!(entry, minicore_runtime::TranscriptEntry::Assistant { text: Some(text), .. } if text == "anthropic text")));

    let openai_tool_workspace = root_path.join("openai-tool-workspace");
    fs::create_dir_all(&openai_tool_workspace).unwrap();
    let openai_tool_id = runtime
        .create_session(session_config(
            &openai_tool_workspace,
            selection("openai", "model"),
            &["echo"],
        ))
        .await
        .unwrap();
    let mut openai_tool_events = stream_for(&runtime, openai_tool_id).await;
    runtime
        .submit(openai_tool_id, "openai tool".to_owned())
        .await
        .unwrap();
    assert_eq!(
        finished(&mut openai_tool_events).await,
        TurnOutcome::Completed
    );
    assert!(
        tool_result(
            runtime
                .transcript(openai_tool_id, None, 200)
                .await
                .unwrap()
                .entries()
        )
        .iter()
        .any(|text| text.contains("provider"))
    );

    let anthropic_tool_workspace = root_path.join("anthropic-tool-workspace");
    fs::create_dir_all(&anthropic_tool_workspace).unwrap();
    let anthropic_tool_id = runtime
        .create_session(session_config(
            &anthropic_tool_workspace,
            selection("anthropic", "model"),
            &["echo"],
        ))
        .await
        .unwrap();
    let mut anthropic_tool_events = stream_for(&runtime, anthropic_tool_id).await;
    runtime
        .submit(anthropic_tool_id, "anthropic tool".to_owned())
        .await
        .unwrap();
    assert_eq!(
        finished(&mut anthropic_tool_events).await,
        TurnOutcome::Completed
    );
    assert!(
        tool_result(
            runtime
                .transcript(anthropic_tool_id, None, 200)
                .await
                .unwrap()
                .entries()
        )
        .iter()
        .any(|text| text.contains("provider"))
    );

    let openai_requests = request_log(&openai_server);
    assert_eq!(openai_requests.len(), 3);
    for request in &openai_requests {
        assert_captured_request_shape(request);
        assert_header(request, "authorization", "Bearer openai-secret");
        assert_header(request, "accept", "text/event-stream");
        let body = request_json(request);
        assert_eq!(body["model"], "openai-model");
        assert_eq!(body["store"], false);
        assert_no_request_keys(
            &body,
            &[
                "previous_response_id",
                "conversation",
                "continuation",
                "continue_from",
                "prompt_cache_key",
                "prompt_cache_retention",
                "cache",
                "cache_read",
                "cache_write",
                "cache_creation",
                "cache_control",
            ],
        );
    }
    let openai_first = request_json(&openai_requests[0]);
    assert!(openai_first["tools"].is_null());
    assert!(openai_first["input"].as_array().is_some_and(|input| {
        input.iter().any(|item| {
            item["type"] == "message"
                && item["role"] == "user"
                && item["content"][0]["text"] == "openai"
        })
    }));
    let openai_tool_first = request_json(&openai_requests[1]);
    assert_eq!(openai_tool_first["tools"][0]["type"], "function");
    assert_eq!(openai_tool_first["tools"][0]["name"], "echo");
    assert!(openai_tool_first["tools"][0]["parameters"].is_object());
    let openai_tool_second = request_json(&openai_requests[2]);
    let openai_input = openai_tool_second["input"]
        .as_array()
        .expect("OpenAI follow-up input array");
    assert!(
        openai_input
            .iter()
            .any(|item| item["type"] == "function_call")
    );
    assert!(openai_input.iter().any(|item| {
        item["type"] == "function_call_output"
            && item["call_id"] == "call_openai_tool"
            && item["output"]
                .as_str()
                .is_some_and(|text| text.contains("provider"))
    }));

    let anthropic_requests = request_log(&anthropic_server);
    assert_eq!(anthropic_requests.len(), 3);
    for request in &anthropic_requests {
        assert_captured_request_shape(request);
        assert_header(request, "x-api-key", "anthropic-secret");
        assert_header(request, "anthropic-version", "2023-06-01");
        assert!(!request.headers.contains_key("anthropic-beta"));
        let body = request_json(request);
        assert_eq!(body["model"], "anthropic-model");
        assert_eq!(body["service_tier"], "standard_only");
        assert_no_request_keys(
            &body,
            &[
                "cache_control",
                "prompt_cache_key",
                "prompt_cache_retention",
                "cache",
                "cache_read",
                "cache_write",
                "cache_creation",
                "previous_response_id",
                "conversation",
                "continuation",
                "continue_from",
            ],
        );
    }
    let anthropic_first = request_json(&anthropic_requests[0]);
    assert!(anthropic_first["tools"].is_null());
    assert!(
        anthropic_first["messages"]
            .as_array()
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user" && message["content"][0]["text"] == "anthropic"
                })
            })
    );
    let anthropic_tool_first = request_json(&anthropic_requests[1]);
    assert_eq!(anthropic_tool_first["tools"][0]["name"], "echo");
    assert!(anthropic_tool_first["tools"][0]["input_schema"].is_object());
    let anthropic_tool_second = request_json(&anthropic_requests[2]);
    let anthropic_messages = anthropic_tool_second["messages"]
        .as_array()
        .expect("Anthropic follow-up messages array");
    assert!(anthropic_messages.iter().any(|message| {
        message["role"] == "assistant"
            && message["content"]
                .as_array()
                .is_some_and(|content| content.iter().any(|block| block["type"] == "tool_use"))
    }));
    assert!(anthropic_messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"].as_array().is_some_and(|content| {
                content.iter().any(|block| {
                    block["type"] == "tool_result"
                        && block["tool_use_id"] == "call_anthropic_tool"
                        && block["content"][0]["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("provider"))
                })
            })
    }));
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root_path).unwrap();

    for (label, provider_kind) in [("openai-error", "openai"), ("anthropic-error", "anthropic")] {
        let (error_root, error_workspace) = root(label);
        let error_server = LoopbackServer::status(
            500,
            "application/json",
            r#"{"error":{"type":"api_error","message":"opaque"}}"#.to_owned(),
        );
        let error_selection = selection(provider_kind, "model");
        let error_descriptor = ModelDescriptor::new(
            error_selection.clone(),
            if provider_kind == "openai" {
                "openai-model"
            } else {
                "anthropic-model"
            },
            ModelLimits::new(Some(1_024), Some(128)).unwrap(),
            BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
        )
        .unwrap();
        let mut error_providers = ProviderRegistry::builder();
        if provider_kind == "openai" {
            error_providers
                .register(
                    OpenAiResponsesProvider::new_loopback_http(
                        &error_server.endpoint(),
                        fixed_credential_source("error-key").unwrap(),
                        vec![error_descriptor],
                    )
                    .unwrap(),
                )
                .unwrap();
        } else {
            error_providers
                .register(
                    AnthropicMessagesProvider::new_loopback_http(
                        &error_server.endpoint(),
                        "2023-06-01",
                        fixed_credential_source("error-key").unwrap(),
                        vec![error_descriptor],
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let error_runtime = Runtime::open(
            runtime_config(
                &error_root,
                error_providers.build(),
                ToolRegistry::default(),
            ),
            Handle::current(),
        )
        .await
        .unwrap();
        let error_id = error_runtime
            .create_session(session_config(&error_workspace, error_selection, &[]))
            .await
            .unwrap();
        let mut error_events = stream_for(&error_runtime, error_id).await;
        error_runtime
            .submit(error_id, "error".to_owned())
            .await
            .unwrap();
        assert!(matches!(
            finished(&mut error_events).await,
            TurnOutcome::Failed { .. }
        ));
        error_runtime.shutdown().await.unwrap();
        let error_requests = request_log(&error_server);
        assert_eq!(error_requests.len(), 1);
        assert_captured_request_shape(&error_requests[0]);
        let error_body = request_json(&error_requests[0]);
        assert_eq!(
            error_body["model"],
            if provider_kind == "openai" {
                "openai-model"
            } else {
                "anthropic-model"
            }
        );
        if provider_kind == "openai" {
            assert_header(&error_requests[0], "authorization", "Bearer error-key");
            assert_eq!(error_body["store"], false);
        } else {
            assert_header(&error_requests[0], "x-api-key", "error-key");
            assert_header(&error_requests[0], "anthropic-version", "2023-06-01");
            assert!(!error_requests[0].headers.contains_key("anthropic-beta"));
            assert_eq!(error_body["service_tier"], "standard_only");
        }
        fs::remove_dir_all(error_root).unwrap();
    }

    for (label, provider_kind) in [
        ("openai-cancel", "openai"),
        ("anthropic-cancel", "anthropic"),
    ] {
        let (cancel_root, cancel_workspace) = root(label);
        let cancel_body = if provider_kind == "openai" {
            openai_cancel_sse()
        } else {
            anthropic_sse("cancelled")
        };
        let (mut cancel_server, mut ready) = LoopbackServer::gated(cancel_body);
        let cancel_selection = selection(provider_kind, "model");
        let cancel_descriptor = ModelDescriptor::new(
            cancel_selection.clone(),
            if provider_kind == "openai" {
                "openai-model"
            } else {
                "anthropic-model"
            },
            ModelLimits::new(Some(1_024), Some(128)).unwrap(),
            BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
        )
        .unwrap();
        let mut cancel_providers = ProviderRegistry::builder();
        if provider_kind == "openai" {
            cancel_providers
                .register(
                    OpenAiResponsesProvider::new_loopback_http(
                        &cancel_server.endpoint(),
                        fixed_credential_source("cancel-key").unwrap(),
                        vec![cancel_descriptor],
                    )
                    .unwrap(),
                )
                .unwrap();
        } else {
            cancel_providers
                .register(
                    AnthropicMessagesProvider::new_loopback_http(
                        &cancel_server.endpoint(),
                        "2023-06-01",
                        fixed_credential_source("cancel-key").unwrap(),
                        vec![cancel_descriptor],
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let cancel_runtime = Runtime::open(
            runtime_config(
                &cancel_root,
                cancel_providers.build(),
                ToolRegistry::default(),
            ),
            Handle::current(),
        )
        .await
        .unwrap();
        let cancel_id = cancel_runtime
            .create_session(session_config(&cancel_workspace, cancel_selection, &[]))
            .await
            .unwrap();
        let mut cancel_events = stream_for(&cancel_runtime, cancel_id).await;
        cancel_runtime
            .submit(cancel_id, "cancel".to_owned())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut ready)
            .await
            .expect("provider loopback did not become ready")
            .expect("provider loopback readiness channel closed");
        cancel_runtime.cancel(cancel_id).unwrap();
        assert_eq!(finished(&mut cancel_events).await, TurnOutcome::Cancelled);
        cancel_server.release();
        cancel_runtime.shutdown().await.unwrap();
        let cancel_requests = request_log(&cancel_server);
        assert_eq!(cancel_requests.len(), 1);
        assert_captured_request_shape(&cancel_requests[0]);
        let cancel_body = request_json(&cancel_requests[0]);
        assert_eq!(
            cancel_body["model"],
            if provider_kind == "openai" {
                "openai-model"
            } else {
                "anthropic-model"
            }
        );
        if provider_kind == "openai" {
            assert_header(&cancel_requests[0], "authorization", "Bearer cancel-key");
            assert_eq!(cancel_body["store"], false);
        } else {
            assert_header(&cancel_requests[0], "x-api-key", "cancel-key");
            assert_header(&cancel_requests[0], "anthropic-version", "2023-06-01");
            assert!(!cancel_requests[0].headers.contains_key("anthropic-beta"));
            assert_eq!(cancel_body["service_tier"], "standard_only");
        }
        fs::remove_dir_all(cancel_root).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_14_session_isolation() {
    let (root, workspace_a) = root("at14");
    let workspace_b = root.join("workspace-b");
    fs::create_dir_all(&workspace_b).unwrap();
    let (provider_a, state_a) = provider("provider_a", vec![ScriptStep::Pending], None);
    let (provider_b, state_b) = provider("provider_b", vec![text_step("answer b")], None);
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![provider_a, provider_b]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let a = runtime
        .create_session(session_config(
            &workspace_a,
            selection("provider_a", "model"),
            &[],
        ))
        .await
        .unwrap();
    let b = runtime
        .create_session(session_config(
            &workspace_b,
            selection("provider_b", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut stream_a = stream_for(&runtime, a).await;
    let mut stream_b = stream_for(&runtime, b).await;

    let turn_a = runtime.submit(a, "input a".to_owned()).await.unwrap();
    wait_flag(&state_a.pending_started).await;
    let turn_b = runtime.submit(b, "input b".to_owned()).await.unwrap();
    let (finished_b_turn, finished_b_outcome) = finished_event(&mut stream_b).await;
    assert_eq!(finished_b_turn, turn_b);
    assert_eq!(finished_b_outcome, TurnOutcome::Completed);
    assert_eq!(state_a.cancellations.load(Ordering::SeqCst), 0);
    assert!(matches!(
        runtime.snapshot(a).unwrap().status(),
        SessionStatus::Running { turn_id } if turn_id == turn_a
    ));

    let page_b = runtime.transcript(b, None, 200).await.unwrap();
    assert_eq!(last_assistant_text(page_b.entries()), Some("answer b"));
    let requests_a = recorded_requests(&state_a);
    let requests_b = recorded_requests(&state_b);
    assert_eq!(requests_a.len(), 1);
    assert_eq!(requests_b.len(), 1);
    assert_eq!(request_user_texts(&requests_a[0]), vec!["input a"]);
    assert_eq!(request_user_texts(&requests_b[0]), vec!["input b"]);
    assert!(!request_user_texts(&requests_a[0]).contains(&"input b"));
    assert!(!request_user_texts(&requests_b[0]).contains(&"input a"));

    runtime.cancel(a).unwrap();
    let (finished_a_turn, finished_a_outcome) = finished_event(&mut stream_a).await;
    assert_eq!(finished_a_turn, turn_a);
    assert_eq!(finished_a_outcome, TurnOutcome::Cancelled);
    let page_a = runtime.transcript(a, None, 200).await.unwrap();
    assert!(page_a.entries().iter().any(|entry| matches!(
        entry,
        minicore_runtime::TranscriptEntry::User { text, .. } if text == "input a"
    )));
    assert!(!page_a.entries().iter().any(|entry| matches!(
        entry,
        minicore_runtime::TranscriptEntry::Assistant { text: Some(text), .. } if text == "answer b"
    )));
    assert!(!page_b.entries().iter().any(|entry| matches!(
        entry,
        minicore_runtime::TranscriptEntry::User { text, .. } if text == "input a"
    )));
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn at_15_event_lag() {
    let (root, workspace) = root("at15");
    let deltas = (0..128).map(|_| "x").collect::<Vec<_>>();
    let (scripted, _) = provider(
        "scripted",
        vec![delta_step("flooded", &deltas), text_step("after lag")],
        None,
    );
    let runtime = Runtime::open(
        runtime_config_with(
            &root,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
            1,
            Duration::from_secs(5),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut lagged = stream_for(&runtime, id).await;
    let mut observer = stream_for(&runtime, id).await;
    let first_turn = runtime.submit(id, "flood".to_owned()).await.unwrap();
    let (finished_turn, finished_outcome) = finished_event(&mut observer).await;
    assert_eq!(finished_turn, first_turn);
    assert_eq!(finished_outcome, TurnOutcome::Completed);
    let expected_snapshot = runtime.snapshot(id).unwrap();
    let expected_page = runtime.transcript(id, None, 200).await.unwrap();
    let expected_terminal = expected_page
        .entries()
        .iter()
        .find_map(|entry| match entry {
            minicore_runtime::TranscriptEntry::Terminal {
                seq,
                turn_id,
                outcome,
            } => Some((*seq, *turn_id, outcome.clone())),
            _ => None,
        })
        .expect("completed terminal in transcript");
    assert_eq!(expected_terminal.1, first_turn);
    assert_eq!(expected_terminal.2, TurnOutcome::Completed);
    assert_eq!(expected_snapshot.status(), SessionStatus::Idle);
    assert_eq!(expected_snapshot.session_id(), id);
    assert_eq!(
        expected_snapshot
            .last_terminal()
            .map(|terminal| terminal.turn_id),
        Some(first_turn)
    );
    assert_eq!(
        expected_snapshot
            .last_terminal()
            .map(|terminal| terminal.outcome.clone()),
        Some(TurnOutcome::Completed)
    );
    assert_eq!(expected_snapshot.conversation_seq(), expected_terminal.0);

    assert!(matches!(
        lagged.recv().await,
        Some(SessionEvent::ResyncRequired)
    ));
    let resync_snapshot = match lagged.recv().await {
        Some(SessionEvent::Snapshot(snapshot)) => snapshot,
        other => panic!("expected resync snapshot, got {other:?}"),
    };
    assert_eq!(resync_snapshot.session_id(), id);
    assert_eq!(resync_snapshot.status(), SessionStatus::Idle);
    assert_eq!(
        resync_snapshot.last_terminal(),
        expected_snapshot.last_terminal()
    );
    assert_eq!(resync_snapshot.conversation_seq(), expected_terminal.0);

    let second_turn = runtime
        .submit(id, "after recovery".to_owned())
        .await
        .unwrap();
    let mut recovery_events = Vec::new();
    let recovery_outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = lagged.recv().await.expect("recovered stream closed early");
            match &event {
                SessionEvent::TurnFinished { turn_id, outcome } => {
                    assert_eq!(*turn_id, second_turn);
                    recovery_events.push(event.clone());
                    break outcome.clone();
                }
                SessionEvent::TurnStarted { turn_id }
                | SessionEvent::TextDelta { turn_id, .. }
                | SessionEvent::ReasoningDelta { turn_id, .. }
                | SessionEvent::ToolStarted { turn_id, .. }
                | SessionEvent::InputRequested { turn_id, .. } => {
                    assert_eq!(*turn_id, second_turn);
                    recovery_events.push(event);
                }
                SessionEvent::ToolFinished { turn_id, .. } => {
                    assert_eq!(*turn_id, second_turn);
                    recovery_events.push(event);
                }
                SessionEvent::Snapshot(_) | SessionEvent::ResyncRequired | SessionEvent::Closed => {
                    panic!("stale or invalid event after resync: {event:?}")
                }
            }
        }
    })
    .await
    .expect("timed out waiting for post-resync turn");
    assert_eq!(recovery_outcome, TurnOutcome::Completed);
    assert!(recovery_events.iter().any(|event| matches!(
        event,
        SessionEvent::TurnStarted { turn_id } if *turn_id == second_turn
    )));
    assert_eq!(
        last_assistant_text(runtime.transcript(id, None, 200).await.unwrap().entries()),
        Some("after lag")
    );
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_16_busy_rule() {
    let (root, workspace) = root("at16");
    let (scripted, state) = provider("scripted", vec![ScriptStep::Pending], None);
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "first".to_owned()).await.unwrap();
    wait_flag(&state.pending_started).await;
    assert_eq!(
        runtime.submit(id, "second".to_owned()).await,
        Err(SessionError::Busy)
    );
    assert_eq!(
        runtime
            .transcript(id, None, 200)
            .await
            .unwrap()
            .entries()
            .iter()
            .filter(|entry| matches!(entry, minicore_runtime::TranscriptEntry::User { .. }))
            .count(),
        1
    );
    runtime.cancel(id).unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Cancelled);
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_17_close() {
    let (root, workspace) = root("at17");
    let (scripted, state) = provider("scripted", vec![ScriptStep::Stubborn], None);
    let runtime = Runtime::open(
        runtime_config_with(
            &root,
            provider_registry(vec![scripted]),
            ToolRegistry::default(),
            8,
            Duration::from_millis(100),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &[],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "close me".to_owned()).await.unwrap();
    wait_flag(&state.pending_started).await;
    let close = runtime.close_session(id);
    tokio::time::timeout(Duration::from_secs(5), close)
        .await
        .expect("close must be bounded")
        .unwrap();
    assert_eq!(runtime.snapshot(id), Err(SessionError::NotFound));
    event_matching(&mut stream, |event| matches!(event, SessionEvent::Closed)).await;
    assert_eq!(stream.recv().await, None);
    runtime.load_session(id).await.unwrap();
    runtime.close_session(id).await.unwrap();
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_18_custom_tool() {
    let (root, workspace) = root("at18");
    let (scripted, state) = provider(
        "scripted",
        vec![
            tool_step("echo", json!({"value": "custom"})),
            text_step("echoed"),
        ],
        None,
    );
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            enabled_tools(&["echo"]),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["echo"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "custom tool".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert!(
        tool_result(page.entries())
            .iter()
            .any(|text| text.contains("custom"))
    );
    assert_eq!(last_assistant_text(page.entries()), Some("echoed"));
    let requests = recorded_requests(&state);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        request_tool_outputs(&requests[1]),
        vec![r#"{"value":"custom"}"#]
    );
    assert_eq!(request_tool_call_count(&requests[1]), 1);
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_19_secret_env() {
    let (root, workspace) = root("at19");
    let (host_key, host_value) = present_host_environment();
    assert_eq!(
        std::env::var(host_key).ok().as_deref(),
        Some(host_value.as_str())
    );
    let (scripted, _) = provider(
        "scripted",
        vec![
            tool_step(
                "run_command",
                json!({
                    "program": std::env::current_exe().unwrap().to_string_lossy(),
                    "args": ["--exact", "at_08_cancel_process", "--nocapture"],
                    "env": {
                        "MINICORE_V2_HELPER_MODE": "env",
                        "MINICORE_V2_HOST_KEY": host_key
                    }
                }),
            ),
            text_step("environment checked"),
            tool_step(
                "run_command",
                json!({
                    "program": std::env::current_exe().unwrap().to_string_lossy(),
                    "args": ["--exact", "at_08_cancel_process", "--nocapture"],
                    "env": {
                        "MINICORE_V2_HELPER_MODE": "env",
                        "SECRET": "do-not-leak"
                    }
                }),
            ),
            text_step("secret rejected"),
        ],
        None,
    );
    let policy = Arc::new(
        ProcessPolicy::new(
            true,
            ProgramPolicy::allow_list([std::env::current_exe().unwrap().to_string_lossy()])
                .unwrap(),
            true,
            ["MINICORE_V2_HELPER_MODE", "MINICORE_V2_HOST_KEY"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
        .unwrap(),
    );
    assert!(policy.inherit_env());
    assert!(policy.allowed_env().contains("MINICORE_V2_HELPER_MODE"));
    assert!(policy.allowed_env().contains("MINICORE_V2_HOST_KEY"));
    assert!(!policy.allowed_env().contains(host_key));
    let runtime = Runtime::open(
        runtime_config(
            &root,
            provider_registry(vec![scripted]),
            process_tools(policy),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(session_config(
            &workspace,
            selection("scripted", "model"),
            &["run_command"],
        ))
        .await
        .unwrap();
    let mut stream = stream_for(&runtime, id).await;
    runtime.submit(id, "env".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    runtime.submit(id, "secret".to_owned()).await.unwrap();
    assert_eq!(finished(&mut stream).await, TurnOutcome::Completed);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    let results = tool_result(page.entries());
    let env_output = results
        .iter()
        .find(|text| text.contains("HOST_KEY"))
        .map(|text| serde_json::from_str::<Value>(text).unwrap())
        .expect("environment output");
    let stdout = env_output["stdout"].as_str().unwrap();
    assert!(stdout.contains(&format!("HOST_KEY=\"{host_key}\"")));
    assert!(stdout.contains("HOST_VALUE=Err(NotPresent)"));
    assert!(!stdout.contains(&host_value));
    assert!(
        results
            .iter()
            .any(|text| text.contains("command execution is not allowed"))
    );
    assert!(!results.iter().any(|text| text.contains("do-not-leak")));
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "P8 static architecture gate: legacy owner deletion is intentionally deferred"]
async fn at_20_no_legacy_coupling() {}
