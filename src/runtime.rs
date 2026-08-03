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

/// The host lifecycle facade for an empty Store V1 runtime.
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
        self.task_context.request_closing();
        let mut lifecycle = lock(&self.lifecycle);
        if *lifecycle == RuntimeLifecycle::Open {
            *lifecycle = RuntimeLifecycle::Closing {
                shutdown_active: false,
            };
        }
    }

    async fn shutdown(&self) {
        let leader = self.begin_shutdown();
        self.task_context.request_closing();
        if leader {
            self.task_context.shutdown().await;
            let durable_state = lock(&self.durable_state).take();
            if let Some(durable_state) = durable_state {
                durable_state.close().await;
            }
            self.complete_shutdown();
        } else {
            self.wait_for_shutdown().await;
        }
    }

    fn begin_shutdown(&self) -> bool {
        let mut lifecycle = lock(&self.lifecycle);
        match *lifecycle {
            RuntimeLifecycle::Open => {
                *lifecycle = RuntimeLifecycle::Closing {
                    shutdown_active: true,
                };
                true
            }
            RuntimeLifecycle::Closing {
                shutdown_active: false,
            } => {
                *lifecycle = RuntimeLifecycle::Closing {
                    shutdown_active: true,
                };
                true
            }
            RuntimeLifecycle::Closing {
                shutdown_active: true,
            }
            | RuntimeLifecycle::Closed => false,
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

    async fn wait_for_shutdown(&self) {
        loop {
            let notified = self.lifecycle_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if *lock(&self.lifecycle) == RuntimeLifecycle::Closed {
                return;
            }
            notified.await;
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
