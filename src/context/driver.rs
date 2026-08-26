use std::sync::Arc;
use std::time::Duration;

use crate::config::SemanticLimits;
use crate::port_call::{PortCallOutcome, run_port_call};
use crate::time::DeadlineSource;

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
        let provider = Arc::clone(provider);
        match run_port_call(
            &cancellation,
            request.deadline,
            self.context_timeout,
            |cancellation, deadline| {
                request.cancellation = cancellation;
                request.deadline = deadline;
                provider.provide(request)
            },
        )
        .await
        {
            PortCallOutcome::Returned(Ok(bundle)) => {
                Self::validate_bundle(bundle, &self.limits).map_err(ContextDriverFailure::ordinary)
            }
            PortCallOutcome::Returned(Err(error)) => Err(ContextDriverFailure::ordinary(error)),
            PortCallOutcome::Cancelled => {
                Err(ContextDriverFailure::ordinary(ContextError::Cancelled))
            }
            PortCallOutcome::DeadlineExceeded(source) => {
                Err(ContextDriverFailure::deadline(source))
            }
            PortCallOutcome::InvalidDeadline(_) | PortCallOutcome::Panicked => {
                Err(ContextDriverFailure::ordinary(ContextError::Internal))
            }
        }
    }
}

#[cfg(test)]
mod tests;
