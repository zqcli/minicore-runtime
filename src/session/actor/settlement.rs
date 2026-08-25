use super::super::event::SessionEvent;
use super::*;
use crate::conversation::{
    ConversationCommitError, ConversationCommitErrorKind, ConversationEntry, TurnTerminal,
};
use crate::error::SessionLogErrorKind;

impl SessionActor {
    pub(super) async fn settle_active(&mut self, override_outcome: Option<RunnerOutcome>) {
        let Some(turn_id) = self.core.active.as_ref().map(|active| active.turn_id) else {
            return;
        };
        let cancellation_won = self.core.closing
            || self
                .core
                .active
                .as_ref()
                .is_some_and(|active| active.cancellation.is_cancelled());
        let pending = self.core.active.as_mut().and_then(|active| {
            active.cancellation.cancel();
            active.pending.take()
        });
        if let Some(pending) = pending {
            let error = if cancellation_won {
                SuspensionError::Cancelled
            } else {
                SuspensionError::RuntimeClosed
            };
            let _ = pending.resume.send(Err(error));
        }
        if let Some(failure) = self
            .core
            .active
            .as_mut()
            .and_then(|active| active.commit_failure.take())
        {
            if let Some(active) = self.core.active.take() {
                self.finish_active_commit_failure(active, failure);
            }
            return;
        }
        let stored_outcome = self
            .core
            .active
            .as_mut()
            .and_then(|active| active.outcome.take());
        let outcome = override_outcome
            .or(stored_outcome)
            .unwrap_or_else(|| internal_outcome(self.conversation.confirmed_turn_usage(turn_id)));
        let usage = outcome.usage();
        let terminal = terminal_for(&outcome, self.core.closing);
        let Some(drafts) = self
            .conversation
            .settlement_drafts(turn_id, terminal.clone(), usage)
        else {
            let diagnostic = Self::diagnostic(
                DiagnosticCode::RuntimeTerminated,
                DiagnosticCategory::Internal,
                "turn settlement state is invalid",
                false,
            );
            if let Some(active) = self.core.active.take() {
                self.finish_failed_state(diagnostic.clone());
                active.completion.runtime_terminated(diagnostic);
            }
            return;
        };
        match self.conversation.append_validated(drafts).await {
            Ok(batch) => {
                let terminal_entry = batch.entries.iter().rev().find_map(|entry| match entry {
                    ConversationEntry::TurnTerminal(entry) => Some(entry.clone()),
                    _ => None,
                });
                let Some(terminal_entry) = terminal_entry else {
                    let diagnostic = Self::diagnostic(
                        DiagnosticCode::RuntimeTerminated,
                        DiagnosticCategory::Internal,
                        "turn settlement terminal is missing",
                        false,
                    );
                    if let Some(active) = self.core.active.take() {
                        self.finish_failed_state(diagnostic.clone());
                        active.completion.runtime_terminated(diagnostic);
                    }
                    return;
                };
                let Some(active) = self.core.active.take() else {
                    return;
                };
                let outcome = TurnOutcome {
                    turn_id,
                    terminal: terminal_entry.terminal,
                    usage: terminal_entry.usage,
                };
                self.record_terminal(outcome.clone());
                active.completion.finish(outcome.clone());
                let _ = self
                    .events
                    .try_emit(SessionEvent::TurnFinished { turn_id, outcome });
            }
            Err(error) => {
                if let Some(active) = self.core.active.take() {
                    self.settlement_failed(active, error);
                }
            }
        }
    }

    fn settlement_failed(&mut self, active: ActiveTurn, error: ConversationCommitError) {
        let diagnostic = commit_diagnostic(&error);
        if self.core.closing && self.core.closing_durability_failure.is_none() {
            self.core.closing_durability_failure = Some(diagnostic.clone());
        }
        let health_changed = matches!(&self.core.health, SessionHealth::Healthy);
        self.core.health = SessionHealth::Degraded {
            diagnostic: diagnostic.clone(),
        };
        self.publish_state();
        if health_changed {
            let _ = self.events.try_emit(SessionEvent::HealthChanged {
                health: self.core.health.clone(),
            });
        }
        if error.kind() == ConversationCommitErrorKind::DurabilityUnknown {
            active.completion.durability_unknown(diagnostic.clone());
        } else {
            active.completion.durability_unavailable(diagnostic.clone());
        }
    }

    pub(super) fn mark_degraded(&mut self, diagnostic: DiagnosticSummary) {
        let health_changed = matches!(&self.core.health, SessionHealth::Healthy);
        if !health_changed {
            return;
        }
        self.core.health = SessionHealth::Degraded {
            diagnostic: diagnostic.clone(),
        };
        self.publish_state();
        let _ = self.events.try_emit(SessionEvent::HealthChanged {
            health: self.core.health.clone(),
        });
    }

    pub(super) fn degrade_on_transcript_failure(
        &mut self,
        diagnostic: DiagnosticSummary,
        unknown: bool,
    ) {
        let health_changed = matches!(&self.core.health, SessionHealth::Healthy);
        if health_changed {
            self.core.health = SessionHealth::Degraded {
                diagnostic: diagnostic.clone(),
            };
        }
        if let Some(active) = self.core.active.as_mut() {
            active.cancellation.cancel();
            if let Some(pending) = active.pending.take() {
                let _ = pending.resume.send(Err(SuspensionError::Cancelled));
            }
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
        self.publish_state();
        if health_changed {
            let _ = self.events.try_emit(SessionEvent::HealthChanged {
                health: self.core.health.clone(),
            });
        }
    }

    fn finish_active_commit_failure(&mut self, active: ActiveTurn, failure: ActiveCommitFailure) {
        if self.core.closing && self.core.closing_durability_failure.is_none() {
            self.core.closing_durability_failure = Some(failure.diagnostic.clone());
        }
        self.publish_state();
        if failure.unknown {
            active.completion.durability_unknown(failure.diagnostic);
        } else {
            active.completion.durability_unavailable(failure.diagnostic);
        }
    }

    fn finish_failed_state(&mut self, diagnostic: DiagnosticSummary) {
        self.core.health = SessionHealth::Degraded {
            diagnostic: diagnostic.clone(),
        };
        self.publish_state();
        let _ = self.events.try_emit(SessionEvent::HealthChanged {
            health: self.core.health.clone(),
        });
    }
}

fn terminal_for(outcome: &RunnerOutcome, closing: bool) -> TurnTerminal {
    if closing {
        return TurnTerminal::CancelledByShutdown;
    }
    match outcome {
        RunnerOutcome::Completed { .. } => TurnTerminal::Completed,
        RunnerOutcome::Failed { diagnostic, .. } => TurnTerminal::Failed {
            diagnostic: diagnostic.clone(),
        },
        RunnerOutcome::Cancelled { .. } => TurnTerminal::CancelledByUser,
        RunnerOutcome::BudgetExceeded { .. } => TurnTerminal::BudgetExceeded,
    }
}

fn internal_outcome(usage: crate::model::Usage) -> RunnerOutcome {
    RunnerOutcome::Failed {
        diagnostic: SessionActor::diagnostic(
            DiagnosticCode::RuntimeTerminated,
            DiagnosticCategory::Internal,
            "turn runner did not provide an outcome",
            false,
        ),
        usage,
    }
}

pub(super) enum TranscriptDisposition {
    Caller(DiagnosticSummary),
    Unavailable(DiagnosticSummary),
    StorageInternal(DiagnosticSummary),
    Degrade(DiagnosticSummary, bool),
    Closed,
}

pub(super) fn classify_transcript_error(error: &ConversationCommitError) -> TranscriptDisposition {
    match error.kind() {
        ConversationCommitErrorKind::Closed
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::Closed) => {
            TranscriptDisposition::Closed
        }
        ConversationCommitErrorKind::TranscriptLimit
        | ConversationCommitErrorKind::TranscriptCursor => {
            TranscriptDisposition::Caller(invalid_input_diagnostic())
        }
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable) => {
            TranscriptDisposition::Unavailable(SessionActor::diagnostic(
                DiagnosticCode::Internal,
                DiagnosticCategory::Storage,
                "session transcript is temporarily unavailable",
                true,
            ))
        }
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Internal) => {
            TranscriptDisposition::StorageInternal(SessionActor::diagnostic(
                DiagnosticCode::Internal,
                DiagnosticCategory::Storage,
                "session transcript store internal error",
                false,
            ))
        }
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Conflict) => {
            TranscriptDisposition::Degrade(
                SessionActor::diagnostic(
                    DiagnosticCode::LogConflict,
                    DiagnosticCategory::Storage,
                    "session log conflict during transcript read",
                    false,
                ),
                false,
            )
        }
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Corrupt)
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::NotInitialized)
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::AlreadyInitialized) => {
            TranscriptDisposition::Degrade(
                SessionActor::diagnostic(
                    DiagnosticCode::LogCorrupt,
                    DiagnosticCategory::Storage,
                    "session log corrupt during transcript read",
                    false,
                ),
                false,
            )
        }
        ConversationCommitErrorKind::DurabilityUnknown
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::UnknownOutcome) => {
            TranscriptDisposition::Degrade(
                SessionActor::diagnostic(
                    DiagnosticCode::LogUnknownOutcome,
                    DiagnosticCategory::Storage,
                    "session log durability unknown during transcript read",
                    false,
                ),
                true,
            )
        }
        ConversationCommitErrorKind::TranscriptContractViolation => TranscriptDisposition::Degrade(
            SessionActor::diagnostic(
                DiagnosticCode::LogCorrupt,
                DiagnosticCategory::Storage,
                "session log transcript contract violation",
                false,
            ),
            false,
        ),
        ConversationCommitErrorKind::TranscriptProjectionMismatch => {
            TranscriptDisposition::Degrade(
                SessionActor::diagnostic(
                    DiagnosticCode::LogConflict,
                    DiagnosticCategory::Storage,
                    "session log transcript projection mismatch",
                    false,
                ),
                false,
            )
        }
        ConversationCommitErrorKind::EmptyBatch
        | ConversationCommitErrorKind::InvalidConfiguration
        | ConversationCommitErrorKind::InvalidManifest
        | ConversationCommitErrorKind::CompatibilityProofMismatch
        | ConversationCommitErrorKind::SessionIdMismatch
        | ConversationCommitErrorKind::ReplayInvalid
        | ConversationCommitErrorKind::RecoveryUncertain
        | ConversationCommitErrorKind::SequenceOverflow
        | ConversationCommitErrorKind::Timestamp
        | ConversationCommitErrorKind::Validation
        | ConversationCommitErrorKind::ContractViolation => TranscriptDisposition::Degrade(
            SessionActor::diagnostic(
                DiagnosticCode::Internal,
                DiagnosticCategory::Storage,
                "session transcript consistency invariant violation",
                false,
            ),
            false,
        ),
    }
}

pub(super) fn commit_diagnostic(error: &ConversationCommitError) -> DiagnosticSummary {
    let code = match error.kind() {
        ConversationCommitErrorKind::DurabilityUnknown
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::UnknownOutcome) => {
            DiagnosticCode::LogUnknownOutcome
        }
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Conflict)
        | ConversationCommitErrorKind::TranscriptProjectionMismatch => DiagnosticCode::LogConflict,
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Corrupt)
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::NotInitialized)
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::AlreadyInitialized)
        | ConversationCommitErrorKind::TranscriptContractViolation => DiagnosticCode::LogCorrupt,
        ConversationCommitErrorKind::Closed
        | ConversationCommitErrorKind::EmptyBatch
        | ConversationCommitErrorKind::InvalidConfiguration
        | ConversationCommitErrorKind::InvalidManifest
        | ConversationCommitErrorKind::CompatibilityProofMismatch
        | ConversationCommitErrorKind::SessionIdMismatch
        | ConversationCommitErrorKind::ReplayInvalid
        | ConversationCommitErrorKind::RecoveryUncertain
        | ConversationCommitErrorKind::TranscriptLimit
        | ConversationCommitErrorKind::TranscriptCursor
        | ConversationCommitErrorKind::SequenceOverflow
        | ConversationCommitErrorKind::Timestamp
        | ConversationCommitErrorKind::Validation
        | ConversationCommitErrorKind::ContractViolation
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::Closed)
        | ConversationCommitErrorKind::Log(SessionLogErrorKind::Internal) => {
            DiagnosticCode::Internal
        }
    };
    SessionActor::diagnostic(
        code,
        DiagnosticCategory::Storage,
        "session conversation commit failed",
        false,
    )
}

fn invalid_input_diagnostic() -> DiagnosticSummary {
    SessionActor::diagnostic(
        DiagnosticCode::InvalidConfiguration,
        DiagnosticCategory::Configuration,
        "session command input is invalid",
        false,
    )
}
