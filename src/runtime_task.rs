use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard};

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

/// A redacted terminal outcome for a tracked blocking operation.
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
                return TrackedBlockingJob::new(settlement, RegistryKey::REJECTED);
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

        TrackedBlockingJob::new(settlement, key)
    }

    /// Closes admission and joins every operation reserved while the owner was open.
    pub(crate) async fn shutdown(&self) {
        let leader = self.owner.begin_shutdown();
        self.owner.cancellation.cancel();
        if leader {
            self.owner.finish_shutdown().await;
        } else {
            self.owner.shutdown_complete.wait().await;
        }
    }
}

/// Opaque caller-side access to one owner-tracked blocking operation.
#[derive(Clone)]
pub(crate) struct TrackedBlockingJob<T>
where
    T: Clone + Send + 'static,
{
    settlement: Arc<Settlement<T>>,
    _registry_key: RegistryKey,
}

impl<T> TrackedBlockingJob<T>
where
    T: Clone + Send + 'static,
{
    fn new(settlement: Arc<Settlement<T>>, registry_key: RegistryKey) -> Self {
        Self {
            settlement,
            _registry_key: registry_key,
        }
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
    shutdown_complete: Completion,
}

impl RuntimeTaskOwner {
    fn new(handle: Handle) -> Self {
        Self {
            handle,
            cancellation: CancellationToken::new(),
            registry: Mutex::new(Registry::new()),
            registry_changed: Notify::new(),
            shutdown_complete: Completion::new(),
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
        let mut registry = lock(&self.registry);
        registry.starting.remove(&key);
        registry
            .blocking
            .insert(key, RegisteredBlockingTask { handle, settlement });
        drop(registry);
        self.registry_changed.notify_waiters();
    }

    fn fail_start(&self, key: RegistryKey) {
        let mut registry = lock(&self.registry);
        registry.starting.remove(&key);
        drop(registry);
        self.registry_changed.notify_waiters();
    }

    fn begin_shutdown(&self) -> bool {
        let mut registry = lock(&self.registry);
        if registry.phase == OwnerPhase::Open {
            registry.phase = OwnerPhase::Closing;
            true
        } else {
            false
        }
    }

    async fn finish_shutdown(&self) {
        let blocking = loop {
            let notified = self.registry_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let blocking = {
                let mut registry = lock(&self.registry);
                if registry.starting.is_empty() {
                    Some(std::mem::take(&mut registry.blocking))
                } else {
                    None
                }
            };
            if let Some(blocking) = blocking {
                break blocking;
            }
            notified.await;
        };

        // Completed raw handles intentionally remain in the owner registry until shutdown. This
        // initial foundation has no detached task or persistent executor/reaper of its own.
        for (_, task) in blocking {
            if task.handle.await.is_err() {
                task.settlement.settle_join_failure();
            }
        }

        let mut registry = lock(&self.registry);
        registry.phase = OwnerPhase::Closed;
        drop(registry);
        self.shutdown_complete.complete();
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct RegistryKey(u64);

impl RegistryKey {
    const REJECTED: Self = Self(0);
}

struct Registry {
    phase: OwnerPhase,
    next_key: u64,
    starting: BTreeSet<RegistryKey>,
    probes: BTreeMap<RegistryKey, JoinHandle<()>>,
    blocking: BTreeMap<RegistryKey, RegisteredBlockingTask>,
}

impl Registry {
    fn new() -> Self {
        Self {
            phase: OwnerPhase::Open,
            next_key: 1,
            starting: BTreeSet::new(),
            probes: BTreeMap::new(),
            blocking: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OwnerPhase {
    Open,
    Closing,
    Closed,
}

struct RegisteredBlockingTask {
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

struct Completion {
    complete: Mutex<bool>,
    changed: Notify,
}

impl Completion {
    fn new() -> Self {
        Self {
            complete: Mutex::new(false),
            changed: Notify::new(),
        }
    }

    fn complete(&self) {
        let mut complete = lock(&self.complete);
        if !*complete {
            *complete = true;
            drop(complete);
            self.changed.notify_waiters();
        }
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if *lock(&self.complete) {
                return;
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
    async fn shutdown_waits_for_a_reserved_starting_registration_to_install_and_join() {
        let context = initialized_context().await;
        let key = context
            .owner
            .reserve_start()
            .expect("a fresh owner accepts the starting registration");

        let mut shutdown = std::pin::pin!(context.shutdown());
        assert!(poll_once_pending(shutdown.as_mut()).await);

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let settlement = Arc::new(super::Settlement::new());
        let entered_by_operation = Arc::clone(&entered);
        let released_by_test = Arc::clone(&release);
        let settlement_by_operation = Arc::clone(&settlement);
        let handle = context.owner.handle.spawn_blocking(move || {
            entered_by_operation.wait();
            released_by_test.wait();
            settlement_by_operation.resolve(Ok(()));
        });
        let join_failure_settlement: Arc<dyn super::JoinFailureSettlement> = settlement.clone();
        context
            .owner
            .install_blocking(key, handle, join_failure_settlement);
        entered.wait();
        assert!(poll_once_pending(shutdown.as_mut()).await);
        release.wait();
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
