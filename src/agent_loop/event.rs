use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use tokio::sync::mpsc;

use crate::agent_loop::{CancelReason, LoopFailureKind, LoopState};
use crate::execution::ConfigRevision;
use crate::ids::{InteractionId, LoopId, ToolCallId};
use crate::interaction::PendingInteraction;
use crate::model::{ModelRef, ReasoningPreference};
use crate::tools::{ToolName, ToolProgress, ToolResultOutcome};
use crate::value::BoundedText;

/// Streaming channel of a model response, used to tag `OutputDelta` events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputChannel {
    Text,
    Reasoning,
}

/// Lightweight, redacted summary of how a loop ended, for the best-effort
/// `Finished` event. It never carries diagnostics or history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopOutcomeSummary {
    Completed,
    Cancelled(CancelReason),
    Failed(LoopFailureKind),
}

/// Best-effort live event for one agent loop.
///
/// Events never participate in execution correctness; the authoritative result
/// is `LoopReport` delivered through `wait`/`join`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LoopEvent {
    Started {
        loop_id: LoopId,
    },
    StateChanged {
        state: LoopState,
    },
    RequestStarted {
        loop_id: LoopId,
        request_index: u32,
        config_revision: ConfigRevision,
        model: ModelRef,
        reasoning: ReasoningPreference,
    },
    OutputDelta {
        loop_id: LoopId,
        request_index: u32,
        channel: OutputChannel,
        delta: BoundedText,
    },
    ToolStarted {
        loop_id: LoopId,
        request_index: u32,
        call_id: ToolCallId,
        tool_name: ToolName,
    },
    ToolProgress {
        loop_id: LoopId,
        request_index: u32,
        call_id: ToolCallId,
        progress: ToolProgress,
    },
    ToolFinished {
        loop_id: LoopId,
        request_index: u32,
        call_id: ToolCallId,
        outcome: ToolResultOutcome,
        output_bytes: usize,
    },
    InteractionRequested {
        loop_id: LoopId,
        interaction: PendingInteraction,
    },
    InteractionResolved {
        loop_id: LoopId,
        interaction_id: InteractionId,
    },
    Finished {
        loop_id: LoopId,
        outcome: LoopOutcomeSummary,
    },
}

/// A best-effort live event with the number of events lost before it by the
/// bounded event queue. Informational only; not a replay cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopEventEnvelope {
    pub dropped_before: u64,
    pub event: LoopEvent,
}

/// The single-consumer, bounded, best-effort stream for live loop events.
///
/// Dropping `LoopEventStream` has no execution effect; the runner keeps going
/// and simply stops being observed.
#[must_use = "LoopEventStream must be consumed or intentionally dropped"]
pub struct LoopEventStream {
    receiver: mpsc::Receiver<LoopEventEnvelope>,
}

/// Crate-private event sink owned by the runner task. Every emission is a
/// `try_send`; a full or closed queue never blocks execution.
pub(crate) struct LoopEventSink {
    sender: mpsc::Sender<LoopEventEnvelope>,
    dropped: u64,
    closed: bool,
}

impl LoopEventStream {
    pub async fn recv(&mut self) -> Option<LoopEventEnvelope> {
        self.receiver.recv().await
    }

    pub fn try_recv(&mut self) -> Result<LoopEventEnvelope, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Stream for LoopEventStream {
    type Item = LoopEventEnvelope;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(context)
    }
}

impl fmt::Debug for LoopEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LoopEventStream { .. }")
    }
}

impl LoopEventSink {
    pub(crate) fn channel(capacity: usize) -> Result<(Self, LoopEventStream), EventSinkError> {
        if capacity == 0 || capacity > MAX_EVENT_CAPACITY {
            return Err(EventSinkError::InvalidCapacity);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                sender,
                dropped: 0,
                closed: false,
            },
            LoopEventStream { receiver },
        ))
    }

    pub(crate) fn record_dropped(&mut self, count: u64) {
        self.dropped = self.dropped.saturating_add(count);
    }

    pub(crate) fn try_emit(&mut self, event: LoopEvent) {
        if self.closed {
            return;
        }
        let dropped_before = self.dropped;
        let envelope = LoopEventEnvelope {
            dropped_before,
            event,
        };
        match self.sender.try_send(envelope) {
            Ok(()) => self.dropped = 0,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped = self.dropped.saturating_add(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => self.closed = true,
        }
    }
}

const MAX_EVENT_CAPACITY: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventSinkError {
    InvalidCapacity,
}
