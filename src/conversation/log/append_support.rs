use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::sync::{Mutex, oneshot};

use super::*;
use crate::config::{CompactionConfig, KernelConfig, SessionManifest, SessionSpec, Timestamp};
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::{ToolCallId, TurnId};
use crate::model::{ModelFinishReason, ModelRef, ReasoningPreference, ToolCall, Usage};
use crate::storage::{
    AppendReceipt, ConversationPage, LogFuture, SessionLog, SessionLogError, SessionLogErrorKind,
};
use crate::time::TimestampError;
use crate::tools::{ToolName, ToolResultOutcome};
use crate::value::BoundedText;

#[derive(Clone, Copy)]
pub(super) enum InitializeScript {
    Normal,
    NonZero,
    Error(SessionLogErrorKind),
    Delay,
    Panic,
}

#[derive(Clone, Copy)]
pub(super) enum AppendScript {
    Normal,
    Error(SessionLogErrorKind),
    ErrorOnce(SessionLogErrorKind),
    UnknownOutcome,
    Delay,
    Panic,
    BadReceipt,
    LateCommitAfterTimeout,
}

#[derive(Clone, Copy)]
pub(super) enum CloseScript {
    Success,
    Error(SessionLogErrorKind),
    Timeout,
    Panic,
}

pub(super) struct Audit {
    pub(super) append_calls: AtomicUsize,
    pub(super) durable_entries: AtomicUsize,
    pub(super) timestamp_calls: AtomicUsize,
    pub(super) close_calls: AtomicUsize,
    pub(super) started_sender: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    pub(super) release_sender: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) release_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    pub(super) done_sender: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) done_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    pub(super) adapter_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) close_started_sender: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) close_started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    pub(super) initialize_started_sender: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) initialize_started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    pub(super) append_started_sender: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) append_started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Audit {
    pub(super) async fn take_initialize_started(&self) -> oneshot::Receiver<()> {
        self.initialize_started_receiver
            .lock()
            .await
            .take()
            .unwrap()
    }

    pub(super) async fn take_close_started(&self) -> oneshot::Receiver<()> {
        self.close_started_receiver.lock().await.take().unwrap()
    }

    pub(super) async fn take_append_started(&self) -> oneshot::Receiver<()> {
        self.append_started_receiver.lock().await.take().unwrap()
    }

    pub(super) fn new() -> Self {
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        let (done_sender, done_receiver) = oneshot::channel();
        let (close_started_sender, close_started_receiver) = oneshot::channel();
        let (initialize_started_sender, initialize_started_receiver) = oneshot::channel();
        let (append_started_sender, append_started_receiver) = oneshot::channel();
        Self {
            append_calls: AtomicUsize::new(0),
            durable_entries: AtomicUsize::new(0),
            timestamp_calls: AtomicUsize::new(0),
            close_calls: AtomicUsize::new(0),
            started_sender: Mutex::new(Some(started_sender)),
            started_receiver: Mutex::new(Some(started_receiver)),
            release_sender: Mutex::new(Some(release_sender)),
            release_receiver: Mutex::new(Some(release_receiver)),
            done_sender: Mutex::new(Some(done_sender)),
            done_receiver: Mutex::new(Some(done_receiver)),
            adapter_task: Mutex::new(None),
            close_started_sender: Mutex::new(Some(close_started_sender)),
            close_started_receiver: Mutex::new(Some(close_started_receiver)),
            initialize_started_sender: Mutex::new(Some(initialize_started_sender)),
            initialize_started_receiver: Mutex::new(Some(initialize_started_receiver)),
            append_started_sender: Mutex::new(Some(append_started_sender)),
            append_started_receiver: Mutex::new(Some(append_started_receiver)),
        }
    }
}

impl Default for Audit {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) struct LocalLog {
    audit: Arc<Audit>,
    initialize_script: InitializeScript,
    append_script: AppendScript,
    close_script: CloseScript,
    append_error_consumed: bool,
    initialized: bool,
    closed: bool,
    manifest: Option<SessionManifest>,
    head: ConversationSeq,
    entries: Vec<ConversationEntry>,
}

impl LocalLog {
    pub(super) fn new(
        audit: Arc<Audit>,
        initialize_script: InitializeScript,
        append_script: AppendScript,
    ) -> Self {
        Self {
            audit,
            initialize_script,
            append_script,
            close_script: CloseScript::Success,
            append_error_consumed: false,
            initialized: false,
            closed: false,
            manifest: None,
            head: ConversationSeq::ZERO,
            entries: Vec::new(),
        }
    }

    pub(super) fn with_close_script(mut self, script: CloseScript) -> Self {
        self.close_script = script;
        self
    }

    fn record_append(
        &mut self,
        expected_head: ConversationSeq,
        entries: &[ConversationEntry],
    ) -> AppendReceipt {
        let new_head = entries.last().map(ConversationEntry::seq).unwrap();
        self.entries.extend(entries.iter().cloned());
        self.head = new_head;
        self.audit
            .durable_entries
            .fetch_add(entries.len(), Ordering::SeqCst);
        AppendReceipt {
            previous_head: expected_head,
            new_head,
            appended: entries.len(),
        }
    }
}

impl SessionLog for LocalLog {
    fn initialize<'a>(&'a mut self, manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        let script = self.initialize_script;
        Box::pin(async move {
            match script {
                InitializeScript::Normal => {
                    self.initialized = true;
                    self.manifest = Some(manifest);
                    Ok(ConversationSeq::ZERO)
                }
                InitializeScript::NonZero => {
                    self.initialized = true;
                    self.manifest = Some(manifest);
                    Ok(ConversationSeq::new(1))
                }
                InitializeScript::Error(kind) => Err(log_error(kind)),
                InitializeScript::Delay => {
                    let sender = self
                        .audit
                        .initialize_started_sender
                        .lock()
                        .await
                        .take()
                        .unwrap();
                    let _ = sender.send(());
                    std::future::pending::<Result<ConversationSeq, SessionLogError>>().await
                }
                InitializeScript::Panic => panic!("local initialize panic"),
            }
        })
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        Box::pin(async move {
            if !self.initialized {
                return Err(log_error(SessionLogErrorKind::NotInitialized));
            }
            self.manifest
                .clone()
                .ok_or_else(|| log_error(SessionLogErrorKind::Corrupt))
        })
    }

    fn read_page<'a>(
        &'a mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        Box::pin(async move {
            let start = after.map_or(0, |value| value.get() as usize);
            let entries: Vec<_> = self
                .entries
                .iter()
                .skip(start)
                .take(limit)
                .cloned()
                .collect();
            let next_after = entries.last().map(ConversationEntry::seq);
            Ok(ConversationPage {
                entries,
                next_after,
                observed_head: self.head,
            })
        })
    }

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        self.audit.append_calls.fetch_add(1, Ordering::SeqCst);
        let script = match self.append_script {
            AppendScript::ErrorOnce(kind) if !self.append_error_consumed => {
                self.append_error_consumed = true;
                AppendScript::Error(kind)
            }
            AppendScript::ErrorOnce(_) => AppendScript::Normal,
            script => script,
        };
        Box::pin(async move {
            match script {
                AppendScript::Normal => Ok(self.record_append(expected_head, &entries)),
                AppendScript::Error(kind) => Err(log_error(kind)),
                AppendScript::ErrorOnce(_) => unreachable!("ErrorOnce resolved before future"),
                AppendScript::UnknownOutcome => {
                    let _ = self.record_append(expected_head, &entries);
                    Err(log_error(SessionLogErrorKind::UnknownOutcome))
                }
                AppendScript::Delay => {
                    let sender = self
                        .audit
                        .append_started_sender
                        .lock()
                        .await
                        .take()
                        .unwrap();
                    let _ = sender.send(());
                    std::future::pending::<Result<AppendReceipt, SessionLogError>>().await
                }
                AppendScript::Panic => panic!("local append panic"),
                AppendScript::LateCommitAfterTimeout => {
                    let audit = Arc::clone(&self.audit);
                    let count = entries.len();
                    let release = audit.release_receiver.lock().await.take().unwrap();
                    let done = audit.done_sender.lock().await.take().unwrap();
                    // This detached task simulates adapter-owned work, not Core-owned
                    // production work.
                    let task_audit = Arc::clone(&audit);
                    let handle = tokio::spawn(async move {
                        let _ = release.await;
                        task_audit
                            .durable_entries
                            .fetch_add(count, Ordering::SeqCst);
                        let _ = done.send(());
                    });
                    *audit.adapter_task.lock().await = Some(handle);
                    let _ = audit.started_sender.lock().await.take().unwrap().send(());
                    std::future::pending::<Result<AppendReceipt, SessionLogError>>().await
                }
                AppendScript::BadReceipt => {
                    let receipt = self.record_append(expected_head, &entries);
                    Ok(AppendReceipt {
                        previous_head: receipt.previous_head,
                        new_head: receipt.previous_head,
                        appended: 0,
                    })
                }
            }
        })
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        self.audit.close_calls.fetch_add(1, Ordering::SeqCst);
        let script = self.close_script;
        Box::pin(async move {
            match script {
                CloseScript::Success => {
                    if self.closed {
                        return Err(log_error(SessionLogErrorKind::Closed));
                    }
                    self.closed = true;
                    Ok(())
                }
                CloseScript::Error(kind) => Err(log_error(kind)),
                CloseScript::Timeout => {
                    let sender = self.audit.close_started_sender.lock().await.take().unwrap();
                    let _ = sender.send(());
                    std::future::pending::<Result<(), SessionLogError>>().await
                }
                CloseScript::Panic => panic!("local close panic"),
            }
        })
    }
}

pub(super) fn log_error(kind: SessionLogErrorKind) -> SessionLogError {
    SessionLogError::new(
        kind,
        DiagnosticSummary::new(
            DiagnosticCode::Internal,
            DiagnosticCategory::Storage,
            BoundedText::new("local log error").unwrap(),
            false,
        ),
    )
}

pub(super) fn timestamp() -> Result<Timestamp, TimestampError> {
    "2026-08-19T12:34:56.789Z".parse()
}

pub(super) fn failing_timestamp() -> Result<Timestamp, TimestampError> {
    Err(TimestampError::Invalid)
}

pub(super) fn timestamp_source(audit: Arc<Audit>) -> TimestampSource {
    Box::new(move || {
        audit.timestamp_calls.fetch_add(1, Ordering::SeqCst);
        timestamp()
    })
}

pub(super) fn spec() -> SessionSpec {
    SessionSpec::new(
        "model:v1".parse::<ModelRef>().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        ["read_file".parse().unwrap()].into_iter().collect(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

pub(super) fn manifest() -> SessionManifest {
    SessionManifest {
        format_version: SessionManifest::FORMAT_VERSION,
        session_id: "ses_00000000000000000000000000000001".parse().unwrap(),
        created_at: timestamp().unwrap(),
        spec: spec(),
    }
}

pub(super) fn kernel(timeout: Duration) -> KernelConfig {
    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.log_operation_timeout = timeout;
    kernel
}

pub(super) async fn open(
    initialize_script: InitializeScript,
    append_script: AppendScript,
    timeout: Duration,
) -> (ConversationLog, Arc<Audit>) {
    let audit = Arc::new(Audit::default());
    let local = LocalLog::new(Arc::clone(&audit), initialize_script, append_script);
    let log = ConversationLog::initialize(
        Box::new(local),
        manifest(),
        kernel(timeout),
        timestamp_source(Arc::clone(&audit)),
    )
    .await
    .unwrap();
    (log, audit)
}

pub(super) async fn initialize_with_kernel(
    manifest: SessionManifest,
    initialize_script: InitializeScript,
    close_script: CloseScript,
    kernel: KernelConfig,
) -> (Result<ConversationLog, ConversationCommitError>, Arc<Audit>) {
    let audit = Arc::new(Audit::default());
    let local = LocalLog::new(Arc::clone(&audit), initialize_script, AppendScript::Normal)
        .with_close_script(close_script);
    let result = ConversationLog::initialize(
        Box::new(local),
        manifest,
        kernel,
        timestamp_source(Arc::clone(&audit)),
    )
    .await;
    (result, audit)
}

pub(super) fn user(turn_id: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::UserMessage(UserMessageDraft {
        turn_id,
        input: UserInputRecord::new(BoundedText::new("hello").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            "model:v1".parse().unwrap(),
            ReasoningPreference::Auto,
            4,
        )
        .unwrap(),
    })
}

pub(super) fn tool_call(id: &str, index: u32) -> ToolCall {
    ToolCall::new(
        id.parse::<ToolCallId>().unwrap(),
        "read_file".parse::<ToolName>().unwrap(),
        json!({"path": "a"}),
        index,
    )
    .unwrap()
}

pub(super) fn assistant_with_tool(turn_id: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::AssistantMessage(AssistantMessageDraft {
        turn_id,
        model: "model:v1".parse().unwrap(),
        text: None,
        reasoning: None,
        tool_calls: vec![tool_call("call-a", 0)],
        usage: Usage::default(),
        finish_reason: ModelFinishReason::ToolCalls,
    })
}

pub(super) fn assistant_final(
    turn_id: TurnId,
    finish_reason: ModelFinishReason,
) -> UnsequencedEntry {
    UnsequencedEntry::AssistantMessage(AssistantMessageDraft {
        turn_id,
        model: "model:v1".parse().unwrap(),
        text: Some(BoundedText::new("done").unwrap()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: Usage::default(),
        finish_reason,
    })
}

pub(super) fn tool_result(turn_id: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::ToolResult(ToolResultDraft {
        turn_id,
        tool_call_id: "call-a".parse().unwrap(),
        tool_name: "read_file".parse().unwrap(),
        outcome: ToolResultOutcome::Success,
        content: BoundedText::new("ok").unwrap(),
    })
}

pub(super) fn summary() -> UnsequencedEntry {
    UnsequencedEntry::Summary(SummaryDraft {
        through: ConversationSeq::new(5),
        summary: BoundedText::new("summary").unwrap(),
    })
}

pub(super) fn terminal(turn_id: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::TurnTerminal(TurnTerminalDraft {
        turn_id,
        terminal: TurnTerminal::Completed,
        usage: Usage::default(),
    })
}

pub(super) fn valid_batch(turn_id: TurnId) -> Vec<UnsequencedEntry> {
    vec![
        user(turn_id),
        assistant_with_tool(turn_id),
        tool_result(turn_id),
        assistant_final(turn_id, ModelFinishReason::Stop),
        terminal(turn_id),
        summary(),
    ]
}

pub(super) fn assert_empty_confirmed(log: &ConversationLog) {
    assert_eq!(log.head(), ConversationSeq::ZERO);
    assert!(log.projection().entries().is_empty());
}
