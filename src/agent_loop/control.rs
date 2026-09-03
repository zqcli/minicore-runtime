use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::execution::UserInput;
use crate::execution::{ConfigRevision, ExecutionConfig};
use crate::ids::{InteractionId, LoopId};
use crate::interaction::InteractionAnswer;
use crate::limits::LoopLimits;

use super::event::EventSinkError;
use super::{
    AnswerError, CancelReason, LoopReport, LoopState, LoopStatus, SteerError, UpdateError,
};

/// Single linearizable state machine for cancelling and sealing a loop.
///
/// `mark_cancel` and `finish_once` are both atomic transitions out of `NONE`;
/// whichever wins first is the authoritative outcome and every later call is
/// a no-op. This gives one linearization point for the final-seal race (no
/// load-then-CAS window).
const STATE_NONE: u8 = 0;
const STATE_CANCEL_USER: u8 = 1;
const STATE_CANCEL_OWNER_DROPPED: u8 = 2;
const STATE_CANCEL_SHUTDOWN: u8 = 3;
const STATE_CANCEL_DEADLINE: u8 = 4;
const STATE_FINISHED: u8 = 5;

/// Outcome of the exactly-once completion seal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FinishSeal {
    /// The runner won the seal; no cancellation preceded it.
    Clean,
    /// A cancellation won the linearization point before the seal.
    CancelledPrior(CancelReason),
    /// Someone already sealed the loop; the publish must not run again.
    AlreadyFinished,
}

/// Everything a request boundary consumes in one atomic take: the latest
/// config candidate (if any) plus every accepted steer in order.
#[derive(Default)]
pub(crate) struct BoundaryChanges {
    pub(crate) config: Option<(ConfigRevision, Arc<ExecutionConfig>)>,
    pub(crate) steers: Vec<UserInput>,
}

/// Outcome of the final-seal gate: whether the runner keeps the loop alive
/// for pending steers or closes accepting and seals.
pub(crate) enum FinalGate {
    /// A steer was accepted before the seal; its changes feed the next
    /// request and accepting stays open.
    Continue(BoundaryChanges),
    /// No steer is pending; accepting closes and the loop seals.
    Seal,
}

/// Shared control plane for one agent loop.
///
/// The runner task owns `LoopControl` alongside its single task plus the final
/// completion sender; handles hold an `Arc` and touch only short critical
/// sections guarded by the mutex. The completion sender lives with the runner
/// task, so its drop (normal exit or external abort) is what makes
/// `LoopHandle::wait` observe the channel as closed.
pub(crate) struct LoopControl {
    pub(crate) id: LoopId,
    pub(crate) cancel: CancellationToken,
    limits: LoopLimits,
    max_pending_steers: usize,

    state_tx: watch::Sender<LoopState>,
    /// Seed receiver shared by all waiters. The sender is owned by the runner
    /// task and never stored here.
    completion_rx: watch::Receiver<Option<Arc<LoopReport>>>,

    state: AtomicU8,
    inner: Mutex<ControlState>,
}

struct ControlState {
    interaction: Option<InteractionSlot>,
    /// False once the final seal closes: update, steer, and the seal races all
    /// linearize on this mutex through this flag.
    accepting_updates: bool,
    /// Revision of the config the runner has actually applied.
    current_revision: ConfigRevision,
    /// Revision the next accepted update will be handed.
    next_revision: ConfigRevision,
    /// Latest accepted config, applied at the next request boundary.
    pending_config: Option<PendingConfig>,
    /// Accepted steers awaiting the next request boundary, in accept order.
    pending_steers: VecDeque<UserInput>,
}

/// Latest accepted config waiting for the next request boundary.
struct PendingConfig {
    revision: ConfigRevision,
    config: Arc<ExecutionConfig>,
}

pub(crate) struct InteractionSlot {
    id: InteractionId,
    expected: crate::interaction::InteractionKind,
    reply: oneshot::Sender<InteractionAnswer>,
}

impl InteractionSlot {
    pub(crate) fn new(
        id: InteractionId,
        expected: crate::interaction::InteractionKind,
        reply: oneshot::Sender<InteractionAnswer>,
    ) -> Self {
        Self {
            id,
            expected,
            reply,
        }
    }
}

fn lock_control(inner: &Mutex<ControlState>) -> MutexGuard<'_, ControlState> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Bundle created by `LoopControl::new` for `AgentLoop::start`.
pub(crate) type ControlParts = Result<
    (
        LoopControl,
        super::event::LoopEventSink,
        super::event::LoopEventStream,
        watch::Sender<Option<Arc<LoopReport>>>,
    ),
    EventSinkError,
>;

impl LoopControl {
    pub(crate) fn new(
        id: LoopId,
        event_capacity: usize,
        limits: LoopLimits,
        max_pending_steers: usize,
    ) -> ControlParts {
        let initial = LoopState::new(
            id,
            LoopStatus::Starting,
            0,
            crate::execution::ConfigRevision::INITIAL,
        );
        let (state_tx, _) = watch::channel(initial);
        let (completion_tx, completion_rx) = watch::channel(None);
        let (sink, stream) = super::event::LoopEventSink::channel(event_capacity)?;
        Ok((
            Self {
                id,
                cancel: CancellationToken::new(),
                limits,
                max_pending_steers,
                state_tx,
                completion_rx,
                state: AtomicU8::new(STATE_NONE),
                inner: Mutex::new(ControlState {
                    interaction: None,
                    accepting_updates: true,
                    current_revision: ConfigRevision::INITIAL,
                    next_revision: ConfigRevision::INITIAL.next(),
                    pending_config: None,
                    pending_steers: VecDeque::new(),
                }),
            },
            sink,
            stream,
            completion_tx,
        ))
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Requests cancellation. Returns whether this call performed the actual
    /// transition; it is `false` once the loop is already cancelled or sealed.
    pub(crate) fn mark_cancel(&self, reason: CancelReason) -> bool {
        let target = match reason {
            CancelReason::User => STATE_CANCEL_USER,
            CancelReason::OwnerDropped => STATE_CANCEL_OWNER_DROPPED,
            CancelReason::Shutdown => STATE_CANCEL_SHUTDOWN,
            CancelReason::Deadline => STATE_CANCEL_DEADLINE,
        };
        if self
            .state
            .compare_exchange(STATE_NONE, target, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Accepts a full config replacement; it takes effect at the next request
    /// boundary. `config` is validated against the loop limits *before* the
    /// lock so an invalid config consumes no revision and never disturbs the
    /// pending slot. Repeated updates before a boundary each hand out a
    /// monotonic revision while the runner only ever applies the latest.
    pub(crate) fn update(&self, config: ExecutionConfig) -> Result<ConfigRevision, UpdateError> {
        config
            .validate_against_limits(&self.limits)
            .map_err(|_| UpdateError::InvalidConfig)?;
        let mut candidate = Some(Arc::new(config));
        let mut replaced = None;
        let revision = {
            let mut guard = lock_control(&self.inner);
            if !guard.accepting_updates {
                None
            } else {
                let revision = guard.next_revision;
                guard.next_revision = revision.next();
                let config = candidate.take().expect("candidate present");
                replaced = guard
                    .pending_config
                    .replace(PendingConfig { revision, config });
                Some(revision)
            }
        };
        drop(replaced);
        drop(candidate);
        revision.ok_or(UpdateError::NotActive)
    }

    /// Accepts a steer for the next request boundary. The input length is
    /// checked against the loop's `max_user_input_bytes` *before* the lock;
    /// an oversized steer fails without touching the queue. Acceptance
    /// linearizes with the final seal, the interaction slot, and every other
    /// steer on the same mutex.
    pub(crate) fn steer(&self, input: UserInput) -> Result<(), SteerError> {
        let input_bytes = input.as_text().len();
        if input_bytes == 0 || input_bytes > self.limits.max_user_input_bytes {
            return Err(SteerError::InvalidInput);
        }
        let mut guard = lock_control(&self.inner);
        if !guard.accepting_updates {
            return Err(SteerError::NotActive);
        }
        if guard.interaction.is_some() {
            return Err(SteerError::WaitingForInput);
        }
        if guard.pending_steers.len() >= self.max_pending_steers {
            return Err(SteerError::QueueFull);
        }
        guard.pending_steers.push_back(input);
        Ok(())
    }

    /// Runner-side take of everything queued for the next request boundary:
    /// the latest config candidate plus every accepted steer in accept order,
    /// atomically. Linearizes with `update`/`steer` and the final seal on the
    /// same mutex. Taking a config is only a *candidate*: the revision is not
    /// recorded until the runner commits it right before a real Model Request
    /// goes out, so a taken config whose preparation fails never counts as
    /// applied.
    pub(crate) fn take_boundary(&self) -> BoundaryChanges {
        let mut guard = lock_control(&self.inner);
        BoundaryChanges {
            config: guard
                .pending_config
                .take()
                .map(|pending| (pending.revision, pending.config)),
            steers: guard.pending_steers.drain(..).collect(),
        }
    }

    /// Final-seal gate, linearized with steer/update on the same mutex as the
    /// seal decision. A steer accepted before the seal keeps accepting open
    /// and returns its boundary changes so the loop continues; otherwise
    /// accepting closes, any pending config is discarded outside the lock,
    /// and the caller seals normally. A pending config alone never extends a
    /// final.
    pub(crate) fn begin_final(&self) -> FinalGate {
        let (gate, discarded) = {
            let mut guard = lock_control(&self.inner);
            if guard.pending_steers.is_empty() {
                guard.accepting_updates = false;
                let discarded = guard.pending_config.take();
                (FinalGate::Seal, discarded)
            } else {
                let changes = BoundaryChanges {
                    config: guard
                        .pending_config
                        .take()
                        .map(|pending| (pending.revision, pending.config)),
                    steers: guard.pending_steers.drain(..).collect(),
                };
                (FinalGate::Continue(changes), None)
            }
        };
        drop(discarded);
        gate
    }

    /// Records `revision` as the last actually issued/applied config revision.
    /// Called by the runner at the exact boundary where a Model Request is
    /// dispatched under that revision.
    pub(crate) fn commit_revision(&self, revision: ConfigRevision) {
        lock_control(&self.inner).current_revision = revision;
    }

    /// Revision of the config that actually served the last issued Model
    /// Request. This is exactly what a finished report records as
    /// `final_config_revision`: a pending or taken-but-never-issued update is
    /// not counted.
    pub(crate) fn applied_revision(&self) -> ConfigRevision {
        lock_control(&self.inner).current_revision
    }

    pub(crate) fn cancel_reason(&self) -> CancelReason {
        match self.state.load(Ordering::SeqCst) {
            STATE_CANCEL_OWNER_DROPPED => CancelReason::OwnerDropped,
            STATE_CANCEL_SHUTDOWN => CancelReason::Shutdown,
            STATE_CANCEL_DEADLINE => CancelReason::Deadline,
            STATE_CANCEL_USER => CancelReason::User,
            _ => CancelReason::User,
        }
    }

    /// Exactly-once completion seal. One winner flips the shared state to
    /// `FINISHED`; every later call returns `AlreadyFinished`. The state
    /// transition and `accepting_updates = false` happen in the same mutex
    /// critical section, taking any pending config which is then dropped
    /// outside the lock, so `update` either lands entirely before the seal
    /// (it wins) or observes `NotActive` after it.
    pub(crate) fn finish_once(&self) -> FinishSeal {
        let (finish, discarded) = {
            let mut guard = lock_control(&self.inner);
            let finish = match self.state.compare_exchange(
                STATE_NONE,
                STATE_FINISHED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => FinishSeal::Clean,
                Err(STATE_NONE) => unreachable!("compare_exchange never reports its own value"),
                Err(cancel @ (STATE_CANCEL_USER..=STATE_CANCEL_DEADLINE)) => {
                    let _ = self.state.compare_exchange(
                        cancel,
                        STATE_FINISHED,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    FinishSeal::CancelledPrior(cancel_reason_value(cancel))
                }
                Err(STATE_FINISHED) => FinishSeal::AlreadyFinished,
                Err(_) => unreachable!("unknown loop state value"),
            };
            guard.accepting_updates = false;
            let discarded = guard.pending_config.take();
            (finish, discarded)
        };
        drop(discarded);
        finish
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STATE_FINISHED
    }

    pub(crate) fn publish_state(&self, state: LoopState) {
        // send_replace retains the value even with no live subscriber, so
        // `current_state` always reflects the latest state.
        let _ = self.state_tx.send_replace(state);
    }

    pub(crate) fn subscribe_state(&self) -> watch::Receiver<LoopState> {
        self.state_tx.subscribe()
    }

    pub(crate) fn current_state(&self) -> LoopState {
        self.state_tx.borrow().clone()
    }

    /// Waiter subscription for the completion report. All receivers are
    /// cloned from the seed receiver; the runner-owned sender's drop closes
    /// the channel for every one of them.
    pub(crate) fn subscribe_completion(&self) -> watch::Receiver<Option<Arc<LoopReport>>> {
        self.completion_rx.clone()
    }

    /// The report already delivered to the completion channel, if any.
    /// Losing ending paths consult this to return the exact same `Arc` that
    /// waiters already hold instead of creating a second, distinct report.
    pub(crate) fn published_report(&self) -> Option<Arc<LoopReport>> {
        self.completion_rx.borrow().clone()
    }

    pub(crate) fn set_interaction(
        &self,
        slot: InteractionSlot,
    ) -> Result<(), InteractionStateError> {
        let mut guard = lock_control(&self.inner);
        if guard.interaction.is_some() {
            return Err(InteractionStateError::Busy);
        }
        guard.interaction = Some(slot);
        Ok(())
    }

    pub(crate) fn take_interaction(&self) -> Option<InteractionSlot> {
        lock_control(&self.inner).interaction.take()
    }

    pub(crate) fn answer(
        &self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), AnswerError> {
        if self.is_finished() {
            return Err(AnswerError::NotActive);
        }
        let mut guard = lock_control(&self.inner);
        let Some(slot) = guard.interaction.as_ref() else {
            return Err(AnswerError::InteractionNotFound);
        };
        if slot.id != interaction_id {
            return Err(AnswerError::WrongInteraction);
        }
        answer
            .validate(&slot.expected)
            .map_err(|_| AnswerError::WrongInteraction)?;
        // The slot was validated above; this take cannot fail.
        if let Some(slot) = guard.interaction.take() {
            let _ = slot.reply.send(answer);
        }
        Ok(())
    }
}

fn cancel_reason_value(value: u8) -> CancelReason {
    match value {
        STATE_CANCEL_OWNER_DROPPED => CancelReason::OwnerDropped,
        STATE_CANCEL_SHUTDOWN => CancelReason::Shutdown,
        STATE_CANCEL_DEADLINE => CancelReason::Deadline,
        _ => CancelReason::User,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InteractionStateError {
    Busy,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Weak};

    use crate::agent_loop::control::{FinalGate, FinishSeal, LoopControl};
    use crate::agent_loop::{CancelReason, SteerError, UpdateError};
    use crate::execution::{ConfigRevision, ExecutionConfig, UserInput};
    use crate::ids::LoopId;
    use crate::limits::LoopLimits;
    use crate::model::{
        Model, ModelCallContext, ModelDescriptor, ModelRef, ModelRequest, ModelStartFuture,
        ModelStream, ReasoningPreference,
    };
    use crate::prompt::{DefaultPromptProvider, PromptFuture, PromptProvider, PromptRequest};
    use crate::tools::ToolSet;

    struct NoopModel {
        descriptor: ModelDescriptor,
    }

    impl Model for NoopModel {
        fn descriptor(&self) -> &ModelDescriptor {
            &self.descriptor
        }

        fn start<'a>(
            &'a self,
            _request: ModelRequest,
            _context: ModelCallContext,
        ) -> ModelStartFuture<'a> {
            Box::pin(async move { Ok::<ModelStream, _>(Box::pin(futures_util::stream::empty())) })
        }
    }

    fn config() -> ExecutionConfig {
        let descriptor = ModelDescriptor::new(
            "fake/noop".parse::<ModelRef>().unwrap(),
            8192,
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
        )
        .unwrap();
        ExecutionConfig::new(
            Arc::new(NoopModel { descriptor }),
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(crate::prompt::DefaultPromptProvider::new(None)),
        )
        .unwrap()
    }

    fn control() -> LoopControl {
        LoopControl::new(LoopId::new().unwrap(), 16, LoopLimits::default(), 4)
            .unwrap()
            .0
    }

    fn steering() -> UserInput {
        UserInput::text("focus").unwrap()
    }

    /// The finish CAS seals exactly once and, in the same mutex critical
    /// section, closes accepting: later update/steer are NotActive and a
    /// later cancel cannot reopen the loop.
    #[test]
    fn finish_once_seals_exactly_once_and_closes_accepting() {
        let control = control();
        assert_eq!(control.finish_once(), FinishSeal::Clean);
        assert_eq!(control.finish_once(), FinishSeal::AlreadyFinished);
        assert!(control.is_finished());
        assert!(!control.mark_cancel(CancelReason::User));
        assert_eq!(control.update(config()), Err(UpdateError::NotActive));
        assert_eq!(control.steer(steering()), Err(SteerError::NotActive));
    }

    /// The cancel CAS vs finish CAS window linearizes: whichever wins first
    /// decides the report outcome and every later transition is a no-op.
    #[test]
    fn mark_cancel_then_finish_reports_cancelled_prior_and_locks() {
        let control = control();
        assert!(control.mark_cancel(CancelReason::User));
        assert_eq!(
            control.finish_once(),
            FinishSeal::CancelledPrior(CancelReason::User)
        );
        assert_eq!(control.finish_once(), FinishSeal::AlreadyFinished);
        assert!(!control.mark_cancel(CancelReason::Deadline));
        assert_eq!(control.cancel_reason(), CancelReason::User);
        assert_eq!(control.update(config()), Err(UpdateError::NotActive));
        assert_eq!(control.steer(steering()), Err(SteerError::NotActive));
    }

    /// begin_final with a pending steer keeps accepting open and consumes the
    /// steer; a second begin_final with nothing pending seals.
    #[test]
    fn begin_final_with_pending_steer_keeps_accepting_open() {
        let control = control();
        control.steer(steering()).unwrap();
        match control.begin_final() {
            FinalGate::Continue(changes) => {
                assert_eq!(changes.steers.len(), 1);
                assert!(changes.config.is_none());
            }
            FinalGate::Seal => panic!("pending steer must continue the loop"),
        }
        // Accepting stays open during the continuation round; with no new
        // steer, the next final seals: begin_final closes accepting (the
        // runner-side finish_once then flips the cancelled/finished state).
        control.update(config()).unwrap();
        assert!(matches!(control.begin_final(), FinalGate::Seal));
        assert_eq!(control.update(config()), Err(UpdateError::NotActive));
    }

    /// begin_final with nothing pending closes accepting in the same lock:
    /// update/steer that race it observe NotActive.
    #[test]
    fn begin_final_without_steer_seals_and_closes_accepting() {
        let control = control();
        assert!(matches!(control.begin_final(), FinalGate::Seal));
        assert_eq!(control.update(config()), Err(UpdateError::NotActive));
        assert_eq!(control.steer(steering()), Err(SteerError::NotActive));
    }

    /// Updates hand out monotonic revisions; take_boundary returns only the
    /// latest, and applied_revision moves only on an explicit commit.
    #[test]
    fn updates_are_monotonic_and_latest_wins_until_commit() {
        let control = control();
        let first = control.update(config()).unwrap();
        let second = control.update(config()).unwrap();
        assert!(first < second);
        let changes = control.take_boundary();
        let (revision, _config) = changes.config.expect("a config candidate is pending");
        assert_eq!(revision, second, "the latest update wins");
        assert_eq!(control.applied_revision(), ConfigRevision::INITIAL);
        control.commit_revision(revision);
        assert_eq!(control.applied_revision(), revision);
    }

    #[derive(Clone)]
    struct ProbeTracker {
        dropped: Arc<AtomicBool>,
        mutex_available_on_drop: Arc<AtomicBool>,
    }

    impl ProbeTracker {
        fn new() -> Self {
            Self {
                dropped: Arc::new(AtomicBool::new(false)),
                mutex_available_on_drop: Arc::new(AtomicBool::new(false)),
            }
        }

        fn is_dropped(&self) -> bool {
            self.dropped.load(Ordering::SeqCst)
        }

        fn was_unlocked(&self) -> bool {
            self.mutex_available_on_drop.load(Ordering::SeqCst)
        }
    }

    struct ProbePromptProvider {
        control: Weak<LoopControl>,
        tracker: ProbeTracker,
        delegate: DefaultPromptProvider,
    }

    impl PromptProvider for ProbePromptProvider {
        fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a> {
            self.delegate.prepare(request)
        }
    }

    impl Drop for ProbePromptProvider {
        fn drop(&mut self) {
            if let Some(control) = self.control.upgrade() {
                let unlocked = control.inner.try_lock().is_ok();
                self.tracker
                    .mutex_available_on_drop
                    .store(unlocked, Ordering::SeqCst);
            }
            self.tracker.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn probe_config(control: &Arc<LoopControl>) -> (ExecutionConfig, ProbeTracker) {
        let tracker = ProbeTracker::new();
        let descriptor = ModelDescriptor::new(
            "fake/noop".parse::<ModelRef>().unwrap(),
            8192,
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
        )
        .unwrap();
        let config = ExecutionConfig::new(
            Arc::new(NoopModel { descriptor }),
            ReasoningPreference::Auto,
            ToolSet::default(),
            None,
            Arc::new(ProbePromptProvider {
                control: Arc::downgrade(control),
                tracker: tracker.clone(),
                delegate: DefaultPromptProvider::new(None),
            }),
        )
        .unwrap();
        (config, tracker)
    }

    /// FIX-08: A second update replacing an existing pending config must drop
    /// the replaced config outside the control mutex.
    #[test]
    fn pending_config_drop_on_update_replace_occurs_outside_control_mutex() {
        let control = Arc::new(control());
        let (first_config, tracker) = probe_config(&control);
        control.update(first_config).unwrap();
        assert!(!tracker.is_dropped());

        control.update(config()).unwrap();
        assert!(
            tracker.is_dropped(),
            "replaced pending config must be dropped on second update"
        );
        assert!(
            tracker.was_unlocked(),
            "replaced pending config must be dropped outside control mutex"
        );
    }

    /// FIX-08: begin_final sealing when no steer is pending must discard
    /// the pending config outside the control mutex.
    #[test]
    fn pending_config_drop_on_begin_final_seal_occurs_outside_control_mutex() {
        let control = Arc::new(control());
        let (pending_config, tracker) = probe_config(&control);
        control.update(pending_config).unwrap();
        assert!(!tracker.is_dropped());

        let gate = control.begin_final();
        assert!(matches!(gate, FinalGate::Seal));
        assert!(
            tracker.is_dropped(),
            "pending config must be discarded when begin_final seals"
        );
        assert!(
            tracker.was_unlocked(),
            "discarded pending config must be dropped outside control mutex"
        );
    }

    /// FIX-08: finish_once must discard any pending config outside the
    /// control mutex.
    #[test]
    fn pending_config_drop_on_finish_once_occurs_outside_control_mutex() {
        let control = Arc::new(control());
        let (pending_config, tracker) = probe_config(&control);
        control.update(pending_config).unwrap();
        assert!(!tracker.is_dropped());

        let seal = control.finish_once();
        assert_eq!(seal, FinishSeal::Clean);
        assert!(
            tracker.is_dropped(),
            "pending config must be discarded on finish_once"
        );
        assert!(
            tracker.was_unlocked(),
            "discarded pending config must be dropped outside control mutex"
        );
    }
}
