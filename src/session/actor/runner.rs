use crate::agent::{CommittedUpdate, RunnerCommitError, TurnSuspension};
use crate::conversation::{
    AssistantMessageDraft, ConversationCommitError, ConversationCommitErrorKind, SummaryDraft,
    ToolResultDraft, UnsequencedEntry,
};
use crate::model::ModelDriverProgress;

use super::super::event::{OutputChannel, SessionEvent, ToolResultSummary};
use super::*;

impl SessionActor {
    pub(super) async fn handle_runner_event(&mut self, event: RunnerEvent) {
        match event {
            RunnerEvent::CommitAssistant { draft, reply } => {
                let result = self.commit_assistant(draft).await;
                let _ = reply.send(result);
            }
            RunnerEvent::CommitToolResult { draft, reply } => {
                let result = self.commit_tool_result(draft).await;
                let _ = reply.send(result);
            }
            RunnerEvent::CommitSummary {
                snapshot_head,
                draft,
                reply,
            } => {
                let result = self.commit_summary(snapshot_head, draft).await;
                let _ = reply.send(result);
            }
            RunnerEvent::Suspend { suspension } => self.register_suspension(suspension),
            RunnerEvent::Finish { outcome } => {
                if let Some(active) = self.core.active.as_mut() {
                    if active.outcome.is_none() {
                        active.outcome = Some(outcome);
                    }
                }
            }
        }
    }

    async fn commit_assistant(
        &mut self,
        draft: AssistantMessageDraft,
    ) -> Result<CommittedUpdate, RunnerCommitError> {
        self.commit_one(UnsequencedEntry::AssistantMessage(draft))
            .await
    }

    async fn commit_tool_result(
        &mut self,
        draft: ToolResultDraft,
    ) -> Result<CommittedUpdate, RunnerCommitError> {
        self.commit_one(UnsequencedEntry::ToolResult(draft)).await
    }

    pub(super) async fn commit_summary(
        &mut self,
        snapshot_head: ConversationSeq,
        draft: SummaryDraft,
    ) -> Result<CommittedUpdate, RunnerCommitError> {
        if self.conversation.head() != snapshot_head {
            return Err(RunnerCommitError::Stale);
        }
        self.commit_one(UnsequencedEntry::Summary(draft)).await
    }

    async fn commit_one(
        &mut self,
        draft: UnsequencedEntry,
    ) -> Result<CommittedUpdate, RunnerCommitError> {
        if self.core.closing && self.core.active.is_none() {
            return Err(RunnerCommitError::RuntimeClosed);
        }
        if matches!(&self.core.health, SessionHealth::Degraded { .. }) {
            return Err(RunnerCommitError::Degraded);
        }
        let previous_head = self.conversation.head();
        match self.conversation.append_validated(vec![draft]).await {
            Ok(batch) => {
                let [entry] = batch
                    .entries
                    .try_into()
                    .expect("single-entry commit must return exactly one entry");
                self.publish_state();
                Ok(CommittedUpdate {
                    previous_head,
                    entry,
                    conversation: self.conversation.view(),
                })
            }
            Err(error) => Err(self.latch_commit_failure(error)),
        }
    }

    fn latch_commit_failure(&mut self, error: ConversationCommitError) -> RunnerCommitError {
        let unknown = error.kind() == ConversationCommitErrorKind::DurabilityUnknown;
        let result = match error.kind() {
            ConversationCommitErrorKind::DurabilityUnknown => RunnerCommitError::DurabilityUnknown,
            ConversationCommitErrorKind::Closed => RunnerCommitError::RuntimeClosed,
            ConversationCommitErrorKind::Validation
            | ConversationCommitErrorKind::SequenceOverflow
            | ConversationCommitErrorKind::Timestamp => RunnerCommitError::Stale,
            ConversationCommitErrorKind::Log(_) => RunnerCommitError::DurabilityUnavailable,
            _ => RunnerCommitError::DurabilityUnavailable,
        };
        let diagnostic = super::settlement::commit_diagnostic(&error);
        if let Some(active) = self.core.active.as_mut() {
            active.cancellation.cancel();
            if active.commit_failure.is_none() {
                active.commit_failure = Some(ActiveCommitFailure {
                    diagnostic: diagnostic.clone(),
                    unknown,
                });
            }
        }
        if self.core.closing && self.core.closing_durability_failure.is_none() {
            self.core.closing_durability_failure = Some(diagnostic.clone());
        }
        self.mark_degraded(diagnostic);
        result
    }

    pub(super) fn register_suspension(&mut self, suspension: TurnSuspension) {
        let TurnSuspension {
            turn_id,
            tool_call_id,
            tool_name,
            kind,
            resume,
        } = suspension;
        if self.core.closing || self.root_cancel.is_cancelled() {
            let _ = resume.send(Err(SuspensionError::Cancelled));
            return;
        }
        let Some(active) = self.core.active.as_ref() else {
            let _ = resume.send(Err(SuspensionError::RuntimeClosed));
            return;
        };
        if active.turn_id != turn_id {
            let _ = resume.send(Err(SuspensionError::StaleTurn));
            return;
        }
        if active.cancellation.is_cancelled() {
            let _ = resume.send(Err(SuspensionError::Cancelled));
            return;
        }
        if matches!(&self.core.health, SessionHealth::Degraded { .. })
            || active.pending.is_some()
            || active.outcome.is_some()
            || active.commit_failure.is_some()
        {
            let _ = resume.send(Err(SuspensionError::InvalidState));
            return;
        }
        if !self
            .conversation
            .is_next_unresolved_tool(turn_id, &tool_call_id, &tool_name)
        {
            let _ = resume.send(Err(SuspensionError::InvalidState));
            return;
        }
        let interaction_id = match InteractionId::new() {
            Ok(interaction_id) => interaction_id,
            Err(_) => {
                let _ = resume.send(Err(SuspensionError::InvalidState));
                return;
            }
        };
        if self.root_cancel.is_cancelled()
            || self
                .core
                .active
                .as_ref()
                .is_some_and(|active| active.cancellation.is_cancelled())
        {
            let _ = resume.send(Err(SuspensionError::Cancelled));
            return;
        }
        let public = PendingInteraction {
            interaction_id,
            turn_id,
            tool_call_id,
            tool_name,
            kind,
        };
        let Some(active) = self.core.active.as_mut() else {
            let _ = resume.send(Err(SuspensionError::RuntimeClosed));
            return;
        };
        debug_assert_eq!(active.turn_id, turn_id);
        active.pending = Some(PendingInteractionState {
            public: public.clone(),
            resume,
        });
        self.publish_state();
        let _ = self.events.try_emit(SessionEvent::InteractionRequested {
            interaction: public,
        });
    }

    pub(super) fn handle_progress(&mut self, progress: RunnerProgress) {
        let Some(turn_id) = self.active_turn_id() else {
            return;
        };
        let event = match progress {
            RunnerProgress::ModelStarted { model_round } => SessionEvent::ModelStarted {
                turn_id,
                round: model_round,
            },
            RunnerProgress::ModelProgress {
                progress: ModelDriverProgress::TextDelta(delta),
                ..
            } => SessionEvent::OutputDelta {
                turn_id,
                channel: OutputChannel::Text,
                delta,
            },
            RunnerProgress::ModelProgress {
                progress: ModelDriverProgress::ReasoningDelta(delta),
                ..
            } => SessionEvent::OutputDelta {
                turn_id,
                channel: OutputChannel::Reasoning,
                delta,
            },
            RunnerProgress::ModelFinished { model_round, usage } => SessionEvent::ModelFinished {
                turn_id,
                round: model_round,
                usage,
            },
            RunnerProgress::ToolStarted {
                tool_call_id,
                tool_name,
            } => SessionEvent::ToolStarted {
                turn_id,
                tool_call_id,
                tool_name,
            },
            RunnerProgress::ToolProgress {
                tool_call_id,
                progress,
            } => SessionEvent::ToolProgress {
                turn_id,
                tool_call_id,
                progress,
            },
            RunnerProgress::ToolFinished {
                tool_call_id,
                outcome,
                content_bytes,
                ..
            } => SessionEvent::ToolFinished {
                turn_id,
                tool_call_id,
                result: ToolResultSummary {
                    outcome,
                    content_bytes,
                },
            },
        };
        let _ = self.events.try_emit(event);
    }

    pub(super) async fn handle_runner_exit(
        &mut self,
        exit: Option<Result<TurnRunnerExit, tokio::task::JoinError>>,
    ) {
        let Some(turn_id) = self.core.active.as_ref().map(|active| active.turn_id) else {
            return;
        };
        let confirmed_usage = self.conversation.confirmed_turn_usage(turn_id);
        let Some(active) = self.core.active.as_mut() else {
            return;
        };
        active.runner.take();
        let fallback = match exit {
            Some(Ok(TurnRunnerExit::Finished { outcome }))
            | Some(Ok(TurnRunnerExit::ProtocolClosed { outcome })) => outcome,
            Some(Ok(TurnRunnerExit::Panicked)) | Some(Err(_)) | None => {
                internal_outcome(confirmed_usage)
            }
        };
        if active.outcome.is_none() {
            active.outcome = Some(fallback);
        }
        self.settle_active(None).await;
    }
}

fn internal_outcome(usage: crate::model::Usage) -> RunnerOutcome {
    RunnerOutcome::Failed {
        diagnostic: SessionActor::diagnostic(
            DiagnosticCode::RuntimeTerminated,
            DiagnosticCategory::Internal,
            "turn runner terminated unexpectedly",
            false,
        ),
        usage,
    }
}
