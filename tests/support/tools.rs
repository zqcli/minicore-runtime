use super::spec;
use minicore_runtime::BoundedText;
use minicore_runtime::tools::{
    Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolFuture, ToolInputAnswerKind,
    ToolInputRequest, ToolInvocation, ToolOutput, ToolSpec,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

pub struct FakeTool {
    pub spec: ToolSpec,
    pub calls: Arc<AtomicUsize>,
    pub request_input: bool,
}

impl FakeTool {
    pub fn new(name: &str, request_input: bool) -> Self {
        Self {
            spec: spec(name, name),
            calls: Arc::new(AtomicUsize::new(0)),
            request_input,
        }
    }
}

impl Tool for FakeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let request_input = self.request_input;
        Box::pin(async move {
            assert!(context.deadline > Instant::now() - Duration::from_secs(1));
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if request_input {
                Ok(ToolExecutionOutcome::RequestInput(
                    ToolInputRequest::new(
                        "choose",
                        vec![BoundedText::new("yes").unwrap()],
                        ToolInputAnswerKind::SingleChoice,
                    )
                    .unwrap(),
                ))
            } else {
                Ok(ToolExecutionOutcome::Completed(
                    ToolOutput::new("done").unwrap(),
                ))
            }
        })
    }
}
