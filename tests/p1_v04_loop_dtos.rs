//! Phase 1 v0.4 contracts: loop history and execution configuration DTOs.
//!
//! These tests pin the new capability surface (LoopId, history items/view,
//! ExecutionConfig, PromptProvider, LoopOptions, LoopReport) without touching
//! the v0.3 runner or storage semantics.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{
    AssistantPart, Model, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind,
    ModelFinishReason, ModelMessage, ModelRequest, ModelStartFuture, ReasoningPreference, Usage,
};
use minicore_runtime::prompt_provider::{
    PreparedPrompt, PromptError, PromptFuture, PromptProvider, PromptRequest,
};
use minicore_runtime::tools::{
    Tool, ToolContext, ToolExecutionOutcome, ToolFuture, ToolInvocation, ToolName, ToolOutput,
    ToolResultOutcome, ToolSet, ToolSpec,
};
use minicore_runtime::value::BoundedText;
use minicore_runtime::{
    AssistantHistory, CancelReason, ConfigRevision, ExecutionConfig, ExecutionConfigError,
    HistoryItem, HistoryView, LoopFailure, LoopFailureKind, LoopId, LoopLimits, LoopLimitsError,
    LoopOptions, LoopOutcome, LoopReport, LoopStartError, SummaryHistory, ToolCallId,
    ToolResultHistory, UserHistory, UserInput, UserMessageKind,
};

struct FakeModel {
    descriptor: ModelDescriptor,
}

impl Model for FakeModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(async {
            Err(ModelError::unknown(
                ModelErrorKind::Internal,
                DiagnosticSummary::new(
                    DiagnosticCode::Internal,
                    DiagnosticCategory::Internal,
                    BoundedText::new("fake model start is not exercised here").unwrap(),
                    false,
                ),
            ))
        })
    }
}

struct FakeTool {
    spec: ToolSpec,
}

impl Tool for FakeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async {
            Ok(ToolExecutionOutcome::Completed(
                ToolOutput::new("done").unwrap(),
            ))
        })
    }
}

struct CapturingPrompt {
    system: Option<BoundedText>,
}

impl PromptProvider for CapturingPrompt {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
        Box::pin(async move {
            let mut messages = Vec::new();
            if let Some(system) = &self.system {
                messages.push(ModelMessage::system(system.as_str()).unwrap());
            }
            for item in request.history.iter() {
                match item {
                    HistoryItem::User(user) => {
                        messages.push(ModelMessage::user(user.input.as_text()).unwrap())
                    }
                    HistoryItem::Assistant(assistant) => {
                        messages.push(ModelMessage::assistant(assistant.content.clone()).unwrap())
                    }
                    HistoryItem::ToolResult(result) => messages.push(
                        ModelMessage::tool_with_outcome(
                            result.call_id.clone(),
                            result.output.clone(),
                            result.outcome,
                        )
                        .unwrap(),
                    ),
                    HistoryItem::Summary(summary) => messages.push(
                        ModelMessage::system(format!("summary: {}", summary.content.as_str()))
                            .unwrap(),
                    ),
                }
            }
            if messages.is_empty() {
                return Err(PromptError::EmptyPrompt);
            }
            Ok(PreparedPrompt { messages })
        })
    }
}

fn text_model(reasoning: &[ReasoningPreference], supports_tools: bool) -> Arc<dyn Model> {
    let descriptor = ModelDescriptor::new(
        "fake/text-model".parse().unwrap(),
        8192,
        reasoning.iter().copied().collect(),
        supports_tools,
    )
    .unwrap();
    Arc::new(FakeModel { descriptor })
}

fn fake_tools() -> ToolSet {
    let spec = ToolSpec::new(
        "echo".parse().unwrap(),
        "echoes its input back",
        json!({"type": "object"}),
    )
    .unwrap();
    let mut builder = ToolSet::builder();
    builder.register(FakeTool { spec });
    builder.build().unwrap()
}

fn sample_items(loop_id: LoopId) -> (HistoryItem, HistoryItem, HistoryItem, HistoryItem) {
    let user = HistoryItem::User(UserHistory {
        loop_id,
        kind: UserMessageKind::Prompt,
        input: UserInput::text("Fix the parser").unwrap(),
    });
    let assistant = HistoryItem::Assistant(AssistantHistory {
        loop_id,
        request_index: 0,
        model: "fake/text-model".parse().unwrap(),
        reasoning: ReasoningPreference::High,
        content: vec![AssistantPart::Text("done".into())],
        finish_reason: ModelFinishReason::Stop,
        usage: Usage::new(1, 2, 0),
    });
    let tool_result = HistoryItem::ToolResult(ToolResultHistory {
        loop_id,
        request_index: 0,
        call_id: ToolCallId::new("call_1").unwrap(),
        tool_name: "echo".parse().unwrap(),
        outcome: ToolResultOutcome::Success,
        output: ToolOutput::new("ok").unwrap(),
    });
    let summary = HistoryItem::Summary(SummaryHistory {
        content: BoundedText::new("earlier context").unwrap(),
    });
    (user, assistant, tool_result, summary)
}

#[test]
fn loop_id_is_canonical_random_prefixed_hex() {
    let id = LoopId::new().expect("the test entropy source is available");
    let encoded = id.to_string();
    assert_eq!(encoded.len(), "lup_".len() + 32);
    assert!(encoded.starts_with("lup_"));
    assert!(
        encoded["lup_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    assert_eq!(LoopId::from_str(&encoded).unwrap(), id);
    assert_ne!(id.to_string(), LoopId::new().unwrap().to_string());

    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, format!("\"{encoded}\""));
    assert_eq!(serde_json::from_str::<LoopId>(&json).unwrap(), id);

    let zero = "lup_00000000000000000000000000000000";
    assert!(
        LoopId::from_str(zero).is_err(),
        "zero payload must be rejected"
    );
}

#[test]
fn history_items_serialize_with_typed_tags_and_round_trip() {
    let loop_id = LoopId::new().unwrap();
    let (user, assistant, tool_result, summary) = sample_items(loop_id);

    assert_eq!(
        serde_json::to_value(&user).unwrap(),
        json!({
            "type": "user",
            "data": {
                "loop_id": loop_id.to_string(),
                "kind": "prompt",
                "input": { "text": "Fix the parser" },
            }
        })
    );
    assert_eq!(
        serde_json::to_value(&assistant).unwrap()["type"],
        json!("assistant")
    );
    assert_eq!(
        serde_json::to_value(&tool_result).unwrap()["type"],
        json!("tool_result")
    );
    assert_eq!(
        serde_json::to_value(&summary).unwrap()["type"],
        json!("summary")
    );

    for item in [&user, &assistant, &tool_result, &summary] {
        let encoded = serde_json::to_string(item).unwrap();
        assert_eq!(
            &serde_json::from_str::<HistoryItem>(&encoded).expect("history item must deserialize"),
            item,
            "round trip mismatch for {encoded}"
        );
    }
}

#[test]
fn history_view_projects_base_and_appended_in_order() {
    let loop_id = LoopId::new().unwrap();
    let (user, assistant, _, _) = sample_items(loop_id);
    let base = vec![user.clone()];
    let appended = vec![assistant.clone()];

    let view = HistoryView::new(&base, &appended);
    assert_eq!(view.base(), &base[..]);
    assert_eq!(view.appended(), &appended[..]);
    assert_eq!(view.len(), 2);
    assert!(!view.is_empty());

    let forward: Vec<_> = view.iter().collect();
    assert_eq!(forward, vec![&base[0], &appended[0]]);

    let mut reversed = view.iter().rev();
    assert_eq!(reversed.next(), Some(&appended[0]));
    assert_eq!(reversed.next(), Some(&base[0]));

    let empty = HistoryView::new(&[][..], &[][..]);
    assert_eq!(empty.len(), 0);
    assert!(empty.is_empty());
}

#[test]
fn loop_limits_default_is_valid_and_bad_values_fail() {
    assert!(LoopLimits::default().validate().is_ok());

    let zero_history = LoopLimits {
        max_history_items: 0,
        ..LoopLimits::default()
    };
    assert_eq!(zero_history.validate(), Err(LoopLimitsError::InvalidBounds));

    let zero_output = LoopLimits {
        max_tool_output_bytes: 0,
        ..LoopLimits::default()
    };
    assert_eq!(zero_output.validate(), Err(LoopLimitsError::InvalidBounds));
}

#[test]
fn loop_options_default_checked_and_rejects_out_of_bounds_fields() {
    let options = LoopOptions::default_checked().expect("defaults must validate");
    assert!(options.validate().is_ok());

    let mut invalid = options.clone();
    invalid.max_pending_steers = 0;
    assert_eq!(invalid.validate(), Err(LoopStartError::InvalidOptions));

    let mut invalid = options.clone();
    invalid.max_pending_steers = 65;
    assert_eq!(invalid.validate(), Err(LoopStartError::InvalidOptions));

    let mut invalid = options.clone();
    invalid.model_retry_attempts = 0;
    assert_eq!(invalid.validate(), Err(LoopStartError::InvalidOptions));

    let mut invalid = options.clone();
    invalid.tool_timeout = Duration::ZERO;
    assert_eq!(invalid.validate(), Err(LoopStartError::InvalidOptions));

    let mut invalid = options.clone();
    invalid.limits.max_history_bytes = 0;
    assert_eq!(invalid.validate(), Err(LoopStartError::InvalidOptions));

    let mut invalid = options.clone();
    invalid.deadline = Some(
        tokio::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap(),
    );
    assert_eq!(invalid.validate(), Err(LoopStartError::InvalidOptions));
}

#[test]
fn config_revision_is_initial_zero_and_round_trips() {
    assert_eq!(ConfigRevision::INITIAL.as_u64(), 0);
    assert_eq!(ConfigRevision::new(7).as_u64(), 7);
    let json = serde_json::to_string(&ConfigRevision::new(7)).unwrap();
    assert_eq!(
        serde_json::from_str::<ConfigRevision>(&json).unwrap(),
        ConfigRevision::new(7)
    );
}

#[test]
fn execution_config_accepts_a_text_only_model() {
    let model = text_model(&[ReasoningPreference::Auto], false);
    let config = ExecutionConfig::new(
        model,
        ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(CapturingPrompt { system: None }),
    )
    .expect("text model with no tools must be a valid snapshot");

    assert_eq!(config.reasoning(), ReasoningPreference::Auto);
    assert_eq!(config.descriptor().model_ref.as_str(), "fake/text-model");
    assert!(config.policy().is_none());
    assert!(config.tools().specs_for(&Default::default()).is_empty());
    // Getter borrows hold across all snapshot parts without copying.
    let prompt = Arc::clone(config.prompt());
    assert!(Arc::ptr_eq(config.prompt(), &prompt));
}

#[test]
fn execution_config_accepts_tool_capable_model_with_tools() {
    let model = text_model(
        &[ReasoningPreference::Auto, ReasoningPreference::High],
        true,
    );
    let config = ExecutionConfig::new(
        model,
        ReasoningPreference::High,
        fake_tools(),
        None,
        Arc::new(CapturingPrompt { system: None }),
    )
    .expect("tool-capable model with tools must be valid");
    assert!(
        config
            .tools()
            .contains(&"echo".parse::<ToolName>().unwrap())
    );
}

#[test]
fn execution_config_rejects_untrusted_reasoning() {
    let model = text_model(&[ReasoningPreference::Auto], false);
    let error = ExecutionConfig::new(
        model,
        ReasoningPreference::High,
        ToolSet::default(),
        None,
        Arc::new(CapturingPrompt { system: None }),
    )
    .err()
    .unwrap();
    assert_eq!(error, ExecutionConfigError::UnsupportedReasoning);
}

#[test]
fn execution_config_rejects_tools_without_model_support() {
    let model = text_model(&[ReasoningPreference::Auto], false);
    let error = ExecutionConfig::new(
        model,
        ReasoningPreference::Auto,
        fake_tools(),
        None,
        Arc::new(CapturingPrompt { system: None }),
    )
    .err()
    .unwrap();
    assert_eq!(error, ExecutionConfigError::ToolsUnsupported);
}

#[test]
fn execution_config_rejects_an_invalid_descriptor() {
    let descriptor = ModelDescriptor {
        model_ref: "fake/broken".parse().unwrap(),
        context_window: 0,
        supported_reasoning: BTreeSet::from([ReasoningPreference::Auto]),
        supports_tools: false,
    };
    let model: Arc<dyn Model> = Arc::new(FakeModel { descriptor });
    let error = ExecutionConfig::new(
        model,
        ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(CapturingPrompt { system: None }),
    )
    .err()
    .unwrap();
    assert_eq!(error, ExecutionConfigError::InvalidDescriptor);
}

#[tokio::test]
async fn prompt_provider_receives_the_full_history_view() {
    let loop_id = LoopId::new().unwrap();
    let (user, assistant, _, summary) = sample_items(loop_id);
    let base = vec![user, summary];
    let appended = vec![assistant];
    let descriptor = ModelDescriptor::new(
        "fake/text-model".parse().unwrap(),
        8192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap();

    let provider = CapturingPrompt {
        system: Some(BoundedText::new("be concise").unwrap()),
    };
    let request = PromptRequest {
        loop_id,
        request_index: 0,
        history: HistoryView::new(&base, &appended),
        model: &descriptor,
        reasoning: ReasoningPreference::Auto,
        tools: &[],
        cancellation: CancellationToken::new(),
        deadline: std::time::Instant::now() + Duration::from_secs(30),
    };

    let prepared = provider
        .prepare(request)
        .await
        .expect("prepare must succeed");
    assert_eq!(prepared.messages.len(), 4);
    assert!(matches!(
        &prepared.messages[1],
        ModelMessage::User(text) if text == "Fix the parser"
    ));
    // base = [user, summary], appended = [assistant]; the summary lands between.
    assert!(
        matches!(&prepared.messages[2], ModelMessage::System(text) if text.starts_with("summary:"))
    );
    assert!(matches!(&prepared.messages[3], ModelMessage::Assistant(_)));
}

#[tokio::test]
async fn prompt_provider_rejects_an_empty_prompt() {
    let loop_id = LoopId::new().unwrap();
    let descriptor = ModelDescriptor::new(
        "fake/text-model".parse().unwrap(),
        8192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap();
    let provider = CapturingPrompt { system: None };
    let request = PromptRequest {
        loop_id,
        request_index: 0,
        history: HistoryView::new(&[][..], &[][..]),
        model: &descriptor,
        reasoning: ReasoningPreference::Auto,
        tools: &[],
        cancellation: CancellationToken::new(),
        deadline: std::time::Instant::now() + Duration::from_secs(30),
    };
    let result = provider.prepare(request).await;
    assert!(matches!(result, Err(PromptError::EmptyPrompt)));
}

#[test]
fn loop_report_and_outcome_are_constructible() {
    let report = LoopReport {
        loop_id: LoopId::new().unwrap(),
        outcome: LoopOutcome::Cancelled(CancelReason::User),
        appended: Arc::new([]),
        usage: Usage::new(1, 2, 0),
        requests: 1,
        tool_rounds: 0,
        final_config_revision: ConfigRevision::INITIAL,
    };
    assert_eq!(report.requests, 1);

    let failure = LoopFailure {
        kind: LoopFailureKind::Model,
        diagnostic: DiagnosticSummary::new(
            DiagnosticCode::ModelUnavailable,
            DiagnosticCategory::Model,
            BoundedText::new("provider down").unwrap(),
            true,
        ),
    };
    assert_eq!(
        LoopOutcome::Failed(failure.clone()),
        LoopOutcome::Failed(LoopFailure {
            kind: LoopFailureKind::Model,
            diagnostic: failure.diagnostic,
        })
    );
}
