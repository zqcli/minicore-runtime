#![allow(
    dead_code,
    reason = "the queue seam is consumed incrementally by SessionExecutor M9 slices"
)]

//! Process-local Session input lanes.
//!
//! The queues only own admitted intent and command identity.  SessionExecutor remains the sole
//! owner of live state and decides when queued FollowUp or Steer input may advance execution.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::prompt::PromptIntent;
use crate::tools::{ToolExecutionRequest, ToolStartError, ToolStartGate, ToolStartPermit};
use crate::wire::{CommandId, TurnId};

pub(crate) const FOLLOW_UP_QUEUE_CAPACITY: usize = 8;
pub(crate) const STEER_QUEUE_CAPACITY: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmergencyControlSignal {
    Cancel,
    SecurityRevoked,
    PrepareForUnload,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum EmergencyControlTarget {
    Submit(CommandId),
    Turn(TurnId),
}

impl std::fmt::Debug for EmergencyControlTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EmergencyControlTarget { .. }")
    }
}

/// The private opaque owner identity of one emergency control plane.
///
/// Deliberately zero-sized and unreachable from outside this module: ownership is only ever
/// compared by `Arc::ptr_eq` on the handle/observation owner, so a foreign handle can never
/// present an observation the owner recognizes (no address integer, no global counter, no
/// reconstructible identity).
struct EmergencyControlOwner;

/// One exact owner-bound observation of an emergency basis.
///
/// The observation is `Clone` (never `Copy`): every clone carries the exact same owner Arc,
/// target, epoch, and signal.  Equality is the exact owner identity (`Arc::ptr_eq`) plus the
/// exact target/epoch/signal fields, never a mere field comparison: a foreign handle bound
/// to the same target at the same epoch is a different owner and compares unequal.  The
/// Debug stays fully redacted.
#[derive(Clone)]
pub(crate) struct EmergencyControlObservation {
    owner: Arc<EmergencyControlOwner>,
    target: EmergencyControlTarget,
    epoch: u64,
    signal: Option<EmergencyControlSignal>,
}

impl Eq for EmergencyControlObservation {}

impl PartialEq for EmergencyControlObservation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.target == other.target
            && self.epoch == other.epoch
            && self.signal == other.signal
    }
}

impl std::fmt::Debug for EmergencyControlObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EmergencyControlObservation { .. }")
    }
}

impl EmergencyControlObservation {
    pub(crate) const fn target(&self) -> EmergencyControlTarget {
        self.target
    }

    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) const fn signal(&self) -> Option<EmergencyControlSignal> {
        self.signal
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmergencyControlSignalOutcome {
    Accepted {
        epoch: u64,
    },
    AlreadySignaled {
        epoch: u64,
        signal: EmergencyControlSignal,
    },
    TerminalReserved {
        epoch: u64,
    },
    StaleTarget,
}

impl EmergencyControlSignalOutcome {
    pub(crate) const fn epoch(self) -> Option<u64> {
        match self {
            Self::Accepted { epoch }
            | Self::AlreadySignaled { epoch, .. }
            | Self::TerminalReserved { epoch } => Some(epoch),
            Self::StaleTarget => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnTerminalClaimOutcome {
    Accepted,
    AlreadyReserved,
    ControlWon { signal: EmergencyControlSignal },
    StaleTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmergencyControlBindError {
    EpochExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmergencyControlMigrationError {
    StaleObservation,
    EpochExhausted,
}

struct EmergencyControlState {
    target: Option<EmergencyControlTarget>,
    epoch: u64,
    signal: Option<EmergencyControlSignal>,
    terminal_reserved: bool,
    cancellation: CancellationToken,
}

/// A process-local sticky emergency signal owned by one Session control plane.
///
/// The handle owns the opaque `EmergencyControlOwner` identity Arc: every observation it
/// binds/observes carries a clone of that exact owner, and every method that consumes or
/// validates an observation requires `Arc::ptr_eq` with this handle's owner in normal
/// release code — a foreign handle bound to the same target at the same epoch is rejected,
/// never `debug_assert`-only. Binding a new target retires any prior signal. For an exact Turn,
/// signal and terminal reservation share this same mutex as a private commit gate: either
/// control wins and cancels the basis, or terminal wins and every later signal reports
/// `TerminalReserved`. Stale targets cannot affect the current target or its cancellation
/// wakeup.
#[derive(Clone)]
pub(crate) struct EmergencyControlHandle {
    owner: Arc<EmergencyControlOwner>,
    state: Arc<Mutex<EmergencyControlState>>,
}

impl std::fmt::Debug for EmergencyControlHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EmergencyControlHandle { .. }")
    }
}

impl EmergencyControlHandle {
    pub(crate) fn new() -> Self {
        Self {
            owner: Arc::new(EmergencyControlOwner),
            state: Arc::new(Mutex::new(EmergencyControlState {
                target: None,
                epoch: 0,
                signal: None,
                terminal_reserved: false,
                cancellation: CancellationToken::new(),
            })),
        }
    }

    /// The exact-owner check every observation-consuming method performs first: the
    /// observation must carry this handle's own opaque owner Arc, never a foreign one.
    fn owner_is(&self, observation: &EmergencyControlObservation) -> bool {
        Arc::ptr_eq(&self.owner, &observation.owner)
    }

    /// The fixed basis check (owner + target + epoch) under the owner mutex, shared by the
    /// permit validations: no callback, no await, no nested lock.  A foreign owner Arc is
    /// rejected before the mutex is even taken.
    fn basis_is_current(
        &self,
        owner: &Arc<EmergencyControlOwner>,
        target: EmergencyControlTarget,
        epoch: u64,
    ) -> bool {
        if !Arc::ptr_eq(&self.owner, owner) {
            return false;
        }
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        state.target == Some(target) && state.epoch == epoch
    }

    pub(crate) fn bind(
        &self,
        target: EmergencyControlTarget,
    ) -> Result<EmergencyControlObservation, EmergencyControlBindError> {
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        state.epoch = state
            .epoch
            .checked_add(1)
            .ok_or(EmergencyControlBindError::EpochExhausted)?;
        state.cancellation.cancel();
        state.target = Some(target);
        state.signal = None;
        state.terminal_reserved = false;
        state.cancellation = CancellationToken::new();
        Ok(EmergencyControlObservation {
            owner: Arc::clone(&self.owner),
            target,
            epoch: state.epoch,
            signal: None,
        })
    }

    pub(crate) fn observe(
        &self,
        target: EmergencyControlTarget,
    ) -> Option<EmergencyControlObservation> {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        (state.target == Some(target)).then_some(EmergencyControlObservation {
            owner: Arc::clone(&self.owner),
            target,
            epoch: state.epoch,
            signal: state.signal,
        })
    }

    /// The locked first-wins signal application shared by `signal` and `signal_current`: a
    /// stale target reports `StaleTarget`, an already-won terminal reports
    /// `TerminalReserved`, an already-won signal keeps its exact epoch and signal
    /// (`AlreadySignaled` never overwrites it), and a fresh signal is set with the emergency
    /// cancellation token cancelled and reports `Accepted`. The caller holds the
    /// owner mutex; the section runs no callback, no await, and locks no nested guard, so a
    /// caller that must hand the signal off synchronously can never be stalled by this
    /// application.
    fn apply_signal(
        state: &mut EmergencyControlState,
        target: EmergencyControlTarget,
        signal: EmergencyControlSignal,
    ) -> EmergencyControlSignalOutcome {
        if state.target != Some(target) {
            return EmergencyControlSignalOutcome::StaleTarget;
        }
        if state.terminal_reserved {
            return EmergencyControlSignalOutcome::TerminalReserved { epoch: state.epoch };
        }
        if let Some(existing) = state.signal {
            return EmergencyControlSignalOutcome::AlreadySignaled {
                epoch: state.epoch,
                signal: existing,
            };
        }
        state.signal = Some(signal);
        state.cancellation.cancel();
        EmergencyControlSignalOutcome::Accepted { epoch: state.epoch }
    }

    /// Atomically reserves terminal settlement for one exact active Turn.  This is the
    /// private terminal/control commit gate: final Assistant live commit and actor-owned
    /// failure/terminal completion call this method, while every Cancel/Security/Unload
    /// producer calls `signal`/`signal_current` under the same mutex.  Exactly one side can
    /// transition an open Turn basis; no callback, await, or nested lock runs here.
    pub(crate) fn claim_turn_terminal(
        &self,
        observation: &EmergencyControlObservation,
    ) -> TurnTerminalClaimOutcome {
        if !self.owner_is(observation)
            || !matches!(observation.target, EmergencyControlTarget::Turn(_))
        {
            return TurnTerminalClaimOutcome::StaleTarget;
        }
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if state.target != Some(observation.target) || state.epoch != observation.epoch {
            return TurnTerminalClaimOutcome::StaleTarget;
        }
        if let Some(signal) = state.signal {
            return TurnTerminalClaimOutcome::ControlWon { signal };
        }
        if state.terminal_reserved {
            return TurnTerminalClaimOutcome::AlreadyReserved;
        }
        state.terminal_reserved = true;
        TurnTerminalClaimOutcome::Accepted
    }

    pub(crate) fn signal(
        &self,
        target: EmergencyControlTarget,
        signal: EmergencyControlSignal,
    ) -> EmergencyControlSignalOutcome {
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        Self::apply_signal(&mut state, target, signal)
    }

    /// The synchronous close handoff: signals the current bound target (if any) with the
    /// exact same fixed nonblocking first-wins semantics as `signal`, under the same owner
    /// mutex. `None` means no target is bound; an already-won terminal returns
    /// `TerminalReserved`; an already-won signal returns its exact `AlreadySignaled` epoch and
    /// signal (never overwritten); a fresh signal is set and the emergency cancellation token
    /// cancelled, returning `Accepted`. No callback, no await,
    /// no nested lock: a Session unload caller makes the sticky unload signal observable on
    /// any active Submit/Turn basis before it wakes the actor or workers, without any
    /// scheduling or poll guess.
    pub(crate) fn signal_current(
        &self,
        signal: EmergencyControlSignal,
    ) -> Option<(EmergencyControlTarget, EmergencyControlSignalOutcome)> {
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        let target = state.target?;
        Some((target, Self::apply_signal(&mut state, target, signal)))
    }

    /// Atomically retires one exact owner-bound emergency basis and captures its first-wins
    /// signal, all under the single owner mutex that `signal`, `bind`, and `migrate_target`
    /// lock.
    ///
    /// This is the admission-failure cleanup linearization point: the caller that must
    /// settle an admission failure with the truthful emergency reason performs its exact
    /// retire-and-capture here, so the signal observation and the basis retirement can never
    /// be split by a concurrent `request_closing`/`signal_current` between them (the old
    /// observe-then-retire pair could observe the signal, then lose the retirement race, or
    /// observe a clean basis and retire a basis that was just signaled).  The mutex section
    /// runs no callback, no await, and locks no nested guard.  A foreign observation (one
    /// whose owner Arc is not this handle's) is rejected in normal release code.  `Some(Some(signal))` is the
    /// captured first-wins signal of the exact retired basis, `Some(None)` is an exact
    /// retired basis that was never signaled, and `None` means the observation is stale or
    /// foreign (the exact target + epoch is no longer current, or the owner does not match)
    /// and nothing was mutated.
    pub(crate) fn retire_and_capture_signal(
        &self,
        observation: &EmergencyControlObservation,
    ) -> Option<Option<EmergencyControlSignal>> {
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
        {
            return None;
        }
        let signal = state.signal;
        state.cancellation.cancel();
        state.target = None;
        state.signal = None;
        state.terminal_reserved = false;
        state.cancellation = CancellationToken::new();
        Some(signal)
    }

    /// Retires one exact owner-bound emergency basis, or `false` when the observation is
    /// stale or foreign (a foreign handle bound to the same target at the same epoch is
    /// never retired by this handle).
    pub(crate) fn retire(&self, observation: &EmergencyControlObservation) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
        {
            return false;
        }
        state.cancellation.cancel();
        state.target = None;
        state.signal = None;
        state.terminal_reserved = false;
        state.cancellation = CancellationToken::new();
        true
    }

    /// Atomically migrates one exact owner-bound emergency basis to a new target, preserving
    /// the exact first-wins signal, all under the single owner mutex that `signal`, `bind`,
    /// and `retire` lock.
    ///
    /// This is the Submit→Turn admission success linearization point: the caller that must
    /// hand the admission's emergency basis to the new Turn performs the whole transition in
    /// one owner-mutex section — validate the exact owner + old target + epoch,
    /// checked-increment the epoch, capture the old signal, cancel the old cancellation
    /// token, install the new target at the new epoch, preserve the exact old signal on the
    /// new basis, and install the new CancellationToken (immediately cancelled when a signal
    /// is preserved) — so a concurrent `request_closing`/`signal_current` can never land in
    /// the gap of the old observe-then-retire-then-bind-then-re-signal sequence and erase or
    /// duplicate a signal.  The mutex section runs no callback, no await, and locks no
    /// nested guard.
    ///
    /// `Err(StaleObservation)` means the exact owner + old target + epoch is no longer
    /// current (including a foreign observation) and nothing was mutated;
    /// `Err(EpochExhausted)` means the checked epoch increment failed and the state is left
    /// exactly as it was.  `Ok` carries the new observation (whose signal is the preserved
    /// first-wins signal) and the preserved signal itself, so the caller can settle the
    /// preserved reason without any further observation.
    pub(crate) fn migrate_target(
        &self,
        observation: &EmergencyControlObservation,
        new_target: EmergencyControlTarget,
    ) -> Result<
        (EmergencyControlObservation, Option<EmergencyControlSignal>),
        EmergencyControlMigrationError,
    > {
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
        {
            return Err(EmergencyControlMigrationError::StaleObservation);
        }
        let epoch = state
            .epoch
            .checked_add(1)
            .ok_or(EmergencyControlMigrationError::EpochExhausted)?;
        let signal = state.signal;
        state.cancellation.cancel();
        state.target = Some(new_target);
        state.epoch = epoch;
        state.terminal_reserved = false;
        let cancellation = CancellationToken::new();
        if signal.is_some() {
            cancellation.cancel();
        }
        state.cancellation = cancellation;
        Ok((
            EmergencyControlObservation {
                owner: Arc::clone(&self.owner),
                target: new_target,
                epoch,
                signal,
            },
            signal,
        ))
    }

    /// Whether the exact owner-bound observation is still the current basis: the opaque
    /// owner Arc must be this handle's own (a foreign handle bound to the same target at the
    /// same epoch is never current here), and the target + epoch must match.
    pub(crate) fn is_current(&self, observation: &EmergencyControlObservation) -> bool {
        self.basis_is_current(&observation.owner, observation.target, observation.epoch)
    }

    /// Whether the exact owner-bound observation is still the current basis and unsignaled:
    /// the owner identity check runs first in normal release code, then the fixed
    /// target/epoch/signal fields under the owner mutex (no callback, no await, no nested
    /// lock).  A foreign observation can never pass.
    pub(crate) fn is_unsignaled_current(&self, observation: &EmergencyControlObservation) -> bool {
        if !self.owner_is(observation) {
            return false;
        }
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        state.target == Some(observation.target)
            && state.epoch == observation.epoch
            && state.signal.is_none()
    }

    /// Reserves one exact Tool start gate only while the exact `target` + `epoch` is current
    /// and unsignaled, holding the same owner mutex that `signal` and `bind` lock.
    ///
    /// This is the linearization point for first-wins start reservations: a caller that must
    /// not start a Tool side effect when Cancel/SecurityRevoked has already won performs its
    /// exact `ToolStartGate::reserve` here, so `signal` and the reservation serialize on one
    /// mutex instead of observe-then-lock racing.  The gate reservation itself is now only an
    /// exact-capture check plus one lock-free atomic CAS: the gate owns no nested mutex, so
    /// holding the emergency owner mutex here locks no sibling guard (no lock ordering), can
    /// never poison the gate (the gate has no lock to poison), and runs no caller callback or
    /// arbitrary work — `signal` and `bind` can never be stalled by this reservation.
    /// `None` means the exact basis is not current or is already signaled (first-wins went to
    /// the signal, or the basis is stale); `Some` is the gate reservation result.  Outside a
    /// Round the caller consumes the returned permit with `start` to obtain the typed
    /// `ToolStartedExecution` proof; the start transition runs outside this mutex, so a
    /// reservation that won here always produces its proof even when the signal lands right
    /// after the reservation (the signal closes only the emergency basis, never the
    /// already-Reserved gate).
    pub(crate) fn reserve_tool_start(
        &self,
        observation: &EmergencyControlObservation,
        gate: &ToolStartGate,
        request: &ToolExecutionRequest,
    ) -> Option<Result<ToolStartPermit, ToolStartError>> {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
            || state.signal.is_some()
        {
            return None;
        }
        Some(gate.reserve(request))
    }

    /// Reserves one exact Interaction presentation only while the exact `target` + `epoch` is
    /// current and unsignaled, holding the same owner mutex that `signal` and `bind` lock.
    ///
    /// This is the linearization point for first-wins interaction presentation: a caller that
    /// must not present a pending Interaction when Cancel/SecurityRevoked has already won
    /// performs its exact reservation here, so `signal` and the reservation serialize on one
    /// mutex instead of observe-then-send racing.  The mutex section runs no callback, no
    /// await, no caller work, and locks no nested guard: it only compares the fixed target/
    /// epoch/signal fields and constructs the permit.  `None` means the exact basis is not
    /// current or is already signaled (first-wins went to the signal, or the basis is stale);
    /// `Some` is the move-only typed permit, which then authorizes the presentation even if
    /// the signal lands right after the reservation (the signal closes only the emergency
    /// basis, never the already-reserved presentation).
    /// Reserves one exact Interaction presentation only while the exact owner-bound
    /// `target` + `epoch` is current and unsignaled, holding the same owner mutex that
    /// `signal` and `bind` lock.
    ///
    /// This is the linearization point for first-wins interaction presentation: a caller that
    /// must not present a pending Interaction when Cancel/SecurityRevoked has already won
    /// performs its exact reservation here, so `signal` and the reservation serialize on one
    /// mutex instead of observe-then-send racing.  The mutex section runs no callback, no
    /// await, no caller work, and locks no nested guard: it only compares the fixed owner/
    /// target/epoch/signal fields and constructs the permit with the exact `ToolExecutionRequest`
    /// capture.  `None` means the exact owner-bound basis is not current or is already
    /// signaled (first-wins went to the signal, the basis is stale, or the observation is
    /// foreign); `Some` is the move-only typed permit, which then authorizes the exact
    /// presentation even if the signal lands right after the reservation (the signal closes
    /// only the emergency basis, never the already-reserved presentation).
    pub(crate) fn reserve_interaction_presentation(
        &self,
        observation: &EmergencyControlObservation,
        request: &ToolExecutionRequest,
    ) -> Option<InteractionPresentationPermit> {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
            || state.signal.is_some()
        {
            return None;
        }
        Some(InteractionPresentationPermit {
            owner: Arc::clone(&self.owner),
            target: observation.target,
            epoch: observation.epoch,
            request: request.clone(),
        })
    }

    /// Reserves one exact UserQuestion answer binding only while the exact owner-bound
    /// `target` + `epoch` is current and unsignaled, holding the same owner mutex that
    /// `signal` and `bind` lock.
    ///
    /// Same first-wins linearization as `reserve_interaction_presentation`, for the answer
    /// binding: an answered question must not invoke its Tools binding when the signal has
    /// already won, and a binding that won its reservation here is authorized even when the
    /// signal lands right after (the signal never rescinds an already-authorized binding).
    /// The mutex section runs no callback, no await, no caller work, and locks no nested
    /// guard: it only compares the fixed owner/target/epoch/signal fields and constructs the
    /// permit with the exact `ToolExecutionRequest` capture.
    pub(crate) fn reserve_user_question_binding(
        &self,
        observation: &EmergencyControlObservation,
        request: &ToolExecutionRequest,
    ) -> Option<UserQuestionBindingPermit> {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
            || state.signal.is_some()
        {
            return None;
        }
        Some(UserQuestionBindingPermit {
            owner: Arc::clone(&self.owner),
            target: observation.target,
            epoch: observation.epoch,
            request: request.clone(),
        })
    }

    /// Reserves one exact unstarted Tool settlement only while the exact owner-bound
    /// `target` + `epoch` is current and unsignaled, holding the same owner mutex that
    /// `signal` and `bind` lock.
    ///
    /// This is the linearization point for first-wins unstarted settlement: a not-yet-driven
    /// Prepared slot that must settle a fixed local outcome (a frozen unknown/planner result,
    /// a frozen PreExecution result, or the generic failed-before-start outcome) when
    /// Cancel/SecurityRevoked has not already won performs its exact reservation here, so
    /// `signal` and the settlement serialize on one mutex instead of observe-then-settle
    /// racing.  The mutex section runs no callback, no await, no caller work, and locks no
    /// nested guard: it only compares the fixed owner/target/epoch/signal fields and
    /// constructs the permit with the exact `ToolExecutionRequest` capture.  `None` means the
    /// exact owner-bound basis is not current or is already signaled (first-wins went to the
    /// signal, the basis is stale, or the observation is foreign, and the slot settles the
    /// matching cancelled-before-start outcome); `Some` is the move-only typed permit, which
    /// then authorizes exactly one fixed local settlement of the caller's choosing even if
    /// the signal lands right after the reservation (the signal closes only the emergency
    /// basis, never the already-authorized local settlement).
    pub(crate) fn reserve_unstarted_tool_settlement(
        &self,
        observation: &EmergencyControlObservation,
        request: &ToolExecutionRequest,
    ) -> Option<UnstartedToolSettlementPermit> {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
            || state.signal.is_some()
        {
            return None;
        }
        Some(UnstartedToolSettlementPermit {
            owner: Arc::clone(&self.owner),
            target: observation.target,
            epoch: observation.epoch,
            request: request.clone(),
        })
    }

    /// Reserves one exact Interaction host-resolution only while the exact owner-bound
    /// `target` + `epoch` is current and unsignaled, holding the same owner mutex that
    /// `signal` and `bind` lock.
    ///
    /// This is the linearization point for first-wins host resolution: a host
    /// Allow/UserAnswer/Denied that must not become a durable fact when
    /// Cancel/SecurityRevoked (especially the synchronous `request_closing` close handoff)
    /// has already won performs its exact reservation here, so `signal_current` and the
    /// reservation serialize on one mutex instead of observe-then-apply racing.  The mutex
    /// section runs no callback, no await, no caller work, and locks no nested guard: it
    /// only compares the fixed owner/target/epoch/signal fields and constructs the permit
    /// with the exact `ToolExecutionRequest` capture.  `None` means the exact owner-bound
    /// basis is not current or is already signaled (first-wins went to the signal, the basis
    /// is stale, or the observation is foreign): the actor must not apply the host
    /// resolution and instead settles the pending interaction with the exact first-wins
    /// signal reason.  `Some` is the move-only typed permit, which authorizes the exact
    /// host resolution apply even if the signal lands right after the reservation (the
    /// signal closes only the emergency basis, never the already-authorized resolution; the
    /// next stage's start/binding gate handles the signal).
    pub(crate) fn reserve_interaction_resolution(
        &self,
        observation: &EmergencyControlObservation,
        request: &ToolExecutionRequest,
    ) -> Option<InteractionResolutionPermit> {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if !self.owner_is(observation)
            || state.target != Some(observation.target)
            || state.epoch != observation.epoch
            || state.signal.is_some()
        {
            return None;
        }
        Some(InteractionResolutionPermit {
            owner: Arc::clone(&self.owner),
            target: observation.target,
            epoch: observation.epoch,
            request: request.clone(),
        })
    }

    /// Awaits the exact owner-bound basis's first-wins cancellation.  A stale, retired, or
    /// foreign observation returns immediately (it never waits on another owner's basis and
    /// never implies the observation is current).
    pub(crate) async fn cancelled(&self, observation: &EmergencyControlObservation) {
        let cancellation = {
            let state = self
                .state
                .lock()
                .expect("emergency control state is not poisoned");
            if !self.owner_is(observation)
                || state.target != Some(observation.target)
                || state.epoch != observation.epoch
            {
                return;
            }
            state.cancellation.clone()
        };
        cancellation.cancelled().await;
    }
}

/// The move-only, redacted typed permit that one exact Interaction presentation already won
/// first-wins against the emergency signal.
///
/// Only `EmergencyControlHandle::reserve_interaction_presentation` constructs it, under the
/// same owner mutex that `signal` and `bind` lock, and only while the exact owner-bound
/// `target` + `epoch` is current and unsignaled.  The permit binds that exact owner Arc +
/// target + epoch and the exact `ToolExecutionRequest` capture, and travels with the
/// `InteractionRequested` completion to the actor, which validates the exact owner/Turn
/// basis and the exact request capture before applying the pending interaction.  It is
/// deliberately not Clone: one presentation authorization is consumed by exactly one
/// completion.
pub(crate) struct InteractionPresentationPermit {
    owner: Arc<EmergencyControlOwner>,
    target: EmergencyControlTarget,
    epoch: u64,
    request: ToolExecutionRequest,
}

impl std::fmt::Debug for InteractionPresentationPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InteractionPresentationPermit { .. }")
    }
}

impl InteractionPresentationPermit {
    /// The exact target the presentation was reserved against; only the owner's completion
    /// path reads it.
    pub(crate) const fn target(&self) -> EmergencyControlTarget {
        self.target
    }

    /// The exact epoch the presentation was reserved against; only the owner's completion
    /// path reads it.
    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The exact `ToolExecutionRequest` capture the presentation was reserved for; only the
    /// owner's completion path reads it.
    pub(crate) fn request(&self) -> &ToolExecutionRequest {
        &self.request
    }

    /// The completion-side authorization, in normal release code (never `debug_assert`-only):
    /// the permit must carry the exact control owner Arc (a foreign handle bound to the same
    /// target + epoch fails), the exact reserved target + epoch must still be the bound
    /// emergency basis (a completion carried on a retired/rebound basis is stale), and the
    /// presented request must be the exact capture the permit was reserved for.  The signal
    /// state is deliberately not consulted: a signal that lands after the reservation
    /// already lost first-wins at presentation time, and the owner still applies the pending
    /// interaction and settles the exact cancellation itself.
    pub(crate) fn validates(
        &self,
        control: &EmergencyControlHandle,
        request: &ToolExecutionRequest,
    ) -> bool {
        control.basis_is_current(&self.owner, self.target, self.epoch)
            && self.request.is_exact_capture(request)
    }

    /// The completion-side validation of the exact owner + reserved basis only (the exact
    /// request capture is validated separately with `validates`).
    pub(crate) fn is_current(&self, control: &EmergencyControlHandle) -> bool {
        control.basis_is_current(&self.owner, self.target, self.epoch)
    }
}

/// The move-only, redacted typed permit that one exact UserQuestion answer binding already
/// won first-wins against the emergency signal.
///
/// Only `EmergencyControlHandle::reserve_user_question_binding` constructs it, under the same
/// owner mutex that `signal` and `bind` lock, and only while the exact owner-bound
/// `target` + `epoch` is current and unsignaled.  The permit binds that exact owner Arc +
/// target + epoch and the exact `ToolExecutionRequest` capture, and is held by the
/// private answer wrapper until the operation slot consumes it right before the Tools
/// binding runs: a signal that lands after the reservation never rescinds the already-
/// authorized binding.  It is deliberately not Clone: one answer binding authorization is
/// consumed exactly once.
pub(crate) struct UserQuestionBindingPermit {
    owner: Arc<EmergencyControlOwner>,
    target: EmergencyControlTarget,
    epoch: u64,
    request: ToolExecutionRequest,
}

impl std::fmt::Debug for UserQuestionBindingPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UserQuestionBindingPermit { .. }")
    }
}

impl UserQuestionBindingPermit {
    /// The exact target the binding was reserved against; only the consuming slot's defensive
    /// re-check reads it.
    pub(crate) const fn target(&self) -> EmergencyControlTarget {
        self.target
    }

    /// The exact epoch the binding was reserved against; only the consuming slot's defensive
    /// re-check reads it.
    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The exact `ToolExecutionRequest` capture the binding was reserved for; only the
    /// consuming slot's re-check reads it.
    pub(crate) fn request(&self) -> &ToolExecutionRequest {
        &self.request
    }

    /// The slot-side authorization, in normal release code (never `debug_assert`-only): the
    /// permit must carry the exact control owner Arc, the exact reserved target + epoch must
    /// still be the bound basis, and the slot's request must be the exact capture the
    /// binding was reserved for.  A failing validation means the consuming slot fails closed
    /// to its identity-bound Abandoned outcome and never invokes the Tools binding.
    pub(crate) fn validates(
        &self,
        control: &EmergencyControlHandle,
        request: &ToolExecutionRequest,
    ) -> bool {
        control.basis_is_current(&self.owner, self.target, self.epoch)
            && self.request.is_exact_capture(request)
    }
}

/// The move-only, redacted typed permit that one exact unstarted Tool settlement already won
/// first-wins against the emergency signal.
///
/// Only `EmergencyControlHandle::reserve_unstarted_tool_settlement` constructs it, under the
/// same owner mutex that `signal` and `bind` lock, and only while the exact owner-bound
/// `target` + `epoch` is current and unsignaled.  The permit binds that exact owner Arc +
/// target + epoch and the exact `ToolExecutionRequest` capture, and authorizes the
/// consuming Prepared slot to settle exactly one fixed local outcome (a frozen
/// unknown/planner result, a frozen PreExecution result, or the generic failed-before-start
/// outcome); it carries no callback and no request payload beyond the exact capture, so
/// nothing caller-supplied ever runs under or through it.  It is deliberately not Clone: one
/// unstarted settlement authorization is consumed by exactly one slot transition to Terminal.
pub(crate) struct UnstartedToolSettlementPermit {
    owner: Arc<EmergencyControlOwner>,
    target: EmergencyControlTarget,
    epoch: u64,
    request: ToolExecutionRequest,
}

impl std::fmt::Debug for UnstartedToolSettlementPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UnstartedToolSettlementPermit { .. }")
    }
}

impl UnstartedToolSettlementPermit {
    /// The exact target the settlement was reserved against; only the consuming slot's
    /// defensive re-check reads it.
    pub(crate) const fn target(&self) -> EmergencyControlTarget {
        self.target
    }

    /// The exact epoch the settlement was reserved against; only the consuming slot's
    /// defensive re-check reads it.
    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The exact `ToolExecutionRequest` capture the settlement was reserved for; only the
    /// consuming slot's re-check reads it.
    pub(crate) fn request(&self) -> &ToolExecutionRequest {
        &self.request
    }

    /// The slot-side authorization, in normal release code (never `debug_assert`-only): the
    /// permit must carry the exact control owner Arc, the exact reserved target + epoch must
    /// still be the bound basis, and the slot's request must be the exact capture the
    /// settlement was reserved for.  A failing validation means the consuming slot fails
    /// closed to its identity-bound Abandoned outcome and never settles the frozen outcome.
    pub(crate) fn validates(
        &self,
        control: &EmergencyControlHandle,
        request: &ToolExecutionRequest,
    ) -> bool {
        control.basis_is_current(&self.owner, self.target, self.epoch)
            && self.request.is_exact_capture(request)
    }
}

/// The move-only, redacted typed permit that one exact Interaction host resolution already
/// won first-wins against the emergency signal.
///
/// Only `EmergencyControlHandle::reserve_interaction_resolution` constructs it, under the
/// same owner mutex that `signal` and `bind` lock, and only while the exact owner-bound
/// `target` + `epoch` is current and unsignaled.  The permit binds that exact owner Arc +
/// target + epoch and the exact `ToolExecutionRequest` capture of the pending interaction,
/// and authorizes the actor to apply exactly one host resolution (Allow/UserAnswer/Denied)
/// even if the signal lands right after the reservation (the next stage's start/binding
/// gate handles the signal).  It is deliberately not Clone: one resolution authorization is
/// consumed by exactly one apply.
pub(crate) struct InteractionResolutionPermit {
    owner: Arc<EmergencyControlOwner>,
    target: EmergencyControlTarget,
    epoch: u64,
    request: ToolExecutionRequest,
}

impl std::fmt::Debug for InteractionResolutionPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InteractionResolutionPermit { .. }")
    }
}

impl InteractionResolutionPermit {
    /// The exact target the resolution was reserved against; only the owner's apply path
    /// reads it.
    pub(crate) const fn target(&self) -> EmergencyControlTarget {
        self.target
    }

    /// The exact epoch the resolution was reserved against; only the owner's apply path
    /// reads it.
    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The exact `ToolExecutionRequest` capture of the pending interaction the resolution
    /// was reserved for; only the owner's apply path reads it.
    pub(crate) fn request(&self) -> &ToolExecutionRequest {
        &self.request
    }

    /// The apply-side authorization, in normal release code (never `debug_assert`-only): the
    /// permit must carry the exact control owner Arc, the exact reserved target + epoch must
    /// still be the bound emergency basis, and the pending interaction's request must be the
    /// exact capture the resolution was reserved for.  A failing validation means the actor
    /// must not apply the host resolution.
    pub(crate) fn validates(
        &self,
        control: &EmergencyControlHandle,
        request: &ToolExecutionRequest,
    ) -> bool {
        control.basis_is_current(&self.owner, self.target, self.epoch)
            && self.request.is_exact_capture(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FollowUpQueueError {
    Full,
    DuplicateCommandId,
}

pub(crate) struct QueuedFollowUp {
    command_id: CommandId,
    intent: PromptIntent,
}

impl QueuedFollowUp {
    pub(crate) fn new(command_id: CommandId, intent: PromptIntent) -> Self {
        Self { command_id, intent }
    }

    pub(crate) const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub(crate) fn into_parts(self) -> (CommandId, PromptIntent) {
        (self.command_id, self.intent)
    }
}

pub(crate) struct FollowUpQueue {
    entries: VecDeque<QueuedFollowUp>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SteerQueueError {
    Full,
    DuplicateCommandId,
}

pub(crate) struct QueuedSteer {
    command_id: CommandId,
    turn_id: TurnId,
    intent: PromptIntent,
}

impl QueuedSteer {
    pub(crate) fn new(command_id: CommandId, turn_id: TurnId, intent: PromptIntent) -> Self {
        Self {
            command_id,
            turn_id,
            intent,
        }
    }

    pub(crate) const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub(crate) const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) fn into_parts(self) -> (CommandId, TurnId, PromptIntent) {
        (self.command_id, self.turn_id, self.intent)
    }
}

pub(crate) struct SteerQueue {
    entries: BTreeMap<TurnId, VecDeque<QueuedSteer>>,
    len: usize,
}

impl Default for SteerQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SteerQueue {
    pub(crate) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            len: 0,
        }
    }

    pub(crate) fn try_push(
        &mut self,
        turn_id: TurnId,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), SteerQueueError> {
        if self
            .entries
            .values()
            .flatten()
            .any(|entry| entry.command_id() == command_id)
        {
            return Err(SteerQueueError::DuplicateCommandId);
        }
        if self
            .entries
            .get(&turn_id)
            .is_some_and(|queue| queue.len() == STEER_QUEUE_CAPACITY)
        {
            return Err(SteerQueueError::Full);
        }
        self.entries
            .entry(turn_id)
            .or_default()
            .push_back(QueuedSteer::new(command_id, turn_id, intent));
        self.len += 1;
        Ok(())
    }

    pub(crate) fn pop_front_for_turn(&mut self, turn_id: TurnId) -> Option<QueuedSteer> {
        let queue = self.entries.get_mut(&turn_id)?;
        let entry = queue.pop_front()?;
        self.len -= 1;
        if queue.is_empty() {
            self.entries.remove(&turn_id);
        }
        Some(entry)
    }

    pub(crate) fn remove(&mut self, command_id: CommandId) -> Option<QueuedSteer> {
        let turn_id = self.entries.iter().find_map(|(turn_id, queue)| {
            queue
                .iter()
                .any(|entry| entry.command_id() == command_id)
                .then_some(*turn_id)
        })?;
        let queue = self.entries.get_mut(&turn_id)?;
        let index = queue
            .iter()
            .position(|entry| entry.command_id() == command_id)?;
        let entry = queue.remove(index)?;
        self.len -= 1;
        if queue.is_empty() {
            self.entries.remove(&turn_id);
        }
        Some(entry)
    }

    pub(crate) fn contains(&self, command_id: CommandId) -> bool {
        self.entries
            .values()
            .flatten()
            .any(|entry| entry.command_id() == command_id)
    }

    pub(crate) fn clear_for_turn(&mut self, turn_id: TurnId) -> Vec<QueuedSteer> {
        let Some(queue) = self.entries.remove(&turn_id) else {
            return Vec::new();
        };
        self.len -= queue.len();
        queue.into_iter().collect()
    }

    pub(crate) fn command_ids_for_turn(&self, turn_id: TurnId) -> Vec<CommandId> {
        self.entries
            .get(&turn_id)
            .map(|queue| queue.iter().map(QueuedSteer::command_id).collect())
            .unwrap_or_default()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.len = 0;
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for FollowUpQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl FollowUpQueue {
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(FOLLOW_UP_QUEUE_CAPACITY),
        }
    }

    pub(crate) fn try_push(
        &mut self,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), FollowUpQueueError> {
        if self
            .entries
            .iter()
            .any(|entry| entry.command_id() == command_id)
        {
            return Err(FollowUpQueueError::DuplicateCommandId);
        }
        if self.entries.len() == FOLLOW_UP_QUEUE_CAPACITY {
            return Err(FollowUpQueueError::Full);
        }
        self.entries
            .push_back(QueuedFollowUp::new(command_id, intent));
        Ok(())
    }

    pub(crate) fn pop_front(&mut self) -> Option<QueuedFollowUp> {
        self.entries.pop_front()
    }

    /// Re-inserts one FollowUp at the front of the queue.  This is the internal
    /// admission-failure re-queue path only: the caller guarantees the entry was just popped
    /// from this queue (so capacity is guaranteed and its command_id is not queued again), and
    /// the insertion is infallible.
    pub(crate) fn push_front(&mut self, command_id: CommandId, intent: PromptIntent) {
        self.entries
            .push_front(QueuedFollowUp::new(command_id, intent));
    }

    pub(crate) fn remove(&mut self, command_id: CommandId) -> Option<QueuedFollowUp> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.command_id() == command_id)?;
        self.entries.remove(index)
    }

    pub(crate) fn command_ids(&self) -> Vec<CommandId> {
        self.entries
            .iter()
            .map(QueuedFollowUp::command_id)
            .collect()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn contains(&self, command_id: CommandId) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.command_id() == command_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::prompt::{PromptBodyIntent, PromptIntent, TextIntent};
    use crate::tools::{ToolCall, ToolExecutionRequest, ToolStartError, ToolStartGate};
    use crate::wire::{CommandId, TurnId};

    use super::{
        EmergencyControlHandle, EmergencyControlMigrationError, EmergencyControlObservation,
        EmergencyControlSignal, EmergencyControlSignalOutcome, EmergencyControlTarget,
        FOLLOW_UP_QUEUE_CAPACITY, FollowUpQueue, FollowUpQueueError, InteractionPresentationPermit,
        InteractionResolutionPermit, QueuedSteer, STEER_QUEUE_CAPACITY, SteerQueue,
        SteerQueueError, UnstartedToolSettlementPermit, UserQuestionBindingPermit,
    };

    use futures_util::FutureExt as _;

    fn intent(text: &str) -> PromptIntent {
        PromptIntent::new(
            PromptBodyIntent::Text(TextIntent::new(text).unwrap()),
            Vec::new(),
        )
        .unwrap()
    }

    fn command_id(value: u8) -> CommandId {
        format!("cmd_{value:032x}").parse().unwrap()
    }

    fn turn_id(value: u8) -> TurnId {
        format!("trn_{value:032x}").parse().unwrap()
    }

    fn tool_request(value: u8, call_id: &str, call_index: u32) -> ToolExecutionRequest {
        ToolExecutionRequest::new(
            format!("itm_{value:032x}").parse().unwrap(),
            ToolCall::new(
                call_id.parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                call_index,
            ),
        )
    }

    /// Test-only field tampering: rebinds the exact target of an owner-bound observation
    /// while keeping its owner and epoch, for the stale-target rejection cases.
    fn tamper_target(
        mut observation: EmergencyControlObservation,
        target: EmergencyControlTarget,
    ) -> EmergencyControlObservation {
        observation.target = target;
        observation
    }

    #[test]
    fn follow_up_queue_is_bounded_fifo_and_removable_by_command_id() {
        let mut queue = FollowUpQueue::new();
        let first = command_id(1);
        let second = command_id(2);
        queue.try_push(first, intent("first")).unwrap();
        queue.try_push(second, intent("second")).unwrap();

        let removed = queue.remove(second).unwrap();
        assert_eq!(removed.command_id(), second);
        assert_eq!(queue.len(), 1);

        let (command_id, _) = queue.pop_front().unwrap().into_parts();
        assert_eq!(command_id, first);
        assert!(queue.is_empty());
    }

    #[test]
    fn follow_up_queue_rejects_duplicate_and_over_capacity_admission() {
        let mut queue = FollowUpQueue::new();
        let duplicate = command_id(7);
        queue.try_push(duplicate, intent("first")).unwrap();
        assert_eq!(
            queue.try_push(duplicate, intent("different")),
            Err(FollowUpQueueError::DuplicateCommandId)
        );

        for value in 1..=u8::try_from(FOLLOW_UP_QUEUE_CAPACITY).unwrap() {
            if value != 7 {
                queue.try_push(command_id(value), intent("queued")).unwrap();
            }
        }
        assert_eq!(queue.len(), FOLLOW_UP_QUEUE_CAPACITY);
        assert_eq!(
            queue.try_push(command_id(31), intent("overflow")),
            Err(FollowUpQueueError::Full)
        );
    }

    #[test]
    fn steer_queue_keeps_independent_turn_fifos_and_clears_one_turn() {
        let mut queue = SteerQueue::new();
        let first_turn = turn_id(1);
        let second_turn = turn_id(2);
        queue
            .try_push(first_turn, command_id(1), intent("first one"))
            .unwrap();
        queue
            .try_push(second_turn, command_id(2), intent("second one"))
            .unwrap();
        queue
            .try_push(first_turn, command_id(3), intent("first two"))
            .unwrap();

        let (command, turn, _) = queue.pop_front_for_turn(first_turn).unwrap().into_parts();
        assert_eq!(command, command_id(1));
        assert_eq!(turn, first_turn);
        assert_eq!(queue.len(), 2);

        let cleared = queue.clear_for_turn(first_turn);
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].command_id(), command_id(3));
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.pop_front_for_turn(second_turn).unwrap().command_id(),
            command_id(2)
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn steer_queue_rejects_duplicate_and_over_capacity_commands() {
        let mut queue = SteerQueue::new();
        let turn = turn_id(9);
        queue
            .try_push(turn, command_id(1), intent("first"))
            .unwrap();
        assert_eq!(
            queue.try_push(turn_id(10), command_id(1), intent("duplicate")),
            Err(SteerQueueError::DuplicateCommandId)
        );
        for value in 2..=u8::try_from(STEER_QUEUE_CAPACITY).unwrap() {
            queue
                .try_push(turn, command_id(value), intent("queued"))
                .unwrap();
        }
        assert_eq!(queue.len(), STEER_QUEUE_CAPACITY);
        assert_eq!(
            queue.try_push(turn, command_id(31), intent("overflow")),
            Err(SteerQueueError::Full)
        );
        let removed = queue.remove(command_id(4)).unwrap();
        assert_eq!(removed.turn_id(), turn);
        assert_eq!(queue.len(), STEER_QUEUE_CAPACITY - 1);

        queue
            .try_push(turn, command_id(32), intent("replacement"))
            .unwrap();
        assert_eq!(queue.len(), STEER_QUEUE_CAPACITY);
        assert_eq!(
            queue.pop_front_for_turn(turn).unwrap().command_id(),
            command_id(1)
        );
        assert_eq!(
            queue.pop_front_for_turn(turn).unwrap().command_id(),
            command_id(2)
        );
        assert_eq!(
            queue.pop_front_for_turn(turn).unwrap().command_id(),
            command_id(3)
        );
        assert_eq!(
            queue.pop_front_for_turn(turn).unwrap().command_id(),
            command_id(5)
        );

        let cleared = queue.clear_for_turn(turn);
        assert_eq!(
            cleared
                .iter()
                .map(QueuedSteer::command_id)
                .collect::<Vec<_>>(),
            vec![command_id(6), command_id(7), command_id(8), command_id(32)]
        );
        assert!(queue.is_empty());
        assert!(queue.clear_for_turn(turn).is_empty());
    }

    #[test]
    fn steer_queue_capacity_is_independent_per_turn() {
        let mut queue = SteerQueue::new();
        let first_turn = turn_id(11);
        let second_turn = turn_id(12);

        for value in 1..=u8::try_from(STEER_QUEUE_CAPACITY).unwrap() {
            queue
                .try_push(first_turn, command_id(value), intent("first turn"))
                .unwrap();
        }
        assert_eq!(queue.len(), STEER_QUEUE_CAPACITY);
        queue
            .try_push(second_turn, command_id(31), intent("second turn"))
            .unwrap();
        assert_eq!(queue.len(), STEER_QUEUE_CAPACITY + 1);
        assert_eq!(
            queue.try_push(first_turn, command_id(32), intent("first turn overflow"),),
            Err(SteerQueueError::Full)
        );
    }

    #[test]
    fn emergency_control_signal_current_handles_no_target_fresh_and_already_signaled() {
        let control = EmergencyControlHandle::new();
        // No bound target: the synchronous close handoff reports None and changes nothing.
        assert_eq!(
            control.signal_current(EmergencyControlSignal::PrepareForUnload),
            None
        );
        assert_eq!(
            control.observe(EmergencyControlTarget::Turn(turn_id(71))),
            None
        );

        // A fresh basis: the current-target signal is Accepted with the exact epoch, the
        // signal is observable and sticky, and the outcome's target matches the bound basis.
        let target = EmergencyControlTarget::Turn(turn_id(71));
        let observation = control.bind(target).unwrap();
        let (actual_target, outcome) = control
            .signal_current(EmergencyControlSignal::PrepareForUnload)
            .expect("a bound target is signaled");
        assert_eq!(actual_target, target);
        assert_eq!(
            outcome,
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert_eq!(
            control.observe(target).unwrap().signal(),
            Some(EmergencyControlSignal::PrepareForUnload)
        );

        // An earlier first-wins signal is never overwritten: the current-target handoff
        // reports the exact AlreadySignaled epoch and original signal.
        assert_eq!(
            control.signal_current(EmergencyControlSignal::PrepareForUnload),
            Some((
                target,
                EmergencyControlSignalOutcome::AlreadySignaled {
                    epoch: observation.epoch(),
                    signal: EmergencyControlSignal::PrepareForUnload,
                }
            ))
        );
        assert_eq!(
            control.signal_current(EmergencyControlSignal::Cancel),
            Some((
                target,
                EmergencyControlSignalOutcome::AlreadySignaled {
                    epoch: observation.epoch(),
                    signal: EmergencyControlSignal::PrepareForUnload,
                }
            ))
        );
        assert_eq!(
            control.observe(target).unwrap().signal(),
            Some(EmergencyControlSignal::PrepareForUnload)
        );

        // A fresh binding retires the prior signal: a later current-target handoff accepts
        // again on the new epoch.
        let rebind = control.bind(target).unwrap();
        assert_eq!(
            control.signal_current(EmergencyControlSignal::SecurityRevoked),
            Some((
                target,
                EmergencyControlSignalOutcome::Accepted {
                    epoch: rebind.epoch()
                }
            ))
        );
        // An earlier Cancel wins first-wins on this basis: the handoff keeps the exact
        // Cancel epoch/reason and never leaks the requested signal.
        assert_eq!(
            control.signal_current(EmergencyControlSignal::PrepareForUnload),
            Some((
                target,
                EmergencyControlSignalOutcome::AlreadySignaled {
                    epoch: rebind.epoch(),
                    signal: EmergencyControlSignal::SecurityRevoked,
                }
            ))
        );
        // The handle stays fully redacted and carries no payload: only the fixed target
        // fields and outcome enum are observable.
        assert_eq!(format!("{control:?}"), "EmergencyControlHandle { .. }");
        assert_eq!(format!("{outcome:?}"), "Accepted { epoch: 1 }");
    }

    #[test]
    fn emergency_control_signal_current_keeps_an_earlier_cancel_exact_and_redacts_everything() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Submit(command_id(72));
        let observation = control.bind(target).unwrap();
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        // The synchronous handoff on a Cancel-won basis reports the exact Cancel signal and
        // epoch — a close can never overwrite the first-wins reason, and the requested
        // PrepareForUnload is never observable.
        assert_eq!(
            control.signal_current(EmergencyControlSignal::PrepareForUnload),
            Some((
                target,
                EmergencyControlSignalOutcome::AlreadySignaled {
                    epoch: observation.epoch(),
                    signal: EmergencyControlSignal::Cancel,
                }
            ))
        );
        assert_eq!(
            control.observe(target).unwrap().signal(),
            Some(EmergencyControlSignal::Cancel)
        );
        // Retiring the basis closes the current-target handoff: no target, None.
        assert!(control.retire(&observation));
        assert_eq!(
            control.signal_current(EmergencyControlSignal::PrepareForUnload),
            None
        );
        // Neither the outcome nor the handle exposes any payload.
        assert_eq!(format!("{control:?}"), "EmergencyControlHandle { .. }");
    }

    #[test]
    fn emergency_control_is_sticky_first_wins_and_retires_by_observation() {
        let control = EmergencyControlHandle::new();
        let turn = turn_id(41);
        let target = EmergencyControlTarget::Turn(turn);
        let observation = control.bind(target).unwrap();
        assert_eq!(control.observe(target), Some(observation.clone()));
        assert!(control.is_current(&observation));
        assert!(control.is_unsignaled_current(&observation));
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::AlreadySignaled {
                epoch: observation.epoch(),
                signal: EmergencyControlSignal::Cancel,
            }
        );
        assert_eq!(
            control.observe(target).unwrap().signal(),
            Some(EmergencyControlSignal::Cancel)
        );

        let replacement = control
            .bind(EmergencyControlTarget::Submit(command_id(42)))
            .unwrap();
        assert!(!control.retire(&observation));
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::StaleTarget
        );
        assert!(control.retire(&replacement));
        assert!(control.observe(replacement.target()).is_none());
        assert!(!control.is_current(&replacement));
        assert!(!control.is_unsignaled_current(&replacement));
        assert!(format!("{control:?}").contains("EmergencyControlHandle { .. }"));
        assert_eq!(
            format!("{observation:?}"),
            "EmergencyControlObservation { .. }"
        );
    }

    #[test]
    fn emergency_control_reserves_tool_start_only_on_exact_unsignaled_current() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(61));
        let observation = control.bind(target).unwrap();
        let request = tool_request(1, "call_exact", 0);
        let gate = ToolStartGate::new(request.clone());

        // The exact current unsignaled basis reserves, and the permit starts the gate.
        let permit = control
            .reserve_tool_start(&observation, &gate, &request)
            .expect("the exact unsignaled current basis reserves");
        assert!(
            permit
                .expect("the reservation itself is not refused")
                .start()
                .is_ok()
        );
        // The exact entry is now started: a later reservation is a duplicate invariant.
        assert!(matches!(
            gate.reserve(&request),
            Err(ToolStartError::InvalidBinding)
        ));

        // A stale epoch never reserves: a re-bound basis retires the first observation, and
        // the reservation does not run on the old basis.
        let stale = control.bind(target).unwrap();
        assert!(
            control
                .reserve_tool_start(&observation, &gate, &request)
                .is_none()
        );
        assert!(
            control
                .reserve_tool_start(&stale, &gate, &request)
                .is_some()
        );
        // A different target on the same owner never reserves either.
        let other = tamper_target(stale.clone(), EmergencyControlTarget::Turn(turn_id(62)));
        assert!(
            control
                .reserve_tool_start(&other, &gate, &request)
                .is_none()
        );

        // After the signal wins, the reservation never runs: the signal is first-wins.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: stale.epoch()
            }
        );
        assert!(
            control
                .reserve_tool_start(&stale, &gate, &request)
                .is_none()
        );
        // The signal that won stays observable.
        assert_eq!(
            control.observe(target).unwrap().signal(),
            Some(EmergencyControlSignal::Cancel)
        );

        // Retiring the observation also closes the reservation basis.
        assert!(control.retire(&stale));
        assert!(
            control
                .reserve_tool_start(&stale, &gate, &request)
                .is_none()
        );
    }

    #[test]
    fn emergency_control_typed_reservation_linearizes_with_signal_on_one_mutex() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(63));
        let observation = control.bind(target).unwrap();
        let request = tool_request(2, "call_first_wins", 0);
        let gate = ToolStartGate::new(request.clone());

        // A reservation that linearized before the signal starts truthfully afterwards: the
        // permit holds the gate in Reserved and start succeeds even after the signal lands.
        let permit = control
            .reserve_tool_start(&observation, &gate, &request)
            .expect("the reservation linearized before the signal")
            .expect("the reservation itself is not refused");
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(permit.start().is_ok());
        // The started gate stays closed: the signal did not resurrect a second reservation.
        assert!(matches!(
            gate.reserve(&request),
            Err(ToolStartError::InvalidBinding)
        ));
    }

    #[test]
    fn emergency_control_typed_reservation_passes_gate_failures_through_under_the_mutex() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(64));
        let observation = control.bind(target).unwrap();
        let request = tool_request(3, "call_closed", 0);
        let gate = ToolStartGate::new(request.clone());

        // A foreign request fails the exact-binding invariant inside the reservation: the
        // gate error is the result while the emergency mutex still guards the basis.
        let foreign = tool_request(4, "call_foreign", 0);
        assert!(matches!(
            control.reserve_tool_start(&observation, &gate, &foreign),
            Some(Err(ToolStartError::InvalidBinding))
        ));

        // A gate explicitly closed before start reports CancelledBeforeStart.
        assert!(gate.cancel_before_start());
        assert!(matches!(
            control.reserve_tool_start(&observation, &gate, &request),
            Some(Err(ToolStartError::CancelledBeforeStart))
        ));

        // The signal still wins over any later reservation, gate errors included.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(
            control
                .reserve_tool_start(&observation, &gate, &request)
                .is_none()
        );
    }

    #[test]
    fn interaction_presentation_permit_is_first_wins_redacted_and_survives_a_later_signal() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(65));
        let observation = control.bind(target).unwrap();
        let request = tool_request(5, "call_present", 0);

        // Signal first: the reservation never runs and no permit exists.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(
            control
                .reserve_interaction_presentation(&observation, &request)
                .is_none()
        );
        // A stale epoch never reserves: a re-bound basis retires the first observation.
        let stale = control.bind(target).unwrap();
        assert!(
            control
                .reserve_interaction_presentation(&observation, &request)
                .is_none()
        );
        // A different target on the same owner never reserves either.
        let other = tamper_target(stale.clone(), EmergencyControlTarget::Turn(turn_id(66)));
        assert!(
            control
                .reserve_interaction_presentation(&other, &request)
                .is_none()
        );

        // A fresh binding retires the prior signal; permit first on this basis: the
        // reservation linearized before the signal, so the permit binds the exact owner +
        // target + epoch and the exact request capture, and its validation survives the
        // later signal (the signal closes only the emergency basis, never the
        // already-won presentation).
        let observation = control.bind(target).unwrap();
        let permit: InteractionPresentationPermit = control
            .reserve_interaction_presentation(&observation, &request)
            .expect("the presentation permit wins first-wins");
        assert!(permit.is_current(&control));
        assert!(permit.validates(&control, &request));
        // A foreign request capture never validates against the permit.
        let foreign_request = tool_request(6, "call_foreign_capture", 0);
        assert!(!permit.validates(&control, &foreign_request));
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(permit.is_current(&control));
        assert!(permit.validates(&control, &request));
        // The Debug is fully redacted and the permit carries no signal.
        assert_eq!(
            format!("{permit:?}"),
            "InteractionPresentationPermit { .. }"
        );
        assert!(
            control
                .reserve_interaction_presentation(&observation, &request)
                .is_none()
        );
        // A foreign handle never validates the permit: same target + epoch, foreign owner.
        let foreign = EmergencyControlHandle::new();
        let _ = foreign.bind(target).unwrap();
        assert!(!permit.is_current(&foreign));
        assert!(!permit.validates(&foreign, &request));

        // Retiring the basis makes the permit's validation fail: a completion carried on a
        // retired/rebound basis is stale.
        assert!(control.retire(&observation));
        assert!(!permit.is_current(&control));
        assert!(!permit.validates(&control, &request));
    }

    #[test]
    fn user_question_binding_permit_is_first_wins_redacted_and_authorizes_across_a_later_signal() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(67));
        let observation = control.bind(target).unwrap();
        let request = tool_request(7, "call_bind", 0);

        // Signal first: the binding authorization never exists.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(
            control
                .reserve_user_question_binding(&observation, &request)
                .is_none()
        );

        // A fresh binding retires the prior signal; permit first on this basis: the binding
        // is authorized even when the signal lands right after the reservation, and a
        // second reservation is refused.
        let observation = control.bind(target).unwrap();
        let permit: UserQuestionBindingPermit = control
            .reserve_user_question_binding(&observation, &request)
            .expect("the binding permit wins first-wins");
        assert_eq!(permit.target(), target);
        assert_eq!(permit.epoch(), observation.epoch());
        assert!(permit.request().is_exact_capture(&request));
        assert!(permit.validates(&control, &request));
        let foreign_request = tool_request(8, "call_bind_foreign", 0);
        assert!(!permit.validates(&control, &foreign_request));
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert_eq!(permit.target(), target);
        assert_eq!(permit.epoch(), observation.epoch());
        assert!(permit.validates(&control, &request));
        assert_eq!(format!("{permit:?}"), "UserQuestionBindingPermit { .. }");
        assert!(
            control
                .reserve_user_question_binding(&observation, &request)
                .is_none()
        );
    }

    #[test]
    fn unstarted_tool_settlement_permit_is_first_wins_redacted_and_authorizes_a_fixed_local_settlement()
     {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(68));
        let observation = control.bind(target).unwrap();
        let request = tool_request(9, "call_settle", 0);

        // Signal first: the unstarted settlement authorization never exists and the slot
        // settles the matching cancelled-before-start outcome instead.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(
            control
                .reserve_unstarted_tool_settlement(&observation, &request)
                .is_none()
        );
        // A stale epoch never reserves: a re-bound basis retires the first observation.
        let stale = control.bind(target).unwrap();
        assert!(
            control
                .reserve_unstarted_tool_settlement(&observation, &request)
                .is_none()
        );
        // A different target on the same owner never reserves either.
        let other = tamper_target(stale.clone(), EmergencyControlTarget::Turn(turn_id(69)));
        assert!(
            control
                .reserve_unstarted_tool_settlement(&other, &request)
                .is_none()
        );

        // A fresh binding retires the prior signal; permit first on this basis: the
        // settlement is authorized exactly once, even when the signal lands right after the
        // reservation, and the Debug is fully redacted with no callback or payload reachable.
        let observation = control.bind(target).unwrap();
        let permit: UnstartedToolSettlementPermit = control
            .reserve_unstarted_tool_settlement(&observation, &request)
            .expect("the unstarted settlement permit wins first-wins");
        assert_eq!(permit.target(), target);
        assert_eq!(permit.epoch(), observation.epoch());
        assert!(permit.request().is_exact_capture(&request));
        assert!(permit.validates(&control, &request));
        let foreign_request = tool_request(10, "call_settle_foreign", 0);
        assert!(!permit.validates(&control, &foreign_request));
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert_eq!(permit.target(), target);
        assert_eq!(permit.epoch(), observation.epoch());
        assert!(permit.validates(&control, &request));
        assert_eq!(
            format!("{permit:?}"),
            "UnstartedToolSettlementPermit { .. }"
        );
        assert!(
            control
                .reserve_unstarted_tool_settlement(&observation, &request)
                .is_none()
        );
    }

    #[test]
    fn interaction_resolution_permit_is_first_wins_redacted_and_authorizes_a_signal_after_reservation()
     {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(70));
        let observation = control.bind(target).unwrap();
        let request = tool_request(11, "call_resolution", 0);

        // Signal first: the host-resolution authorization never exists and the pending
        // interaction is owner-cancelled with the exact first-wins reason instead.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(
            control
                .reserve_interaction_resolution(&observation, &request)
                .is_none()
        );
        // A stale epoch never reserves: a re-bound basis retires the first observation.
        let stale = control.bind(target).unwrap();
        assert!(
            control
                .reserve_interaction_resolution(&observation, &request)
                .is_none()
        );
        // A different target on the same owner never reserves either.
        let other = tamper_target(
            stale.clone(),
            EmergencyControlTarget::Submit(command_id(71)),
        );
        assert!(
            control
                .reserve_interaction_resolution(&other, &request)
                .is_none()
        );

        // A fresh binding retires the prior signal; permit first on this basis: the
        // reservation authorizes exactly one host resolution (Allow/UserAnswer/Denied) even
        // when the signal lands right after, a second reservation is refused, and the Debug
        // is fully redacted with no payload beyond the exact request capture.
        let observation = control.bind(target).unwrap();
        let permit: InteractionResolutionPermit = control
            .reserve_interaction_resolution(&observation, &request)
            .expect("the resolution permit wins first-wins");
        assert_eq!(permit.target(), target);
        assert_eq!(permit.epoch(), observation.epoch());
        assert!(permit.request().is_exact_capture(&request));
        assert!(permit.validates(&control, &request));
        let foreign_request = tool_request(12, "call_resolution_foreign", 0);
        assert!(!permit.validates(&control, &foreign_request));
        // A foreign handle never validates the permit: same target + epoch, foreign owner.
        let foreign = EmergencyControlHandle::new();
        let _ = foreign.bind(target).unwrap();
        assert!(!permit.validates(&foreign, &request));
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(permit.validates(&control, &request));
        assert_eq!(format!("{permit:?}"), "InteractionResolutionPermit { .. }");
        assert!(
            control
                .reserve_interaction_resolution(&observation, &request)
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_control_wakes_only_the_current_bound_waiter() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(51));
        let observation = control.bind(target).unwrap();
        let waiter_control = control.clone();
        let waiter_observation = observation.clone();
        let waiter = tokio::spawn(async move {
            waiter_control.cancelled(&waiter_observation).await;
        });
        tokio::task::yield_now().await;
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("the bound emergency waiter wakes")
            .expect("the emergency waiter does not panic");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_control_migrate_preserves_the_signal_on_the_new_basis_and_wakes_the_new_waiter()
     {
        let control = EmergencyControlHandle::new();
        let old_target = EmergencyControlTarget::Submit(command_id(81));
        let new_target = EmergencyControlTarget::Turn(turn_id(82));
        let old_observation = control.bind(old_target).unwrap();

        // The old basis wins Cancel first-wins before the migration.
        assert_eq!(
            control.signal(old_target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: old_observation.epoch()
            }
        );

        // The exact atomic migration captures the old signal, retires the old basis, and
        // preserves the exact Cancel on the new basis at the checked-incremented epoch.
        let (new_observation, preserved) = control
            .migrate_target(&old_observation, new_target)
            .expect("the exact old basis migrates");
        assert_eq!(preserved, Some(EmergencyControlSignal::Cancel));
        assert_eq!(new_observation.target(), new_target);
        assert_eq!(new_observation.epoch(), old_observation.epoch() + 1);
        assert_eq!(
            new_observation.signal(),
            Some(EmergencyControlSignal::Cancel)
        );
        assert_eq!(
            control.observe(new_target).unwrap().signal(),
            Some(EmergencyControlSignal::Cancel)
        );
        assert_eq!(new_observation, control.observe(new_target).unwrap());

        // The old basis is fully retired: no observation, no signal, no waiter.
        assert!(control.observe(old_target).is_none());
        assert!(!control.is_current(&old_observation));
        assert_eq!(
            control.signal(old_target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::StaleTarget
        );

        // The preserved signal is first-wins on the new basis: a later signal is
        // AlreadySignaled with the exact preserved Cancel and the new epoch.
        assert_eq!(
            control.signal(new_target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::AlreadySignaled {
                epoch: new_observation.epoch(),
                signal: EmergencyControlSignal::Cancel,
            }
        );

        // The migrated basis carries an already-cancelled token: a new waiter on the exact
        // new target + epoch resolves immediately on the first poll — no spawn, no
        // yield/timeout guess.
        assert!(control.cancelled(&new_observation).now_or_never().is_some());

        // The handle and observations stay fully redacted.
        assert_eq!(format!("{control:?}"), "EmergencyControlHandle { .. }");
        assert_eq!(
            format!("{new_observation:?}"),
            "EmergencyControlObservation { .. }"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_control_migrate_without_a_signal_leaves_the_new_basis_unsignaled() {
        let control = EmergencyControlHandle::new();
        let old_target = EmergencyControlTarget::Submit(command_id(83));
        let new_target = EmergencyControlTarget::Turn(turn_id(84));
        let old_observation = control.bind(old_target).unwrap();

        // No signal won the old basis: the migration preserves `None` and the new basis is
        // unsignaled at the checked-incremented epoch.
        let (new_observation, preserved) = control
            .migrate_target(&old_observation, new_target)
            .expect("the exact old basis migrates");
        assert_eq!(preserved, None);
        assert_eq!(new_observation.signal(), None);
        assert_eq!(new_observation.epoch(), old_observation.epoch() + 1);
        assert_eq!(control.observe(new_target), Some(new_observation.clone()));

        // The new basis accepts a fresh signal first-wins: it was not already signaled.
        assert_eq!(
            control.signal(new_target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: new_observation.epoch()
            }
        );
        assert_eq!(
            control.observe(new_target).unwrap().signal(),
            Some(EmergencyControlSignal::SecurityRevoked)
        );
    }

    #[test]
    fn emergency_control_migrate_rejects_stale_observations_without_mutation() {
        let control = EmergencyControlHandle::new();
        let old_target = EmergencyControlTarget::Submit(command_id(85));
        let new_target = EmergencyControlTarget::Turn(turn_id(86));

        // A re-bind retires the first observation: migrating the stale observation is
        // rejected and nothing is mutated.
        let first = control.bind(old_target).unwrap();
        let second = control.bind(old_target).unwrap();
        assert_eq!(
            control.migrate_target(&first, new_target),
            Err(EmergencyControlMigrationError::StaleObservation)
        );
        assert_eq!(control.observe(old_target), Some(second.clone()));
        assert!(control.is_current(&second));
        assert!(control.observe(new_target).is_none());

        // A tampered target (never bound) is rejected without mutation too.
        let tampered = tamper_target(second.clone(), EmergencyControlTarget::Turn(turn_id(88)));
        assert_eq!(
            control.migrate_target(&tampered, new_target),
            Err(EmergencyControlMigrationError::StaleObservation)
        );
        assert_eq!(control.observe(old_target), Some(second.clone()));
        assert!(control.observe(new_target).is_none());

        // A signal on the exact current basis still wins untouched after the rejected
        // migrations, and the exact basis still retires/captures atomically.
        assert_eq!(
            control.signal(old_target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: second.epoch()
            }
        );
        assert_eq!(
            control.retire_and_capture_signal(&second),
            Some(Some(EmergencyControlSignal::Cancel))
        );
        assert!(control.observe(old_target).is_none());
        assert!(control.retire_and_capture_signal(&second).is_none());
    }

    #[test]
    fn emergency_control_retire_and_capture_signal_is_atomic_and_stale_aware() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Submit(command_id(89));
        let observation = control.bind(target).unwrap();

        // An unsignaled exact basis retires and captures `None`.
        assert_eq!(control.retire_and_capture_signal(&observation), Some(None));
        assert!(control.observe(target).is_none());

        // A stale observation never mutates the new basis.
        let rebind = control.bind(target).unwrap();
        assert_eq!(control.retire_and_capture_signal(&observation), None);
        assert_eq!(control.observe(target), Some(rebind.clone()));

        // A signaled exact basis retires and captures the exact first-wins signal.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted {
                epoch: rebind.epoch()
            }
        );
        assert_eq!(
            control.retire_and_capture_signal(&rebind),
            Some(Some(EmergencyControlSignal::SecurityRevoked))
        );
        assert!(control.observe(target).is_none());
    }

    #[test]
    fn cross_handle_observations_with_the_same_target_and_epoch_are_foreign_and_rejected() {
        let owner = EmergencyControlHandle::new();
        let foreign = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(91));
        let owner_observation = owner.bind(target).unwrap();
        let foreign_observation = foreign.bind(target).unwrap();
        // Identical target and epoch, but a foreign owner Arc: the observations never
        // compare equal (Arc::ptr_eq) and every owner-bound validation rejects the foreign
        // observation in normal release code — never debug_assert-only.
        assert_eq!(owner_observation.target(), foreign_observation.target());
        assert_eq!(owner_observation.epoch(), foreign_observation.epoch());
        assert_ne!(owner_observation, foreign_observation);
        assert!(!owner.is_current(&foreign_observation));
        assert!(!owner.is_unsignaled_current(&foreign_observation));
        assert!(!owner.retire(&foreign_observation));
        assert_eq!(owner.retire_and_capture_signal(&foreign_observation), None);
        assert_eq!(
            owner.migrate_target(
                &foreign_observation,
                EmergencyControlTarget::Submit(command_id(92)),
            ),
            Err(EmergencyControlMigrationError::StaleObservation)
        );

        // A foreign observation cannot reserve any first-wins slot on the owner handle: no
        // start, presentation, binding, unstarted settlement, or host-resolution permit.
        let request = tool_request(13, "call_foreign_slot", 0);
        let gate = ToolStartGate::new(request.clone());
        assert!(
            owner
                .reserve_tool_start(&foreign_observation, &gate, &request)
                .is_none()
        );
        assert!(
            owner
                .reserve_interaction_presentation(&foreign_observation, &request)
                .is_none()
        );
        assert!(
            owner
                .reserve_user_question_binding(&foreign_observation, &request)
                .is_none()
        );
        assert!(
            owner
                .reserve_unstarted_tool_settlement(&foreign_observation, &request)
                .is_none()
        );
        assert!(
            owner
                .reserve_interaction_resolution(&foreign_observation, &request)
                .is_none()
        );

        // The rejection never disturbed either owner's own basis: the foreign handle still
        // reserves its own slots, and the owner's own observation stays current.
        assert!(
            foreign
                .reserve_interaction_presentation(&foreign_observation, &request)
                .is_some()
        );
        assert!(owner.is_current(&owner_observation));
        assert!(foreign.is_current(&foreign_observation));

        // Permits bind the exact owner Arc: a permit reserved by the owner is rejected by
        // the foreign handle's validation, and a foreign request capture is rejected by the
        // owner's own validation.
        let owner_permit = owner
            .reserve_interaction_presentation(&owner_observation, &request)
            .expect("the owner's own basis reserves");
        assert!(owner_permit.is_current(&owner));
        assert!(!owner_permit.is_current(&foreign));
        assert!(owner_permit.validates(&owner, &request));
        assert!(!owner_permit.validates(&foreign, &request));
        let foreign_request = tool_request(14, "call_foreign_capture", 0);
        assert!(!owner_permit.validates(&owner, &foreign_request));
        let foreign_permit = foreign
            .reserve_interaction_presentation(&foreign_observation, &request)
            .expect("the foreign handle's own basis reserves");
        assert!(!foreign_permit.is_current(&owner));
        assert!(foreign_permit.is_current(&foreign));
        assert!(!foreign_permit.validates(&owner, &request));
        assert!(foreign_permit.validates(&foreign, &request));

        // A foreign cancelled wait returns immediately stale: it never waits on the owner's
        // token and never implies the foreign observation is current on the owner handle.
        assert!(owner.is_unsignaled_current(&owner_observation));
        assert!(
            !owner
                .cancelled(&foreign_observation)
                .now_or_never()
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cross_handle_cancelled_wait_returns_immediately_stale() {
        let owner = EmergencyControlHandle::new();
        let foreign = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(93));
        let owner_observation = owner.bind(target).unwrap();
        let foreign_observation = foreign.bind(target).unwrap();
        // The owner's basis is unsignaled and its own wait is pending, but a foreign
        // observation (same target + epoch, foreign owner) never waits on the owner's
        // token: the wait resolves immediately, stale, without any scheduling guess.
        assert!(owner.is_unsignaled_current(&owner_observation));
        assert!(
            owner
                .cancelled(&foreign_observation)
                .now_or_never()
                .is_some()
        );
        // The foreign handle's own waiter is still pending on its own unsignaled basis.
        assert!(
            foreign
                .cancelled(&foreign_observation)
                .now_or_never()
                .is_none()
        );
        assert_eq!(
            foreign.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: foreign_observation.epoch()
            }
        );
        // Only the foreign handle's own waiter resolves; the owner's basis is untouched.
        assert!(
            foreign
                .cancelled(&foreign_observation)
                .now_or_never()
                .is_some()
        );
        assert!(owner.is_unsignaled_current(&owner_observation));
    }
}
