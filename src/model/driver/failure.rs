use std::time::Duration;

use tokio::time::Instant as TokioInstant;

use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::time::DeadlineSource;
use crate::value::BoundedText;

use super::super::model_port::ModelCallContext;
use super::super::response::{DeliveryState, ModelError, ModelErrorKind, RetryHint};

const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub(super) struct RetryPolicySnapshot {
    pub(super) max_attempts: u8,
    pub(super) base_delay: Duration,
}

impl RetryPolicySnapshot {
    pub(super) fn new(max_attempts: u8, base_delay: Duration) -> Self {
        Self {
            max_attempts,
            base_delay,
        }
    }

    pub(super) fn delay_for_retry(
        self,
        retry_index: u8,
        retry_after: Option<Duration>,
    ) -> Option<Duration> {
        let mut exponential = self.base_delay;
        for _ in 0..retry_index {
            exponential = exponential
                .checked_mul(2)
                .unwrap_or(MAX_RETRY_DELAY)
                .min(MAX_RETRY_DELAY);
        }
        match retry_after {
            Some(value) if value > MAX_RETRY_DELAY => None,
            Some(value) => Some(value.max(exponential)),
            None => Some(exponential),
        }
    }

    pub(super) fn evaluate_retry(
        self,
        attempt: u8,
        max_attempts: u8,
        failure: &AttemptFailure,
        deadline: TokioInstant,
    ) -> Option<Duration> {
        if attempt + 1 >= max_attempts
            || failure.observed_event
            || failure.error.delivery() != DeliveryState::NotStarted
        {
            return None;
        }
        let retry_after = match failure.error.retry_hint() {
            RetryHint::Retryable { retry_after } => *retry_after,
            RetryHint::Never => return None,
        };
        let delay = self.delay_for_retry(attempt, retry_after)?;
        if retry_fits(deadline, delay) {
            Some(delay)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelDriverFailure {
    pub(super) error: ModelError,
    pub(super) deadline_source: Option<DeadlineSource>,
}

impl ModelDriverFailure {
    pub(crate) fn error(&self) -> &ModelError {
        &self.error
    }

    pub(crate) fn into_error(self) -> ModelError {
        self.error
    }

    pub(crate) const fn deadline_source(&self) -> Option<DeadlineSource> {
        self.deadline_source
    }

    pub(super) fn ordinary(error: ModelError) -> Self {
        Self {
            error,
            deadline_source: None,
        }
    }

    pub(super) fn deadline(error: ModelError, source: DeadlineSource) -> Self {
        Self {
            error,
            deadline_source: Some(source),
        }
    }
}

pub(super) struct AttemptFailure {
    pub(super) error: ModelError,
    pub(super) observed_event: bool,
    pub(super) deadline_source: Option<DeadlineSource>,
}

impl AttemptFailure {
    pub(super) fn new(error: ModelError, observed_event: bool) -> Self {
        Self {
            error,
            observed_event,
            deadline_source: None,
        }
    }

    pub(super) fn deadline(
        error: ModelError,
        observed_event: bool,
        source: DeadlineSource,
    ) -> Self {
        Self {
            error,
            observed_event,
            deadline_source: Some(source),
        }
    }

    pub(super) fn into_driver_failure(self) -> ModelDriverFailure {
        ModelDriverFailure {
            error: self.error,
            deadline_source: self.deadline_source,
        }
    }
}

pub(super) fn observed_delivery(observed_event: bool) -> DeliveryState {
    if observed_event {
        DeliveryState::Started
    } else {
        DeliveryState::Unknown
    }
}

pub(super) fn normalize_after_event(error: ModelError, observed_event: bool) -> ModelError {
    if observed_event
        && (error.delivery() != DeliveryState::Started || *error.retry_hint() != RetryHint::Never)
    {
        ModelError::started(error.kind(), error.diagnostic().clone())
    } else {
        error
    }
}

pub(super) fn generated_error(kind: ModelErrorKind, delivery: DeliveryState) -> ModelError {
    let (code, msg) = match kind {
        ModelErrorKind::Cancelled => (DiagnosticCode::RuntimeTerminated, "model request cancelled"),
        ModelErrorKind::Timeout => (DiagnosticCode::ModelTimeout, "model request timed out"),
        ModelErrorKind::Panicked => (DiagnosticCode::Internal, "model driver panicked"),
        ModelErrorKind::InvalidRequest => (
            DiagnosticCode::InvalidConfiguration,
            "invalid model request",
        ),
        ModelErrorKind::InvalidProviderResponse
        | ModelErrorKind::UnexpectedToolCall
        | ModelErrorKind::IncompleteResponse => (
            DiagnosticCode::ModelMalformedResponse,
            "invalid model provider response",
        ),
        ModelErrorKind::ProviderUnavailable | ModelErrorKind::Unavailable => (
            DiagnosticCode::ModelUnavailable,
            "model provider unavailable",
        ),
        _ => (DiagnosticCode::Internal, "model operation failed"),
    };
    let diagnostic = DiagnosticSummary::new(
        code,
        DiagnosticCategory::Model,
        BoundedText::new(msg).expect("static diagnostic must fit BoundedText"),
        false,
    );
    match delivery {
        DeliveryState::NotStarted => {
            ModelError::permanent(kind, DeliveryState::NotStarted, diagnostic)
        }
        DeliveryState::Started => ModelError::started(kind, diagnostic),
        DeliveryState::Unknown => ModelError::unknown(kind, diagnostic),
    }
}

pub(super) fn retry_fits(deadline: TokioInstant, delay: Duration) -> bool {
    TokioInstant::now()
        .checked_add(delay)
        .is_some_and(|retry_at| retry_at < deadline)
}

pub(super) async fn wait_for_retry(
    context: &ModelCallContext,
    deadline: TokioInstant,
    deadline_source: DeadlineSource,
    delay: Duration,
) -> Result<(), ModelDriverFailure> {
    let retry_at = TokioInstant::now().checked_add(delay).ok_or_else(|| {
        ModelDriverFailure::ordinary(generated_error(
            ModelErrorKind::InvalidRequest,
            DeliveryState::NotStarted,
        ))
    })?;
    tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => Err(ModelDriverFailure::ordinary(generated_error(
            ModelErrorKind::Cancelled,
            DeliveryState::NotStarted,
        ))),
        _ = tokio::time::sleep_until(deadline) => Err(ModelDriverFailure::deadline(
            generated_error(ModelErrorKind::Timeout, DeliveryState::NotStarted),
            deadline_source,
        )),
        _ = tokio::time::sleep_until(retry_at) => Ok(()),
    }
}
