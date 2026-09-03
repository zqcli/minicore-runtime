use std::sync::Arc;

use tokio::sync::watch;

use crate::execution::UserInput;
use crate::execution::{ConfigRevision, ExecutionConfig};
use crate::ids::{InteractionId, LoopId};
use crate::interaction::InteractionAnswer;

use super::control::LoopControl;
use super::{AnswerError, LoopReport, LoopState, LoopWaitError, SteerError, UpdateError};

/// Live handle to one agent loop. `LoopHandle` is cheap to clone and holds no
/// history, store, or session metadata: only the shared `LoopControl`.
#[derive(Clone)]
pub struct LoopHandle {
    control: Arc<LoopControl>,
}

impl LoopHandle {
    pub(crate) fn new(control: Arc<LoopControl>) -> Self {
        Self { control }
    }

    pub fn id(&self) -> LoopId {
        self.control.id
    }

    pub fn state(&self) -> LoopState {
        self.control.current_state()
    }

    pub fn watch_state(&self) -> watch::Receiver<LoopState> {
        self.control.subscribe_state()
    }

    pub fn answer(
        &self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), AnswerError> {
        self.control.answer(interaction_id, answer)
    }

    /// Requests cancellation. Returns whether this call actually cancelled the
    /// loop; repeated calls return `false`.
    pub fn cancel(&self) -> bool {
        self.control
            .mark_cancel(crate::agent_loop::CancelReason::User)
    }

    /// Atomically replaces the loop's execution config. The new config takes
    /// effect at the next model-request boundary (the in-flight model and the
    /// tool batch it produced keep the previous snapshot); `Ok` carries the
    /// monotonic revision handed to this update. After the loop completes
    /// this returns `NotActive`; a config that violates the loop limits is
    /// rejected as `InvalidConfig` without consuming a revision.
    pub fn update(&self, config: ExecutionConfig) -> Result<ConfigRevision, UpdateError> {
        self.control.update(config)
    }

    /// Queues a human steer for the next request boundary. The in-flight model
    /// and the tool batch it produced are not disturbed; the steer is applied
    /// (in accept order, alongside any pending config update) to the next
    /// model request's prompt. A successful call means the steer was accepted
    /// into this process-local queue. It is applied at the next request boundary.
    /// If the loop is cancelled, shut down, or the process exits before that
    /// boundary, the queued steer may be discarded and will not appear in
    /// `LoopReport::appended`. `QueueFull` means the bounded queue is full,
    /// `WaitingForInput` that an interaction is pending, and `NotActive` that
    /// the loop already sealed.
    pub fn steer(&self, input: UserInput) -> Result<(), SteerError> {
        self.control.steer(input)
    }

    pub fn is_finished(&self) -> bool {
        self.control.is_finished()
    }

    pub async fn wait(&self) -> Result<Arc<LoopReport>, LoopWaitError> {
        let mut receiver = self.control.subscribe_completion();
        loop {
            {
                let value = receiver.borrow();
                if let Some(report) = value.as_ref() {
                    return Ok(Arc::clone(report));
                }
            }
            if receiver.changed().await.is_err() {
                return Err(LoopWaitError::CompletionClosed);
            }
        }
    }
}

impl std::fmt::Debug for LoopHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoopHandle")
            .field("loop_id", &self.control.id)
            .finish()
    }
}
