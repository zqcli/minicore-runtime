use std::fmt;

use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{TurnOptions, UserInput};
use crate::conversation::{ConversationSeq, TranscriptPage};
use crate::error::SessionError;
use crate::ids::{InteractionId, SessionId, SessionInstanceId};

use super::command::SessionCommand;
use super::state::SessionState;
use super::turn_handle::TurnHandle;
use crate::interaction::InteractionAnswer;

#[derive(Clone)]
pub struct SessionHandle {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    commands: mpsc::Sender<SessionCommand>,
    state: watch::Receiver<SessionState>,
}

impl SessionHandle {
    pub(crate) fn new(
        session_id: SessionId,
        instance_id: SessionInstanceId,
        commands: mpsc::Sender<SessionCommand>,
        state: watch::Receiver<SessionState>,
    ) -> Self {
        Self {
            session_id,
            instance_id,
            commands,
            state,
        }
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn instance_id(&self) -> SessionInstanceId {
        self.instance_id
    }

    pub fn state(&self) -> SessionState {
        self.state.borrow().clone()
    }

    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.state.clone()
    }

    pub async fn submit(
        &self,
        input: UserInput,
        options: TurnOptions,
    ) -> Result<TurnHandle, SessionError> {
        let (reply, receiver) = oneshot::channel();
        self.send(SessionCommand::Submit {
            input,
            options,
            reply,
        })?;
        receiver.await.map_err(|_| SessionError::Closed)?
    }

    pub async fn answer(
        &self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), SessionError> {
        let (reply, receiver) = oneshot::channel();
        self.send(SessionCommand::Answer {
            interaction_id,
            answer,
            reply,
        })?;
        receiver.await.map_err(|_| SessionError::Closed)?
    }

    pub async fn transcript(
        &self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> Result<TranscriptPage, SessionError> {
        let (reply, receiver) = oneshot::channel();
        self.send(SessionCommand::Transcript {
            after,
            limit,
            reply,
        })?;
        receiver.await.map_err(|_| SessionError::Closed)?
    }

    fn send(&self, command: SessionCommand) -> Result<(), SessionError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SessionError::Backpressure,
                mpsc::error::TrySendError::Closed(_) => SessionError::Closed,
            })
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("session_id", &self.session_id)
            .field("instance_id", &self.instance_id)
            .field("state", &self.state.borrow().status)
            .finish_non_exhaustive()
    }
}
