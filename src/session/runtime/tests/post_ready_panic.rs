use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use crate::config::{CompactionConfig, KernelConfig, SessionManifest, SessionSpec};
use crate::conversation::{ConversationEntry, ConversationSeq};
use crate::error::{SessionShutdownError, TurnWaitError};
use crate::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelRef, ModelStartFuture, ModelStream,
    ReasoningPreference,
};
use crate::session::{SessionEvent, SessionRuntime, SessionRuntimeOptions, SessionStatus};
use crate::storage::{AppendReceipt, ConversationPage, LogFuture, SessionLog};
use crate::tools::ToolSet;
use crate::value::BoundedText;
use crate::{SessionBindings, SessionId, TurnOptions, UserInput};

struct ModelProbe {
    polled: tokio::sync::Notify,
    dropped_flag: AtomicBool,
}

struct PendingModel {
    descriptor: ModelDescriptor,
    probe: Arc<ModelProbe>,
}

impl Model for PendingModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: crate::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        Box::pin(PendingStart {
            probe: Arc::clone(&self.probe),
            announced: false,
        })
    }
}

struct PendingStart {
    probe: Arc<ModelProbe>,
    announced: bool,
}

impl Future for PendingStart {
    type Output = Result<ModelStream, ModelError>;

    fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.announced {
            self.announced = true;
            self.probe.polled.notify_one();
        }
        Poll::Pending
    }
}

impl Drop for PendingStart {
    fn drop(&mut self) {
        self.probe.dropped_flag.store(true, Ordering::SeqCst);
    }
}

struct LogProbe {
    appends: AtomicUsize,
    closes: AtomicUsize,
    dropped_before_close: AtomicBool,
    model: Arc<ModelProbe>,
}

struct PanicProofLog(Arc<LogProbe>);

impl SessionLog for PanicProofLog {
    fn initialize<'a>(&'a mut self, _: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        Box::pin(async { Ok(ConversationSeq::ZERO) })
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        panic!("post-ready panic test must not load")
    }

    fn read_page<'a>(
        &'a mut self,
        _: Option<ConversationSeq>,
        _: usize,
    ) -> LogFuture<'a, ConversationPage> {
        panic!("post-ready panic test must not read")
    }

    fn append<'a>(
        &'a mut self,
        previous_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        self.0.appends.fetch_add(1, Ordering::SeqCst);
        let appended = entries.len();
        let new_head = entries.last().unwrap().seq();
        Box::pin(async move {
            Ok(AppendReceipt {
                previous_head,
                new_head,
                appended,
            })
        })
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        self.0.dropped_before_close.store(
            self.0.model.dropped_flag.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.0.closes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn post_ready_actor_panic_joins_pending_runner_before_close() {
    let session_id: SessionId = "ses_00000000000000000000000000000091".parse().unwrap();
    let model_ref: ModelRef = "host:post-ready-panic".parse().unwrap();
    let spec = SessionSpec::new(
        model_ref.clone(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    let model_probe = Arc::new(ModelProbe {
        polled: tokio::sync::Notify::new(),
        dropped_flag: AtomicBool::new(false),
    });
    let model: Arc<dyn Model> = Arc::new(PendingModel {
        descriptor: ModelDescriptor::new(
            model_ref,
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
        )
        .unwrap(),
        probe: Arc::clone(&model_probe),
    });
    let bindings =
        SessionBindings::new(model, ToolSet::builder().build().unwrap(), None, None, None);
    let log_probe = Arc::new(LogProbe {
        appends: AtomicUsize::new(0),
        closes: AtomicUsize::new(0),
        dropped_before_close: AtomicBool::new(false),
        model: Arc::clone(&model_probe),
    });
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        bindings,
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    let mut runtime = SessionRuntime::create(
        session_id,
        spec,
        Box::new(PanicProofLog(Arc::clone(&log_probe))),
        options,
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut state = handle.watch_state();
    let mut events = runtime.take_events().unwrap();
    let barrier = crate::session::actor::tests::script_post_ready_panic(session_id);
    let turn = handle
        .submit(UserInput::text("question").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    model_probe.polled.notified().await;
    assert_eq!(handle.state().status, SessionStatus::Running);
    barrier.wait().await;
    assert!(matches!(
        turn.wait().await,
        Err(TurnWaitError::RuntimeTerminated(_))
    ));
    assert!(matches!(
        runtime.shutdown().await,
        Err(SessionShutdownError::ActorTerminated(_))
    ));
    assert_eq!(log_probe.appends.load(Ordering::SeqCst), 1);
    assert_eq!(log_probe.closes.load(Ordering::SeqCst), 1);
    assert!(log_probe.dropped_before_close.load(Ordering::SeqCst));

    let mut started = false;
    while let Some(envelope) = events.recv().await {
        assert_eq!(envelope.session_id, session_id);
        match envelope.event {
            SessionEvent::TurnStarted { .. } => started = true,
            SessionEvent::TurnFinished { .. } => panic!("unexpected TurnFinished"),
            _ => {}
        }
    }
    assert!(started);
    while state.changed().await.is_ok() {}
    let final_state = state.borrow().clone();
    assert_eq!(final_state.status, SessionStatus::Closing);
    assert!(final_state.active_turn.is_none() && final_state.pending_interaction.is_none());
    assert!(final_state.validate().is_ok());
}
