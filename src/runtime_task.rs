use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use time::OffsetDateTime;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::wire::Timestamp;

/// A synchronous source of wall-clock timestamps for owner-controlled operations.
pub(crate) trait Clock {
    fn now(&self) -> Timestamp;
}

/// The production wall-clock source.
pub(crate) struct SystemClock;

impl SystemClock {
    fn timestamp_from_utc(value: OffsetDateTime) -> Timestamp {
        let nanoseconds = value.nanosecond();
        let truncated = value
            .replace_nanosecond(nanoseconds - (nanoseconds % 1_000_000))
            .expect("truncating a nanosecond component stays within its valid range");
        Timestamp::from_utc(truncated)
            .expect("the system clock supplies a UTC timestamp with a non-zero year")
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Self::timestamp_from_utc(OffsetDateTime::now_utc())
    }
}

/// The injected Tokio handle could not provide the required timer service.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct RuntimeDependencyUnavailable;

impl fmt::Debug for RuntimeDependencyUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RuntimeDependencyUnavailable")
    }
}

impl fmt::Display for RuntimeDependencyUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime dependency unavailable")
    }
}

impl Error for RuntimeDependencyUnavailable {}

/// A redacted terminal outcome for an owner-tracked operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeTaskError {
    OwnerClosing,
    OperationPanicked,
    WorkerUnavailable,
}

impl fmt::Display for RuntimeTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime task failed")
    }
}

impl Error for RuntimeTaskError {}

#[derive(Clone)]
pub(crate) struct RuntimeTaskContext {
    owner: Arc<RuntimeTaskOwner>,
}

impl RuntimeTaskContext {
    /// Validates an injected host handle before making the owner available to other modules.
    pub(crate) async fn new(handle: Handle) -> Result<Self, RuntimeDependencyUnavailable> {
        let owner = Arc::new(RuntimeTaskOwner::new(handle));
        let key = owner
            .reserve_start()
            .map_err(|_| RuntimeDependencyUnavailable)?;
        let probe = catch_unwind(AssertUnwindSafe(|| {
            owner.handle.spawn(async {
                tokio::time::sleep(std::time::Duration::ZERO).await;
            })
        }));
        let probe = match probe {
            Ok(probe) => probe,
            Err(_) => {
                owner.fail_start(key);
                return Err(RuntimeDependencyUnavailable);
            }
        };

        owner.install_probe(key, probe);
        let Some(probe) = owner.take_probe(key) else {
            return Err(RuntimeDependencyUnavailable);
        };
        if probe.await.is_err() {
            return Err(RuntimeDependencyUnavailable);
        }

        Ok(Self { owner })
    }

    /// Starts owner-retained blocking work. The raw Tokio handle remains in the owner registry.
    pub(crate) fn spawn_blocking_tracked<T, F>(&self, operation: F) -> TrackedBlockingJob<T>
    where
        T: Clone + Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let settlement = Arc::new(Settlement::new());
        let key = match self.owner.reserve_start() {
            Ok(key) => key,
            Err(error) => {
                settlement.resolve(Err(error));
                return TrackedBlockingJob::new(settlement);
            }
        };

        let task_owner = Arc::clone(&self.owner);
        let task_settlement = Arc::clone(&settlement);
        let spawned = catch_unwind(AssertUnwindSafe(|| {
            self.owner.handle.spawn_blocking(move || {
                // The task owns the owner while it is running, so dropping every caller-side
                // context or job cannot detach an admitted blocking operation.
                let _owner = task_owner;
                let outcome = catch_unwind(AssertUnwindSafe(operation))
                    .map_err(|_| RuntimeTaskError::OperationPanicked);
                task_settlement.resolve(outcome);
            })
        }));

        match spawned {
            Ok(handle) => {
                let join_failure_settlement: Arc<dyn JoinFailureSettlement> = settlement.clone();
                self.owner
                    .install_blocking(key, handle, join_failure_settlement);
            }
            Err(_) => {
                self.owner.fail_start(key);
                settlement.resolve(Err(RuntimeTaskError::WorkerUnavailable));
            }
        }

        TrackedBlockingJob::new(settlement)
    }

    /// Starts owner-retained asynchronous work. The raw Tokio handle remains in the owner
    /// registry, so dropping every caller-side task handle cannot detach admitted work.
    pub(crate) fn spawn_tracked<F>(&self, future: F) -> Result<TrackedTask, RuntimeTaskError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let settlement = Arc::new(Settlement::new());
        let key = self.owner.reserve_start()?;
        let task_owner = Arc::clone(&self.owner);
        let task_settlement = Arc::clone(&settlement);
        let spawned = catch_unwind(AssertUnwindSafe(|| {
            self.owner.handle.spawn(async move {
                // The task owns the owner while it is running, so dropping every caller-side
                // context or task cannot detach an admitted asynchronous operation.
                let _owner = task_owner;
                let guard = AsyncTaskSettlementGuard::new(task_settlement);
                future.await;
                guard.settle_ok();
            })
        }));

        match spawned {
            Ok(handle) => {
                let join_failure_settlement: Arc<dyn JoinFailureSettlement> = settlement.clone();
                self.owner
                    .install_async(key, handle, join_failure_settlement);
                Ok(TrackedTask::new(settlement))
            }
            Err(_) => {
                self.owner.fail_start(key);
                settlement.resolve(Err(RuntimeTaskError::WorkerUnavailable));
                Err(RuntimeTaskError::WorkerUnavailable)
            }
        }
    }

    /// Synchronously rejects future admissions without claiming the active shutdown leader.
    ///
    /// Runtime facade drop uses this best-effort signal. A later explicit shutdown can still
    /// become the leader that joins already accepted work.
    pub(crate) fn request_closing(&self) {
        self.owner.request_closing();
    }

    #[cfg(test)]
    pub(crate) fn abort_latest_registered_task(&self) {
        let registry = lock(&self.owner.registry);
        registry
            .tasks
            .iter()
            .next_back()
            .expect("a test-created actor has a retained raw task handle")
            .1
            .handle
            .abort();
    }

    /// Closes admission and joins every operation reserved while the owner was open.
    pub(crate) async fn shutdown(&self) {
        self.owner.cancellation.cancel();
        loop {
            // Register before inspecting leadership so a cancelled leader cannot clear its
            // claim between our inspection and this wait.
            let notified = self.owner.registry_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            match self.owner.begin_shutdown() {
                ShutdownAttempt::Leader(mut leadership) => {
                    self.owner.finish_shutdown().await;
                    leadership.complete();
                    return;
                }
                ShutdownAttempt::Closed => return,
                ShutdownAttempt::Waiting => notified.await,
            }
        }
    }
}

/// Opaque caller-side access to one owner-tracked asynchronous operation.
#[derive(Clone)]
pub(crate) struct TrackedTask {
    settlement: Arc<Settlement<()>>,
}

impl TrackedTask {
    fn new(settlement: Arc<Settlement<()>>) -> Self {
        Self { settlement }
    }

    pub(crate) async fn wait(&self) -> Result<(), RuntimeTaskError> {
        self.settlement.wait().await
    }
}

impl fmt::Debug for TrackedTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TrackedTask { .. }")
    }
}

/// Opaque caller-side access to one owner-tracked blocking operation.
#[derive(Clone)]
pub(crate) struct TrackedBlockingJob<T>
where
    T: Clone + Send + 'static,
{
    settlement: Arc<Settlement<T>>,
}

impl<T> TrackedBlockingJob<T>
where
    T: Clone + Send + 'static,
{
    fn new(settlement: Arc<Settlement<T>>) -> Self {
        Self { settlement }
    }

    pub(crate) async fn wait(&self) -> Result<T, RuntimeTaskError> {
        self.settlement.wait().await
    }
}

impl<T> fmt::Debug for TrackedBlockingJob<T>
where
    T: Clone + Send + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TrackedBlockingJob { .. }")
    }
}

struct RuntimeTaskOwner {
    handle: Handle,
    cancellation: CancellationToken,
    registry: Mutex<Registry>,
    registry_changed: Notify,
}

impl RuntimeTaskOwner {
    fn new(handle: Handle) -> Self {
        Self {
            handle,
            cancellation: CancellationToken::new(),
            registry: Mutex::new(Registry::new()),
            registry_changed: Notify::new(),
        }
    }

    fn reserve_start(&self) -> Result<RegistryKey, RuntimeTaskError> {
        let mut registry = lock(&self.registry);
        if registry.phase != OwnerPhase::Open {
            return Err(RuntimeTaskError::OwnerClosing);
        }
        let Some(next_key) = registry.next_key.checked_add(1) else {
            return Err(RuntimeTaskError::WorkerUnavailable);
        };
        let key = RegistryKey(registry.next_key);
        registry.next_key = next_key;
        registry.starting.insert(key);
        Ok(key)
    }

    fn install_probe(&self, key: RegistryKey, handle: JoinHandle<()>) {
        let mut registry = lock(&self.registry);
        registry.starting.remove(&key);
        registry.probes.insert(key, handle);
        drop(registry);
        self.registry_changed.notify_waiters();
    }

    fn take_probe(&self, key: RegistryKey) -> Option<JoinHandle<()>> {
        let mut registry = lock(&self.registry);
        registry.probes.remove(&key)
    }

    fn install_blocking(
        &self,
        key: RegistryKey,
        handle: JoinHandle<()>,
        settlement: Arc<dyn JoinFailureSettlement>,
    ) {
        self.install_task(key, handle, settlement);
    }

    fn install_async(
        &self,
        key: RegistryKey,
        handle: JoinHandle<()>,
        settlement: Arc<dyn JoinFailureSettlement>,
    ) {
        self.install_task(key, handle, settlement);
    }

    fn install_task(
        &self,
        key: RegistryKey,
        handle: JoinHandle<()>,
        settlement: Arc<dyn JoinFailureSettlement>,
    ) {
        let mut registry = lock(&self.registry);
        registry.starting.remove(&key);
        registry
            .tasks
            .insert(key, RegisteredTask { handle, settlement });
        drop(registry);
        self.registry_changed.notify_waiters();
    }

    fn fail_start(&self, key: RegistryKey) {
        let mut registry = lock(&self.registry);
        registry.starting.remove(&key);
        drop(registry);
        self.registry_changed.notify_waiters();
    }

    fn request_closing(&self) {
        let mut registry = lock(&self.registry);
        let changed = registry.phase == OwnerPhase::Open;
        if registry.phase == OwnerPhase::Open {
            registry.phase = OwnerPhase::Closing;
        }
        drop(registry);
        self.cancellation.cancel();
        if changed {
            self.registry_changed.notify_waiters();
        }
    }

    fn begin_shutdown(self: &Arc<Self>) -> ShutdownAttempt {
        let mut registry = lock(&self.registry);
        if registry.phase == OwnerPhase::Closed {
            return ShutdownAttempt::Closed;
        }
        if registry.shutdown_active {
            return ShutdownAttempt::Waiting;
        }
        registry.phase = OwnerPhase::Closing;
        registry.shutdown_active = true;
        ShutdownAttempt::Leader(ShutdownLeadership::new(Arc::clone(self)))
    }

    async fn finish_shutdown(self: &Arc<Self>) {
        loop {
            let notified = self.registry_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let work = {
                let mut registry = lock(&self.registry);
                if !registry.starting.is_empty() {
                    ShutdownWork::Wait
                } else if registry.tasks.is_empty() {
                    assert!(
                        registry.starting.is_empty(),
                        "shutdown cannot close with starts"
                    );
                    assert!(
                        registry.tasks.is_empty(),
                        "shutdown cannot close with tasks"
                    );
                    // Completion and leadership release are one registry transition. A later
                    // caller must observe Closed rather than wait behind a stale leader claim.
                    registry.phase = OwnerPhase::Closed;
                    registry.shutdown_active = false;
                    ShutdownWork::Complete
                } else {
                    ShutdownWork::JoinAll(JoinAllRegisteredTasks::new(
                        Arc::clone(self),
                        std::mem::take(&mut registry.tasks),
                    ))
                }
            };
            match work {
                ShutdownWork::JoinAll(join) => join.await,
                ShutdownWork::Wait => notified.await,
                ShutdownWork::Complete => {
                    self.registry_changed.notify_waiters();
                    return;
                }
            }
        }
    }
}

enum ShutdownAttempt {
    Leader(ShutdownLeadership),
    Waiting,
    Closed,
}

/// Holds the exclusive shutdown claim until its owner finishes or its future is cancelled.
struct ShutdownLeadership {
    owner: Arc<RuntimeTaskOwner>,
    completed: bool,
}

impl ShutdownLeadership {
    fn new(owner: Arc<RuntimeTaskOwner>) -> Self {
        Self {
            owner,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for ShutdownLeadership {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut registry = lock(&self.owner.registry);
        let was_active = registry.phase == OwnerPhase::Closing && registry.shutdown_active;
        if was_active {
            registry.shutdown_active = false;
        }
        drop(registry);
        if was_active {
            self.owner.registry_changed.notify_waiters();
        }
    }
}

enum ShutdownWork {
    JoinAll(JoinAllRegisteredTasks),
    Wait,
    Complete,
}

/// Owns every raw task removed from the registry for one shutdown joining pass.
///
/// Polling every handle in each pass prevents a lower-key parent that awaits a later-key child
/// from serially blocking the child join that settles its `TrackedTask`. If shutdown polling is
/// cancelled, the guard restores every still-pending exact handle and settlement before anything
/// can drop a raw handle and detach an operation.
struct JoinAllRegisteredTasks {
    owner: Arc<RuntimeTaskOwner>,
    tasks: BTreeMap<RegistryKey, RegisteredTask>,
}

impl JoinAllRegisteredTasks {
    fn new(owner: Arc<RuntimeTaskOwner>, tasks: BTreeMap<RegistryKey, RegisteredTask>) -> Self {
        Self { owner, tasks }
    }
}

impl Future for JoinAllRegisteredTasks {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        let mut completed = Vec::new();

        for (key, task) in &mut this.tasks {
            if let Poll::Ready(joined) = Pin::new(&mut task.handle).poll(context) {
                if joined.is_err() {
                    task.settlement.settle_join_failure();
                }
                completed.push(*key);
            }
        }

        for key in completed {
            let removed = this.tasks.remove(&key);
            debug_assert!(removed.is_some(), "each completed owner task has one slot");
        }

        if this.tasks.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for JoinAllRegisteredTasks {
    fn drop(&mut self) {
        if self.tasks.is_empty() {
            return;
        }
        let mut registry = lock(&self.owner.registry);
        for (key, task) in std::mem::take(&mut self.tasks) {
            let previous = registry.tasks.insert(key, task);
            debug_assert!(
                previous.is_none(),
                "an in-flight owner task has one registry slot"
            );
        }
        drop(registry);
        self.owner.registry_changed.notify_waiters();
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct RegistryKey(u64);

struct Registry {
    phase: OwnerPhase,
    shutdown_active: bool,
    next_key: u64,
    starting: BTreeSet<RegistryKey>,
    probes: BTreeMap<RegistryKey, JoinHandle<()>>,
    tasks: BTreeMap<RegistryKey, RegisteredTask>,
}

impl Registry {
    fn new() -> Self {
        Self {
            phase: OwnerPhase::Open,
            shutdown_active: false,
            next_key: 1,
            starting: BTreeSet::new(),
            probes: BTreeMap::new(),
            tasks: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OwnerPhase {
    Open,
    Closing,
    Closed,
}

struct RegisteredTask {
    handle: JoinHandle<()>,
    settlement: Arc<dyn JoinFailureSettlement>,
}

trait JoinFailureSettlement: Send + Sync {
    fn settle_join_failure(&self);
}

impl<T> JoinFailureSettlement for Settlement<T>
where
    T: Clone + Send + 'static,
{
    fn settle_join_failure(&self) {
        self.resolve(Err(RuntimeTaskError::WorkerUnavailable));
    }
}

/// Settles any asynchronous task that is unwound or cancelled after it begins execution.
/// A task cancelled before its first poll is settled by the owner when its registry handle joins.
struct AsyncTaskSettlementGuard {
    settlement: Arc<Settlement<()>>,
    settled: bool,
}

impl AsyncTaskSettlementGuard {
    fn new(settlement: Arc<Settlement<()>>) -> Self {
        Self {
            settlement,
            settled: false,
        }
    }

    fn settle_ok(mut self) {
        self.settlement.resolve(Ok(()));
        self.settled = true;
    }
}

impl Drop for AsyncTaskSettlementGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.settlement
                .resolve(Err(RuntimeTaskError::WorkerUnavailable));
        }
    }
}

struct Settlement<T> {
    outcome: Mutex<Option<Result<T, RuntimeTaskError>>>,
    changed: Notify,
}

impl<T> Settlement<T>
where
    T: Clone + Send + 'static,
{
    fn new() -> Self {
        Self {
            outcome: Mutex::new(None),
            changed: Notify::new(),
        }
    }

    fn resolve(&self, outcome: Result<T, RuntimeTaskError>) {
        let mut stored = lock(&self.outcome);
        if stored.is_none() {
            *stored = Some(outcome);
            drop(stored);
            self.changed.notify_waiters();
        }
    }

    async fn wait(&self) -> Result<T, RuntimeTaskError> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = lock(&self.outcome).as_ref() {
                return outcome.clone();
            }
            notified.await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::panic::AssertUnwindSafe;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use tokio::runtime::{Builder, Handle};
    use tokio::sync::Notify;

    use super::{
        Clock, RuntimeDependencyUnavailable, RuntimeTaskContext, RuntimeTaskError, SystemClock,
    };

    async fn initialized_context() -> RuntimeTaskContext {
        RuntimeTaskContext::new(Handle::current())
            .await
            .expect("the Tokio test runtime has a time driver")
    }

    async fn poll_once_pending<F>(mut future: Pin<&mut F>) -> bool
    where
        F: Future<Output = ()>,
    {
        poll_fn(|context| {
            std::task::Poll::Ready(matches!(
                future.as_mut().poll(context),
                std::task::Poll::Pending
            ))
        })
        .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timer_probe_succeeds_on_a_current_thread_runtime_with_time() {
        let context = initialized_context().await;

        context.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timer_probe_succeeds_on_a_multi_thread_runtime_with_time() {
        let context = initialized_context().await;

        context.shutdown().await;
    }

    #[test]
    fn timer_probe_without_a_time_driver_returns_a_typed_error_without_panicking() {
        let runtime = Builder::new_current_thread()
            .build()
            .expect("the test runtime builds without a time driver");
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(RuntimeTaskContext::new(runtime.handle().clone()))
        }));

        assert!(matches!(result, Ok(Err(RuntimeDependencyUnavailable))));
    }

    #[test]
    fn tracked_work_uses_the_injected_handle_without_an_ambient_runtime() {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .expect("the injected test runtime builds");
        let context = runtime
            .block_on(RuntimeTaskContext::new(runtime.handle().clone()))
            .expect("the injected runtime has a time driver");

        // This call runs outside every Tokio runtime context. It can succeed only through the
        // Handle retained by RuntimeTaskContext.
        let job = context.spawn_blocking_tracked(|| 11_u8);

        assert_eq!(runtime.block_on(job.wait()), Ok(11));
        runtime.block_on(context.shutdown());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracked_blocking_job_uses_the_injected_handle_and_never_exposes_a_raw_join_handle() {
        let context = initialized_context().await;
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let entered_by_operation = Arc::clone(&entered);
        let released_by_test = Arc::clone(&release);

        let job = context.spawn_blocking_tracked(move || {
            entered_by_operation.wait();
            released_by_test.wait();
            7_u8
        });

        entered.wait();
        release.wait();

        assert_eq!(job.wait().await, Ok(7));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracked_async_task_settles_normally_without_exposing_a_raw_join_handle() {
        let context = initialized_context().await;
        let completed = Arc::new(AtomicBool::new(false));
        let completed_by_task = Arc::clone(&completed);

        let task = context
            .spawn_tracked(async move {
                completed_by_task.store(true, Ordering::SeqCst);
            })
            .expect("an open owner admits asynchronous work");

        assert_eq!(task.wait().await, Ok(()));
        assert!(completed.load(Ordering::SeqCst));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_a_task_does_not_cancel_or_detach_its_async_operation_from_shutdown() {
        let context = initialized_context().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let entered_by_task = Arc::clone(&entered);
        let release_by_task = Arc::clone(&release);
        let completed_by_task = Arc::clone(&completed);

        let task = context
            .spawn_tracked(async move {
                entered_by_task.notify_one();
                release_by_task.notified().await;
                completed_by_task.store(true, Ordering::SeqCst);
            })
            .expect("an open owner admits asynchronous work");
        entered.notified().await;
        drop(task);

        let mut shutdown = std::pin::pin!(context.shutdown());
        assert!(poll_once_pending(shutdown.as_mut()).await);
        release.notify_one();
        shutdown.await;

        assert!(completed.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_panic_is_redacted_cloneable_and_settles_shared_waiters_exactly_once() {
        let context = initialized_context().await;
        let task = context
            .spawn_tracked(async {
                panic!("async task payload must not cross the task boundary");
            })
            .expect("an open owner admits asynchronous work");
        let shared_waiter = task.clone();

        let (first, second) = tokio::join!(task.wait(), shared_waiter.wait());

        assert_eq!(first, Err(RuntimeTaskError::WorkerUnavailable));
        assert_eq!(second, Err(RuntimeTaskError::WorkerUnavailable));
        assert_eq!(
            first.expect_err("the task panicked").to_string(),
            "runtime task failed"
        );
        assert!(!format!("{second:?}").contains("async task payload"));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_a_started_async_task_uses_its_raii_settlement() {
        let context = initialized_context().await;
        let entered = Arc::new(Notify::new());
        let entered_by_task = Arc::clone(&entered);
        let task = context
            .spawn_tracked(async move {
                entered_by_task.notify_one();
                std::future::pending::<()>().await;
            })
            .expect("an open owner admits asynchronous work");
        entered.notified().await;

        {
            let registry = super::lock(&context.owner.registry);
            registry
                .tasks
                .values()
                .next()
                .expect("the raw handle is retained only in the owner registry")
                .handle
                .abort();
        }

        assert_eq!(task.wait().await, Err(RuntimeTaskError::WorkerUnavailable));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_polls_a_parent_waiting_for_a_later_unpolled_aborted_child() {
        let context = initialized_context().await;
        let parent_waiting = Arc::new(Notify::new());
        let parent_context = context.clone();
        let parent_waiting_by_task = Arc::clone(&parent_waiting);
        let parent = context
            .spawn_tracked(async move {
                let child = parent_context
                    .spawn_tracked(std::future::pending())
                    .expect("the parent creates its child before shutdown closes admission");
                // This runs in the parent before it yields, so Tokio cannot have polled the
                // child future. The child settlement must therefore come from owner joining.
                parent_context.abort_latest_registered_task();
                parent_waiting_by_task.notify_one();

                assert_eq!(child.wait().await, Err(RuntimeTaskError::WorkerUnavailable));
            })
            .expect("the open owner admits the parent");
        parent_waiting.notified().await;

        tokio::time::timeout(std::time::Duration::from_secs(1), context.shutdown())
            .await
            .expect("shutdown polls the later child instead of serially waiting on its parent");
        assert_eq!(parent.wait().await, Ok(()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_a_job_does_not_cancel_or_detach_its_blocking_operation_from_shutdown() {
        let context = initialized_context().await;
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let completed = Arc::new(AtomicBool::new(false));
        let entered_by_operation = Arc::clone(&entered);
        let released_by_test = Arc::clone(&release);
        let completed_by_operation = Arc::clone(&completed);

        let job = context.spawn_blocking_tracked(move || {
            entered_by_operation.wait();
            released_by_test.wait();
            completed_by_operation.store(true, Ordering::SeqCst);
        });
        entered.wait();
        drop(job);

        let mut shutdown = std::pin::pin!(context.shutdown());
        assert!(poll_once_pending(shutdown.as_mut()).await);
        release.wait();
        shutdown.await;

        assert!(completed.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn operation_panic_is_redacted_cloneable_and_settles_shared_waiters_exactly_once() {
        let context = initialized_context().await;
        let runs = Arc::new(AtomicUsize::new(0));
        let runs_by_operation = Arc::clone(&runs);
        let job = context.spawn_blocking_tracked(move || {
            runs_by_operation.fetch_add(1, Ordering::SeqCst);
            panic!("operation payload must not cross the task boundary");
        });
        let shared_waiter = job.clone();

        let (first, second) = tokio::join!(job.wait(), shared_waiter.wait());

        assert_eq!(first, Err(RuntimeTaskError::OperationPanicked));
        assert_eq!(second, Err(RuntimeTaskError::OperationPanicked));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            first.expect_err("the operation panicked").to_string(),
            "runtime task failed"
        );
        assert!(!format!("{second:?}").contains("operation payload"));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_shutdown_waits_for_admitted_work_and_rejects_later_spawns() {
        let context = initialized_context().await;
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let entered_by_operation = Arc::clone(&entered);
        let released_by_test = Arc::clone(&release);
        let admitted = context.spawn_blocking_tracked(move || {
            entered_by_operation.wait();
            released_by_test.wait();
        });
        entered.wait();

        let mut first_shutdown = std::pin::pin!(context.shutdown());
        assert!(poll_once_pending(first_shutdown.as_mut()).await);
        let second_context = context.clone();
        let mut second_shutdown = std::pin::pin!(second_context.shutdown());
        assert!(poll_once_pending(second_shutdown.as_mut()).await);

        let started_after_closing = Arc::new(AtomicBool::new(false));
        let started_by_rejected_operation = Arc::clone(&started_after_closing);
        let rejected = context.spawn_blocking_tracked(move || {
            started_by_rejected_operation.store(true, Ordering::SeqCst);
        });
        assert_eq!(rejected.wait().await, Err(RuntimeTaskError::OwnerClosing));
        assert!(!started_after_closing.load(Ordering::SeqCst));

        release.wait();
        first_shutdown.await;
        second_shutdown.await;
        assert_eq!(admitted.wait().await, Ok(()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_shutdown_while_joining_restores_raw_jobs_for_the_next_leader() {
        let context = initialized_context().await;
        let task_entered = Arc::new(Notify::new());
        let task_release = Arc::new(Notify::new());
        let task_entered_by_operation = Arc::clone(&task_entered);
        let task_release_by_operation = Arc::clone(&task_release);
        let task = context
            .spawn_tracked(async move {
                task_entered_by_operation.notify_one();
                task_release_by_operation.notified().await;
            })
            .expect("the open owner admits the asynchronous operation");
        task_entered.notified().await;

        let job_entered = Arc::new(Barrier::new(2));
        let job_release = Arc::new(Barrier::new(2));
        let job_entered_by_operation = Arc::clone(&job_entered);
        let job_release_by_operation = Arc::clone(&job_release);
        let job = context.spawn_blocking_tracked(move || {
            job_entered_by_operation.wait();
            job_release_by_operation.wait();
        });
        job_entered.wait();

        let mut first = Box::pin(context.shutdown());
        assert!(poll_once_pending(first.as_mut()).await);
        drop(first);
        {
            let registry = super::lock(&context.owner.registry);
            assert!(matches!(registry.phase, super::OwnerPhase::Closing));
            assert!(!registry.shutdown_active);
            assert_eq!(
                registry.tasks.len(),
                2,
                "cancellation restores the joined raw task"
            );
        }

        let mut second = Box::pin(context.shutdown());
        assert!(poll_once_pending(second.as_mut()).await);
        task_release.notify_one();
        job_release.wait();
        second.await;

        assert_eq!(task.wait().await, Ok(()));
        assert_eq!(job.wait().await, Ok(()));
        let registry = super::lock(&context.owner.registry);
        assert!(matches!(registry.phase, super::OwnerPhase::Closed));
        assert!(
            !registry.shutdown_active && registry.starting.is_empty() && registry.tasks.is_empty(),
            "successful shutdown closes and releases leadership in one empty-registry transition"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_shutdown_while_waiting_for_starting_releases_its_leadership() {
        let context = initialized_context().await;
        let key = context
            .owner
            .reserve_start()
            .expect("the fresh owner reserves a starting registration");

        let mut first = Box::pin(context.shutdown());
        assert!(poll_once_pending(first.as_mut()).await);
        drop(first);
        {
            let registry = super::lock(&context.owner.registry);
            assert!(matches!(registry.phase, super::OwnerPhase::Closing));
            assert!(!registry.shutdown_active);
        }

        context.owner.fail_start(key);
        context.shutdown().await;
        assert!(matches!(
            super::lock(&context.owner.registry).phase,
            super::OwnerPhase::Closed
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_closing_request_does_not_consume_the_later_shutdown_leadership() {
        let context = initialized_context().await;

        context.request_closing();
        let rejected = context.spawn_blocking_tracked(|| 3_u8);
        assert_eq!(rejected.wait().await, Err(RuntimeTaskError::OwnerClosing));
        assert_eq!(
            context
                .spawn_tracked(async {})
                .expect_err("closing rejects async work"),
            RuntimeTaskError::OwnerClosing
        );

        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_waits_for_a_reserved_async_starting_registration_to_install_and_join() {
        let context = initialized_context().await;
        let key = context
            .owner
            .reserve_start()
            .expect("a fresh owner accepts the starting registration");

        let mut shutdown = std::pin::pin!(context.shutdown());
        assert!(poll_once_pending(shutdown.as_mut()).await);

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let settlement = Arc::new(super::Settlement::new());
        let entered_by_operation = Arc::clone(&entered);
        let released_by_test = Arc::clone(&release);
        let settlement_by_operation = Arc::clone(&settlement);
        let handle = context.owner.handle.spawn(async move {
            entered_by_operation.notify_one();
            released_by_test.notified().await;
            settlement_by_operation.resolve(Ok(()));
        });
        let join_failure_settlement: Arc<dyn super::JoinFailureSettlement> = settlement.clone();
        context
            .owner
            .install_async(key, handle, join_failure_settlement);
        entered.notified().await;
        assert!(poll_once_pending(shutdown.as_mut()).await);
        release.notify_one();
        shutdown.await;
        assert_eq!(settlement.wait().await, Ok(()));
    }

    #[test]
    fn system_clock_truncates_instead_of_rounding_to_milliseconds() {
        let timestamp = SystemClock::timestamp_from_utc(
            OffsetDateTime::parse("2026-08-03T12:34:56.987654321Z", &Rfc3339)
                .expect("the fixture timestamp is valid"),
        );

        assert_eq!(timestamp.to_string(), "2026-08-03T12:34:56.987Z");
        assert_eq!(SystemClock.now().as_datetime().nanosecond() % 1_000_000, 0);
    }
}
