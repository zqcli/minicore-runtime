use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::context::ToolContext;
use super::types::{ToolError, ToolName, ToolOutput, ToolSpec};

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>>;

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, args: Value) -> ToolFuture<'a>;
}

impl<T: Tool + ?Sized> Tool for Arc<T> {
    fn spec(&self) -> ToolSpec {
        (**self).spec()
    }

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, args: Value) -> ToolFuture<'a> {
        (**self).execute(ctx, args)
    }
}

struct RegisteredTool {
    tool: Arc<dyn Tool>,
    spec: ToolSpec,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<ToolName, RegisteredTool>>,
}

#[derive(Default)]
pub struct ToolRegistryBuilder {
    tools: BTreeMap<ToolName, RegisteredTool>,
}

impl ToolRegistry {
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    pub fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        self.tools
            .get(name)
            .map(|registered| Arc::clone(&registered.tool))
    }

    pub fn specs(
        &self,
        enabled: &std::collections::BTreeSet<ToolName>,
    ) -> Result<Vec<ToolSpec>, ToolError> {
        enabled
            .iter()
            .map(|name| {
                self.tools
                    .get(name)
                    .map(|registered| registered.spec.clone())
                    .ok_or(ToolError::UnknownTool)
            })
            .collect()
    }
}

impl ToolRegistryBuilder {
    pub fn register<T>(&mut self, tool: T) -> Result<(), ToolError>
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let spec =
            catch_unwind(AssertUnwindSafe(|| tool.spec())).map_err(|_| ToolError::Panicked)?;
        let name = spec.name().clone();
        if self.tools.contains_key(&name) {
            return Err(ToolError::DuplicateTool);
        }
        self.tools.insert(name, RegisteredTool { tool, spec });
        Ok(())
    }

    pub fn build(self) -> ToolRegistry {
        ToolRegistry {
            tools: Arc::new(self.tools),
        }
    }
}
