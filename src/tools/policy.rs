use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::value::BoundedText;

use super::tool::{ToolInvocation, ToolSpec};
use super::types::valid_text;

pub const MAX_TOOL_POLICY_TEXT_BYTES: usize = 8_192;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolPolicyError {
    #[error("tool policy evaluation was cancelled")]
    Cancelled,
    #[error("tool policy evaluation failed")]
    Failed,
    #[error("tool policy returned an invalid decision")]
    InvalidDecision,
    #[error("tool policy evaluation panicked")]
    Panicked,
    #[error("tool policy evaluation failed internally")]
    Internal,
}

#[derive(Clone)]
pub struct ToolPolicyRequest {
    pub invocation: ToolInvocation,
    pub spec: ToolSpec,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

impl fmt::Debug for ToolPolicyRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolPolicyRequest")
            .field("invocation", &self.invocation)
            .field("spec", &self.spec)
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalRisk {
    Low,
    Medium,
    High,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ApprovalRequest {
    pub prompt: BoundedText,
    pub risk: ApprovalRisk,
}

impl ApprovalRequest {
    pub fn new(prompt: impl AsRef<str>, risk: ApprovalRisk) -> Result<Self, ToolPolicyError> {
        let request = Self {
            prompt: checked_policy_text(prompt.as_ref())?,
            risk,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ToolPolicyError> {
        validate_policy_text(self.prompt.as_str())
    }
}

impl fmt::Debug for ApprovalRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApprovalRequest")
            .field("prompt_bytes", &self.prompt.byte_len())
            .field("risk", &self.risk)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    AllowOnce,
    Deny,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ToolDecision {
    Allow,
    Deny { reason: BoundedText },
    RequireApproval { request: ApprovalRequest },
}

impl ToolDecision {
    pub fn deny(reason: impl AsRef<str>) -> Result<Self, ToolPolicyError> {
        let decision = Self::Deny {
            reason: checked_policy_text(reason.as_ref())?,
        };
        decision.validate()?;
        Ok(decision)
    }

    pub fn require_approval(request: ApprovalRequest) -> Result<Self, ToolPolicyError> {
        let decision = Self::RequireApproval { request };
        decision.validate()?;
        Ok(decision)
    }

    pub fn validate(&self) -> Result<(), ToolPolicyError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny { reason } => validate_policy_text(reason.as_str()),
            Self::RequireApproval { request } => request.validate(),
        }
    }
}

impl fmt::Debug for ToolDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow => formatter.write_str("Allow"),
            Self::Deny { reason } => formatter
                .debug_struct("Deny")
                .field("reason_bytes", &reason.byte_len())
                .finish(),
            Self::RequireApproval { request } => formatter
                .debug_struct("RequireApproval")
                .field("request", request)
                .finish(),
        }
    }
}

pub type ToolPolicyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolDecision, ToolPolicyError>> + Send + 'a>>;

pub trait ToolPolicy: Send + Sync + 'static {
    fn decide<'a>(&'a self, request: ToolPolicyRequest) -> ToolPolicyFuture<'a>;
}

impl<T: ToolPolicy + ?Sized> ToolPolicy for Arc<T> {
    fn decide<'a>(&'a self, request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        (**self).decide(request)
    }
}

fn checked_policy_text(value: &str) -> Result<BoundedText, ToolPolicyError> {
    validate_policy_text(value)?;
    BoundedText::new_with_max_bytes(value, MAX_TOOL_POLICY_TEXT_BYTES)
        .map_err(|_| ToolPolicyError::InvalidDecision)
}

fn validate_policy_text(value: &str) -> Result<(), ToolPolicyError> {
    if valid_text(value, MAX_TOOL_POLICY_TEXT_BYTES, false) {
        Ok(())
    } else {
        Err(ToolPolicyError::InvalidDecision)
    }
}
