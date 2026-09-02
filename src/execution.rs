use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{Model, ModelDescriptor, ReasoningPreference};
use crate::prompt_provider::PromptProvider;
use crate::tools::{ToolPolicy, ToolSet};

/// Monotonic revision of the active `ExecutionConfig` inside a loop.
///
/// The initial config is revision 0; every accepted `LoopHandle::update`
/// yields the next revision. A request and the tool batch it produces share
/// exactly one revision.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ConfigRevision(u64);

impl ConfigRevision {
    /// Revision assigned to the initial `ExecutionConfig` of a loop.
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ExecutionConfigError {
    #[error("model descriptor access panicked")]
    ModelDescriptorPanicked,
    #[error("model descriptor is invalid")]
    InvalidDescriptor,
    #[error("model does not support the requested reasoning preference")]
    UnsupportedReasoning,
    #[error("model does not support tools but tools are configured")]
    ToolsUnsupported,
    #[error("tool set contains invalid tool specs")]
    InvalidTools,
}

/// Immutable capability snapshot for one agent-loop request.
///
/// The entire `ExecutionConfig` is replaced atomically at request boundaries
/// by `LoopHandle::update`; there are no partial setters for model, reasoning,
/// tools, policy, or prompt.
#[derive(Clone)]
pub struct ExecutionConfig {
    model: Arc<dyn Model>,
    descriptor: ModelDescriptor,
    reasoning: ReasoningPreference,
    tools: ToolSet,
    policy: Option<Arc<dyn ToolPolicy>>,
    prompt: Arc<dyn PromptProvider>,
}

impl ExecutionConfig {
    pub fn new(
        model: Arc<dyn Model>,
        reasoning: ReasoningPreference,
        tools: ToolSet,
        policy: Option<Arc<dyn ToolPolicy>>,
        prompt: Arc<dyn PromptProvider>,
    ) -> Result<Self, ExecutionConfigError> {
        let descriptor = match catch_unwind(AssertUnwindSafe(|| model.descriptor().clone())) {
            Ok(descriptor) => descriptor,
            Err(_) => return Err(ExecutionConfigError::ModelDescriptorPanicked),
        };
        descriptor
            .validate()
            .map_err(|_| ExecutionConfigError::InvalidDescriptor)?;
        if !descriptor.supports_reasoning(reasoning) {
            return Err(ExecutionConfigError::UnsupportedReasoning);
        }
        let tool_specs: Vec<_> = tools.frozen_specs().collect();
        if !tool_specs.is_empty() && !descriptor.supports_tools {
            return Err(ExecutionConfigError::ToolsUnsupported);
        }
        for spec in &tool_specs {
            spec.validate()
                .map_err(|_| ExecutionConfigError::InvalidTools)?;
        }
        Ok(Self {
            model,
            descriptor,
            reasoning,
            tools,
            policy,
            prompt,
        })
    }

    pub fn model(&self) -> &Arc<dyn Model> {
        &self.model
    }

    pub fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    pub fn reasoning(&self) -> ReasoningPreference {
        self.reasoning
    }

    pub fn tools(&self) -> &ToolSet {
        &self.tools
    }

    pub fn policy(&self) -> Option<&Arc<dyn ToolPolicy>> {
        self.policy.as_ref()
    }

    pub fn prompt(&self) -> &Arc<dyn PromptProvider> {
        &self.prompt
    }
}
