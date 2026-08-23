pub mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use futures_util::{poll, stream};
use minicore_runtime::conversation::{ConversationEntry, TurnTerminal};
use minicore_runtime::error::SessionError;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::SessionEvent;
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, SessionSpec, TurnOptions, UserInput,
};
use tokio::sync::Semaphore;

use support::fake_session_log::{FakeSessionLog, Script, ScriptGate};

struct BlockingModel {
    descriptor: ModelDescriptor,
    calls: Arc<AtomicUsize>,
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl Model for BlockingModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            started.add_permits(1);
            let permit = release.acquire_owned().await.unwrap();
            permit.forget();
            let events = vec![
                Ok(ModelEvent::text_delta("answer").unwrap()),
                Ok(ModelEvent::Usage {
                    usage: Usage::new(1, 1, 0),
                }),
                Ok(ModelEvent::Finish {
                    reason: ModelFinishReason::Stop,
                }),
            ];
            let stream: ModelStream = Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

struct Fixture {
    spec: SessionSpec,
    bindings: SessionBindings,
    calls: Arc<AtomicUsize>,
}

fn fixture() -> Fixture {
    let model_ref: ModelRef = "host:command-races".parse().unwrap();
    let spec = SessionSpec::new(
        model_ref.clone(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(BlockingModel {
        descriptor: ModelDescriptor::new(
            model_ref,
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
        )
        .unwrap(),
        calls: Arc::clone(&calls),
        started: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
    });
    Fixture {
        spec,
        bindings: SessionBindings::new(
            model,
            ToolSet::builder().build().unwrap(),
            None,
            None,
            None,
        ),
        calls,
    }
}

fn options(fixture: &Fixture) -> SessionRuntimeOptions {
    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.command_capacity = 1;
    SessionRuntimeOptions::new(
        kernel,
        fixture.bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_queued_submit_is_skipped_and_full_mailbox_is_backpressure() {
    let fixture = fixture();
    let gate = ScriptGate::new();
    let mut log = FakeSessionLog::new();
    log.script_append(Script::GateContinue(gate.clone()));
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(61),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let first_handle = handle.clone();
    let first = tokio::spawn(async move {
        first_handle
            .submit(UserInput::text("first").unwrap(), TurnOptions::default())
            .await
    });
    gate.wait_entered().await;
    let mut queued =
        Box::pin(handle.submit(UserInput::text("queued").unwrap(), TurnOptions::default()));
    assert!(matches!(poll!(queued.as_mut()), Poll::Pending));
    assert!(matches!(
        handle
            .submit(UserInput::text("overflow").unwrap(), TurnOptions::default())
            .await,
        Err(SessionError::Backpressure)
    ));
    drop(queued);
    gate.release();
    let turn = first.await.unwrap().unwrap();
    assert!(turn.cancel());
    assert_eq!(
        turn.wait().await.unwrap().terminal,
        TurnTerminal::CancelledByUser
    );
    assert_eq!(
        inspection
            .entries()
            .iter()
            .filter(|entry| matches!(entry, ConversationEntry::UserMessage(_)))
            .count(),
        1
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_submit_during_user_append_cancels_and_settles_committed_turn() {
    let fixture = fixture();
    let gate = ScriptGate::new();
    let mut log = FakeSessionLog::new();
    log.script_append(Script::GateContinue(gate.clone()));
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(62),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let mut submit = Box::pin(handle.submit(
        UserInput::text("abandoned").unwrap(),
        TurnOptions::default(),
    ));
    assert!(matches!(poll!(submit.as_mut()), Poll::Pending));
    gate.wait_entered().await;
    drop(submit);
    gate.release();
    let terminal = loop {
        if let SessionEvent::TurnFinished { outcome, .. } = events.recv().await.unwrap().event {
            break outcome.terminal;
        }
    };
    assert_eq!(terminal, TurnTerminal::CancelledByUser);
    let entries = inspection.entries();
    assert!(matches!(
        entries.as_slice(),
        [
            ConversationEntry::UserMessage(_),
            ConversationEntry::TurnTerminal(_),
        ]
    ));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}
