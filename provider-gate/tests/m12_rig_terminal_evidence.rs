//! M12 evidence probe: rig 0.40.0 synthesizes a zero-usage `Final` when the
//! SSE body ends without a protocol terminal event (early EOF), so through
//! rig's own API a consumer cannot tell "completed" from "dropped
//! mid-response". This test-only wrapper around the public
//! `rig::http_client::HttpClientExt` seam forwards the streaming body
//! byte-for-byte below rig's own eventsource decoder and records terminal
//! evidence during rig's polls.
//!
//! Terminal rules: OpenAI success is exactly `response.completed`
//! (`response.failed`/`response.incomplete`/`error` are non-success);
//! Anthropic success is `message_delta` with a non-null `stop_reason` — rig
//! stops polling its event source at that event, so `message_stop` must not
//! be required.

mod support;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::{CompletionModel, CompletionRequest, Message};
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, ReqwestClient, Response,
    StreamingResponse,
};
use rig::providers::anthropic::completion::CLAUDE_SONNET_4_6;
use rig::providers::openai::GPT_4O_MINI;
use rig::streaming::StreamedAssistantContent;
use rig::wasm_compat::{WasmCompatSend, WasmCompatSendStream};
use serde_json::{Value, json};

/// rig's private `BoxedStream` alias, spelled through the public
/// `WasmCompatSendStream` trait so responses built here unify with
/// `StreamingResponse`.
type BoxedStream = Pin<Box<dyn WasmCompatSendStream<InnerItem = http_client::Result<Bytes>>>>;

/// Terminal evidence recorded while forwarding the streaming body.
#[derive(Debug, Clone, Default)]
struct Evidence {
    /// Every body byte forwarded, in order (byte-transparency proof).
    forwarded: Vec<u8>,
    /// The stream was polled to `None` (natural EOF).
    eof: bool,
    /// A poll yielded `Err` (transport error).
    transport_error: bool,
    /// Dropped without EOF or error (rig closed the event source early).
    dropped: bool,
    /// OpenAI `response.completed` observed.
    openai_completed: bool,
    /// Provider non-success terminal (`response.failed`/`response.incomplete`/`error`).
    provider_error: bool,
    /// Anthropic `message_delta` with non-null `stop_reason` observed.
    anthropic_stop_reason: bool,
}

impl Evidence {
    /// Protocol success. OpenAI: exactly `response.completed`. Anthropic:
    /// `message_delta` with non-null `stop_reason` — `message_stop` is not
    /// required because rig stops polling at the delta.
    fn terminal_success(&self) -> bool {
        !self.provider_error && (self.openai_completed || self.anthropic_stop_reason)
    }
}

/// `Arc`-shared evidence for the record path inside the stream wrapper.
#[derive(Debug, Clone, Default)]
struct EvidenceShared(Arc<Mutex<Evidence>>);

impl EvidenceShared {
    fn lock(&self) -> MutexGuard<'_, Evidence> {
        self.0.lock().expect("evidence mutex must not be poisoned")
    }

    fn snapshot(&self) -> Evidence {
        self.lock().clone()
    }
}

/// Incremental SSE parser: consumes arbitrary byte chunks, buffers only the
/// current line and the current frame's `data:` payload, and completes a
/// frame at the blank-line boundary. CR, LF, and CRLF are all accepted.
#[derive(Default)]
struct SseFrameParser {
    line: Vec<u8>,
    frame_data: Vec<u8>,
    frame_has_field: bool,
    pending_cr: bool,
}

impl SseFrameParser {
    fn feed(&mut self, chunk: &[u8], evidence: &mut Evidence) {
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue; // CRLF: the \r already ended the line
                }
            }
            match byte {
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(evidence);
                }
                b'\n' => self.end_line(evidence),
                _ => self.line.push(byte),
            }
        }
    }

    fn end_line(&mut self, evidence: &mut Evidence) {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            // Blank line: the frame boundary.
            if self.frame_has_field && !self.frame_data.is_empty() {
                self.finish_frame(evidence);
            }
            self.frame_data.clear();
            self.frame_has_field = false;
        } else if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            self.frame_data.extend_from_slice(value);
            self.frame_data.push(b'\n');
            self.frame_has_field = true;
        } else if line.first() != Some(&b':') {
            // Non-data fields (event:, id:, retry:) belong to the frame but
            // carry no data; comment lines (`:...`) are ignored.
            self.frame_has_field = true;
        }
    }

    fn finish_frame(&mut self, evidence: &mut Evidence) {
        let mut data = std::mem::take(&mut self.frame_data);
        if data.last() == Some(&b'\n') {
            data.pop(); // SSE joins data lines with \n and drops the final one
        }
        record_frame(&data, evidence);
    }
}

/// Decode one complete SSE frame's `data:` payload and record terminal
/// evidence from its `type` field. Non-JSON frames (e.g. `[DONE]`) are ignored.
fn record_frame(data: &[u8], evidence: &mut Evidence) {
    let Ok(value) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let Some(event_type) = value.get("type").and_then(Value::as_str) else {
        return;
    };
    match event_type {
        "response.completed" => evidence.openai_completed = true,
        "response.failed" | "response.incomplete" | "error" => evidence.provider_error = true,
        "message_delta" => {
            let stop_reason = value.pointer("/delta/stop_reason");
            if stop_reason.is_some_and(|stop_reason| !stop_reason.is_null()) {
                evidence.anthropic_stop_reason = true;
            }
        }
        _ => {}
    }
}

/// Test-only [`HttpClientExt`] over [`ReqwestClient`]: unary/multipart are
/// delegated unchanged; the streaming body is forwarded byte-for-byte through
/// [`ObservedStream`] so evidence is recorded during rig's polls.
#[derive(Clone, Debug, Default)]
struct TerminalEvidenceHttpClient {
    inner: ReqwestClient,
    evidence: EvidenceShared,
}

impl TerminalEvidenceHttpClient {
    fn new(inner: ReqwestClient, evidence: EvidenceShared) -> Self {
        Self { inner, evidence }
    }
}

impl HttpClientExt for TerminalEvidenceHttpClient {
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
        // Convert T outside the async block so the captured request is 'static
        // even though the trait's `T` bound carries no lifetime.
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        async move { inner.send(Request::from_parts(parts, body)).await }
    }

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
        let inner = self.inner.clone();
        let evidence = self.evidence.clone();
        async move {
            let response = inner.send_streaming(req).await?;
            // Status, version, and headers are preserved verbatim; only the
            // body is re-boxed through the observer.
            let (parts, body) = response.into_parts();
            let observed: BoxedStream = Box::pin(ObservedStream::new(body, evidence));
            Ok(Response::from_parts(parts, observed))
        }
    }
}

/// Forwards every body byte unchanged while the SSE parser records evidence
/// during each poll. `Drop` records "dropped before EOF" when the stream is
/// never polled to `None` (cancellation, or rig closing its event source).
struct ObservedStream {
    inner: BoxedStream,
    parser: SseFrameParser,
    evidence: EvidenceShared,
    terminated: bool,
}

impl ObservedStream {
    fn new(inner: BoxedStream, evidence: EvidenceShared) -> Self {
        Self {
            inner,
            parser: SseFrameParser::default(),
            evidence,
            terminated: false,
        }
    }
}

impl Stream for ObservedStream {
    type Item = http_client::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) => {
                let mut evidence = this.evidence.lock();
                evidence.forwarded.extend_from_slice(&bytes);
                this.parser.feed(&bytes, &mut evidence);
                drop(evidence);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.terminated = true;
                this.evidence.lock().transport_error = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.terminated = true;
                this.evidence.lock().eof = true;
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for ObservedStream {
    fn drop(&mut self) {
        if !self.terminated {
            self.evidence.lock().dropped = true;
        }
    }
}

/// Real rig OpenAI/Anthropic clients (reqwest-backed) with the wrapper
/// injected through the public builder seam: `builder().http_client(...)`.
fn wrapped_openai(
    base_url: &str,
    evidence: &EvidenceShared,
) -> rig::providers::openai::Client<TerminalEvidenceHttpClient> {
    let http_client = TerminalEvidenceHttpClient::new(ReqwestClient::new(), evidence.clone());
    rig::providers::openai::Client::builder()
        .api_key("test-key")
        .base_url(base_url)
        .http_client(http_client)
        .build()
        .expect("openai client must build with the wrapped http client")
}

fn wrapped_anthropic(
    base_url: &str,
    evidence: &EvidenceShared,
) -> rig::providers::anthropic::Client<TerminalEvidenceHttpClient> {
    let http_client = TerminalEvidenceHttpClient::new(ReqwestClient::new(), evidence.clone());
    rig::providers::anthropic::Client::builder()
        .api_key("sk-m12-evidence")
        .base_url(base_url)
        .http_client(http_client)
        .build()
        .expect("anthropic client must build with the wrapped http client")
}

/// The streaming request for OpenAI Responses: `stream: true` is set by rig.
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

/// Serialize SSE `data:` events with exact `data: {json}\n\n` framing.
fn sse_body(events: &[Value]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body
}

/// One OpenAI text delta.
fn openai_text_delta() -> Value {
    json!({
        "type": "response.output_text.delta",
        "item_id": "msg_m12",
        "output_index": 0,
        "content_index": 0,
        "sequence_number": 1,
        "delta": "The weather in Paris is 22°C."
    })
}

/// OpenAI terminal sequence: text delta + `response.completed` with real usage.
fn openai_terminal_events() -> Vec<Value> {
    vec![
        openai_text_delta(),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp_m12t",
                "object": "response",
                "created_at": 1752000000,
                "status": "completed",
                "model": GPT_4O_MINI,
                "output": [
                    {
                        "type": "message",
                        "id": "msg_m12t",
                        "role": "assistant",
                        "status": "completed",
                        "content": [
                            { "type": "output_text", "text": "The weather in Paris is 22°C." }
                        ]
                    }
                ],
                "usage": { "input_tokens": 7, "output_tokens": 9, "total_tokens": 16 }
            }
        }),
    ]
}

/// Full Anthropic terminal body ending at the `message_delta` carrying a
/// non-null `stop_reason` and real usage — deliberately no `message_stop`:
/// rig stops polling at the delta, so the terminal must be recognized without
/// it.
const ANTHROPIC_TERMINAL_BODY: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_m12a","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello from Claude"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":12,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}

"#;

/// Partial Anthropic body cut off mid-sentence: no stop-reason `message_delta`
/// — the connection simply ends.
const ANTHROPIC_EARLY_EOF_BODY: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_m12e","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Berlin is 22"}}

"#;

/// OpenAI complete through the real reqwest-backed client: rig's output and
/// real usage stay intact, and the observer records `response.completed` plus
/// natural EOF, with every body byte forwarded unchanged.
#[tokio::test(flavor = "current_thread")]
async fn openai_completed_rig_output_intact_observer_sees_terminal() {
    let body = sse_body(&openai_terminal_events());
    let evidence = EvidenceShared::default();

    let server = support::LoopbackServer::spawn_sse(&[(200, &body)]);
    let client = wrapped_openai(server.base_url(), &evidence);
    let model = client.completion_model(GPT_4O_MINI);
    let mut stream = model
        .stream(openai_request())
        .await
        .expect("stream must start");

    let mut step = 0usize;
    while let Some(item) = stream.next().await {
        match (
            step,
            item.expect("terminal body must not surface a stream error"),
        ) {
            (0, StreamedAssistantContent::Text(text)) => {
                assert_eq!(text.text, "The weather in Paris is 22°C.");
            }
            (1, StreamedAssistantContent::Final(response)) => {
                assert_eq!(response.usage.input_tokens, 7);
                assert_eq!(response.usage.output_tokens, 9);
                assert_eq!(response.usage.total_tokens, 16);
            }
            (_, other) => panic!("unexpected item at step {step}: {other:?}"),
        }
        step += 1;
    }
    assert_eq!(step, 2, "one text delta + the real final");
    assert_eq!(stream.usage().input_tokens, 7);

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method(), "POST");
    assert_eq!(requests[0].path(), "/responses");
    assert_eq!(requests[0].json_body()["stream"], true);
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert!(requests[0].body_len() > 0);

    let evidence = evidence.snapshot();
    assert_eq!(
        evidence.forwarded,
        body.as_bytes(),
        "every body byte must pass through the wrapper unchanged, in order"
    );
    assert!(evidence.openai_completed);
    assert!(evidence.terminal_success());
    assert!(!evidence.provider_error);
    assert!(evidence.eof, "the OpenAI read loop drains to natural EOF");
    assert!(
        !evidence.dropped,
        "EOF was observed, so drop is not 'before EOF'"
    );
    assert!(!evidence.transport_error);
}

/// OpenAI early EOF: the connection closes after a partial delta without
/// `response.completed`. Rig 0.40.0 still synthesizes a zero-usage Final, but
/// the observer records natural EOF without protocol terminal.
#[tokio::test(flavor = "current_thread")]
async fn openai_early_eof_synthetic_final_but_observer_sees_eof_without_terminal() {
    let body = sse_body(&[openai_text_delta()]);
    let evidence = EvidenceShared::default();

    let server = support::LoopbackServer::spawn_sse(&[(200, &body)]);
    let client = wrapped_openai(server.base_url(), &evidence);
    let model = client.completion_model(GPT_4O_MINI);
    let mut stream = model
        .stream(openai_request())
        .await
        .expect("stream must start");

    let mut step = 0usize;
    while let Some(item) = stream.next().await {
        match (
            step,
            item.expect("early EOF must not surface a stream error"),
        ) {
            (0, StreamedAssistantContent::Text(text)) => {
                assert_eq!(text.text, "The weather in Paris is 22°C.");
            }
            (1, StreamedAssistantContent::Final(response)) => {
                assert_eq!(
                    response.usage.total_tokens, 0,
                    "synthetic final has zero usage"
                );
            }
            (_, other) => panic!("unexpected item at step {step}: {other:?}"),
        }
        step += 1;
    }
    assert_eq!(step, 2, "partial delta + synthesized final");

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path(), "/responses");

    let evidence = evidence.snapshot();
    assert!(
        !evidence.terminal_success(),
        "no response.completed was observed"
    );
    assert!(!evidence.openai_completed);
    assert!(!evidence.provider_error);
    assert!(evidence.eof, "the connection ended naturally");
    assert!(!evidence.dropped, "EOF was observed, not a drop");
    assert!(!evidence.transport_error);
}

/// Anthropic complete: the body ends at the stop-reason `message_delta` (no
/// `message_stop`). Rig's output and real usage stay intact; the observer
/// records `anthropic_stop_reason`. Rig breaks its read loop at the delta and
/// closes its event source, so EOF is unobservable here and the drop is rig's
/// normal close, not cancellation.
#[tokio::test(flavor = "current_thread")]
async fn anthropic_stop_reason_delta_rig_output_intact_observer_sees_terminal() {
    let evidence = EvidenceShared::default();

    let server_requests = {
        let server = support::LoopbackServer::spawn_sse(&[(200, ANTHROPIC_TERMINAL_BODY)]);
        let client = wrapped_anthropic(server.base_url(), &evidence);
        let model = client.completion_model(CLAUDE_SONNET_4_6);
        let mut stream = model
            .stream(model.completion_request("Hello").max_tokens(16).build())
            .await
            .expect("stream must start");

        let mut step = 0usize;
        while let Some(item) = stream.next().await {
            match (
                step,
                item.expect("terminal body must not surface a stream error"),
            ) {
                (0, StreamedAssistantContent::Text(text)) => {
                    assert_eq!(text.text, "Hello from Claude");
                }
                (1, StreamedAssistantContent::Final(response)) => {
                    assert_eq!(response.usage.output_tokens, 12);
                    assert_eq!(response.usage.input_tokens, Some(5));
                }
                (_, other) => panic!("unexpected item at step {step}: {other:?}"),
            }
            step += 1;
        }
        assert_eq!(step, 2, "one text delta + the real final");
        assert_eq!(stream.usage().output_tokens, 12);

        let requests = server.join();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path(), "/v1/messages");
        requests
    };
    assert_eq!(server_requests.len(), 1);

    let evidence = evidence.snapshot();
    assert!(evidence.anthropic_stop_reason);
    assert!(evidence.terminal_success());
    assert!(
        !evidence.eof,
        "rig closes the event source at message_delta"
    );
    assert!(
        evidence.dropped,
        "rig's early event-source close surfaces as 'dropped before EOF'"
    );
    assert!(!evidence.transport_error);
}

/// Anthropic early EOF: the connection closes after a partial text block with
/// no stop-reason `message_delta`. Rig synthesizes a zero-usage Final; the
/// observer records EOF without protocol terminal.
#[tokio::test(flavor = "current_thread")]
async fn anthropic_early_eof_synthetic_final_but_observer_sees_eof_without_terminal() {
    let evidence = EvidenceShared::default();

    let server = support::LoopbackServer::spawn_sse(&[(200, ANTHROPIC_EARLY_EOF_BODY)]);
    let client = wrapped_anthropic(server.base_url(), &evidence);
    let model = client.completion_model(CLAUDE_SONNET_4_6);
    let mut stream = model
        .stream(model.completion_request("Hello").max_tokens(16).build())
        .await
        .expect("stream must start");

    let mut step = 0usize;
    while let Some(item) = stream.next().await {
        match (
            step,
            item.expect("early EOF must not surface a stream error"),
        ) {
            (0, StreamedAssistantContent::Text(text)) => assert_eq!(text.text, "Berlin is 22"),
            (1, StreamedAssistantContent::Final(response)) => {
                assert_eq!(response.usage.output_tokens, 0);
                assert_eq!(response.usage.input_tokens, None);
            }
            (_, other) => panic!("unexpected item at step {step}: {other:?}"),
        }
        step += 1;
    }
    assert_eq!(step, 2, "partial text delta + synthesized final");

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path(), "/v1/messages");

    let evidence = evidence.snapshot();
    assert!(!evidence.terminal_success());
    assert!(
        !evidence.anthropic_stop_reason,
        "no stop-reason delta observed"
    );
    assert!(evidence.eof);
    assert!(!evidence.dropped);
    assert!(!evidence.transport_error);
}

/// Fragmentation: feed chunks that split frames mid-JSON-token, mid-key, and
/// across the blank-line frame boundary straight into the incremental parser;
/// terminal detection must survive arbitrary chunking.
#[test]
fn fragmented_frames_split_across_chunks_are_recognized() {
    // OpenAI terminal frame split mid-token, mid-key, and across the `\n\n`
    // boundary (the final blank line arrives in its own chunk).
    let chunks: &[&str] = &[
        r#"data: {"type":"response.cre"#,
        r#"ated","sequence_number":0,"response":{"id":"resp_f","object":"response","created_at":1,"status":"in_progress","model":"gpt-5"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","sequence_number":1,"response":{"id":"resp_f","#,
        r#""object":"response","created_at":1,"status":"completed","model":"gpt-5","output":[{"type":"message","id":"msg_f","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hi"}]}],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#,
        "\n",
        "\n",
    ];
    let mut evidence = Evidence::default();
    let mut parser = SseFrameParser::default();
    for chunk in chunks {
        parser.feed(chunk.as_bytes(), &mut evidence);
    }
    assert!(evidence.openai_completed);
    assert!(evidence.terminal_success());
    assert!(!evidence.provider_error);

    // A split non-success frame (`response.failed`) must not count as success.
    let mut evidence = Evidence::default();
    let mut parser = SseFrameParser::default();
    for chunk in [
        r#"data: {"type":"response.fai"#,
        r#"led","sequence_number":1,"response":{"id":"resp_f","object":"response","created_at":1,"status":"failed","model":"gpt-5","output":[]}}"#,
        "\n\n",
    ] {
        parser.feed(chunk.as_bytes(), &mut evidence);
    }
    assert!(evidence.provider_error);
    assert!(!evidence.openai_completed);
    assert!(!evidence.terminal_success());

    // Anthropic stop-reason `message_delta` split mid-key.
    let mut evidence = Evidence::default();
    let mut parser = SseFrameParser::default();
    for chunk in [
        r#"data: {"type":"message_delta","delta":{"stop_re"#,
        r#"ason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#,
        "\n\n",
    ] {
        parser.feed(chunk.as_bytes(), &mut evidence);
    }
    assert!(evidence.anthropic_stop_reason);
    assert!(evidence.terminal_success());
}
