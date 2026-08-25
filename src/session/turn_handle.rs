use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::conversation::TurnTerminal;
use crate::error::{DiagnosticSummary, TurnWaitError};
use crate::ids::{SessionId, SessionInstanceId, TurnId};
use crate::model::Usage;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOutcome {
    pub turn_id: TurnId,
    pub terminal: TurnTerminal,
    pub usage: Usage,
}

/// A cloneable handle for cancellation and durable completion of one Turn.
///
/// Dropping `TurnHandle` does not cancel the Turn. A Host may intentionally
/// detach a handle, but should record that decision.
#[must_use = "TurnHandle should be awaited, cancelled, or intentionally detached"]
#[derive(Clone)]
pub struct TurnHandle {
    inner: Arc<TurnInner>,
}

#[derive(Clone)]
pub(crate) struct TurnCompletion {
    inner: Arc<TurnInner>,
}

struct TurnInner {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    turn_id: TurnId,
    cancellation: CancellationToken,
    state: Mutex<TurnCompletionState>,
    completion: Notify,
}

enum TurnCompletionState {
    Running { cancel_requested: bool },
    Finished(Result<TurnOutcome, TurnWaitError>),
}

impl TurnHandle {
    pub(crate) fn new(
        session_id: SessionId,
        instance_id: SessionInstanceId,
        turn_id: TurnId,
        cancellation: CancellationToken,
    ) -> (Self, TurnCompletion) {
        let inner = Arc::new(TurnInner {
            session_id,
            instance_id,
            turn_id,
            cancellation,
            state: Mutex::new(TurnCompletionState::Running {
                cancel_requested: false,
            }),
            completion: Notify::new(),
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            TurnCompletion { inner },
        )
    }

    pub fn session_id(&self) -> SessionId {
        self.inner.session_id
    }

    pub fn instance_id(&self) -> SessionInstanceId {
        self.inner.instance_id
    }

    pub fn turn_id(&self) -> TurnId {
        self.inner.turn_id
    }

    pub fn cancel(&self) -> bool {
        let mut state = self.inner.lock_state();
        match &mut *state {
            TurnCompletionState::Running { cancel_requested } if !*cancel_requested => {
                *cancel_requested = true;
                self.inner.cancellation.cancel();
                true
            }
            TurnCompletionState::Running { .. } | TurnCompletionState::Finished(_) => false,
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(&*self.inner.lock_state(), TurnCompletionState::Finished(_))
    }

    pub async fn wait(&self) -> Result<TurnOutcome, TurnWaitError> {
        loop {
            let notified = self.inner.completion.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.inner.result() {
                return result;
            }
            notified.await;
        }
    }
}

impl fmt::Debug for TurnHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnHandle")
            .field("session_id", &self.session_id())
            .field("instance_id", &self.instance_id())
            .field("turn_id", &self.turn_id())
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl TurnCompletion {
    pub(crate) fn finish(&self, outcome: TurnOutcome) -> bool {
        if outcome.turn_id != self.inner.turn_id {
            return false;
        }
        self.complete(Ok(outcome))
    }

    pub(crate) fn durability_unknown(&self, diagnostic: DiagnosticSummary) -> bool {
        self.complete(Err(TurnWaitError::DurabilityUnknown(diagnostic)))
    }

    pub(crate) fn durability_unavailable(&self, diagnostic: DiagnosticSummary) -> bool {
        self.complete(Err(TurnWaitError::DurabilityUnavailable(diagnostic)))
    }

    pub(crate) fn runtime_terminated(&self, diagnostic: DiagnosticSummary) -> bool {
        self.complete(Err(TurnWaitError::RuntimeTerminated(diagnostic)))
    }

    fn complete(&self, result: Result<TurnOutcome, TurnWaitError>) -> bool {
        let mut state = self.inner.lock_state();
        if matches!(&*state, TurnCompletionState::Finished(_)) {
            return false;
        }
        *state = TurnCompletionState::Finished(result);
        drop(state);
        self.inner.completion.notify_waiters();
        true
    }
}

impl TurnInner {
    fn lock_state(&self) -> MutexGuard<'_, TurnCompletionState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn result(&self) -> Option<Result<TurnOutcome, TurnWaitError>> {
        match &*self.lock_state() {
            TurnCompletionState::Running { .. } => None,
            TurnCompletionState::Finished(result) => Some(result.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, task::Poll};

    use tokio::sync::Barrier;

    use super::*;
    use crate::error::{DiagnosticCategory, DiagnosticCode};
    use crate::value::BoundedText;

    fn ids() -> (SessionId, SessionInstanceId, TurnId) {
        (
            "ses_00000000000000000000000000000001".parse().unwrap(),
            "ins_00000000000000000000000000000001".parse().unwrap(),
            "trn_00000000000000000000000000000001".parse().unwrap(),
        )
    }

    fn channel() -> (TurnHandle, TurnCompletion, CancellationToken) {
        let (session_id, instance_id, turn_id) = ids();
        let cancellation = CancellationToken::new();
        let (handle, completion) =
            TurnHandle::new(session_id, instance_id, turn_id, cancellation.clone());
        (handle, completion, cancellation)
    }

    fn outcome() -> TurnOutcome {
        TurnOutcome {
            turn_id: ids().2,
            terminal: TurnTerminal::Completed,
            usage: Usage::new(1, 2, 3),
        }
    }

    fn diagnostic() -> DiagnosticSummary {
        DiagnosticSummary::new(
            DiagnosticCode::RuntimeTerminated,
            DiagnosticCategory::Internal,
            BoundedText::new("private diagnostic").unwrap(),
            false,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clones_wait_for_one_first_wins_completion() {
        let (handle, completion, _) = channel();
        let second_handle = handle.clone();
        let first = handle.wait();
        let second = second_handle.wait();
        tokio::pin!(first, second);

        assert!(matches!(futures_util::poll!(first.as_mut()), Poll::Pending));
        assert!(matches!(
            futures_util::poll!(second.as_mut()),
            Poll::Pending
        ));
        assert!(completion.finish(outcome()));
        assert!(!completion.runtime_terminated(diagnostic()));
        assert_eq!(first.await, Ok(outcome()));
        assert_eq!(second.await, Ok(outcome()));
        assert!(handle.is_finished());
        assert!(!handle.cancel());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_and_completion_share_one_linearization_point() {
        for _ in 0..32 {
            let (handle, completion, cancellation) = channel();
            let barrier = Arc::new(Barrier::new(3));
            let cancel_task = tokio::spawn({
                let handle = handle.clone();
                let barrier = Arc::clone(&barrier);
                async move {
                    barrier.wait().await;
                    handle.cancel()
                }
            });
            let finish_task = tokio::spawn({
                let barrier = Arc::clone(&barrier);
                async move {
                    barrier.wait().await;
                    completion.finish(outcome())
                }
            });
            barrier.wait().await;
            let cancelled = cancel_task.await.unwrap();
            assert!(finish_task.await.unwrap());
            assert_eq!(cancellation.is_cancelled(), cancelled);
            assert_eq!(handle.wait().await, Ok(outcome()));
            assert!(!handle.cancel());
        }
    }

    #[test]
    fn repeated_cancel_and_drop_do_not_create_new_cancellation() {
        let (dropped, _completion, dropped_cancellation) = channel();
        drop(dropped);
        assert!(!dropped_cancellation.is_cancelled());

        let (handle, _completion, cancellation) = channel();
        assert!(handle.cancel());
        assert!(!handle.cancel());
        assert!(cancellation.is_cancelled());
        assert!(!handle.is_finished());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn internal_error_publishers_are_first_wins_and_redacted() {
        let (handle, completion, _) = channel();
        assert!(completion.durability_unknown(diagnostic()));
        assert!(!completion.durability_unavailable(diagnostic()));
        assert_eq!(
            handle.wait().await,
            Err(TurnWaitError::DurabilityUnknown(diagnostic()))
        );
        assert!(!format!("{:?}", handle.wait().await.unwrap_err()).contains("private diagnostic"));
    }
}
