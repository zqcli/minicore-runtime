use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Semaphore, watch};

use super::*;
use crate::config::{CompactionConfig, SessionManifest, Timestamp, TimestampError};
use crate::conversation::{ConversationEntry, TurnTerminal};
use crate::model::{
    Model, ModelCallContext, ModelDescriptor, ModelRequest, ModelStartFuture, ReasoningPreference,
    Usage,
};
use crate::storage::{
    AppendReceipt, ConversationPage, LogFuture, SessionLog, SessionLogError, SessionLogErrorKind,
};
use crate::tools::ToolSet;
use crate::value::BoundedText;

fn ids() -> (SessionId, SessionInstanceId, crate::ids::TurnId) {
    (
        "ses_00000000000000000000000000000001".parse().unwrap(),
        "ins_00000000000000000000000000000001".parse().unwrap(),
        "trn_00000000000000000000000000000001".parse().unwrap(),
    )
}

fn timestamp() -> Timestamp {
    "2026-08-19T12:34:56.789Z".parse().unwrap()
}

#[test]
fn initial_state_rehydrates_confirmed_head_and_last_terminal() {
    let (session_id, instance_id, turn_id) = ids();
    let state = initial_state(
        session_id,
        instance_id,
        ConversationSeq::new(9),
        Some(TurnTerminalEntry {
            seq: ConversationSeq::new(9),
            turn_id,
            terminal: TurnTerminal::Completed,
            usage: Usage::new(1, 2, 3),
            created_at: timestamp(),
        }),
    );
    assert_eq!(state.status, SessionStatus::Idle);
    assert_eq!(state.health, SessionHealth::Healthy);
    assert_eq!(state.conversation_seq, ConversationSeq::new(9));
    assert_eq!(
        state.last_terminal,
        Some(TurnOutcome {
            turn_id,
            terminal: TurnTerminal::Completed,
            usage: Usage::new(1, 2, 3),
        })
    );
    assert!(state.validate().is_ok());
}

struct TestModel(ModelDescriptor);

impl Model for TestModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.0
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        panic!("idle owner must not call Model::start")
    }
}

fn ownership() -> (KernelConfig, SessionBindings, SessionSpec) {
    let spec = SessionSpec::new(
        "host:model".parse().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    let model: Arc<dyn Model> = Arc::new(TestModel(ModelDescriptor {
        model_ref: spec.model.clone(),
        context_window: 1,
        supported_reasoning: BTreeSet::from([ReasoningPreference::Auto]),
        supports_tools: false,
    }));
    let bindings =
        SessionBindings::new(model, ToolSet::builder().build().unwrap(), None, None, None);
    (KernelConfig::default_checked().unwrap(), bindings, spec)
}

struct CloseProbe {
    state: Mutex<Option<watch::Receiver<SessionState>>>,
    close_admitted: Arc<Semaphore>,
    close_release: Arc<Semaphore>,
    saw_closing: AtomicBool,
    close_count: AtomicUsize,
    close_error: bool,
}

struct ProbeLog(Arc<CloseProbe>);

impl SessionLog for ProbeLog {
    fn initialize<'a>(&'a mut self, _manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        Box::pin(async { Ok(ConversationSeq::ZERO) })
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        Box::pin(async { Err(test_log_error()) })
    }

    fn read_page<'a>(
        &'a mut self,
        _after: Option<ConversationSeq>,
        _limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        Box::pin(async { Err(test_log_error()) })
    }

    fn append<'a>(
        &'a mut self,
        _expected_head: ConversationSeq,
        _entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        Box::pin(async { Err(test_log_error()) })
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        let probe = Arc::clone(&self.0);
        Box::pin(async move {
            probe.close_count.fetch_add(1, Ordering::SeqCst);
            let saw_closing = probe
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .is_some_and(|state| state.borrow().status == SessionStatus::Closing);
            probe.saw_closing.store(saw_closing, Ordering::SeqCst);
            probe.close_admitted.add_permits(1);
            let permit = Arc::clone(&probe.close_release)
                .acquire_owned()
                .await
                .unwrap();
            permit.forget();
            if probe.close_error {
                Err(test_log_error())
            } else {
                Ok(())
            }
        })
    }
}

fn test_log_error() -> SessionLogError {
    SessionLogError::new(
        SessionLogErrorKind::Internal,
        crate::error::DiagnosticSummary::bounded_static(
            crate::error::DiagnosticCode::Internal,
            crate::error::DiagnosticCategory::Storage,
            "unused test log operation",
            false,
        ),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_publishes_closing_before_close_and_drops_state_sender_afterward() {
    let probe = Arc::new(CloseProbe {
        state: Mutex::new(None),
        close_admitted: Arc::new(Semaphore::new(0)),
        close_release: Arc::new(Semaphore::new(0)),
        saw_closing: AtomicBool::new(false),
        close_count: AtomicUsize::new(0),
        close_error: false,
    });
    let (kernel, bindings, spec) = ownership();
    let manifest = SessionManifest::new(ids().0, spec.clone()).unwrap();
    let conversation = ConversationLog::initialize(
        Box::new(ProbeLog(Arc::clone(&probe))),
        manifest,
        kernel.clone(),
        Box::new(|| Ok::<_, TimestampError>(timestamp())),
    )
    .await
    .unwrap();
    let (mut owner, mut channels) =
        IdleSessionOwner::new(conversation, kernel, bindings, spec, ids().0, ids().1)
            .map_err(|_| ())
            .unwrap();
    assert_eq!(channels.state.borrow().status, SessionStatus::Idle);
    assert_eq!(channels.state.borrow().health, SessionHealth::Healthy);
    assert_eq!(
        channels.state.borrow().conversation_seq,
        ConversationSeq::ZERO
    );
    assert!(matches!(
        channels.events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    *probe
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(channels.state.clone());
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move { run_idle_owner(&mut owner, task_cancel).await });
    cancel.cancel();
    let admitted = Arc::clone(&probe.close_admitted)
        .acquire_owned()
        .await
        .unwrap();
    admitted.forget();
    assert_eq!(channels.state.borrow().status, SessionStatus::Closing);
    channels.state.borrow_and_update();
    assert!(probe.saw_closing.load(Ordering::SeqCst));
    probe.close_release.add_permits(1);
    assert!(matches!(task.await.unwrap(), SessionActorExit::Closed));
    assert!(channels.state.changed().await.is_err());
    assert_eq!(probe.close_count.load(Ordering::SeqCst), 1);
    assert!(channels.events.recv().await.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn actor_loop_panic_after_ready_closes_once_and_maps_actor_terminated() {
    let probe = Arc::new(CloseProbe {
        state: Mutex::new(None),
        close_admitted: Arc::new(Semaphore::new(0)),
        close_release: Arc::new(Semaphore::new(0)),
        saw_closing: AtomicBool::new(false),
        close_count: AtomicUsize::new(0),
        close_error: true,
    });
    let (kernel, bindings, spec) = ownership();
    let manifest = SessionManifest::new(ids().0, spec.clone()).unwrap();
    let conversation = ConversationLog::initialize(
        Box::new(ProbeLog(Arc::clone(&probe))),
        manifest,
        kernel.clone(),
        Box::new(|| Ok::<_, TimestampError>(timestamp())),
    )
    .await
    .unwrap();
    let (mut owner, channels) =
        IdleSessionOwner::new(conversation, kernel, bindings, spec, ids().0, ids().1)
            .map_err(|_| ())
            .unwrap();
    owner.panic_on_run = true;
    *probe
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(channels.state.clone());
    let task =
        tokio::spawn(async move { run_idle_owner(&mut owner, CancellationToken::new()).await });
    let admitted = Arc::clone(&probe.close_admitted)
        .acquire_owned()
        .await
        .unwrap();
    admitted.forget();
    assert_eq!(channels.state.borrow().status, SessionStatus::Closing);
    assert!(probe.saw_closing.load(Ordering::SeqCst));
    probe.close_release.add_permits(1);
    let exit = task.await.unwrap();
    assert!(matches!(&exit, SessionActorExit::PanicCloseFailed(_)));
    assert!(matches!(
        super::super::runtime::map_actor_exit(exit),
        Err(crate::error::SessionShutdownError::ActorTerminated(_))
    ));
    assert_eq!(probe.close_count.load(Ordering::SeqCst), 1);
}
