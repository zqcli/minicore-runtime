use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use thiserror::Error;

use crate::compaction::CompactionStrategy;
use crate::config::{CompactionConfig, SemanticLimits, SessionSpec};
use crate::context::ContextProvider;
use crate::model::Model;
use crate::tools::{ToolPolicy, ToolSet};

#[derive(Clone)]
pub struct SessionBindings {
    pub model: Arc<dyn Model>,
    pub tools: ToolSet,
    pub tool_policy: Option<Arc<dyn ToolPolicy>>,
    pub context: Option<Arc<dyn ContextProvider>>,
    pub compaction: Option<Arc<dyn CompactionStrategy>>,
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
        if spec
            .enabled_tools
            .iter()
            .any(|name| !self.tools.contains(name))
        {
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
        Ok(())
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
