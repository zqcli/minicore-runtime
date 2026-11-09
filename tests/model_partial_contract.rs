mod support;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{
    AssistantPart, Model, ModelCallContext, ModelDescriptor, ModelError, ModelErrorKind,
    ModelEvent, ModelPartial, ModelRequest, ModelStartFuture, ModelStream, ProviderReplay, Usage,
};
use minicore_runtime::tools::{ToolResultOutcome, ToolSet};
use minicore_runtime::{
    AgentLoop, BoundedText, CancelReason, HistoryItem, LoopOptions, LoopOutcome,
};
use tokio::sync::Notify;

use support::loops::*;

fn interrupted() -> ModelError {
    ModelError::started(
        ModelErrorKind::ProviderUnavailable,
        DiagnosticSummary::new(
            DiagnosticCode::ModelUnavailable,
            DiagnosticCategory::Model,
            BoundedText::new("test stream failed").unwrap(),
            false,
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn failure_returns_assembled_partial_and_unfinished_tool_arguments_without_execution() {
    let mut events = vec![
        ModelEvent::reasoning_delta("think\r").unwrap(),
        ModelEvent::text_delta("Hello\r").unwrap(),
    ];
    let mut completed = tool_call_events(vec![("echo", "complete-call")]);
    completed.pop();
    events.extend(completed);
    events.extend([
        ModelEvent::ToolCallStart {
            tool_call_id: "unfinished".parse().unwrap(),
            tool_name: "echo".parse().unwrap(),
        },
        ModelEvent::tool_call_arguments_delta("unfinished".parse().unwrap(), "{\"secret\":")
            .unwrap(),
        ModelEvent::text_delta("\nworld").unwrap(),
        ModelEvent::Usage {
            usage: Usage::new(5, 7, 2),
        },
        ModelEvent::ProviderReplay {
            replay: ProviderReplay::new("test/v1", serde_json::json!({"opaque": "retained"}))
                .unwrap(),
        },
    ]);
    let (model, attempts, _) = scripted_model_counted(vec![Round::StreamError {
        events,
        error: interrupted(),
    }]);
    let (agent, calls) = start_echo(
        model,
        ToolBehavior::Succeed,
        LoopOptions::default_checked().unwrap(),
    );
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(report.outcome, LoopOutcome::Failed(_)));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(calls.lock().unwrap().is_empty());
    assert!(matches!(&report.appended[..], [HistoryItem::User(_)]));
    let partial = report.partial.as_ref().unwrap();
    assert_eq!(partial.request_index, 0);
    assert!(
        matches!(&partial.content[0], AssistantPart::Reasoning(value) if value.text() == Some("think\n"))
    );
    assert!(
        matches!(&partial.content[1], AssistantPart::Text(text) if text.as_ref() == "Hello\nworld")
    );
    assert!(
        matches!(&partial.content[2], AssistantPart::ToolCall(call) if call.tool_call_id().as_str() == "complete-call")
    );
    assert_eq!(partial.incomplete_tool_calls.len(), 1);
    let incomplete = &partial.incomplete_tool_calls[0];
    assert_eq!((incomplete.part_index, incomplete.call_index), (3, 1));
    assert_eq!(incomplete.tool_call_id.as_str(), "unfinished");
    assert_eq!(incomplete.arguments_text, "{\"secret\":");
    assert_eq!(partial.usage, Some(Usage::new(5, 7, 2)));
    assert!(partial.provider_replay.is_some());
    let encoded = serde_json::to_vec(partial).unwrap();
    assert_eq!(
        serde_json::from_slice::<ModelPartial>(&encoded).unwrap(),
        *partial
    );
    assert!(!format!("{partial:?}").contains("secret"));
}

struct HoldingAfterEvents {
    descriptor: ModelDescriptor,
    events: Vec<ModelEvent>,
    drained: Arc<Notify>,
}

impl Model for HoldingAfterEvents {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }
    fn start<'a>(&'a self, _: ModelRequest, _: ModelCallContext) -> ModelStartFuture<'a> {
        let drained = self.drained.clone();
        let events = self.events.clone();
        Box::pin(async move {
            let stream = futures_util::stream::iter(events.into_iter().map(Ok)).chain(
                futures_util::stream::poll_fn(move |_| {
                    drained.notify_one();
                    std::task::Poll::Pending
                }),
            );
            Ok(Box::pin(stream) as ModelStream)
        })
    }
}

fn holding(events: Vec<ModelEvent>) -> (Arc<dyn Model>, Arc<Notify>) {
    let drained = Arc::new(Notify::new());
    let (base, _) = scripted_model(vec![]);
    (
        Arc::new(HoldingAfterEvents {
            descriptor: base.descriptor().clone(),
            events,
            drained: drained.clone(),
        }),
        drained,
    )
}

#[tokio::test]
async fn cancellation_keeps_partial_without_an_event_subscriber() {
    let (model, drained) = holding(
        (0..100)
            .map(|_| ModelEvent::text_delta("delta ").unwrap())
            .collect(),
    );
    let mut options = LoopOptions::default_checked().unwrap();
    options.event_capacity = 1;
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        options,
        no_inputs(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), drained.notified())
        .await
        .unwrap();
    agent.handle().cancel();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    let partial = report.partial.as_ref().unwrap();
    assert!(
        matches!(&partial.content[..], [AssistantPart::Text(text)] if text.as_ref() == "delta ".repeat(100))
    );
    assert!(matches!(&report.appended[..], [HistoryItem::User(_)]));
}

#[tokio::test(start_paused = true)]
async fn deadline_keeps_partial_from_actual_assembly() {
    let (model, drained) = holding(vec![ModelEvent::text_delta("before deadline").unwrap()]);
    let mut options = LoopOptions::default_checked().unwrap();
    options.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        options,
        no_inputs(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), drained.notified())
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    let report = tokio::time::timeout(Duration::from_secs(1), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        report.outcome,
        LoopOutcome::Cancelled(CancelReason::Deadline)
    );
    assert!(
        matches!(&report.partial.as_ref().unwrap().content[..], [AssistantPart::Text(text)] if text.as_ref() == "before deadline")
    );
}

#[tokio::test]
async fn cancelled_empty_model_has_no_synthetic_partial() {
    let (model, drained) = holding(vec![]);
    let agent = AgentLoop::start(
        request(config(model, ToolSet::default(), None)),
        LoopOptions::default_checked().unwrap(),
        no_inputs(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), drained.notified())
        .await
        .unwrap();
    agent.handle().cancel();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    assert!(report.partial.is_none());
}

#[tokio::test]
async fn tool_cancellation_preserves_completed_response_and_never_creates_partial() {
    let (model, _) = scripted_model(vec![Round::Tools {
        calls: vec![("echo", "running")],
    }]);
    let entered = Arc::new(Notify::new());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let tools = tool_set_with_entered(calls.clone(), ToolBehavior::Hold, Some(entered.clone()));
    let notified = entered.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let agent = AgentLoop::start(
        request(config(model, tools, None)),
        LoopOptions::default_checked().unwrap(),
        no_inputs(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), notified)
        .await
        .unwrap();
    agent.handle().cancel();
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, LoopOutcome::Cancelled(CancelReason::User));
    assert!(report.partial.is_none());
    assert_eq!(calls.lock().unwrap().len(), 1);
    let HistoryItem::Assistant(assistant) = &report.appended[1] else {
        panic!("complete response lost")
    };
    assert_eq!(
        assistant.finish_reason,
        minicore_runtime::model::ModelFinishReason::ToolCalls
    );
    assert_eq!(
        tool_results(&report)[0].outcome,
        ToolResultOutcome::Cancelled
    );
}

#[tokio::test]
async fn incomplete_second_request_has_original_index_without_reclassifying_first_response() {
    let (model, _) = scripted_model(vec![
        Round::Tools {
            calls: vec![("echo", "ran-once")],
        },
        Round::StreamError {
            events: vec![ModelEvent::text_delta("unfinished second answer").unwrap()],
            error: interrupted(),
        },
    ]);
    let (agent, calls) = start_echo(
        model,
        ToolBehavior::Succeed,
        LoopOptions::default_checked().unwrap(),
    );
    let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(report.requests, 2);
    assert_eq!(report.appended.len(), 3);
    assert_eq!(report.partial.as_ref().unwrap().request_index, 1);
}

#[tokio::test]
async fn eof_and_invalid_tool_end_retain_unfinished_content() {
    for malformed_tool in [false, true] {
        let mut events = vec![ModelEvent::text_delta("retained").unwrap()];
        if malformed_tool {
            events.extend([
                ModelEvent::ToolCallStart {
                    tool_call_id: "unfinished".parse().unwrap(),
                    tool_name: "echo".parse().unwrap(),
                },
                ModelEvent::tool_call_arguments_delta("unfinished".parse().unwrap(), "{invalid")
                    .unwrap(),
                ModelEvent::ToolCallEnd {
                    tool_call_id: "unfinished".parse().unwrap(),
                },
            ]);
        }
        let (model, _) = scripted_model(vec![Round::Events(events)]);
        let (agent, calls) = start_echo(
            model,
            ToolBehavior::Succeed,
            LoopOptions::default_checked().unwrap(),
        );
        let report = tokio::time::timeout(Duration::from_secs(5), agent.join())
            .await
            .unwrap()
            .unwrap();
        let partial = report.partial.as_ref().unwrap();
        assert!(
            matches!(&partial.content[..], [AssistantPart::Text(text)] if text.as_ref() == "retained")
        );
        assert_eq!(
            partial.incomplete_tool_calls.len(),
            usize::from(malformed_tool)
        );
        assert!(calls.lock().unwrap().is_empty());
    }
}
