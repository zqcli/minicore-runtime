use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::durable_state::{DurableOpenError, DurableState};
use crate::runtime_task::RuntimeTaskContext;

/// Host configuration for a MiniCore runtime instance.
#[non_exhaustive]
pub struct MiniCoreRuntimeConfig {
    durable_root: PathBuf,
}

impl MiniCoreRuntimeConfig {
    pub fn new(durable_root: PathBuf) -> Self {
        Self { durable_root }
    }
}

impl fmt::Debug for MiniCoreRuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MiniCoreRuntimeConfig { .. }")
    }
}

/// A closed, redacted failure result from runtime initialization.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum RuntimeInitializationError {
    RuntimeDependencyUnavailable,
    StoreInUse,
    UnsupportedStoreFormat,
    DurableStateCorrupt,
    DurableStateTooLarge,
    StorageUnavailable,
}

impl fmt::Debug for RuntimeInitializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RuntimeDependencyUnavailable => "RuntimeDependencyUnavailable",
            Self::StoreInUse => "StoreInUse",
            Self::UnsupportedStoreFormat => "UnsupportedStoreFormat",
            Self::DurableStateCorrupt => "DurableStateCorrupt",
            Self::DurableStateTooLarge => "DurableStateTooLarge",
            Self::StorageUnavailable => "StorageUnavailable",
        })
    }
}

impl fmt::Display for RuntimeInitializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RuntimeDependencyUnavailable => "runtime dependency unavailable",
            Self::StoreInUse => "durable store is already in use",
            Self::UnsupportedStoreFormat => "durable store format is unsupported",
            Self::DurableStateCorrupt => "durable state is corrupt",
            Self::DurableStateTooLarge => "durable state is too large",
            Self::StorageUnavailable => "durable storage unavailable",
        })
    }
}

impl Error for RuntimeInitializationError {}

/// The host lifecycle facade for the currently supported Store V1 runtime foundation.
pub struct MiniCoreRuntime {
    inner: Arc<RuntimeInner>,
}

impl MiniCoreRuntime {
    pub async fn open(
        config: MiniCoreRuntimeConfig,
        handle: Handle,
    ) -> Result<Self, RuntimeInitializationError> {
        let task_context = RuntimeTaskContext::new(handle)
            .await
            .map_err(|_| RuntimeInitializationError::RuntimeDependencyUnavailable)?;
        let durable_state =
            match DurableState::open(config.durable_root, task_context.clone()).await {
                Ok(durable_state) => durable_state,
                Err(error) => {
                    task_context.shutdown().await;
                    return Err(error.into());
                }
            };

        let inner = Arc::new(RuntimeInner::new(task_context, durable_state));
        inner.retain_until_shutdown();
        Ok(Self { inner })
    }

    /// Closes admission, joins accepted work, and releases the Store V1 root lease.
    ///
    /// Hosts must await this before tearing down the injected Tokio runtime.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

impl fmt::Debug for MiniCoreRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MiniCoreRuntime { .. }")
    }
}

impl Drop for MiniCoreRuntime {
    fn drop(&mut self) {
        self.inner.request_closing();
    }
}

impl From<DurableOpenError> for RuntimeInitializationError {
    fn from(error: DurableOpenError) -> Self {
        match error {
            DurableOpenError::StoreInUse => Self::StoreInUse,
            DurableOpenError::UnsupportedStoreFormat => Self::UnsupportedStoreFormat,
            DurableOpenError::DurableStateCorrupt => Self::DurableStateCorrupt,
            DurableOpenError::DurableStateTooLarge => Self::DurableStateTooLarge,
            DurableOpenError::StorageUnavailable => Self::StorageUnavailable,
        }
    }
}

struct RuntimeInner {
    task_context: RuntimeTaskContext,
    retained_until_shutdown: Mutex<Option<Arc<RuntimeInner>>>,
    durable_state: Mutex<Option<DurableState>>,
    lifecycle: Mutex<RuntimeLifecycle>,
    lifecycle_changed: Notify,
}

impl RuntimeInner {
    fn new(task_context: RuntimeTaskContext, durable_state: DurableState) -> Self {
        Self {
            task_context,
            retained_until_shutdown: Mutex::new(None),
            durable_state: Mutex::new(Some(durable_state)),
            lifecycle: Mutex::new(RuntimeLifecycle::Open),
            lifecycle_changed: Notify::new(),
        }
    }

    // A dropped facade must only request Closing; it cannot release the lease before an
    // awaited shutdown has drained the owner. Explicit shutdown breaks this retention.
    fn retain_until_shutdown(self: &Arc<Self>) {
        *lock(&self.retained_until_shutdown) = Some(Arc::clone(self));
    }

    fn request_closing(&self) {
        let mut lifecycle = lock(&self.lifecycle);
        let changed = *lifecycle == RuntimeLifecycle::Open;
        if *lifecycle == RuntimeLifecycle::Open {
            *lifecycle = RuntimeLifecycle::Closing {
                shutdown_active: false,
            };
        }
        drop(lifecycle);
        self.request_durable_actor_closing();
        self.task_context.request_closing();
        if changed {
            self.lifecycle_changed.notify_waiters();
        }
    }

    async fn shutdown(self: &Arc<Self>) {
        loop {
            // Register before inspecting leadership so a cancelled leader cannot clear its
            // claim between our inspection and this wait.
            let notified = self.lifecycle_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            match self.begin_shutdown() {
                RuntimeShutdownAttempt::Leader(mut leadership) => {
                    self.request_durable_actor_closing();
                    self.task_context.request_closing();

                    // Keep the original owner in the mutex while awaiting. A cancelled leader
                    // therefore retains both the DurableState resource owner and its root lease
                    // for the next shutdown leader to take over.
                    let durable_state = lock(&self.durable_state).as_ref().cloned();
                    if let Some(durable_state) = durable_state {
                        durable_state.close().await;
                        let removed = lock(&self.durable_state).take();
                        drop(removed);
                    } else {
                        self.task_context.shutdown().await;
                    }
                    self.complete_shutdown();
                    leadership.complete();
                    return;
                }
                RuntimeShutdownAttempt::Closed => return,
                RuntimeShutdownAttempt::Waiting => notified.await,
            }
        }
    }

    fn request_durable_actor_closing(&self) {
        if let Some(durable_state) = lock(&self.durable_state).as_ref() {
            durable_state.request_closing();
        }
    }

    fn begin_shutdown(self: &Arc<Self>) -> RuntimeShutdownAttempt {
        let mut lifecycle = lock(&self.lifecycle);
        match *lifecycle {
            RuntimeLifecycle::Open => {
                *lifecycle = RuntimeLifecycle::Closing {
                    shutdown_active: true,
                };
                RuntimeShutdownAttempt::Leader(RuntimeShutdownLeadership::new(Arc::clone(self)))
            }
            RuntimeLifecycle::Closing {
                shutdown_active: false,
            } => {
                *lifecycle = RuntimeLifecycle::Closing {
                    shutdown_active: true,
                };
                RuntimeShutdownAttempt::Leader(RuntimeShutdownLeadership::new(Arc::clone(self)))
            }
            RuntimeLifecycle::Closing {
                shutdown_active: true,
            } => RuntimeShutdownAttempt::Waiting,
            RuntimeLifecycle::Closed => RuntimeShutdownAttempt::Closed,
        }
    }

    fn complete_shutdown(&self) {
        let mut lifecycle = lock(&self.lifecycle);
        *lifecycle = RuntimeLifecycle::Closed;
        drop(lifecycle);
        self.lifecycle_changed.notify_waiters();
        let retained = lock(&self.retained_until_shutdown).take();
        drop(retained);
    }
}

enum RuntimeShutdownAttempt {
    Leader(RuntimeShutdownLeadership),
    Waiting,
    Closed,
}

/// Holds the runtime shutdown claim until close completes or its caller cancels the future.
struct RuntimeShutdownLeadership {
    inner: Arc<RuntimeInner>,
    completed: bool,
}

impl RuntimeShutdownLeadership {
    fn new(inner: Arc<RuntimeInner>) -> Self {
        Self {
            inner,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RuntimeShutdownLeadership {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut lifecycle = lock(&self.inner.lifecycle);
        let was_active = matches!(
            *lifecycle,
            RuntimeLifecycle::Closing {
                shutdown_active: true
            }
        );
        if was_active {
            *lifecycle = RuntimeLifecycle::Closing {
                shutdown_active: false,
            };
        }
        drop(lifecycle);
        if was_active {
            self.inner.lifecycle_changed.notify_waiters();
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RuntimeLifecycle {
    Open,
    Closing { shutdown_active: bool },
    Closed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::future::{Future, poll_fn};
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::runtime::Handle;
    use tokio::sync::Notify;

    use super::{
        MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError, RuntimeLifecycle,
    };
    use crate::runtime_task::RuntimeTaskError;

    static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            loop {
                let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
                assert_ne!(suffix, 0, "test root suffix must be nonzero");
                let path = std::env::temp_dir().join(format!(
                    "minicore-runtime-lifecycle-{}-{suffix}",
                    std::process::id()
                ));
                if !path.exists() {
                    return Self { path };
                }
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            if self.path.exists() {
                fs::remove_dir_all(&self.path)
                    .expect("the temporary runtime root is removed deterministically");
            }
        }
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
    async fn facade_drop_requests_closing_but_a_remaining_facade_can_settle_and_release() {
        let root = TempRoot::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
        let remaining_facade = MiniCoreRuntime {
            inner: std::sync::Arc::clone(&runtime.inner),
        };

        drop(runtime);

        assert_eq!(
            remaining_facade
                .inner
                .task_context
                .spawn_tracked(async {})
                .expect_err("facade drop closes task admission"),
            RuntimeTaskError::OwnerClosing
        );
        assert!(matches!(
            MiniCoreRuntime::open(
                MiniCoreRuntimeConfig::new(root.path().to_owned()),
                Handle::current(),
            )
            .await,
            Err(RuntimeInitializationError::StoreInUse)
        ));

        remaining_facade.shutdown().await;

        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("an explicit shutdown after facade drop releases the lease");
        reopened.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_shutdown_keeps_the_lease_and_lets_a_later_leader_finish_and_reopen() {
        let root = TempRoot::new();
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the runtime opens");
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered_by_task = Arc::clone(&entered);
        let release_by_task = Arc::clone(&release);
        let task = runtime
            .inner
            .task_context
            .spawn_tracked(async move {
                entered_by_task.notify_one();
                release_by_task.notified().await;
            })
            .expect("the open runtime admits owner-retained work");
        entered.notified().await;

        let mut first = Box::pin(runtime.shutdown());
        assert!(poll_once_pending(first.as_mut()).await);
        drop(first);

        assert!(matches!(
            *super::lock(&runtime.inner.lifecycle),
            RuntimeLifecycle::Closing {
                shutdown_active: false
            }
        ));
        assert!(super::lock(&runtime.inner.durable_state).is_some());
        assert!(matches!(
            MiniCoreRuntime::open(
                MiniCoreRuntimeConfig::new(root.path().to_owned()),
                Handle::current(),
            )
            .await,
            Err(RuntimeInitializationError::StoreInUse)
        ));

        let mut second = Box::pin(runtime.shutdown());
        assert!(poll_once_pending(second.as_mut()).await);
        release.notify_one();
        second.await;
        assert_eq!(task.wait().await, Ok(()));

        let reopened = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            Handle::current(),
        )
        .await
        .expect("the second shutdown leader releases the retained lease");
        reopened.shutdown().await;
    }
}
