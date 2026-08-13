use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
#[cfg(test)]
use std::sync::Barrier;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use time::OffsetDateTime;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

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
        let context = Self { owner };
        let probe = context
            .spawn_tracked(async {
                tokio::time::sleep(std::time::Duration::ZERO).await;
            })
            .map_err(|_| RuntimeDependencyUnavailable)?;
        if probe.join_registered().await.is_err() {
            return Err(RuntimeDependencyUnavailable);
        }

        Ok(context)
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
                return TrackedBlockingJob::rejected(settlement);
            }
        };

        let job_owner = Arc::clone(&self.owner);
        let task_owner = Arc::clone(&self.owner);
        let task_settlement = Arc::clone(&settlement);
        let spawned = catch_unwind(AssertUnwindSafe(|| {
            #[cfg(test)]
            if let Some(handle) = self.owner.spawn_blocking_with_injected_failure() {
                return handle;
            }
            #[cfg(test)]
            let entry_gate = self.owner.take_next_blocking_job_entry_gate();
            #[cfg(test)]
            let panic_after_operation = self.owner.take_post_operation_panic();
            self.owner.handle.spawn_blocking(move || {
                // The task owns the owner while it is running, so dropping every caller-side
                // context or job cannot detach an admitted blocking operation.
                let _owner = task_owner;
                #[cfg(test)]
                if let Some(entry_gate) = entry_gate {
                    // The worker has entered the spawned job: hold it before the operation
                    // closure so a test can deterministically observe the exact scheduled
                    // job while it is held in flight.
                    entry_gate.hold_worker();
                }
                let outcome = catch_unwind(AssertUnwindSafe(operation))
                    .map_err(|_| RuntimeTaskError::OperationPanicked);
                task_settlement.resolve(outcome);
                #[cfg(test)]
                if panic_after_operation {
                    // The operation ran and settled its result; unwind after its catch_unwind
                    // so the exact raw Tokio join reports a real failure.
                    panic!("injected post-operation join failure");
                }
            })
        }));

        match spawned {
            Ok(handle) => {
                let join_failure_settlement: Arc<dyn JoinFailureSettlement> = settlement.clone();
                self.owner
                    .install_task(key, handle, join_failure_settlement);
            }
            Err(_) => {
                self.owner.fail_start(key);
                settlement.resolve(Err(RuntimeTaskError::WorkerUnavailable));
                return TrackedBlockingJob::rejected(settlement);
            }
        }

        TrackedBlockingJob::new(job_owner, key, settlement)
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
                    .install_task(key, handle, join_failure_settlement);
                Ok(TrackedTask::new(Arc::clone(&self.owner), key, settlement))
            }
            Err(_) => {
                self.owner.fail_start(key);
                settlement.resolve(Err(RuntimeTaskError::WorkerUnavailable));
                Err(RuntimeTaskError::WorkerUnavailable)
            }
        }
    }

    /// Reaps a caller-dropped tracked task on the injected runtime handle.  The raw task remains
    /// owner-registered until this waiter joins it, so detached command owners still participate
    /// in orderly shutdown while their registry slot is released after normal completion.
    pub(crate) fn reap_tracked(&self, task: TrackedTask) {
        drop(self.owner.handle.spawn(async move {
            let _ = task.wait().await;
        }));
    }

    /// Reports whether this owner has stopped accepting new tracked work.
    pub(crate) fn is_closing(&self) -> bool {
        self.owner.is_closing()
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

    /// Test-only one-shot seam: the next admitted blocking job joins as an immediate worker
    /// cancellation without ever running its operation closure.  The fault is consumed by that
    /// exact `spawn_blocking_tracked` admission and never touches any other registered task.
    #[cfg(test)]
    pub(crate) fn inject_next_blocking_job_join_failure(&self) {
        self.owner.inject_next_blocking_job_join_failure();
    }

    /// Test-only one-shot seam: the next admitted blocking job runs its operation closure to
    /// completion and settles its result, then unwinds after the operation's `catch_unwind` so
    /// the exact raw Tokio join reports a real failure.  The fault is consumed by that exact
    /// `spawn_blocking_tracked` admission, runs only inside the spawned worker, and never
    /// touches the parent/caller task or any other registered task.
    #[cfg(test)]
    pub(crate) fn inject_next_blocking_job_post_operation_panic(&self) {
        self.owner.inject_next_blocking_job_post_operation_panic();
    }

    /// Test-only one-shot seam: the exact next `spawn_blocking_tracked` admission holds its
    /// worker inside the spawned job after entry and before the operation closure until the
    /// returned controller releases it.  The controller observes the worker's entry and the
    /// release as deterministic barrier rendezvous (no sleeps, timeouts, or polling), so a
    /// test can prove a cancellation arrived while the exact job was scheduled and held in
    /// flight.  The gate is consumed by that exact ordinary admission and never touches any
    /// other registered task; the existing join-failure and post-operation-panic seams remain
    /// independently one-shot.
    #[cfg(test)]
    pub(crate) fn arm_next_blocking_job_entry_gate(&self) -> BlockingJobEntryController {
        self.owner.arm_next_blocking_job_entry_gate()
    }

    #[cfg(test)]
    pub(crate) fn registered_task_count_for_test(&self) -> usize {
        let registry = lock(&self.owner.registry);
        registry.starting.len() + registry.joining.len() + registry.tasks.len()
    }

    /// Closes admission and joins every operation reserved while the owner was open.
    pub(crate) async fn shutdown(&self) {
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
    owner: Arc<RuntimeTaskOwner>,
    key: RegistryKey,
    settlement: Arc<Settlement<()>>,
}

impl TrackedTask {
    fn new(
        owner: Arc<RuntimeTaskOwner>,
        key: RegistryKey,
        settlement: Arc<Settlement<()>>,
    ) -> Self {
        Self {
            owner,
            key,
            settlement,
        }
    }

    /// Joins this task's exact owner registration before observing its shared settlement.
    ///
    /// This is intentionally private: initialization uses the exact-registration failure result,
    /// while normal crate-private waiters may share the same settlement path.
    async fn join_registered(&self) -> Result<(), RuntimeTaskError> {
        let task = self
            .owner
            .take_registered(self.key)
            .ok_or(RuntimeTaskError::WorkerUnavailable)?;
        let mut join = SingleRegisteredTaskJoinGuard::new(Arc::clone(&self.owner), self.key, task);
        let join_failed = join.join().await.is_err();
        let task = join.finish(join_failed);
        drop(task);
        self.settlement.wait().await
    }

    pub(crate) async fn wait(&self) -> Result<(), RuntimeTaskError> {
        loop {
            // A caller-side close may race an admitted task before its first poll.  Both
            // notifications must be enabled before inspecting either source: the first waiter
            // may be cancelled while it owns the exact raw handle, and its guard then restores
            // that handle by notifying the registry rather than the shared settlement.
            let registry_changed = self.owner.registry_changed.notified();
            tokio::pin!(registry_changed);
            registry_changed.as_mut().enable();
            let settlement_changed = self.settlement.changed.notified();
            tokio::pin!(settlement_changed);
            settlement_changed.as_mut().enable();

            if let Some(task) = self.owner.take_registered(self.key) {
                let mut join =
                    SingleRegisteredTaskJoinGuard::new(Arc::clone(&self.owner), self.key, task);
                let join_failed = join.join().await.is_err();
                let task = join.finish(join_failed);
                drop(task);
                continue;
            }

            if let Some(outcome) = self.settlement.current() {
                if !self.owner.has_registration(self.key) {
                    return outcome;
                }
                // The shared result can settle before the owner finishes reaping the exact raw
                // handle. Do not wait on the already-ready settlement notification: only the
                // registry transition can make this exact waiter eligible to return.
                registry_changed.await;
                continue;
            }

            tokio::select! {
                biased;
                _ = registry_changed => {}
                _ = settlement_changed => {}
            }
        }
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
    owner: Option<Arc<RuntimeTaskOwner>>,
    key: Option<RegistryKey>,
    settlement: Arc<Settlement<T>>,
}

impl<T> TrackedBlockingJob<T>
where
    T: Clone + Send + 'static,
{
    fn new(owner: Arc<RuntimeTaskOwner>, key: RegistryKey, settlement: Arc<Settlement<T>>) -> Self {
        Self {
            owner: Some(owner),
            key: Some(key),
            settlement,
        }
    }

    fn rejected(settlement: Arc<Settlement<T>>) -> Self {
        Self {
            owner: None,
            key: None,
            settlement,
        }
    }

    pub(crate) async fn wait(&self) -> Result<T, RuntimeTaskError> {
        let (Some(owner), Some(key)) = (&self.owner, self.key) else {
            return self.settlement.wait().await;
        };

        loop {
            // A cloned waiter must not observe an operation's provisional settlement while
            // another waiter still owns the exact raw join. A raw task can fail after the
            // operation settled, and that terminal join failure must win for every waiter.
            let registry_changed = owner.registry_changed.notified();
            tokio::pin!(registry_changed);
            registry_changed.as_mut().enable();
            let settlement_changed = self.settlement.changed.notified();
            tokio::pin!(settlement_changed);
            settlement_changed.as_mut().enable();

            if let Some(task) = owner.take_registered(key) {
                let mut join = SingleRegisteredTaskJoinGuard::new(Arc::clone(owner), key, task);
                let join_failed = join.join().await.is_err();
                let task = join.finish(join_failed);
                drop(task);
                continue;
            }

            if let Some(outcome) = self.settlement.current() {
                if !owner.has_registration(key) {
                    return outcome;
                }
                registry_changed.await;
                continue;
            }

            tokio::select! {
                biased;
                _ = registry_changed => {}
                _ = settlement_changed => {}
            }
        }
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

/// Test-only one-shot controller for the exact next blocking job admission: the worker
/// signals its entry and then blocks inside the spawned job, before the operation closure,
/// until the test releases it.  Both sides are `std::sync::Barrier` pairs, so entry
/// observation and release are deterministic rendezvous points, never sleeps or polls.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct BlockingJobEntryController {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

#[cfg(test)]
impl BlockingJobEntryController {
    fn new() -> Self {
        Self {
            entered: Arc::new(Barrier::new(2)),
            release: Arc::new(Barrier::new(2)),
        }
    }

    /// Deterministically observes the gated worker's entry: returns exactly when the worker
    /// has entered the spawned job and is blocked before its operation closure.
    pub(crate) fn wait_until_entered(&self) {
        self.entered.wait();
    }

    /// Releases the held worker so it invokes its operation closure and settles.  Returns
    /// exactly when the worker has passed the gate.
    pub(crate) fn release(&self) {
        self.release.wait();
    }

    /// The worker side of the gate: signal entry, then block until the test releases.
    fn hold_worker(&self) {
        self.entered.wait();
        self.release.wait();
    }
}

struct RuntimeTaskOwner {
    handle: Handle,
    registry: Mutex<Registry>,
    registry_changed: Notify,
    #[cfg(test)]
    next_blocking_job_join_failure: AtomicBool,
    #[cfg(test)]
    next_blocking_job_post_operation_panic: AtomicBool,
    #[cfg(test)]
    next_blocking_job_entry_gate: Mutex<Option<BlockingJobEntryController>>,
}

impl RuntimeTaskOwner {
    fn new(handle: Handle) -> Self {
        Self {
            handle,
            registry: Mutex::new(Registry::new()),
            registry_changed: Notify::new(),
            #[cfg(test)]
            next_blocking_job_join_failure: AtomicBool::new(false),
            #[cfg(test)]
            next_blocking_job_post_operation_panic: AtomicBool::new(false),
            #[cfg(test)]
            next_blocking_job_entry_gate: Mutex::new(None),
        }
    }

    /// Test-only one-shot join-failure injection for the next blocking job admission.
    #[cfg(test)]
    fn inject_next_blocking_job_join_failure(&self) {
        self.next_blocking_job_join_failure
            .store(true, Ordering::Release);
    }

    /// Test-only one-shot post-operation panic injection for the next blocking job admission.
    #[cfg(test)]
    fn inject_next_blocking_job_post_operation_panic(&self) {
        self.next_blocking_job_post_operation_panic
            .store(true, Ordering::Release);
    }

    /// Test-only one-shot entry-gate arming for the next blocking job admission: stores a
    /// private controller and returns its test-side clone, so the test and the exact next
    /// worker share the same barrier pair.
    #[cfg(test)]
    fn arm_next_blocking_job_entry_gate(&self) -> BlockingJobEntryController {
        let controller = BlockingJobEntryController::new();
        *lock(&self.next_blocking_job_entry_gate) = Some(controller.clone());
        controller
    }

    /// Test-only one-shot consumption of the entry gate: `Some` holds the exact next admitted
    /// blocking operation's worker at the gate until its controller releases it.
    #[cfg(test)]
    fn take_next_blocking_job_entry_gate(&self) -> Option<BlockingJobEntryController> {
        lock(&self.next_blocking_job_entry_gate).take()
    }

    /// Test-only one-shot consumption of the post-operation panic seam: `true` arms the next
    /// admitted blocking operation to unwind after it settles its result.
    #[cfg(test)]
    fn take_post_operation_panic(&self) -> bool {
        self.next_blocking_job_post_operation_panic
            .swap(false, Ordering::AcqRel)
    }

    /// Test-only: when the one-shot fault is armed, the next `spawn_blocking_tracked` installs
    /// an immediately-aborted pending async raw handle as its owner registration, so the exact
    /// owner join settles `WorkerUnavailable` without the operation closure ever running.
    #[cfg(test)]
    fn spawn_blocking_with_injected_failure(&self) -> Option<JoinHandle<()>> {
        if self
            .next_blocking_job_join_failure
            .swap(false, Ordering::AcqRel)
        {
            let handle = self.handle.spawn(std::future::pending::<()>());
            handle.abort();
            Some(handle)
        } else {
            None
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

    fn take_registered(&self, key: RegistryKey) -> Option<RegisteredTask> {
        let mut registry = lock(&self.registry);
        let task = registry.tasks.remove(&key)?;
        let inserted = registry.joining.insert(key);
        debug_assert!(inserted, "one waiter joins one exact owner task");
        Some(task)
    }

    fn has_registration(&self, key: RegistryKey) -> bool {
        let registry = lock(&self.registry);
        registry.starting.contains(&key)
            || registry.tasks.contains_key(&key)
            || registry.joining.contains(&key)
    }

    fn finish_registered_join(&self, key: RegistryKey) {
        let mut registry = lock(&self.registry);
        let removed = registry.joining.remove(&key);
        debug_assert!(removed, "a completed join clears its exact owner slot");
        drop(registry);
        self.registry_changed.notify_waiters();
    }

    fn fail_start(&self, key: RegistryKey) {
        let mut registry = lock(&self.registry);
        registry.starting.remove(&key);
        drop(registry);
        self.registry_changed.notify_waiters();
    }

    fn is_closing(&self) -> bool {
        let registry = lock(&self.registry);
        registry.phase != OwnerPhase::Open
    }

    fn request_closing(&self) {
        let mut registry = lock(&self.registry);
        let changed = registry.phase == OwnerPhase::Open;
        if registry.phase == OwnerPhase::Open {
            registry.phase = OwnerPhase::Closing;
        }
        drop(registry);
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
                if !registry.starting.is_empty() || !registry.joining.is_empty() {
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
                    assert!(
                        registry.joining.is_empty(),
                        "shutdown cannot close with caller-joined tasks"
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
    joining: BTreeSet<RegistryKey>,
    tasks: BTreeMap<RegistryKey, RegisteredTask>,
}

impl Registry {
    fn new() -> Self {
        Self {
            phase: OwnerPhase::Open,
            shutdown_active: false,
            next_key: 1,
            starting: BTreeSet::new(),
            joining: BTreeSet::new(),
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

/// Owns one exact registered task while its raw join handle is being awaited.
///
/// Dropping this guard while the join is still pending restores the same raw handle and
/// settlement to the same registry key, so cancellation cannot detach or orphan the operation.
struct SingleRegisteredTaskJoinGuard {
    owner: Arc<RuntimeTaskOwner>,
    key: RegistryKey,
    task: Option<RegisteredTask>,
}

impl SingleRegisteredTaskJoinGuard {
    fn new(owner: Arc<RuntimeTaskOwner>, key: RegistryKey, task: RegisteredTask) -> Self {
        Self {
            owner,
            key,
            task: Some(task),
        }
    }

    async fn join(&mut self) -> Result<(), tokio::task::JoinError> {
        Pin::new(
            &mut self
                .task
                .as_mut()
                .expect("a single join guard owns one registered task")
                .handle,
        )
        .await
    }

    fn finish(&mut self, join_failed: bool) -> RegisteredTask {
        let task = self
            .task
            .take()
            .expect("a single registered task join finishes only once");
        if join_failed {
            // Publish the terminal join result before making the exact registration disappear.
            // Shared waiters may already see a provisional operation result, so waking them via
            // the registry transition first would allow that result to escape.
            task.settlement.settle_join_failure();
        }
        self.owner.finish_registered_join(self.key);
        task
    }
}

impl Drop for SingleRegisteredTaskJoinGuard {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        let mut registry = lock(&self.owner.registry);
        let removed = registry.joining.remove(&self.key);
        debug_assert!(removed, "a cancelled join restores its exact joining slot");
        let previous = registry.tasks.insert(self.key, task);
        debug_assert!(
            previous.is_none(),
            "an in-flight owner task has one exact registry slot"
        );
        drop(registry);
        self.owner.registry_changed.notify_waiters();
    }
}

trait JoinFailureSettlement: Send + Sync {
    fn settle_join_failure(&self);
}

impl<T> JoinFailureSettlement for Settlement<T>
where
    T: Clone + Send + 'static,
{
    fn settle_join_failure(&self) {
        // A raw join failure is terminal and wins over any result the worker already settled:
        // the operation may have run and physically committed its side effect before the task
        // died, so waiters must observe `WorkerUnavailable` instead of a provisional success.
        // The terminal outcome is idempotent: a repeated join failure keeps the same error.
        let mut stored = lock(&self.outcome);
        let changed = !matches!(
            stored.as_ref(),
            Some(Err(RuntimeTaskError::WorkerUnavailable))
        );
        if changed {
            *stored = Some(Err(RuntimeTaskError::WorkerUnavailable));
            drop(stored);
            self.changed.notify_waiters();
        }
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

    fn current(&self) -> Option<Result<T, RuntimeTaskError>> {
        lock(&self.outcome).clone()
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
        F: Future,
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
    fn closed_injected_handle_returns_a_typed_error_without_pending_forever() {
        let closed_runtime = Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("the closed-handle runtime builds");
        let closed_handle = closed_runtime.handle().clone();
        closed_runtime.shutdown_background();

        let driver = Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("the driver runtime builds");
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            driver.block_on(RuntimeTaskContext::new(closed_handle))
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
    async fn injected_blocking_join_failure_settles_without_running_the_operation() {
        let context = initialized_context().await;
        let runs = Arc::new(AtomicUsize::new(0));
        let runs_by_operation = Arc::clone(&runs);

        context.inject_next_blocking_job_join_failure();
        let job = context.spawn_blocking_tracked(move || {
            runs_by_operation.fetch_add(1, Ordering::SeqCst);
            3_u8
        });

        assert_eq!(job.wait().await, Err(RuntimeTaskError::WorkerUnavailable));
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "the injected join failure aborts the worker before its operation closure runs"
        );

        // The seam is one-shot: the next admission runs its closure normally.
        let next = context.spawn_blocking_tracked(|| 5_u8);
        assert_eq!(next.wait().await, Ok(5));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn injected_post_operation_panic_settles_worker_unavailable_after_the_operation_ran() {
        let context = initialized_context().await;
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_by_operation = Arc::clone(&ran);

        context.inject_next_blocking_job_post_operation_panic();
        let job = context.spawn_blocking_tracked(move || {
            ran_by_operation.fetch_add(1, Ordering::SeqCst);
            7_u8
        });

        assert_eq!(
            job.wait().await,
            Err(RuntimeTaskError::WorkerUnavailable),
            "the raw join failure overrides the operation's provisional success"
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "the injected post-operation panic runs the operation closure to completion first"
        );

        // The seam is one-shot and targets exactly the next admission: the next blocking job
        // runs normally on the same owner, untouched by the consumed fault.
        let next = context.spawn_blocking_tracked(|| 5_u8);
        assert_eq!(next.wait().await, Ok(5));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn armed_blocking_entry_gate_holds_the_exact_next_job_before_its_operation_closure() {
        let context = initialized_context().await;
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_by_operation = Arc::clone(&ran);

        let gate = context.arm_next_blocking_job_entry_gate();
        let job = context.spawn_blocking_tracked(move || {
            ran_by_operation.fetch_add(1, Ordering::SeqCst);
            13_u8
        });

        // The worker has entered the spawned job and is held at the gate before its
        // operation closure: a deterministic barrier rendezvous, no sleeps or polling.
        gate.wait_until_entered();
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "the gated worker is held before its operation closure runs"
        );

        gate.release();
        assert_eq!(job.wait().await, Ok(13));
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "releasing the gate lets the exact operation closure run and settle"
        );

        // The seam is one-shot and targets exactly the next admission: the next blocking job
        // runs normally on the same owner, untouched by the consumed gate.
        let next = context.spawn_blocking_tracked(|| 5_u8);
        assert_eq!(next.wait().await, Ok(5));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_blocking_waiters_do_not_escape_a_provisional_result_before_join_failure() {
        let context = initialized_context().await;
        let key = context
            .owner
            .reserve_start()
            .expect("the open owner reserves the blocking task");
        let settlement = Arc::new(super::Settlement::new());
        let worker_settled = Arc::new(Barrier::new(2));
        let worker_release = Arc::new(Barrier::new(2));
        let settlement_by_worker = Arc::clone(&settlement);
        let settled_by_worker = Arc::clone(&worker_settled);
        let release_by_worker = Arc::clone(&worker_release);
        let handle = context.owner.handle.spawn_blocking(move || {
            settlement_by_worker.resolve(Ok(7_u8));
            settled_by_worker.wait();
            release_by_worker.wait();
            panic!("blocking task fails after its provisional result");
        });
        let join_failure_settlement: Arc<dyn super::JoinFailureSettlement> = settlement.clone();
        context
            .owner
            .install_task(key, handle, join_failure_settlement);
        let job = super::TrackedBlockingJob::new(Arc::clone(&context.owner), key, settlement);
        let shared_waiter = job.clone();

        // The operation result is already present, but the raw task is held before its terminal
        // panic. Waiter A owns that exact join; waiter B must remain pending instead of returning
        // the provisional `Ok(7)`.
        worker_settled.wait();
        let mut waiter_a = Box::pin(job.wait());
        assert!(poll_once_pending(waiter_a.as_mut()).await);
        let mut waiter_b = Box::pin(shared_waiter.wait());
        assert!(poll_once_pending(waiter_b.as_mut()).await);

        worker_release.wait();
        let (first, second) = tokio::join!(waiter_a, waiter_b);
        assert_eq!(first, Err(RuntimeTaskError::WorkerUnavailable));
        assert_eq!(second, Err(RuntimeTaskError::WorkerUnavailable));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_job_wait_reaps_without_becoming_invisible_to_concurrent_shutdown() {
        let context = initialized_context().await;
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let entered_by_operation = Arc::clone(&entered);
        let released_by_test = Arc::clone(&release);
        let job = context.spawn_blocking_tracked(move || {
            entered_by_operation.wait();
            released_by_test.wait();
            17_u8
        });
        entered.wait();

        let mut wait = Box::pin(job.wait());
        assert!(poll_once_pending(wait.as_mut()).await);
        {
            let registry = super::lock(&context.owner.registry);
            assert!(registry.tasks.is_empty());
            assert_eq!(registry.joining.len(), 1);
        }

        let mut shutdown = Box::pin(context.shutdown());
        assert!(
            poll_once_pending(shutdown.as_mut()).await,
            "shutdown waits for a caller-joined blocking task"
        );
        release.wait();
        assert_eq!(wait.await, Ok(17));
        shutdown.await;

        let registry = super::lock(&context.owner.registry);
        assert!(registry.starting.is_empty());
        assert!(registry.joining.is_empty());
        assert!(registry.tasks.is_empty());
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
    async fn reaped_tracked_task_releases_its_owner_registration() {
        let context = initialized_context().await;
        let completed = Arc::new(AtomicBool::new(false));
        let completed_by_task = Arc::clone(&completed);
        let task = context
            .spawn_tracked(async move {
                completed_by_task.store(true, Ordering::SeqCst);
            })
            .expect("an open owner admits asynchronous work");
        context.reap_tracked(task);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if completed.load(Ordering::SeqCst) {
                    let registry = super::lock(&context.owner.registry);
                    if registry.starting.is_empty()
                        && registry.tasks.is_empty()
                        && registry.joining.is_empty()
                    {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached reaper joins and removes the owner registration");
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_registered_join_restores_the_exact_task_for_owner_shutdown() {
        let context = initialized_context().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered_by_task = Arc::clone(&entered);
        let release_by_task = Arc::clone(&release);
        let task = context
            .spawn_tracked(async move {
                entered_by_task.notify_one();
                release_by_task.notified().await;
            })
            .expect("an open owner admits asynchronous work");
        entered.notified().await;

        let mut join = Box::pin(task.join_registered());
        assert!(poll_once_pending(join.as_mut()).await);
        drop(join);

        {
            let registry = super::lock(&context.owner.registry);
            assert!(
                registry.tasks.contains_key(&task.key),
                "cancelling a registered join restores its exact owner slot"
            );
            assert!(
                registry.joining.is_empty(),
                "a cancelled registered join cannot remain hidden from shutdown"
            );
        }

        release.notify_one();
        context.shutdown().await;
        assert_eq!(task.wait().await, Ok(()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_first_shared_waiter_wakes_second_for_an_aborted_pre_poll_task() {
        let context = initialized_context().await;
        // The admitted task is aborted while it is still queued, before its future can construct
        // AsyncTaskSettlementGuard.  Its settlement must therefore come from owner reaping.
        let task_before_first_poll = context
            .spawn_tracked(std::future::pending())
            .expect("the open owner admits the pre-poll task");
        context.abort_latest_registered_task();
        let second_waiter = task_before_first_poll.clone();

        // Waiter A owns the exact registration, but the cancelled raw task has not yet produced
        // its join result.  The poll-once seam leaves A in the cancellation window.
        let mut waiter_a = Box::pin(task_before_first_poll.wait());
        assert!(poll_once_pending(waiter_a.as_mut()).await);
        {
            let registry = super::lock(&context.owner.registry);
            assert_eq!(registry.joining.len(), 1);
            assert!(registry.tasks.is_empty());
        }

        // Waiter B has no settlement notification to await yet.  It must also register the owner
        // notification so A's guard restoration wakes it after A is cancelled.
        let mut waiter_b = Box::pin(second_waiter.wait());
        assert!(poll_once_pending(waiter_b.as_mut()).await);
        drop(waiter_a);

        assert_eq!(waiter_b.await, Err(RuntimeTaskError::WorkerUnavailable));
        context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn settled_shared_waiter_stays_pending_until_a_cancelled_join_is_reaped() {
        let context = initialized_context().await;
        let release = Arc::new(Notify::new());
        let release_by_task = Arc::clone(&release);
        let task = context
            .spawn_tracked(async move {
                release_by_task.notified().await;
            })
            .expect("the open owner admits the asynchronous operation");
        let second_waiter = task.clone();

        // Waiter A owns the exact raw handle while its JoinHandle is still pending. Settle the
        // shared result independently to reproduce the result-before-reap race.
        let mut waiter_a = Box::pin(task.wait());
        assert!(poll_once_pending(waiter_a.as_mut()).await);
        {
            let registry = super::lock(&context.owner.registry);
            assert!(registry.joining.contains(&second_waiter.key));
            assert!(!registry.tasks.contains_key(&second_waiter.key));
        }
        second_waiter.settlement.resolve(Ok(()));

        // B must not observe the ready shared result while A still owns the joining slot.
        let mut waiter_b = Box::pin(second_waiter.wait());
        assert!(poll_once_pending(waiter_b.as_mut()).await);

        // Cancelling A restores the same handle and notifies the registry. Releasing the task
        // then lets B take and reap that restored handle before it returns.
        drop(waiter_a);
        release.notify_one();
        assert_eq!(waiter_b.await, Ok(()));

        let registered_count = {
            let registry = super::lock(&context.owner.registry);
            assert!(!registry.starting.contains(&second_waiter.key));
            assert!(!registry.tasks.contains_key(&second_waiter.key));
            assert!(!registry.joining.contains(&second_waiter.key));
            registry.starting.len() + registry.tasks.len() + registry.joining.len()
        };
        assert_eq!(registered_count, 0);
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
                .next_back()
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

        context.shutdown().await;
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
            .install_task(key, handle, join_failure_settlement);
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
