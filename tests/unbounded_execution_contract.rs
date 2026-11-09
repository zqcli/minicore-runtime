mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use minicore_runtime::model::{
    AssistantPart, Model, ModelCallContext, ModelDescriptor, ModelEvent, ModelFinishReason,
    ModelMessage, ModelRequest, ModelStartFuture, ProviderReplay, ReasoningContent,
    ReasoningPreference, Usage,
};
use minicore_runtime::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, Tool, ToolContext, ToolDecision,
    ToolExecutionOutcome, ToolFuture, ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest,
    ToolInvocation, ToolOutput, ToolPolicy, ToolPolicyFuture, ToolPolicyRequest, ToolProgress,
    ToolResultOutcome, ToolSet, ToolSpec,
};
use minicore_runtime::{
    AgentLoop, AssistantHistory, HistoryFuture, HistoryItem, InteractionAnswer, InteractionKind,
    LoopEvent, LoopHistory, LoopId, LoopOptions, LoopOutcome, OutputChannel, SummaryHistory, Text,
    ToolCallId, UserInput,
};
use serde_json::{Value, json};
use support::loops::*;

struct CaptureModel {
    inner: Arc<dyn Model>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Model for CaptureModel {
    fn descriptor(&self) -> &ModelDescriptor {
        self.inner.descriptor()
    }

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.start(request, context)
    }
}

#[derive(Default)]
struct CaptureHistory(Mutex<Vec<HistoryItem>>);

impl LoopHistory for CaptureHistory {
    fn append<'a>(
        &'a self,
        _loop_id: LoopId,
        offset: usize,
        items: &'a [HistoryItem],
    ) -> HistoryFuture<'a> {
        Box::pin(async move {
            let mut recorded = self.0.lock().unwrap();
            assert_eq!(offset, recorded.len());
            recorded.extend_from_slice(items);
            Ok(())
        })
    }
}

struct ExactTool {
    spec: ToolSpec,
    arguments: Value,
    output: String,
    calls: Mutex<Vec<ToolCallId>>,
}

impl Tool for ExactTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let index = {
                let mut calls = self.calls.lock().unwrap();
                let index = calls.len();
                calls.push(invocation.tool_call_id().clone());
                index
            };
            let empty = json!({});
            assert_eq!(
                invocation.arguments(),
                if index == 0 { &self.arguments } else { &empty }
            );
            if index == 0 {
                for index in 0..4097 {
                    assert!(
                        context.progress.emit(
                            ToolProgress::new(
                                Some(Text::new(format!("progress-{index}"))),
                                Some(index),
                                Some(4097),
                            )
                            .unwrap()
                        )
                    );
                }
            }
            Ok(ToolExecutionOutcome::Completed(
                ToolOutput::new(if index == 0 { &self.output } else { "ok" }).unwrap(),
            ))
        })
    }
}

#[tokio::test]
async fn large_payloads_and_sixty_five_calls_survive_the_real_loop_exactly() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let input = "u".repeat(262_145);
        let text = "t".repeat(262_145);
        let reasoning = "r".repeat(262_145);
        let summary = format!("{}€", "s".repeat(262_142));
        let replay = ProviderReplay::new("fixture/v1", json!({
            "payload": "p".repeat(262_145), "nodes": vec![Value::Null; 16_385],
        })).unwrap();
        let metadata = ReasoningContent::new(
            Some(reasoning.clone()), Some("s".repeat(131_073)),
            Some("e".repeat(262_145)), Some("g".repeat(16_385)),
        ).unwrap();
        let arguments = json!({"payload": "a".repeat(65_537), "nodes": vec![Value::Null; 4097]});
        let schema = json!({"type": "object", "description": "s".repeat(65_537), "nodes": vec![Value::Null; 4097]});
        let tool = Arc::new(ExactTool {
            spec: ToolSpec::new("large".parse().unwrap(), "d".repeat(4097), schema.clone()).unwrap(),
            arguments: arguments.clone(), output: "o".repeat(262_145), calls: Mutex::new(Vec::new()),
        });
        let mut events = vec![ModelEvent::text_delta(&text[..258_049]), ModelEvent::reasoning_delta(&reasoning)];
        events.extend((0..4096).map(|_| ModelEvent::text_delta("t")));
        for index in 0..65 {
            let id: ToolCallId = format!("call_{index}").parse().unwrap();
            events.extend([
                ModelEvent::ToolCallStart { tool_call_id: id.clone(), tool_name: "large".parse().unwrap() },
                ModelEvent::tool_call_arguments_delta(id.clone(), if index == 0 { arguments.to_string() } else { "{}".to_owned() }),
                ModelEvent::ToolCallEnd { tool_call_id: id },
            ]);
        }
        events.extend([ModelEvent::ProviderReplay { replay: replay.clone() }, ModelEvent::Finish { reason: ModelFinishReason::ToolCalls }]);
        let (inner, _) = scripted_model(vec![Round::Events(events), Round::Final { text: "done" }]);
        let model = Arc::new(CaptureModel { inner, requests: Mutex::new(Vec::new()) });
        let mut builder = ToolSet::builder();
        builder.register(tool.clone());
        let mut request = request(config(model.clone(), builder.build().unwrap(), None));
        request.input = UserInput::text(&input).unwrap();
        request.history = Arc::from([
            HistoryItem::Summary(SummaryHistory { content: Text::new(&summary) }),
            HistoryItem::Assistant(AssistantHistory {
                loop_id: LoopId::new().unwrap(), request_index: 0,
                model: model.descriptor().model_ref.clone(), reasoning: ReasoningPreference::Auto,
                content: vec![AssistantPart::Reasoning(metadata.clone())], provider_replay: None,
                finish_reason: ModelFinishReason::Stop, usage: Usage::default(),
            }),
        ]);
        let saved = Arc::new(CaptureHistory::default());
        let mut options = LoopOptions::default_checked().unwrap();
        options.history = Some(saved.clone());
        let mut agent = AgentLoop::start(request, options, no_inputs()).unwrap();
        let mut observations = agent.take_events().unwrap();
        let report = finish(agent).await;
        assert_eq!(report.outcome, LoopOutcome::Completed);
        assert_eq!((report.requests, report.tool_rounds), (2, 1));
        assert_eq!(saved.0.lock().unwrap().as_slice(), report.appended.as_ref());
        assert!(matches!(&report.appended[0], HistoryItem::User(user) if user.input.as_text() == input));
        let HistoryItem::Assistant(assistant) = &report.appended[1] else { panic!("complete assistant missing") };
        assert!(matches!(&assistant.content[0], AssistantPart::Text(actual) if actual.as_ref() == text));
        assert!(matches!(&assistant.content[1], AssistantPart::Reasoning(actual) if actual.text() == Some(reasoning.as_str())));
        assert_eq!(assistant.provider_replay.as_ref(), Some(&replay));
        assert_eq!(assistant.content.len(), 67);
        {
        let calls = tool.calls.lock().unwrap();
        assert_eq!(calls.len(), 65);
        let results = tool_results(&report);
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result.call_id, calls[index]);
            assert_eq!(result.outcome, ToolResultOutcome::Success);
            assert_eq!(result.output.content().as_str(), if index == 0 { &tool.output } else { "ok" });
        }
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools()[0].input_schema(), &schema);
        assert_eq!(requests[0].tools()[0].description().as_str(), "d".repeat(4097));
        assert_eq!(requests[0].messages()[0], ModelMessage::system(format!("Conversation summary:\n{summary}")).unwrap());
        assert_eq!(requests[0].messages()[1], ModelMessage::assistant(vec![AssistantPart::Reasoning(metadata)]).unwrap());
        assert_eq!(requests[0].messages()[2], ModelMessage::user(input).unwrap());
        assert!(requests[1].messages().iter().any(|message| matches!(message, ModelMessage::AssistantWithReplay { parts, provider_replay } if parts == &assistant.content && provider_replay == &replay)));
        assert!(requests[1].messages().iter().any(|message| matches!(message, ModelMessage::Tool { output, .. } if output.content().as_str() == tool.output)));
        }
        let (mut actual_text, mut actual_reasoning) = (String::new(), String::new());
        let mut progress = 0;
        while let Some(envelope) = observations.recv().await {
            match envelope.event {
                LoopEvent::OutputDelta { request_index: 0, channel, delta, .. } => {
                    match channel { OutputChannel::Text => &mut actual_text, OutputChannel::Reasoning => &mut actual_reasoning }.push_str(delta.as_str());
                }
                LoopEvent::ToolProgress { progress: actual, .. } => {
                    assert_eq!(actual.completed, Some(progress));
                    assert_eq!(actual.message.unwrap().as_str(), format!("progress-{progress}"));
                    progress += 1;
                }
                _ => {}
            }
        }
        assert_eq!(actual_text, text);
        assert_eq!(actual_reasoning, reasoning);
        assert_eq!(progress, 4097);
    }).await.unwrap();
}

struct LargeApproval(String);
impl ToolPolicy for LargeApproval {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        Box::pin(async move {
            ToolDecision::require_approval(
                ApprovalRequest::new(&self.0, ApprovalRisk::Low).unwrap(),
            )
        })
    }
}

#[tokio::test]
async fn large_approval_and_choice_payloads_round_trip_through_live_interactions() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let request_input = ToolInputRequest::new(
            "q".repeat(8193),
            (0..33).map(|index| Text::new(format!("{}-{index}", "c".repeat(1025)))).collect(),
            ToolInputAnswerKind::SingleChoice,
        ).unwrap();
        let expected = request_input.choices[32].as_str().to_owned();
        let approval = "a".repeat(8193);
        let (model, _) = scripted_model(vec![Round::Tools { calls: vec![("echo", "call_1")] }, Round::Final { text: "done" }]);
        let agent = AgentLoop::start(
            request(config(model, tool_set(Arc::new(Mutex::new(Vec::new())), ToolBehavior::RequestInvalidInput(request_input.clone())), Some(Arc::new(LargeApproval(approval.clone()))))),
            LoopOptions::default_checked().unwrap(), no_inputs(),
        ).unwrap();
        let handle = agent.handle();
        let mut state = handle.watch_state();
        state.wait_for(|state| state.pending_interaction.is_some()).await.unwrap();
        let pending = state.borrow_and_update().pending_interaction.clone().unwrap();
        assert!(matches!(pending.kind, InteractionKind::Approval(request) if request.prompt.as_str() == approval));
        handle.answer(pending.interaction_id, InteractionAnswer::Approval(ApprovalDecision::AllowOnce)).unwrap();
        state.wait_for(|state| state.pending_interaction.as_ref().is_some_and(|pending| matches!(pending.kind, InteractionKind::ToolInput(_)))).await.unwrap();
        let pending = state.borrow_and_update().pending_interaction.clone().unwrap();
        assert!(matches!(pending.kind, InteractionKind::ToolInput(request) if request == request_input));
        handle.answer(pending.interaction_id, InteractionAnswer::ToolInput(ToolInputAnswer::Choice { index: 32 })).unwrap();
        let report = finish(agent).await;
        assert_eq!(report.outcome, LoopOutcome::Completed);
        let results = tool_results(&report);
        assert_eq!(results[0].outcome, ToolResultOutcome::InputProvided);
        assert_eq!(serde_json::from_str::<Value>(results[0].output.content().as_str()).unwrap(), json!({"choice_index": 32, "choice": expected}));
    }).await.unwrap();
}
