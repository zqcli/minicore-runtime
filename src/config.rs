use std::time::Duration;

use thiserror::Error;

pub use crate::time::{Timestamp, TimestampError};

mod kernel;
mod retry;
mod session;
mod session_spec;

pub use kernel::{KernelConfig, SemanticLimits};
pub use retry::{RetryPolicy, RetryPolicyError};
pub use session::{SessionManifest, TurnOptions, UserInput};
pub use session_spec::{CompactionConfig, SessionSpec};

pub const MAX_KERNEL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ConfigError {
    #[error("configuration path must be absolute and free of dot components")]
    InvalidPath,
    #[error("configuration text is empty, unsafe, or too large")]
    InvalidText,
    #[error("configuration value is outside its bound")]
    InvalidBounds,
    #[error("configuration retry policy is invalid")]
    InvalidRetryPolicy,
    #[error("configuration timestamp could not be created")]
    Timestamp,
}
