pub mod support;

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::stream;
use minicore_runtime::compaction::{
    CompactionError, CompactionFuture, CompactionProposal, CompactionRequest, CompactionStrategy,
};
use minicore_runtime::conversation::{ConversationEntry, ConversationSeq};
use minicore_runtime::error::{SessionLogErrorKind, TurnWaitError};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelError, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::{SessionEvent, SessionHealth};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, SessionSpec, TurnOptions, UserInput,
};

use support::fake_session_log::{FakeSessionLog, Operation, Script};

struct CountingModel {
    descriptor: ModelDescriptor,
    calls: Arc<AtomicUsize>,
    responses: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
}

impl Model for CountingModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = lock(&self.responses).pop_front().unwrap();
        Box::pin(async move {
            let stream: ModelStream = Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

struct FixedCompaction {
    calls: Arc<AtomicUsize>,
}

impl CompactionStrategy for FixedCompaction {
    fn compact<'a>(&'a self, request: CompactionRequest) -> CompactionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if !request
                .candidate
                .completed_boundaries()
                .contains(&ConversationSeq::new(3))
            {
                return Err(CompactionError::InvalidRequest);
            }
            Ok(CompactionProposal {
                through_seq: ConversationSeq::new(3),
                summary: BoundedText::new("prior turn summary").unwrap(),
            })
        })
    }
}

fn final_response() -> Vec<Result<ModelEvent, ModelError>> {
    vec![
        Ok(ModelEvent::text_delta("answer").unwrap()),
        Ok(ModelEvent::Usage {
            usage: Usage::new(3, 2, 1),
        }),
        Ok(ModelEvent::Finish {
            reason: ModelFinishReason::Stop,
        }),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn known_summary_commit_failure_stops_before_second_model_call() {
    let model_ref: ModelRef = "host:summary-latch".parse().unwrap();
    let model_calls = Arc::new(AtomicUsize::new(0));
    let compaction_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn Model> = Arc::new(CountingModel {
        descriptor: ModelDescriptor::new(
            model_ref.clone(),
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
        )
        .unwrap(),
        calls: Arc::clone(&model_calls),
        responses: Mutex::new(VecDeque::from([final_response(), final_response()])),
    });
    let compaction: Arc<dyn CompactionStrategy> = Arc::new(FixedCompaction {
        calls: Arc::clone(&compaction_calls),
    });
    let spec = SessionSpec::new(
        model_ref,
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Enabled {
            trigger_tokens: 2,
            target_tokens: 1,
        },
    )
    .unwrap();
    let bindings = SessionBindings::new(
        model,
        ToolSet::builder().build().unwrap(),
        None,
        None,
        Some(compaction),
    );
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        bindings,
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    let mut log = FakeSessionLog::new();
    for script in [
        Script::Continue,
        Script::Continue,
        Script::Continue,
        Script::Continue,
        Script::Error(SessionLogErrorKind::Unavailable),
        Script::Continue,
    ] {
        log.script_append(script);
    }
    let inspection = log.inspection();
    let mut runtime = SessionRuntime::create(session(51), spec, Box::new(log), options)
        .await
        .unwrap();
    let handle = runtime.handle();
    let mut events = runtime.take_events().unwrap();
    let first = handle
        .submit(UserInput::text("first").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    first.wait().await.unwrap();
    let second = handle
        .submit(UserInput::text("second").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let second_id = second.turn_id();
    assert!(matches!(
        second.wait().await,
        Err(TurnWaitError::DurabilityUnavailable(_))
    ));
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    assert_eq!(compaction_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));
    assert_eq!(
        inspection
            .operations()
            .iter()
            .filter(|operation| matches!(operation, Operation::Append { .. }))
            .count(),
        5
    );
    let entries = inspection.entries();
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, ConversationEntry::Summary(_)))
    );
    assert!(!entries.iter().any(|entry| matches!(
        entry,
        ConversationEntry::TurnTerminal(terminal) if terminal.turn_id == second_id
    )));
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event.event,
            SessionEvent::TurnFinished { turn_id, .. } if turn_id == second_id
        ));
    }
    runtime.shutdown().await.unwrap();
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
