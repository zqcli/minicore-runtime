use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{Model, ModelDescriptor, ReasoningPreference};
use crate::prompt::PromptProvider;
use crate::tools::{ToolPolicy, ToolSet};
use crate::value::Text;

/// Errors constructing user input.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum UserInputError {
    #[error("user input is empty")]
    InvalidText,
}

/// User input for one agent loop (the initial request or an input from `LoopInputs`).
///
/// Constructors require non-empty UTF-8 text without a byte limit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserInput {
    Text(Text),
}

impl UserInput {
    /// Creates non-empty text user input.
    pub fn text(value: impl AsRef<str>) -> Result<Self, UserInputError> {
        let text = Text::new(value);
        if text.is_empty() {
            return Err(UserInputError::InvalidText);
        }
        Ok(Self::Text(text))
    }

    /// Returns the input text as a string slice.
    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text.as_str(),
        }
    }
}

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

    /// Next revision; saturating so handed-out revisions stay monotonic even
    /// at the (unreachable) `u64` ceiling.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
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
        if tools.frozen_specs().len() != 0 && !descriptor.supports_tools {
            return Err(ExecutionConfigError::ToolsUnsupported);
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
