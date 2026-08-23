use std::fmt;

use thiserror::Error;
use tokio::sync::mpsc;

use crate::ids::{SessionId, SessionInstanceId};

use super::event::{SessionEvent, SessionEventEnvelope};

const MAX_EVENT_CAPACITY: usize = 4_096;

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
    #[error("events-dropped markers are owned by the internal event sink")]
    MarkerReserved,
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
        if matches!(&event, SessionEvent::EventsDropped { .. }) {
            return Err(InternalEventSinkError::MarkerReserved);
        }
        if self.sender.is_closed() {
            return Err(InternalEventSinkError::Closed);
        }
        if self.dropped != 0 {
            let marker = self.envelope(SessionEvent::EventsDropped {
                count: self.dropped,
            });
            match self.sender.try_send(marker) {
                Ok(()) => self.dropped = 0,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.record_drop();
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(InternalEventSinkError::Closed);
                }
            }
        }
        let envelope = self.envelope(event);
        match self.sender.try_send(envelope) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.record_drop();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(InternalEventSinkError::Closed),
        }
    }

    fn envelope(&self, event: SessionEvent) -> SessionEventEnvelope {
        SessionEventEnvelope {
            session_id: self.session_id,
            instance_id: self.instance_id,
            event,
        }
    }

    fn record_drop(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }
}

const _: () = {
    let _ = std::mem::size_of::<InternalEventSink>();
    let _ = InternalEventSink::channel;
    let _ = InternalEventSink::try_emit;
};

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
    fn channel_capacity_is_checked_and_stream_is_truly_bounded() {
        let (session_id, instance_id) = ids();
        assert!(matches!(
            InternalEventSink::channel(session_id, instance_id, 0),
            Err(InternalEventSinkError::InvalidCapacity)
        ));
        assert!(matches!(
            InternalEventSink::channel(session_id, instance_id, MAX_EVENT_CAPACITY + 1),
            Err(InternalEventSinkError::InvalidCapacity)
        ));
        let (mut sink, mut stream) =
            InternalEventSink::channel(session_id, instance_id, 1).unwrap();
        assert_eq!(sink.try_emit(event(1)), Ok(()));
        assert_eq!(sink.try_emit(event(2)), Ok(()));
        assert_eq!(sink.dropped, 1);
        assert!(matches!(
            stream.try_recv().unwrap().event,
            SessionEvent::ModelStarted { round: 1, .. }
        ));
    }

    #[test]
    fn full_marker_attempt_accumulates_then_precedes_the_next_ordinary_event() {
        let (session_id, instance_id) = ids();
        let (mut sink, mut stream) =
            InternalEventSink::channel(session_id, instance_id, 2).unwrap();
        sink.try_emit(event(1)).unwrap();
        sink.try_emit(event(2)).unwrap();
        sink.try_emit(event(3)).unwrap();
        assert_eq!(sink.dropped, 1);

        sink.try_emit(event(4)).unwrap();
        assert_eq!(sink.dropped, 2);

        let first = stream.try_recv().unwrap();
        assert_eq!(first.session_id, session_id);
        assert_eq!(first.instance_id, instance_id);
        assert_eq!(first.event, event(1));
        let second = stream.try_recv().unwrap();
        assert_eq!(second.session_id, session_id);
        assert_eq!(second.instance_id, instance_id);
        assert_eq!(second.event, event(2));

        sink.try_emit(event(5)).unwrap();
        let marker = stream.try_recv().unwrap();
        assert_eq!(marker.session_id, session_id);
        assert_eq!(marker.instance_id, instance_id);
        assert_eq!(marker.event, SessionEvent::EventsDropped { count: 2 });
        let ordinary = stream.try_recv().unwrap();
        assert_eq!(ordinary.session_id, session_id);
        assert_eq!(ordinary.instance_id, instance_id);
        assert_eq!(ordinary.event, event(5));
        assert_eq!(sink.dropped, 0);
    }

    #[test]
    fn marker_full_drops_current_and_closed_receiver_is_terminal() {
        let (session_id, instance_id) = ids();
        let (mut sink, mut stream) =
            InternalEventSink::channel(session_id, instance_id, 1).unwrap();
        sink.try_emit(event(1)).unwrap();
        sink.try_emit(event(2)).unwrap();
        sink.try_emit(event(3)).unwrap();
        assert_eq!(sink.dropped, 2);
        let _ = stream.try_recv().unwrap();
        assert_eq!(
            sink.try_emit(SessionEvent::EventsDropped { count: 99 }),
            Err(InternalEventSinkError::MarkerReserved)
        );
        drop(stream);
        assert_eq!(sink.try_emit(event(4)), Err(InternalEventSinkError::Closed));
        assert_eq!(sink.try_emit(event(5)), Err(InternalEventSinkError::Closed));
        assert_eq!(sink.dropped, 2);
    }
}
