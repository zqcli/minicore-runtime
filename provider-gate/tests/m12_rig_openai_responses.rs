//! M12 slice: Rig 0.40.0 OpenAI Responses reality/contract probe.
//!
//! Drives the *real* `rig-core` 0.40.0 OpenAI Responses completion client
//! (`providers::openai::Client` + `ResponsesCompletionModel`) with a custom
//! `base_url` pointing at a test-owned loopback HTTP server (`tests/support`)
//! on 127.0.0.1:0. The server captures and parses the actual POST (request
//! line, headers, Content-Length-delimited body) and scripts realistic
//! provider responses.
//!
//! Contract proven here (for a future private RigProviderAdapter):
//! - request mapping: top-level `instructions`, ordered `input` items
//!   (including an assistant function call + its matching tool result),
//!   function `tools` name/description/parameters, `tool_choice`,
//!   `max_output_tokens`, structured-output schema, and reasoning parameters;
//! - response mapping/round-trip: ordered reasoning -> text -> tool-call
//!   output items, provider response/message/tool-call identities, status,
//!   usage (cached + reasoning tokens), and the raw response fields a private
//!   adapter needs;
//! - retry semantics: one completion invocation against a provider 5xx makes
//!   exactly one HTTP request (rig performs no automatic retry).

mod support;

use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::message::{ReasoningContent, ToolChoice};
use rig::completion::{
    AssistantContent, CompletionError, CompletionModel, CompletionRequest, Message, ToolDefinition,
};
use rig::providers::openai::responses_api::{
    CompletionResponse as ResponsesApiResponse, Output, OutputRole, ResponseObject, ResponseStatus,
    ResponsesUsage,
};
use rig::providers::openai::{Client, GPT_4O_MINI};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Scripted provider responses
// ---------------------------------------------------------------------------

/// A realistic OpenAI Responses API success payload: ordered reasoning,
/// assistant text message, and a function call, plus detailed usage.
const SUCCESS_RESPONSE_BODY: &str = r#"{
  "id": "resp_123",
  "object": "response",
  "created_at": 1752000000,
  "status": "completed",
  "model": "gpt-4o-mini",
  "output": [
    {
      "type": "reasoning",
      "id": "rs_1",
      "summary": [
        { "type": "summary_text", "text": "Thinking..." }
      ],
      "content": [
        { "type": "reasoning_text", "text": "Let me think step by step" }
      ],
      "status": "completed"
    },
    {
      "type": "message",
      "id": "msg_456",
      "role": "assistant",
      "status": "completed",
      "content": [
        { "type": "output_text", "text": "The weather in Paris is 22°C and sunny." }
      ]
    },
    {
      "type": "function_call",
      "id": "fc_999",
      "call_id": "call_xyz",
      "name": "get_weather",
      "arguments": "{\"city\":\"Lyon\"}",
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
}"#;

const PROVIDER_500_BODY: &str = r#"{"error":{"message":"provider exploded"}}"#;

// ---------------------------------------------------------------------------
// Completion request fixtures
// ---------------------------------------------------------------------------

/// The full contract request: preamble (lifted to top-level `instructions`),
/// ordered chat history (user -> mid-conversation system -> assistant tool
/// call -> matching tool result -> final user prompt), one function tool with
/// `tool_choice`, `max_output_tokens`, reasoning parameters and a structured
/// output schema.
fn contract_request() -> CompletionRequest {
    let output_schema: rig::schemars::Schema = serde_json::from_value(json!({
        "type": "object",
        "title": "WeatherReport",
        "properties": {
            "summary": { "type": "string", "title": "Summary" },
            "celsius": { "type": "boolean", "title": "Celsius" }
        }
    }))
    .expect("output schema must deserialize");

    CompletionRequest {
        model: None,
        preamble: Some("You are the weather oracle.".to_string()),
        chat_history: OneOrMany::many(vec![
            Message::user("What's the weather in Paris?"),
            Message::system("Mid-conversation reminder: always answer in Celsius."),
            Message::Assistant {
                id: Some("msg_prior".to_string()),
                content: OneOrMany::one(AssistantContent::tool_call_with_call_id(
                    "fc_999",
                    "call_abc".to_string(),
                    "get_weather",
                    json!({"city": "Paris"}),
                )),
            },
            Message::tool_result_with_call_id(
                "call_abc",
                Some("call_abc".to_string()),
                "22°C and sunny",
            ),
            Message::user("Summarize the weather."),
        ])
        .expect("chat history is non-empty"),
        documents: Vec::new(),
        tools: vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get the current weather for a city.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
        }],
        temperature: Some(0.2),
        max_tokens: Some(512),
        tool_choice: Some(ToolChoice::Specific {
            function_names: vec!["get_weather".to_string()],
        }),
        additional_params: Some(json!({
            "reasoning": { "effort": "low", "summary": "concise" },
        })),
        output_schema: Some(output_schema),
    }
}

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
fn openai_client(base_url: &str) -> rig::providers::openai::Client {
    Client::builder()
        .api_key("test-key")
        .base_url(base_url)
        .build()
        .expect("openai client build")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The main protocol test: real loopback server, real rig-core OpenAI
/// Responses client, full request-mapping and response round-trip assertions.
#[tokio::test]
async fn responses_loopback_round_trip_maps_request_and_response() {
    let server = support::LoopbackServer::spawn(&[(200, SUCCESS_RESPONSE_BODY)]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let outcome = model.completion(contract_request()).await;

    // Signal shutdown and join the server thread before asserting, so it can
    // never outlive the test (or hang it) whatever the outcome.
    let requests = server.join();
    let result = outcome.expect("completion must succeed");

    // --- Exactly one HTTP request for one completion invocation. ---
    assert_eq!(
        requests.len(),
        1,
        "one completion invocation must make exactly one HTTP request"
    );

    // --- Transport-level request shape. ---
    let request = &requests[0];
    assert_eq!(request.method(), "POST");
    // rig posts to `{base_url}/responses` (the OpenAI "/v1" prefix lives in
    // the provider's default base URL, which we override).
    assert_eq!(request.path(), "/responses");
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
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

    let wire: Value = request.json_body();

    // --- Top-level system/instructions mapping. ---
    assert_eq!(wire["model"], GPT_4O_MINI);
    assert_eq!(wire["instructions"], "You are the weather oracle.");
    assert_eq!(wire["max_output_tokens"], 512);
    assert_eq!(wire["temperature"], 0.2);

    // --- Ordered input items: user, mid-conversation system, assistant
    // function call, matching tool result, final user prompt. ---
    // Message input items carry the internally-tagged `"type": "message"`
    // alongside `role` — this is the exact wire shape rig 0.40.0 emits.
    assert_eq!(
        wire["input"],
        json!([
            { "type": "message", "role": "user",
              "content": [{ "type": "input_text", "text": "What's the weather in Paris?" }] },
            { "type": "message", "role": "system",
              "content": [{ "type": "input_text",
                            "text": "Mid-conversation reminder: always answer in Celsius." }] },
            { "type": "function_call",
              "id": "fc_999",
              "call_id": "call_abc",
              "name": "get_weather",
              "arguments": "{\"city\":\"Paris\"}",
              "status": "completed" },
            { "type": "function_call_output",
              "call_id": "call_abc",
              "output": "22°C and sunny",
              "status": "completed" },
            { "type": "message", "role": "user",
              "content": [{ "type": "input_text", "text": "Summarize the weather." }] }
        ])
    );

    // --- Function tool definition (name/description/parameters) + choice. ---
    assert_eq!(
        wire["tools"],
        json!([
            { "type": "function",
              "name": "get_weather",
              "description": "Get the current weather for a city.",
              "parameters": {
                  "type": "object",
                  "properties": { "city": { "type": "string" } },
                  "required": ["city"]
              } }
        ])
    );
    assert_eq!(
        wire["tool_choice"],
        json!({"type": "function", "name": "get_weather"})
    );

    // --- Reasoning parameters (+ automatic include) and structured output. ---
    assert_eq!(
        wire["reasoning"],
        json!({"effort": "low", "summary": "concise"})
    );
    assert_eq!(wire["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(wire["text"]["format"]["type"], "json_schema");
    assert_eq!(wire["text"]["format"]["name"], "WeatherReport");
    assert_eq!(wire["text"]["format"]["strict"], true);
    assert_eq!(wire["text"]["format"]["schema"]["type"], "object");
    assert_eq!(
        wire["text"]["format"]["schema"]["additionalProperties"],
        false
    );
    let required = wire["text"]["format"]["schema"]["required"]
        .as_array()
        .expect("schema required list");
    assert_eq!(
        required.len(),
        2,
        "sanitizer forces every property into required"
    );
    assert!(required.contains(&json!("summary")));
    assert!(required.contains(&json!("celsius")));
    assert_eq!(
        wire["text"]["format"]["schema"]["properties"]["summary"]["type"],
        "string"
    );

    // --- Response mapping / round-trip: ordered reasoning -> text -> tool
    // call, provider identities, status, usage. ---
    assert_eq!(result.message_id.as_deref(), Some("msg_456"));
    assert_eq!(result.usage.input_tokens, 42);
    assert_eq!(result.usage.output_tokens, 11);
    assert_eq!(result.usage.total_tokens, 53);
    assert_eq!(result.usage.cached_input_tokens, 7);
    assert_eq!(result.usage.reasoning_tokens, 4);

    let items: Vec<&AssistantContent> = result.choice.iter().collect();
    assert_eq!(
        items.len(),
        3,
        "reasoning + text + tool call in provider order"
    );

    match items[0] {
        AssistantContent::Reasoning(reasoning) => {
            assert_eq!(reasoning.id.as_deref(), Some("rs_1"));
            assert_eq!(reasoning.content.len(), 2);
            assert!(
                matches!(&reasoning.content[0], ReasoningContent::Summary(text) if text == "Thinking..."),
                "summary first, got {:?}",
                reasoning.content[0]
            );
            assert!(
                matches!(&reasoning.content[1], ReasoningContent::Text { text, signature: None }
                    if text == "Let me think step by step"),
                "reasoning text second, got {:?}",
                reasoning.content[1]
            );
        }
        other => panic!("expected reasoning output first, got {other:?}"),
    }
    match items[1] {
        AssistantContent::Text(text) => {
            assert_eq!(text.text, "The weather in Paris is 22°C and sunny.");
        }
        other => panic!("expected text output second, got {other:?}"),
    }
    match items[2] {
        AssistantContent::ToolCall(call) => {
            assert_eq!(call.id, "fc_999");
            assert_eq!(call.call_id.as_deref(), Some("call_xyz"));
            assert_eq!(call.function.name, "get_weather");
            assert_eq!(call.function.arguments, json!({"city": "Lyon"}));
        }
        other => panic!("expected function call output third, got {other:?}"),
    }

    // --- Raw response fields a future private adapter consumes. ---
    let raw: &ResponsesApiResponse = &result.raw_response;
    assert_eq!(raw.id, "resp_123");
    assert!(matches!(raw.object, ResponseObject::Response));
    assert_eq!(raw.created_at, 1_752_000_000);
    assert_eq!(raw.status, ResponseStatus::Completed);
    assert_eq!(raw.model, GPT_4O_MINI);
    assert!(raw.error.is_none());
    assert!(raw.incomplete_details.is_none());
    assert_eq!(raw.output.len(), 3);
    assert!(matches!(&raw.output[0], Output::Reasoning { id, .. } if id == "rs_1"));
    match &raw.output[1] {
        Output::Message(message) => {
            assert_eq!(message.id, "msg_456");
            assert!(matches!(message.role, OutputRole::Assistant));
            assert_eq!(message.status, ResponseStatus::Completed);
        }
        other => panic!("expected output message second, got {other:?}"),
    }
    assert!(matches!(&raw.output[2], Output::FunctionCall(call)
        if call.call_id == "call_xyz" && call.name == "get_weather"));
    let usage: &ResponsesUsage = raw.usage.as_ref().expect("usage present");
    assert_eq!(usage.input_tokens, 42);
    assert_eq!(usage.output_tokens, 11);
    assert_eq!(usage.total_tokens, 53);
    assert_eq!(
        usage
            .input_tokens_details
            .as_ref()
            .expect("input details")
            .cached_tokens,
        7
    );
    assert_eq!(
        usage
            .output_tokens_details
            .as_ref()
            .expect("output details")
            .reasoning_tokens,
        4
    );
}

/// Provider 5xx: exactly one HTTP request, no automatic retry, and the error
/// preserves status + body for the future adapter.
#[tokio::test]
async fn responses_provider_5xx_makes_exactly_one_http_request() {
    // Script two 500s so an incorrect retry would be served and counted
    // instead of hanging the test on a queued-but-unserved connection.
    let server =
        support::LoopbackServer::spawn(&[(500, PROVIDER_500_BODY), (500, PROVIDER_500_BODY)]);
    let model = openai_client(server.base_url()).completion_model(GPT_4O_MINI);
    let outcome = model.completion(simple_request()).await;

    let requests = server.join();
    let err = outcome.expect_err("provider 5xx must surface as a completion error");

    assert_eq!(
        requests.len(),
        1,
        "one completion invocation against a provider 5xx must make exactly \
         one HTTP request (rig performs no automatic retry)"
    );

    assert!(
        matches!(err, CompletionError::HttpError(_)),
        "expected an HTTP error, got {err:?}"
    );
    assert_eq!(
        err.provider_response_status().map(|s| s.as_u16()),
        Some(500),
        "error must preserve the provider status"
    );
    let body = err
        .provider_response_body()
        .expect("error must preserve the provider body");
    assert!(
        body.contains("provider exploded"),
        "unexpected body: {body}"
    );
}
