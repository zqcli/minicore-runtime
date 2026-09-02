use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::execution::{ConfigRevision, ExecutionConfig};
use crate::ids::{InteractionId, LoopId};
use crate::interaction::InteractionAnswer;
use crate::limits::LoopLimits;

use super::event::EventSinkError;
use super::{AnswerError, CancelReason, LoopReport, LoopState, LoopStatus, UpdateError};

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

    state_tx: watch::Sender<LoopState>,
    /// Seed receiver shared by all waiters. The sender is owned by the runner
    /// task and never stored here.
    completion_rx: watch::Receiver<Option<Arc<LoopReport>>>,

    state: AtomicU8,
    inner: Mutex<ControlState>,
}

struct ControlState {
    interaction: Option<InteractionSlot>,
    /// False once the final seal closes: update (and, in Phase 4, steer and
    /// seal races) linearize on this mutex through this flag.
    accepting_updates: bool,
    /// Revision of the config the runner has actually applied.
    current_revision: ConfigRevision,
    /// Revision the next accepted update will be handed.
    next_revision: ConfigRevision,
    /// Latest accepted config, applied at the next request boundary.
    pending_config: Option<PendingConfig>,
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
    pub(crate) fn new(id: LoopId, event_capacity: usize, limits: LoopLimits) -> ControlParts {
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
                state_tx,
                completion_rx,
                state: AtomicU8::new(STATE_NONE),
                inner: Mutex::new(ControlState {
                    interaction: None,
                    accepting_updates: true,
                    current_revision: ConfigRevision::INITIAL,
                    next_revision: ConfigRevision::INITIAL.next(),
                    pending_config: None,
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
        let mut guard = lock_control(&self.inner);
        if !guard.accepting_updates {
            return Err(UpdateError::NotActive);
        }
        let revision = guard.next_revision;
        guard.next_revision = revision.next();
        guard.pending_config = Some(PendingConfig {
            revision,
            config: Arc::new(config),
        });
        Ok(revision)
    }

    /// Runner-side take of the latest accepted config at a request boundary.
    /// Linearizes with `update` and the final seal on the same mutex. Taking
    /// a config is only a *candidate*: the revision is not recorded until the
    /// runner commits it right before a real Model Request goes out, so a
    /// taken config whose preparation fails never counts as applied.
    pub(crate) fn take_pending_config(&self) -> Option<(ConfigRevision, Arc<ExecutionConfig>)> {
        lock_control(&self.inner)
            .pending_config
            .take()
            .map(|pending| (pending.revision, pending.config))
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
    /// critical section, so `update` either lands entirely before the seal
    /// (it wins) or observes `NotActive` after it.
    pub(crate) fn finish_once(&self) -> FinishSeal {
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
