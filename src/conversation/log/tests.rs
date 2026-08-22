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

struct Audit {
    append_calls: AtomicUsize,
    durable_entries: AtomicUsize,
    timestamp_calls: AtomicUsize,
    started_sender: Mutex<Option<oneshot::Sender<()>>>,
    started_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    release_sender: Mutex<Option<oneshot::Sender<()>>>,
    release_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    done_sender: Mutex<Option<oneshot::Sender<()>>>,
    done_receiver: Mutex<Option<oneshot::Receiver<()>>>,
    adapter_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Audit {
    fn new() -> Self {
        let (started_sender, started_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        let (done_sender, done_receiver) = oneshot::channel();
        Self {
            append_calls: AtomicUsize::new(0),
            durable_entries: AtomicUsize::new(0),
            timestamp_calls: AtomicUsize::new(0),
            started_sender: Mutex::new(Some(started_sender)),
            started_receiver: Mutex::new(Some(started_receiver)),
            release_sender: Mutex::new(Some(release_sender)),
            release_receiver: Mutex::new(Some(release_receiver)),
            done_sender: Mutex::new(Some(done_sender)),
            done_receiver: Mutex::new(Some(done_receiver)),
            adapter_task: Mutex::new(None),
        }
    }
}

impl Default for Audit {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
enum InitializeScript {
    Normal,
    NonZero,
    Error(SessionLogErrorKind),
    Delay,
    Panic,
}

#[derive(Clone, Copy)]
enum AppendScript {
    Normal,
    Error(SessionLogErrorKind),
    ErrorOnce(SessionLogErrorKind),
    UnknownOutcome,
    Delay,
    Panic,
    BadReceipt,
    LateCommitAfterTimeout,
}

struct LocalLog {
    audit: Arc<Audit>,
    initialize_script: InitializeScript,
    append_script: AppendScript,
    append_error_consumed: bool,
    initialized: bool,
    closed: bool,
    manifest: Option<SessionManifest>,
    head: ConversationSeq,
    entries: Vec<ConversationEntry>,
}

impl LocalLog {
    fn new(
        audit: Arc<Audit>,
        initialize_script: InitializeScript,
        append_script: AppendScript,
    ) -> Self {
        Self {
            audit,
            initialize_script,
            append_script,
            append_error_consumed: false,
            initialized: false,
            closed: false,
            manifest: None,
            head: ConversationSeq::ZERO,
            entries: Vec::new(),
        }
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
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    self.initialized = true;
                    self.manifest = Some(manifest);
                    Ok(ConversationSeq::ZERO)
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
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Ok(self.record_append(expected_head, &entries))
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
        Box::pin(async move {
            if self.closed {
                return Err(log_error(SessionLogErrorKind::Closed));
            }
            self.closed = true;
            Ok(())
        })
    }
}

fn log_error(kind: SessionLogErrorKind) -> SessionLogError {
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

fn timestamp() -> Result<Timestamp, TimestampError> {
    "2026-08-19T12:34:56.789Z".parse()
}

fn failing_timestamp() -> Result<Timestamp, TimestampError> {
    Err(TimestampError::Invalid)
}

fn timestamp_source(audit: Arc<Audit>) -> TimestampSource {
    Box::new(move || {
        audit.timestamp_calls.fetch_add(1, Ordering::SeqCst);
        timestamp()
    })
}

fn spec() -> SessionSpec {
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

fn manifest() -> SessionManifest {
    SessionManifest {
        format_version: SessionManifest::FORMAT_VERSION,
        session_id: "ses_00000000000000000000000000000001".parse().unwrap(),
        created_at: timestamp().unwrap(),
        spec: spec(),
    }
}

fn kernel(timeout: Duration) -> KernelConfig {
    let mut kernel = KernelConfig::default_checked().unwrap();
    kernel.log_operation_timeout = timeout;
    kernel
}

async fn open(
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

fn user(turn_id: TurnId) -> UnsequencedEntry {
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

fn tool_call(id: &str, index: u32) -> ToolCall {
    ToolCall::new(
        id.parse::<ToolCallId>().unwrap(),
        "read_file".parse::<ToolName>().unwrap(),
        json!({"path": "a"}),
        index,
    )
    .unwrap()
}

fn assistant_with_tool(turn_id: TurnId) -> UnsequencedEntry {
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

fn assistant_final(turn_id: TurnId, finish_reason: ModelFinishReason) -> UnsequencedEntry {
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

fn tool_result(turn_id: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::ToolResult(ToolResultDraft {
        turn_id,
        tool_call_id: "call-a".parse().unwrap(),
        tool_name: "read_file".parse().unwrap(),
        outcome: ToolResultOutcome::Success,
        content: BoundedText::new("ok").unwrap(),
    })
}

fn summary() -> UnsequencedEntry {
    UnsequencedEntry::Summary(SummaryDraft {
        through: ConversationSeq::new(5),
        summary: BoundedText::new("summary").unwrap(),
    })
}

fn terminal(turn_id: TurnId) -> UnsequencedEntry {
    UnsequencedEntry::TurnTerminal(TurnTerminalDraft {
        turn_id,
        terminal: TurnTerminal::Completed,
        usage: Usage::default(),
    })
}

fn valid_batch(turn_id: TurnId) -> Vec<UnsequencedEntry> {
    vec![
        user(turn_id),
        assistant_with_tool(turn_id),
        tool_result(turn_id),
        assistant_final(turn_id, ModelFinishReason::Stop),
        terminal(turn_id),
        summary(),
    ]
}

fn assert_empty_confirmed(log: &ConversationLog) {
    assert_eq!(log.head(), ConversationSeq::ZERO);
    assert!(log.projection().entries().is_empty());
}

#[tokio::test]
async fn initialize_accepts_only_zero_and_maps_failures_safely() {
    let (log, _) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(log.head(), ConversationSeq::ZERO);
    assert!(log.projection().entries().is_empty());

    let mut invalid_kernel = kernel(Duration::from_secs(1));
    invalid_kernel.log_operation_timeout = Duration::ZERO;
    let error = ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::new(Audit::default()),
            InitializeScript::Normal,
            AppendScript::Normal,
        )),
        manifest(),
        invalid_kernel,
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::InvalidConfiguration
    );

    let mut invalid_manifest = manifest();
    invalid_manifest.format_version = 2;
    let error = ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::new(Audit::default()),
            InitializeScript::Normal,
            AppendScript::Normal,
        )),
        invalid_manifest,
        kernel(Duration::from_secs(1)),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::InvalidManifest);

    let error = ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::new(Audit::default()),
            InitializeScript::NonZero,
            AppendScript::Normal,
        )),
        manifest(),
        kernel(Duration::from_secs(1)),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::ContractViolation);

    let error = ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::new(Audit::default()),
            InitializeScript::Error(SessionLogErrorKind::Unavailable),
            AppendScript::Normal,
        )),
        manifest(),
        kernel(Duration::from_secs(1)),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
    );

    let error = ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::new(Audit::default()),
            InitializeScript::Error(SessionLogErrorKind::UnknownOutcome),
            AppendScript::Normal,
        )),
        manifest(),
        kernel(Duration::from_secs(1)),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
}

#[tokio::test]
async fn initialize_timeout_and_panic_are_durability_unknown() {
    for script in [InitializeScript::Delay, InitializeScript::Panic] {
        let error = ConversationLog::initialize(
            Box::new(LocalLog::new(
                Arc::new(Audit::default()),
                script,
                AppendScript::Normal,
            )),
            manifest(),
            kernel(Duration::from_millis(1)),
            Box::new(timestamp),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    }
}

#[tokio::test]
async fn append_assigns_ordered_seq_and_timestamp_and_updates_projection_after_durable_append() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    let turn_id = "trn_00000000000000000000000000000001".parse().unwrap();
    let batch = log.append_validated(valid_batch(turn_id)).await.unwrap();
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 6);
    assert_eq!(audit.timestamp_calls.load(Ordering::SeqCst), 6);
    assert_eq!(batch.entries.len(), 6);
    assert_eq!(batch.head, ConversationSeq::new(6));
    assert_eq!(batch.projection.entries(), batch.entries.as_slice());
    assert!(batch.projection.latest_summary().is_some());
    assert_eq!(
        batch.projection.latest_summary_through(),
        Some(ConversationSeq::new(5))
    );
    for (index, entry) in batch.entries.iter().enumerate() {
        assert_eq!(entry.seq(), ConversationSeq::new((index + 1) as u64));
        let created_at = match entry {
            ConversationEntry::UserMessage(value) => &value.created_at,
            ConversationEntry::AssistantMessage(value) => &value.created_at,
            ConversationEntry::ToolResult(value) => &value.created_at,
            ConversationEntry::Summary(value) => &value.created_at,
            ConversationEntry::TurnTerminal(value) => &value.created_at,
        };
        assert_eq!(created_at, &timestamp().unwrap());
    }
    assert_eq!(log.head(), ConversationSeq::new(6));
    assert_eq!(log.projection().entries(), batch.entries.as_slice());
}

#[tokio::test]
async fn validation_and_timestamp_failures_do_not_call_or_commit_log() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    let turn_id = "trn_00000000000000000000000000000002".parse().unwrap();
    let error = log
        .append_validated(vec![assistant_final(turn_id, ModelFinishReason::Stop)])
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::Validation);
    assert_eq!(
        error.validation_error(),
        Some(ConversationValidationError::MissingActiveTurn)
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);
    assert_empty_confirmed(&log);

    log.timestamp_source = Box::new(failing_timestamp);
    let error = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::Timestamp);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);
    assert_empty_confirmed(&log);
}

#[tokio::test]
async fn known_failure_and_validation_failure_leave_confirmed_state_unchanged() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Error(SessionLogErrorKind::Conflict),
        Duration::from_secs(1),
    )
    .await;
    let turn_id = "trn_00000000000000000000000000000003".parse().unwrap();
    let error = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Conflict)
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 0);
    assert_empty_confirmed(&log);
}

#[tokio::test]
async fn known_error_does_not_latch_and_next_append_can_commit() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::ErrorOnce(SessionLogErrorKind::Unavailable),
        Duration::from_secs(1),
    )
    .await;
    let turn_id = "trn_00000000000000000000000000000008".parse().unwrap();
    let first = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(
        first.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
    );
    assert_empty_confirmed(&log);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);

    let committed = log.append_validated(vec![user(turn_id)]).await.unwrap();
    assert_eq!(committed.head, ConversationSeq::new(1));
    assert_eq!(log.head(), ConversationSeq::new(1));
    assert_eq!(log.projection().entries().len(), 1);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 2);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_outcome_timeout_panic_and_bad_receipt_never_commit_memory() {
    let scripts = [
        AppendScript::UnknownOutcome,
        AppendScript::Panic,
        AppendScript::BadReceipt,
    ];
    for script in scripts {
        let (mut log, audit) = open(InitializeScript::Normal, script, Duration::from_secs(1)).await;
        let turn_id = "trn_00000000000000000000000000000004".parse().unwrap();
        let error = log
            .append_validated(vec![user(turn_id)])
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
        assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
        let durable_after_first = audit.durable_entries.load(Ordering::SeqCst);
        let timestamps_after_first = audit.timestamp_calls.load(Ordering::SeqCst);
        assert_empty_confirmed(&log);
        let second = log
            .append_validated(vec![user(turn_id)])
            .await
            .err()
            .unwrap();
        assert_eq!(
            second.kind(),
            ConversationCommitErrorKind::DurabilityUnknown
        );
        assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            audit.durable_entries.load(Ordering::SeqCst),
            durable_after_first
        );
        assert_eq!(
            audit.timestamp_calls.load(Ordering::SeqCst),
            timestamps_after_first
        );
        assert_empty_confirmed(&log);
    }
}

#[tokio::test]
async fn cooperative_delay_timeout_latches_without_durable_commit() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Delay,
        Duration::from_millis(1),
    )
    .await;
    let turn_id = "trn_00000000000000000000000000000005".parse().unwrap();
    let error = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 0);
    let timestamps_after_first = audit.timestamp_calls.load(Ordering::SeqCst);
    assert_empty_confirmed(&log);
    let second = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(
        second.kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 0);
    assert_eq!(
        audit.timestamp_calls.load(Ordering::SeqCst),
        timestamps_after_first
    );
    assert_empty_confirmed(&log);
}

#[tokio::test(start_paused = true)]
async fn late_adapter_commit_after_timeout_does_not_reopen_core_state() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::LateCommitAfterTimeout,
        Duration::from_millis(1),
    )
    .await;
    let turn_id = "trn_00000000000000000000000000000007".parse().unwrap();
    let started = audit.started_receiver.lock().await.take().unwrap();
    let mut append = Box::pin(log.append_validated(vec![user(turn_id)]));
    tokio::select! {
        _ = started => {}
        result = &mut append => {
            panic!("append completed before adapter task started: {result:?}")
        }
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = append.await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 0);
    let timestamps_after_first = audit.timestamp_calls.load(Ordering::SeqCst);
    assert_empty_confirmed(&log);

    let done = audit.done_receiver.lock().await.take().unwrap();
    let release = audit.release_sender.lock().await.take().unwrap();
    release.send(()).unwrap();
    done.await.unwrap();
    let handle = audit.adapter_task.lock().await.take().unwrap();
    handle.await.unwrap();
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 1);
    let second = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(
        second.kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 1);
    assert_eq!(
        audit.timestamp_calls.load(Ordering::SeqCst),
        timestamps_after_first
    );
    assert_empty_confirmed(&log);
}

#[tokio::test]
async fn empty_closed_and_sequence_overflow_are_rejected_before_append() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    let error = log.append_validated(Vec::new()).await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::EmptyBatch);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);

    log.closed = true;
    let turn_id = "trn_00000000000000000000000000000006".parse().unwrap();
    let error = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::Closed);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);

    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    log.state.set_head_for_test(ConversationSeq::new(u64::MAX));
    let error = log
        .append_validated(vec![user(turn_id)])
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::SequenceOverflow);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn draft_types_are_unsequenced_and_do_not_contain_timestamp_fields() {
    let source = include_str!("../log.rs");
    for declaration in [
        "pub(crate) struct UserMessageDraft",
        "pub(crate) struct AssistantMessageDraft",
        "pub(crate) struct ToolResultDraft",
        "pub(crate) struct SummaryDraft",
        "pub(crate) struct TurnTerminalDraft",
        "pub(crate) enum UnsequencedEntry",
    ] {
        assert!(
            source.contains(declaration),
            "missing draft declaration: {declaration}"
        );
    }
    let drafts = source
        .split_once("pub(crate) struct UserMessageDraft")
        .and_then(|(_, rest)| rest.split_once("pub(crate) enum UnsequencedEntry"))
        .map(|(drafts, _)| drafts)
        .unwrap();
    assert!(!drafts.contains("pub(crate) seq"));
    assert!(!drafts.contains("pub(crate) created_at"));
}
