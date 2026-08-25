use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use thiserror::Error;

use crate::compaction::CompactionStrategy;
use crate::config::{CompactionConfig, SemanticLimits, SessionSpec};
use crate::context::ContextProvider;
use crate::model::{Model, ModelDescriptor, ModelLimits};
use crate::tools::{EnabledTools, ToolPolicy, ToolSet};

#[derive(Clone)]
pub struct SessionBindings {
    pub model: Arc<dyn Model>,
    pub tools: ToolSet,
    pub tool_policy: Option<Arc<dyn ToolPolicy>>,
    pub context: Option<Arc<dyn ContextProvider>>,
    pub compaction: Option<Arc<dyn CompactionStrategy>>,
}

pub(crate) struct ValidatedSessionBindings {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) model_descriptor: ModelDescriptor,
    pub(crate) model_limits: ModelLimits,
    pub(crate) enabled_tools: EnabledTools,
    pub(crate) tool_policy: Option<Arc<dyn ToolPolicy>>,
    pub(crate) context: Option<Arc<dyn ContextProvider>>,
    pub(crate) compaction: Option<Arc<dyn CompactionStrategy>>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SessionBindingError {
    #[error("session binding limits are invalid")]
    InvalidLimits,
    #[error("session specification is invalid")]
    InvalidSpec,
    #[error("bound model descriptor panicked")]
    ModelPanicked,
    #[error("bound model descriptor is invalid")]
    InvalidModelDescriptor,
    #[error("bound model does not match the session specification")]
    ModelMismatch,
    #[error("bound model does not support the requested reasoning mode")]
    UnsupportedReasoning,
    #[error("bound model does not support tools")]
    UnsupportedTools,
    #[error("an enabled tool is missing")]
    MissingTool,
    #[error("enabled tools require a tool policy")]
    MissingToolPolicy,
    #[error("enabled compaction requires a compaction strategy")]
    MissingCompactionStrategy,
    #[error("the frozen tool set exceeds the configured tool count")]
    TooManyTools,
    #[error("a frozen tool specification is invalid")]
    InvalidToolSpec,
}

impl SessionBindings {
    pub fn new(
        model: Arc<dyn Model>,
        tools: ToolSet,
        tool_policy: Option<Arc<dyn ToolPolicy>>,
        context: Option<Arc<dyn ContextProvider>>,
        compaction: Option<Arc<dyn CompactionStrategy>>,
    ) -> Self {
        Self {
            model,
            tools,
            tool_policy,
            context,
            compaction,
        }
    }

    pub fn validate(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<(), SessionBindingError> {
        self.freeze(spec, limits).map(|_| ())
    }

    pub(crate) fn freeze(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<ValidatedSessionBindings, SessionBindingError> {
        limits
            .validate()
            .map_err(|_| SessionBindingError::InvalidLimits)?;
        spec.validate(limits)
            .map_err(|_| SessionBindingError::InvalidSpec)?;

        let descriptor = catch_unwind(AssertUnwindSafe(|| self.model.descriptor().clone()))
            .map_err(|_| SessionBindingError::ModelPanicked)?;
        descriptor
            .validate()
            .map_err(|_| SessionBindingError::InvalidModelDescriptor)?;
        if descriptor.model_ref != spec.model {
            return Err(SessionBindingError::ModelMismatch);
        }
        if !descriptor.supports_reasoning(spec.reasoning) {
            return Err(SessionBindingError::UnsupportedReasoning);
        }

        if !spec.enabled_tools.is_empty() {
            if !descriptor.supports_tools {
                return Err(SessionBindingError::UnsupportedTools);
            }
            if self.tool_policy.is_none() {
                return Err(SessionBindingError::MissingToolPolicy);
            }
        }
        let enabled_tools = self.tools.enabled_subset(&spec.enabled_tools);
        if enabled_tools.specs().len() != spec.enabled_tools.len() {
            return Err(SessionBindingError::MissingTool);
        }

        let frozen_specs = self.tools.frozen_specs();
        if frozen_specs.len() > limits.max_tool_count {
            return Err(SessionBindingError::TooManyTools);
        }
        for tool_spec in frozen_specs {
            tool_spec
                .validate_for_bindings(limits.max_tool_name_bytes, limits.max_tool_schema_bytes)
                .map_err(|_| SessionBindingError::InvalidToolSpec)?;
        }

        if matches!(&spec.compaction, CompactionConfig::Enabled { .. }) && self.compaction.is_none()
        {
            return Err(SessionBindingError::MissingCompactionStrategy);
        }
        let context_window = u32::try_from(descriptor.context_window).unwrap_or(u32::MAX);
        let model_limits = ModelLimits::new(Some(context_window), None)
            .map_err(|_| SessionBindingError::InvalidModelDescriptor)?;
        Ok(ValidatedSessionBindings {
            model: Arc::clone(&self.model),
            model_descriptor: descriptor,
            model_limits,
            enabled_tools,
            tool_policy: self.tool_policy.clone(),
            context: self.context.clone(),
            compaction: self.compaction.clone(),
        })
    }
}

impl fmt::Debug for SessionBindings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionBindings")
            .field("model", &"<redacted>")
            .field("tool_count", &self.tools.frozen_specs().len())
            .field("has_tool_policy", &self.tool_policy.is_some())
            .field("has_context", &self.context.is_some())
            .field("has_compaction", &self.compaction.is_some())
            .finish()
    }
}
