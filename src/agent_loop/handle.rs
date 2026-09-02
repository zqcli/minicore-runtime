use std::sync::Arc;

use tokio::sync::watch;

use crate::ids::{InteractionId, LoopId};
use crate::interaction::InteractionAnswer;

use super::control::LoopControl;
use super::{AnswerError, LoopReport, LoopState, LoopWaitError};

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
