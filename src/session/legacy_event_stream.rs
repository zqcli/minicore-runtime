// P4-B/P5 deletion target: remove with broadcast observation migration.

use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};

use super::legacy_event::LegacySessionEvent;
use super::legacy_snapshot::LegacySessionSnapshot;
use super::legacy_state::LegacySessionStatus;
use crate::error::LegacySessionError;

pub(crate) const MAX_EVENT_CAPACITY: usize = 4_096;

const _: () = {
    let _ = std::mem::size_of::<LegacySessionObservation>();
    let _ = std::mem::size_of::<LegacySessionEventStream>();
    let _ = LegacySessionObservation::new;
    let _ = LegacySessionObservation::snapshot;
    let _ = LegacySessionObservation::publish_snapshot;
    let _ = LegacySessionObservation::publish;
    let _ = LegacySessionObservation::close;
    let _ = LegacySessionObservation::subscribe;
    let _ = LegacySessionEventStream::snapshot;
    let _ = LegacySessionEventStream::recv;
};

struct ObservationInner {
    gate: Mutex<ObservationState>,
    snapshot_tx: watch::Sender<LegacySessionSnapshot>,
}

struct ObservationState {
    closed: bool,
    owner_count: usize,
    event_tx: Option<broadcast::Sender<LegacySessionEvent>>,
}

pub(crate) struct LegacySessionObservation {
    inner: Arc<ObservationInner>,
}

impl LegacySessionObservation {
    pub(crate) fn new(
        initial: LegacySessionSnapshot,
        event_capacity: usize,
    ) -> Result<Self, LegacySessionError> {
        if event_capacity == 0 || event_capacity > MAX_EVENT_CAPACITY {
            return Err(LegacySessionError::InvalidInput);
        }
        initial
            .validate()
            .map_err(|_| LegacySessionError::InvalidInput)?;
        let (snapshot_tx, _) = watch::channel(initial);
        let (event_tx, _) = broadcast::channel(event_capacity);
        Ok(Self {
            inner: Arc::new(ObservationInner {
                gate: Mutex::new(ObservationState {
                    closed: false,
                    owner_count: 1,
                    event_tx: Some(event_tx),
                }),
                snapshot_tx,
            }),
        })
    }

    pub(crate) fn snapshot(&self) -> LegacySessionSnapshot {
        self.inner.snapshot_tx.borrow().clone()
    }

    pub(crate) fn publish_snapshot(
        &self,
        snapshot: LegacySessionSnapshot,
    ) -> Result<(), LegacySessionError> {
        self.publish(snapshot, None)
    }

    pub(crate) fn publish(
        &self,
        snapshot: LegacySessionSnapshot,
        event: Option<LegacySessionEvent>,
    ) -> Result<(), LegacySessionError> {
        snapshot
            .validate()
            .map_err(|_| LegacySessionError::InvalidInput)?;
        let state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(LegacySessionError::Closing);
        }
        if event.as_ref().is_some_and(|event| {
            matches!(
                event,
                LegacySessionEvent::Snapshot(_)
                    | LegacySessionEvent::ResyncRequired
                    | LegacySessionEvent::Closed
            )
        }) {
            return Err(LegacySessionError::InvalidInput);
        }
        self.inner.snapshot_tx.send_replace(snapshot);
        if let Some(event) = event {
            if let Some(sender) = state.event_tx.as_ref() {
                let _ = sender.send(event);
            }
        }
        Ok(())
    }

    pub(crate) fn close(&self, snapshot: LegacySessionSnapshot) -> Result<(), LegacySessionError> {
        if snapshot.status() != LegacySessionStatus::Closing {
            return Err(LegacySessionError::InvalidInput);
        }
        snapshot
            .validate()
            .map_err(|_| LegacySessionError::InvalidInput)?;
        let mut state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(LegacySessionError::Closing);
        }
        self.inner.snapshot_tx.send_replace(snapshot);
        if let Some(sender) = state.event_tx.as_ref() {
            let _ = sender.send(LegacySessionEvent::Closed);
        }
        state.closed = true;
        Ok(())
    }

    pub(crate) fn subscribe(&self) -> Result<LegacySessionEventStream, LegacySessionError> {
        let state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(LegacySessionError::Closing);
        }
        let events = state
            .event_tx
            .as_ref()
            .ok_or(LegacySessionError::Closing)?
            .subscribe();
        let initial = self.inner.snapshot_tx.borrow().clone();
        Ok(LegacySessionEventStream {
            observation: Arc::clone(&self.inner),
            initial: Some(initial),
            pending_resync_snapshot: None,
            pending_resync_close: false,
            events,
            closed: false,
        })
    }
}

impl Clone for LegacySessionObservation {
    fn clone(&self) -> Self {
        let mut state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.owner_count = state.owner_count.saturating_add(1);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for LegacySessionObservation {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.owner_count = state.owner_count.saturating_sub(1);
        if state.owner_count == 0 {
            state.closed = true;
            state.event_tx.take();
        }
    }
}

pub(crate) struct LegacySessionEventStream {
    observation: Arc<ObservationInner>,
    initial: Option<LegacySessionSnapshot>,
    pending_resync_snapshot: Option<LegacySessionSnapshot>,
    pending_resync_close: bool,
    events: broadcast::Receiver<LegacySessionEvent>,
    closed: bool,
}

impl LegacySessionEventStream {
    pub(crate) fn snapshot(&self) -> LegacySessionSnapshot {
        self.observation.snapshot_tx.borrow().clone()
    }

    pub(crate) async fn recv(&mut self) -> Option<LegacySessionEvent> {
        if self.closed {
            return None;
        }
        if let Some(snapshot) = self.initial.take() {
            return Some(LegacySessionEvent::Snapshot(snapshot));
        }
        if let Some(snapshot) = self.pending_resync_snapshot.take() {
            return Some(LegacySessionEvent::Snapshot(snapshot));
        }
        if self.pending_resync_close {
            self.pending_resync_close = false;
            self.closed = true;
            return Some(LegacySessionEvent::Closed);
        }
        match self.events.recv().await {
            Ok(LegacySessionEvent::Closed) => {
                self.closed = true;
                Some(LegacySessionEvent::Closed)
            }
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let (events, snapshot, closed) = {
                    let state = self
                        .observation
                        .gate
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let events = state.event_tx.as_ref().map(|sender| sender.subscribe());
                    let snapshot = self.observation.snapshot_tx.borrow().clone();
                    (events, snapshot, state.closed)
                };
                if let Some(events) = events {
                    self.events = events;
                }
                self.pending_resync_snapshot = Some(snapshot);
                self.pending_resync_close = closed;
                Some(LegacySessionEvent::ResyncRequired)
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.closed = true;
                Some(LegacySessionEvent::Closed)
            }
        }
    }
}

impl fmt::Debug for LegacySessionEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LegacySessionEventStream { .. }")
    }
}

#[cfg(test)]
mod tests;
