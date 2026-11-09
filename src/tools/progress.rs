use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::value::{Text, valid_text};

#[derive(Clone, Eq, PartialEq)]
pub struct ToolProgress {
    pub message: Option<Text>,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

impl fmt::Debug for ToolProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolProgress")
            .field("message_bytes", &self.message.as_ref().map(Text::byte_len))
            .field("completed", &self.completed)
            .field("total", &self.total)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolProgressError {
    #[error("tool progress contains disallowed control characters")]
    InvalidText,
    #[error("tool progress completed value exceeds total")]
    CompletedExceedsTotal,
}

impl ToolProgress {
    pub fn new(
        message: Option<Text>,
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
        if self
            .message
            .as_ref()
            .is_some_and(|message| !valid_text(message.as_str(), true))
        {
            return Err(ToolProgressError::InvalidText);
        }
        if let (Some(completed), Some(total)) = (self.completed, self.total)
            && completed > total
        {
            return Err(ToolProgressError::CompletedExceedsTotal);
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
    use crate::value::Text;

    struct ChannelEmitter {
        sender: mpsc::UnboundedSender<ToolProgress>,
    }

    impl ToolProgressEmitter for ChannelEmitter {
        fn emit(&self, progress: ToolProgress) -> bool {
            self.sender.send(progress).is_ok()
        }
    }

    fn channel() -> (ToolProgressSink, mpsc::UnboundedReceiver<ToolProgress>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            ToolProgressSink::from_emitter(ChannelEmitter { sender }),
            receiver,
        )
    }

    fn progress() -> ToolProgress {
        ToolProgress::new(Some(Text::new("working")), Some(1), Some(2)).unwrap()
    }

    #[test]
    fn send_is_positive_when_the_channel_accepts() {
        let (sink, mut receiver) = channel();
        assert!(sink.emit(progress()));
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn send_retains_bursts_and_reports_closed_without_waiting() {
        let (sink, receiver) = channel();
        assert!(sink.emit(progress()));
        for _ in 0..4097 {
            assert!(sink.emit(progress()));
        }
        assert_eq!(receiver.len(), 4098);
        drop(receiver);
        assert!(!sink.emit(progress()));
    }

    #[test]
    fn invalid_progress_is_rejected_before_the_emitter() {
        let (sink, mut receiver) = channel();
        let invalid = ToolProgress {
            message: None,
            completed: Some(2),
            total: Some(1),
        };
        assert!(!sink.emit(invalid));
        assert!(receiver.try_recv().is_err());
    }
}
