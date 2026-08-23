use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::config::SessionManifest;
use crate::conversation::{ConversationEntry, ConversationSeq};
use crate::error::SessionLogError;

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
