use crate::limits::LoopLimits;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use crate::time::{DeadlineSource, effective_deadline};
use crate::value::BoundedText;

use super::port::{Model, ModelCallContext, ModelDescriptor};
#[cfg(test)]
use super::response::ModelEvent;
use super::{DeliveryState, ModelError, ModelErrorKind};
use super::{ModelRequest, ModelResponse};

mod assembler;
mod failure;

use assembler::Assembler;
pub(crate) use failure::ModelDriverFailure;
use failure::{
    AttemptFailure, RetryPolicySnapshot, generated_error, normalize_after_event, observed_delivery,
    wait_for_retry,
};

pub(crate) const MAX_MODEL_CALL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const MAX_MODEL_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct ModelDriverConfig {
    model_call_timeout: Duration,
    retry_policy: RetryPolicySnapshot,
    limits: LoopLimits,
}

impl ModelDriverConfig {
    pub(crate) fn new(
        model_call_timeout: Duration,
        max_attempts: u8,
        base_delay: Duration,
        limits: LoopLimits,
    ) -> Self {
        Self {
            model_call_timeout,
            retry_policy: RetryPolicySnapshot::new(max_attempts, base_delay),
            limits,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModelDriverProgress {
    TextDelta(BoundedText),
    ReasoningDelta(BoundedText),
}

pub(crate) struct ModelDriver {
    model: Arc<dyn Model>,
    descriptor: ModelDescriptor,
    model_call_timeout: Duration,
    retry_policy: RetryPolicySnapshot,
    limits: LoopLimits,
}

impl ModelDriver {
    #[cfg(test)]
    pub(crate) fn new(
        model: Arc<dyn Model>,
        config: ModelDriverConfig,
    ) -> Result<Self, ModelError> {
        let descriptor = catch_unwind(AssertUnwindSafe(|| model.descriptor().clone()))
            .map_err(|_| generated_error(ModelErrorKind::Panicked, DeliveryState::NotStarted))?;
        descriptor.validate().map_err(|_| {
            generated_error(ModelErrorKind::InvalidRequest, DeliveryState::NotStarted)
        })?;
        Ok(Self::from_validated(model, descriptor, config))
    }

    pub(crate) fn from_validated(
        model: Arc<dyn Model>,
        descriptor: ModelDescriptor,
        config: ModelDriverConfig,
    ) -> Self {
        Self {
            model,
            descriptor,
            model_call_timeout: config.model_call_timeout,
            retry_policy: config.retry_policy,
            limits: config.limits,
        }
    }

    #[cfg(test)]
    pub(crate) async fn run(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::Sender<ModelDriverProgress>,
    ) -> Result<ModelResponse, ModelError> {
        let progress_dropped = AtomicU64::new(0);
        self.run_detailed(request, context, progress, &progress_dropped)
            .await
            .map_err(ModelDriverFailure::into_error)
    }

    pub(crate) async fn run_detailed(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::Sender<ModelDriverProgress>,
        progress_dropped: &AtomicU64,
    ) -> Result<ModelResponse, ModelDriverFailure> {
        let deadline =
            effective_deadline(context.deadline, self.model_call_timeout).map_err(|_| {
                ModelDriverFailure::ordinary(generated_error(
                    ModelErrorKind::InvalidRequest,
                    DeliveryState::NotStarted,
                ))
            })?;
        precheck(&context, deadline.tokio(), deadline.source())?;
        self.validate_request(&request)
            .map_err(ModelDriverFailure::ordinary)?;

        let max_attempts = self.retry_policy.max_attempts;
        for attempt in 0..max_attempts {
            precheck(&context, deadline.tokio(), deadline.source())?;
            let mut attempt_context = context.clone();
            attempt_context.deadline = deadline.standard();
            attempt_context.cancellation = context.cancellation.child_token();
            let _attempt_cancel = attempt_context.cancellation.clone().drop_guard();
            match self
                .run_attempt(
                    &request,
                    attempt_context,
                    deadline.tokio(),
                    deadline.source(),
                    progress,
                    progress_dropped,
                )
                .await
            {
                Ok(response) => return Ok(response),
                Err(failure) => match self.retry_policy.evaluate_retry(
                    attempt,
                    max_attempts,
                    &failure,
                    deadline.tokio(),
                ) {
                    Some(delay) => {
                        wait_for_retry(&context, deadline.tokio(), deadline.source(), delay)
                            .await?;
                    }
                    None => return Err(failure.into_driver_failure()),
                },
            }
        }
        Err(ModelDriverFailure::ordinary(generated_error(
            ModelErrorKind::Internal,
            DeliveryState::NotStarted,
        )))
    }

    fn validate_request(&self, request: &ModelRequest) -> Result<(), ModelError> {
        let invalid = || generated_error(ModelErrorKind::InvalidRequest, DeliveryState::NotStarted);
        if !self.descriptor.supports_reasoning(request.reasoning()) {
            return Err(invalid());
        }
        if !request.tools().is_empty() && !self.descriptor.supports_tools {
            return Err(invalid());
        }
        if request
            .limits()
            .context_window_tokens()
            .is_some_and(|limit| u64::from(limit) > self.descriptor.context_window)
        {
            return Err(invalid());
        }
        Ok(())
    }

    async fn run_attempt(
        &self,
        request: &ModelRequest,
        context: ModelCallContext,
        deadline: TokioInstant,
        deadline_source: DeadlineSource,
        progress: &mpsc::Sender<ModelDriverProgress>,
        progress_dropped: &AtomicU64,
    ) -> Result<ModelResponse, AttemptFailure> {
        let cancellation = context.cancellation.clone();
        let start = catch_unwind(AssertUnwindSafe(|| {
            self.model.start(request.clone(), context)
        }))
        .map_err(|_| {
            AttemptFailure::new(
                generated_error(ModelErrorKind::Panicked, DeliveryState::Unknown),
                false,
            )
        })?;
        let start = AssertUnwindSafe(start).catch_unwind();
        tokio::pin!(start);
        let _start_cancel = cancellation.clone().drop_guard();
        let timeout = tokio::time::sleep_until(deadline);
        tokio::pin!(timeout);
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AttemptFailure::new(
                    generated_error(ModelErrorKind::Cancelled, DeliveryState::Unknown),
                    false,
                ));
            }
            _ = &mut timeout => {
                return Err(AttemptFailure::deadline(
                    generated_error(ModelErrorKind::Timeout, DeliveryState::Unknown),
                    false,
                    deadline_source,
                ));
            }
            result = &mut start => match result {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return Err(AttemptFailure::new(error, false)),
                Err(_) => return Err(AttemptFailure::new(
                    generated_error(ModelErrorKind::Panicked, DeliveryState::Unknown),
                    false,
                )),
            }
        };

        let mut stream = stream;
        let _stream_cancel = cancellation.clone().drop_guard();
        let mut assembler = Assembler::new(request.tools(), &self.limits);
        loop {
            let next = AssertUnwindSafe(stream.next()).catch_unwind();
            tokio::pin!(next);
            let item = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(AttemptFailure::new(
                        generated_error(
                            ModelErrorKind::Cancelled,
                            observed_delivery(assembler.observed_event),
                        ),
                        assembler.observed_event,
                    ));
                }
                _ = &mut timeout => {
                    return Err(AttemptFailure::deadline(
                        generated_error(
                            ModelErrorKind::Timeout,
                            observed_delivery(assembler.observed_event),
                        ),
                        assembler.observed_event,
                        deadline_source,
                    ));
                }
                result = &mut next => result,
            };
            match item {
                Ok(Some(Ok(event))) => {
                    assembler.observed_event = true;
                    let progress_event = assembler
                        .push(event)
                        .map_err(|error| AttemptFailure::new(error, assembler.observed_event))?;
                    if let Some(progress_event) = progress_event {
                        match progress.try_send(progress_event) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                progress_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {}
                        }
                    }
                }
                Ok(Some(Err(error))) => {
                    let error = normalize_after_event(error, assembler.observed_event);
                    return Err(AttemptFailure::new(error, assembler.observed_event));
                }
                Ok(None) => {
                    let observed_event = assembler.observed_event;
                    return assembler
                        .finish()
                        .map_err(|error| AttemptFailure::new(error, observed_event));
                }
                Err(_) => {
                    return Err(AttemptFailure::new(
                        generated_error(
                            ModelErrorKind::Panicked,
                            observed_delivery(assembler.observed_event),
                        ),
                        assembler.observed_event,
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

fn precheck(
    context: &ModelCallContext,
    deadline: TokioInstant,
    source: DeadlineSource,
) -> Result<(), ModelDriverFailure> {
    if context.cancellation.is_cancelled() {
        return Err(ModelDriverFailure::ordinary(generated_error(
            ModelErrorKind::Cancelled,
            DeliveryState::NotStarted,
        )));
    }
    if TokioInstant::now() >= deadline {
        return Err(ModelDriverFailure::deadline(
            generated_error(ModelErrorKind::Timeout, DeliveryState::NotStarted),
            source,
        ));
    }
    Ok(())
}
