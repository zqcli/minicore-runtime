pub mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::stream;
use minicore_runtime::context::{ContextBundle, ContextFuture, ContextProvider, ContextRequest};
use minicore_runtime::conversation::TurnTerminal;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelEvent, ModelFinishReason, ModelRef,
    ModelStartFuture, ModelStream, ReasoningPreference, Usage,
};
use minicore_runtime::session::SessionHealth;
use minicore_runtime::tools::{
    Tool, ToolContext, ToolDecision, ToolError, ToolExecutionOutcome, ToolFuture, ToolInvocation,
    ToolOutput, ToolPolicy, ToolPolicyFuture, ToolPolicyRequest, ToolSet, ToolSpec,
};
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SessionBindings, SessionId, SessionRuntime,
    SessionRuntimeOptions, SessionSpec, ToolCallId, TurnOptions, UserInput,
};
use serde_json::json;
use tokio::sync::{Barrier, Semaphore};

use support::fake_session_log::FakeSessionLog;

struct SharedContext {
    first_round_barrier: Arc<Barrier>,
    calls: AtomicUsize,
}

impl ContextProvider for SharedContext {
    fn provide<'a>(&'a self, _request: ContextRequest) -> ContextFuture<'a> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let barrier = Arc::clone(&self.first_round_barrier);
        Box::pin(async move {
            if call < 2 {
                barrier.wait().await;
            }
            Ok(ContextBundle { blocks: Vec::new() })
        })
    }
}

struct SharedModel {
    descriptor: ModelDescriptor,
    first_round_barrier: Arc<Barrier>,
    calls: AtomicUsize,
    tool_name: minicore_runtime::tools::ToolName,
}

impl Model for SharedModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: minicore_runtime::model::ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let barrier = Arc::clone(&self.first_round_barrier);
        let tool_name = self.tool_name.clone();
        Box::pin(async move {
            let events = if context.round == 0 {
                if call < 2 {
                    barrier.wait().await;
                }
                let tool_call_id: ToolCallId = format!("call-{}", context.turn_id).parse().unwrap();
                vec![
                    Ok(ModelEvent::ToolCallStart {
                        tool_call_id: tool_call_id.clone(),
                        tool_name,
                    }),
                    Ok(ModelEvent::tool_call_arguments_delta(tool_call_id.clone(), "{}").unwrap()),
                    Ok(ModelEvent::ToolCallEnd { tool_call_id }),
                    Ok(ModelEvent::Usage {
                        usage: Usage::new(1, 1, 0),
                    }),
                    Ok(ModelEvent::Finish {
                        reason: ModelFinishReason::ToolCalls,
                    }),
                ]
            } else {
                vec![
                    Ok(ModelEvent::text_delta("done").unwrap()),
                    Ok(ModelEvent::Usage {
                        usage: Usage::new(1, 1, 0),
                    }),
                    Ok(ModelEvent::Finish {
                        reason: ModelFinishReason::Stop,
                    }),
                ]
            };
            let stream: ModelStream = Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

struct SharedPolicy {
    barrier: Arc<Barrier>,
    calls: AtomicUsize,
}

impl ToolPolicy for SharedPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let barrier = Arc::clone(&self.barrier);
        Box::pin(async move {
            if call < 2 {
                barrier.wait().await;
            }
            Ok(ToolDecision::Allow)
        })
    }
}

struct SharedTool {
    spec: ToolSpec,
    first_session: SessionId,
    started: Arc<Semaphore>,
    second_release: Arc<Semaphore>,
}

impl Tool for SharedTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        let first = invocation.session_id == self.first_session;
        let started = Arc::clone(&self.started);
        let second_release = Arc::clone(&self.second_release);
        Box::pin(async move {
            started.add_permits(1);
            if first {
                context.cancellation.cancelled().await;
                Err(ToolError::Cancelled)
            } else {
                let permit = second_release.acquire_owned().await.unwrap();
                permit.forget();
                Ok(ToolExecutionOutcome::Completed(
                    ToolOutput::new("shared success").unwrap(),
                ))
            }
        })
    }
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn options(bindings: SessionBindings) -> SessionRuntimeOptions {
    SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        bindings,
        tokio::runtime::Handle::current(),
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn two_runtimes_share_exact_ports_concurrently_with_cancellation_isolation() {
    let first_session = session(121);
    let second_session = session(122);
    let model_ref: ModelRef = "host:shared-ports".parse().unwrap();
    let tool_name: minicore_runtime::tools::ToolName = "shared".parse().unwrap();
    let context_barrier = Arc::new(Barrier::new(3));
    let model_barrier = Arc::new(Barrier::new(3));
    let policy_barrier = Arc::new(Barrier::new(3));
    let tool_started = Arc::new(Semaphore::new(0));
    let second_release = Arc::new(Semaphore::new(0));

    let model = Arc::new(SharedModel {
        descriptor: ModelDescriptor::new(
            model_ref.clone(),
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
        first_round_barrier: Arc::clone(&model_barrier),
        calls: AtomicUsize::new(0),
        tool_name: tool_name.clone(),
    });
    let policy = Arc::new(SharedPolicy {
        barrier: Arc::clone(&policy_barrier),
        calls: AtomicUsize::new(0),
    });
    let context = Arc::new(SharedContext {
        first_round_barrier: Arc::clone(&context_barrier),
        calls: AtomicUsize::new(0),
    });
    let tool = Arc::new(SharedTool {
        spec: ToolSpec::new(tool_name.clone(), "shared", json!({"type": "object"})).unwrap(),
        first_session,
        started: Arc::clone(&tool_started),
        second_release: Arc::clone(&second_release),
    });
    let mut builder = ToolSet::builder();
    let registered: Arc<dyn Tool> = tool;
    builder.register_arc(registered);
    let tools = builder.build().unwrap();
    let model_port: Arc<dyn Model> = model.clone();
    let policy_port: Arc<dyn ToolPolicy> = policy.clone();
    let context_port: Arc<dyn ContextProvider> = context.clone();
    let bindings = SessionBindings::new(
        model_port,
        tools,
        Some(policy_port),
        Some(context_port),
        None,
    );
    let first_bindings = bindings.clone();
    let second_bindings = bindings;
    assert!(Arc::ptr_eq(&first_bindings.model, &second_bindings.model));
    assert!(Arc::ptr_eq(
        first_bindings.tool_policy.as_ref().unwrap(),
        second_bindings.tool_policy.as_ref().unwrap(),
    ));
    assert!(Arc::ptr_eq(
        first_bindings.context.as_ref().unwrap(),
        second_bindings.context.as_ref().unwrap(),
    ));
    let spec = SessionSpec::new(
        model_ref,
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::from([tool_name]),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();

    let first_log = FakeSessionLog::new();
    let first_inspection = first_log.inspection();
    let second_log = FakeSessionLog::new();
    let second_inspection = second_log.inspection();
    let first_runtime = SessionRuntime::create(
        first_session,
        spec.clone(),
        Box::new(first_log),
        options(first_bindings),
    )
    .await
    .unwrap();
    let second_runtime = SessionRuntime::create(
        second_session,
        spec,
        Box::new(second_log),
        options(second_bindings),
    )
    .await
    .unwrap();
    let first_handle = first_runtime.handle();
    let second_handle = second_runtime.handle();
    let first_turn = first_handle
        .submit(UserInput::text("first").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let second_turn = second_handle
        .submit(UserInput::text("second").unwrap(), TurnOptions::default())
        .await
        .unwrap();

    context_barrier.wait().await;
    model_barrier.wait().await;
    policy_barrier.wait().await;
    for _ in 0..2 {
        let permit = Arc::clone(&tool_started).acquire_owned().await.unwrap();
        permit.forget();
    }
    assert!(first_turn.cancel());
    assert_eq!(
        first_turn.wait().await.unwrap().terminal,
        TurnTerminal::CancelledByUser
    );
    assert!(!second_turn.is_finished());
    second_release.add_permits(1);
    assert_eq!(
        second_turn.wait().await.unwrap().terminal,
        TurnTerminal::Completed
    );
    assert_eq!(first_handle.state().health, SessionHealth::Healthy);
    assert_eq!(second_handle.state().health, SessionHealth::Healthy);
    assert_ne!(first_inspection.entries(), second_inspection.entries());
    first_runtime.shutdown().await.unwrap();
    second_runtime.shutdown().await.unwrap();
}
