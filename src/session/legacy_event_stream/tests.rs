use super::*;

use crate::ids::{SessionId, TurnId};
use crate::model::Usage;
use crate::session::legacy_snapshot::{
    LegacySessionSnapshot, LegacySnapshotHistory, LegacyTurnSummary,
};
use crate::session::legacy_state::LegacySessionStatus;

fn snapshot(
    session_id: SessionId,
    sequence: u64,
    status: LegacySessionStatus,
) -> LegacySessionSnapshot {
    LegacySessionSnapshot::new(
        session_id,
        status,
        status.turn_id().map(LegacyTurnSummary::new),
        None,
        Usage::default(),
        LegacySnapshotHistory::new(None, None),
        sequence,
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn observation_rejects_zero_capacity_and_delivers_first_snapshot_then_event() {
    let session_id = SessionId::new().unwrap();
    let initial = snapshot(session_id, 0, LegacySessionStatus::Idle);
    assert!(matches!(
        LegacySessionObservation::new(initial.clone(), 0),
        Err(SessionError::InvalidInput)
    ));
    assert!(matches!(
        LegacySessionObservation::new(initial.clone(), MAX_EVENT_CAPACITY + 1),
        Err(SessionError::InvalidInput)
    ));
    assert!(matches!(
        LegacySessionObservation::new(initial.clone(), usize::MAX),
        Err(SessionError::InvalidInput)
    ));
    let exact_capacity =
        LegacySessionObservation::new(initial.clone(), MAX_EVENT_CAPACITY).unwrap();
    drop(exact_capacity);
    let observation = LegacySessionObservation::new(initial.clone(), 4).unwrap();
    for event in [
        LegacySessionEvent::Snapshot(initial.clone()),
        LegacySessionEvent::ResyncRequired,
        LegacySessionEvent::Closed,
    ] {
        assert!(matches!(
            observation.publish(initial.clone(), Some(event)),
            Err(SessionError::InvalidInput)
        ));
    }
    let mut stream = observation.subscribe().unwrap();
    assert_eq!(
        stream.recv().await,
        Some(LegacySessionEvent::Snapshot(initial))
    );
    let turn_id = TurnId::new().unwrap();
    let running = snapshot(session_id, 1, LegacySessionStatus::Running { turn_id });
    observation
        .publish(
            running.clone(),
            Some(LegacySessionEvent::TurnStarted { turn_id }),
        )
        .unwrap();
    assert_eq!(
        stream.recv().await,
        Some(LegacySessionEvent::TurnStarted { turn_id })
    );
    assert_eq!(stream.snapshot(), running);
}

#[tokio::test(flavor = "current_thread")]
async fn subscribe_after_publish_captures_latest_snapshot_without_replaying_event() {
    let session_id = SessionId::new().unwrap();
    let initial = snapshot(session_id, 0, LegacySessionStatus::Idle);
    let observation = LegacySessionObservation::new(initial, 4).unwrap();
    let turn_id = TurnId::new().unwrap();
    let running = snapshot(session_id, 1, LegacySessionStatus::Running { turn_id });
    observation
        .publish(
            running.clone(),
            Some(LegacySessionEvent::TurnStarted { turn_id }),
        )
        .unwrap();
    let mut stream = observation.subscribe().unwrap();
    assert_eq!(
        stream.recv().await,
        Some(LegacySessionEvent::Snapshot(running.clone()))
    );
    assert_eq!(observation.snapshot(), running);
    drop(observation);
    assert_eq!(stream.recv().await, Some(LegacySessionEvent::Closed));
    assert_eq!(stream.recv().await, None);
}

#[tokio::test(flavor = "current_thread")]
async fn lag_returns_resync_then_continues_and_snapshot_recovers_latest_state() {
    let session_id = SessionId::new().unwrap();
    let initial = snapshot(session_id, 0, LegacySessionStatus::Idle);
    let observation = LegacySessionObservation::new(initial, 2).unwrap();
    let mut stream = observation.subscribe().unwrap();
    assert!(matches!(
        stream.recv().await,
        Some(LegacySessionEvent::Snapshot(_))
    ));
    let first_turn = TurnId::new().unwrap();
    let first = snapshot(
        session_id,
        1,
        LegacySessionStatus::Running {
            turn_id: first_turn,
        },
    );
    observation
        .publish(
            first,
            Some(LegacySessionEvent::TurnStarted {
                turn_id: first_turn,
            }),
        )
        .unwrap();
    let second = snapshot(
        session_id,
        2,
        LegacySessionStatus::Running {
            turn_id: first_turn,
        },
    );
    observation
        .publish(
            second,
            Some(LegacySessionEvent::TextDelta {
                turn_id: first_turn,
                delta: "e2".to_owned(),
            }),
        )
        .unwrap();
    let third = snapshot(
        session_id,
        3,
        LegacySessionStatus::Running {
            turn_id: first_turn,
        },
    );
    observation
        .publish(
            third.clone(),
            Some(LegacySessionEvent::ReasoningDelta {
                turn_id: first_turn,
                delta: "e3".to_owned(),
            }),
        )
        .unwrap();
    assert_eq!(
        stream.recv().await,
        Some(LegacySessionEvent::ResyncRequired)
    );
    assert_eq!(
        stream.recv().await,
        Some(LegacySessionEvent::Snapshot(third.clone()))
    );
    assert_eq!(stream.snapshot(), third);
    let fourth = snapshot(
        session_id,
        4,
        LegacySessionStatus::Running {
            turn_id: first_turn,
        },
    );
    observation
        .publish(
            fourth.clone(),
            Some(LegacySessionEvent::TextDelta {
                turn_id: first_turn,
                delta: "e4".to_owned(),
            }),
        )
        .unwrap();
    assert_eq!(
        stream.recv().await,
        Some(LegacySessionEvent::TextDelta {
            turn_id: first_turn,
            delta: "e4".to_owned(),
        })
    );
    assert_eq!(stream.snapshot(), fourth);
}

#[tokio::test(flavor = "current_thread")]
async fn close_delivers_closed_once_then_eof_and_rejects_future_publish() {
    let session_id = SessionId::new().unwrap();
    let initial = snapshot(session_id, 0, LegacySessionStatus::Idle);
    let observation = LegacySessionObservation::new(initial, 4).unwrap();
    assert!(matches!(
        observation.close(snapshot(session_id, 1, LegacySessionStatus::Idle)),
        Err(SessionError::InvalidInput)
    ));
    let mut stream = observation.subscribe().unwrap();
    assert!(matches!(
        stream.recv().await,
        Some(LegacySessionEvent::Snapshot(_))
    ));
    let closing = snapshot(session_id, 1, LegacySessionStatus::Closing);
    observation.close(closing.clone()).unwrap();
    assert_eq!(stream.snapshot(), closing);
    assert_eq!(stream.recv().await, Some(LegacySessionEvent::Closed));
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
    let initial = snapshot(session_id, 0, LegacySessionStatus::Idle);
    let observation = LegacySessionObservation::new(initial, 4).unwrap();
    let mut stream = observation.subscribe().unwrap();
    assert!(matches!(
        stream.recv().await,
        Some(LegacySessionEvent::Snapshot(_))
    ));
    drop(observation);
    assert_eq!(stream.recv().await, Some(LegacySessionEvent::Closed));
    assert_eq!(stream.recv().await, None);
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_subscribers_each_receive_their_own_first_snapshot_and_event() {
    let session_id = SessionId::new().unwrap();
    let initial = snapshot(session_id, 0, LegacySessionStatus::Idle);
    let observation = LegacySessionObservation::new(initial.clone(), 4).unwrap();
    let mut first = observation.subscribe().unwrap();
    let mut second = observation.subscribe().unwrap();
    assert_eq!(
        first.recv().await,
        Some(LegacySessionEvent::Snapshot(initial.clone()))
    );
    assert_eq!(
        second.recv().await,
        Some(LegacySessionEvent::Snapshot(initial))
    );
    let turn_id = TurnId::new().unwrap();
    let running = snapshot(session_id, 1, LegacySessionStatus::Running { turn_id });
    observation
        .publish(running, Some(LegacySessionEvent::TurnStarted { turn_id }))
        .unwrap();
    assert_eq!(
        first.recv().await,
        Some(LegacySessionEvent::TurnStarted { turn_id })
    );
    assert_eq!(
        second.recv().await,
        Some(LegacySessionEvent::TurnStarted { turn_id })
    );
}
