use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::conversation::{ConversationCommitError, ConversationCommitErrorKind, DurabilityClass};
use crate::error::SessionShutdownError;
use crate::storage::SessionLogErrorKind;

use super::actor::SessionActorExit;

pub(super) fn construct_shutdown_timeout<'a>(
    runtime: &Handle,
    timeout: Duration,
    task: &'a mut JoinHandle<SessionActorExit>,
) -> Result<tokio::time::Timeout<&'a mut JoinHandle<SessionActorExit>>, ()> {
    catch_unwind(AssertUnwindSafe(|| {
        let _entered = runtime.enter();
        tokio::time::timeout(timeout, task)
    }))
    .map_err(|_| ())
}

pub(super) fn map_actor_exit(exit: SessionActorExit) -> Result<(), SessionShutdownError> {
    match exit {
        SessionActorExit::Closed => Ok(()),
        SessionActorExit::OpenFailed | SessionActorExit::Panicked => {
            Err(SessionShutdownError::actor_terminated())
        }
        SessionActorExit::DurabilityFailed { close_error } => {
            let _secondary_close_kind = close_error.as_deref().map(ConversationCommitError::kind);
            Err(SessionShutdownError::durability())
        }
        SessionActorExit::PanicCloseFailed(error) => {
            let _close_kind = error.kind();
            Err(SessionShutdownError::actor_terminated())
        }
        SessionActorExit::CloseFailed(error)
            if error.durability_class() == DurabilityClass::UnknownOutcome =>
        {
            Err(SessionShutdownError::durability())
        }
        SessionActorExit::CloseFailed(error) => match error.kind() {
            ConversationCommitErrorKind::Log(kind) => Err(SessionShutdownError::log_close(kind)),
            _ => Err(SessionShutdownError::log_close(
                SessionLogErrorKind::Internal,
            )),
        },
    }
}
