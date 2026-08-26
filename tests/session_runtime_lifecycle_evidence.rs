pub mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use futures_util::{poll, stream};
use minicore_runtime::config::SessionManifest;
use minicore_runtime::conversation::{ConversationEntry, TurnTerminal};
use minicore_runtime::error::{
    SessionError, SessionLogErrorKind, SessionOpenErrorKind, TurnWaitError,
};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::{SessionEvent, SessionHealth};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, SessionSpec, TurnOptions, UserInput,
};
use tokio::sync::Semaphore;

use support::fake_session_log::{FakeSessionLog, Operation, Script};

struct EvidenceModel {
    descriptor: ModelDescriptor,
    gated: bool,
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
    descriptor_calls: Arc<AtomicUsize>,
    panic_on_descriptor_call: Option<usize>,
}

impl Model for EvidenceModel {
    fn descriptor(&self) -> &ModelDescriptor {
        let call = self.descriptor_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.panic_on_descriptor_call == Some(call) {
            panic!("scripted descriptor panic");
        }
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let gated = self.gated;
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            if gated {
                started.add_permits(1);
                let permit = release.acquire_owned().await.unwrap();
                permit.forget();
            }
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
    started: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
    descriptor_calls: Arc<AtomicUsize>,
}

fn fixture(gated: bool) -> Fixture {
    fixture_with_static_environment(gated, None, false)
}

fn fixture_with_descriptor_panic(gated: bool, panic_on_descriptor_call: Option<usize>) -> Fixture {
    fixture_with_static_environment(gated, panic_on_descriptor_call, false)
}

fn fixture_with_static_environment(
    gated: bool,
    panic_on_descriptor_call: Option<usize>,
    invalid_descriptor: bool,
) -> Fixture {
    let model_ref: ModelRef = "host:lifecycle-evidence".parse().unwrap();
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let descriptor_calls = Arc::new(AtomicUsize::new(0));
    let descriptor = ModelDescriptor {
        model_ref: model_ref.clone(),
        context_window: if invalid_descriptor { 0 } else { 4_096 },
        supported_reasoning: BTreeSet::from([ReasoningPreference::Auto]),
        supports_tools: false,
    };
    let model: Arc<dyn Model> = Arc::new(EvidenceModel {
        descriptor,
        gated,
        started: Arc::clone(&started),
        release,
        calls: Arc::clone(&calls),
        descriptor_calls: Arc::clone(&descriptor_calls),
        panic_on_descriptor_call,
    });
    let spec = SessionSpec::new(
        model_ref,
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
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
        calls,
        descriptor_calls,
    }
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn options(fixture: &Fixture, command_capacity: Option<usize>) -> SessionRuntimeOptions {
    let mut kernel = KernelConfig::default_checked().unwrap();
    if let Some(capacity) = command_capacity {
        kernel.command_capacity = capacity;
    }
    SessionRuntimeOptions::new(
        kernel,
        fixture.bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap()
}

async fn wait_started(fixture: &Fixture) {
    let permit = Arc::clone(&fixture.started).acquire_owned().await.unwrap();
    permit.forget();
}

async fn assert_static_open_failure(fixture: &Fixture, session_id: SessionId) {
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let error = match SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(log),
        options(fixture, None),
    )
    .await
    {
        Err(error) => error,
        Ok(runtime) => {
            runtime.shutdown().await.unwrap();
            panic!("static environment failure unexpectedly opened a runtime")
        }
    };
    assert_eq!(error.kind(), SessionOpenErrorKind::BindingMismatch);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.descriptor_calls.load(Ordering::SeqCst), 1);
    assert!(inspection.entries().is_empty());
    assert_eq!(inspection.operations(), vec![Operation::Close]);
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn created_session_freezes_descriptor_for_all_turns() {
    let fixture = fixture(false);
    let runtime = SessionRuntime::create(
        session(80),
        fixture.spec.clone(),
        Box::new(FakeSessionLog::new()),
        options(&fixture, None),
    )
    .await
    .unwrap();
    let opened = fixture.descriptor_calls.load(Ordering::SeqCst);
    assert_eq!(opened, 1);
    let handle = runtime.handle();
    for turn in 0..100 {
        handle
            .submit(
                UserInput::text(format!("baseline-{turn}")).unwrap(),
                TurnOptions::default(),
            )
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 100);
    assert_eq!(fixture.descriptor_calls.load(Ordering::SeqCst), opened);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_session_turns_do_not_repeat_descriptor_validation() {
    let fixture = fixture(false);
    let session_id = session(79);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, None),
    )
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let loaded = fixture.descriptor_calls.load(Ordering::SeqCst);
    assert_eq!(loaded, 1);
    let log =
        FakeSessionLog::with_initial(inspection.manifest().unwrap(), inspection.entries()).unwrap();
    let runtime = SessionRuntime::load(session_id, Box::new(log), options(&fixture, None))
        .await
        .unwrap();
    let opened = fixture.descriptor_calls.load(Ordering::SeqCst);
    assert_eq!(opened, loaded + 1);
    for turn in 0..100 {
        runtime
            .handle()
            .submit(
                UserInput::text(format!("loaded-{turn}")).unwrap(),
                TurnOptions::default(),
            )
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 100);
    assert_eq!(fixture.descriptor_calls.load(Ordering::SeqCst), opened);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn descriptor_panic_during_open_fails_before_user_append_and_model_start() {
    let fixture = fixture_with_descriptor_panic(false, Some(1));
    assert_static_open_failure(&fixture, session(77)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_static_environment_fails_before_user_append_and_model_start() {
    let fixture = fixture_with_static_environment(false, None, true);
    assert_static_open_failure(&fixture, session(78)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_on_already_initialized_log_fails_closes_and_leaves_no_task_owner() {
    let fixture = fixture(false);
    let session_id = session(81);
    let manifest = SessionManifest::new(session_id, fixture.spec.clone()).unwrap();
    let log = FakeSessionLog::with_initial(manifest, Vec::new()).unwrap();
    let inspection = log.inspection();
    let error = match SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, None),
    )
    .await
    {
        Err(error) => error,
        Ok(runtime) => {
            runtime.shutdown().await.unwrap();
            panic!("already initialized log unexpectedly created a runtime")
        }
    };
    assert_eq!(error.kind(), SessionOpenErrorKind::Log);
    assert_eq!(
        error.log_error().unwrap().kind(),
        SessionLogErrorKind::AlreadyInitialized
    );
    assert_eq!(
        inspection.operations(),
        vec![Operation::Initialize, Operation::Close]
    );
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn user_message_append_failure_never_starts_turn_or_model() {
    let fixture = fixture(false);
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Error(SessionLogErrorKind::Conflict));
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(
        session(86),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, None),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    assert!(matches!(
        handle
            .submit(UserInput::text("rejected").unwrap(), TurnOptions::default())
            .await,
        Err(SessionError::Degraded(_))
    ));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let state = handle.state();
    assert_eq!(state.status, minicore_runtime::SessionStatus::Idle);
    assert!(matches!(state.health, SessionHealth::Degraded { .. }));
    assert!(state.active_turn.is_none());
    assert!(state.pending_interaction.is_none());
    assert_eq!(state.conversation_seq.get(), 0);
    assert!(inspection.entries().is_empty());
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.event, SessionEvent::TurnStarted { .. }));
    }
    runtime.shutdown().await.unwrap();
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn reload_changes_instance_and_stale_handles_turns_and_events_are_isolated() {
    let fixture = fixture(true);
    let session_id = session(82);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let mut first = SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, None),
    )
    .await
    .unwrap();
    let old_instance = first.instance_id();
    let old_handle = first.handle();
    let mut old_events = first.take_events().unwrap();
    let old_turn = old_handle
        .submit(UserInput::text("first").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    wait_started(&fixture).await;
    let stale_event = loop {
        let event = old_events.recv().await.unwrap();
        if matches!(event.event, SessionEvent::TurnStarted { .. }) {
            break event;
        }
    };
    first.shutdown().await.unwrap();
    assert_eq!(
        old_turn.wait().await.unwrap().terminal,
        TurnTerminal::CancelledByShutdown
    );
    let durable_manifest = inspection.manifest().unwrap();
    let durable_entries = inspection.entries();

    let replacement_log = FakeSessionLog::with_initial(durable_manifest, durable_entries).unwrap();
    let mut second = SessionRuntime::load(
        session_id,
        Box::new(replacement_log),
        options(&fixture, None),
    )
    .await
    .unwrap();
    let new_instance = second.instance_id();
    assert_ne!(old_instance, new_instance);
    assert_eq!(stale_event.instance_id, old_instance);
    assert_ne!(stale_event.instance_id, new_instance);
    assert!(matches!(
        old_handle
            .submit(UserInput::text("stale").unwrap(), TurnOptions::default())
            .await,
        Err(SessionError::Closed)
    ));
    assert!(!old_turn.cancel());
    assert_eq!(second.handle().state().instance_id, new_instance);
    assert!(second.take_events().unwrap().try_recv().is_err());
    second.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn second_submit_is_busy_with_exact_turn_and_does_not_append_another_user() {
    let fixture = fixture(true);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(83),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, None),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let first = handle
        .submit(UserInput::text("first").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    wait_started(&fixture).await;
    assert!(matches!(
        handle
            .submit(UserInput::text("second").unwrap(), TurnOptions::default())
            .await,
        Err(SessionError::Busy { active_turn }) if active_turn == first.turn_id()
    ));
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
async fn active_conflict_degrades_rejects_submit_and_creates_no_terminal() {
    let fixture = fixture(false);
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(Script::Error(SessionLogErrorKind::Conflict));
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(84),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, None),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("conflict").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::DurabilityUnavailable(_))
    ));
    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));
    assert!(matches!(
        handle
            .submit(UserInput::text("rejected").unwrap(), TurnOptions::default())
            .await,
        Err(SessionError::Degraded(_))
    ));
    assert!(
        !inspection
            .entries()
            .iter()
            .any(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_active_append_rejects_later_submit_without_new_append_or_model() {
    let fixture = fixture(false);
    let mut log = FakeSessionLog::new();
    log.script_append(Script::Continue);
    log.script_append(Script::UnknownOutcome { committed: false });
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(87),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, None),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let turn = handle
        .submit(UserInput::text("unknown").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::DurabilityUnknown(_))
    ));
    let before = (
        fixture.calls.load(Ordering::SeqCst),
        append_count(&inspection.operations()),
    );
    assert!(matches!(
        handle
            .submit(UserInput::text("rejected").unwrap(), TurnOptions::default())
            .await,
        Err(SessionError::Degraded(_))
    ));
    assert_eq!(
        (
            fixture.calls.load(Ordering::SeqCst),
            append_count(&inspection.operations()),
        ),
        before
    );
    assert!(
        !inspection
            .entries()
            .iter()
            .any(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn full_command_mailbox_cannot_block_root_shutdown() {
    let fixture = fixture(true);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(
        session(85),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, Some(1)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let active = handle
        .submit(UserInput::text("active").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    wait_started(&fixture).await;
    let mut queued =
        Box::pin(handle.submit(UserInput::text("queued").unwrap(), TurnOptions::default()));
    assert!(matches!(poll!(queued.as_mut()), Poll::Pending));
    let mut overflow =
        Box::pin(handle.submit(UserInput::text("full").unwrap(), TurnOptions::default()));
    assert!(matches!(
        poll!(overflow.as_mut()),
        Poll::Ready(Err(SessionError::Backpressure))
    ));
    runtime.shutdown().await.unwrap();
    assert_eq!(
        active.wait().await.unwrap().terminal,
        TurnTerminal::CancelledByShutdown
    );
    assert!(queued.await.is_err());
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
}

fn append_count(operations: &[Operation]) -> usize {
    operations
        .iter()
        .filter(|operation| matches!(operation, Operation::Append { .. }))
        .count()
}
