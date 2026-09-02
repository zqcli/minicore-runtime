use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::error::DiagnosticSummary;
use crate::execution::ConfigRevision;
use crate::history::HistoryItem;
use crate::ids::LoopId;
use crate::limits::LoopLimits;
use crate::model::Usage;

const MAX_TOOL_ROUNDS: u16 = 1_024;
const MAX_PENDING_STEERS: usize = 64;
const MAX_EVENT_CAPACITY: usize = 4_096;
const MAX_RETRY_ATTEMPTS: u8 = 10;

/// Options for one agent loop.
///
/// All fields are public; start from [`LoopOptions::default_checked`] and
/// mutate only what the call site needs. There is deliberately no builder.
#[derive(Clone, Debug)]
pub struct LoopOptions {
    pub deadline: Option<tokio::time::Instant>,
    pub max_tool_rounds: u16,
    pub max_pending_steers: usize,
    pub event_capacity: usize,
    pub prompt_timeout: Duration,
    pub model_timeout: Duration,
    pub policy_timeout: Duration,
    pub tool_timeout: Duration,
    pub model_retry_attempts: u8,
    pub model_retry_base_delay: Duration,
    pub limits: LoopLimits,
}

impl LoopOptions {
    pub fn default_checked() -> Result<Self, LoopStartError> {
        let options = Self {
            deadline: None,
            max_tool_rounds: 32,
            max_pending_steers: 16,
            event_capacity: 256,
            prompt_timeout: Duration::from_secs(30),
            model_timeout: Duration::from_secs(10 * 60),
            policy_timeout: Duration::from_secs(30),
            tool_timeout: Duration::from_secs(30 * 60),
            model_retry_attempts: 2,
            model_retry_base_delay: Duration::from_millis(100),
            limits: LoopLimits::default(),
        };
        options.validate()?;
        Ok(options)
    }

    pub fn validate(&self) -> Result<(), LoopStartError> {
        if !(1..=MAX_TOOL_ROUNDS).contains(&self.max_tool_rounds)
            || !(1..=MAX_PENDING_STEERS).contains(&self.max_pending_steers)
            || !(1..=MAX_EVENT_CAPACITY).contains(&self.event_capacity)
            || self.prompt_timeout.is_zero()
            || self.model_timeout.is_zero()
            || self.policy_timeout.is_zero()
            || self.tool_timeout.is_zero()
            || self.model_retry_attempts == 0
            || self.model_retry_attempts > MAX_RETRY_ATTEMPTS
            || self.model_retry_base_delay.is_zero()
            || self
                .deadline
                .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            return Err(LoopStartError::InvalidOptions);
        }
        self.limits
            .validate()
            .map_err(|_| LoopStartError::InvalidOptions)
    }
}

/// Incremental result of one finished agent loop.
///
/// `appended` holds only items this loop produced in memory; the host decides
/// when, whether, and how to persist them.
#[derive(Clone, Debug)]
pub struct LoopReport {
    pub loop_id: LoopId,
    pub outcome: LoopOutcome,
    pub appended: Arc<[HistoryItem]>,
    pub usage: Usage,
    pub requests: u32,
    pub tool_rounds: u16,
    pub final_config_revision: ConfigRevision,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoopOutcome {
    Completed,
    Cancelled(CancelReason),
    Failed(LoopFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelReason {
    User,
    OwnerDropped,
    Shutdown,
    Deadline,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoopFailure {
    pub kind: LoopFailureKind,
    pub diagnostic: DiagnosticSummary,
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopFailureKind {
    Prompt,
    Model,
    InvalidModelResponse,
    OutputLimit,
    Refused,
    ContentFiltered,
    Policy,
    Interaction,
    MaxToolRounds,
    Internal,
}

/// Errors that prevent an agent loop from starting.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LoopStartError {
    #[error("starting an agent loop requires a running Tokio runtime")]
    NoTokioRuntime,
    #[error("loop options are invalid")]
    InvalidOptions,
    #[error("execution config is invalid")]
    InvalidConfig,
    #[error("loop input is invalid")]
    InvalidInput,
    #[error("history exceeds the configured loop limits")]
    HistoryTooLarge,
    #[error("loop id generation failed")]
    IdGeneration,
}
