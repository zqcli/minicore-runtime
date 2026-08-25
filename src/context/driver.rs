use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::time::Instant as TokioInstant;

use crate::config::SemanticLimits;
use crate::time::{DeadlineSource, effective_deadline};

use super::{ContextBlock, ContextBundle, ContextError, ContextProvider, ContextRequest};

const MAX_CONTEXT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ValidatedContextBundle {
    blocks: Vec<ContextBlock>,
}

impl ValidatedContextBundle {
    pub(crate) fn blocks(&self) -> &[ContextBlock] {
        &self.blocks
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ContextDriverFailure {
    error: ContextError,
    deadline_source: Option<DeadlineSource>,
}

impl ContextDriverFailure {
    pub(crate) const fn error(self) -> ContextError {
        self.error
    }

    pub(crate) const fn deadline_source(self) -> Option<DeadlineSource> {
        self.deadline_source
    }

    const fn ordinary(error: ContextError) -> Self {
        Self {
            error,
            deadline_source: None,
        }
    }

    const fn deadline(source: DeadlineSource) -> Self {
        Self {
            error: ContextError::DeadlineExceeded,
            deadline_source: Some(source),
        }
    }
}

pub(crate) struct ContextDriver {
    provider: Option<Arc<dyn ContextProvider>>,
    context_timeout: Duration,
    limits: SemanticLimits,
}

impl ContextDriver {
    pub(crate) fn new(
        provider: Option<Arc<dyn ContextProvider>>,
        context_timeout: Duration,
        limits: SemanticLimits,
    ) -> Result<Self, ContextError> {
        if context_timeout.is_zero()
            || context_timeout > MAX_CONTEXT_TIMEOUT
            || limits.validate().is_err()
        {
            return Err(ContextError::InvalidLimits);
        }
        Ok(Self {
            provider,
            context_timeout,
            limits,
        })
    }

    pub(crate) fn empty_bundle() -> ValidatedContextBundle {
        ValidatedContextBundle { blocks: Vec::new() }
    }

    fn validate_bundle(
        bundle: ContextBundle,
        limits: &SemanticLimits,
    ) -> Result<ValidatedContextBundle, ContextError> {
        let bundle = bundle.validate_and_sort(limits)?;
        Ok(ValidatedContextBundle {
            blocks: bundle.blocks,
        })
    }

    #[cfg(test)]
    pub(crate) fn validated_for_tests(
        bundle: ContextBundle,
        limits: &SemanticLimits,
    ) -> Result<ValidatedContextBundle, ContextError> {
        Self::validate_bundle(bundle, limits)
    }

    #[cfg(test)]
    pub(crate) async fn provide(
        &self,
        request: ContextRequest,
    ) -> Result<ValidatedContextBundle, ContextError> {
        self.provide_detailed(request)
            .await
            .map_err(ContextDriverFailure::error)
    }

    pub(crate) async fn provide_detailed(
        &self,
        mut request: ContextRequest,
    ) -> Result<ValidatedContextBundle, ContextDriverFailure> {
        let Some(provider) = self.provider.as_ref() else {
            return Ok(Self::empty_bundle());
        };
        let cancellation = request.cancellation.clone();
        if cancellation.is_cancelled() {
            return Err(ContextDriverFailure::ordinary(ContextError::Cancelled));
        }
        let deadline = effective_deadline(request.deadline, self.context_timeout)
            .map_err(|_| ContextDriverFailure::ordinary(ContextError::Internal))?;
        if TokioInstant::now() >= deadline.tokio() {
            return Err(ContextDriverFailure::deadline(deadline.source()));
        }
        request.deadline = deadline.standard();
        let future = catch_unwind(AssertUnwindSafe(|| provider.provide(request)))
            .map_err(|_| ContextDriverFailure::ordinary(ContextError::Internal))?;
        let future = AssertUnwindSafe(future).catch_unwind();
        tokio::pin!(future);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(ContextDriverFailure::ordinary(ContextError::Cancelled));
            }
            _ = tokio::time::sleep_until(deadline.tokio()) => {
                return Err(ContextDriverFailure::deadline(deadline.source()));
            }
            result = &mut future => result,
        };
        match result {
            Ok(Ok(bundle)) => {
                Self::validate_bundle(bundle, &self.limits).map_err(ContextDriverFailure::ordinary)
            }
            Ok(Err(error)) => Err(ContextDriverFailure::ordinary(error)),
            Err(_) => Err(ContextDriverFailure::ordinary(ContextError::Internal)),
        }
    }
}

#[cfg(test)]
mod tests;
