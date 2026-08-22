use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::config::SessionManifest;
use crate::conversation::{ConversationEntry, ConversationSeq};
use crate::error::DiagnosticSummary;

pub type LogFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SessionLogError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationPage {
    pub entries: Vec<ConversationEntry>,
    pub next_after: Option<ConversationSeq>,
    pub observed_head: ConversationSeq,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendReceipt {
    pub previous_head: ConversationSeq,
    pub new_head: ConversationSeq,
    pub appended: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLogErrorKind {
    NotInitialized,
    AlreadyInitialized,
    Conflict,
    Corrupt,
    Unavailable,
    UnknownOutcome,
    Closed,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLogError {
    kind: SessionLogErrorKind,
    diagnostic: DiagnosticSummary,
}

impl SessionLogError {
    pub fn new(kind: SessionLogErrorKind, diagnostic: DiagnosticSummary) -> Self {
        Self { kind, diagnostic }
    }

    pub const fn kind(&self) -> SessionLogErrorKind {
        self.kind
    }

    pub const fn diagnostic(&self) -> &DiagnosticSummary {
        &self.diagnostic
    }
}

impl fmt::Display for SessionLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "session log error: {:?}", self.kind)
    }
}

impl std::error::Error for SessionLogError {}

pub trait SessionLog: Send + 'static {
    fn initialize<'a>(&'a mut self, manifest: SessionManifest) -> LogFuture<'a, ConversationSeq>;

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest>;

    fn read_page<'a>(
        &'a mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> LogFuture<'a, ConversationPage>;

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt>;

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()>;
}
