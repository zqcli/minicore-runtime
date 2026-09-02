//! One live agent loop, end to end.
//!
//! This example drives a single `AgentLoop` through the public API only:
//! fake model and tool adapters stand in for the host's real ones, and the
//! host (this binary) owns every `HistoryItem` that is handed in or taken
//! out. Nothing here persists anything; the host decides what to keep.
//!
//! ```text
//! cargo run --example agent_loop
//! ```
//!
//! The example shows a simple text loop, a tool loop, live event streaming,
//! a deterministic steer+update demo (the second request proves both took
//! effect), and a cancel on a separate held loop.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::stream;
use tokio::sync::Notify;

use minicore_runtime::execution::{ExecutionConfig, UserInput};
use minicore_runtime::history::HistoryItem;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason, ModelRef,
    ModelRequest, ModelStartFuture, ModelStream, ReasoningPreference,
};
use minicore_runtime::prompt::DefaultPromptProvider;
use minicore_runtime::tools::{
    Tool, ToolContext, ToolExecutionOutcome, ToolFuture, ToolInvocation, ToolOutput, ToolSet,
    ToolSpec,
};
use minicore_runtime::{
    AgentLoop, CancelReason, LoopEvent, LoopOptions, LoopOutcome, LoopReport, LoopRequest,
};

/// Answers every request with one delta and a Stop.
struct EchoModel {
    descriptor: ModelDescriptor,
}

impl Model for EchoModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(async move {
            Ok::<ModelStream, _>(Box::pin(stream::iter(vec![
                Ok(ModelEvent::text_delta("Hello from the model").unwrap()),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                }),
            ])))
        })
    }
}

/// Request 0 issues one `clock` tool call; later requests answer with text.
struct ToolModel {
    descriptor: ModelDescriptor,
}

impl Model for ToolModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let events: Vec<Result<ModelEvent, ModelError>> = if context.request_index == 0 {
            let call_id: minicore_runtime::ToolCallId = "call_clock_0001".parse().unwrap();
            vec![
                Ok(ModelEvent::ToolCallStart {
                    tool_call_id: call_id.clone(),
                    tool_name: "clock".parse().unwrap(),
                }),
                Ok(ModelEvent::tool_call_arguments_delta(
                    call_id.clone(),
                    r#"{"question":"what time is it"}"#,
                )
                .unwrap()),
                Ok(ModelEvent::ToolCallEnd {
                    tool_call_id: call_id,
                }),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::ToolCalls,
                }),
            ]
        } else {
            vec![
                Ok(ModelEvent::text_delta("It is 12:00").unwrap()),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                }),
            ]
        };
        Box::pin(async move { Ok::<ModelStream, _>(Box::pin(stream::iter(events))) })
    }
}

/// Request 0 holds open until the Notify fires, then answers "first". Any
/// request past 0 answers "first-again", which only ever appears if a config
/// update did not actually replace this model for the second request.
struct FirstModel {
    descriptor: ModelDescriptor,
    release: Arc<Notify>,
}

impl Model for FirstModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let release = Arc::clone(&self.release);
        if context.request_index == 0 {
            Box::pin(async move {
                Ok::<ModelStream, _>(Box::pin(stream::unfold(Some(0u8), move |state| {
                    let release = Arc::clone(&release);
                    async move {
                        match state {
                            None => None,
                            Some(0) => {
                                release.notified().await;
                                Some((Ok(ModelEvent::text_delta("first").unwrap()), Some(1)))
                            }
                            Some(1) => Some((
                                Ok(ModelEvent::Finish {
                                    reason: ModelFinishReason::Stop,
                                }),
                                None,
                            )),
                            Some(_) => unreachable!(),
                        }
                    }
                })))
            })
        } else {
            Box::pin(async move {
                Ok::<ModelStream, _>(Box::pin(stream::iter(vec![
                    Ok(ModelEvent::text_delta("first-again").unwrap()),
                    Ok(ModelEvent::Finish {
                        reason: ModelFinishReason::Stop,
                    }),
                ])))
            })
        }
    }
}

/// Answers "after-update" and stops on every request. The steer+update demo
/// swaps the config for this model so request 1 proves the update applied.
struct SecondModel {
    descriptor: ModelDescriptor,
}

impl Model for SecondModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(async move {
            Ok::<ModelStream, _>(Box::pin(stream::iter(vec![
                Ok(ModelEvent::text_delta("after-update").unwrap()),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                }),
            ])))
        })
    }
}

/// A fake clock tool: answers any invocation with a fixed string.
struct ClockTool {
    spec: ToolSpec,
}

impl Tool for ClockTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            Ok(ToolExecutionOutcome::Completed(
                ToolOutput::new("12:00").unwrap(),
            ))
        })
    }
}

fn descriptor(model_name: &str, supports_tools: bool) -> ModelDescriptor {
    ModelDescriptor::new(
        format!("fake/{model_name}").parse::<ModelRef>().unwrap(),
        8_192,
        BTreeSet::from([ReasoningPreference::Auto]),
        supports_tools,
    )
    .unwrap()
}

fn clock_toolset() -> ToolSet {
    let mut builder = ToolSet::builder();
    builder.register(ClockTool {
        spec: ToolSpec::new(
            "clock".parse().unwrap(),
            "Reads the current time",
            serde_json::json!({
                "type": "object",
                "properties": { "question": { "type": "string" } }
            }),
        )
        .unwrap(),
    });
    builder.build().unwrap()
}

fn config(
    model: Arc<dyn Model>,
    tools: ToolSet,
) -> Result<ExecutionConfig, minicore_runtime::execution::ExecutionConfigError> {
    ExecutionConfig::new(
        model,
        ReasoningPreference::Auto,
        tools,
        None,
        Arc::new(DefaultPromptProvider::new(None)),
    )
}

fn start(
    model: Arc<dyn Model>,
    tools: ToolSet,
) -> Result<AgentLoop, minicore_runtime::LoopStartError> {
    let request = LoopRequest::new(
        Arc::from(Vec::<HistoryItem>::new()),
        UserInput::text("hello").unwrap(),
        config(model, tools).unwrap(),
    );
    AgentLoop::start(request, LoopOptions::default_checked().unwrap())
}

fn printable(event: &LoopEvent) -> String {
    match event {
        LoopEvent::Started { loop_id, .. } => format!("started {loop_id}"),
        LoopEvent::RequestStarted {
            request_index,
            config_revision,
            ..
        } => format!("request {request_index} started (revision {config_revision:?})"),
        LoopEvent::OutputDelta { channel, delta, .. } => format!("delta[{channel:?}] {delta:?}"),
        LoopEvent::ToolStarted { tool_name, .. } => format!("tool started: {tool_name}"),
        LoopEvent::ToolFinished {
            call_id, outcome, ..
        } => format!("tool finished: {call_id} -> {outcome:?}"),
        LoopEvent::Finished { outcome, .. } => format!("finished: {outcome:?}"),
        other => format!("{other:?}"),
    }
}

async fn stream_events(mut events: minicore_runtime::LoopEventStream) {
    while let Some(envelope) = events.recv().await {
        println!(
            "  {:>3} dropped, {}",
            envelope.dropped_before,
            printable(&envelope.event)
        );
    }
    println!("  (event stream closed)");
}

async fn simple_text_loop() {
    println!("== simple text loop");
    let model = Arc::new(EchoModel {
        descriptor: descriptor("echo", false),
    });
    let mut agent = start(model, ToolSet::default()).unwrap();
    let events = agent.take_events().unwrap();
    let report = agent.join().await.unwrap();
    stream_events(events).await;
    summarize(&report);
}

async fn tool_loop() {
    println!("== tool loop");
    let model = Arc::new(ToolModel {
        descriptor: descriptor("toolbot", true),
    });
    let mut agent = start(model, clock_toolset()).unwrap();
    let events = agent.take_events().unwrap();
    let report = agent.join().await.unwrap();
    stream_events(events).await;
    summarize(&report);
}

/// Waits until a request at `request_index` has actually been issued (so the
/// model is in flight), then returns.
async fn wait_for_request(events: &mut minicore_runtime::LoopEventStream, request_index: u32) {
    while let Some(envelope) = events.recv().await {
        if let LoopEvent::RequestStarted {
            request_index: index,
            ..
        } = envelope.event
        {
            if index == request_index {
                return;
            }
        }
    }
    panic!("loop ended before request {request_index}");
}

/// Proves steer + update land at the next request: request 0 (held model)
/// finishes with a pending steer, so the final-seal keeps the loop alive for
/// request 1, which runs under the *updated* config and therefore answers
/// "after-update". `report.requests == 2` plus the second request's
/// `RequestStarted` revision prove both took effect.
async fn steer_update_boundaries() {
    println!("== steer + update at the next request");
    let release = Arc::new(Notify::new());
    let mut agent = start(
        Arc::new(FirstModel {
            descriptor: descriptor("first", true),
            release: Arc::clone(&release),
        }),
        clock_toolset(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let handle = agent.handle().clone();

    // Wait until request 0 is issued and the model is holding. This makes the
    // steer/update below deterministic: they cannot sneak into request 0.
    wait_for_request(&mut events, 0).await;

    handle
        .steer(UserInput::text("please focus on the time").unwrap())
        .expect("loop is live, steer is accepted");
    let revision = handle
        .update(
            config(
                Arc::new(SecondModel {
                    descriptor: descriptor("second", true),
                }),
                clock_toolset(),
            )
            .unwrap(),
        )
        .expect("loop is live, update is accepted");
    release.notify_one();

    let report = agent.join().await.unwrap();

    // The update swapped the model: request 1 says "after-update", never
    // "first-again". The steer kept the loop alive through the final.
    let texts: Vec<String> = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::Assistant(assistant) => assistant.content.first(),
            _ => None,
        })
        .filter_map(|part| match part {
            minicore_runtime::model::AssistantPart::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        report.requests, 2,
        "the pending steer must extend the final"
    );
    assert!(
        texts.iter().any(|text| text.contains("after-update")),
        "request 1 must run under the updated model, got {texts:?}"
    );
    assert!(!texts.iter().any(|text| text.contains("first-again")));

    // The second RequestStarted must carry exactly the revision update() gave.
    let mut second_request_with_revision = false;
    while let Some(envelope) = events.recv().await {
        if let LoopEvent::RequestStarted {
            request_index: 1,
            config_revision: observed,
            ..
        } = envelope.event
        {
            assert_eq!(
                observed, revision,
                "request 1 must use the updated revision"
            );
            second_request_with_revision = true;
        }
    }
    assert!(
        second_request_with_revision,
        "expected a RequestStarted for request 1"
    );
    println!("  steer + update applied at request 1 (revision {revision:?})");
}

/// Cancelling a held loop ends it as `Cancelled(User)`, deterministically.
async fn cancel_held_loop() {
    println!("== cancel a held loop");
    let release = Arc::new(Notify::new()); // never released
    let mut agent = start(
        Arc::new(FirstModel {
            descriptor: descriptor("held", true),
            release: Arc::clone(&release),
        }),
        clock_toolset(),
    )
    .unwrap();
    let mut events = agent.take_events().unwrap();
    let handle = agent.handle().clone();

    // The model holds request 0 open until told otherwise; cancel it there.
    wait_for_request(&mut events, 0).await;
    assert!(handle.cancel(), "live loop must accept cancel");

    let report = agent.join().await.unwrap();
    assert!(
        matches!(report.outcome, LoopOutcome::Cancelled(CancelReason::User)),
        "expected Cancelled(User), got {:?}",
        report.outcome
    );
    println!("  cancelled: {:?}", report.outcome);
}

fn summarize(report: &LoopReport) {
    let text = report
        .appended
        .iter()
        .filter_map(|item| match item {
            HistoryItem::Assistant(assistant) => assistant.content.first(),
            _ => None,
        })
        .map(|part| format!("{part:?}"))
        .collect::<Vec<_>>()
        .join(" | ");
    println!(
        "  report: outcome={:?} requests={} tool_rounds={} appended={} text=[{text}]",
        report.outcome,
        report.requests,
        report.tool_rounds,
        report.appended.len()
    );
}

#[tokio::main]
async fn main() {
    simple_text_loop().await;
    tool_loop().await;
    steer_update_boundaries().await;
    cancel_held_loop().await;
    println!("done");
}
