use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use minicore_runtime::compaction::{CompactionFuture, CompactionRequest, CompactionStrategy};
use minicore_runtime::context::{ContextFuture, ContextProvider, ContextRequest};
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelRequest, ModelStartFuture, ReasoningPreference,
};
use minicore_runtime::tools::{
    Tool, ToolContext, ToolFuture, ToolInvocation, ToolPolicy, ToolPolicyFuture, ToolPolicyRequest,
    ToolSet, ToolSpec,
};
use minicore_runtime::{BoundedText, CompactionConfig, SemanticLimits, SessionSpec};
use serde_json::{Value, json};

#[derive(Default)]
pub struct Calls {
    pub descriptor: AtomicUsize,
    pub model_start: AtomicUsize,
    pub tool_spec: AtomicUsize,
    pub tool_execute: AtomicUsize,
    pub policy: AtomicUsize,
    pub context: AtomicUsize,
    pub compaction: AtomicUsize,
}

struct FakeModel {
    descriptor: ModelDescriptor,
    calls: Arc<Calls>,
    panic_descriptor: bool,
}

impl Model for FakeModel {
    fn descriptor(&self) -> &ModelDescriptor {
        self.calls.descriptor.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_descriptor, "descriptor panic");
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        self.calls.model_start.fetch_add(1, Ordering::SeqCst);
        panic!("model start must not be called by bindings validation")
    }
}

struct FakeTool {
    spec: ToolSpec,
    calls: Arc<Calls>,
}

impl Tool for FakeTool {
    fn spec(&self) -> &ToolSpec {
        self.calls.tool_spec.fetch_add(1, Ordering::SeqCst);
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        self.calls.tool_execute.fetch_add(1, Ordering::SeqCst);
        panic!("tool execute must not be called by bindings validation")
    }
}

struct FakePolicy(Arc<Calls>);

impl ToolPolicy for FakePolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        self.0.policy.fetch_add(1, Ordering::SeqCst);
        panic!("policy must not be called by bindings validation")
    }
}

struct FakeContext(Arc<Calls>);

impl ContextProvider for FakeContext {
    fn provide<'a>(&'a self, _request: ContextRequest) -> ContextFuture<'a> {
        self.0.context.fetch_add(1, Ordering::SeqCst);
        panic!("context must not be called by bindings validation")
    }
}

struct FakeCompaction(Arc<Calls>);

impl CompactionStrategy for FakeCompaction {
    fn compact<'a>(&'a self, _request: CompactionRequest) -> CompactionFuture<'a> {
        self.0.compaction.fetch_add(1, Ordering::SeqCst);
        panic!("compaction must not be called by bindings validation")
    }
}

pub fn descriptor(
    model_ref: &str,
    reasoning: BTreeSet<ReasoningPreference>,
    supports_tools: bool,
    context_window: u64,
) -> ModelDescriptor {
    ModelDescriptor {
        model_ref: model_ref.parse().unwrap(),
        context_window,
        supported_reasoning: reasoning,
        supports_tools,
    }
}

pub fn model(
    calls: &Arc<Calls>,
    descriptor: ModelDescriptor,
    panic_descriptor: bool,
) -> Arc<dyn Model> {
    Arc::new(FakeModel {
        descriptor,
        calls: Arc::clone(calls),
        panic_descriptor,
    })
}

pub fn base_model(calls: &Arc<Calls>, supports_tools: bool) -> Arc<dyn Model> {
    model(
        calls,
        descriptor(
            "host:model",
            BTreeSet::from([ReasoningPreference::Auto]),
            supports_tools,
            1,
        ),
        false,
    )
}

pub fn tool_spec(name: &str, description: impl AsRef<str>, schema: Value) -> ToolSpec {
    ToolSpec::new(name.parse().unwrap(), description, schema).unwrap()
}

pub fn tool_set(calls: &Arc<Calls>, specs: Vec<ToolSpec>) -> ToolSet {
    let mut builder = ToolSet::builder();
    for spec in specs {
        builder.register(FakeTool {
            spec,
            calls: Arc::clone(calls),
        });
    }
    builder.build().unwrap()
}

pub fn spec(
    enabled_tools: &[&str],
    reasoning: ReasoningPreference,
    compaction: CompactionConfig,
) -> SessionSpec {
    SessionSpec::new(
        "host:model".parse().unwrap(),
        reasoning,
        BoundedText::new("system").unwrap(),
        enabled_tools
            .iter()
            .map(|name| name.parse().unwrap())
            .collect(),
        4,
        compaction,
    )
    .unwrap()
}

pub fn policy(calls: &Arc<Calls>) -> Arc<dyn ToolPolicy> {
    Arc::new(FakePolicy(Arc::clone(calls)))
}

pub fn context(calls: &Arc<Calls>) -> Arc<dyn ContextProvider> {
    Arc::new(FakeContext(Arc::clone(calls)))
}

pub fn compaction(calls: &Arc<Calls>) -> Arc<dyn CompactionStrategy> {
    Arc::new(FakeCompaction(Arc::clone(calls)))
}

pub fn compact_schema(target: usize) -> Value {
    let schema = json!({"padding": "x".repeat(target.checked_sub(14).unwrap())});
    assert_eq!(serde_json::to_vec(&schema).unwrap().len(), target);
    schema
}

pub fn validate_tools(
    model: &Arc<dyn Model>,
    tools: ToolSet,
    spec: &SessionSpec,
    limits: &SemanticLimits,
) -> Result<(), minicore_runtime::session::SessionBindingError> {
    minicore_runtime::SessionBindings::new(Arc::clone(model), tools, None, None, None)
        .validate(spec, limits)
}
