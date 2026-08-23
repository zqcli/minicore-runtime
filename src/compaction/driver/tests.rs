use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll};

use tokio::sync::{Barrier, Notify};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::compaction::CompactionFuture;
use crate::config::{CompactionConfig, SessionSpec};
use crate::conversation::{
    AssistantMessageEntry, ConversationEntry, ConversationView, SummaryEntry, TurnExecutionRecord,
    TurnTerminal, TurnTerminalEntry, UserInputRecord, UserMessageEntry,
};
use crate::model::{ModelFinishReason, ModelRef, ReasoningPreference, Usage};
use crate::time::Timestamp;

#[cfg(test)]
mod basic;
#[cfg(test)]
mod cancellation;
#[cfg(test)]
mod concurrency;
#[cfg(test)]
mod deadline;
#[cfg(test)]
mod validation;

#[derive(Clone)]
pub(super) struct CandidateControlHook {
    reached: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl CandidateControlHook {
    pub(super) async fn wait_reached(&self) {
        self.reached.wait().await;
    }

    pub(super) async fn release(&self) {
        self.release.wait().await;
    }
}

static CANDIDATE_CONTROL_HOOKS: OnceLock<Mutex<BTreeMap<TurnId, CandidateControlHook>>> =
    OnceLock::new();

pub(super) fn install_candidate_control_hook(turn_id: TurnId) -> CandidateControlHook {
    let hook = CandidateControlHook {
        reached: Arc::new(Barrier::new(2)),
        release: Arc::new(Barrier::new(2)),
    };
    assert!(
        lock(candidate_control_hooks())
            .insert(turn_id, hook.clone())
            .is_none(),
        "candidate control hook already installed for turn"
    );
    hook
}

pub(super) async fn pause_after_candidate(turn_id: TurnId) {
    let hook = lock(candidate_control_hooks()).remove(&turn_id);
    if let Some(hook) = hook {
        hook.reached.wait().await;
        hook.release.wait().await;
    }
}

fn candidate_control_hooks() -> &'static Mutex<BTreeMap<TurnId, CandidateControlHook>> {
    CANDIDATE_CONTROL_HOOKS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn timestamp() -> Timestamp {
    "2026-08-19T12:34:56.789Z".parse().unwrap()
}

fn session_id() -> SessionId {
    "ses_00000000000000000000000000000071".parse().unwrap()
}

fn turn_id(value: u8) -> TurnId {
    format!("trn_{value:032}").parse().unwrap()
}

fn model_ref() -> ModelRef {
    "host:compaction".parse().unwrap()
}

fn spec() -> SessionSpec {
    SessionSpec::new(
        model_ref(),
        ReasoningPreference::Auto,
        BoundedText::new("").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

fn user(seq: u64, turn: u8, text: &str) -> ConversationEntry {
    ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(turn),
        input: UserInputRecord::new(BoundedText::new(text).unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(model_ref(), ReasoningPreference::Auto, 1).unwrap(),
        created_at: timestamp(),
    })
}

fn assistant(seq: u64, turn: u8, text: &str) -> ConversationEntry {
    ConversationEntry::AssistantMessage(AssistantMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(turn),
        model: model_ref(),
        text: Some(BoundedText::new(text).unwrap()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: Usage::default(),
        finish_reason: ModelFinishReason::Stop,
        created_at: timestamp(),
    })
}

fn terminal(seq: u64, turn: u8) -> ConversationEntry {
    ConversationEntry::TurnTerminal(TurnTerminalEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(turn),
        terminal: TurnTerminal::Completed,
        usage: Usage::default(),
        created_at: timestamp(),
    })
}

fn summary(seq: u64, through: u64, text: &str) -> ConversationEntry {
    ConversationEntry::Summary(SummaryEntry {
        seq: ConversationSeq::new(seq),
        through: ConversationSeq::new(through),
        summary: BoundedText::new(text).unwrap(),
        created_at: timestamp(),
    })
}

fn canonical_candidate(entries: Vec<ConversationEntry>) -> CompactionCandidate {
    let head = entries
        .last()
        .map(ConversationEntry::seq)
        .unwrap_or(ConversationSeq::ZERO);
    ConversationView::from_confirmed(head, entries.into())
        .validated_compaction_candidate(&spec(), &SemanticLimits::default())
        .unwrap()
}

fn completed_candidate() -> CompactionCandidate {
    canonical_candidate(vec![
        user(1, 1, "secret candidate input"),
        assistant(2, 1, "secret candidate answer"),
        terminal(3, 1),
    ])
}

fn active_candidate() -> CompactionCandidate {
    canonical_candidate(vec![
        user(1, 1, "old input"),
        assistant(2, 1, "old answer"),
        terminal(3, 1),
        user(4, 2, "active input"),
    ])
}

fn summarized_candidate(include_newer_boundary: bool) -> CompactionCandidate {
    let mut entries = vec![
        user(1, 1, "old input"),
        assistant(2, 1, "old answer"),
        terminal(3, 1),
        summary(4, 3, "old summary"),
    ];
    if include_newer_boundary {
        entries.extend([
            user(5, 2, "new input"),
            assistant(6, 2, "new answer"),
            terminal(7, 2),
        ]);
    }
    canonical_candidate(entries)
}

fn proposal(through: u64, summary: &str) -> CompactionProposal {
    CompactionProposal {
        through_seq: ConversationSeq::new(through),
        summary: BoundedText::new(summary).unwrap(),
    }
}

fn limits(max_summary_bytes: usize) -> SemanticLimits {
    SemanticLimits {
        max_model_text_bytes_per_round: max_summary_bytes,
        ..SemanticLimits::default()
    }
}

fn strategy_port(strategy: &Arc<ScriptStrategy>) -> Arc<dyn CompactionStrategy> {
    Arc::<ScriptStrategy>::clone(strategy)
}

fn driver(
    strategy: Option<Arc<dyn CompactionStrategy>>,
    max_summary_bytes: usize,
) -> CompactionDriver {
    CompactionDriver::new(strategy, Duration::from_secs(5), limits(max_summary_bytes)).unwrap()
}

async fn run(
    driver: &CompactionDriver,
    candidate: CompactionCandidate,
    target_tokens: u64,
    deadline_after: Duration,
    cancellation: CancellationToken,
) -> Result<ValidatedCompactionProposal, CompactionError> {
    driver
        .run(
            session_id(),
            turn_id(9),
            candidate,
            target_tokens,
            Instant::now() + deadline_after,
            cancellation,
        )
        .await
}

struct FutureProbe {
    polled: AtomicBool,
    dropped: AtomicBool,
    cancelled_before_drop: AtomicBool,
    notify: Notify,
}

impl FutureProbe {
    fn shared() -> Arc<Self> {
        Arc::new(Self {
            polled: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            cancelled_before_drop: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    async fn wait_polled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.polled.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

struct PendingCompactionFuture {
    probe: Arc<FutureProbe>,
    cancellation: CancellationToken,
}

impl Future for PendingCompactionFuture {
    type Output = Result<CompactionProposal, CompactionError>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        self.probe.polled.store(true, Ordering::SeqCst);
        self.probe.notify.notify_waiters();
        Poll::Pending
    }
}

impl Drop for PendingCompactionFuture {
    fn drop(&mut self) {
        self.probe
            .cancelled_before_drop
            .store(self.cancellation.is_cancelled(), Ordering::SeqCst);
        self.probe.dropped.store(true, Ordering::SeqCst);
    }
}

enum StrategyBehavior {
    Proposal(CompactionProposal),
    Error(CompactionError),
    ConstructionPanic,
    FuturePanic,
    Pending(Arc<FutureProbe>),
    Barrier(Arc<Barrier>, CompactionProposal),
}

struct ScriptStrategy {
    behaviors: Mutex<VecDeque<StrategyBehavior>>,
    requests: Mutex<Vec<CompactionRequest>>,
    calls: AtomicUsize,
}

impl ScriptStrategy {
    fn new(behaviors: Vec<StrategyBehavior>) -> Arc<Self> {
        Arc::new(Self {
            behaviors: Mutex::new(behaviors.into()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<CompactionRequest> {
        lock(&self.requests).clone()
    }
}

impl CompactionStrategy for ScriptStrategy {
    fn compact<'a>(&'a self, request: CompactionRequest) -> CompactionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(request.clone());
        let behavior = lock(&self.behaviors)
            .pop_front()
            .unwrap_or_else(|| StrategyBehavior::Proposal(proposal(3, "summary")));
        match behavior {
            StrategyBehavior::Proposal(proposal) => Box::pin(async move { Ok(proposal) }),
            StrategyBehavior::Error(error) => Box::pin(async move { Err(error) }),
            StrategyBehavior::ConstructionPanic => panic!("scripted compaction construction panic"),
            StrategyBehavior::FuturePanic => {
                Box::pin(async { panic!("scripted compaction future panic") })
            }
            StrategyBehavior::Pending(probe) => Box::pin(PendingCompactionFuture {
                probe,
                cancellation: request.cancellation,
            }),
            StrategyBehavior::Barrier(barrier, proposal) => Box::pin(async move {
                barrier.wait().await;
                Ok(proposal)
            }),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
