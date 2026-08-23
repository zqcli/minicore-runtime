use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelRequest, ModelStartFuture, ReasoningPreference,
};
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{BoundedText, CompactionConfig, SessionBindings, SessionSpec};

pub struct BindingFixture {
    pub bindings: SessionBindings,
    pub spec: SessionSpec,
    pub descriptor_calls: Arc<AtomicUsize>,
}

struct FakeModel {
    descriptor: ModelDescriptor,
    descriptor_calls: Arc<AtomicUsize>,
}

impl Model for FakeModel {
    fn descriptor(&self) -> &ModelDescriptor {
        self.descriptor_calls.fetch_add(1, Ordering::SeqCst);
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        panic!("idle owner must not start the model")
    }
}

pub fn fixture(model_ref: &str) -> BindingFixture {
    let descriptor_calls = Arc::new(AtomicUsize::new(0));
    let descriptor = ModelDescriptor {
        model_ref: model_ref.parse().unwrap(),
        context_window: 1,
        supported_reasoning: BTreeSet::from([ReasoningPreference::Auto]),
        supports_tools: false,
    };
    let model: Arc<dyn Model> = Arc::new(FakeModel {
        descriptor,
        descriptor_calls: Arc::clone(&descriptor_calls),
    });
    let spec = SessionSpec::new(
        model_ref.parse().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    BindingFixture {
        bindings: SessionBindings::new(
            model,
            ToolSet::builder().build().unwrap(),
            None,
            None,
            None,
        ),
        spec,
        descriptor_calls,
    }
}
