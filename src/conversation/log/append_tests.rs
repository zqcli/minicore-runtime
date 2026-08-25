use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::append_support::*;
use super::*;
use crate::model::ModelFinishReason;
use crate::storage::SessionLogErrorKind;

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
    assert_eq!(
        error.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::Unavailable
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
    assert_eq!(
        error.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::UnknownOutcome
    );
}

#[tokio::test(start_paused = true)]
async fn initialize_timeout_and_panic_are_durability_unknown() {
    let audit = Arc::new(Audit::default());
    let started = audit.take_initialize_started().await;
    let mut initialize = Box::pin(ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::clone(&audit),
            InitializeScript::Delay,
            AppendScript::Normal,
        )),
        manifest(),
        kernel(Duration::from_millis(1)),
        Box::new(timestamp),
    ));
    tokio::select! {
        _ = started => {}
        _ = &mut initialize => panic!("initialize completed before adapter started"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        initialize.await.err().unwrap().kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );

    let error = ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::new(Audit::default()),
            InitializeScript::Panic,
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

#[tokio::test]
async fn initialize_failures_close_once_and_preserve_primary_and_secondary() {
    let mut invalid_kernel = kernel(Duration::from_millis(1));
    invalid_kernel.log_operation_timeout = Duration::ZERO;
    let (result, audit) = initialize_with_kernel(
        manifest(),
        InitializeScript::Normal,
        CloseScript::Error(SessionLogErrorKind::Conflict),
        invalid_kernel,
    )
    .await;
    let error = result.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::InvalidConfiguration
    );
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Conflict)
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let mut invalid_manifest = manifest();
    invalid_manifest.format_version = 2;
    let (result, audit) = initialize_with_kernel(
        invalid_manifest,
        InitializeScript::Normal,
        CloseScript::Success,
        kernel(Duration::from_millis(1)),
    )
    .await;
    assert_eq!(
        result.err().unwrap().kind(),
        ConversationCommitErrorKind::InvalidManifest
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let (result, audit) = initialize_with_kernel(
        manifest(),
        InitializeScript::Error(SessionLogErrorKind::Unavailable),
        CloseScript::Error(SessionLogErrorKind::Conflict),
        kernel(Duration::from_millis(1)),
    )
    .await;
    let error = result.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
    );
    assert_eq!(
        error.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::Unavailable
    );
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Conflict)
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let (result, audit) = initialize_with_kernel(
        manifest(),
        InitializeScript::Error(SessionLogErrorKind::Unavailable),
        CloseScript::Panic,
        kernel(Duration::from_millis(1)),
    )
    .await;
    let error = result.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
    );
    assert_eq!(
        error.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::Unavailable
    );
    assert!(matches!(
        error.secondary_close_outcome(),
        Some(ConversationCloseOutcome::Panic)
    ));
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Internal)
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    for script in [
        InitializeScript::Error(SessionLogErrorKind::UnknownOutcome),
        InitializeScript::Panic,
    ] {
        let (result, audit) = initialize_with_kernel(
            manifest(),
            script,
            CloseScript::Success,
            kernel(Duration::from_millis(1)),
        )
        .await;
        let error = result.err().unwrap();
        assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
        assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn initialize_timeout_and_nonzero_head_close_once_with_timeout_secondary() {
    let audit = Arc::new(Audit::default());
    let started = audit.take_initialize_started().await;
    let mut initialize = Box::pin(ConversationLog::initialize(
        Box::new(LocalLog::new(
            Arc::clone(&audit),
            InitializeScript::Delay,
            AppendScript::Normal,
        )),
        manifest(),
        kernel(Duration::from_millis(1)),
        timestamp_source(Arc::clone(&audit)),
    ));
    tokio::select! {
        _ = started => {}
        _ = &mut initialize => panic!("initialize completed before starting"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = initialize.await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let audit = Arc::new(Audit::default());
    let close_started = audit.take_close_started().await;
    let mut initialize = Box::pin(ConversationLog::initialize(
        Box::new(
            LocalLog::new(
                Arc::clone(&audit),
                InitializeScript::NonZero,
                AppendScript::Normal,
            )
            .with_close_script(CloseScript::Timeout),
        ),
        manifest(),
        kernel(Duration::from_millis(1)),
        timestamp_source(Arc::clone(&audit)),
    ));
    tokio::select! {
        _ = close_started => {}
        _ = &mut initialize => panic!("initialize completed before close started"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = initialize.await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::ContractViolation);
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::UnknownOutcome)
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn successful_initialize_does_not_close() {
    let (log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(log.head(), ConversationSeq::ZERO);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 0);
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
    }
    assert_eq!(log.head(), ConversationSeq::new(6));
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
    assert_eq!(
        error.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::Conflict
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
    assert_eq!(
        first.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::Unavailable
    );
    let committed = log.append_validated(vec![user(turn_id)]).await.unwrap();
    assert_eq!(committed.head, ConversationSeq::new(1));
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unknown_outcome_timeout_panic_and_bad_receipt_never_commit_memory() {
    for script in [
        AppendScript::UnknownOutcome,
        AppendScript::Panic,
        AppendScript::BadReceipt,
    ] {
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
        assert_empty_confirmed(&log);
    }
}

#[tokio::test(start_paused = true)]
async fn cooperative_delay_timeout_latches_without_durable_commit() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Delay,
        Duration::from_millis(1),
    )
    .await;
    let turn_id = "trn_00000000000000000000000000000005".parse().unwrap();
    let started = audit.take_append_started().await;
    let mut append = Box::pin(log.append_validated(vec![user(turn_id)]));
    tokio::select! {
        _ = started => {}
        result = &mut append => panic!("append completed before adapter started: {result:?}"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = append.await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 0);
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
        result = &mut append => panic!("append completed before adapter task started: {result:?}"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        append.await.err().unwrap().kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_empty_confirmed(&log);

    let done = audit.done_receiver.lock().await.take().unwrap();
    let release = audit.release_sender.lock().await.take().unwrap();
    release.send(()).unwrap();
    done.await.unwrap();
    let handle = audit.adapter_task.lock().await.take().unwrap();
    handle.await.unwrap();
    assert_eq!(audit.durable_entries.load(Ordering::SeqCst), 1);
    assert_eq!(
        log.append_validated(vec![user(turn_id)])
            .await
            .err()
            .unwrap()
            .kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn empty_closed_and_sequence_overflow_are_rejected_before_append() {
    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(
        log.append_validated(Vec::new()).await.err().unwrap().kind(),
        ConversationCommitErrorKind::EmptyBatch
    );
    log.closed = true;
    let turn_id = "trn_00000000000000000000000000000006".parse().unwrap();
    assert_eq!(
        log.append_validated(vec![user(turn_id)])
            .await
            .err()
            .unwrap()
            .kind(),
        ConversationCommitErrorKind::Closed
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);

    let (mut log, audit) = open(
        InitializeScript::Normal,
        AppendScript::Normal,
        Duration::from_secs(1),
    )
    .await;
    std::sync::Arc::get_mut(&mut log.state)
        .expect("log owns the only state reference")
        .set_head_for_test(ConversationSeq::new(u64::MAX));
    assert_eq!(
        log.append_validated(vec![user(turn_id)])
            .await
            .err()
            .unwrap()
            .kind(),
        ConversationCommitErrorKind::SequenceOverflow
    );
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
