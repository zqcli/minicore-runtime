use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::SessionError;
use crate::ids::SessionId;
use crate::session::command::SessionHandle;
use crate::session::conversation::ConversationLog;

pub(crate) struct ManagedSession {
    pub(crate) handle: SessionHandle,
    pub(crate) conversation: Arc<ConversationLog>,
    join: Arc<JoinOnce<Result<(), SessionError>>>,
}

impl ManagedSession {
    pub(crate) fn new(
        handle: SessionHandle,
        conversation: Arc<ConversationLog>,
        actor: JoinHandle<Result<(), SessionError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            handle,
            conversation,
            join: JoinOnce::new(actor, || Err(SessionError::Internal)),
        })
    }

    pub(crate) async fn close(&self) -> Result<(), SessionError> {
        self.handle.request_close();
        let handle_result = self.handle.close().await;
        let join_result = self.join.join().await;
        first_error(handle_result, join_result)
    }

    pub(crate) fn request_close(&self) {
        self.handle.request_close();
    }
}

fn first_error(
    first: Result<(), SessionError>,
    second: Result<(), SessionError>,
) -> Result<(), SessionError> {
    first.and(second)
}

struct JoinState<R> {
    handle: Option<JoinHandle<R>>,
    started: bool,
    joining: bool,
    result: Option<R>,
    join_error: fn() -> R,
}

pub(crate) struct JoinOnce<R> {
    state: Mutex<JoinState<R>>,
    notify: Arc<Notify>,
}

impl<R> JoinOnce<R>
where
    R: Clone + Send + 'static,
{
    pub(crate) fn new(handle: JoinHandle<R>, join_error: fn() -> R) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(JoinState {
                handle: Some(handle),
                started: true,
                joining: false,
                result: None,
                join_error,
            }),
            notify: Arc::new(Notify::new()),
        })
    }

    pub(crate) fn pending(join_error: fn() -> R) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(JoinState {
                handle: None,
                started: false,
                joining: false,
                result: None,
                join_error,
            }),
            notify: Arc::new(Notify::new()),
        })
    }

    pub(crate) fn start<F>(&self, runtime: &Handle, future: F)
    where
        F: Future<Output = R> + Send + 'static,
    {
        let mut state = lock(&self.state);
        if state.started {
            return;
        }
        state.handle = Some(runtime.spawn(future));
        state.started = true;
        drop(state);
        self.notify.notify_waiters();
    }

    pub(crate) fn join(self: &Arc<Self>) -> JoinFuture<R> {
        JoinFuture {
            owner: Arc::clone(self),
            waiter: None,
            joining: false,
        }
    }

    pub(crate) fn needs_retention(&self) -> bool {
        lock(&self.state).handle.is_some()
    }
}

pub(crate) struct JoinFuture<R> {
    owner: Arc<JoinOnce<R>>,
    waiter: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    joining: bool,
}

impl<R> Future for JoinFuture<R>
where
    R: Clone + Send + 'static,
{
    type Output = R;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            let mut state = lock(&this.owner.state);
            if let Some(result) = state.result.clone() {
                this.joining = false;
                return Poll::Ready(result);
            }
            if !state.started {
                let notify = Arc::clone(&this.owner.notify);
                let waiter = this
                    .waiter
                    .get_or_insert_with(|| Box::pin(async move { notify.notified().await }));
                drop(state);
                match waiter.as_mut().poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(()) => {
                        this.waiter = None;
                    }
                }
                continue;
            }
            if !state.joining || this.joining {
                if !this.joining {
                    state.joining = true;
                    this.joining = true;
                }
                let Some(handle) = state.handle.as_mut() else {
                    let result = (state.join_error)();
                    state.result = Some(result.clone());
                    state.joining = false;
                    this.joining = false;
                    drop(state);
                    this.owner.notify.notify_waiters();
                    return Poll::Ready(result);
                };
                match Pin::new(handle).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => {
                        let result = result.unwrap_or_else(|_| (state.join_error)());
                        state.result = Some(result.clone());
                        state.joining = false;
                        state.handle.take();
                        this.joining = false;
                        drop(state);
                        this.owner.notify.notify_waiters();
                        return Poll::Ready(result);
                    }
                }
            }
            let notify = Arc::clone(&this.owner.notify);
            let waiter = this
                .waiter
                .get_or_insert_with(|| Box::pin(async move { notify.notified().await }));
            drop(state);
            match waiter.as_mut().poll(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => {
                    this.waiter = None;
                }
            }
        }
    }
}

impl<R> Drop for JoinFuture<R> {
    fn drop(&mut self) {
        if self.joining {
            let mut state = lock(&self.owner.state);
            if state.result.is_none() {
                state.joining = false;
            }
            drop(state);
            self.owner.notify.notify_waiters();
        }
    }
}

pub(crate) struct SessionManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    state: Mutex<ManagerState>,
    loading_notify: Notify,
}

struct ManagerState {
    loaded: BTreeMap<SessionId, Arc<ManagedSession>>,
    loading: BTreeSet<SessionId>,
    closing: bool,
}

impl Clone for SessionManager {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl SessionManager {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                state: Mutex::new(ManagerState {
                    loaded: BTreeMap::new(),
                    loading: BTreeSet::new(),
                    closing: false,
                }),
                loading_notify: Notify::new(),
            }),
        }
    }

    pub(crate) fn begin_load(&self, id: SessionId) -> Result<LoadReservation, SessionError> {
        let mut state = lock(&self.inner.state);
        if state.closing {
            return Err(SessionError::Closing);
        }
        if state.loaded.contains_key(&id) {
            return Err(SessionError::AlreadyLoaded);
        }
        if state.loading.contains(&id) {
            return Err(SessionError::Busy);
        }
        state.loading.insert(id);
        Ok(LoadReservation {
            manager: self.clone(),
            id,
            active: true,
        })
    }

    pub(crate) fn begin_shutdown(&self) -> Vec<(SessionId, Arc<ManagedSession>)> {
        let mut state = lock(&self.inner.state);
        state.closing = true;
        state
            .loaded
            .iter()
            .map(|(id, session)| (*id, Arc::clone(session)))
            .collect()
    }

    pub(crate) fn finish_load(
        &self,
        reservation: &mut LoadReservation,
        session: Arc<ManagedSession>,
    ) -> bool {
        let (closing, signal) = {
            let mut state = lock(&self.inner.state);
            state.loading.remove(&reservation.id);
            let closing = state.closing;
            let signal = closing.then(|| Arc::clone(&session));
            state.loaded.insert(reservation.id, session);
            reservation.active = false;
            (closing, signal)
        };
        self.inner.loading_notify.notify_waiters();
        if let Some(session) = signal {
            session.request_close();
        }
        closing
    }

    pub(crate) fn get(&self, id: SessionId) -> Option<Arc<ManagedSession>> {
        lock(&self.inner.state).loaded.get(&id).cloned()
    }

    pub(crate) fn remove_exact(&self, id: SessionId, expected: &Arc<ManagedSession>) -> bool {
        let mut state = lock(&self.inner.state);
        if state
            .loaded
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            state.loaded.remove(&id);
            true
        } else {
            false
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<(SessionId, Arc<ManagedSession>)> {
        lock(&self.inner.state)
            .loaded
            .iter()
            .map(|(id, session)| (*id, Arc::clone(session)))
            .collect()
    }

    pub(crate) fn is_closing(&self) -> bool {
        lock(&self.inner.state).closing
    }

    pub(crate) async fn wait_loading(&self) {
        loop {
            let notified = self.inner.loading_notify.notified();
            if lock(&self.inner.state).loading.is_empty() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn has_loaded(&self) -> bool {
        !lock(&self.inner.state).loaded.is_empty()
    }
}

pub(crate) struct LoadReservation {
    manager: SessionManager,
    id: SessionId,
    active: bool,
}

impl Drop for LoadReservation {
    fn drop(&mut self) {
        if self.active {
            lock(&self.manager.inner.state).loading.remove(&self.id);
            self.manager.inner.loading_notify.notify_waiters();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const _: () = {
    let _ = SessionManager::new;
    let _ = ManagedSession::new;
    let _ = ManagedSession::close;
    let _ = JoinOnce::<Result<(), SessionError>>::pending;
};
