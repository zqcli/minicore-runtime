use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{KernelConfig, SessionSpec};
use crate::error::{EventStreamTakenError, SessionOpenError, SessionShutdownError};
use crate::ids::{SessionId, SessionInstanceId};
use crate::storage::SessionLog;

use super::actor::{RunnerLifecycle, SessionActorExit};
use super::event_stream::SessionEventStream;
use super::handle::SessionHandle;
use super::runtime_open::{OpenPayload, OpenReady, OpenRequest, SharedOpenPayload, run_open};
use super::runtime_shutdown::{construct_shutdown_timeout, map_actor_exit};
use crate::bindings::SessionBindings;

pub struct SessionRuntimeOptions {
    kernel: KernelConfig,
    bindings: SessionBindings,
    task_runtime: Handle,
}

pub(super) struct SessionRuntimeParts {
    pub(super) kernel: KernelConfig,
    pub(super) bindings: SessionBindings,
}

impl SessionRuntimeOptions {
    /// Creates options for one session owner.
    ///
    /// The configured `task_runtime` must be timer-enabled, alive, and actively driven.
    /// This applies while `SessionRuntime::create`, `SessionRuntime::load`, or owner
    /// shutdown is in progress. A live but undriven current-thread runtime cannot
    /// advance owner, cleanup, or timeout tasks.
    pub fn new(
        kernel: KernelConfig,
        bindings: SessionBindings,
        task_runtime: Handle,
    ) -> Result<Self, SessionOpenError> {
        kernel
            .validate()
            .map_err(|_| SessionOpenError::invalid_configuration())?;
        if !runtime_has_timer(&task_runtime) {
            return Err(SessionOpenError::invalid_configuration());
        }
        Ok(Self {
            kernel,
            bindings,
            task_runtime,
        })
    }

    pub fn kernel(&self) -> &KernelConfig {
        &self.kernel
    }

    pub fn bindings(&self) -> &SessionBindings {
        &self.bindings
    }

    pub fn task_runtime(&self) -> &Handle {
        &self.task_runtime
    }

    pub(super) fn into_parts(self) -> SessionRuntimeParts {
        SessionRuntimeParts {
            kernel: self.kernel,
            bindings: self.bindings,
        }
    }
}

impl fmt::Debug for SessionRuntimeOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionRuntimeOptions")
            .field("command_capacity", &self.kernel.command_capacity)
            .field("runner_capacity", &self.kernel.runner_capacity)
            .field("event_capacity", &self.kernel.event_capacity)
            .field("shutdown_timeout", &self.kernel.shutdown_timeout)
            .field("log_operation_timeout", &self.kernel.log_operation_timeout)
            .field("tool_count", &self.bindings.tools.frozen_specs().len())
            .field("has_tool_policy", &self.bindings.tool_policy.is_some())
            .field("has_context", &self.bindings.context.is_some())
            .field("has_compaction", &self.bindings.compaction.is_some())
            .finish()
    }
}

/// The unique owner of one loaded Session and its actor resources.
///
/// Dropping `SessionRuntime` only triggers best-effort cancellation.
/// Call `shutdown(self).await` for complete cleanup and the durability barrier.
#[must_use = "SessionRuntime owns a loaded session and should be retained until explicit shutdown"]
pub struct SessionRuntime {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    handle: SessionHandle,
    events: Option<SessionEventStream>,
    owner_cancel: CancellationToken,
    task: Option<JoinHandle<SessionActorExit>>,
    runner_lifecycle: RunnerLifecycle,
    task_runtime: Handle,
    shutdown_timeout: Duration,
}

impl SessionRuntime {
    pub async fn create(
        session_id: SessionId,
        spec: SessionSpec,
        log: Box<dyn SessionLog>,
        options: SessionRuntimeOptions,
    ) -> Result<Self, SessionOpenError> {
        Self::open(OpenRequest::Create { session_id, spec }, log, options).await
    }

    pub async fn load(
        expected_session_id: SessionId,
        log: Box<dyn SessionLog>,
        options: SessionRuntimeOptions,
    ) -> Result<Self, SessionOpenError> {
        Self::open(
            OpenRequest::Load {
                expected_session_id,
            },
            log,
            options,
        )
        .await
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn instance_id(&self) -> SessionInstanceId {
        self.instance_id
    }

    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }

    pub fn take_events(&mut self) -> Result<SessionEventStream, EventStreamTakenError> {
        self.events
            .take()
            .ok_or(EventStreamTakenError::AlreadyTaken)
    }

    /// Cancels and joins this session owner using the configured runtime's
    /// timer driver for the shutdown deadline.
    pub async fn shutdown(mut self) -> Result<(), SessionShutdownError> {
        self.owner_cancel.cancel();
        let Some(mut task) = self.task.take() else {
            return Err(SessionShutdownError::actor_terminated());
        };
        let timeout = match construct_shutdown_timeout(
            &self.task_runtime,
            self.shutdown_timeout,
            &mut task,
        ) {
            Ok(timeout) => timeout,
            Err(()) => {
                task.abort();
                let _ = task.await;
                self.runner_lifecycle.abort_and_wait().await;
                return Err(SessionShutdownError::actor_terminated());
            }
        };
        match timeout.await {
            Ok(Ok(exit)) => map_actor_exit(exit),
            Ok(Err(_)) => {
                self.runner_lifecycle.abort_and_wait().await;
                Err(SessionShutdownError::actor_terminated())
            }
            Err(_) => {
                task.abort();
                let _ = task.await;
                self.runner_lifecycle.abort_and_wait().await;
                Err(SessionShutdownError::timeout())
            }
        }
    }

    async fn open(
        request: OpenRequest,
        log: Box<dyn SessionLog>,
        options: SessionRuntimeOptions,
    ) -> Result<Self, SessionOpenError> {
        let shutdown_timeout = options.kernel.shutdown_timeout;
        let task_runtime = options.task_runtime.clone();
        let owner_cancel = CancellationToken::new();
        let payload_claimed = CancellationToken::new();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let payload = OpenPayload::shared(request, log, options);
        let mut guard = OpenGuard::new(
            owner_cancel.clone(),
            &task_runtime,
            &payload,
            &payload_claimed,
        );
        let task_payload = std::sync::Arc::clone(&payload);
        let task_cancel = owner_cancel.clone();
        let task_claimed = payload_claimed.clone();
        let task = match spawn_owner(
            &task_runtime,
            task_payload,
            task_cancel,
            task_claimed,
            ready_sender,
        ) {
            Ok(task) => task,
            Err(_) => {
                guard.cancel();
                return Err(guard
                    .await_watchers()
                    .await
                    .unwrap_or_else(SessionOpenError::actor_start_failed));
            }
        };
        guard.set_owner_task(task);
        match ready_receiver.await {
            Ok(Ok(OpenReady {
                session_id,
                instance_id,
                handle,
                events,
                runner_lifecycle,
            })) => {
                if guard.await_watchers().await.is_some() {
                    return Err(SessionOpenError::actor_start_failed());
                }
                let Some(task) = guard.disarm() else {
                    return Err(SessionOpenError::actor_start_failed());
                };
                Ok(Self {
                    session_id,
                    instance_id,
                    handle,
                    events: Some(events),
                    owner_cancel,
                    task: Some(task),
                    runner_lifecycle,
                    task_runtime,
                    shutdown_timeout,
                })
            }
            Ok(Err(error)) => {
                guard.join_owner().await;
                let _ = guard.await_watchers().await;
                Err(error)
            }
            Err(_) => {
                guard.cancel();
                guard.join_owner().await;
                match guard.await_watchers().await {
                    Some(error) => Err(error),
                    None => Err(SessionOpenError::actor_start_failed()),
                }
            }
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.owner_cancel.cancel();
    }
}

struct OpenGuard {
    owner_cancel: CancellationToken,
    owner_task: Option<JoinHandle<SessionActorExit>>,
    cleanup_watchers: Vec<JoinHandle<Option<SessionOpenError>>>,
    armed: bool,
}

impl OpenGuard {
    fn new(
        owner_cancel: CancellationToken,
        task_runtime: &Handle,
        payload: &SharedOpenPayload,
        payload_claimed: &CancellationToken,
    ) -> Self {
        let mut guard = Self {
            owner_cancel,
            owner_task: None,
            cleanup_watchers: Vec::new(),
            armed: true,
        };
        guard.spawn_watcher(task_runtime, payload, payload_claimed);
        if let Ok(current) = Handle::try_current() {
            guard.spawn_watcher(&current, payload, payload_claimed);
        }
        guard
    }

    fn set_owner_task(&mut self, task: JoinHandle<SessionActorExit>) {
        self.owner_task = Some(task);
    }

    fn disarm(&mut self) -> Option<JoinHandle<SessionActorExit>> {
        let task = self.owner_task.take();
        if task.is_some() {
            self.armed = false;
        }
        task
    }

    async fn join_owner(&mut self) {
        if let Some(task) = self.owner_task.as_mut() {
            let _ = task.await;
        }
        self.armed = false;
    }

    fn cancel(&self) {
        self.owner_cancel.cancel();
    }

    fn spawn_watcher(
        &mut self,
        runtime: &Handle,
        payload: &SharedOpenPayload,
        payload_claimed: &CancellationToken,
    ) {
        let payload = std::sync::Arc::clone(payload);
        let owner_cancel = self.owner_cancel.clone();
        let payload_claimed = payload_claimed.clone();
        let task = catch_unwind(AssertUnwindSafe(|| {
            runtime.spawn(watch_unclaimed_payload(
                payload,
                owner_cancel,
                payload_claimed,
            ))
        }));
        if let Ok(task) = task {
            self.cleanup_watchers.push(task);
        }
    }

    async fn await_watchers(&mut self) -> Option<SessionOpenError> {
        let result = {
            let mut pending = FuturesUnordered::new();
            for task in &mut self.cleanup_watchers {
                pending.push(task);
            }
            let mut result = None;
            while let Some(joined) = pending.next().await {
                if let Ok(Some(error)) = joined {
                    result = Some(error);
                    break;
                }
            }
            result
        };
        self.cleanup_watchers.clear();
        result
    }
}

async fn watch_unclaimed_payload(
    payload: SharedOpenPayload,
    owner_cancel: CancellationToken,
    payload_claimed: CancellationToken,
) -> Option<SessionOpenError> {
    if !current_runtime_has_timer() {
        return None;
    }
    tokio::select! {
        biased;
        _ = payload_claimed.cancelled() => None,
        _ = owner_cancel.cancelled() => cleanup_shared_payload(payload).await,
    }
}

async fn cleanup_shared_payload(payload: SharedOpenPayload) -> Option<SessionOpenError> {
    #[cfg(test)]
    if tests::take_scripted_payload_panic(&payload, tests::PayloadPanicPoint::CleanupBeforeTake) {
        panic!("scripted cleanup task panic before payload take");
    }
    match take_shared_payload(&payload) {
        Some(payload) => Some(payload.close_unstarted().await),
        None => None,
    }
}

fn take_shared_payload(payload: &SharedOpenPayload) -> Option<OpenPayload> {
    payload
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if self.armed {
            self.owner_cancel.cancel();
        }
    }
}

fn spawn_owner(
    runtime: &Handle,
    payload: SharedOpenPayload,
    owner_cancel: CancellationToken,
    payload_claimed: CancellationToken,
    ready: oneshot::Sender<Result<OpenReady, SessionOpenError>>,
) -> Result<JoinHandle<SessionActorExit>, ()> {
    catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        if tests::take_scripted_payload_panic(&payload, tests::PayloadPanicPoint::OwnerSpawn) {
            panic!("scripted owner spawn panic");
        }
        runtime.spawn(run_open(payload, owner_cancel, payload_claimed, ready))
    }))
    .map_err(|_| ())
}

fn runtime_has_timer(runtime: &Handle) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let _entered = runtime.enter();
        drop(tokio::time::sleep(Duration::ZERO));
    }))
    .is_ok()
}

fn current_runtime_has_timer() -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        drop(tokio::time::sleep(Duration::ZERO));
    }))
    .is_ok()
}

#[cfg(test)]
mod tests;
