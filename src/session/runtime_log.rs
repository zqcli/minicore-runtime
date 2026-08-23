use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::{SessionManifest, Timestamp};
use crate::conversation::{
    ConversationCloseOutcome, ConversationEntry, ConversationSeq, TimestampSource,
    close_unopened_log,
};
use crate::error::{
    DiagnosticCategory, DiagnosticCode, DiagnosticSummary, SessionLogError, SessionLogErrorKind,
    SessionOpenError,
};
use crate::storage::{AppendReceipt, ConversationPage, LogFuture, SessionLog};

pub(super) fn cancellable_log(
    log: Box<dyn SessionLog>,
    cancel: CancellationToken,
) -> Box<dyn SessionLog> {
    Box::new(OpenCancellationLog { inner: log, cancel })
}

pub(super) async fn close_raw(
    log: Box<dyn SessionLog>,
    timeout: Duration,
    primary: SessionOpenError,
) -> SessionOpenError {
    with_secondary(primary, close_unopened_log(log, timeout).await)
}

pub(super) fn with_secondary(
    primary: SessionOpenError,
    secondary: Option<ConversationCloseOutcome>,
) -> SessionOpenError {
    primary.with_secondary_diagnostic(secondary.map(secondary_diagnostic))
}

pub(super) fn synthetic_log_error(kind: SessionLogErrorKind) -> SessionLogError {
    SessionLogError::new(
        kind,
        DiagnosticSummary::bounded_static(
            log_code(kind),
            DiagnosticCategory::Storage,
            "session log violated the owner lifecycle contract",
            false,
        ),
    )
}

fn secondary_diagnostic(outcome: ConversationCloseOutcome) -> DiagnosticSummary {
    match outcome {
        ConversationCloseOutcome::Known(error) => DiagnosticSummary::bounded_static(
            log_code(error.kind()),
            DiagnosticCategory::Storage,
            "session log close failed after an open failure",
            false,
        ),
        ConversationCloseOutcome::Timeout => DiagnosticSummary::bounded_static(
            DiagnosticCode::LogUnknownOutcome,
            DiagnosticCategory::Storage,
            "session log close timed out after an open failure",
            false,
        ),
        ConversationCloseOutcome::Panic => DiagnosticSummary::bounded_static(
            DiagnosticCode::Internal,
            DiagnosticCategory::Storage,
            "session log close panicked after an open failure",
            false,
        ),
    }
}

fn log_code(kind: SessionLogErrorKind) -> DiagnosticCode {
    match kind {
        SessionLogErrorKind::Conflict => DiagnosticCode::LogConflict,
        SessionLogErrorKind::Corrupt => DiagnosticCode::LogCorrupt,
        SessionLogErrorKind::UnknownOutcome => DiagnosticCode::LogUnknownOutcome,
        SessionLogErrorKind::NotInitialized | SessionLogErrorKind::AlreadyInitialized => {
            DiagnosticCode::InvalidSessionManifest
        }
        SessionLogErrorKind::Closed => DiagnosticCode::SessionClosed,
        SessionLogErrorKind::Unavailable | SessionLogErrorKind::Internal => {
            DiagnosticCode::Internal
        }
    }
}

struct OpenCancellationLog {
    inner: Box<dyn SessionLog>,
    cancel: CancellationToken,
}

impl OpenCancellationLog {
    fn cancelled<'a, T>() -> LogFuture<'a, T> {
        Box::pin(async { Err(synthetic_log_error(SessionLogErrorKind::Closed)) })
    }
}

impl SessionLog for OpenCancellationLog {
    fn initialize<'a>(&'a mut self, manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        if self.cancel.is_cancelled() {
            Self::cancelled()
        } else {
            self.inner.initialize(manifest)
        }
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        if self.cancel.is_cancelled() {
            Self::cancelled()
        } else {
            self.inner.load_manifest()
        }
    }

    fn read_page<'a>(
        &'a mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        if self.cancel.is_cancelled() {
            Self::cancelled()
        } else {
            self.inner.read_page(after, limit)
        }
    }

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        self.inner.append(expected_head, entries)
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        self.inner.close()
    }
}

pub(super) fn timestamp_source() -> TimestampSource {
    Box::new(Timestamp::now_utc)
}
