use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{Mutex, oneshot};

use super::*;
use crate::config::{CompactionConfig, KernelConfig, SessionManifest, SessionSpec, Timestamp};
use crate::conversation::LoadCompatibilityValidated;
use crate::conversation::load::PendingConversationLoad;
use crate::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use crate::ids::{SessionId, ToolCallId, TurnId};
use crate::model::{ModelFinishReason, ModelRef, ReasoningPreference, ToolCall, Usage};
use crate::storage::{
    AppendReceipt, ConversationPage, LogFuture, SessionLog, SessionLogError, SessionLogErrorKind,
};
use crate::time::TimestampError;
use crate::tools::{ToolName, ToolResultOutcome};
use crate::value::BoundedText;

#[derive(Clone, Copy)]
pub(super) enum AppendMode {
    Success,
    Known(SessionLogErrorKind),
    Unknown,
    Timeout,
    Panic,
    BadReceipt,
}

#[derive(Clone, Copy)]
pub(super) enum CloseMode {
    Success,
    Known(SessionLogErrorKind),
    Timeout,
    Panic,
}

#[derive(Clone, Copy)]
pub(super) enum OperationMode {
    Success,
    Known(SessionLogErrorKind),
    Timeout,
    Panic,
}

pub(super) struct Audit {
    pub(super) pages: Mutex<VecDeque<ConversationPage>>,
    pub(super) durable: Mutex<Vec<ConversationEntry>>,
    pub(super) batches: Mutex<Vec<Vec<ConversationEntry>>>,
    pub(super) append_calls: AtomicUsize,
    pub(super) read_calls: AtomicUsize,
    pub(super) close_calls: AtomicUsize,
    force_read_unknown: AtomicBool,
    load_started_sender: Mutex<Option<oneshot::Sender<()>>>,
    load_started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    read_started_sender: Mutex<Option<oneshot::Sender<()>>>,
    read_started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    append_started_sender: Mutex<Option<oneshot::Sender<()>>>,
    append_started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    close_started_sender: Mutex<Option<oneshot::Sender<()>>>,
    close_started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Audit {
    pub(super) fn new(pages: Vec<ConversationPage>) -> Arc<Self> {
        let (load_started_sender, load_started_receiver) = oneshot::channel();
        let (read_started_sender, read_started_receiver) = oneshot::channel();
        let (append_started_sender, append_started_receiver) = oneshot::channel();
        let (close_started_sender, close_started_receiver) = oneshot::channel();
        Arc::new(Self {
            pages: Mutex::new(pages.into()),
            durable: Mutex::new(Vec::new()),
            batches: Mutex::new(Vec::new()),
            append_calls: AtomicUsize::new(0),
            read_calls: AtomicUsize::new(0),
            close_calls: AtomicUsize::new(0),
            force_read_unknown: AtomicBool::new(false),
            load_started_sender: Mutex::new(Some(load_started_sender)),
            load_started_receiver: Mutex::new(Some(load_started_receiver)),
            read_started_sender: Mutex::new(Some(read_started_sender)),
            read_started_receiver: Mutex::new(Some(read_started_receiver)),
            append_started_sender: Mutex::new(Some(append_started_sender)),
            append_started_receiver: Mutex::new(Some(append_started_receiver)),
            close_started_sender: Mutex::new(Some(close_started_sender)),
            close_started_receiver: Mutex::new(Some(close_started_receiver)),
        })
    }

    pub(super) async fn take_load_started(&self) -> oneshot::Receiver<()> {
        self.load_started_receiver.lock().await.take().unwrap()
    }

    pub(super) async fn take_read_started(&self) -> oneshot::Receiver<()> {
        self.read_started_receiver.lock().await.take().unwrap()
    }

    pub(super) async fn take_append_started(&self) -> oneshot::Receiver<()> {
        self.append_started_receiver.lock().await.take().unwrap()
    }

    pub(super) async fn take_close_started(&self) -> oneshot::Receiver<()> {
        self.close_started_receiver.lock().await.take().unwrap()
    }

    pub(super) fn force_read_unknown(&self) {
        self.force_read_unknown.store(true, Ordering::SeqCst);
    }
}

pub(super) struct PagedLog {
    audit: Arc<Audit>,
    manifest: SessionManifest,
    append_mode: AppendMode,
    close_mode: CloseMode,
    load_mode: OperationMode,
    read_mode: OperationMode,
    head: ConversationSeq,
}

impl PagedLog {
    pub(super) fn new(
        audit: Arc<Audit>,
        manifest: SessionManifest,
        append_mode: AppendMode,
        close_mode: CloseMode,
    ) -> Self {
        Self {
            audit,
            manifest,
            append_mode,
            close_mode,
            load_mode: OperationMode::Success,
            read_mode: OperationMode::Success,
            head: ConversationSeq::ZERO,
        }
    }

    pub(super) fn with_load_mode(mut self, mode: OperationMode) -> Self {
        self.load_mode = mode;
        self
    }

    pub(super) fn with_read_mode(mut self, mode: OperationMode) -> Self {
        self.read_mode = mode;
        self
    }

    async fn record_append(
        &mut self,
        expected_head: ConversationSeq,
        entries: &[ConversationEntry],
    ) -> AppendReceipt {
        let new_head = entries.last().map(ConversationEntry::seq).unwrap();
        self.head = new_head;
        self.audit
            .durable
            .lock()
            .await
            .extend(entries.iter().cloned());
        self.audit.batches.lock().await.push(entries.to_vec());
        AppendReceipt {
            previous_head: expected_head,
            new_head,
            appended: entries.len(),
        }
    }

    async fn generated_page(
        &self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> ConversationPage {
        let durable = self.audit.durable.lock().await;
        let start = after.map_or(0, |cursor| cursor.get() as usize);
        let entries: Vec<_> = durable.iter().skip(start).take(limit).cloned().collect();
        let next_after = entries
            .last()
            .and_then(|entry| (entry.seq() != self.head).then_some(entry.seq()));
        ConversationPage {
            entries,
            next_after,
            observed_head: self.head,
        }
    }
}

impl SessionLog for PagedLog {
    fn initialize<'a>(&'a mut self, _manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        Box::pin(async { Ok(ConversationSeq::ZERO) })
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        let manifest = self.manifest.clone();
        let mode = self.load_mode;
        let audit = Arc::clone(&self.audit);
        Box::pin(async move {
            match mode {
                OperationMode::Success => Ok(manifest),
                OperationMode::Known(kind) => Err(log_error(kind)),
                OperationMode::Timeout => {
                    let sender = audit.load_started_sender.lock().await.take().unwrap();
                    let _ = sender.send(());
                    std::future::pending::<Result<SessionManifest, SessionLogError>>().await
                }
                OperationMode::Panic => panic!("paged log manifest panic"),
            }
        })
    }

    fn read_page<'a>(
        &'a mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        self.audit.read_calls.fetch_add(1, Ordering::SeqCst);
        let audit = Arc::clone(&self.audit);
        let mode = if audit.force_read_unknown.load(Ordering::SeqCst) {
            OperationMode::Known(SessionLogErrorKind::UnknownOutcome)
        } else {
            self.read_mode
        };
        Box::pin(async move {
            match mode {
                OperationMode::Success => {}
                OperationMode::Known(kind) => return Err(log_error(kind)),
                OperationMode::Timeout => {
                    let sender = audit.read_started_sender.lock().await.take().unwrap();
                    let _ = sender.send(());
                    return std::future::pending::<Result<ConversationPage, SessionLogError>>()
                        .await;
                }
                OperationMode::Panic => panic!("paged log read panic"),
            }
            let scripted = { audit.pages.lock().await.pop_front() };
            if let Some(page) = scripted {
                self.head = page.observed_head;
                audit
                    .durable
                    .lock()
                    .await
                    .extend(page.entries.iter().cloned());
                Ok(page)
            } else {
                Ok(self.generated_page(after, limit).await)
            }
        })
    }

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        self.audit.append_calls.fetch_add(1, Ordering::SeqCst);
        let mode = self.append_mode;
        Box::pin(async move {
            match mode {
                AppendMode::Success => Ok(self.record_append(expected_head, &entries).await),
                AppendMode::Known(kind) => Err(log_error(kind)),
                AppendMode::Unknown => {
                    let _ = self.record_append(expected_head, &entries).await;
                    Err(log_error(SessionLogErrorKind::UnknownOutcome))
                }
                AppendMode::Timeout => {
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
                AppendMode::Panic => panic!("paged log append panic"),
                AppendMode::BadReceipt => {
                    let receipt = self.record_append(expected_head, &entries).await;
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
        let mode = self.close_mode;
        Box::pin(async move {
            match mode {
                CloseMode::Success => Ok(()),
                CloseMode::Known(kind) => Err(log_error(kind)),
                CloseMode::Timeout => {
                    let sender = self.audit.close_started_sender.lock().await.take().unwrap();
                    let _ = sender.send(());
                    std::future::pending::<Result<(), SessionLogError>>().await
                }
                CloseMode::Panic => panic!("paged log close panic"),
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
            BoundedText::new("paged log error").unwrap(),
            false,
        ),
    )
}

pub(super) fn timestamp() -> Result<Timestamp, TimestampError> {
    "2026-08-19T12:34:56.789Z".parse()
}

pub(super) fn kernel() -> KernelConfig {
    KernelConfig::default_checked().unwrap()
}

pub(super) fn kernel_with_output_limit(max: usize) -> KernelConfig {
    let mut value = kernel();
    value.limits.max_tool_output_bytes = max;
    value
}

pub(super) fn session_id() -> SessionId {
    "ses_00000000000000000000000000000021".parse().unwrap()
}

pub(super) fn turn_id(value: u8) -> TurnId {
    format!("trn_{value:032x}").parse().unwrap()
}

pub(super) fn spec() -> SessionSpec {
    SessionSpec::new(
        "model:v1".parse::<ModelRef>().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        ["read_file", "write_file"]
            .into_iter()
            .map(|name| name.parse().unwrap())
            .collect(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

pub(super) fn manifest() -> SessionManifest {
    SessionManifest {
        format_version: SessionManifest::FORMAT_VERSION,
        session_id: session_id(),
        created_at: timestamp().unwrap(),
        spec: spec(),
    }
}

pub(super) fn user(seq: u64, turn: TurnId) -> ConversationEntry {
    ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn,
        input: UserInputRecord::new(BoundedText::new("hello").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            "model:v1".parse().unwrap(),
            ReasoningPreference::Auto,
            4,
        )
        .unwrap(),
        created_at: timestamp().unwrap(),
    })
}

pub(super) fn assistant(seq: u64, turn: TurnId, calls: Vec<ToolCall>) -> ConversationEntry {
    let has_calls = !calls.is_empty();
    ConversationEntry::AssistantMessage(AssistantMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn,
        model: "model:v1".parse().unwrap(),
        text: if has_calls {
            None
        } else {
            Some(BoundedText::new("done").unwrap())
        },
        reasoning: None,
        tool_calls: calls,
        usage: Usage::default(),
        finish_reason: if has_calls {
            ModelFinishReason::ToolCalls
        } else {
            ModelFinishReason::Stop
        },
        created_at: timestamp().unwrap(),
    })
}

pub(super) fn tool_call(id: &str, index: u32, name: &str) -> ToolCall {
    ToolCall::new(
        id.parse::<ToolCallId>().unwrap(),
        name.parse::<ToolName>().unwrap(),
        json!({"path": "a"}),
        index,
    )
    .unwrap()
}

pub(super) fn result(seq: u64, turn: TurnId, id: &str, name: &str) -> ConversationEntry {
    ConversationEntry::ToolResult(ToolResultEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn,
        tool_call_id: id.parse().unwrap(),
        tool_name: name.parse().unwrap(),
        outcome: ToolResultOutcome::Success,
        content: BoundedText::new("ok").unwrap(),
        created_at: timestamp().unwrap(),
    })
}

pub(super) fn terminal(seq: u64, turn: TurnId) -> ConversationEntry {
    ConversationEntry::TurnTerminal(TurnTerminalEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn,
        terminal: TurnTerminal::Completed,
        usage: Usage::default(),
        created_at: timestamp().unwrap(),
    })
}

pub(super) fn page(
    entries: Vec<ConversationEntry>,
    next_after: Option<u64>,
    observed_head: u64,
) -> ConversationPage {
    ConversationPage {
        entries,
        next_after: next_after.map(ConversationSeq::new),
        observed_head: ConversationSeq::new(observed_head),
    }
}

pub(super) fn completed_pages() -> Vec<ConversationPage> {
    let turn = turn_id(1);
    vec![
        page(vec![user(1, turn)], Some(1), 3),
        page(vec![assistant(2, turn, Vec::new())], Some(2), 3),
        page(vec![terminal(3, turn)], None, 3),
    ]
}

pub(super) fn active_pages(calls: Vec<ToolCall>) -> Vec<ConversationPage> {
    let turn = turn_id(2);
    vec![
        page(vec![user(1, turn)], Some(1), 2),
        page(vec![assistant(2, turn, calls)], None, 2),
    ]
}

pub(super) async fn begin_with_modes(
    pages: Vec<ConversationPage>,
    append_mode: AppendMode,
    load_mode: OperationMode,
    read_mode: OperationMode,
    close_mode: CloseMode,
    kernel: KernelConfig,
) -> (
    Result<PendingConversationLoad, ConversationCommitError>,
    Arc<Audit>,
) {
    let audit = Audit::new(pages);
    let result = ConversationLog::begin_load(
        session_id(),
        Box::new(
            PagedLog::new(Arc::clone(&audit), manifest(), append_mode, close_mode)
                .with_load_mode(load_mode)
                .with_read_mode(read_mode),
        ),
        kernel,
        Box::new(timestamp),
    )
    .await;
    (result, audit)
}

pub(super) async fn begin_default(
    pages: Vec<ConversationPage>,
    append_mode: AppendMode,
) -> (
    Result<PendingConversationLoad, ConversationCommitError>,
    Arc<Audit>,
) {
    begin_with_modes(
        pages,
        append_mode,
        OperationMode::Success,
        OperationMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await
}

pub(super) async fn load_with(
    pages: Vec<ConversationPage>,
    append_mode: AppendMode,
) -> (Result<ConversationLog, ConversationCommitError>, Arc<Audit>) {
    load_with_modes(
        pages,
        append_mode,
        OperationMode::Success,
        OperationMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await
}

pub(super) async fn load_with_modes(
    pages: Vec<ConversationPage>,
    append_mode: AppendMode,
    load_mode: OperationMode,
    read_mode: OperationMode,
    close_mode: CloseMode,
    kernel: KernelConfig,
) -> (Result<ConversationLog, ConversationCommitError>, Arc<Audit>) {
    let (pending, audit) =
        begin_with_modes(pages, append_mode, load_mode, read_mode, close_mode, kernel).await;
    let result = match pending {
        Ok(pending) => {
            let proof = bindings_validated(&pending);
            pending.finish(proof).await
        }
        Err(error) => Err(error),
    };
    (result, audit)
}

pub(super) fn bindings_validated(pending: &PendingConversationLoad) -> LoadCompatibilityValidated {
    // Test-only stand-in for the P4 SessionBindings validation boundary.
    LoadCompatibilityValidated::after_session_bindings_validation(pending)
}

pub(super) async fn initialized_log(
    audit: Arc<Audit>,
    append_mode: AppendMode,
    close_mode: CloseMode,
    kernel: KernelConfig,
) -> ConversationLog {
    ConversationLog::initialize(
        Box::new(PagedLog::new(
            Arc::clone(&audit),
            manifest(),
            append_mode,
            close_mode,
        )),
        manifest(),
        kernel,
        Box::new(timestamp),
    )
    .await
    .unwrap()
}

pub(super) fn user_draft(turn: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::UserMessage(UserMessageDraft {
        turn_id: turn,
        input: UserInputRecord::new(BoundedText::new("hello").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            "model:v1".parse().unwrap(),
            ReasoningPreference::Auto,
            4,
        )
        .unwrap(),
    })
}

pub(super) fn final_draft(turn: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::AssistantMessage(AssistantMessageDraft {
        turn_id: turn,
        model: "model:v1".parse().unwrap(),
        text: Some(BoundedText::new("done").unwrap()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: Usage::default(),
        finish_reason: ModelFinishReason::Stop,
    })
}

pub(super) fn terminal_draft(turn: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::TurnTerminal(TurnTerminalDraft {
        turn_id: turn,
        terminal: TurnTerminal::Completed,
        usage: Usage::default(),
    })
}
