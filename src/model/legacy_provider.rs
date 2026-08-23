// P5/P6 deletion target: remove with the legacy batch model runner.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::response::ModelError;
use super::types::{
    LegacyModelDescriptor, LegacyModelEvent, LegacyProviderId, ModelRequest, ModelResponse,
};

const MAX_EVENT_CAPACITY: usize = 1_024;
const MAX_EVENT_DELTA_BYTES: usize = 64 * 1024;

fn valid_event(event: &LegacyModelEvent) -> bool {
    let delta = match event {
        LegacyModelEvent::TextDelta { delta } | LegacyModelEvent::ReasoningDelta { delta } => delta,
    };
    !delta.is_empty()
        && delta.len() <= MAX_EVENT_DELTA_BYTES
        && delta
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

struct LegacyModelEventSinkInner {
    state: Mutex<LegacyModelEventSinkState>,
}

struct LegacyModelEventSinkState {
    active: bool,
    sender: Option<mpsc::Sender<LegacyModelEvent>>,
}

impl LegacyModelEventSinkInner {
    fn lock_state(&self) -> MutexGuard<'_, LegacyModelEventSinkState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone)]
pub(crate) struct LegacyModelEventSink {
    inner: Arc<LegacyModelEventSinkInner>,
}

impl LegacyModelEventSink {
    pub(crate) fn channel(
        capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<LegacyModelEvent>), ModelError> {
        if capacity == 0 || capacity > MAX_EVENT_CAPACITY {
            return Err(ModelError::InvalidRequest);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                inner: Arc::new(LegacyModelEventSinkInner {
                    state: Mutex::new(LegacyModelEventSinkState {
                        active: true,
                        sender: Some(sender),
                    }),
                }),
            },
            receiver,
        ))
    }

    pub(crate) fn publish(&self, event: LegacyModelEvent) -> bool {
        if !valid_event(&event) {
            return false;
        }
        let mut state = self.inner.lock_state();
        if !state.active {
            return false;
        }
        let result = match state.sender.as_ref() {
            Some(sender) => sender.try_send(event),
            None => return false,
        };
        match result {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => false,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                state.active = false;
                state.sender = None;
                false
            }
        }
    }

    pub(crate) fn close(&self) {
        let sender = {
            let mut state = self.inner.lock_state();
            state.active = false;
            state.sender.take()
        };
        drop(sender);
    }
}

#[derive(Clone)]
pub(crate) struct LegacyModelCallContext {
    cancellation: CancellationToken,
    events: LegacyModelEventSink,
}

impl LegacyModelCallContext {
    pub(crate) fn new(
        cancellation: CancellationToken,
        events: LegacyModelEventSink,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            cancellation,
            events,
        })
    }

    pub(crate) const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(crate) const fn event_sink(&self) -> &LegacyModelEventSink {
        &self.events
    }

    pub(crate) fn publish(&self, event: LegacyModelEvent) -> bool {
        self.events.publish(event)
    }

    pub(crate) fn close(&self) {
        self.events.close();
    }
}

pub(crate) type LegacyModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelResponse, ModelError>> + Send + 'a>>;

pub(crate) trait LegacyModelProvider: Send + Sync {
    fn id(&self) -> &LegacyProviderId;

    fn models(&self) -> &[LegacyModelDescriptor];

    fn generate(
        &self,
        request: ModelRequest,
        context: LegacyModelCallContext,
    ) -> LegacyModelFuture<'_>;
}

impl<T: LegacyModelProvider + ?Sized> LegacyModelProvider for Arc<T> {
    fn id(&self) -> &LegacyProviderId {
        (**self).id()
    }

    fn models(&self) -> &[LegacyModelDescriptor] {
        (**self).models()
    }

    fn generate(
        &self,
        request: ModelRequest,
        context: LegacyModelCallContext,
    ) -> LegacyModelFuture<'_> {
        (**self).generate(request, context)
    }
}

const _: () = {
    // P5/P6 deletion target: remove with the legacy batch model runner.
    let _ = std::mem::size_of::<LegacyModelEventSink>();
    let _ = LegacyModelEventSink::channel;
    let _ = LegacyModelEventSink::publish;
    let _ = LegacyModelEventSink::close;
    let _ = LegacyModelCallContext::new;
    let _ = LegacyModelCallContext::cancellation;
    let _ = LegacyModelCallContext::event_sink;
    let _ = LegacyModelCallContext::publish;
    let _ = LegacyModelCallContext::close;
};
