use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;

use super::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLogErrorKind {
    NotInitialized,
    AlreadyInitialized,
    Conflict,
    Corrupt,
    Unavailable,
    UnknownOutcome,
    Closed,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurabilityClass {
    KnownFailure,
    UnknownOutcome,
    ConsistencyFailure,
    NotApplicable,
}

impl SessionLogErrorKind {
    pub(crate) const fn durability_class(self) -> DurabilityClass {
        match self {
            Self::Conflict | Self::Corrupt | Self::NotInitialized | Self::AlreadyInitialized => {
                DurabilityClass::ConsistencyFailure
            }
            Self::UnknownOutcome => DurabilityClass::UnknownOutcome,
            Self::Unavailable | Self::Closed | Self::Internal => DurabilityClass::KnownFailure,
        }
    }

    pub(crate) const fn diagnostic_code(self) -> DiagnosticCode {
        match self {
            Self::Conflict => DiagnosticCode::LogConflict,
            Self::Corrupt => DiagnosticCode::LogCorrupt,
            Self::UnknownOutcome => DiagnosticCode::LogUnknownOutcome,
            Self::NotInitialized | Self::AlreadyInitialized => {
                DiagnosticCode::InvalidSessionManifest
            }
            Self::Closed => DiagnosticCode::SessionClosed,
            Self::Unavailable | Self::Internal => DiagnosticCode::Internal,
        }
    }

    pub(crate) const fn retryable(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLogError {
    kind: SessionLogErrorKind,
    diagnostic: DiagnosticSummary,
}

impl SessionLogError {
    pub fn new(kind: SessionLogErrorKind, diagnostic: DiagnosticSummary) -> Self {
        Self { kind, diagnostic }
    }

    pub const fn kind(&self) -> SessionLogErrorKind {
        self.kind
    }

    pub const fn diagnostic(&self) -> &DiagnosticSummary {
        &self.diagnostic
    }
}

impl fmt::Display for SessionLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "session log error: {:?}", self.kind)
    }
}

impl std::error::Error for SessionLogError {}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOpenErrorKind {
    InvalidConfiguration,
    InvalidManifest,
    SessionIdMismatch,
    BindingMismatch,
    Log,
    RecoveryUncertain,
    ActorStartFailed,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionOpenError {
    kind: SessionOpenErrorKind,
    diagnostic: DiagnosticSummary,
    details: Option<Box<SessionOpenErrorDetails>>,
}

#[derive(Clone, Default, Eq, PartialEq)]
struct SessionOpenErrorDetails {
    session_id_mismatch: Option<(SessionId, SessionId)>,
    log_error: Option<SessionLogError>,
    secondary_diagnostic: Option<DiagnosticSummary>,
}

impl SessionOpenError {
    pub const fn kind(&self) -> SessionOpenErrorKind {
        self.kind
    }

    pub const fn diagnostic(&self) -> &DiagnosticSummary {
        &self.diagnostic
    }

    pub fn session_id_mismatch(&self) -> Option<(SessionId, SessionId)> {
        match self.details.as_deref() {
            Some(details) => details.session_id_mismatch,
            None => None,
        }
    }

    pub fn log_error(&self) -> Option<&SessionLogError> {
        self.details
            .as_deref()
            .and_then(|details| details.log_error.as_ref())
    }

    pub fn secondary_diagnostic(&self) -> Option<&DiagnosticSummary> {
        self.details
            .as_deref()
            .and_then(|details| details.secondary_diagnostic.as_ref())
    }

    pub(crate) fn invalid_configuration() -> Self {
        Self::new(
            SessionOpenErrorKind::InvalidConfiguration,
            DiagnosticCode::InvalidConfiguration,
            DiagnosticCategory::Configuration,
            "session runtime configuration is invalid",
            false,
        )
    }

    pub(crate) fn invalid_manifest() -> Self {
        Self::new(
            SessionOpenErrorKind::InvalidManifest,
            DiagnosticCode::InvalidSessionManifest,
            DiagnosticCategory::Configuration,
            "session manifest or confirmed conversation is invalid",
            false,
        )
    }

    pub(crate) fn for_session_id_mismatch(expected: SessionId, actual: SessionId) -> Self {
        let mut error = Self::new(
            SessionOpenErrorKind::SessionIdMismatch,
            DiagnosticCode::InvalidSessionManifest,
            DiagnosticCategory::Configuration,
            "loaded manifest session identity does not match",
            false,
        );
        error.details_mut().session_id_mismatch = Some((expected, actual));
        error
    }

    pub(crate) fn binding_mismatch(model_mismatch: bool) -> Self {
        Self::new(
            SessionOpenErrorKind::BindingMismatch,
            if model_mismatch {
                DiagnosticCode::ModelMismatch
            } else {
                DiagnosticCode::InvalidConfiguration
            },
            DiagnosticCategory::Configuration,
            "session bindings are incompatible with the durable specification",
            false,
        )
    }

    pub(crate) fn log(log_error: SessionLogError) -> Self {
        let mut error = Self::new(
            SessionOpenErrorKind::Log,
            log_error.kind().diagnostic_code(),
            DiagnosticCategory::Storage,
            "session log operation failed while opening the session",
            log_error.kind().retryable(),
        );
        error.details_mut().log_error = Some(log_error);
        error
    }

    pub(crate) fn recovery_uncertain(log_error: Option<SessionLogError>) -> Self {
        let mut error = Self::new(
            SessionOpenErrorKind::RecoveryUncertain,
            DiagnosticCode::LogUnknownOutcome,
            DiagnosticCategory::Storage,
            "restart recovery durability is uncertain",
            false,
        );
        if let Some(log_error) = log_error {
            error.details_mut().log_error = Some(log_error);
        }
        error
    }

    pub(crate) fn actor_start_failed() -> Self {
        Self::new(
            SessionOpenErrorKind::ActorStartFailed,
            DiagnosticCode::RuntimeTerminated,
            DiagnosticCategory::Internal,
            "session owner task terminated before becoming ready",
            false,
        )
    }

    pub(crate) fn with_secondary_diagnostic(
        mut self,
        diagnostic: Option<DiagnosticSummary>,
    ) -> Self {
        if let Some(diagnostic) = diagnostic {
            self.details_mut().secondary_diagnostic = Some(diagnostic);
        }
        self
    }

    fn new(
        kind: SessionOpenErrorKind,
        code: DiagnosticCode,
        category: DiagnosticCategory,
        message: &'static str,
        retryable: bool,
    ) -> Self {
        Self {
            kind,
            diagnostic: DiagnosticSummary::bounded_static(code, category, message, retryable),
            details: None,
        }
    }

    fn details_mut(&mut self) -> &mut SessionOpenErrorDetails {
        self.details.get_or_insert_with(Box::default)
    }
}

impl fmt::Debug for SessionOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionOpenError")
            .field("kind", &self.kind)
            .field("diagnostic", &self.diagnostic)
            .field("session_id_mismatch", &self.session_id_mismatch())
            .field("has_log_error", &self.log_error().is_some())
            .field("secondary_diagnostic", &self.secondary_diagnostic())
            .finish()
    }
}

impl fmt::Display for SessionOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            SessionOpenErrorKind::InvalidConfiguration => "session configuration is invalid",
            SessionOpenErrorKind::InvalidManifest => "session manifest is invalid",
            SessionOpenErrorKind::SessionIdMismatch => "session identity does not match",
            SessionOpenErrorKind::BindingMismatch => "session bindings do not match",
            SessionOpenErrorKind::Log => "session log open failed",
            SessionOpenErrorKind::RecoveryUncertain => "session recovery is uncertain",
            SessionOpenErrorKind::ActorStartFailed => "session owner failed to start",
        })
    }
}

impl std::error::Error for SessionOpenError {}

#[non_exhaustive]
#[derive(Clone, Eq, PartialEq)]
pub enum SessionShutdownError {
    Timeout(DiagnosticSummary),
    Durability(DiagnosticSummary),
    LogClose(DiagnosticSummary),
    ActorTerminated(DiagnosticSummary),
}

impl SessionShutdownError {
    pub(crate) fn timeout() -> Self {
        Self::Timeout(DiagnosticSummary::bounded_static(
            DiagnosticCode::ShutdownTimeout,
            DiagnosticCategory::Cancellation,
            "session shutdown exceeded its configured timeout",
            false,
        ))
    }

    pub(crate) fn durability() -> Self {
        Self::Durability(DiagnosticSummary::bounded_static(
            DiagnosticCode::LogUnknownOutcome,
            DiagnosticCategory::Storage,
            "session shutdown did not reach durable completion",
            false,
        ))
    }

    pub(crate) fn log_close(kind: SessionLogErrorKind) -> Self {
        Self::LogClose(DiagnosticSummary::bounded_static(
            kind.diagnostic_code(),
            DiagnosticCategory::Storage,
            "session log close failed",
            kind.retryable(),
        ))
    }

    pub(crate) fn actor_terminated() -> Self {
        Self::ActorTerminated(DiagnosticSummary::bounded_static(
            DiagnosticCode::RuntimeTerminated,
            DiagnosticCategory::Internal,
            "session owner task terminated unexpectedly",
            false,
        ))
    }

    fn diagnostic(&self) -> &DiagnosticSummary {
        match self {
            Self::Timeout(diagnostic)
            | Self::Durability(diagnostic)
            | Self::LogClose(diagnostic)
            | Self::ActorTerminated(diagnostic) => diagnostic,
        }
    }
}

impl fmt::Debug for SessionShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionShutdownError")
            .field("kind", &shutdown_kind(self))
            .field("diagnostic", self.diagnostic())
            .finish()
    }
}

impl fmt::Display for SessionShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Timeout(_) => "session shutdown timed out",
            Self::Durability(_) => "session shutdown durability failed",
            Self::LogClose(_) => "session log close failed during shutdown",
            Self::ActorTerminated(_) => "session owner task terminated unexpectedly",
        })
    }
}

impl std::error::Error for SessionShutdownError {}

fn shutdown_kind(error: &SessionShutdownError) -> &'static str {
    match error {
        SessionShutdownError::Timeout(_) => "Timeout",
        SessionShutdownError::Durability(_) => "Durability",
        SessionShutdownError::LogClose(_) => "LogClose",
        SessionShutdownError::ActorTerminated(_) => "ActorTerminated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_log_durability_classification_matrix_is_stable() {
        for (kind, class, code, retryable) in [
            (
                SessionLogErrorKind::Unavailable,
                DurabilityClass::KnownFailure,
                DiagnosticCode::Internal,
                true,
            ),
            (
                SessionLogErrorKind::Internal,
                DurabilityClass::KnownFailure,
                DiagnosticCode::Internal,
                false,
            ),
            (
                SessionLogErrorKind::Closed,
                DurabilityClass::KnownFailure,
                DiagnosticCode::SessionClosed,
                false,
            ),
            (
                SessionLogErrorKind::UnknownOutcome,
                DurabilityClass::UnknownOutcome,
                DiagnosticCode::LogUnknownOutcome,
                false,
            ),
            (
                SessionLogErrorKind::Conflict,
                DurabilityClass::ConsistencyFailure,
                DiagnosticCode::LogConflict,
                false,
            ),
            (
                SessionLogErrorKind::Corrupt,
                DurabilityClass::ConsistencyFailure,
                DiagnosticCode::LogCorrupt,
                false,
            ),
            (
                SessionLogErrorKind::NotInitialized,
                DurabilityClass::ConsistencyFailure,
                DiagnosticCode::InvalidSessionManifest,
                false,
            ),
            (
                SessionLogErrorKind::AlreadyInitialized,
                DurabilityClass::ConsistencyFailure,
                DiagnosticCode::InvalidSessionManifest,
                false,
            ),
        ] {
            assert_eq!(kind.durability_class(), class);
            assert_eq!(kind.diagnostic_code(), code);
            assert_eq!(kind.retryable(), retryable);
        }
    }
}
