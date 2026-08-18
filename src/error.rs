use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicErrorCode {
    InvalidInput,
    Busy,
    NotFound,
    Closing,
    InteractionMismatch,
    Unavailable,
    Cancelled,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicErrorSummary {
    pub code: PublicErrorCode,
    pub retryable: bool,
}

impl PublicErrorSummary {
    pub const fn new(code: PublicErrorCode) -> Self {
        Self {
            code,
            retryable: matches!(code, PublicErrorCode::Busy | PublicErrorCode::Unavailable),
        }
    }

    pub const fn with_retryable(code: PublicErrorCode, retryable: bool) -> Self {
        Self { code, retryable }
    }

    pub const fn code(&self) -> PublicErrorCode {
        self.code
    }

    pub const fn retryable(&self) -> bool {
        self.retryable
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
pub enum RuntimeError {
    #[error("invalid runtime configuration")]
    InvalidConfiguration,
    #[error("runtime is closing")]
    Closing,
    #[error("runtime internal failure")]
    Internal,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
pub enum SessionError {
    #[error("session not found")]
    NotFound,
    #[error("session is already loaded")]
    AlreadyLoaded,
    #[error("session is busy")]
    Busy,
    #[error("session is closing")]
    Closing,
    #[error("interaction does not match the current turn")]
    InteractionMismatch,
    #[error("session input is invalid")]
    InvalidInput,
    #[error("session is unavailable")]
    Unavailable,
    #[error("session internal failure")]
    Internal,
}
