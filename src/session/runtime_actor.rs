use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::{KernelConfig, SessionSpec};
use crate::conversation::{
    ConversationCommitError, ConversationLog, ConversationSeq, TurnTerminalEntry,
};
use crate::ids::{SessionId, SessionInstanceId};

use super::bindings::SessionBindings;
use super::event_stream::{InternalEventSink, SessionEventStream};
use super::state::{SessionHealth, SessionState, SessionStatus};
use super::turn_handle::TurnOutcome;

pub(super) enum SessionActorExit {
    Closed,
    CloseFailed(ConversationCommitError),
    OpenFailed,
    Panicked,
    PanicCloseFailed(ConversationCommitError),
}

pub(super) struct IdleOwnerBuildFailure {
    pub(super) log: ConversationLog,
}

pub(super) struct IdleSessionOwner {
    log: ConversationLog,
    _kernel: KernelConfig,
    _bindings: SessionBindings,
    _spec: SessionSpec,
    state_sender: watch::Sender<SessionState>,
    _events: InternalEventSink,
    #[cfg(test)]
    panic_on_run: bool,
}

pub(super) struct IdleOwnerChannels {
    pub(super) state: watch::Receiver<SessionState>,
    pub(super) events: SessionEventStream,
}

impl IdleSessionOwner {
    pub(super) fn new(
        log: ConversationLog,
        kernel: KernelConfig,
        bindings: SessionBindings,
        spec: SessionSpec,
        session_id: SessionId,
        instance_id: SessionInstanceId,
    ) -> Result<(Self, IdleOwnerChannels), Box<IdleOwnerBuildFailure>> {
        let state = initial_state(session_id, instance_id, log.head(), log.last_terminal());
        if state.validate().is_err() {
            return Err(Box::new(IdleOwnerBuildFailure { log }));
        }
        let (state_sender, state) = watch::channel(state);
        let (events, event_stream) =
            match InternalEventSink::channel(session_id, instance_id, kernel.event_capacity) {
                Ok(channels) => channels,
                Err(_) => {
                    return Err(Box::new(IdleOwnerBuildFailure { log }));
                }
            };
        Ok((
            Self {
                log,
                _kernel: kernel,
                _bindings: bindings,
                _spec: spec,
                state_sender,
                _events: events,
                #[cfg(test)]
                panic_on_run: false,
            },
            IdleOwnerChannels {
                state,
                events: event_stream,
            },
        ))
    }

    pub(super) async fn run(&mut self, owner_cancel: CancellationToken) -> SessionActorExit {
        #[cfg(test)]
        if self.panic_on_run {
            panic!("scripted idle owner panic after ready");
        }
        owner_cancel.cancelled().await;
        self.mark_closing();
        match self.log.close().await {
            Ok(()) => SessionActorExit::Closed,
            Err(error) => SessionActorExit::CloseFailed(error),
        }
    }

    pub(super) async fn close_before_ready(
        mut self,
    ) -> Option<crate::conversation::ConversationCloseOutcome> {
        self.mark_closing();
        self.log.close_after_open_failure().await
    }

    async fn close_after_panic(&mut self) -> Result<(), ConversationCommitError> {
        self.mark_closing();
        self.log.close().await
    }

    fn mark_closing(&self) {
        let mut closing = self.state_sender.borrow().clone();
        closing.status = SessionStatus::Closing;
        closing.pending_interaction = None;
        self.state_sender.send_replace(closing);
    }
}

pub(super) async fn run_idle_owner(
    owner: &mut IdleSessionOwner,
    owner_cancel: CancellationToken,
) -> SessionActorExit {
    match AssertUnwindSafe(owner.run(owner_cancel))
        .catch_unwind()
        .await
    {
        Ok(exit) => exit,
        Err(_) => match owner.close_after_panic().await {
            Ok(()) => SessionActorExit::Panicked,
            Err(error) => SessionActorExit::PanicCloseFailed(error),
        },
    }
}

fn initial_state(
    session_id: SessionId,
    instance_id: SessionInstanceId,
    conversation_seq: ConversationSeq,
    last_terminal: Option<TurnTerminalEntry>,
) -> SessionState {
    SessionState {
        session_id,
        instance_id,
        status: SessionStatus::Idle,
        health: SessionHealth::Healthy,
        active_turn: None,
        pending_interaction: None,
        conversation_seq,
        last_terminal: last_terminal.map(|terminal| TurnOutcome {
            turn_id: terminal.turn_id,
            terminal: terminal.terminal,
            usage: terminal.usage,
        }),
    }
}

#[cfg(test)]
mod tests;
