//! M12 slice: Rig 0.40 Anthropic Messages reality/contract.
//!
//! Exercises the *real* Rig Anthropic client and completion model against a
//! deterministic loopback HTTP server on `127.0.0.1:0` (`tests/support`) plus
//! Rig's own `test-utils` recording client for the cache-mode variants.
//! Everything here is offline, bounded and deterministic: no external
//! network, no credentials, no sleeps, no timeout-based absence proofs, and
//! every test-owned thread is joined before assertions.
//!
//! Contract covered:
//! - POST /v1/messages request line, `anthropic-version` and auth header
//!   presence (values never printed), `Content-Length` honored body capture;
//! - system preamble + history system mapping into the `system` block array;
//! - ordered user / assistant / tool-use / tool-result messages, including a
//!   `thinking` block with signature in history;
//! - `ToolDefinition` name/description/`input_schema` mapping;
//! - explicit `max_tokens` and `tool_choice` mapping;
//! - manual prompt caching (`with_prompt_caching`) markers on system / last
//!   tool / last message block, and automatic caching
//!   (`with_automatic_caching` / `with_automatic_caching_1h`) top-level
//!   `cache_control`;
//! - a valid Anthropic response with ordered thinking / text / tool_use blocks
//!   carrying a signature, preserving response id / model / stop reason / tool
//!   call identity and usage including cache read and cache creation tokens,
//!   through both the generic and the raw response;
//! - exactly one HTTP request per completion invocation, including on a
//!   provider 5xx (Rig's unary path has no automatic retry).

mod support;

use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::{
    CompletionError, CompletionModel as _, ToolDefinition,
    message::{AssistantContent, Message, Reasoning, ToolChoice},
};
use rig::providers::anthropic::Client;
use rig::providers::anthropic::completion::{ANTHROPIC_VERSION_LATEST, CLAUDE_SONNET_4_6};
use rig::test_utils::RecordingHttpClient;
use serde_json::{Value, json};

/// Scripted Anthropic Messages response: ordered thinking / text / tool_use
/// blocks with a signature, a tool_use stop reason, and cache usage fields.
const ANTHROPIC_MESSAGES_200_BODY: &str = r#"{
  "id": "msg_01M12LOOPBACK",
  "type": "message",
  "role": "assistant",
  "model": "claude-sonnet-4-6",
  "content": [
    {"type": "thinking", "thinking": "Let me verify the weather data.", "signature": "sig_01M12RESPONSE"},
    {"type": "text", "text": "Berlin is 22 degrees and sunny."},
    {"type": "tool_use", "id": "toolu_01M12RESPONSE", "name": "lookup_weather", "input": {"city": "Berlin"}}
  ],
  "stop_reason": "tool_use",
  "stop_sequence": null,
  "usage": {
    "input_tokens": 25,
    "output_tokens": 19,
    "cache_creation_input_tokens": 11,
    "cache_read_input_tokens": 9
  }
}"#;

/// Scripted Anthropic provider error body served for the 5xx contract test.
const ANTHROPIC_500_BODY: &str =
    r#"{"type":"error","error":{"type":"api_error","message":"m12 simulated provider failure"}}"#;

/// Main mapping test: a real loopback exchange through the real Rig Anthropic
/// client (reqwest backend) against `127.0.0.1:0`.
#[tokio::test(flavor = "current_thread")]
async fn m12_loopback_anthropic_messages_request_mapping_and_response_contract() {
    let server = support::LoopbackServer::spawn(&[(200, ANTHROPIC_MESSAGES_200_BODY)]);

    let client = Client::builder()
        .api_key("sk-m12-loopback")
        .base_url(server.base_url())
        .build()
        .expect("rig anthropic client must build against the loopback base url");

    let model = client
        .completion_model(CLAUDE_SONNET_4_6)
        .with_prompt_caching();

    let assistant_history = Message::Assistant {
        id: None,
        content: OneOrMany::many(vec![
            AssistantContent::Reasoning(Reasoning::new_with_signature(
                "I should check the weather service.",
                Some("sig_01M12HISTORY".to_string()),
            )),
            AssistantContent::text("Let me look that up."),
            AssistantContent::tool_call(
                "toolu_01M12HISTORY",
                "lookup_weather",
                json!({"city": "Berlin"}),
            ),
        ])
        .expect("assistant history content must be non-empty"),
    };

    let request = model
        .completion_request("Now summarize the weather.")
        .preamble("M12 loopback preamble.".to_string())
        .message(Message::system("You are the M12 loopback assistant."))
        .message(Message::user("What is the weather in Berlin?"))
        .message(assistant_history)
        .message(Message::tool_result(
            "toolu_01M12HISTORY",
            "22 degrees, sunny",
        ))
        .tool(ToolDefinition {
            name: "lookup_weather".to_string(),
            description: "Look up the current weather for a city.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        })
        .tool(ToolDefinition {
            name: "lookup_time".to_string(),
            description: "Look up the current time for a city.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        })
        .tool_choice(ToolChoice::Specific {
            function_names: vec!["lookup_weather".to_string()],
        })
        .max_tokens(321)
        .build();

    let response = model.completion(request).await;

    // Settle the server thread before asserting: the poison connection makes
    // the accept loop exit, and join propagates any server-side panic.
    let captured = server.join();
    let response = response.expect("loopback completion must succeed");

    // ------------------------------------------------------------------
    // Exactly one HTTP request per completion invocation.
    // ------------------------------------------------------------------
    assert_eq!(
        captured.len(),
        1,
        "one completion invocation must make exactly one HTTP request"
    );
    let request = &captured[0];

    // ------------------------------------------------------------------
    // Request line, headers (values never printed), Content-Length.
    // ------------------------------------------------------------------
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/v1/messages");

    assert_header(request, "anthropic-version", ANTHROPIC_VERSION_LATEST);
    assert_header(request, "content-type", "application/json");
    let api_key = request
        .header("x-api-key")
        .expect("x-api-key auth header must be present");
    assert!(
        api_key == "sk-m12-loopback",
        "x-api-key auth header must carry the configured test key"
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

    // ------------------------------------------------------------------
    // Request body: model / max_tokens / tool_choice / system mapping.
    // ------------------------------------------------------------------
    let body = request.json_body();
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["max_tokens"], 321);
    assert_eq!(
        body["tool_choice"],
        json!({"type": "tool", "name": "lookup_weather"})
    );
    assert!(body.get("temperature").is_none());

    // Preamble and history system messages hoist into the `system` block
    // array; manual prompt caching marks only the last system block.
    assert_eq!(
        body["system"],
        json!([
            {"type": "text", "text": "M12 loopback preamble."},
            {
                "type": "text",
                "text": "You are the M12 loopback assistant.",
                "cache_control": {"type": "ephemeral"}
            }
        ])
    );

    // ------------------------------------------------------------------
    // Ordered user / assistant (thinking + text + tool_use) / tool_result
    // messages, with the signature preserved in the history thinking block.
    // ------------------------------------------------------------------
    let messages = body["messages"]
        .as_array()
        .expect("messages must be an array");
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[0]["content"][0],
        json!({"type": "text", "text": "What is the weather in Berlin?"})
    );
    assert_eq!(messages[1]["role"], "assistant");
    let assistant_blocks = messages[1]["content"]
        .as_array()
        .expect("assistant content must be an array");
    assert_eq!(assistant_blocks.len(), 3);
    assert_eq!(
        assistant_blocks[0],
        json!({
            "type": "thinking",
            "thinking": "I should check the weather service.",
            "signature": "sig_01M12HISTORY"
        })
    );
    assert_eq!(
        assistant_blocks[1],
        json!({"type": "text", "text": "Let me look that up."})
    );
    assert_eq!(
        assistant_blocks[2],
        json!({
            "type": "tool_use",
            "id": "toolu_01M12HISTORY",
            "name": "lookup_weather",
            "input": {"city": "Berlin"}
        })
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"][0],
        json!({
            "type": "tool_result",
            "tool_use_id": "toolu_01M12HISTORY",
            "content": [{"type": "text", "text": "22 degrees, sunny"}]
        })
    );
    assert_eq!(messages[3]["role"], "user");
    assert_eq!(messages[3]["content"][0]["type"], "text");
    assert_eq!(
        messages[3]["content"][0]["text"],
        "Now summarize the weather."
    );
    // Manual prompt caching marks the last content block of the last message.
    assert_eq!(
        messages[3]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );

    // ------------------------------------------------------------------
    // ToolDefinition mapping; manual caching marks only the last tool.
    // ------------------------------------------------------------------
    let tools = body["tools"].as_array().expect("tools must be an array");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "lookup_weather");
    assert_eq!(
        tools[0]["description"],
        "Look up the current weather for a city."
    );
    assert_eq!(
        tools[0]["input_schema"],
        json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        })
    );
    assert!(
        tools[0].get("cache_control").is_none(),
        "only the last tool may carry the manual cache marker"
    );
    assert_eq!(tools[1]["name"], "lookup_time");
    assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));

    // ------------------------------------------------------------------
    // Response: raw provider response preserves id / model / stop reason,
    // reasoning signature, tool call identity/order and cache usage.
    // ------------------------------------------------------------------
    let raw = &response.raw_response;
    assert_eq!(raw.id, "msg_01M12LOOPBACK");
    assert_eq!(raw.model, "claude-sonnet-4-6");
    assert_eq!(raw.role, "assistant");
    assert_eq!(raw.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(raw.usage.input_tokens, 25);
    assert_eq!(raw.usage.output_tokens, 19);
    assert_eq!(raw.usage.cache_read_input_tokens, Some(9));
    assert_eq!(raw.usage.cache_creation_input_tokens, Some(11));

    // Generic response usage folds cache read/creation into the typed fields.
    assert_eq!(response.usage.input_tokens, 25);
    assert_eq!(response.usage.output_tokens, 19);
    assert_eq!(response.usage.cached_input_tokens, 9);
    assert_eq!(response.usage.cache_creation_input_tokens, 11);
    assert_eq!(response.usage.total_tokens, 25 + 9 + 11 + 19);

    // Ordered generic choice: reasoning (with signature) -> text -> tool call.
    let mut blocks = response.choice.iter();
    match blocks.next().expect("first response block") {
        AssistantContent::Reasoning(reasoning) => {
            assert_eq!(
                reasoning.first_text(),
                Some("Let me verify the weather data.")
            );
            assert_eq!(reasoning.first_signature(), Some("sig_01M12RESPONSE"));
        }
        other => panic!("first response block must be reasoning, got {other:?}"),
    }
    match blocks.next().expect("second response block") {
        AssistantContent::Text(text) => {
            assert_eq!(text.text, "Berlin is 22 degrees and sunny.");
        }
        other => panic!("second response block must be text, got {other:?}"),
    }
    match blocks.next().expect("third response block") {
        AssistantContent::ToolCall(tool_call) => {
            assert_eq!(tool_call.id, "toolu_01M12RESPONSE");
            assert_eq!(tool_call.function.name, "lookup_weather");
            assert_eq!(tool_call.function.arguments, json!({"city": "Berlin"}));
        }
        other => panic!("third response block must be a tool call, got {other:?}"),
    }
    assert!(
        blocks.next().is_none(),
        "response must have exactly three blocks"
    );
}

/// Cache-mode contract, through Rig's recording client (no network):
/// automatic caching emits a top-level `cache_control`; the 1-hour variant
/// adds the TTL; manual prompt caching combined with automatic caching keeps
/// the top-level breakpoint plus the manual system and last-tool markers,
/// while message blocks carry no per-block markers.
#[tokio::test(flavor = "current_thread")]
async fn m12_cache_control_mode_contract() {
    // Automatic caching: top-level `cache_control`, no per-block markers.
    let http_client = RecordingHttpClient::new(ANTHROPIC_MESSAGES_200_BODY);
    let model = recorded_client(http_client.clone())
        .completion_model(CLAUDE_SONNET_4_6)
        .with_automatic_caching();
    let response = model
        .completion(model.completion_request("ping").max_tokens(16).build())
        .await
        .expect("recorded completion must succeed");
    assert_eq!(response.raw_response.id, "msg_01M12LOOPBACK");
    let body = recorded_body(&http_client);
    assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
    assert_no_block_markers(&body);

    // 1-hour automatic caching: the TTL rides on the top-level field.
    let http_client = RecordingHttpClient::new(ANTHROPIC_MESSAGES_200_BODY);
    let model = recorded_client(http_client.clone())
        .completion_model(CLAUDE_SONNET_4_6)
        .with_automatic_caching_1h();
    let response = model
        .completion(model.completion_request("ping").max_tokens(16).build())
        .await
        .expect("recorded completion must succeed");
    assert_eq!(response.raw_response.id, "msg_01M12LOOPBACK");
    let body = recorded_body(&http_client);
    assert_eq!(
        body["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );

    // Manual + automatic: top-level breakpoint plus manual system and
    // last-tool markers; the top-level breakpoint owns the moving message
    // cache point.
    let http_client = RecordingHttpClient::new(ANTHROPIC_MESSAGES_200_BODY);
    let model = recorded_client(http_client.clone())
        .completion_model(CLAUDE_SONNET_4_6)
        .with_prompt_caching()
        .with_automatic_caching();
    let request = model
        .completion_request("ping")
        .preamble("Combined caching preamble.".to_string())
        .tool(ToolDefinition {
            name: "lookup_weather".to_string(),
            description: "Look up the current weather for a city.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        })
        .tool(ToolDefinition {
            name: "lookup_time".to_string(),
            description: "Look up the current time for a city.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        })
        .max_tokens(16)
        .build();
    let response = model
        .completion(request)
        .await
        .expect("recorded completion must succeed");
    assert_eq!(response.raw_response.id, "msg_01M12LOOPBACK");

    let body = recorded_body(&http_client);
    assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));

    let system = body["system"].as_array().expect("system blocks array");
    assert_eq!(system.len(), 1);
    assert_eq!(
        system[0],
        json!({
            "type": "text",
            "text": "Combined caching preamble.",
            "cache_control": {"type": "ephemeral"}
        })
    );

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));

    assert_no_block_markers(&body);
}

/// A provider 5xx surfaces as an error after exactly one HTTP request:
/// Rig's unary completion path performs no automatic retry.
#[tokio::test(flavor = "current_thread")]
async fn m12_provider_5xx_makes_exactly_one_request_without_retry() {
    // Script two 500s so an incorrect retry would be served and counted.
    let server =
        support::LoopbackServer::spawn(&[(500, ANTHROPIC_500_BODY), (500, ANTHROPIC_500_BODY)]);

    let client = Client::builder()
        .api_key("sk-m12-5xx")
        .base_url(server.base_url())
        .build()
        .expect("rig anthropic client must build against the loopback base url");

    let model = client.completion_model(CLAUDE_SONNET_4_6);
    let request = model.completion_request("hello").max_tokens(16).build();

    let error = model.completion(request).await;

    let captured = server.join();
    let error = error.expect_err("a provider 5xx must surface as an error");

    assert!(
        matches!(error, CompletionError::HttpError(_)),
        "expected CompletionError::HttpError, got {error:?}"
    );
    assert_eq!(
        error
            .provider_response_status()
            .map(|status| status.as_u16()),
        Some(500),
        "the provider status must be preserved"
    );

    assert_eq!(
        captured.len(),
        1,
        "provider 5xx must produce exactly one HTTP request (automatic retry = 0)"
    );
    assert_eq!(captured[0].path(), "/v1/messages");
}

/// Assert a header is present with exactly the expected value, without
/// echoing either value (e.g. the auth key) in a failure message.
fn assert_header(captured: &support::CapturedRequest, name: &str, expected: &str) {
    let value = captured
        .header(name)
        .unwrap_or_else(|| panic!("missing HTTP header `{name}`"));
    assert!(
        value == expected,
        "HTTP header `{name}` did not match the expected value"
    );
}

/// Anthropic client over Rig's recording HTTP backend (no network).
fn recorded_client<H: rig::http_client::HttpClientExt>(http_client: H) -> Client<H> {
    Client::builder()
        .api_key("sk-m12-recorded")
        .base_url("http://rig-m12.invalid")
        .http_client(http_client)
        .build()
        .expect("rig anthropic client over the recording http client")
}

/// Assert the recording client captured exactly one well-formed request to
/// the loopback base URL and return its parsed JSON body.
fn recorded_body(http_client: &RecordingHttpClient) -> Value {
    let captured = http_client.requests();
    assert_eq!(
        captured.len(),
        1,
        "one completion invocation must make exactly one HTTP request"
    );
    let captured = &captured[0];
    assert!(
        captured.uri.starts_with("http://rig-m12.invalid"),
        "unexpected recorded uri: {}",
        captured.uri
    );
    assert!(
        captured.uri.ends_with("/v1/messages"),
        "unexpected recorded uri: {}",
        captured.uri
    );
    assert_eq!(
        captured
            .headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some(ANTHROPIC_VERSION_LATEST),
        "anthropic-version header must be present and match the client default"
    );
    serde_json::from_slice(&captured.body).expect("captured request body must be JSON")
}

/// Assert no message content block carries a `cache_control` marker.
fn assert_no_block_markers(body: &Value) {
    for message in body["messages"].as_array().expect("messages array") {
        for block in message["content"].as_array().expect("content blocks array") {
            assert!(
                block.get("cache_control").is_none(),
                "automatic caching must not add per-block markers"
            );
        }
    }
}
