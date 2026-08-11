//! M12 slice: Rig 0.40.0 OpenAI Responses *streaming* reality/contract probe.
//!
//! Drives the real `rig-core` 0.40.0 OpenAI Responses streaming path
//! (`CompletionModel::stream` on the reqwest-backed `GenericResponsesCompletionModel`)
//! against the test-owned loopback SSE server (`tests/support`,
//! [`support::LoopbackServer::spawn_sse`]). The server captures the actual
//! POST (request line, headers, Content-Length-delimited body) and scripts
//! realistic `text/event-stream` sequences, then closes the connection.
//!
//! Contract proven here (for a future private RigProviderAdapter):
//! - terminal SSE contract: exactly one POST to `/responses` carrying
//!   `stream: true`; the consumer-visible item order (reasoning delta -> text
//!   deltas -> reasoning done item -> final after `response.completed`);
//!   message identity and usage rig surfaces after the drain;
//! - `cancel()` contract: rig cancellation is *normal stream termination* —
//!   the item after `cancel()` is `None`, no error item, no synthetic final;
//! - early-EOF truth: a connection closed after partial deltas and without
//!   `response.completed` does **not** error. Rig 0.40.0 unconditionally runs
//!   `RawChoiceAccumulator::finish()` and synthesizes a final with zero/default
//!   usage. A MiniCore adapter MUST track provider terminal evidence
//!   (`response.completed`) itself and map a missing terminal to
//!   StreamInterrupted/Incomplete instead of trusting rig's synthetic final.

mod support;

use futures_util::StreamExt;
use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::message::{Reasoning, ReasoningContent};
use rig::completion::{
    AssistantContent, CompletionError, CompletionModel, CompletionRequest, Message, Usage,
};
use rig::providers::openai::responses_api::ResponsesUsage;
use rig::providers::openai::{Client, GPT_4O_MINI};
use rig::streaming::StreamedAssistantContent;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The streaming request: a plain user message; `stream: true` is set by rig
/// itself inside `GenericResponsesCompletionModel::stream`.
fn simple_request() -> CompletionRequest {
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

/// Builds a real rig-core OpenAI Responses client pinned to `base_url`.
fn openai_client(base_url: &str) -> Client {
    Client::builder()
        .api_key("test-key")
        .base_url(base_url)
        .build()
        .expect("openai client build")
}

/// Serialize SSE `data:` events with the exact framing the eventsource-stream
/// parser requires: `data: {json}` followed by a blank line. An event not
/// terminated by a blank line is dropped on EOF, so every event gets one.
fn sse_body(events: &[Value]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body
}

/// A realistic finite OpenAI Responses SSE sequence: response.created, message
/// item added, reasoning + text deltas, message done (identity), reasoning
/// done, and response.completed carrying full usage.
fn terminal_sse_events() -> Vec<Value> {
    vec![
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_123",
                "object": "response",
                "created_at": 1752000000,
                "status": "in_progress",
                "model": GPT_4O_MINI,
            }
        }),
        json!({
            "type": "response.output_item.added",
            "item_id": "msg_456",
            "output_index": 0,
            "sequence_number": 1,
            "item": {
                "type": "message",
                "id": "msg_456",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            }
        }),
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_1",
            "output_index": 1,
            "summary_index": 0,
            "sequence_number": 2,
            "delta": "Let me think"
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_456",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 3,
            "delta": "The weather"
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_456",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 4,
            "delta": " in Paris is 22°C."
        }),
        json!({
            "type": "response.output_item.done",
            "item_id": "msg_456",
            "output_index": 0,
            "sequence_number": 5,
            "item": {
                "type": "message",
                "id": "msg_456",
                "role": "assistant",
                "status": "completed",
                "content": [
                    { "type": "output_text", "text": "The weather in Paris is 22°C." }
                ]
            }
        }),
        json!({
            "type": "response.output_item.done",
            "item_id": "rs_1",
            "output_index": 1,
            "sequence_number": 6,
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [
                    { "type": "summary_text", "text": "Hidden chain-of-thought" }
                ],
                "content": [
                    { "type": "reasoning_text", "text": "Full reasoning" }
                ],
                "status": "completed"
            }
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 7,
            "response": {
                "id": "resp_123",
                "object": "response",
                "created_at": 1752000000,
                "status": "completed",
                "model": GPT_4O_MINI,
                "output": [
                    {
                        "type": "message",
                        "id": "msg_456",
                        "role": "assistant",
                        "status": "completed",
                        "content": [
                            { "type": "output_text", "text": "The weather in Paris is 22°C." }
                        ]
                    },
                    {
                        "type": "reasoning",
                        "id": "rs_1",
                        "summary": [],
                        "content": [],
                        "status": "completed"
                    }
                ],
                "usage": {
                    "input_tokens": 42,
                    "input_tokens_details": { "cached_tokens": 7 },
                    "output_tokens": 11,
                    "output_tokens_details": { "reasoning_tokens": 4 },
                    "total_tokens": 53
                }
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The complete terminal SSE contract: a realistic finite OpenAI Responses
/// SSE sequence drained through `futures_util::StreamExt`, with request-shape
/// and post-drain evidence assertions.
#[tokio::test]
async fn streaming_terminal_contract_full_sse_sequence() {
    let server = support::LoopbackServer::spawn_sse(&[(200, &sse_body(&terminal_sse_events()))]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let mut stream = model
        .stream(simple_request())
        .await
        .expect("stream must start");

    // --- Consumer-visible items in exact provider order. ---
    let mut step = 0usize;
    let mut final_usage: Option<ResponsesUsage> = None;
    while let Some(item) = stream.next().await {
        match (step, item.expect("terminal stream must not error")) {
            (
                0,
                StreamedAssistantContent::ReasoningDelta {
                    id: None,
                    reasoning,
                },
            ) => {
                assert_eq!(reasoning, "Let me think", "summary delta first, id None");
            }
            (1, StreamedAssistantContent::Text(text)) => {
                assert_eq!(text.text, "The weather");
            }
            (2, StreamedAssistantContent::Text(text)) => {
                assert_eq!(text.text, " in Paris is 22°C.");
            }
            (3, StreamedAssistantContent::Reasoning(reasoning)) => {
                assert_eq!(reasoning.id.as_deref(), Some("rs_1"));
                assert!(
                    matches!(&reasoning.content[..], [ReasoningContent::Summary(text)] if text == "Hidden chain-of-thought"),
                    "done reasoning item emits its summary block, got {:?}",
                    reasoning.content
                );
            }
            (4, StreamedAssistantContent::Reasoning(reasoning)) => {
                assert_eq!(reasoning.id.as_deref(), Some("rs_1"));
                assert!(
                    matches!(&reasoning.content[..], [ReasoningContent::Text { text, signature: None }] if text == "Full reasoning"),
                    "done reasoning item emits its text block, got {:?}",
                    reasoning.content
                );
            }
            (5, StreamedAssistantContent::Final(res)) => final_usage = Some(res.usage),
            (_, other) => panic!("unexpected streamed item at step {step}: {other:?}"),
        }
        step += 1;
    }
    assert_eq!(step, 6, "six consumer items in provider order");

    // --- Transport-level request shape: exactly one POST to /responses with
    // stream:true. ---
    let requests = server.join();
    assert_eq!(
        requests.len(),
        1,
        "one stream() invocation must make exactly one HTTP request"
    );
    let request = &requests[0];
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/responses");
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(
        request.header("accept"),
        Some("text/event-stream"),
        "the SSE event source must advertise text/event-stream"
    );
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
    let wire: Value = request.json_body();
    assert_eq!(wire["model"], GPT_4O_MINI);
    assert_eq!(
        wire["stream"], true,
        "streaming request must carry stream:true"
    );

    // --- Final usage carried by the response.completed event. ---
    let final_usage = final_usage.expect("response.completed usage must be yielded");
    assert_eq!(final_usage.input_tokens, 42);
    assert_eq!(final_usage.output_tokens, 11);
    assert_eq!(final_usage.total_tokens, 53);
    assert_eq!(
        final_usage
            .input_tokens_details
            .as_ref()
            .expect("input details")
            .cached_tokens,
        7
    );
    assert_eq!(
        final_usage
            .output_tokens_details
            .as_ref()
            .expect("output details")
            .reasoning_tokens,
        4
    );

    // --- Message/response evidence rig exposes after the drain. ---
    assert_eq!(
        stream.message_id.as_deref(),
        Some("msg_456"),
        "message identity captured from output_item.done"
    );
    let final_res = stream
        .response
        .as_ref()
        .expect("response.completed must populate stream.response");
    assert_eq!(final_res.usage.total_tokens, 53);
    assert_eq!(stream.usage().input_tokens, 42);
    assert_eq!(stream.usage().cached_input_tokens, 7);
    assert_eq!(stream.usage().reasoning_tokens, 4);

    // --- Aggregated choice: delta reasoning, joined text, done-item blocks. ---
    let choice: Vec<&AssistantContent> = stream.choice.iter().collect();
    assert_eq!(
        choice.len(),
        4,
        "reasoning delta + text + 2 done-item blocks"
    );
    assert!(
        matches!(
            choice[0],
            AssistantContent::Reasoning(Reasoning { id: None, content, .. })
                if matches!(&content[..], [ReasoningContent::Text { text, signature: None }] if text == "Let me think")
        ),
        "first aggregated block is the reasoning delta, got {:?}",
        choice[0]
    );
    assert!(
        matches!(choice[1], AssistantContent::Text(text) if text.text == "The weather in Paris is 22°C."),
        "text deltas join into one block, got {:?}",
        choice[1]
    );
    assert!(
        matches!(
            choice[2],
            AssistantContent::Reasoning(Reasoning { id: Some(id), content, .. })
                if id == "rs_1"
                    && matches!(&content[..], [ReasoningContent::Summary(_)])
        ),
        "done reasoning summary block, got {:?}",
        choice[2]
    );
    assert!(
        matches!(
            choice[3],
            AssistantContent::Reasoning(Reasoning { id: Some(id), content, .. })
                if id == "rs_1"
                    && matches!(&content[..], [ReasoningContent::Text { .. }])
        ),
        "done reasoning text block, got {:?}",
        choice[3]
    );
}

/// The `AbortHandle` contract: after the first semantic item, `cancel()` makes
/// the next item `None` — rig surfaces cancellation as normal stream
/// termination, not as an error, and no synthetic final is produced.
#[tokio::test]
async fn cancel_after_first_item_terminates_stream_normally() {
    let events = vec![
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 1,
            "delta": "hello "
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 2,
            "delta": "world"
        }),
        json!({
            "type": "response.output_item.done",
            "item_id": "msg_1",
            "output_index": 0,
            "sequence_number": 3,
            "item": {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [
                    { "type": "output_text", "text": "hello world" }
                ]
            }
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 4,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1752000000,
                "status": "completed",
                "model": GPT_4O_MINI,
                "usage": {
                    "input_tokens": 5,
                    "output_tokens": 3,
                    "total_tokens": 8
                }
            }
        }),
    ];
    let body = sse_body(&events);
    let server = support::LoopbackServer::spawn_sse(&[(200, &body)]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let mut stream = model
        .stream(simple_request())
        .await
        .expect("stream must start");

    // Read the first semantic item: the first text delta.
    let first = stream
        .next()
        .await
        .expect("first item")
        .expect("no stream error");
    assert!(
        matches!(&first, StreamedAssistantContent::Text(text) if text.text == "hello "),
        "first item is the first delta, got {first:?}"
    );

    // Rig cancellation contract: normal stream termination, next item is None.
    stream.cancel();
    assert!(
        stream.next().await.is_none(),
        "after cancel() the next item must be None (cancellation is normal \
         stream termination, not an error)"
    );

    // Settle/join the server before asserting, so it cannot outlive the test.
    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path(), "/responses");
    assert_eq!(requests[0].json_body()["stream"], true);

    // Cancellation prevented the terminal evidence: no final response, and
    // usage stays at the zero sentinel.
    assert!(
        stream.response.is_none(),
        "cancel() must prevent any (synthesized or real) final response"
    );
    assert_eq!(stream.usage(), Usage::new());
}

/// Early-EOF reality: the server closes the connection after partial deltas
/// with no `response.completed`. Rig 0.40.0 does **not** error — the drain
/// yields a synthesized `StreamedAssistantContent::Final` carrying the
/// accumulator's initial zero/default usage.
///
/// This is the exact behavior a future MiniCore adapter must NOT trust as
/// provider terminal evidence: the adapter MUST track `response.completed`
/// itself and map a missing terminal to StreamInterrupted/Incomplete.
#[tokio::test]
async fn early_eof_without_completed_yields_synthetic_zero_usage_final_never_error() {
    let events = vec![
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_early",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 1,
            "delta": "partial "
        }),
        json!({
            "type": "response.output_item.done",
            "item_id": "msg_early",
            "output_index": 0,
            "sequence_number": 2,
            "item": {
                "type": "message",
                "id": "msg_early",
                "role": "assistant",
                "status": "completed",
                "content": [
                    { "type": "output_text", "text": "partial " }
                ]
            }
        }),
    ];
    // No response.completed: the body ends after the message done item and the
    // server closes the connection (exact Content-Length, Connection: close).
    let body = sse_body(&events);
    let server = support::LoopbackServer::spawn_sse(&[(200, &body)]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let mut stream = model
        .stream(simple_request())
        .await
        .expect("stream must start");

    // Drain fully. Every item must be Ok — early EOF is not an error.
    let mut step = 0usize;
    let mut final_usage: Option<ResponsesUsage> = None;
    while let Some(item) = stream.next().await {
        match (
            step,
            item.expect("early EOF must not surface a stream error"),
        ) {
            (0, StreamedAssistantContent::Text(text)) => assert_eq!(text.text, "partial "),
            (1, StreamedAssistantContent::Final(res)) => final_usage = Some(res.usage),
            (_, other) => panic!("unexpected item at step {step}: {other:?}"),
        }
        step += 1;
    }
    assert_eq!(step, 2, "partial delta + synthesized final, no error item");

    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path(), "/responses");
    assert_eq!(requests[0].json_body()["stream"], true);

    // The synthesized final carries the accumulator's initial zero usage.
    let final_usage = final_usage.expect("rig 0.40.0 synthesizes a final on early EOF");
    assert_eq!(final_usage.input_tokens, 0);
    assert_eq!(final_usage.output_tokens, 0);
    assert_eq!(final_usage.total_tokens, 0);

    // Post-drain evidence mirrors the synthesis: stream.response is Some —
    // this is NOT provider terminal evidence — and usage is the zero sentinel.
    assert!(
        stream.response.is_some(),
        "rig 0.40.0 records the synthesized final on stream.response"
    );
    assert_eq!(stream.usage(), Usage::new());

    // Message identity from the done item is still surfaced, and the partial
    // text is still aggregated into the choice.
    assert_eq!(stream.message_id.as_deref(), Some("msg_early"));
    let choice: Vec<&AssistantContent> = stream.choice.iter().collect();
    assert_eq!(choice.len(), 1);
    assert!(
        matches!(choice[0], AssistantContent::Text(text) if text.text == "partial "),
        "partial text must survive early EOF, got {:?}",
        choice[0]
    );
}

/// HTTP 500 reality: `GenericEventSource` ships an unconditional
/// exponential-backoff retry policy (`DEFAULT_RETRY`: 300ms start, infinite
/// retries, error-agnostic), but automatic retry is *effectively zero* on the
/// OpenAI Responses path: the reqwest client surfaces the 500 as
/// `InvalidStatusCodeWithMessage` (status **and** body preserved), and the
/// provider wrapper breaks out of the event-source loop on the first error,
/// dropping the event source mid-retry-cycle — the scheduled reconnect never
/// fires. Consumer-visible: one error item, then `None`, exactly one POST.
#[tokio::test]
async fn http_500_breaks_stream_with_effectively_zero_retries_despite_retry_policy() {
    // Two scripted 500s: the second is a tripwire that proves the retry
    // cycle never re-issues the POST.
    let error_body =
        "{\"error\":{\"message\":\"boom\",\"type\":\"server_error\",\"code\":\"server_error\"}}";
    let server = support::LoopbackServer::spawn_sse(&[(500, error_body), (500, error_body)]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let mut stream = model
        .stream(simple_request())
        .await
        .expect("stream must start");

    // First poll: the provider error is the first item, carrying both the
    // 500 status and the exact error body.
    let err = stream
        .next()
        .await
        .expect("first item must be the provider error")
        .expect_err("HTTP 500 must surface as a stream error");
    assert!(
        matches!(
            &err,
            CompletionError::HttpError(
                rig::http_client::Error::InvalidStatusCodeWithMessage(code, _)
            ) if code.as_u16() == 500
        ),
        "the 500 must surface as an HTTP status error, got {err:?}"
    );
    assert_eq!(
        err.provider_response_status().map(|status| status.as_u16()),
        Some(500),
        "the 500 status must stay recoverable"
    );
    assert_eq!(
        err.provider_response_body(),
        Some(error_body),
        "the 500 body must be preserved verbatim"
    );

    // Second poll: the provider wrapper breaks on the first error and drops
    // the event source before its retry delay elapses — the stream ends.
    assert!(
        stream.next().await.is_none(),
        "the error must terminate the stream: no retry item, no synthesized final"
    );

    // Exactly one POST /responses: the scripted second 500 is never consumed.
    let requests = server.join();
    assert_eq!(
        requests.len(),
        1,
        "the event source retry cycle must never re-issue the POST"
    );
    assert_eq!(requests[0].method(), "POST");
    assert_eq!(requests[0].path(), "/responses");
    assert_eq!(requests[0].json_body()["stream"], true);
}
