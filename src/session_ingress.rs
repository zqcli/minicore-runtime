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

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct EmergencyControlObservation {
    target: EmergencyControlTarget,
    epoch: u64,
    signal: Option<EmergencyControlSignal>,
}

impl std::fmt::Debug for EmergencyControlObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EmergencyControlObservation { .. }")
    }
}

impl EmergencyControlObservation {
    pub(crate) const fn target(self) -> EmergencyControlTarget {
        self.target
    }

    pub(crate) const fn epoch(self) -> u64 {
        self.epoch
    }

    pub(crate) const fn signal(self) -> Option<EmergencyControlSignal> {
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
    StaleTarget,
}

impl EmergencyControlSignalOutcome {
    pub(crate) const fn epoch(self) -> Option<u64> {
        match self {
            Self::Accepted { epoch } | Self::AlreadySignaled { epoch, .. } => Some(epoch),
            Self::StaleTarget => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmergencyControlBindError {
    EpochExhausted,
}

struct EmergencyControlState {
    target: Option<EmergencyControlTarget>,
    epoch: u64,
    signal: Option<EmergencyControlSignal>,
    cancellation: CancellationToken,
}

/// A process-local sticky emergency signal owned by one Session control plane.
///
/// Binding a new target retires any prior signal.  A signal is first-wins for the bound target;
/// stale targets cannot affect the current target or its cancellation wakeup.
#[derive(Clone)]
pub(crate) struct EmergencyControlHandle {
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
            state: Arc::new(Mutex::new(EmergencyControlState {
                target: None,
                epoch: 0,
                signal: None,
                cancellation: CancellationToken::new(),
            })),
        }
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
        state.cancellation = CancellationToken::new();
        Ok(EmergencyControlObservation {
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
            target,
            epoch: state.epoch,
            signal: state.signal,
        })
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
        if state.target != Some(target) {
            return EmergencyControlSignalOutcome::StaleTarget;
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

    pub(crate) fn retire(&self, observation: EmergencyControlObservation) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if state.target != Some(observation.target) || state.epoch != observation.epoch {
            return false;
        }
        state.cancellation.cancel();
        state.target = None;
        state.signal = None;
        state.cancellation = CancellationToken::new();
        true
    }

    pub(crate) fn is_current(&self, target: EmergencyControlTarget, epoch: u64) -> bool {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        state.target == Some(target) && state.epoch == epoch
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
        target: EmergencyControlTarget,
        epoch: u64,
        gate: &ToolStartGate,
        request: &ToolExecutionRequest,
    ) -> Option<Result<ToolStartPermit, ToolStartError>> {
        let state = self
            .state
            .lock()
            .expect("emergency control state is not poisoned");
        if state.target != Some(target) || state.epoch != epoch || state.signal.is_some() {
            return None;
        }
        Some(gate.reserve(request))
    }

    pub(crate) async fn cancelled(&self, target: EmergencyControlTarget, epoch: u64) {
        let cancellation = {
            let state = self
                .state
                .lock()
                .expect("emergency control state is not poisoned");
            if state.target != Some(target) || state.epoch != epoch {
                return;
            }
            state.cancellation.clone()
        };
        cancellation.cancelled().await;
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
        EmergencyControlHandle, EmergencyControlSignal, EmergencyControlSignalOutcome,
        EmergencyControlTarget, FOLLOW_UP_QUEUE_CAPACITY, FollowUpQueue, FollowUpQueueError,
        QueuedSteer, STEER_QUEUE_CAPACITY, SteerQueue, SteerQueueError,
    };

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
    fn emergency_control_is_sticky_first_wins_and_retires_by_observation() {
        let control = EmergencyControlHandle::new();
        let turn = turn_id(41);
        let target = EmergencyControlTarget::Turn(turn);
        let observation = control.bind(target).unwrap();
        assert_eq!(control.observe(target), Some(observation));
        assert!(control.is_current(target, observation.epoch()));
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
        assert!(!control.retire(observation));
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::StaleTarget
        );
        assert!(control.retire(replacement));
        assert!(control.observe(replacement.target()).is_none());
        assert!(!control.is_current(replacement.target(), replacement.epoch()));
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
            .reserve_tool_start(target, observation.epoch(), &gate, &request)
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

        // A stale epoch never reserves: the reservation does not run on an old basis.
        assert!(
            control
                .reserve_tool_start(target, observation.epoch() + 1, &gate, &request)
                .is_none()
        );
        // A different target never reserves.
        assert!(
            control
                .reserve_tool_start(
                    EmergencyControlTarget::Turn(turn_id(62)),
                    observation.epoch(),
                    &gate,
                    &request,
                )
                .is_none()
        );

        // After the signal wins, the reservation never runs: the signal is first-wins.
        assert_eq!(
            control.signal(target, EmergencyControlSignal::Cancel),
            EmergencyControlSignalOutcome::Accepted {
                epoch: observation.epoch()
            }
        );
        assert!(
            control
                .reserve_tool_start(target, observation.epoch(), &gate, &request)
                .is_none()
        );
        // The signal that won stays observable.
        assert_eq!(
            control.observe(target).unwrap().signal(),
            Some(EmergencyControlSignal::Cancel)
        );

        // Retiring the observation also closes the reservation basis.
        assert!(control.retire(observation));
        assert!(
            control
                .reserve_tool_start(target, observation.epoch(), &gate, &request)
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
            .reserve_tool_start(target, observation.epoch(), &gate, &request)
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
            control.reserve_tool_start(target, observation.epoch(), &gate, &foreign),
            Some(Err(ToolStartError::InvalidBinding))
        ));

        // A gate explicitly closed before start reports CancelledBeforeStart.
        assert!(gate.cancel_before_start());
        assert!(matches!(
            control.reserve_tool_start(target, observation.epoch(), &gate, &request),
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
                .reserve_tool_start(target, observation.epoch(), &gate, &request)
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_control_wakes_only_the_current_bound_waiter() {
        let control = EmergencyControlHandle::new();
        let target = EmergencyControlTarget::Turn(turn_id(51));
        let observation = control.bind(target).unwrap();
        let waiter_control = control.clone();
        let waiter = tokio::spawn(async move {
            waiter_control.cancelled(target, observation.epoch()).await;
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
}
