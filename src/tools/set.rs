use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use thiserror::Error;

use super::tool::{Tool, ToolSpec};
use super::types::ToolName;

#[derive(Clone, Default)]
pub struct ToolSet {
    tools: Arc<BTreeMap<ToolName, Arc<dyn Tool>>>,
    specs: Arc<BTreeMap<ToolName, ToolSpec>>,
}

#[derive(Clone)]
pub(crate) struct EnabledTools {
    entries: Arc<BTreeMap<ToolName, EnabledTool>>,
    specs: Arc<[ToolSpec]>,
}

#[derive(Clone)]
pub(crate) struct EnabledTool {
    pub(crate) implementation: Arc<dyn Tool>,
    pub(crate) spec: ToolSpec,
}

#[derive(Default)]
pub struct ToolSetBuilder {
    tools: BTreeMap<ToolName, RegisteredTool>,
    error: Option<ToolSetError>,
}

struct RegisteredTool {
    tool: Arc<dyn Tool>,
    spec: ToolSpec,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolSetError {
    #[error("tool name is already registered")]
    DuplicateTool,
    #[error("tool specification panicked")]
    Panicked,
    #[error("tool specification is invalid")]
    InvalidSpec,
}

impl ToolSet {
    pub fn builder() -> ToolSetBuilder {
        ToolSetBuilder::default()
    }

    pub fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(Arc::clone)
    }

    pub fn contains(&self, name: &ToolName) -> bool {
        self.tools.contains_key(name)
    }

    pub fn specs_for(&self, enabled: &BTreeSet<ToolName>) -> Vec<ToolSpec> {
        enabled
            .iter()
            .filter_map(|name| self.specs.get(name).cloned())
            .collect()
    }

    pub(crate) fn frozen_specs(&self) -> impl ExactSizeIterator<Item = &ToolSpec> {
        self.specs.values()
    }

    pub(crate) fn enabled_subset(&self, enabled: &BTreeSet<ToolName>) -> EnabledTools {
        let mut entries = BTreeMap::new();
        let mut specs = Vec::with_capacity(enabled.len());
        for name in enabled {
            if let (Some(implementation), Some(spec)) = (self.tools.get(name), self.specs.get(name))
            {
                let spec = spec.clone();
                specs.push(spec.clone());
                entries.insert(
                    name.clone(),
                    EnabledTool {
                        implementation: Arc::clone(implementation),
                        spec,
                    },
                );
            }
        }
        EnabledTools {
            entries: Arc::new(entries),
            specs: specs.into(),
        }
    }
}

impl EnabledTools {
    pub(crate) fn get(&self, name: &ToolName) -> Option<&EnabledTool> {
        self.entries.get(name)
    }

    pub(crate) fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }
}

impl ToolSetBuilder {
    pub fn register<T>(&mut self, tool: T) -> &mut Self
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        if self.error.is_some() {
            return self;
        }
        let spec = match catch_unwind(AssertUnwindSafe(|| tool.spec().clone())) {
            Ok(spec) => spec,
            Err(_) => {
                self.error = Some(ToolSetError::Panicked);
                return self;
            }
        };
        if spec.validate().is_err() {
            self.error = Some(ToolSetError::InvalidSpec);
            return self;
        }
        let name = spec.name().clone();
        if self.tools.contains_key(&name) {
            self.error = Some(ToolSetError::DuplicateTool);
            return self;
        }
        self.tools.insert(name, RegisteredTool { tool, spec });
        self
    }

    pub fn build(self) -> Result<ToolSet, ToolSetError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut tools = BTreeMap::new();
        let mut specs = BTreeMap::new();
        for (name, registered) in self.tools {
            tools.insert(name.clone(), Arc::clone(&registered.tool));
            specs.insert(name, registered.spec);
        }
        Ok(ToolSet {
            tools: Arc::new(tools),
            specs: Arc::new(specs),
        })
    }
}
