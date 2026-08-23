use std::future::pending;

use super::*;

pub(super) enum ActorSignal {
    RootCancelled,
    Critical(Option<RunnerEvent>),
    RunnerExited(Option<Result<TurnRunnerExit, tokio::task::JoinError>>),
    Command(Option<SessionCommand>),
    Progress(Option<RunnerProgress>),
}

impl SessionActor {
    pub(super) async fn run(&mut self) -> SessionActorExit {
        loop {
            #[cfg(test)]
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.runner.is_some())
            {
                if let Some(barrier) = super::tests::take_post_ready_panic(self.session_id) {
                    barrier.wait().await;
                    panic!("scripted session actor panic with active turn");
                }
            }
            if self.closing && self.active.is_none() {
                return self.close_log().await;
            }
            let signal = self.next_signal().await;
            match signal {
                ActorSignal::RootCancelled => self.begin_shutdown(),
                ActorSignal::Critical(Some(event)) => self.handle_runner_event(event).await,
                ActorSignal::Critical(None) => {
                    if let Some(active) = self.active.as_mut() {
                        active.critical_open = false;
                    }
                }
                ActorSignal::RunnerExited(exit) => self.handle_runner_exit(exit).await,
                ActorSignal::Command(Some(command)) => self.handle_command(command).await,
                ActorSignal::Command(None) => self.begin_shutdown(),
                ActorSignal::Progress(Some(progress)) => self.handle_progress(progress),
                ActorSignal::Progress(None) => {
                    if let Some(active) = self.active.as_mut() {
                        active.progress_open = false;
                    }
                }
            }
        }
    }

    pub(super) async fn next_signal(&mut self) -> ActorSignal {
        let root_open = !self.closing;
        if let Some(active) = self.active.as_mut() {
            let root = self.root_cancel.clone();
            let commands = &mut self.commands;
            tokio::select! {
                biased;
                _ = root.cancelled(), if root_open => ActorSignal::RootCancelled,
                event = active.critical.recv(), if active.critical_open => {
                    ActorSignal::Critical(event)
                }
                exit = await_runner(&mut active.runner) => ActorSignal::RunnerExited(exit),
                command = commands.recv() => ActorSignal::Command(command),
                progress = active.progress.recv(), if active.progress_open => {
                    ActorSignal::Progress(progress)
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = self.root_cancel.cancelled() => ActorSignal::RootCancelled,
                command = self.commands.recv() => ActorSignal::Command(command),
            }
        }
    }

    pub(super) fn begin_shutdown(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        if self.closing_durability_failure.is_none() {
            self.closing_durability_failure = self
                .active
                .as_ref()
                .and_then(|active| active.commit_failure.as_ref())
                .map(|failure| failure.diagnostic.clone());
        }
        let mut state = self.state();
        state.status = SessionStatus::Closing;
        state.pending_interaction = None;
        self.publish_state(state);
        if let Some(active) = self.active.as_mut() {
            active.cancellation.cancel();
            if let Some(pending) = active.pending.take() {
                let _ = pending.resume.send(Err(SuspensionError::Cancelled));
            }
        }
    }

    pub(super) async fn close_log(&mut self) -> SessionActorExit {
        let close_error = self.conversation.close().await.err();
        if self.closing_durability_failure.take().is_some() {
            return SessionActorExit::DurabilityFailed {
                close_error: close_error.map(Box::new),
            };
        }
        match close_error {
            None => SessionActorExit::Closed,
            Some(error) => SessionActorExit::CloseFailed(error),
        }
    }

    pub(in crate::session) async fn close_before_ready(
        mut self,
    ) -> Option<crate::conversation::ConversationCloseOutcome> {
        self.begin_shutdown();
        self.conversation.close_after_open_failure().await
    }
}

async fn await_runner(
    runner: &mut Option<JoinHandle<TurnRunnerExit>>,
) -> Option<Result<TurnRunnerExit, tokio::task::JoinError>> {
    match runner {
        Some(runner) => Some(runner.await),
        None => pending().await,
    }
}
