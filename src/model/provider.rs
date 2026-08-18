use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::types::{
    ModelDescriptor, ModelError, ModelEvent, ModelRequest, ModelResponse, ProviderId,
};

const MAX_EVENT_CAPACITY: usize = 1_024;
const MAX_EVENT_DELTA_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ProviderCredentialError {
    #[error("provider credential must be non-empty safe opaque ASCII within 256 bytes")]
    Invalid,
}

#[derive(Clone)]
pub struct ProviderCredential(Box<str>);

impl ProviderCredential {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ProviderCredentialError> {
        value.as_ref().parse()
    }

    pub(crate) fn header(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProviderCredential {
    type Err = ProviderCredentialError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 256
            || value
                .bytes()
                .any(|byte| !(0x21..=0x7e).contains(&byte) || matches!(byte, b'"' | b'\\'))
        {
            return Err(ProviderCredentialError::Invalid);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderCredential(<redacted>)")
    }
}

pub type CredentialSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Option<ProviderCredential>> + Send + 'a>>;

pub trait CredentialSource: Send + Sync {
    fn resolve(&self) -> CredentialSourceFuture<'_>;
}

struct FixedCredentialSource(ProviderCredential);

impl CredentialSource for FixedCredentialSource {
    fn resolve(&self) -> CredentialSourceFuture<'_> {
        let credential = self.0.clone();
        Box::pin(async move { Some(credential) })
    }
}

pub fn fixed_credential_source(
    value: &str,
) -> Result<Arc<dyn CredentialSource>, ProviderCredentialError> {
    Ok(Arc::new(FixedCredentialSource(value.parse()?)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderEndpointPolicy {
    HttpsOnly,
    AllowLoopbackHttp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiReasoningProgress {
    SummaryOnly,
    RawText,
}

fn valid_event(event: &ModelEvent) -> bool {
    let delta = match event {
        ModelEvent::TextDelta { delta } | ModelEvent::ReasoningDelta { delta } => delta,
    };
    !delta.is_empty()
        && delta.len() <= MAX_EVENT_DELTA_BYTES
        && delta
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

struct ModelEventSinkInner {
    state: Mutex<ModelEventSinkState>,
}

struct ModelEventSinkState {
    active: bool,
    sender: Option<mpsc::Sender<ModelEvent>>,
}

impl ModelEventSinkInner {
    fn lock_state(&self) -> MutexGuard<'_, ModelEventSinkState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone)]
pub struct ModelEventSink {
    inner: Arc<ModelEventSinkInner>,
}

impl ModelEventSink {
    pub fn channel(capacity: usize) -> Result<(Self, mpsc::Receiver<ModelEvent>), ModelError> {
        if capacity == 0 || capacity > MAX_EVENT_CAPACITY {
            return Err(ModelError::InvalidRequest);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                inner: Arc::new(ModelEventSinkInner {
                    state: Mutex::new(ModelEventSinkState {
                        active: true,
                        sender: Some(sender),
                    }),
                }),
            },
            receiver,
        ))
    }

    pub fn publish(&self, event: ModelEvent) -> bool {
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

    pub fn try_publish(&self, event: ModelEvent) -> bool {
        self.publish(event)
    }

    pub fn close(&self) {
        let sender = {
            let mut state = self.inner.lock_state();
            state.active = false;
            state.sender.take()
        };
        drop(sender);
    }

    pub fn is_closed(&self) -> bool {
        let state = self.inner.lock_state();
        !state.active || state.sender.is_none()
    }
}

#[derive(Clone)]
pub struct ModelCallContext {
    cancellation: CancellationToken,
    events: ModelEventSink,
}

impl ModelCallContext {
    pub fn new(
        cancellation: CancellationToken,
        events: ModelEventSink,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            cancellation,
            events,
        })
    }

    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub const fn event_sink(&self) -> &ModelEventSink {
        &self.events
    }

    pub fn publish(&self, event: ModelEvent) -> bool {
        self.events.publish(event)
    }

    pub fn close(&self) {
        self.events.close();
    }
}

pub type ModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelResponse, ModelError>> + Send + 'a>>;

/// A provider future owns all provider work. Dropping it must stop owner-visible work;
/// implementations must not detach work beyond this future.
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &ProviderId;

    fn models(&self) -> &[ModelDescriptor];

    fn generate<'a>(&'a self, request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'a>;
}

impl<T: ModelProvider + ?Sized> ModelProvider for Arc<T> {
    fn id(&self) -> &ProviderId {
        (**self).id()
    }

    fn models(&self) -> &[ModelDescriptor] {
        (**self).models()
    }

    fn generate<'a>(&'a self, request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'a> {
        (**self).generate(request, ctx)
    }
}
