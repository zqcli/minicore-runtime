//! A multi-turn agent built from one-shot loops.
//!
//! `MemoryAgent` is deliberately *not* part of the library. It shows the
//! v0.4 composition pattern: the host keeps `history`, builds a fresh
//! `ExecutionConfig`, and creates a thin `AgentLoop` per turn. The runtime
//! never sees the session; it only runs one loop and hands back the appended
//! `HistoryItem`s, which the host folds back into its own history.
//!
//! ```text
//! cargo run --example memory_agent
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::stream;

use minicore_runtime::execution::{ExecutionConfig, UserInput};
use minicore_runtime::history::HistoryItem;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelEvent, ModelFinishReason, ModelRef,
    ModelRequest, ModelStartFuture, ModelStream, ReasoningPreference,
};
use minicore_runtime::prompt::DefaultPromptProvider;
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{AgentLoop, LoopOptions, LoopRequest};

/// Answers every request with the echoed user text plus a counter.
struct EchoModel {
    descriptor: ModelDescriptor,
    turn: std::sync::atomic::AtomicU64,
}

impl Model for EchoModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let turn = self.turn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let user_text = request
            .messages()
            .iter()
            .find_map(|message| match message {
                minicore_runtime::model::ModelMessage::User(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Box::pin(async move {
            Ok::<ModelStream, _>(Box::pin(stream::iter(vec![
                Ok(ModelEvent::text_delta(format!("[turn {turn}] you said: {user_text}")).unwrap()),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                }),
            ])))
        })
    }
}

fn descriptor() -> ModelDescriptor {
    ModelDescriptor::new(
        "fake/echo".parse::<ModelRef>().unwrap(),
        8_192,
        BTreeSet::from([ReasoningPreference::Auto]),
        false,
    )
    .unwrap()
}

/// A host-owned, host-persisted conversation composed of one-shot loops.
struct MemoryAgent {
    history: Vec<HistoryItem>,
    config: ExecutionConfig,
}

impl MemoryAgent {
    fn new(config: ExecutionConfig) -> Self {
        Self {
            history: Vec::new(),
            config,
        }
    }

    /// Runs one fresh loop for this input and folds its result back into the
    /// host history, then returns the model's text reply.
    async fn chat(&mut self, input: &str) -> Result<String, Box<dyn std::error::Error>> {
        let request = LoopRequest::new(
            Arc::from(self.history.clone()),
            UserInput::text(input)?,
            self.config.clone(),
        );
        let agent = AgentLoop::start(request, LoopOptions::default_checked()?)?;
        let report = agent.join().await?;

        let reply = report
            .appended
            .iter()
            .find_map(|item| match item {
                HistoryItem::Assistant(assistant) => assistant.content.first(),
                _ => None,
            })
            .map(|part| match part {
                minicore_runtime::model::AssistantPart::Text(text) => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();

        self.history.extend(report.appended.iter().cloned());
        Ok(reply)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ExecutionConfig::new(
        Arc::new(EchoModel {
            descriptor: descriptor(),
            turn: std::sync::atomic::AtomicU64::new(0),
        }),
        ReasoningPreference::Auto,
        ToolSet::default(),
        None,
        Arc::new(DefaultPromptProvider::new(None)),
    )?;

    let mut agent = MemoryAgent::new(config);
    for question in ["hello", "what is the time", "goodbye"] {
        let reply = agent.chat(question).await?;
        println!("user: {question}\nassistant: {reply}\n");
    }
    println!("host history now holds {} items", agent.history.len());
    Ok(())
}
