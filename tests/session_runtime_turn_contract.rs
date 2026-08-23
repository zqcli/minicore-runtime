pub mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::stream;
use minicore_runtime::config::SessionSpec;
use minicore_runtime::conversation::{ConversationEntry, TurnTerminal};
use minicore_runtime::error::{
    SessionError, SessionLogErrorKind, SessionShutdownError, TurnWaitError,
};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::{SessionEvent, SessionHealth, SessionStatus};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, TurnOptions, UserInput,
};
use tokio::sync::Semaphore;

use support::fake_session_log::{FakeSessionLog, Operation, Script, ScriptGate};

struct GatedModel {
    descriptor: ModelDescriptor,
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl Model for GatedModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            started.add_permits(1);
            let permit = release.acquire_owned().await.unwrap();
            permit.forget();
            let events = vec![
                Ok(ModelEvent::text_delta("answer").unwrap()),
                Ok(ModelEvent::Usage {
                    usage: Usage::new(3, 2, 1),
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
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

fn fixture() -> Fixture {
    let model_ref: ModelRef = "host:actor".parse().unwrap();
    let spec = SessionSpec::new(
        model_ref.clone(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let model: Arc<dyn Model> = Arc::new(GatedModel {
        descriptor: ModelDescriptor::new(
            model_ref,
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
        )
        .unwrap(),
        started: Arc::clone(&started),
        release: Arc::clone(&release),
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
        started,
        release,
    }
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn options(fixture: &Fixture) -> SessionRuntimeOptions {
    SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        fixture.bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn model_only_turn_is_durable_before_completion_and_transcript_visible() {
    let fixture = fixture();
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(31),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let started = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    started.forget();

    assert_eq!(handle.state().status, SessionStatus::Running);
    assert_eq!(inspection.entries().len(), 1);
    assert!(matches!(
        inspection.entries()[0],
        ConversationEntry::UserMessage(_)
    ));
    assert!(matches!(
        events.recv().await.unwrap().event,
        SessionEvent::TurnStarted { turn_id } if turn_id == turn.turn_id()
    ));

    fixture.release.add_permits(1);
    let outcome = turn.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::Completed);
    assert_eq!(outcome.usage, Usage::new(3, 2, 1));
    let entries = inspection.entries();
    assert!(matches!(
        entries.as_slice(),
        [
            ConversationEntry::UserMessage(_),
            ConversationEntry::AssistantMessage(_),
            ConversationEntry::TurnTerminal(_),
        ]
    ));
    assert_eq!(handle.state().status, SessionStatus::Idle);
    assert_eq!(handle.state().conversation_seq, inspection.head());
    let page = handle.transcript(None, 32).await.unwrap();
    assert_eq!(page.entries.len(), 3);
    assert!(inspection.operations().iter().any(|operation| matches!(
        operation,
        Operation::Append { entries, .. }
            if matches!(entries.as_slice(), [ConversationEntry::TurnTerminal(_)])
    )));
    assert_eq!(
        std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| matches!(
                event.event,
                SessionEvent::TurnFinished { turn_id, .. } if turn_id == outcome.turn_id
            ))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
            .count(),
        1
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn active_shutdown_settles_shutdown_terminal_and_closes_old_handle() {
    let fixture = fixture();
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(32),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let started = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    started.forget();
    runtime.shutdown().await.unwrap();
    let outcome = turn.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::CancelledByShutdown);
    assert_eq!(inspection.close_count(), 1);
    assert!(matches!(
        handle
            .submit(UserInput::text("stale").unwrap(), TurnOptions::default())
            .await,
        Err(SessionError::Closed)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_assistant_commit_degrades_without_fabricated_terminal() {
    let fixture = fixture();
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(Script::UnknownOutcome { committed: false });
    log.script_append(Script::Continue);
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(33),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let started = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    started.forget();
    fixture.release.add_permits(1);
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::DurabilityUnknown(_))
    ));
    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));
    assert_eq!(handle.state().conversation_seq.get(), 1);
    assert!(
        !inspection
            .entries()
            .iter()
            .any(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
    );
    assert_eq!(append_count(&inspection.operations()), 2);
    let page = handle.transcript(None, 32).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    assert!(!page.complete);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn known_assistant_commit_failure_latches_without_settlement_append() {
    let fixture = fixture();
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(Script::Error(SessionLogErrorKind::Unavailable));
    log.script_append(Script::Continue);
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(35),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let started = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    started.forget();
    fixture.release.add_permits(1);
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::DurabilityUnavailable(_))
    ));
    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));
    assert_eq!(append_count(&inspection.operations()), 2);
    assert_eq!(inspection.entries().len(), 1);
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.event, SessionEvent::TurnFinished { .. }));
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn known_settlement_failure_is_unavailable_without_turn_finished() {
    let fixture = fixture();
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(Script::Continue);
    log.script_append(Script::Error(SessionLogErrorKind::Unavailable));
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(34),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let started = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    started.forget();
    fixture.release.add_permits(1);
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::DurabilityUnavailable(_))
    ));
    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));
    assert_eq!(handle.state().conversation_seq.get(), 2);
    assert!(
        !inspection
            .entries()
            .iter()
            .any(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.event, SessionEvent::TurnFinished { .. }));
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_known_settlement_failure_is_primary_and_closes_once() {
    shutdown_settlement_failure(
        session(36),
        Script::GateError(ScriptGate::new(), SessionLogErrorKind::Unavailable),
        false,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_unknown_settlement_failure_is_primary_and_closes_once() {
    shutdown_settlement_failure(
        session(37),
        Script::GateUnknownOutcome(ScriptGate::new(), false),
        true,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_observes_active_assistant_commit_latch_before_close() {
    let fixture = fixture();
    let gate = ScriptGate::new();
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(Script::GateError(
        gate.clone(),
        SessionLogErrorKind::Unavailable,
    ));
    log.script_append(Script::Continue);
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(38),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let mut events = runtime.take_events().unwrap();
    let turn = runtime
        .handle()
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let started = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    started.forget();
    fixture.release.add_permits(1);
    gate.wait_entered().await;
    let mut shutdown = Box::pin(runtime.shutdown());
    assert!(matches!(
        futures_util::poll!(shutdown.as_mut()),
        std::task::Poll::Pending
    ));
    gate.release();
    assert!(matches!(
        shutdown.await,
        Err(SessionShutdownError::Durability(_))
    ));
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::DurabilityUnavailable(_))
    ));
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(append_count(&inspection.operations()), 2);
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.event, SessionEvent::TurnFinished { .. }));
    }
}

async fn shutdown_settlement_failure(session_id: SessionId, script: Script, unknown: bool) {
    let fixture = fixture();
    let gate = match &script {
        Script::GateError(gate, _) | Script::GateUnknownOutcome(gate, _) => gate.clone(),
        _ => unreachable!(),
    };
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(script);
    log.script_append(Script::Continue);
    if !unknown {
        log.script_close(Script::Error(SessionLogErrorKind::Internal));
    }
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let started = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    started.forget();
    let shutdown = tokio::spawn(runtime.shutdown());
    gate.wait_entered().await;
    gate.release();
    assert!(matches!(
        shutdown.await.unwrap(),
        Err(SessionShutdownError::Durability(_))
    ));
    if unknown {
        assert!(matches!(
            turn.wait().await,
            Err(TurnWaitError::DurabilityUnknown(_))
        ));
    } else {
        assert!(matches!(
            turn.wait().await,
            Err(TurnWaitError::DurabilityUnavailable(_))
        ));
    }
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(append_count(&inspection.operations()), 2);
    assert!(
        !inspection
            .entries()
            .iter()
            .any(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.event, SessionEvent::TurnFinished { .. }));
    }
}

fn append_count(operations: &[Operation]) -> usize {
    operations
        .iter()
        .filter(|operation| matches!(operation, Operation::Append { .. }))
        .count()
}
