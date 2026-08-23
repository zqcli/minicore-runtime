use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;
use tokio::task::AbortHandle;

#[derive(Clone)]
pub(in crate::session) struct RunnerLifecycle {
    inner: Arc<RunnerLifecycleInner>,
}

struct RunnerLifecycleInner {
    state: Mutex<RunnerLifecycleState>,
    finished: Notify,
}

struct RunnerLifecycleState {
    generation: u64,
    active: bool,
    abort: Option<AbortHandle>,
}

pub(super) struct RunnerGuard {
    lifecycle: RunnerLifecycle,
    generation: u64,
}

impl RunnerLifecycle {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(RunnerLifecycleInner {
                state: Mutex::new(RunnerLifecycleState {
                    generation: 0,
                    active: false,
                    abort: None,
                }),
                finished: Notify::new(),
            }),
        }
    }

    pub(super) fn start(&self) -> RunnerGuard {
        let mut state = lock(&self.inner.state);
        state.generation = state.generation.wrapping_add(1);
        state.active = true;
        state.abort = None;
        RunnerGuard {
            lifecycle: self.clone(),
            generation: state.generation,
        }
    }

    pub(super) fn install_abort(&self, generation: u64, abort: AbortHandle) {
        let mut state = lock(&self.inner.state);
        if state.active && state.generation == generation {
            state.abort = Some(abort);
        }
    }

    pub(in crate::session) async fn abort_and_wait(&self) {
        let abort = {
            let state = lock(&self.inner.state);
            if !state.active {
                return;
            }
            state.abort.clone()
        };
        if let Some(abort) = abort {
            abort.abort();
        }
        loop {
            let notified = self.inner.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !lock(&self.inner.state).active {
                return;
            }
            notified.await;
        }
    }

    fn finish(&self, generation: u64) {
        let mut state = lock(&self.inner.state);
        if state.active && state.generation == generation {
            state.active = false;
            state.abort = None;
            drop(state);
            self.inner.finished.notify_waiters();
        }
    }
}

impl RunnerGuard {
    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        self.lifecycle.finish(self.generation);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
