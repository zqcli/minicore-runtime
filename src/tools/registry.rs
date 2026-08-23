use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::legacy_context::LegacyToolContext;
use super::legacy_types::{LegacyToolError, LegacyToolOutput};
use super::tool::ToolSpec;
use super::types::ToolName;

pub(crate) type LegacyToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<LegacyToolOutput, LegacyToolError>> + Send + 'a>>;

pub(crate) trait LegacyTool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn execute<'a>(&'a self, ctx: LegacyToolContext<'a>, args: Value) -> LegacyToolFuture<'a>;
}

impl<T: LegacyTool + ?Sized> LegacyTool for Arc<T> {
    fn spec(&self) -> ToolSpec {
        (**self).spec()
    }

    fn execute<'a>(&'a self, ctx: LegacyToolContext<'a>, args: Value) -> LegacyToolFuture<'a> {
        (**self).execute(ctx, args)
    }
}

struct RegisteredTool {
    tool: Arc<dyn LegacyTool>,
    spec: ToolSpec,
}

#[derive(Clone, Default)]
pub(crate) struct ToolRegistry {
    tools: Arc<BTreeMap<ToolName, RegisteredTool>>,
}

#[derive(Default)]
pub(crate) struct ToolRegistryBuilder {
    tools: BTreeMap<ToolName, RegisteredTool>,
}

impl ToolRegistry {
    pub(crate) fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    pub(crate) fn get(&self, name: &ToolName) -> Option<Arc<dyn LegacyTool>> {
        self.tools
            .get(name)
            .map(|registered| Arc::clone(&registered.tool))
    }

    pub(crate) fn specs(
        &self,
        enabled: &std::collections::BTreeSet<ToolName>,
    ) -> Result<Vec<ToolSpec>, LegacyToolError> {
        enabled
            .iter()
            .map(|name| {
                self.tools
                    .get(name)
                    .map(|registered| registered.spec.clone())
                    .ok_or(LegacyToolError::UnknownTool)
            })
            .collect()
    }
}

impl ToolRegistryBuilder {
    pub(crate) fn register<T>(&mut self, tool: T) -> Result<(), LegacyToolError>
    where
        T: LegacyTool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    pub(crate) fn register_arc(
        &mut self,
        tool: Arc<dyn LegacyTool>,
    ) -> Result<(), LegacyToolError> {
        let spec = catch_unwind(AssertUnwindSafe(|| tool.spec()))
            .map_err(|_| LegacyToolError::Panicked)?;
        let name = spec.name().clone();
        if self.tools.contains_key(&name) {
            return Err(LegacyToolError::DuplicateTool);
        }
        self.tools.insert(name, RegisteredTool { tool, spec });
        Ok(())
    }

    pub(crate) fn build(self) -> ToolRegistry {
        ToolRegistry {
            tools: Arc::new(self.tools),
        }
    }
}

const _: () = {
    // P6 deletion target: remove when SessionActor consumes ToolSet directly.
    let _ = std::mem::size_of::<ToolRegistryBuilder>();
    let _: fn() -> ToolRegistryBuilder = ToolRegistry::builder;
    let _: fn(&mut ToolRegistryBuilder, Arc<dyn LegacyTool>) -> Result<(), LegacyToolError> =
        ToolRegistryBuilder::register::<Arc<dyn LegacyTool>>;
    let _: fn(ToolRegistryBuilder) -> ToolRegistry = ToolRegistryBuilder::build;
};
