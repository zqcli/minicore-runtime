use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;

use super::*;

pub(in crate::session) async fn run_session_actor(actor: &mut SessionActor) -> SessionActorExit {
    match AssertUnwindSafe(actor.run()).catch_unwind().await {
        Ok(exit) => exit,
        Err(_) => actor.close_after_panic().await,
    }
}

impl SessionActor {
    pub(crate) async fn close_after_panic(&mut self) -> SessionActorExit {
        self.begin_shutdown();
        if let Some(active) = self.core.active.as_mut() {
            active.cancellation.cancel();
            if let Some(pending) = active.pending.take() {
                let _ = pending.resume.send(Err(SuspensionError::RuntimeClosed));
            }
        }
        if let Some(runner) = self
            .core
            .active
            .as_mut()
            .and_then(|active| active.runner.as_mut())
        {
            let _ = runner.await;
        }
        if let Some(mut active) = self.core.active.take() {
            active.runner.take();
            self.publish_state();
            if let Some(failure) = active.commit_failure.take() {
                if self.core.closing_durability_failure.is_none() {
                    self.core.closing_durability_failure = Some(failure.diagnostic.clone());
                }
                if failure.unknown {
                    active.completion.durability_unknown(failure.diagnostic);
                } else {
                    active.completion.durability_unavailable(failure.diagnostic);
                }
            } else {
                active.completion.runtime_terminated(Self::diagnostic(
                    DiagnosticCode::RuntimeTerminated,
                    DiagnosticCategory::Internal,
                    "session actor panicked",
                    false,
                ));
            }
        }
        match self.close_log().await {
            SessionActorExit::Closed => SessionActorExit::Panicked,
            SessionActorExit::CloseFailed(error) => SessionActorExit::PanicCloseFailed(error),
            durability @ SessionActorExit::DurabilityFailed { .. } => durability,
            SessionActorExit::OpenFailed
            | SessionActorExit::Panicked
            | SessionActorExit::PanicCloseFailed(_) => SessionActorExit::Panicked,
        }
    }
}
