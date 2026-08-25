use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::ids::{SessionId, SessionInstanceId};

use super::event::{SessionEvent, SessionEventEnvelope};

const MAX_EVENT_CAPACITY: usize = 4_096;

/// The single-consumer, bounded, best-effort stream for live Session events.
///
/// Dropping `SessionEventStream` has no execution effect; it only closes the
/// receiver and allows the actor to continue without a live consumer.
#[must_use = "SessionEventStream must be consumed or intentionally dropped"]
pub struct SessionEventStream {
    receiver: mpsc::Receiver<SessionEventEnvelope>,
}

pub(crate) struct InternalEventSink {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    sender: mpsc::Sender<SessionEventEnvelope>,
    dropped: u64,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum InternalEventSinkError {
    #[error("event stream capacity is invalid")]
    InvalidCapacity,
    #[error("event stream receiver is closed")]
    Closed,
}

impl SessionEventStream {
    pub async fn recv(&mut self) -> Option<SessionEventEnvelope> {
        self.receiver.recv().await
    }

    pub fn try_recv(&mut self) -> Result<SessionEventEnvelope, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Stream for SessionEventStream {
    type Item = SessionEventEnvelope;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(context)
    }
}

impl fmt::Debug for SessionEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionEventStream { .. }")
    }
}

impl InternalEventSink {
    pub(crate) fn channel(
        session_id: SessionId,
        instance_id: SessionInstanceId,
        capacity: usize,
    ) -> Result<(Self, SessionEventStream), InternalEventSinkError> {
        if !(1..=MAX_EVENT_CAPACITY).contains(&capacity) {
            return Err(InternalEventSinkError::InvalidCapacity);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                session_id,
                instance_id,
                sender,
                dropped: 0,
            },
            SessionEventStream { receiver },
        ))
    }

    pub(crate) fn try_emit(&mut self, event: SessionEvent) -> Result<(), InternalEventSinkError> {
        let dropped_before = self.dropped;
        let envelope = self.envelope(event, dropped_before);
        match self.sender.try_send(envelope) {
            Ok(()) => {
                self.dropped = 0;
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped = dropped_before.saturating_add(1);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(InternalEventSinkError::Closed),
        }
    }

    fn envelope(&self, event: SessionEvent, dropped_before: u64) -> SessionEventEnvelope {
        SessionEventEnvelope {
            session_id: self.session_id,
            instance_id: self.instance_id,
            dropped_before,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TurnId;

    fn ids() -> (SessionId, SessionInstanceId) {
        (
            "ses_00000000000000000000000000000001".parse().unwrap(),
            "ins_00000000000000000000000000000001".parse().unwrap(),
        )
    }

    fn event(value: u8) -> SessionEvent {
        SessionEvent::ModelStarted {
            turn_id: format!("trn_{value:032}").parse::<TurnId>().unwrap(),
            round: value.into(),
        }
    }

    #[test]
    fn channel_capacity_is_checked_and_closed_receiver_is_terminal() {
        let (session_id, instance_id) = ids();
        assert!(matches!(
            InternalEventSink::channel(session_id, instance_id, 0),
            Err(InternalEventSinkError::InvalidCapacity)
        ));
        assert!(matches!(
            InternalEventSink::channel(session_id, instance_id, MAX_EVENT_CAPACITY + 1),
            Err(InternalEventSinkError::InvalidCapacity)
        ));
        let (mut sink, stream) = InternalEventSink::channel(session_id, instance_id, 1).unwrap();
        assert_eq!(sink.try_emit(event(1)), Ok(()));
        assert_eq!(sink.try_emit(event(2)), Ok(()));
        assert_eq!(sink.dropped, 1);
        drop(stream);
        assert_eq!(sink.try_emit(event(1)), Err(InternalEventSinkError::Closed));
        assert_eq!(sink.dropped, 1);
        assert_eq!(sink.try_emit(event(2)), Err(InternalEventSinkError::Closed));
        assert_eq!(sink.dropped, 1);
    }

    #[test]
    fn capacity_one_recovers_ordinary_events_with_dropped_count() {
        let (session_id, instance_id) = ids();
        let (mut sink, mut stream) =
            InternalEventSink::channel(session_id, instance_id, 1).unwrap();
        assert_eq!(sink.try_emit(event(1)), Ok(()));
        assert_eq!(sink.try_emit(event(2)), Ok(()));
        assert_eq!(sink.try_emit(event(3)), Ok(()));

        let first = stream.try_recv().unwrap();
        assert_eq!(first.session_id, session_id);
        assert_eq!(first.instance_id, instance_id);
        assert_eq!(first.event, event(1));
        assert_eq!(first.dropped_before, 0);

        assert_eq!(sink.try_emit(event(4)), Ok(()));
        let next = stream.try_recv().unwrap();
        assert_eq!(next.session_id, session_id);
        assert_eq!(next.instance_id, instance_id);
        assert_eq!(next.event, event(4));
        assert_eq!(next.dropped_before, 2);

        assert_eq!(sink.try_emit(event(5)), Ok(()));
        let third = stream.try_recv().unwrap();
        assert_eq!(third.session_id, session_id);
        assert_eq!(third.instance_id, instance_id);
        assert_eq!(third.event, event(5));
        assert_eq!(third.dropped_before, 0);
    }

    #[test]
    fn cumulative_and_saturating_drop_counts_are_exact() {
        let (session_id, instance_id) = ids();
        let (mut sink, mut stream) =
            InternalEventSink::channel(session_id, instance_id, 2).unwrap();
        assert_eq!(sink.try_emit(event(1)), Ok(()));
        assert_eq!(sink.try_emit(event(2)), Ok(()));

        for i in 3..=7 {
            assert_eq!(sink.try_emit(event(i)), Ok(()));
        }

        let first = stream.try_recv().unwrap();
        assert_eq!(first.session_id, session_id);
        assert_eq!(first.instance_id, instance_id);
        assert_eq!(first.event, event(1));
        assert_eq!(first.dropped_before, 0);

        let second = stream.try_recv().unwrap();
        assert_eq!(second.session_id, session_id);
        assert_eq!(second.instance_id, instance_id);
        assert_eq!(second.event, event(2));
        assert_eq!(second.dropped_before, 0);

        assert_eq!(sink.try_emit(event(8)), Ok(()));
        let next = stream.try_recv().unwrap();
        assert_eq!(next.session_id, session_id);
        assert_eq!(next.instance_id, instance_id);
        assert_eq!(next.event, event(8));
        assert_eq!(next.dropped_before, 5);

        assert_eq!(sink.try_emit(event(9)), Ok(()));
        let final_ev = stream.try_recv().unwrap();
        assert_eq!(final_ev.session_id, session_id);
        assert_eq!(final_ev.instance_id, instance_id);
        assert_eq!(final_ev.event, event(9));
        assert_eq!(final_ev.dropped_before, 0);

        assert_eq!(sink.try_emit(event(10)), Ok(()));
        assert_eq!(sink.try_emit(event(11)), Ok(()));
        sink.dropped = u64::MAX;
        assert_eq!(sink.try_emit(event(12)), Ok(()));
        assert_eq!(sink.dropped, u64::MAX);
        let _ = stream.try_recv().unwrap();
        let _ = stream.try_recv().unwrap();
        assert_eq!(sink.try_emit(event(13)), Ok(()));
        let saturated = stream.try_recv().unwrap();
        assert_eq!(saturated.event, event(13));
        assert_eq!(saturated.dropped_before, u64::MAX);
    }
}
