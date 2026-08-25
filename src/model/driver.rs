use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use crate::time::{DeadlineSource, effective_deadline};
use crate::tools::ToolName;
use crate::value::{BoundedText, MAX_JSON_BYTES};

use super::model_port::{Model, ModelCallContext, ModelDescriptor};
#[cfg(test)]
use super::response::ModelEvent;
use super::response::{DeliveryState, ModelError, ModelErrorKind};
use super::types::{ModelRequest, ModelResponse};

mod assembler;
mod failure;

use assembler::Assembler;
pub(crate) use failure::ModelDriverFailure;
use failure::{
    AttemptFailure, RetryPolicySnapshot, generated_error, normalize_after_event, observed_delivery,
    wait_for_retry,
};

const MAX_MODEL_CALL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct ModelDriverConfig {
    model_call_timeout: Duration,
    retry_policy: RetryPolicySnapshot,
    limits: SemanticLimitsSnapshot,
}

#[derive(Clone)]
pub(crate) struct SemanticLimitsSnapshot {
    max_tool_count: usize,
    max_tool_name_bytes: usize,
    max_tool_schema_bytes: usize,
    max_tool_input_bytes: usize,
    max_model_text_bytes_per_round: usize,
    max_model_reasoning_bytes_per_round: usize,
}

impl ModelDriverConfig {
    pub(crate) fn from_kernel_values(
        model_call_timeout: Duration,
        max_attempts: u8,
        base_delay: Duration,
        limits: SemanticLimitsSnapshot,
    ) -> Self {
        Self {
            model_call_timeout,
            retry_policy: RetryPolicySnapshot::new(max_attempts, base_delay),
            limits,
        }
    }

    fn validate(&self) -> bool {
        !self.model_call_timeout.is_zero()
            && self.model_call_timeout <= MAX_MODEL_CALL_TIMEOUT
            && (1..=4).contains(&self.retry_policy.max_attempts)
            && self.retry_policy.base_delay <= MAX_RETRY_DELAY
            && self.limits.valid()
    }
}

impl SemanticLimitsSnapshot {
    pub(crate) fn from_kernel_values(
        max_tool_count: usize,
        max_tool_name_bytes: usize,
        max_tool_schema_bytes: usize,
        max_tool_input_bytes: usize,
        max_model_text_bytes_per_round: usize,
        max_model_reasoning_bytes_per_round: usize,
    ) -> Self {
        Self {
            max_tool_count,
            max_tool_name_bytes,
            max_tool_schema_bytes,
            max_tool_input_bytes,
            max_model_text_bytes_per_round,
            max_model_reasoning_bytes_per_round,
        }
    }

    fn valid(&self) -> bool {
        (1..=4_096).contains(&self.max_tool_count)
            && (1..=64).contains(&self.max_tool_name_bytes)
            && (1..=MAX_JSON_BYTES).contains(&self.max_tool_schema_bytes)
            && (1..=MAX_JSON_BYTES).contains(&self.max_tool_input_bytes)
            && (1..=BoundedText::MAX_BYTES).contains(&self.max_model_text_bytes_per_round)
            && (1..=BoundedText::MAX_BYTES).contains(&self.max_model_reasoning_bytes_per_round)
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
    limits: SemanticLimitsSnapshot,
}

impl ModelDriver {
    pub(crate) fn new(
        model: Arc<dyn Model>,
        config: ModelDriverConfig,
    ) -> Result<Self, ModelError> {
        if !config.validate() {
            return Err(generated_error(
                ModelErrorKind::InvalidRequest,
                DeliveryState::NotStarted,
            ));
        }
        let descriptor = catch_unwind(AssertUnwindSafe(|| model.descriptor().clone()))
            .map_err(|_| generated_error(ModelErrorKind::Panicked, DeliveryState::NotStarted))?;
        descriptor.validate().map_err(|_| {
            generated_error(ModelErrorKind::InvalidRequest, DeliveryState::NotStarted)
        })?;
        Ok(Self {
            model,
            descriptor,
            model_call_timeout: config.model_call_timeout,
            retry_policy: config.retry_policy,
            limits: config.limits,
        })
    }

    #[cfg(test)]
    pub(crate) async fn run(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::Sender<ModelDriverProgress>,
    ) -> Result<ModelResponse, ModelError> {
        self.run_detailed(request, context, progress)
            .await
            .map_err(ModelDriverFailure::into_error)
    }

    pub(crate) async fn run_detailed(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        progress: &mpsc::Sender<ModelDriverProgress>,
    ) -> Result<ModelResponse, ModelDriverFailure> {
        let deadline =
            effective_deadline(context.deadline, self.model_call_timeout).map_err(|_| {
                ModelDriverFailure::ordinary(generated_error(
                    ModelErrorKind::InvalidRequest,
                    DeliveryState::NotStarted,
                ))
            })?;
        if context.cancellation.is_cancelled() {
            return Err(ModelDriverFailure::ordinary(generated_error(
                ModelErrorKind::Cancelled,
                DeliveryState::NotStarted,
            )));
        }
        if TokioInstant::now() >= deadline.tokio() {
            return Err(ModelDriverFailure::deadline(
                generated_error(ModelErrorKind::Timeout, DeliveryState::NotStarted),
                deadline.source(),
            ));
        }
        self.validate_request(&request)
            .map_err(ModelDriverFailure::ordinary)?;

        let max_attempts = self.retry_policy.max_attempts;
        for attempt in 0..max_attempts {
            if context.cancellation.is_cancelled() {
                return Err(ModelDriverFailure::ordinary(generated_error(
                    ModelErrorKind::Cancelled,
                    DeliveryState::NotStarted,
                )));
            }
            if TokioInstant::now() >= deadline.tokio() {
                return Err(ModelDriverFailure::deadline(
                    generated_error(ModelErrorKind::Timeout, DeliveryState::NotStarted),
                    deadline.source(),
                ));
            }
            let mut attempt_context = context.clone();
            attempt_context.deadline = deadline.standard();
            match self
                .run_attempt(
                    request.clone(),
                    attempt_context,
                    deadline.tokio(),
                    deadline.source(),
                    progress,
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
        self.descriptor.validate().map_err(|_| invalid())?;
        if !self.descriptor.supports_reasoning(request.reasoning()) {
            return Err(invalid());
        }
        if request.tools().len() > self.limits.max_tool_count
            || (!request.tools().is_empty() && !self.descriptor.supports_tools)
        {
            return Err(invalid());
        }
        if request
            .limits()
            .context_window_tokens()
            .is_some_and(|limit| u64::from(limit) > self.descriptor.context_window)
        {
            return Err(invalid());
        }
        let mut previous: Option<&ToolName> = None;
        for tool in request.tools() {
            tool.validate_for_bindings(
                self.limits.max_tool_name_bytes,
                self.limits.max_tool_schema_bytes,
            )
            .map_err(|_| invalid())?;
            if previous.is_some_and(|previous| previous >= tool.name()) {
                return Err(invalid());
            }
            previous = Some(tool.name());
        }
        Ok(())
    }

    async fn run_attempt(
        &self,
        request: ModelRequest,
        context: ModelCallContext,
        deadline: TokioInstant,
        deadline_source: DeadlineSource,
        progress: &mpsc::Sender<ModelDriverProgress>,
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
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AttemptFailure::new(
                    generated_error(ModelErrorKind::Cancelled, DeliveryState::Unknown),
                    false,
                ));
            }
            _ = tokio::time::sleep_until(deadline) => {
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
                _ = tokio::time::sleep_until(deadline) => {
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
                        let _ = progress.try_send(progress_event);
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
