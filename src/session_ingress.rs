#![allow(
    dead_code,
    reason = "the queue seam is consumed incrementally by SessionExecutor M9 slices"
)]

//! Process-local Session input lanes.
//!
//! The queues only own admitted intent and command identity.  SessionExecutor remains the sole
//! owner of live state and decides when a queued FollowUp may start a new Turn.

use std::collections::VecDeque;

use crate::prompt::PromptIntent;
use crate::wire::CommandId;

pub(crate) const FOLLOW_UP_QUEUE_CAPACITY: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FollowUpQueueError {
    Full,
    DuplicateCommandId,
}

pub(crate) struct QueuedFollowUp {
    command_id: CommandId,
    intent: PromptIntent,
}

impl QueuedFollowUp {
    pub(crate) fn new(command_id: CommandId, intent: PromptIntent) -> Self {
        Self { command_id, intent }
    }

    pub(crate) const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub(crate) fn into_parts(self) -> (CommandId, PromptIntent) {
        (self.command_id, self.intent)
    }
}

pub(crate) struct FollowUpQueue {
    entries: VecDeque<QueuedFollowUp>,
}

impl Default for FollowUpQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl FollowUpQueue {
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(FOLLOW_UP_QUEUE_CAPACITY),
        }
    }

    pub(crate) fn try_push(
        &mut self,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), FollowUpQueueError> {
        if self
            .entries
            .iter()
            .any(|entry| entry.command_id() == command_id)
        {
            return Err(FollowUpQueueError::DuplicateCommandId);
        }
        if self.entries.len() == FOLLOW_UP_QUEUE_CAPACITY {
            return Err(FollowUpQueueError::Full);
        }
        self.entries
            .push_back(QueuedFollowUp::new(command_id, intent));
        Ok(())
    }

    pub(crate) fn pop_front(&mut self) -> Option<QueuedFollowUp> {
        self.entries.pop_front()
    }

    pub(crate) fn remove(&mut self, command_id: CommandId) -> Option<QueuedFollowUp> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.command_id() == command_id)?;
        self.entries.remove(index)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::prompt::{PromptBodyIntent, PromptIntent, TextIntent};
    use crate::wire::CommandId;

    use super::{FOLLOW_UP_QUEUE_CAPACITY, FollowUpQueue, FollowUpQueueError};

    fn intent(text: &str) -> PromptIntent {
        PromptIntent::new(
            PromptBodyIntent::Text(TextIntent::new(text).unwrap()),
            Vec::new(),
        )
        .unwrap()
    }

    fn command_id(value: u8) -> CommandId {
        format!("cmd_{value:032x}").parse().unwrap()
    }

    #[test]
    fn follow_up_queue_is_bounded_fifo_and_removable_by_command_id() {
        let mut queue = FollowUpQueue::new();
        let first = command_id(1);
        let second = command_id(2);
        queue.try_push(first, intent("first")).unwrap();
        queue.try_push(second, intent("second")).unwrap();

        let removed = queue.remove(second).unwrap();
        assert_eq!(removed.command_id(), second);
        assert_eq!(queue.len(), 1);

        let (command_id, _) = queue.pop_front().unwrap().into_parts();
        assert_eq!(command_id, first);
        assert!(queue.is_empty());
    }

    #[test]
    fn follow_up_queue_rejects_duplicate_and_over_capacity_admission() {
        let mut queue = FollowUpQueue::new();
        let duplicate = command_id(7);
        queue.try_push(duplicate, intent("first")).unwrap();
        assert_eq!(
            queue.try_push(duplicate, intent("different")),
            Err(FollowUpQueueError::DuplicateCommandId)
        );

        for value in 1..=u8::try_from(FOLLOW_UP_QUEUE_CAPACITY).unwrap() {
            if value != 7 {
                queue.try_push(command_id(value), intent("queued")).unwrap();
            }
        }
        assert_eq!(queue.len(), FOLLOW_UP_QUEUE_CAPACITY);
        assert_eq!(
            queue.try_push(command_id(31), intent("overflow")),
            Err(FollowUpQueueError::Full)
        );
    }
}
