use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

use crate::ids::{InteractionId, SessionId, TurnId};
use crate::workspace::Workspace;

use super::types::{ToolError, UserAnswer, UserQuestion};

const MAX_QUESTION_BYTES: usize = 8_192;
const MAX_CHOICE_BYTES: usize = 1_024;
const MAX_CHOICES: usize = 32;

fn valid_question(question: &str, choices: Option<&[String]>) -> Result<(), ToolError> {
    if question.is_empty()
        || question.len() > MAX_QUESTION_BYTES
        || question.chars().any(char::is_control)
    {
        return Err(ToolError::InvalidInteraction);
    }
    if let Some(choices) = choices {
        if choices.is_empty() || choices.len() > MAX_CHOICES {
            return Err(ToolError::InvalidInteraction);
        }
        if choices.iter().any(|choice| {
            choice.is_empty()
                || choice.len() > MAX_CHOICE_BYTES
                || choice.chars().any(char::is_control)
        }) {
            return Err(ToolError::InvalidInteraction);
        }
    }
    Ok(())
}

struct InteractionChannelInner {
    state: Mutex<InteractionChannelState>,
    notify: Notify,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InteractionSlotId(u64);

struct InteractionChannelState {
    receiver_open: bool,
    client_count: usize,
    queued: Option<Arc<InteractionRequestInner>>,
    pending: Option<Arc<RequestState>>,
    next_slot_id: u64,
}

impl InteractionChannelState {
    fn allocate_slot_id(&mut self) -> Result<InteractionSlotId, ToolError> {
        let value = self
            .next_slot_id
            .checked_add(1)
            .ok_or(ToolError::Internal)?;
        self.next_slot_id = value;
        Ok(InteractionSlotId(value))
    }
}

impl InteractionChannelInner {
    fn lock_state(&self) -> MutexGuard<'_, InteractionChannelState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct RequestState {
    channel: Weak<InteractionChannelInner>,
    slot_id: InteractionSlotId,
    active: AtomicBool,
    reply: Mutex<Option<oneshot::Sender<Result<UserAnswer, ToolError>>>>,
}

pub(crate) struct InteractionResponse {
    reply: Option<oneshot::Sender<Result<UserAnswer, ToolError>>>,
}

impl InteractionResponse {
    pub(crate) fn respond(mut self, answer: UserAnswer) -> Result<(), ToolError> {
        let Some(reply) = self.reply.take() else {
            return Err(ToolError::InteractionClosed);
        };
        reply
            .send(Ok(answer))
            .map_err(|_| ToolError::InteractionClosed)
    }

    pub(crate) fn reject(mut self, error: ToolError) -> Result<(), ToolError> {
        let Some(reply) = self.reply.take() else {
            return Err(ToolError::InteractionClosed);
        };
        reply
            .send(Err(error))
            .map_err(|_| ToolError::InteractionClosed)
    }
}

impl Drop for InteractionResponse {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(ToolError::InteractionClosed));
        }
    }
}

impl RequestState {
    fn new(
        channel: &Arc<InteractionChannelInner>,
        slot_id: InteractionSlotId,
        reply: oneshot::Sender<Result<UserAnswer, ToolError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            channel: Arc::downgrade(channel),
            slot_id,
            active: AtomicBool::new(true),
            reply: Mutex::new(Some(reply)),
        })
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn take_reply(&self) -> Option<oneshot::Sender<Result<UserAnswer, ToolError>>> {
        self.reply
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn remove_from_channel(self: &Arc<Self>) {
        let Some(channel) = self.channel.upgrade() else {
            return;
        };
        let changed = {
            let mut state = channel.lock_state();
            let mut changed = false;
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.slot_id == self.slot_id)
            {
                state.pending = None;
                changed = true;
            }
            if state
                .queued
                .as_ref()
                .is_some_and(|queued| queued.state.slot_id == self.slot_id)
            {
                state.queued = None;
                changed = true;
            }
            changed
        };
        if changed {
            channel.notify.notify_waiters();
        }
    }

    fn release(self: &Arc<Self>) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }
        self.remove_from_channel();
        if let Some(reply) = self.take_reply() {
            let _ = reply.send(Err(ToolError::InteractionClosed));
        }
    }

    fn claim(self: &Arc<Self>) -> Result<InteractionResponse, ToolError> {
        if !self.active.swap(false, Ordering::AcqRel) {
            return Err(ToolError::InteractionClosed);
        }
        self.remove_from_channel();
        let Some(reply) = self.take_reply() else {
            return Err(ToolError::InteractionClosed);
        };
        Ok(InteractionResponse { reply: Some(reply) })
    }

    fn finish(self: &Arc<Self>, result: Result<UserAnswer, ToolError>) -> Result<(), ToolError> {
        let response = self.claim()?;
        match result {
            Ok(answer) => response.respond(answer),
            Err(error) => response.reject(error),
        }
    }
}

struct InteractionRequestInner {
    state: Arc<RequestState>,
    turn_id: TurnId,
    question: String,
    choices: Option<Vec<String>>,
}

pub struct InteractionClient {
    inner: Arc<InteractionChannelInner>,
}

pub struct InteractionReceiver {
    inner: Arc<InteractionChannelInner>,
}

struct InteractionWaitGuard {
    state: Arc<RequestState>,
}

impl Drop for InteractionWaitGuard {
    fn drop(&mut self) {
        self.state.release();
    }
}

fn release_slots(queued: Option<Arc<InteractionRequestInner>>, pending: Option<Arc<RequestState>>) {
    if let Some(queued) = queued {
        queued.state.release();
    }
    if let Some(pending) = pending {
        pending.release();
    }
}

impl InteractionClient {
    pub fn channel() -> (Self, InteractionReceiver) {
        let inner = Arc::new(InteractionChannelInner {
            state: Mutex::new(InteractionChannelState {
                receiver_open: true,
                client_count: 1,
                queued: None,
                pending: None,
                next_slot_id: 0,
            }),
            notify: Notify::new(),
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            InteractionReceiver { inner },
        )
    }

    pub async fn ask_user(
        &self,
        turn_id: TurnId,
        question: impl Into<String>,
        choices: Option<Vec<String>>,
        cancellation: CancellationToken,
    ) -> Result<UserAnswer, ToolError> {
        let question = question.into();
        valid_question(&question, choices.as_deref())?;
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let (request_state, response) = {
            let mut channel_state = self.inner.lock_state();
            if cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if !channel_state.receiver_open || channel_state.client_count == 0 {
                return Err(ToolError::InteractionClosed);
            }
            if channel_state.pending.is_some() || channel_state.queued.is_some() {
                return Err(ToolError::InteractionBusy);
            }

            let (reply, response) = oneshot::channel();
            let slot_id = channel_state.allocate_slot_id()?;
            let request_state = RequestState::new(&self.inner, slot_id, reply);
            let request = Arc::new(InteractionRequestInner {
                state: Arc::clone(&request_state),
                turn_id,
                question,
                choices,
            });
            channel_state.pending = Some(Arc::clone(&request_state));
            channel_state.queued = Some(request);
            (request_state, response)
        };
        self.inner.notify.notify_one();

        let mut response = Box::pin(response);
        let _wait_guard = InteractionWaitGuard {
            state: Arc::clone(&request_state),
        };
        let result = tokio::select! {
            result = &mut response => result,
            _ = cancellation.cancelled() => {
                let _ = request_state.finish(Err(ToolError::Cancelled));
                response.await
            }
        };
        result.unwrap_or(Err(ToolError::InteractionClosed))
    }
}

impl Clone for InteractionClient {
    fn clone(&self) -> Self {
        let mut state = self.inner.lock_state();
        state.client_count = state.client_count.saturating_add(1);
        drop(state);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for InteractionClient {
    fn drop(&mut self) {
        let (slots, last_client) = {
            let mut state = self.inner.lock_state();
            if state.client_count > 0 {
                state.client_count -= 1;
            }
            if state.client_count == 0 {
                ((state.queued.take(), state.pending.take()), true)
            } else {
                ((None, None), false)
            }
        };
        release_slots(slots.0, slots.1);
        if last_client {
            self.inner.notify.notify_waiters();
        }
    }
}

impl InteractionReceiver {
    pub async fn recv(&mut self) -> Option<InteractionRequest> {
        loop {
            let notified = self.inner.notify.notified();
            let next = {
                let mut state = self.inner.lock_state();
                if !state.receiver_open || state.client_count == 0 {
                    return None;
                }
                match state.queued.take() {
                    None => None,
                    Some(request) => {
                        if request.state.is_active()
                            && state
                                .pending
                                .as_ref()
                                .is_some_and(|pending| pending.slot_id == request.state.slot_id)
                        {
                            return Some(InteractionRequest { inner: request });
                        }
                        Some(request)
                    }
                }
            };
            if let Some(stale) = next {
                stale.state.release();
                continue;
            }
            notified.await;
        }
    }
}

impl Drop for InteractionReceiver {
    fn drop(&mut self) {
        let slots = {
            let mut state = self.inner.lock_state();
            if !state.receiver_open {
                return;
            }
            state.receiver_open = false;
            (state.queued.take(), state.pending.take())
        };
        release_slots(slots.0, slots.1);
        self.inner.notify.notify_waiters();
    }
}

pub struct InteractionRequest {
    inner: Arc<InteractionRequestInner>,
}

impl InteractionRequest {
    pub fn turn_id(&self) -> TurnId {
        self.inner.turn_id
    }

    pub fn question(&self) -> &str {
        &self.inner.question
    }

    pub fn choices(&self) -> Option<&[String]> {
        self.inner.choices.as_deref()
    }

    pub fn user_question(&self, interaction_id: InteractionId) -> Result<UserQuestion, ToolError> {
        if !self.inner.state.is_active() {
            return Err(ToolError::InteractionClosed);
        }
        UserQuestion::new(
            interaction_id,
            self.inner.question.clone(),
            self.inner.choices.clone(),
        )
        .map_err(|_| ToolError::InvalidInteraction)
    }

    pub(crate) fn claim_response(self) -> Result<InteractionResponse, ToolError> {
        self.inner.state.claim()
    }

    pub fn respond(self, answer: UserAnswer) -> Result<(), ToolError> {
        self.claim_response()?.respond(answer)
    }

    pub fn reject(self, error: ToolError) -> Result<(), ToolError> {
        self.claim_response()?.reject(error)
    }
}

impl Drop for InteractionRequest {
    fn drop(&mut self) {
        self.inner.state.release();
    }
}

pub struct ToolContext<'a> {
    session_id: SessionId,
    turn_id: TurnId,
    workspace: &'a Workspace,
    cancellation: CancellationToken,
    interactions: &'a InteractionClient,
}

impl<'a> ToolContext<'a> {
    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        workspace: &'a Workspace,
        cancellation: CancellationToken,
        interactions: &'a InteractionClient,
    ) -> Result<Self, ToolError> {
        Ok(Self {
            session_id,
            turn_id,
            workspace,
            cancellation,
            interactions,
        })
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn workspace(&self) -> &'a Workspace {
        self.workspace
    }

    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub const fn interactions(&self) -> &'a InteractionClient {
        self.interactions
    }

    pub async fn ask_user(
        &self,
        question: impl Into<String>,
        choices: Option<Vec<String>>,
    ) -> Result<UserAnswer, ToolError> {
        self.interactions
            .ask_user(self.turn_id, question, choices, self.cancellation.clone())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InteractionChannelState, InteractionClient, InteractionId, ToolError, TurnId, UserAnswer,
    };
    use tokio_util::sync::CancellationToken;

    #[tokio::test(flavor = "current_thread")]
    async fn claimed_response_owns_sender_and_responds_without_request_finish() {
        let (client, mut receiver) = InteractionClient::channel();
        let task_client = client.clone();
        let task = tokio::runtime::Handle::current().spawn(async move {
            task_client
                .ask_user(
                    TurnId::new().unwrap(),
                    "question",
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        let request = receiver.recv().await.unwrap();
        let response = request.claim_response().unwrap();
        response
            .respond(UserAnswer::new("answer").unwrap())
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap().text(), "answer");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn claimed_response_wins_after_runner_cancellation_attempt() {
        let (client, mut receiver) = InteractionClient::channel();
        let cancellation = CancellationToken::new();
        let task_client = client.clone();
        let task_cancellation = cancellation.clone();
        let task = tokio::runtime::Handle::current().spawn(async move {
            task_client
                .ask_user(TurnId::new().unwrap(), "question", None, task_cancellation)
                .await
        });
        let request = receiver.recv().await.unwrap();
        let response = request.claim_response().unwrap();
        cancellation.cancel();
        response
            .respond(UserAnswer::new("accepted").unwrap())
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap().text(), "accepted");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dequeued_cancelled_request_preserves_interaction_closed_error() {
        let (client, mut receiver) = InteractionClient::channel();
        let cancellation = CancellationToken::new();
        let task_client = client.clone();
        let task_cancellation = cancellation.clone();
        let task = tokio::runtime::Handle::current().spawn(async move {
            task_client
                .ask_user(TurnId::new().unwrap(), "question", None, task_cancellation)
                .await
        });
        let request = receiver.recv().await.unwrap();
        cancellation.cancel();
        assert_eq!(task.await.unwrap(), Err(ToolError::Cancelled));
        assert_eq!(
            request.user_question(InteractionId::new().unwrap()),
            Err(ToolError::InteractionClosed)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_claimed_response_settles_waiter_as_closed() {
        let (client, mut receiver) = InteractionClient::channel();
        let task_client = client.clone();
        let task = tokio::runtime::Handle::current().spawn(async move {
            task_client
                .ask_user(
                    TurnId::new().unwrap(),
                    "question",
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        let request = receiver.recv().await.unwrap();
        let response = request.claim_response().unwrap();
        drop(response);
        assert_eq!(task.await.unwrap(), Err(ToolError::InteractionClosed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_request_drop_does_not_clear_a_new_slot() {
        let (client, mut receiver) = InteractionClient::channel();
        let first_client = client.clone();
        let first = tokio::runtime::Handle::current().spawn(async move {
            first_client
                .ask_user(
                    TurnId::new().unwrap(),
                    "old request",
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        let old_request = receiver.recv().await.unwrap();
        {
            let mut state = client.inner.lock_state();
            assert!(state.pending.take().is_some());
        }

        let second_client = client.clone();
        let second = tokio::runtime::Handle::current().spawn(async move {
            second_client
                .ask_user(
                    TurnId::new().unwrap(),
                    "new request",
                    None,
                    CancellationToken::new(),
                )
                .await
        });
        let new_request = receiver.recv().await.unwrap();
        let new_slot_id = new_request.inner.state.slot_id;
        drop(old_request);

        assert_eq!(first.await.unwrap(), Err(ToolError::InteractionClosed));
        assert_eq!(
            client
                .inner
                .lock_state()
                .pending
                .as_ref()
                .map(|pending| pending.slot_id),
            Some(new_slot_id)
        );
        new_request
            .respond(UserAnswer::new("accepted").unwrap())
            .unwrap();
        assert_eq!(second.await.unwrap().unwrap().text(), "accepted");
    }

    #[test]
    fn interaction_slot_ids_are_monotonic_and_checked() {
        let mut state = InteractionChannelState {
            receiver_open: true,
            client_count: 1,
            queued: None,
            pending: None,
            next_slot_id: 0,
        };
        assert_eq!(
            state.allocate_slot_id().unwrap(),
            super::InteractionSlotId(1)
        );
        assert_eq!(
            state.allocate_slot_id().unwrap(),
            super::InteractionSlotId(2)
        );
        state.next_slot_id = u64::MAX;
        assert_eq!(state.allocate_slot_id(), Err(ToolError::Internal));
        assert_eq!(state.next_slot_id, u64::MAX);
    }
}
