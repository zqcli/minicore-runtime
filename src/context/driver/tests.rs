use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::sync::{Barrier, Notify};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::context::{ContextBlock, ContextFuture, ContextSlot};
use crate::ids::ContextSourceId;
use crate::value::BoundedText;

#[cfg(test)]
mod behavior;
#[cfg(test)]
mod concurrency;
#[cfg(test)]
mod deadline;
#[cfg(test)]
mod validation;

fn request(deadline_after: Duration, cancellation: CancellationToken) -> ContextRequest {
    ContextRequest {
        session_id: "ses_00000000000000000000000000000061".parse().unwrap(),
        instance_id: "ins_00000000000000000000000000000061".parse().unwrap(),
        turn_id: "trn_00000000000000000000000000000061".parse().unwrap(),
        model_round: 2,
        conversation: crate::conversation::ConversationView::empty(),
        remaining_context_budget: 123,
        cancellation,
        deadline: Instant::now() + deadline_after,
    }
}

fn limits() -> SemanticLimits {
    SemanticLimits::default()
}

fn block(source: &str, slot: ContextSlot, priority: i16, content: &str) -> ContextBlock {
    ContextBlock {
        source: source.parse::<ContextSourceId>().unwrap(),
        slot,
        priority,
        content: BoundedText::new(content).unwrap(),
    }
}

fn bundle(blocks: Vec<ContextBlock>) -> ContextBundle {
    ContextBundle { blocks }
}

struct StreamProbe {
    polled: AtomicBool,
    dropped: AtomicBool,
    notify: Notify,
}

impl StreamProbe {
    fn shared() -> Arc<Self> {
        Arc::new(Self {
            polled: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
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

struct PendingContextFuture {
    probe: Arc<StreamProbe>,
}

impl Future for PendingContextFuture {
    type Output = Result<ContextBundle, ContextError>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        self.probe.polled.store(true, Ordering::SeqCst);
        self.probe.notify.notify_waiters();
        Poll::Pending
    }
}

impl Drop for PendingContextFuture {
    fn drop(&mut self) {
        self.probe.dropped.store(true, Ordering::SeqCst);
    }
}

enum ProviderBehavior {
    Bundle(ContextBundle),
    Error(ContextError),
    ConstructionPanic,
    FuturePanic,
    Pending(Arc<StreamProbe>),
    Barrier(Arc<Barrier>, ContextBundle),
}

struct ScriptProvider {
    behaviors: Mutex<VecDeque<ProviderBehavior>>,
    requests: Mutex<Vec<ContextRequest>>,
    calls: AtomicUsize,
}

impl ScriptProvider {
    fn new(behaviors: Vec<ProviderBehavior>) -> Arc<Self> {
        Arc::new(Self {
            behaviors: Mutex::new(behaviors.into()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<ContextRequest> {
        lock(&self.requests).clone()
    }
}

impl ContextProvider for ScriptProvider {
    fn provide<'a>(&'a self, request: ContextRequest) -> ContextFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(request);
        let behavior = lock(&self.behaviors)
            .pop_front()
            .unwrap_or_else(|| ProviderBehavior::Bundle(bundle(Vec::new())));
        match behavior {
            ProviderBehavior::Bundle(bundle) => Box::pin(async move { Ok(bundle) }),
            ProviderBehavior::Error(error) => Box::pin(async move { Err(error) }),
            ProviderBehavior::ConstructionPanic => panic!("scripted context construction panic"),
            ProviderBehavior::FuturePanic => Box::pin(async { panic!("scripted context panic") }),
            ProviderBehavior::Pending(probe) => Box::pin(PendingContextFuture { probe }),
            ProviderBehavior::Barrier(barrier, bundle) => Box::pin(async move {
                barrier.wait().await;
                Ok(bundle)
            }),
        }
    }
}

fn provider_port(provider: &Arc<ScriptProvider>) -> Arc<dyn ContextProvider> {
    Arc::<ScriptProvider>::clone(provider)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
