use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::conversation::validate_user_text;
use super::event_stream::{SessionEventStream, SessionObservation};
use super::snapshot::SessionSnapshot;
use crate::error_v2::SessionError;
use crate::ids_v2::{InteractionId, TurnId};
use crate::tools_v2::UserAnswer;

pub(crate) const DEFAULT_COMMAND_CAPACITY: usize = 64;
pub(crate) const MAX_COMMAND_CAPACITY: usize = 4_096;

pub(super) struct CloseCompletion {
    sender: watch::Sender<Option<Result<(), SessionError>>>,
}

#[derive(Clone)]
pub(super) struct CloseCompletionWaiter {
    receiver: watch::Receiver<Option<Result<(), SessionError>>>,
}

impl CloseCompletion {
    pub(super) fn channel() -> (Self, CloseCompletionWaiter) {
        let (sender, receiver) = watch::channel(None);
        (Self { sender }, CloseCompletionWaiter { receiver })
    }

    pub(super) fn complete(&self, result: Result<(), SessionError>) {
        self.sender.send_replace(Some(result));
    }
}

impl CloseCompletionWaiter {
    async fn wait(mut self) -> Result<(), SessionError> {
        loop {
            if let Some(result) = self.receiver.borrow().clone() {
                return result;
            }
            if self.receiver.changed().await.is_err() {
                return Err(SessionError::Internal);
            }
        }
    }
}

pub(crate) enum SessionCommand {
    Submit {
        input: String,
        reply: oneshot::Sender<Result<TurnId, SessionError>>,
    },
    Answer {
        interaction_id: InteractionId,
        answer: UserAnswer,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
}

pub(crate) struct CancelSlot(pub(crate) Mutex<Option<(TurnId, CancellationToken)>>);

impl CancelSlot {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(None))
    }

    pub(crate) fn install(&self, turn_id: TurnId, cancellation: CancellationToken) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((turn_id, cancellation));
    }

    pub(crate) fn clear(&self, turn_id: TurnId) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot
            .as_ref()
            .is_some_and(|(current, _)| *current == turn_id)
        {
            *slot = None;
        }
    }

    pub(crate) fn request_close(&self, close_requested: &CancellationToken) {
        let slot = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        close_requested.cancel();
        if let Some((_, cancellation)) = slot.as_ref() {
            cancellation.cancel();
        }
    }

    pub(crate) fn cancel_current(&self) -> bool {
        let slot = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, cancellation)) = slot.as_ref() {
            cancellation.cancel();
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
pub(crate) struct SessionHandle {
    commands: mpsc::Sender<SessionCommand>,
    observation: SessionObservation,
    cancel: Arc<CancelSlot>,
    close_requested: CancellationToken,
    close_complete: CloseCompletionWaiter,
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionHandle { .. }")
    }
}

impl SessionHandle {
    pub(super) fn new_for_actor(
        commands: mpsc::Sender<SessionCommand>,
        observation: SessionObservation,
        cancel: Arc<CancelSlot>,
        close_requested: CancellationToken,
        close_complete: CloseCompletionWaiter,
    ) -> Self {
        Self {
            commands,
            observation,
            cancel,
            close_requested,
            close_complete,
        }
    }

    pub(crate) async fn submit(&self, input: String) -> Result<TurnId, SessionError> {
        validate_user_text(&input).map_err(|_| SessionError::InvalidInput)?;
        if self.close_requested.is_cancelled() {
            return Err(SessionError::Closing);
        }
        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(SessionCommand::Submit { input, reply })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SessionError::Busy,
                mpsc::error::TrySendError::Closed(_) => SessionError::Closing,
            })?;
        receiver.await.map_err(|_| SessionError::Closing)?
    }

    pub(crate) async fn answer(
        &self,
        interaction_id: InteractionId,
        answer: UserAnswer,
    ) -> Result<(), SessionError> {
        if self.close_requested.is_cancelled() {
            return Err(SessionError::Closing);
        }
        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(SessionCommand::Answer {
                interaction_id,
                answer,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SessionError::Busy,
                mpsc::error::TrySendError::Closed(_) => SessionError::Closing,
            })?;
        receiver.await.map_err(|_| SessionError::Closing)?
    }

    pub(crate) fn cancel(&self) -> Result<(), SessionError> {
        if self.close_requested.is_cancelled() {
            return Err(SessionError::Closing);
        }
        if self.cancel.cancel_current() {
            Ok(())
        } else {
            Err(SessionError::InvalidInput)
        }
    }

    pub(crate) fn snapshot(&self) -> SessionSnapshot {
        self.observation.snapshot()
    }

    pub(crate) fn subscribe(&self) -> Result<SessionEventStream, SessionError> {
        self.observation.subscribe()
    }

    pub(crate) async fn close(&self) -> Result<(), SessionError> {
        self.cancel.request_close(&self.close_requested);
        self.close_complete.clone().wait().await
    }
}

const _: () = {
    let _ = DEFAULT_COMMAND_CAPACITY;
    let _ = MAX_COMMAND_CAPACITY;
    let _ = std::mem::size_of::<SessionCommand>();
    let _ = std::mem::size_of::<CancelSlot>();
    let _ = std::mem::size_of::<SessionHandle>();
    let _ = SessionHandle::submit;
    let _ = SessionHandle::answer;
    let _ = SessionHandle::cancel;
    let _ = SessionHandle::snapshot;
    let _ = SessionHandle::subscribe;
    let _ = SessionHandle::close;
};
