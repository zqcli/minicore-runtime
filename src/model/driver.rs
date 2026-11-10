use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use crate::time::{DeadlineSource, effective_deadline};
use crate::value::Text;

use super::port::{Model, ModelCallContext, ModelDescriptor};
#[cfg(test)]
use super::response::ModelEvent;
use super::{DeliveryState, ModelError, ModelErrorKind};
use super::{ModelRequest, ModelResponse, ModelRetryPhase, ModelRetryProgress};

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
}

impl ModelDriverConfig {
    pub(crate) fn new(
        model_call_timeout: Duration,
        max_attempts: u8,
        base_delay: Duration,
    ) -> Self {
        Self {
            model_call_timeout,
            retry_policy: RetryPolicySnapshot::new(max_attempts, base_delay),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModelDriverProgress {
    TextDelta(Text),
    ReasoningDelta(Text),
}

pub(crate) enum ModelDriverUpdate {
    AttemptStarted(u8),
    RetryWaiting(ModelRetryProgress),
    Finished(Duration),
}

type StateUpdate<'a> = &'a mut (dyn FnMut(ModelDriverUpdate) + Send);

pub(crate) struct ModelDriver {
    model: Arc<dyn Model>,
    descriptor: ModelDescriptor,
    model_call_timeout: Duration,
    retry_policy: RetryPolicySnapshot,
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
        }
    }

    #[cfg(test)]
    pub(crate) async fn run(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::UnboundedSender<ModelDriverProgress>,
    ) -> Result<ModelResponse, ModelError> {
        self.run_detailed(request, context, progress)
            .await
            .map_err(ModelDriverFailure::into_error)
    }

    #[cfg(test)]
    pub(crate) async fn run_detailed(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::UnboundedSender<ModelDriverProgress>,
    ) -> Result<ModelResponse, ModelDriverFailure> {
        self.run_observed(request, context, progress, &mut |_| {})
            .await
    }

    pub(crate) async fn run_observed(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::UnboundedSender<ModelDriverProgress>,
        update: StateUpdate<'_>,
    ) -> Result<ModelResponse, ModelDriverFailure> {
        let mut retry_wait_elapsed = Duration::ZERO;
        let result = self
            .run_requests(request, context, progress, update, &mut retry_wait_elapsed)
            .await;
        update(ModelDriverUpdate::Finished(retry_wait_elapsed));
        result
    }

    async fn run_requests(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::UnboundedSender<ModelDriverProgress>,
        update: StateUpdate<'_>,
        retry_wait_elapsed: &mut Duration,
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

        let mut attempt = 0;
        loop {
            precheck(&context, deadline.tokio(), deadline.source())?;
            let mut attempt_context = context.clone();
            attempt_context.deadline = deadline.standard();
            attempt_context.cancellation = context.cancellation.child_token();
            let _attempt_cancel = attempt_context.cancellation.clone().drop_guard();
            let start = catch_unwind(AssertUnwindSafe(|| {
                self.model.start(request.clone(), attempt_context.clone())
            }));
            update(ModelDriverUpdate::AttemptStarted(attempt + 1));
            let start = start.map_err(|_| {
                ModelDriverFailure::ordinary(generated_error(
                    ModelErrorKind::Panicked,
                    DeliveryState::Unknown,
                ))
            })?;
            match self
                .run_attempt(
                    &request,
                    attempt_context,
                    deadline.source(),
                    start,
                    progress,
                )
                .await
            {
                Ok(response) => return Ok(response),
                Err(failure) => {
                    match self
                        .retry_policy
                        .evaluate_retry(attempt, &failure, deadline.tokio())
                    {
                        Some(delay) => {
                            let retry_deadline = TokioInstant::now() + delay;
                            update(ModelDriverUpdate::RetryWaiting(ModelRetryProgress {
                                attempt: attempt + 2,
                                max_attempts: self.retry_policy.max_attempts,
                                error: failure.error.clone(),
                                phase: ModelRetryPhase::Waiting {
                                    retry_at: SystemTime::now() + delay,
                                    retry_deadline,
                                },
                            }));
                            let wait_started = TokioInstant::now();
                            let waited = wait_for_retry(
                                &context,
                                deadline.tokio(),
                                deadline.source(),
                                retry_deadline,
                                failure.error,
                            )
                            .await;
                            *retry_wait_elapsed += wait_started.elapsed();
                            waited?;
                            attempt += 1;
                        }
                        None => return Err(failure.into_driver_failure()),
                    }
                }
            }
        }
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
        deadline_source: DeadlineSource,
        start: super::ModelStartFuture<'_>,
        progress: &mpsc::UnboundedSender<ModelDriverProgress>,
    ) -> Result<ModelResponse, AttemptFailure> {
        let cancellation = context.cancellation.clone();
        let request_index = context.request_index;
        let deadline = TokioInstant::from_std(context.deadline);
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
        let mut assembler = Assembler::new(request.tools());
        let result = async {
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
                        let progress_event = assembler.push(event).map_err(|error| {
                            AttemptFailure::new(error, assembler.observed_event)
                        })?;
                        if let Some(progress_event) = progress_event {
                            let _ = progress.send(progress_event);
                            // Let the runner forward this delta before polling another ready event.
                            tokio::task::yield_now().await;
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
        .await;
        result.map_err(|failure: AttemptFailure| {
            failure.with_partial(assembler.into_partial(request_index))
        })
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
