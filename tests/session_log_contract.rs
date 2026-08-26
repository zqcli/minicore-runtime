pub mod support;

use std::collections::BTreeSet;

use minicore_runtime::config::{CompactionConfig, SessionManifest, SessionSpec, Timestamp};
use minicore_runtime::conversation::{
    AssistantMessageEntry, ConversationEntry, ConversationSeq, SummaryEntry, ToolResultEntry,
    TurnExecutionRecord, TurnTerminal, TurnTerminalEntry, UserInputRecord, UserMessageEntry,
};
use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::ids::{SessionId, ToolCallId, TurnId};
use minicore_runtime::model::{ModelFinishReason, ModelRef, ReasoningPreference, ToolCall, Usage};
use minicore_runtime::storage::{
    AppendReceipt, ConversationPage, LogFuture, SessionLog, SessionLogError, SessionLogErrorKind,
};
use minicore_runtime::tools::{ToolName, ToolResultOutcome};
use minicore_runtime::value::BoundedText;
use minicore_runtime::{
    ConversationEntry as RootConversationEntry, ConversationSeq as RootConversationSeq,
    TurnTerminal as RootTurnTerminal,
};
use serde_json::json;

use support::fake_session_log::{FakeSessionLog, Operation, Script};

#[test]
fn conversation_log_does_not_use_shared_port_call_execution() {
    for path in [
        "../src/conversation/log.rs",
        "../src/conversation/session_log.rs",
        "../src/session/runtime_log.rs",
    ] {
        let source = match path {
            "../src/conversation/log.rs" => include_str!("../src/conversation/log.rs"),
            "../src/conversation/session_log.rs" => {
                include_str!("../src/conversation/session_log.rs")
            }
            "../src/session/runtime_log.rs" => include_str!("../src/session/runtime_log.rs"),
            _ => unreachable!(),
        };
        assert!(!source.contains("run_port_call"), "{path} uses port helper");
    }
}

fn timestamp() -> Timestamp {
    "2026-08-19T12:34:56.789Z".parse().unwrap()
}

fn spec() -> SessionSpec {
    SessionSpec::new(
        "model:v1".parse::<ModelRef>().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        64,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

fn manifest() -> SessionManifest {
    SessionManifest::new(SessionId::new().unwrap(), spec()).unwrap()
}

fn diagnostic(code: DiagnosticCode, message: &str) -> DiagnosticSummary {
    DiagnosticSummary::new(
        code,
        DiagnosticCategory::Storage,
        BoundedText::new(message).unwrap(),
        false,
    )
}

fn user_entry(seq: u64, turn_id: TurnId) -> ConversationEntry {
    ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id,
        input: UserInputRecord::new(BoundedText::new("hello").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            "model:v1".parse().unwrap(),
            ReasoningPreference::Auto,
            64,
        )
        .unwrap(),
        created_at: timestamp(),
    })
}

fn all_entries(turn_id: TurnId) -> Vec<ConversationEntry> {
    let tool_name = "read_file".parse::<ToolName>().unwrap();
    let call = ToolCall::new(
        ToolCallId::new("call-1").unwrap(),
        tool_name.clone(),
        json!({"path": "README.md"}),
        0,
    )
    .unwrap();
    vec![
        user_entry(1, turn_id),
        ConversationEntry::AssistantMessage(AssistantMessageEntry {
            seq: ConversationSeq::new(2),
            turn_id,
            model: "model:v1".parse().unwrap(),
            text: Some(BoundedText::new("answer").unwrap()),
            reasoning: None,
            tool_calls: vec![call],
            usage: Usage::new(1, 2, 0),
            finish_reason: ModelFinishReason::ToolCalls,
            created_at: timestamp(),
        }),
        ConversationEntry::ToolResult(ToolResultEntry {
            seq: ConversationSeq::new(3),
            turn_id,
            tool_call_id: ToolCallId::new("call-1").unwrap(),
            tool_name,
            outcome: ToolResultOutcome::Success,
            content: BoundedText::new("done").unwrap(),
            created_at: timestamp(),
        }),
        ConversationEntry::Summary(SummaryEntry {
            seq: ConversationSeq::new(4),
            through: ConversationSeq::new(3),
            summary: BoundedText::new("summary").unwrap(),
            created_at: timestamp(),
        }),
        ConversationEntry::TurnTerminal(TurnTerminalEntry {
            seq: ConversationSeq::new(5),
            turn_id,
            terminal: TurnTerminal::Failed {
                diagnostic: diagnostic(DiagnosticCode::LogCorrupt, "safe diagnostic"),
            },
            usage: Usage::default(),
            created_at: timestamp(),
        }),
    ]
}

fn initialize_signature<'a>(
    log: &'a mut dyn SessionLog,
    manifest: SessionManifest,
) -> LogFuture<'a, ConversationSeq> {
    log.initialize(manifest)
}

fn load_manifest_signature<'a>(log: &'a mut dyn SessionLog) -> LogFuture<'a, SessionManifest> {
    log.load_manifest()
}

fn read_page_signature<'a>(
    log: &'a mut dyn SessionLog,
    after: Option<ConversationSeq>,
    limit: usize,
) -> LogFuture<'a, ConversationPage> {
    log.read_page(after, limit)
}

fn append_signature<'a>(
    log: &'a mut dyn SessionLog,
    expected_head: ConversationSeq,
    entries: Vec<ConversationEntry>,
) -> LogFuture<'a, AppendReceipt> {
    log.append(expected_head, entries)
}

fn close_signature<'a>(log: &'a mut dyn SessionLog) -> LogFuture<'a, ()> {
    log.close()
}

#[test]
fn public_port_signatures_and_send_boundary_are_exact() {
    fn assert_send<T: Send>() {}

    assert_send::<FakeSessionLog>();
    assert_send::<Box<dyn SessionLog>>();
    let _ = initialize_signature;
    let _ = load_manifest_signature;
    let _ = read_page_signature;
    let _ = append_signature;
    let _ = close_signature;
}

#[tokio::test(flavor = "current_thread")]
async fn fake_session_log_enforces_head_pages_errors_close_and_operation_order() {
    let mut log = FakeSessionLog::new();
    let inspection = log.inspection();
    let stored_manifest = manifest();

    assert_eq!(
        log.initialize(stored_manifest.clone()).await.unwrap(),
        ConversationSeq::ZERO
    );
    assert_eq!(
        log.initialize(stored_manifest.clone())
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::AlreadyInitialized
    );
    assert_eq!(log.load_manifest().await.unwrap(), stored_manifest);

    let turn_id = TurnId::new().unwrap();
    let mut entries = all_entries(turn_id);
    let first_batch: Vec<_> = entries.drain(..2).collect();
    let expected_first_batch = first_batch.clone();
    let receipt = log
        .append(ConversationSeq::ZERO, first_batch)
        .await
        .unwrap();
    assert_eq!(
        receipt,
        AppendReceipt {
            previous_head: ConversationSeq::ZERO,
            new_head: ConversationSeq::new(2),
            appended: 2,
        }
    );

    let first_page = log.read_page(None, 1).await.unwrap();
    assert_eq!(first_page.entries.len(), 1);
    assert_eq!(first_page.next_after, Some(ConversationSeq::new(1)));
    assert_eq!(first_page.observed_head, ConversationSeq::new(2));
    let second_page = log.read_page(first_page.next_after, 10).await.unwrap();
    assert_eq!(second_page.entries.len(), 1);
    assert_eq!(second_page.next_after, None);
    assert_eq!(second_page.observed_head, ConversationSeq::new(2));

    let conflict = log
        .append(ConversationSeq::ZERO, vec![user_entry(3, turn_id)])
        .await
        .unwrap_err();
    assert_eq!(conflict.kind(), SessionLogErrorKind::Conflict);

    log.script_read(Script::Error(SessionLogErrorKind::Corrupt));
    assert_eq!(
        log.read_page(None, 1).await.unwrap_err().kind(),
        SessionLogErrorKind::Corrupt
    );
    log.close().await.unwrap();

    assert_eq!(
        inspection.operations(),
        vec![
            Operation::Initialize,
            Operation::Initialize,
            Operation::LoadManifest,
            Operation::Append {
                expected_head: ConversationSeq::ZERO,
                entries: expected_first_batch.clone(),
            },
            Operation::ReadPage {
                after: None,
                limit: 1,
            },
            Operation::ReadPage {
                after: Some(ConversationSeq::new(1)),
                limit: 10,
            },
            Operation::Append {
                expected_head: ConversationSeq::ZERO,
                entries: vec![user_entry(3, turn_id)],
            },
            Operation::ReadPage {
                after: None,
                limit: 1,
            },
            Operation::Close,
        ]
    );
    assert_eq!(inspection.max_concurrent_mutable_operations(), 1);
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.entries(), expected_first_batch);
    assert_eq!(inspection.manifest(), Some(stored_manifest));

    let mut closed = FakeSessionLog::new();
    closed.initialize(manifest()).await.unwrap();
    closed.close().await.unwrap();
    assert_eq!(
        closed.load_manifest().await.unwrap_err().kind(),
        SessionLogErrorKind::Closed
    );
    assert_eq!(
        closed.close().await.unwrap_err().kind(),
        SessionLogErrorKind::Closed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fake_session_log_scripts_cover_initialize_load_and_corruption() {
    let stored_manifest = manifest();
    let mut scripted_initialize = FakeSessionLog::new();
    let initialize_inspection = scripted_initialize.inspection();
    scripted_initialize.script_initialize(Script::Error(SessionLogErrorKind::Unavailable));
    assert_eq!(
        scripted_initialize
            .initialize(stored_manifest.clone())
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::Unavailable
    );
    assert_eq!(initialize_inspection.manifest(), None);
    assert_eq!(initialize_inspection.entries(), Vec::new());
    assert_eq!(
        scripted_initialize
            .initialize(stored_manifest.clone())
            .await
            .unwrap(),
        ConversationSeq::ZERO
    );
    assert_eq!(
        initialize_inspection.manifest(),
        Some(stored_manifest.clone())
    );

    let mut scripted_load = FakeSessionLog::new();
    scripted_load
        .initialize(stored_manifest.clone())
        .await
        .unwrap();
    scripted_load.script_load_manifest(Script::Error(SessionLogErrorKind::Unavailable));
    assert_eq!(
        scripted_load.load_manifest().await.unwrap_err().kind(),
        SessionLogErrorKind::Unavailable
    );
    scripted_load.script_load_manifest(Script::Delay(std::time::Duration::ZERO));
    assert_eq!(
        scripted_load.load_manifest().await.unwrap(),
        stored_manifest
    );

    let turn_id = TurnId::new().unwrap();
    let initial: Vec<_> = all_entries(turn_id).into_iter().take(2).collect();
    let mut corrupt = FakeSessionLog::with_initial(stored_manifest, initial.clone()).unwrap();
    let corrupt_inspection = corrupt.inspection();
    corrupt.mark_corrupt();
    assert_eq!(
        corrupt.load_manifest().await.unwrap_err().kind(),
        SessionLogErrorKind::Corrupt
    );
    assert_eq!(
        corrupt.read_page(None, 1).await.unwrap_err().kind(),
        SessionLogErrorKind::Corrupt
    );
    assert_eq!(
        corrupt
            .append(ConversationSeq::new(2), vec![user_entry(3, turn_id)])
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::Corrupt
    );
    assert_eq!(corrupt_inspection.entries(), initial);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fake_session_log_scripts_are_popped_before_async_work() {
    let turn_id = TurnId::new().unwrap();
    let initial: Vec<_> = all_entries(turn_id).into_iter().take(2).collect();
    let mut log = FakeSessionLog::with_initial(manifest(), initial.clone()).unwrap();
    let inspection = log.inspection();
    log.script_append(Script::Delay(std::time::Duration::from_secs(5)));
    let next = user_entry(3, turn_id);
    let task = tokio::spawn(async move { log.append(ConversationSeq::new(2), vec![next]).await });
    tokio::task::yield_now().await;
    assert_eq!(inspection.operations().len(), 1);
    assert_eq!(inspection.max_concurrent_mutable_operations(), 1);
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    assert_eq!(
        task.await.unwrap().unwrap().new_head,
        ConversationSeq::new(3)
    );
    assert_eq!(inspection.entries().len(), 3);
    assert_eq!(inspection.head(), ConversationSeq::new(3));
}

#[tokio::test(flavor = "current_thread")]
async fn fake_session_log_scripts_cover_atomic_failures_unknown_outcomes_and_panic() {
    let turn_id = TurnId::new().unwrap();
    let initial: Vec<_> = all_entries(turn_id).into_iter().take(2).collect();
    let next = user_entry(3, turn_id);

    let mut known_failure = FakeSessionLog::with_initial(manifest(), initial.clone()).unwrap();
    known_failure.script_append(Script::Error(SessionLogErrorKind::Unavailable));
    assert_eq!(
        known_failure
            .append(ConversationSeq::new(2), vec![next.clone()])
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::Unavailable
    );
    assert_eq!(known_failure.inspection().entries(), initial);

    let mut unknown_not_committed =
        FakeSessionLog::with_initial(manifest(), known_failure.inspection().entries()).unwrap();
    unknown_not_committed.script_append(Script::UnknownOutcome { committed: false });
    assert_eq!(
        unknown_not_committed
            .append(ConversationSeq::new(2), vec![next.clone()])
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::UnknownOutcome
    );
    assert_eq!(unknown_not_committed.inspection().entries(), initial);

    let mut unknown_committed = FakeSessionLog::with_initial(manifest(), initial.clone()).unwrap();
    unknown_committed.script_append(Script::UnknownOutcome { committed: true });
    assert_eq!(
        unknown_committed
            .append(ConversationSeq::new(2), vec![next.clone()])
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::UnknownOutcome
    );
    assert_eq!(unknown_committed.inspection().entries().len(), 3);

    let mut invalid_batches = FakeSessionLog::with_initial(manifest(), initial.clone()).unwrap();
    assert_eq!(
        invalid_batches
            .append(ConversationSeq::new(2), Vec::new())
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::Internal
    );
    assert_eq!(
        invalid_batches
            .append(ConversationSeq::new(2), vec![user_entry(4, turn_id)])
            .await
            .unwrap_err()
            .kind(),
        SessionLogErrorKind::Conflict
    );
    assert_eq!(invalid_batches.inspection().entries(), initial);

    let mut panic_log = FakeSessionLog::with_initial(manifest(), initial.clone()).unwrap();
    let panic_inspection = panic_log.inspection();
    panic_log.script_append(Script::Panic);
    let panic_task =
        tokio::spawn(async move { panic_log.append(ConversationSeq::new(2), vec![next]).await });
    assert!(matches!(
        panic_task.await,
        Err(ref error) if error.is_panic()
    ));
    assert_eq!(panic_inspection.entries(), initial);
    assert_eq!(panic_inspection.max_concurrent_mutable_operations(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn fake_session_log_initial_state_read_corruption_and_close_failure_are_observable() {
    let turn_id = TurnId::new().unwrap();
    assert!(FakeSessionLog::with_initial(manifest(), vec![user_entry(2, turn_id)]).is_err());
    let initial: Vec<_> = all_entries(turn_id).into_iter().take(3).collect();
    let stored_manifest = manifest();
    let mut log = FakeSessionLog::with_initial(stored_manifest.clone(), initial.clone()).unwrap();
    let inspection = log.inspection();
    assert_eq!(inspection.manifest(), Some(stored_manifest));
    assert_eq!(inspection.entries(), initial);
    assert_eq!(inspection.head(), ConversationSeq::new(3));

    log.script_read(Script::Error(SessionLogErrorKind::Corrupt));
    assert_eq!(
        log.read_page(None, 2).await.unwrap_err().kind(),
        SessionLogErrorKind::Corrupt
    );
    log.script_close(Script::Error(SessionLogErrorKind::Unavailable));
    assert_eq!(
        log.close().await.unwrap_err().kind(),
        SessionLogErrorKind::Unavailable
    );
    assert_eq!(inspection.close_count(), 1);
    log.close().await.unwrap();
    assert_eq!(inspection.close_count(), 2);
}

#[test]
fn conversation_dtos_are_exact_checked_and_roundtrip() {
    assert_eq!(ConversationSeq::ZERO.get(), 0);
    assert_eq!(ConversationSeq::ZERO.next(), Some(ConversationSeq::new(1)));
    assert_eq!(ConversationSeq::new(42).get(), 42);

    let turn_id = TurnId::new().unwrap();
    let entries = all_entries(turn_id);
    assert_eq!(entries.len(), 5);
    for entry in &entries {
        let encoded = serde_json::to_value(entry).unwrap();
        let decoded: ConversationEntry = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(&decoded, entry);
        assert_eq!(decoded.seq(), entry.seq());
    }
    assert!(entries[0].turn_id().is_some());
    assert!(entries[3].turn_id().is_none());

    let timestamp = timestamp();
    assert_eq!(
        serde_json::from_value::<Timestamp>(serde_json::to_value(&timestamp).unwrap()).unwrap(),
        timestamp
    );
    let summary = diagnostic(DiagnosticCode::ModelTimeout, "model timed out");
    assert_eq!(
        serde_json::from_value::<DiagnosticSummary>(serde_json::to_value(&summary).unwrap())
            .unwrap(),
        summary
    );
    for outcome in [
        ToolResultOutcome::Success,
        ToolResultOutcome::Failed,
        ToolResultOutcome::Denied,
        ToolResultOutcome::Cancelled,
        ToolResultOutcome::InputProvided,
    ] {
        assert_eq!(
            serde_json::from_value::<ToolResultOutcome>(serde_json::to_value(outcome).unwrap())
                .unwrap(),
            outcome
        );
    }
}

#[test]
fn conversation_nested_deserializers_reject_unknown_and_invalid_values() {
    let entries = all_entries(TurnId::new().unwrap());
    let summary = diagnostic(DiagnosticCode::ModelTimeout, "model timed out");

    for entry in &entries {
        let mut unknown = serde_json::to_value(entry).unwrap();
        let payload = unknown
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap()
            .as_object_mut()
            .unwrap();
        payload.insert("unknown_entry_field".to_owned(), json!(true));
        assert!(serde_json::from_value::<ConversationEntry>(unknown).is_err());
    }

    let mut empty_input = serde_json::to_value(&entries[0]).unwrap();
    let input = empty_input
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("input")
        .unwrap()
        .as_object_mut()
        .unwrap();
    input.insert("text".to_owned(), json!(""));
    assert!(serde_json::from_value::<ConversationEntry>(empty_input).is_err());
    assert!(UserInputRecord::new(BoundedText::new("").unwrap()).is_err());

    let mut unknown_input = serde_json::to_value(&entries[0]).unwrap();
    unknown_input
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("input")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("unknown_input_field".to_owned(), json!(true));
    assert!(serde_json::from_value::<ConversationEntry>(unknown_input).is_err());

    let mut zero_rounds = serde_json::to_value(&entries[0]).unwrap();
    let execution = zero_rounds
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("execution")
        .unwrap()
        .as_object_mut()
        .unwrap();
    execution.insert("max_tool_rounds".to_owned(), json!(0));
    assert!(serde_json::from_value::<ConversationEntry>(zero_rounds).is_err());
    assert!(
        TurnExecutionRecord::new("model:v1".parse().unwrap(), ReasoningPreference::Auto, 0)
            .is_err()
    );

    let mut unknown_execution = serde_json::to_value(&entries[0]).unwrap();
    unknown_execution
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("execution")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("unknown_execution_field".to_owned(), json!(true));
    assert!(serde_json::from_value::<ConversationEntry>(unknown_execution).is_err());

    let mut unknown_tool_call = serde_json::to_value(&entries[1]).unwrap();
    let tool_call = unknown_tool_call
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("tool_calls")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .first_mut()
        .unwrap()
        .as_object_mut()
        .unwrap();
    tool_call.insert("unknown_tool_field".to_owned(), json!(true));
    assert!(serde_json::from_value::<ConversationEntry>(unknown_tool_call).is_err());

    let mut unknown_usage = serde_json::to_value(&entries[1]).unwrap();
    let usage = unknown_usage
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("usage")
        .unwrap()
        .as_object_mut()
        .unwrap();
    usage.insert("unknown_usage_field".to_owned(), json!(true));
    assert!(serde_json::from_value::<ConversationEntry>(unknown_usage).is_err());

    let mut unknown_failed_payload = serde_json::to_value(&entries[4]).unwrap();
    let failed = unknown_failed_payload
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("terminal")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("failed")
        .unwrap()
        .as_object_mut()
        .unwrap();
    failed.insert("unknown_failure_field".to_owned(), json!(true));
    assert!(serde_json::from_value::<ConversationEntry>(unknown_failed_payload).is_err());
    assert!(serde_json::from_value::<TurnTerminal>(json!({"completed": {}})).is_err());

    let mut unknown_diagnostic = serde_json::to_value(&summary).unwrap();
    unknown_diagnostic
        .as_object_mut()
        .unwrap()
        .insert("raw_source".to_owned(), json!("secret/path"));
    assert!(serde_json::from_value::<DiagnosticSummary>(unknown_diagnostic).is_err());
    assert!(
        serde_json::from_value::<ConversationEntry>(json!({
            "interaction": {}
        }))
        .is_err()
    );

    let debug = format!("{:?}", entries[1]);
    assert!(debug.contains("call-1"));
    assert!(debug.contains("read_file"));
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("README.md"));
    assert!(!debug.contains("path"));

    let lib = include_str!("../src/lib.rs");
    assert!(lib.contains("pub mod conversation;"));
    assert!(lib.contains("pub mod storage;"));
    let _: Option<RootConversationEntry> = None;
    let _: Option<RootConversationSeq> = None;
    let _: Option<RootTurnTerminal> = None;
}

#[test]
fn session_log_error_never_displays_raw_diagnostic_message() {
    let raw = "postgres://user:secret@host/private/path";
    let error = SessionLogError::new(
        SessionLogErrorKind::Internal,
        diagnostic(DiagnosticCode::Internal, raw),
    );
    assert_eq!(error.kind(), SessionLogErrorKind::Internal);
    assert_eq!(error.diagnostic().message.as_str(), raw);
    assert!(!format!("{error:?}").contains(raw));
    assert!(!format!("{error}").contains(raw));
}
