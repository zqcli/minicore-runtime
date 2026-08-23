use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use tokio::time::Instant as TokioInstant;

use crate::config::SemanticLimits;

use super::{ContextBundle, ContextError, ContextProvider, ContextRequest};

const MAX_CONTEXT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

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

    pub(crate) async fn provide(
        &self,
        mut request: ContextRequest,
    ) -> Result<ContextBundle, ContextError> {
        let Some(provider) = self.provider.as_ref() else {
            return ContextBundle { blocks: Vec::new() }.validate_and_sort(&self.limits);
        };
        let cancellation = request.cancellation.clone();
        if cancellation.is_cancelled() {
            return Err(ContextError::Cancelled);
        }
        let (deadline, adapter_deadline) =
            effective_deadline(request.deadline, self.context_timeout)?;
        if TokioInstant::now() >= deadline {
            return Err(ContextError::DeadlineExceeded);
        }
        request.deadline = adapter_deadline;
        let future = catch_unwind(AssertUnwindSafe(|| provider.provide(request)))
            .map_err(|_| ContextError::Internal)?;
        let future = AssertUnwindSafe(future).catch_unwind();
        tokio::pin!(future);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ContextError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => {
                return Err(ContextError::DeadlineExceeded);
            }
            result = &mut future => result,
        };
        match result {
            Ok(Ok(bundle)) => bundle.validate_and_sort(&self.limits),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(ContextError::Internal),
        }
    }
}

fn effective_deadline(
    request_deadline: Instant,
    timeout: Duration,
) -> Result<(TokioInstant, Instant), ContextError> {
    let configured = TokioInstant::now()
        .checked_add(timeout)
        .ok_or(ContextError::DeadlineExceeded)?;
    let deadline = TokioInstant::from_std(request_deadline).min(configured);
    Ok((deadline, deadline.into_std()))
}

#[cfg(test)]
mod tests;
