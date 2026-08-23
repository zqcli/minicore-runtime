use crate::context::{ContextDriverFailure, ContextError};
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::model::{ModelDriverFailure, ModelErrorKind, Usage};
use crate::prompt::PromptError;
use crate::time::DeadlineSource;

use super::super::runner_protocol::{RunnerCommitError, RunnerOutcome, SuspensionError};
use super::super::turn_context::TurnRunnerRequestError;
use super::support::CriticalFailure;

pub(super) fn request_failure(error: TurnRunnerRequestError) -> RunnerOutcome {
    match error {
        TurnRunnerRequestError::Configuration => failed(
            DiagnosticCode::InvalidConfiguration,
            DiagnosticCategory::Configuration,
            "turn runner configuration is invalid",
            false,
            Usage::default(),
        ),
        TurnRunnerRequestError::Bindings
        | TurnRunnerRequestError::ModelDescriptor => failed(
            DiagnosticCode::ModelMismatch,
            DiagnosticCategory::Configuration,
            "turn runner bindings are invalid",
            false,
            Usage::default(),
        ),
        TurnRunnerRequestError::Conversation => {
            internal_failure("turn conversation is invalid", Usage::default())
        }
    }
}

pub(super) fn context_failure(failure: ContextDriverFailure, usage: Usage) -> RunnerOutcome {
    let error = failure.error();
    if error == ContextError::DeadlineExceeded
        && failure.deadline_source() == Some(DeadlineSource::Turn)
    {
        return budget_exceeded(usage);
    }
    match error {
        ContextError::Cancelled => RunnerOutcome::Cancelled { usage },
        ContextError::DeadlineExceeded => failed(
            DiagnosticCode::ContextFailed,
            DiagnosticCategory::Context,
            "turn context provider deadline expired",
            true,
            usage,
        ),
        ContextError::Unavailable => failed(
            DiagnosticCode::ContextFailed,
            DiagnosticCategory::Context,
            "turn context is unavailable",
            true,
            usage,
        ),
        _ => failed(
            DiagnosticCode::ContextFailed,
            DiagnosticCategory::Context,
            "turn context failed",
            false,
            usage,
        ),
    }
}

pub(super) fn prompt_failure(error: PromptError, usage: Usage) -> RunnerOutcome {
    match error {
        PromptError::ContextOverflow => failed(
            DiagnosticCode::ContextFailed,
            DiagnosticCategory::Compaction,
            "turn requires compaction",
            false,
            usage,
        ),
        _ => internal_failure("turn prompt construction failed", usage),
    }
}

pub(super) fn model_failure(failure: ModelDriverFailure, usage: Usage) -> RunnerOutcome {
    let error = failure.error();
    if error.kind() == ModelErrorKind::Timeout
        && failure.deadline_source() == Some(DeadlineSource::Turn)
    {
        return budget_exceeded(usage);
    }
    match error.kind() {
        ModelErrorKind::Cancelled => RunnerOutcome::Cancelled { usage },
        ModelErrorKind::Timeout => failed(
            DiagnosticCode::ModelTimeout,
            DiagnosticCategory::Model,
            "turn model deadline expired",
            true,
            usage,
        ),
        ModelErrorKind::ContextOverflow => failed(
            DiagnosticCode::ContextFailed,
            DiagnosticCategory::Compaction,
            "turn requires compaction",
            false,
            usage,
        ),
        ModelErrorKind::Unavailable
        | ModelErrorKind::ProviderUnavailable
        | ModelErrorKind::RateLimited
        | ModelErrorKind::TransportUnavailable => failed(
            DiagnosticCode::ModelUnavailable,
            DiagnosticCategory::Model,
            "turn model is unavailable",
            true,
            usage,
        ),
        ModelErrorKind::InvalidProviderResponse
        | ModelErrorKind::IncompleteResponse
        | ModelErrorKind::StreamInterrupted
        | ModelErrorKind::UnexpectedToolCall => failed(
            DiagnosticCode::ModelMalformedResponse,
            DiagnosticCategory::Model,
            "turn model response is invalid",
            false,
            usage,
        ),
        _ => failed(
            DiagnosticCode::Internal,
            DiagnosticCategory::Model,
            "turn model failed",
            false,
            usage,
        ),
    }
}

pub(super) fn critical_failure(error: CriticalFailure, usage: Usage) -> RunnerOutcome {
    match error {
        CriticalFailure::Cancelled | CriticalFailure::Suspension(SuspensionError::Cancelled) => {
            RunnerOutcome::Cancelled { usage }
        }
        CriticalFailure::DeadlineExceeded
        | CriticalFailure::Suspension(SuspensionError::DeadlineExceeded) => budget_exceeded(usage),
        CriticalFailure::Commit(RunnerCommitError::Stale)
        | CriticalFailure::Suspension(SuspensionError::StaleTurn)
        | CriticalFailure::InvalidAck => failed(
            DiagnosticCode::SessionBusy,
            DiagnosticCategory::Internal,
            "turn protocol became stale",
            false,
            usage,
        ),
        CriticalFailure::Commit(RunnerCommitError::Degraded) => failed(
            DiagnosticCode::SessionDegraded,
            DiagnosticCategory::Storage,
            "turn commit was rejected",
            false,
            usage,
        ),
        CriticalFailure::Commit(RunnerCommitError::DurabilityUnavailable) => failed(
            DiagnosticCode::LogConflict,
            DiagnosticCategory::Storage,
            "turn durability is unavailable",
            true,
            usage,
        ),
        CriticalFailure::Commit(RunnerCommitError::DurabilityUnknown) => failed(
            DiagnosticCode::LogUnknownOutcome,
            DiagnosticCategory::Storage,
            "turn durability is unknown",
            false,
            usage,
        ),
        CriticalFailure::RuntimeClosed
        | CriticalFailure::Commit(RunnerCommitError::RuntimeClosed)
        | CriticalFailure::Suspension(SuspensionError::RuntimeClosed) => failed(
            DiagnosticCode::RuntimeTerminated,
            DiagnosticCategory::Internal,
            "turn runtime is closed",
            false,
            usage,
        ),
        CriticalFailure::Suspension(SuspensionError::InvalidState) => {
            internal_failure("turn suspension state is invalid", usage)
        }
    }
}

pub(super) const fn budget_exceeded(usage: Usage) -> RunnerOutcome {
    RunnerOutcome::BudgetExceeded { usage }
}

pub(super) fn internal_failure(message: &'static str, usage: Usage) -> RunnerOutcome {
    failed(
        DiagnosticCode::Internal,
        DiagnosticCategory::Internal,
        message,
        false,
        usage,
    )
}

pub(super) fn failed(
    code: DiagnosticCode,
    category: DiagnosticCategory,
    message: &'static str,
    retryable: bool,
    usage: Usage,
) -> RunnerOutcome {
    RunnerOutcome::Failed {
        diagnostic: DiagnosticSummary::bounded_static(code, category, message, retryable),
        usage,
    }
}
