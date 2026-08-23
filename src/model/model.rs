use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::ids::{SessionId, SessionInstanceId, TurnId};

use super::response::{ModelError, ModelEvent};
use super::types::{ModelRef, ModelRequest, ModelValueError, ReasoningPreference};

#[derive(Clone, Eq, PartialEq)]
pub struct ModelDescriptor {
    pub model_ref: ModelRef,
    pub context_window: u64,
    pub supported_reasoning: BTreeSet<ReasoningPreference>,
    pub supports_tools: bool,
}

impl ModelDescriptor {
    pub fn new(
        model_ref: ModelRef,
        context_window: u64,
        supported_reasoning: BTreeSet<ReasoningPreference>,
        supports_tools: bool,
    ) -> Result<Self, ModelValueError> {
        let descriptor = Self {
            model_ref,
            context_window,
            supported_reasoning,
            supports_tools,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<(), ModelValueError> {
        if self.context_window == 0 || self.supported_reasoning.is_empty() {
            return Err(ModelValueError::InvalidDescriptor);
        }
        Ok(())
    }

    pub fn supports_reasoning(&self, preference: ReasoningPreference) -> bool {
        self.supported_reasoning.contains(&preference)
    }
}

impl fmt::Debug for ModelDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelDescriptor")
            .field("model_ref", &self.model_ref)
            .field("context_window", &self.context_window)
            .field("supported_reasoning", &self.supported_reasoning)
            .field("supports_tools", &self.supports_tools)
            .finish()
    }
}

#[derive(Clone)]
pub struct ModelCallContext {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub turn_id: TurnId,
    /// Zero-based model call round within the current Turn.
    pub round: u16,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

impl ModelCallContext {
    pub fn new(
        session_id: SessionId,
        instance_id: SessionInstanceId,
        turn_id: TurnId,
        round: u16,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Self {
        Self {
            session_id,
            instance_id,
            turn_id,
            round,
            cancellation,
            deadline,
        }
    }
}

impl fmt::Debug for ModelCallContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCallContext")
            .field("session_id", &self.session_id)
            .field("instance_id", &self.instance_id)
            .field("turn_id", &self.turn_id)
            .field("round", &self.round)
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("deadline", &self.deadline)
            .finish()
    }
}

pub type ModelStartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelStream, ModelError>> + Send + 'a>>;

pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send + 'static>>;

pub trait Model: Send + Sync + 'static {
    fn descriptor(&self) -> &ModelDescriptor;

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a>;
}

impl<T: Model + ?Sized> Model for Arc<T> {
    fn descriptor(&self) -> &ModelDescriptor {
        (**self).descriptor()
    }

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        (**self).start(request, context)
    }
}
