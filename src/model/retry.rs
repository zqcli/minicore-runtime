use std::time::SystemTime;

use tokio::time::Instant;

use super::ModelError;

/// Progress of an internal retry, associated with its enclosing loop and request index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRetryProgress {
    /// Total attempt number including the initial call, starting at two for retries.
    pub attempt: u8,
    pub max_attempts: u8,
    /// The preceding attempt's original structured failure.
    pub error: ModelError,
    pub phase: ModelRetryPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelRetryPhase {
    Waiting {
        /// Source wall-clock estimate for display, never used to schedule execution.
        retry_at: SystemTime,
        /// The monotonic target actually used by the driver's backoff wait.
        retry_deadline: Instant,
    },
    Requesting,
}
