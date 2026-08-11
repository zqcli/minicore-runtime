//! M12 slice: Rig 0.40.0 Anthropic Messages *streaming* reality/contract probe.
//!
//! Drives the real Rig Anthropic streaming path (`CompletionModel::stream`) over
//! a test-owned loopback HTTP server (`tests/support::LoopbackServer::spawn_sse`)
//! on 127.0.0.1:0 with the reqwest backend and no credentials. Everything is
//! offline, bounded and deterministic: no sleeps, no timeouts, no absence-based
//! proofs, and every test-owned server thread is joined before assertions.
//!
//! Contract proven here (for a future private MiniCore streaming adapter):
//! - the streaming wire body: POST /v1/messages with `stream: true`, exactly one
//!   HTTP request per stream invocation;
//! - a full terminal Anthropic SSE sequence (message_start -> thinking
//!   start/delta/signature/stop -> text start/delta/stop -> tool_use
//!   start/input_json_delta/stop -> message_delta with stop_reason + usage ->
//!   message_stop): emitted reasoning / text / tool identity and order,
//!   signature preservation, final usage, and the accumulated choice;
//! - `cancel()` terminates the stream deterministically after the first item;
//! - early EOF (connection close without message_delta / message_stop): Rig
//!   0.40.0 synthesizes a `FinalResponse` with default (zero) usage, so a
//!   consumer cannot tell "complete" from "dropped" through this API.

mod support;

use futures_util::StreamExt;
use rig::client::CompletionClient;
use rig::completion::message::AssistantContent;
use rig::completion::{CompletionModel, ToolDefinition, Usage};
use rig::providers::anthropic::Client;
use rig::providers::anthropic::client::AnthropicExt;
use rig::providers::anthropic::completion::{CLAUDE_SONNET_4_6, GenericCompletionModel};
use rig::providers::anthropic::streaming::StreamingCompletionResponse as AnthropicStreamingResponse;
use rig::streaming::{StreamedAssistantContent, ToolCallDeltaContent};
use serde_json::json;

/// The concrete completion model Rig builds for the loopback client (the
/// reqwest-backed default backend).
type LoopbackModel = GenericCompletionModel<AnthropicExt, rig::http_client::ReqwestClient>;

/// Full terminal Anthropic Messages SSE sequence: message_start (id/model/input
/// usage), a thinking block (start, two deltas, signature, stop), a text block,
/// a tool_use block (start, two input_json deltas, stop), message_delta with a
/// `tool_use` stop_reason and output/cache usage, and message_stop.
const TERMINAL_SSE_BODY: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01M12STREAM","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me reason about the weather."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_01M12STREAM"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Berlin is 22"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" degrees and sunny."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01M12STREAM","name":"lookup_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\": \"Ber"}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"lin\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":19,"cache_creation_input_tokens":11,"cache_read_input_tokens":9}}

event: message_stop
data: {"type":"message_stop"}

"#;

/// Early-EOF body: a partial semantic response (completed thinking block, text
/// deltas cut off mid-sentence) followed by connection close — no message_delta,
/// no message_stop, no terminal evidence of any kind.
const EARLY_EOF_SSE_BODY: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01M12EOF","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Checking the forecast source."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_01M12EOF"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Berlin is 22"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" degrees"}}

"#;

/// Real Rig Anthropic client pinned to the loopback base URL.
fn loopback_model(base_url: &str) -> LoopbackModel {
    Client::builder()
        .api_key("sk-m12-stream")
        .base_url(base_url)
        .build()
        .expect("rig anthropic client must build against the loopback base url")
        .completion_model(CLAUDE_SONNET_4_6)
}

/// Streaming request advertising the tool the scripted response calls.
fn weather_request(model: &LoopbackModel) -> rig::completion::CompletionRequest {
    model
        .completion_request("What is the weather in Berlin?")
        .tool(ToolDefinition {
            name: "lookup_weather".to_string(),
            description: "Look up the current weather for a city.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        })
        .max_tokens(64)
        .build()
}

/// The main streaming contract: wire body, emitted item identity/order,
/// signature preservation, final usage, and the accumulated choice.
#[tokio::test(flavor = "current_thread")]
async fn m12_anthropic_streaming_terminal_contract() {
    let server = support::LoopbackServer::spawn_sse(&[(200, TERMINAL_SSE_BODY)]);
    let model = loopback_model(server.base_url());
    let mut stream = model
        .stream(weather_request(&model))
        .await
        .expect("streaming request must start against the loopback server");

    // Drain with futures_util::StreamExt.
    let mut items: Vec<StreamedAssistantContent<AnthropicStreamingResponse>> = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item.expect("stream items must not error"));
    }

    // Settle the server thread before asserting.
    let captured = server.join();

    // ------------------------------------------------------------------
    // Exactly one HTTP request; POST /v1/messages with `stream: true`.
    // ------------------------------------------------------------------
    assert_eq!(
        captured.len(),
        1,
        "one stream invocation must make exactly one HTTP request"
    );
    let request = &captured[0];
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/v1/messages");
    assert_eq!(request.header("content-type"), Some("application/json"));
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
    let body = request.json_body();
    assert_eq!(
        body["stream"],
        json!(true),
        "streaming body must set stream: true"
    );
    assert_eq!(body["model"], CLAUDE_SONNET_4_6);
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(
        body["tool_choice"],
        json!({"type": "auto"}),
        "tools advertised with choice unset: the streaming path sends explicit auto"
    );
    assert_eq!(body["tools"][0]["name"], "lookup_weather");
    assert_eq!(body["messages"][0]["role"], "user");

    // ------------------------------------------------------------------
    // Emitted items in provider order: reasoning delta, full reasoning
    // block, two text deltas, tool name + input JSON deltas, complete tool
    // call, final response. message_start / message_stop / block starts are
    // consumed internally and never surface.
    // ------------------------------------------------------------------
    assert_eq!(
        items.len(),
        9,
        "reasoning delta + reasoning block + 2 text + 3 tool deltas + tool call + final"
    );

    match &items[0] {
        StreamedAssistantContent::ReasoningDelta { id, reasoning } => {
            assert!(id.is_none(), "anthropic streams no reasoning block id");
            assert_eq!(reasoning, "Let me reason about the weather.");
        }
        other => panic!("first item must be the thinking delta, got {other:?}"),
    }

    match &items[1] {
        StreamedAssistantContent::Reasoning(reasoning) => {
            assert!(reasoning.id.is_none());
            assert_eq!(
                reasoning.first_text(),
                Some("Let me reason about the weather.")
            );
            // The signature delta is folded into the final reasoning block.
            assert_eq!(reasoning.first_signature(), Some("sig_01M12STREAM"));
        }
        other => panic!("second item must be the full reasoning block, got {other:?}"),
    }

    match &items[2] {
        StreamedAssistantContent::Text(text) => assert_eq!(text.text, "Berlin is 22"),
        other => panic!("third item must be the first text delta, got {other:?}"),
    }
    match &items[3] {
        StreamedAssistantContent::Text(text) => assert_eq!(text.text, " degrees and sunny."),
        other => panic!("fourth item must be the second text delta, got {other:?}"),
    }

    // Tool call: name delta first, then one delta per input_json_delta, all
    // correlated by the rig-generated internal call id.
    let (tool_id, internal_call_id) = match &items[4] {
        StreamedAssistantContent::ToolCallDelta {
            id,
            internal_call_id,
            content,
        } => {
            assert_eq!(id, "toolu_01M12STREAM");
            assert!(
                matches!(content, ToolCallDeltaContent::Name(name) if name.as_str() == "lookup_weather"),
                "tool call must start with its name, got {content:?}"
            );
            (id.clone(), internal_call_id.clone())
        }
        other => panic!("fifth item must be the tool name delta, got {other:?}"),
    };
    for (index, expected) in [(5usize, "{\"city\": \"Ber"), (6, "lin\"}")] {
        match &items[index] {
            StreamedAssistantContent::ToolCallDelta {
                id,
                internal_call_id: inner,
                content,
            } => {
                assert_eq!(id, &tool_id);
                assert_eq!(inner, &internal_call_id);
                assert!(
                    matches!(content, ToolCallDeltaContent::Delta(partial)
                        if partial.as_str() == expected),
                    "input json delta {index} must be {expected:?}, got {content:?}"
                );
            }
            other => panic!("item {index} must be an input json delta, got {other:?}"),
        }
    }
    match &items[7] {
        StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id: inner,
        } => {
            assert_eq!(
                inner, &internal_call_id,
                "final tool call keeps the delta id"
            );
            assert_eq!(tool_call.id, "toolu_01M12STREAM");
            assert!(
                tool_call.call_id.is_none(),
                "anthropic has no separate call id"
            );
            assert_eq!(tool_call.function.name, "lookup_weather");
            assert_eq!(tool_call.function.arguments, json!({"city": "Berlin"}));
        }
        other => panic!("eighth item must be the complete tool call, got {other:?}"),
    }

    match &items[8] {
        StreamedAssistantContent::Final(response) => {
            assert_eq!(response.usage.output_tokens, 19);
            assert_eq!(response.usage.input_tokens, Some(25));
            assert_eq!(response.usage.cache_creation_input_tokens, Some(11));
            assert_eq!(response.usage.cache_read_input_tokens, Some(9));
        }
        other => panic!("final item must carry the provider usage, got {other:?}"),
    }

    // ------------------------------------------------------------------
    // Wrapper state: final response captured, usage folded from
    // message_start input tokens + message_delta output/cache tokens.
    // ------------------------------------------------------------------
    let final_response = stream
        .response
        .as_ref()
        .expect("final response must be captured");
    assert_eq!(final_response.usage.output_tokens, 19);
    let usage = stream.usage();
    assert_eq!(usage.input_tokens, 25);
    assert_eq!(usage.output_tokens, 19);
    assert_eq!(usage.cached_input_tokens, 9);
    assert_eq!(usage.cache_creation_input_tokens, 11);
    assert_eq!(usage.total_tokens, 25 + 9 + 11 + 19);

    // Raw id / model / stop_reason gaps: the anthropic streaming response type
    // exposes only `usage`, and Rig never yields a `MessageId` item on this
    // path — `message_start` id/model and `message_delta` stop_reason are
    // telemetry-only in 0.40.0 (recorded into tracing spans / used solely to
    // break the read loop). A MiniCore adapter must not expect them here.
    assert!(
        stream.message_id.is_none(),
        "anthropic streaming never yields a MessageId item"
    );

    // ------------------------------------------------------------------
    // Accumulated choice: Rig 0.40 keeps BOTH the delta-accumulated
    // reasoning (no signature) and the full reasoning block (signature),
    // then text and the tool call, in provider order.
    // ------------------------------------------------------------------
    let choice: Vec<&AssistantContent> = stream.choice.iter().collect();
    assert_eq!(choice.len(), 4);
    match choice[0] {
        AssistantContent::Reasoning(reasoning) => {
            assert_eq!(
                reasoning.first_text(),
                Some("Let me reason about the weather.")
            );
            assert!(
                reasoning.first_signature().is_none(),
                "delta-accumulated reasoning carries no signature"
            );
        }
        other => panic!("choice[0] must be the delta-accumulated reasoning, got {other:?}"),
    }
    match choice[1] {
        AssistantContent::Reasoning(reasoning) => {
            assert_eq!(reasoning.first_signature(), Some("sig_01M12STREAM"));
        }
        other => panic!("choice[1] must be the full reasoning block, got {other:?}"),
    }
    match choice[2] {
        AssistantContent::Text(text) => {
            assert_eq!(text.text, "Berlin is 22 degrees and sunny.");
        }
        other => panic!("choice[2] must be the accumulated text, got {other:?}"),
    }
    match choice[3] {
        AssistantContent::ToolCall(tool_call) => {
            assert_eq!(tool_call.id, "toolu_01M12STREAM");
            assert_eq!(tool_call.function.name, "lookup_weather");
            assert_eq!(tool_call.function.arguments, json!({"city": "Berlin"}));
        }
        other => panic!("choice[3] must be the tool call, got {other:?}"),
    }
}

/// `cancel()` contract: after the first semantic item of a valid multi-event
/// body, the AbortHandle terminates the stream deterministically — the next
/// item is `None` immediately (no sleep, no timeout, no absence window).
#[tokio::test(flavor = "current_thread")]
async fn m12_anthropic_streaming_cancel_terminates_after_first_item() {
    let server = support::LoopbackServer::spawn_sse(&[(200, TERMINAL_SSE_BODY)]);
    let model = loopback_model(server.base_url());
    let mut stream = model
        .stream(weather_request(&model))
        .await
        .expect("streaming request must start against the loopback server");

    let first = stream
        .next()
        .await
        .expect("first semantic item must arrive before cancel")
        .expect("first stream item must not error");
    assert!(
        matches!(&first, StreamedAssistantContent::ReasoningDelta { reasoning, .. }
            if reasoning == "Let me reason about the weather."),
        "first item must be the initial thinking delta, got {first:?}"
    );

    stream.cancel();
    assert!(
        stream.next().await.is_none(),
        "cancel must surface as immediate stream termination"
    );

    // Settle and join the server thread before asserting.
    let captured = server.join();
    assert_eq!(
        captured.len(),
        1,
        "an aborted stream must not reconnect or re-request"
    );
}

/// Early-EOF reality: connection closes after partial semantic deltas, without
/// message_delta (stop_reason) or message_stop. Rig 0.40.0 drains the partial
/// content and then *synthesizes* a `FinalResponse` with default (zero) usage,
/// so `response` is `Some` and `usage()` is the zero sentinel — the public API
/// cannot distinguish "completed" from "dropped mid-response".
#[tokio::test(flavor = "current_thread")]
async fn m12_anthropic_streaming_early_eof_synthesizes_final_response() {
    let server = support::LoopbackServer::spawn_sse(&[(200, EARLY_EOF_SSE_BODY)]);
    let model = loopback_model(server.base_url());
    let mut stream = model
        .stream(weather_request(&model))
        .await
        .expect("streaming request must start against the loopback server");

    let mut items: Vec<StreamedAssistantContent<AnthropicStreamingResponse>> = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item.expect("stream items must not error"));
    }
    let captured = server.join();

    assert_eq!(
        captured.len(),
        1,
        "early EOF after one SSE body must make exactly one HTTP request"
    );

    // The partial semantic deltas are delivered before the close.
    assert_eq!(
        items.len(),
        5,
        "reasoning delta + full reasoning + 2 text deltas + synthesized final"
    );
    match &items[0] {
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            assert_eq!(reasoning, "Checking the forecast source.");
        }
        other => panic!("first item must be the thinking delta, got {other:?}"),
    }
    match &items[1] {
        StreamedAssistantContent::Reasoning(reasoning) => {
            assert_eq!(reasoning.first_signature(), Some("sig_01M12EOF"));
        }
        other => panic!("second item must be the full reasoning block, got {other:?}"),
    }
    match &items[3] {
        StreamedAssistantContent::Text(text) => assert_eq!(text.text, " degrees"),
        other => panic!("fourth item must be the last text delta, got {other:?}"),
    }

    // No terminal evidence reached the stream, yet a Final item is yielded:
    // Rig 0.40.0 synthesizes it with the default (all-zero) usage.
    match items.last().expect("a final item is always yielded") {
        StreamedAssistantContent::Final(response) => {
            assert_eq!(response.usage.output_tokens, 0);
            assert_eq!(response.usage.input_tokens, None);
            assert_eq!(response.usage.cache_creation_input_tokens, None);
            assert_eq!(response.usage.cache_read_input_tokens, None);
        }
        other => panic!("expected a synthesized final response, got {other:?}"),
    }

    let final_response = stream
        .response
        .as_ref()
        .expect("the synthesized final response must be captured");
    assert_eq!(final_response.usage.output_tokens, 0);
    assert_eq!(stream.usage(), Usage::new());
    assert!(
        !stream.usage().has_values(),
        "a dropped stream reports the zero usage sentinel"
    );

    // The accumulated choice keeps the partial content.
    let choice: Vec<&AssistantContent> = stream.choice.iter().collect();
    assert_eq!(choice.len(), 3);
    match choice[2] {
        AssistantContent::Text(text) => assert_eq!(text.text, "Berlin is 22 degrees"),
        other => panic!("choice[2] must be the partial text, got {other:?}"),
    }

    // Truthful behavior note: `stream.response` being `Some` here does NOT mean
    // the provider terminated the message — the connection simply ended. A
    // future MiniCore adapter must independently track terminal evidence
    // (message_delta stop_reason + message_stop) and fail closed on its absence.
}
