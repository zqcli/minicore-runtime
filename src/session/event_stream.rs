use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};

use super::event::SessionEvent;
use super::snapshot::SessionSnapshot;
use super::state::SessionStatus;
use crate::error::SessionError;

pub(crate) const MAX_EVENT_CAPACITY: usize = 4_096;

const _: () = {
    let _ = std::mem::size_of::<SessionObservation>();
    let _ = std::mem::size_of::<SessionEventStream>();
    let _ = SessionObservation::new;
    let _ = SessionObservation::snapshot;
    let _ = SessionObservation::publish_snapshot;
    let _ = SessionObservation::publish;
    let _ = SessionObservation::close;
    let _ = SessionObservation::subscribe;
    let _ = SessionEventStream::snapshot;
    let _ = SessionEventStream::recv;
};

struct ObservationInner {
    gate: Mutex<ObservationState>,
    snapshot_tx: watch::Sender<SessionSnapshot>,
}

struct ObservationState {
    closed: bool,
    owner_count: usize,
    event_tx: Option<broadcast::Sender<SessionEvent>>,
}

pub(crate) struct SessionObservation {
    inner: Arc<ObservationInner>,
}

impl SessionObservation {
    pub(crate) fn new(
        initial: SessionSnapshot,
        event_capacity: usize,
    ) -> Result<Self, SessionError> {
        if event_capacity == 0 || event_capacity > MAX_EVENT_CAPACITY {
            return Err(SessionError::InvalidInput);
        }
        initial.validate().map_err(|_| SessionError::InvalidInput)?;
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

    pub(crate) fn snapshot(&self) -> SessionSnapshot {
        self.inner.snapshot_tx.borrow().clone()
    }

    pub(crate) fn publish_snapshot(&self, snapshot: SessionSnapshot) -> Result<(), SessionError> {
        self.publish(snapshot, None)
    }

    pub(crate) fn publish(
        &self,
        snapshot: SessionSnapshot,
        event: Option<SessionEvent>,
    ) -> Result<(), SessionError> {
        snapshot
            .validate()
            .map_err(|_| SessionError::InvalidInput)?;
        let state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(SessionError::Closing);
        }
        if event.as_ref().is_some_and(|event| {
            matches!(
                event,
                SessionEvent::Snapshot(_) | SessionEvent::ResyncRequired | SessionEvent::Closed
            )
        }) {
            return Err(SessionError::InvalidInput);
        }
        self.inner.snapshot_tx.send_replace(snapshot);
        if let Some(event) = event {
            if let Some(sender) = state.event_tx.as_ref() {
                let _ = sender.send(event);
            }
        }
        Ok(())
    }

    pub(crate) fn close(&self, snapshot: SessionSnapshot) -> Result<(), SessionError> {
        if snapshot.status() != SessionStatus::Closing {
            return Err(SessionError::InvalidInput);
        }
        snapshot
            .validate()
            .map_err(|_| SessionError::InvalidInput)?;
        let mut state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(SessionError::Closing);
        }
        self.inner.snapshot_tx.send_replace(snapshot);
        if let Some(sender) = state.event_tx.as_ref() {
            let _ = sender.send(SessionEvent::Closed);
        }
        state.closed = true;
        Ok(())
    }

    pub(crate) fn subscribe(&self) -> Result<SessionEventStream, SessionError> {
        let state = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(SessionError::Closing);
        }
        let events = state
            .event_tx
            .as_ref()
            .ok_or(SessionError::Closing)?
            .subscribe();
        let initial = self.inner.snapshot_tx.borrow().clone();
        Ok(SessionEventStream {
            observation: Arc::clone(&self.inner),
            initial: Some(initial),
            pending_resync_snapshot: None,
            pending_resync_close: false,
            events,
            closed: false,
        })
    }
}

impl Clone for SessionObservation {
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

impl Drop for SessionObservation {
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

pub struct SessionEventStream {
    observation: Arc<ObservationInner>,
    initial: Option<SessionSnapshot>,
    pending_resync_snapshot: Option<SessionSnapshot>,
    pending_resync_close: bool,
    events: broadcast::Receiver<SessionEvent>,
    closed: bool,
}

impl SessionEventStream {
    pub fn snapshot(&self) -> SessionSnapshot {
        self.observation.snapshot_tx.borrow().clone()
    }

    pub async fn recv(&mut self) -> Option<SessionEvent> {
        if self.closed {
            return None;
        }
        if let Some(snapshot) = self.initial.take() {
            return Some(SessionEvent::Snapshot(snapshot));
        }
        if let Some(snapshot) = self.pending_resync_snapshot.take() {
            return Some(SessionEvent::Snapshot(snapshot));
        }
        if self.pending_resync_close {
            self.pending_resync_close = false;
            self.closed = true;
            return Some(SessionEvent::Closed);
        }
        match self.events.recv().await {
            Ok(SessionEvent::Closed) => {
                self.closed = true;
                Some(SessionEvent::Closed)
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
                Some(SessionEvent::ResyncRequired)
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.closed = true;
                Some(SessionEvent::Closed)
            }
        }
    }
}

impl fmt::Debug for SessionEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionEventStream { .. }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ids::{SessionId, TurnId};
    use crate::model::Usage;
    use crate::session::snapshot::{SessionSnapshot, SnapshotHistory, TurnSummary};
    use crate::session::state::SessionStatus;

    fn snapshot(session_id: SessionId, sequence: u64, status: SessionStatus) -> SessionSnapshot {
        SessionSnapshot::new(
            session_id,
            status,
            status.turn_id().map(TurnSummary::new),
            None,
            Usage::default(),
            SnapshotHistory::new(None, None),
            sequence,
        )
        .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observation_rejects_zero_capacity_and_delivers_first_snapshot_then_event() {
        let session_id = SessionId::new().unwrap();
        let initial = snapshot(session_id, 0, SessionStatus::Idle);
        assert!(matches!(
            SessionObservation::new(initial.clone(), 0),
            Err(SessionError::InvalidInput)
        ));
        assert!(matches!(
            SessionObservation::new(initial.clone(), MAX_EVENT_CAPACITY + 1),
            Err(SessionError::InvalidInput)
        ));
        assert!(matches!(
            SessionObservation::new(initial.clone(), usize::MAX),
            Err(SessionError::InvalidInput)
        ));
        let exact_capacity = SessionObservation::new(initial.clone(), MAX_EVENT_CAPACITY).unwrap();
        drop(exact_capacity);
        let observation = SessionObservation::new(initial.clone(), 4).unwrap();
        for event in [
            SessionEvent::Snapshot(initial.clone()),
            SessionEvent::ResyncRequired,
            SessionEvent::Closed,
        ] {
            assert!(matches!(
                observation.publish(initial.clone(), Some(event)),
                Err(SessionError::InvalidInput)
            ));
        }
        let mut stream = observation.subscribe().unwrap();
        assert_eq!(stream.recv().await, Some(SessionEvent::Snapshot(initial)));
        let turn_id = TurnId::new().unwrap();
        let running = snapshot(session_id, 1, SessionStatus::Running { turn_id });
        observation
            .publish(running.clone(), Some(SessionEvent::TurnStarted { turn_id }))
            .unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        assert_eq!(stream.snapshot(), running);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscribe_after_publish_captures_latest_snapshot_without_replaying_event() {
        let session_id = SessionId::new().unwrap();
        let initial = snapshot(session_id, 0, SessionStatus::Idle);
        let observation = SessionObservation::new(initial, 4).unwrap();
        let turn_id = TurnId::new().unwrap();
        let running = snapshot(session_id, 1, SessionStatus::Running { turn_id });
        observation
            .publish(running.clone(), Some(SessionEvent::TurnStarted { turn_id }))
            .unwrap();
        let mut stream = observation.subscribe().unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::Snapshot(running.clone()))
        );
        assert_eq!(observation.snapshot(), running);
        drop(observation);
        assert_eq!(stream.recv().await, Some(SessionEvent::Closed));
        assert_eq!(stream.recv().await, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lag_returns_resync_then_continues_and_snapshot_recovers_latest_state() {
        let session_id = SessionId::new().unwrap();
        let initial = snapshot(session_id, 0, SessionStatus::Idle);
        let observation = SessionObservation::new(initial, 2).unwrap();
        let mut stream = observation.subscribe().unwrap();
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::Snapshot(_))
        ));
        let first_turn = TurnId::new().unwrap();
        let first = snapshot(
            session_id,
            1,
            SessionStatus::Running {
                turn_id: first_turn,
            },
        );
        observation
            .publish(
                first,
                Some(SessionEvent::TurnStarted {
                    turn_id: first_turn,
                }),
            )
            .unwrap();
        let second = snapshot(
            session_id,
            2,
            SessionStatus::Running {
                turn_id: first_turn,
            },
        );
        observation
            .publish(
                second.clone(),
                Some(SessionEvent::TextDelta {
                    turn_id: first_turn,
                    delta: "e2".to_owned(),
                }),
            )
            .unwrap();
        let third = snapshot(
            session_id,
            3,
            SessionStatus::Running {
                turn_id: first_turn,
            },
        );
        observation
            .publish(
                third.clone(),
                Some(SessionEvent::ReasoningDelta {
                    turn_id: first_turn,
                    delta: "e3".to_owned(),
                }),
            )
            .unwrap();
        assert_eq!(stream.recv().await, Some(SessionEvent::ResyncRequired));
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::Snapshot(third.clone()))
        );
        assert_eq!(stream.snapshot(), third);
        let fourth = snapshot(
            session_id,
            4,
            SessionStatus::Running {
                turn_id: first_turn,
            },
        );
        observation
            .publish(
                fourth.clone(),
                Some(SessionEvent::TextDelta {
                    turn_id: first_turn,
                    delta: "e4".to_owned(),
                }),
            )
            .unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TextDelta {
                turn_id: first_turn,
                delta: "e4".to_owned(),
            })
        );
        assert_eq!(stream.snapshot(), fourth);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_delivers_closed_once_then_eof_and_rejects_future_publish() {
        let session_id = SessionId::new().unwrap();
        let initial = snapshot(session_id, 0, SessionStatus::Idle);
        let observation = SessionObservation::new(initial, 4).unwrap();
        assert!(matches!(
            observation.close(snapshot(session_id, 1, SessionStatus::Idle)),
            Err(SessionError::InvalidInput)
        ));
        let mut stream = observation.subscribe().unwrap();
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::Snapshot(_))
        ));
        let closing = snapshot(session_id, 1, SessionStatus::Closing);
        observation.close(closing.clone()).unwrap();
        assert_eq!(stream.snapshot(), closing);
        assert_eq!(stream.recv().await, Some(SessionEvent::Closed));
        assert_eq!(stream.recv().await, None);
        assert_eq!(
            observation.close(closing.clone()),
            Err(SessionError::Closing)
        );
        assert_eq!(
            observation.publish_snapshot(closing),
            Err(SessionError::Closing)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_owner_closes_stream_without_a_closed_event() {
        let session_id = SessionId::new().unwrap();
        let initial = snapshot(session_id, 0, SessionStatus::Idle);
        let observation = SessionObservation::new(initial, 4).unwrap();
        let mut stream = observation.subscribe().unwrap();
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::Snapshot(_))
        ));
        drop(observation);
        assert_eq!(stream.recv().await, Some(SessionEvent::Closed));
        assert_eq!(stream.recv().await, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_subscribers_each_receive_their_own_first_snapshot_and_event() {
        let session_id = SessionId::new().unwrap();
        let initial = snapshot(session_id, 0, SessionStatus::Idle);
        let observation = SessionObservation::new(initial.clone(), 4).unwrap();
        let mut first = observation.subscribe().unwrap();
        let mut second = observation.subscribe().unwrap();
        assert_eq!(
            first.recv().await,
            Some(SessionEvent::Snapshot(initial.clone()))
        );
        assert_eq!(second.recv().await, Some(SessionEvent::Snapshot(initial)));
        let turn_id = TurnId::new().unwrap();
        let running = snapshot(session_id, 1, SessionStatus::Running { turn_id });
        observation
            .publish(running, Some(SessionEvent::TurnStarted { turn_id }))
            .unwrap();
        assert_eq!(
            first.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        assert_eq!(
            second.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
    }
}
