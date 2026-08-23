use std::sync::Arc;

use thiserror::Error;

use crate::value::BoundedText;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolProgress {
    pub message: Option<BoundedText>,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolProgressError {
    #[error("tool progress completed value exceeds total")]
    CompletedExceedsTotal,
}

impl ToolProgress {
    pub fn new(
        message: Option<BoundedText>,
        completed: Option<u64>,
        total: Option<u64>,
    ) -> Result<Self, ToolProgressError> {
        let progress = Self {
            message,
            completed,
            total,
        };
        progress.validate()?;
        Ok(progress)
    }

    pub fn validate(&self) -> Result<(), ToolProgressError> {
        if let (Some(completed), Some(total)) = (self.completed, self.total) {
            if completed > total {
                return Err(ToolProgressError::CompletedExceedsTotal);
            }
        }
        Ok(())
    }
}

pub(crate) trait ToolProgressEmitter: Send + Sync {
    fn emit(&self, progress: ToolProgress) -> bool;
}

struct NoopToolProgressEmitter;

impl ToolProgressEmitter for NoopToolProgressEmitter {
    fn emit(&self, _progress: ToolProgress) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct ToolProgressSink {
    inner: Arc<dyn ToolProgressEmitter>,
}

const _: () = {
    let _ = ToolProgressSink::from_emitter::<NoopToolProgressEmitter>;
};

impl Default for ToolProgressSink {
    fn default() -> Self {
        Self {
            inner: Arc::new(NoopToolProgressEmitter),
        }
    }
}

impl ToolProgressSink {
    pub fn emit(&self, progress: ToolProgress) -> bool {
        if progress.validate().is_err() {
            return false;
        }
        self.inner.emit(progress)
    }

    pub(crate) fn from_emitter<E>(emitter: E) -> Self
    where
        E: ToolProgressEmitter + 'static,
    {
        Self {
            inner: Arc::new(emitter),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::{ToolProgress, ToolProgressEmitter, ToolProgressSink};
    use crate::value::BoundedText;

    struct ChannelEmitter {
        sender: mpsc::Sender<ToolProgress>,
    }

    impl ToolProgressEmitter for ChannelEmitter {
        fn emit(&self, progress: ToolProgress) -> bool {
            self.sender.try_send(progress).is_ok()
        }
    }

    fn channel(capacity: usize) -> (ToolProgressSink, mpsc::Receiver<ToolProgress>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            ToolProgressSink::from_emitter(ChannelEmitter { sender }),
            receiver,
        )
    }

    fn progress() -> ToolProgress {
        ToolProgress::new(Some(BoundedText::new("working").unwrap()), Some(1), Some(2)).unwrap()
    }

    #[test]
    fn try_send_is_positive_when_the_bounded_channel_accepts() {
        let (sink, mut receiver) = channel(1);
        assert!(sink.emit(progress()));
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn try_send_reports_full_and_closed_without_waiting() {
        let (sink, receiver) = channel(1);
        assert!(sink.emit(progress()));
        assert!(!sink.emit(progress()));
        drop(receiver);
        assert!(!sink.emit(progress()));
    }

    #[test]
    fn invalid_progress_is_rejected_before_the_emitter() {
        let (sink, mut receiver) = channel(1);
        let invalid = ToolProgress {
            message: None,
            completed: Some(2),
            total: Some(1),
        };
        assert!(!sink.emit(invalid));
        assert!(receiver.try_recv().is_err());
    }
}
