use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use serde_json::{Value, json};
use tokio::sync::{Barrier, Notify, mpsc};

use super::*;
use crate::ids::{SessionId, SessionInstanceId, ToolCallId, TurnId};
use crate::tools::{
    ApprovalRequest, ApprovalRisk, ToolError, ToolFuture, ToolInputAnswer, ToolInputAnswerKind,
    ToolPolicyError, ToolPolicyFuture, ToolSpec, ToolValueError,
};

#[cfg(test)]
mod approval;
#[cfg(test)]
mod basic;
#[cfg(test)]
mod concurrency;
#[cfg(test)]
mod execution;
#[cfg(test)]
mod input_progress;
#[cfg(test)]
mod policy;

fn session_id() -> SessionId {
    "ses_00000000000000000000000000000051".parse().unwrap()
}

fn instance_id() -> SessionInstanceId {
    "ins_00000000000000000000000000000051".parse().unwrap()
}

fn turn_id() -> TurnId {
    "trn_00000000000000000000000000000051".parse().unwrap()
}

fn call_id(value: u8) -> ToolCallId {
    format!("call_{value:032}").parse().unwrap()
}

fn spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        name.parse().unwrap(),
        description,
        json!({"type": "object"}),
    )
    .unwrap()
}

fn invocation(name: &str, value: u8, arguments: Value) -> ToolInvocation {
    ToolInvocation::new(
        session_id(),
        instance_id(),
        turn_id(),
        call_id(value),
        name.parse().unwrap(),
        arguments,
    )
    .unwrap()
}

fn approval_request() -> ApprovalRequest {
    ApprovalRequest::new("approve operation", ApprovalRisk::High).unwrap()
}

fn input_request(kind: ToolInputAnswerKind) -> ToolInputRequest {
    let choices = match kind {
        ToolInputAnswerKind::Text => Vec::new(),
        ToolInputAnswerKind::SingleChoice => vec![
            BoundedText::new("alpha").unwrap(),
            BoundedText::new("beta").unwrap(),
        ],
    };
    ToolInputRequest::new("provide input", choices, kind).unwrap()
}

fn config() -> ToolDriverConfig {
    ToolDriverConfig::from_kernel_values(
        Duration::from_secs(5),
        Duration::from_secs(10),
        MAX_JSON_BYTES,
        BoundedText::MAX_BYTES,
    )
}

fn config_with(
    policy_timeout: Duration,
    tool_timeout: Duration,
    max_input: usize,
    max_output: usize,
) -> ToolDriverConfig {
    ToolDriverConfig::from_kernel_values(policy_timeout, tool_timeout, max_input, max_output)
}

fn driver(
    tool: Arc<ScriptTool>,
    policy: Option<Arc<dyn ToolPolicy>>,
    config: ToolDriverConfig,
) -> ToolDriver {
    let name = tool.specs[0].name().clone();
    let mut builder = ToolSet::builder();
    let registered = Arc::clone(&tool);
    let registered: Arc<dyn Tool> = registered;
    builder.register_arc(registered);
    ToolDriver::new(
        builder.build().unwrap(),
        BTreeSet::from([name]),
        policy,
        config,
    )
    .unwrap()
}

fn allow_policy() -> Arc<ScriptPolicy> {
    ScriptPolicy::new(vec![PolicyBehavior::Decision(ToolDecision::Allow)])
}

fn policy_port(policy: &Arc<ScriptPolicy>) -> Arc<dyn ToolPolicy> {
    Arc::<ScriptPolicy>::clone(policy)
}

fn channels() -> (
    mpsc::Sender<TurnSuspension>,
    mpsc::Receiver<TurnSuspension>,
    mpsc::Sender<ToolDriverProgress>,
    mpsc::Receiver<ToolDriverProgress>,
) {
    let (suspensions, suspension_rx) = mpsc::channel(4);
    let (progress, progress_rx) = mpsc::channel(4);
    (suspensions, suspension_rx, progress, progress_rx)
}

fn deadline_after(duration: Duration) -> Instant {
    Instant::now() + duration
}

fn assert_outcome(result: &ToolDriverResult, outcome: ToolResultOutcome, content: &str) {
    assert_eq!(result.outcome, outcome);
    assert_eq!(result.output.content().as_str(), content);
}

struct OperationProbe {
    polled: AtomicBool,
    dropped: AtomicBool,
    cancelled_before_drop: AtomicBool,
    notify: Notify,
}

impl OperationProbe {
    fn shared() -> Arc<Self> {
        Arc::new(Self {
            polled: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            cancelled_before_drop: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    async fn wait_polled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.polled.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

struct PendingOperation<T> {
    probe: Arc<OperationProbe>,
    cancellation: CancellationToken,
    marker: PhantomData<T>,
}

impl<T> PendingOperation<T> {
    fn new(probe: Arc<OperationProbe>, cancellation: CancellationToken) -> Self {
        Self {
            probe,
            cancellation,
            marker: PhantomData,
        }
    }
}

impl<T> Future for PendingOperation<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        self.probe.polled.store(true, Ordering::SeqCst);
        self.probe.notify.notify_waiters();
        Poll::Pending
    }
}

impl<T> Drop for PendingOperation<T> {
    fn drop(&mut self) {
        self.probe
            .cancelled_before_drop
            .store(self.cancellation.is_cancelled(), Ordering::SeqCst);
        self.probe.dropped.store(true, Ordering::SeqCst);
    }
}

struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

enum ToolBehavior {
    Complete(ToolOutput),
    Error(ToolError),
    ConstructionPanic,
    FuturePanic,
    Pending(Arc<OperationProbe>),
    RequestInput(ToolInputRequest, Arc<AtomicBool>),
    Progress(Vec<ToolProgress>, ToolOutput),
    Barrier {
        gate: Arc<Barrier>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    },
}

struct ScriptTool {
    specs: [ToolSpec; 2],
    spec_calls: AtomicUsize,
    behaviors: Mutex<VecDeque<ToolBehavior>>,
    invocations: Mutex<Vec<ToolInvocation>>,
    deadlines: Mutex<Vec<Instant>>,
    calls: AtomicUsize,
}

impl ScriptTool {
    fn new(name: &str, behaviors: Vec<ToolBehavior>) -> Arc<Self> {
        let value = spec(name, "frozen spec");
        Arc::new(Self {
            specs: [value.clone(), value],
            spec_calls: AtomicUsize::new(0),
            behaviors: Mutex::new(behaviors.into()),
            invocations: Mutex::new(Vec::new()),
            deadlines: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn mutating(name: &str, behaviors: Vec<ToolBehavior>) -> Arc<Self> {
        Arc::new(Self {
            specs: [spec(name, "frozen spec"), spec(name, "mutated spec")],
            spec_calls: AtomicUsize::new(0),
            behaviors: Mutex::new(behaviors.into()),
            invocations: Mutex::new(Vec::new()),
            deadlines: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn spec_calls(&self) -> usize {
        self.spec_calls.load(Ordering::SeqCst)
    }

    fn invocations(&self) -> Vec<ToolInvocation> {
        lock(&self.invocations).clone()
    }

    fn deadlines(&self) -> Vec<Instant> {
        lock(&self.deadlines).clone()
    }
}

impl Tool for ScriptTool {
    fn spec(&self) -> &ToolSpec {
        let index = self.spec_calls.fetch_add(1, Ordering::SeqCst).min(1);
        &self.specs[index]
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.invocations).push(invocation);
        lock(&self.deadlines).push(context.deadline);
        let behavior = lock(&self.behaviors)
            .pop_front()
            .unwrap_or_else(|| ToolBehavior::Complete(ToolOutput::new("default").unwrap()));
        match behavior {
            ToolBehavior::Complete(output) => {
                Box::pin(async move { Ok(ToolExecutionOutcome::Completed(output)) })
            }
            ToolBehavior::Error(error) => Box::pin(async move { Err(error) }),
            ToolBehavior::ConstructionPanic => panic!("scripted tool construction panic"),
            ToolBehavior::FuturePanic => Box::pin(async { panic!("scripted tool future panic") }),
            ToolBehavior::Pending(probe) => {
                Box::pin(PendingOperation::new(probe, context.cancellation))
            }
            ToolBehavior::RequestInput(request, dropped) => Box::pin(async move {
                let _drop = DropSignal(dropped);
                Ok(ToolExecutionOutcome::RequestInput(request))
            }),
            ToolBehavior::Progress(values, output) => Box::pin(async move {
                for value in values {
                    let _ = context.progress.emit(value);
                }
                Ok(ToolExecutionOutcome::Completed(output))
            }),
            ToolBehavior::Barrier {
                gate,
                active,
                max_active,
            } => Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(current, Ordering::SeqCst);
                gate.wait().await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(ToolExecutionOutcome::Completed(
                    ToolOutput::new("done").unwrap(),
                ))
            }),
        }
    }
}

enum PolicyBehavior {
    Decision(ToolDecision),
    DecisionWithDrop(ToolDecision, Arc<AtomicBool>),
    Error(ToolPolicyError),
    ConstructionPanic,
    FuturePanic,
    Pending(Arc<OperationProbe>),
}

struct ScriptPolicy {
    behaviors: Mutex<VecDeque<PolicyBehavior>>,
    requests: Mutex<Vec<ToolPolicyRequest>>,
    calls: AtomicUsize,
    completions: AtomicUsize,
    completion_notify: Notify,
}

impl ScriptPolicy {
    fn new(behaviors: Vec<PolicyBehavior>) -> Arc<Self> {
        Arc::new(Self {
            behaviors: Mutex::new(behaviors.into()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            completions: AtomicUsize::new(0),
            completion_notify: Notify::new(),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<ToolPolicyRequest> {
        lock(&self.requests).clone()
    }

    async fn wait_completions(&self, count: usize) {
        loop {
            let notified = self.completion_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.completions.load(Ordering::SeqCst) >= count {
                return;
            }
            notified.await;
        }
    }
}

impl ToolPolicy for ScriptPolicy {
    fn decide<'a>(&'a self, request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(request.clone());
        let behavior = lock(&self.behaviors)
            .pop_front()
            .unwrap_or(PolicyBehavior::Decision(ToolDecision::Allow));
        if matches!(&behavior, PolicyBehavior::ConstructionPanic) {
            panic!("scripted policy construction panic");
        }
        let completions = &self.completions;
        let notify = &self.completion_notify;
        match behavior {
            PolicyBehavior::Decision(decision) => Box::pin(async move {
                completions.fetch_add(1, Ordering::SeqCst);
                notify.notify_waiters();
                Ok(decision)
            }),
            PolicyBehavior::DecisionWithDrop(decision, dropped) => Box::pin(async move {
                let _drop = DropSignal(dropped);
                completions.fetch_add(1, Ordering::SeqCst);
                notify.notify_waiters();
                Ok(decision)
            }),
            PolicyBehavior::Error(error) => Box::pin(async move {
                completions.fetch_add(1, Ordering::SeqCst);
                notify.notify_waiters();
                Err(error)
            }),
            PolicyBehavior::FuturePanic => Box::pin(async { panic!("scripted policy panic") }),
            PolicyBehavior::Pending(probe) => {
                Box::pin(PendingOperation::new(probe, request.cancellation))
            }
            PolicyBehavior::ConstructionPanic => unreachable!(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
