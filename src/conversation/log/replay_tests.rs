use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::load_support::*;
use super::*;
use crate::storage::SessionLogErrorKind;

#[tokio::test]
async fn two_phase_load_exposes_manifest_before_replay_and_abort_closes_once() {
    let audit = Audit::new(completed_pages());
    let pending = ConversationLog::begin_load(
        session_id(),
        Box::new(PagedLog::new(
            Arc::clone(&audit),
            manifest(),
            AppendMode::Success,
            CloseMode::Success,
        )),
        kernel(),
        Box::new(timestamp),
    )
    .await
    .unwrap();
    assert_eq!(pending.manifest(), &manifest());
    assert_eq!(audit.read_calls.load(Ordering::SeqCst), 0);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 0);
    assert_eq!(pending.abort().await, None);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let abort_audit = Audit::new(Vec::new());
    let pending = ConversationLog::begin_load(
        session_id(),
        Box::new(PagedLog::new(
            Arc::clone(&abort_audit),
            manifest(),
            AppendMode::Success,
            CloseMode::Known(SessionLogErrorKind::Conflict),
        )),
        kernel(),
        Box::new(timestamp),
    )
    .await
    .unwrap();
    assert!(matches!(
        pending.abort().await,
        Some(ConversationCloseOutcome::Known(error))
            if error.kind() == SessionLogErrorKind::Conflict
    ));
    assert_eq!(abort_audit.close_calls.load(Ordering::SeqCst), 1);

    let repair_audit = Audit::new(active_pages(Vec::new()));
    let pending = ConversationLog::begin_load(
        session_id(),
        Box::new(PagedLog::new(
            Arc::clone(&repair_audit),
            manifest(),
            AppendMode::Success,
            CloseMode::Success,
        )),
        kernel(),
        Box::new(timestamp),
    )
    .await
    .unwrap();
    assert_eq!(repair_audit.read_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repair_audit.append_calls.load(Ordering::SeqCst), 0);
    let proof = bindings_validated(&pending);
    let log = pending.finish(proof).await.unwrap();
    assert_eq!(log.head(), ConversationSeq::new(3));
    let last_terminal = log.last_terminal().unwrap();
    assert_eq!(last_terminal.seq, ConversationSeq::new(3));
    assert_eq!(last_terminal.turn_id, turn_id(2));
    assert_eq!(last_terminal.terminal, TurnTerminal::CancelledByRestart);
    assert_eq!(repair_audit.read_calls.load(Ordering::SeqCst), 2);
    assert_eq!(repair_audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(repair_audit.close_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn compatibility_proof_is_bound_to_one_pending_load() {
    let (pending_a, audit_a) = begin_default(completed_pages(), AppendMode::Success).await;
    let pending_a = pending_a.unwrap();
    let (pending_b, audit_b) = begin_default(completed_pages(), AppendMode::Success).await;
    let pending_b = pending_b.unwrap();
    let proof_a = bindings_validated(&pending_a);
    let error = pending_b.finish(proof_a).await.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::CompatibilityProofMismatch
    );
    assert_eq!(audit_b.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit_a.close_calls.load(Ordering::SeqCst), 0);

    // The proof was moved into B and cannot be reused; A gets its own proof.
    let proof_a = bindings_validated(&pending_a);
    let log = pending_a.finish(proof_a).await.unwrap();
    assert_eq!(log.head(), ConversationSeq::new(3));
    assert_eq!(audit_a.close_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn compatibility_proof_mismatch_preserves_primary_with_known_close_error() {
    let (pending_a, audit_a) = begin_default(completed_pages(), AppendMode::Success).await;
    let pending_a = pending_a.unwrap();
    let (pending_b, audit_b) = begin_with_modes(
        completed_pages(),
        AppendMode::Success,
        OperationMode::Success,
        OperationMode::Success,
        CloseMode::Known(SessionLogErrorKind::Conflict),
        kernel(),
    )
    .await;
    let pending_b = pending_b.unwrap();
    let proof_a = bindings_validated(&pending_a);
    let error = pending_b.finish(proof_a).await.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::CompatibilityProofMismatch
    );
    assert!(error.primary_log_error().is_none());
    assert!(matches!(
        error.secondary_close_outcome(),
        Some(ConversationCloseOutcome::Known(_))
    ));
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Conflict)
    );
    assert_eq!(audit_b.close_calls.load(Ordering::SeqCst), 1);

    let proof_a = bindings_validated(&pending_a);
    let log = pending_a.finish(proof_a).await.unwrap();
    assert_eq!(log.head(), ConversationSeq::new(3));
    assert_eq!(audit_a.close_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn compatibility_proof_mismatch_preserves_primary_with_timeout_close() {
    let (pending_a, audit_a) = begin_default(completed_pages(), AppendMode::Success).await;
    let pending_a = pending_a.unwrap();
    let mut value = kernel();
    value.log_operation_timeout = Duration::from_millis(1);
    let (pending_b, audit_b) = begin_with_modes(
        completed_pages(),
        AppendMode::Success,
        OperationMode::Success,
        OperationMode::Success,
        CloseMode::Timeout,
        value,
    )
    .await;
    let pending_b = pending_b.unwrap();
    let proof_a = bindings_validated(&pending_a);
    let started = audit_b.take_close_started().await;
    let mut finish = Box::pin(pending_b.finish(proof_a));
    tokio::select! {
        _ = started => {}
        _ = &mut finish => panic!("mismatched finish completed before close started"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = finish.await.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::CompatibilityProofMismatch
    );
    assert!(error.primary_log_error().is_none());
    assert!(matches!(
        error.secondary_close_outcome(),
        Some(ConversationCloseOutcome::Timeout)
    ));
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::UnknownOutcome)
    );
    assert_eq!(audit_b.close_calls.load(Ordering::SeqCst), 1);

    let proof_a = bindings_validated(&pending_a);
    let log = pending_a.finish(proof_a).await.unwrap();
    assert_eq!(log.head(), ConversationSeq::new(3));
    assert_eq!(audit_a.close_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn begin_manifest_known_failure_preserves_primary_and_secondary_errors() {
    let (result, audit) = load_with_modes(
        Vec::new(),
        AppendMode::Success,
        OperationMode::Known(SessionLogErrorKind::Unavailable),
        OperationMode::Success,
        CloseMode::Known(SessionLogErrorKind::Conflict),
        kernel(),
    )
    .await;
    let error = result.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
    );
    let primary = error.primary_log_error().unwrap();
    assert_eq!(primary.kind(), SessionLogErrorKind::Unavailable);
    assert_eq!(primary.diagnostic().message.as_str(), "paged log error");
    assert!(!format!("{error:?}").contains("paged log error"));
    assert!(!error.to_string().contains("paged log error"));
    assert_eq!(
        error.secondary_close_error().unwrap().kind(),
        SessionLogErrorKind::Conflict
    );
    assert!(matches!(
        error.secondary_close_outcome(),
        Some(ConversationCloseOutcome::Known(_))
    ));
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Conflict)
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn invalid_kernel_begin_failure_still_closes_owned_adapter() {
    let audit = Audit::new(Vec::new());
    let mut invalid = kernel();
    invalid.log_operation_timeout = Duration::ZERO;
    let started = audit.take_close_started().await;
    let mut begin = Box::pin(ConversationLog::begin_load(
        session_id(),
        Box::new(PagedLog::new(
            Arc::clone(&audit),
            manifest(),
            AppendMode::Success,
            CloseMode::Timeout,
        )),
        invalid,
        Box::new(timestamp),
    ));
    tokio::select! {
        _ = started => {}
        _ = &mut begin => panic!("invalid begin completed before fallback close started"),
    }
    tokio::time::advance(Duration::from_secs(30)).await;
    let error = begin.await.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::InvalidConfiguration
    );
    assert!(error.secondary_close_error().is_none());
    assert!(matches!(
        error.secondary_close_outcome(),
        Some(ConversationCloseOutcome::Timeout)
    ));
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::UnknownOutcome)
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn begin_manifest_timeout_and_panic_close_once() {
    let audit = Audit::new(Vec::new());
    let started = audit.take_load_started().await;
    let mut timeout_load = Box::pin(ConversationLog::begin_load(
        session_id(),
        Box::new(
            PagedLog::new(
                Arc::clone(&audit),
                manifest(),
                AppendMode::Success,
                CloseMode::Success,
            )
            .with_load_mode(OperationMode::Timeout),
        ),
        {
            let mut value = kernel();
            value.log_operation_timeout = Duration::from_millis(1);
            value
        },
        Box::new(timestamp),
    ));
    tokio::select! {
        _ = started => {}
        _ = &mut timeout_load => panic!("manifest load completed before starting"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        timeout_load.await.err().unwrap().kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let panic_audit = Audit::new(Vec::new());
    let error = ConversationLog::begin_load(
        session_id(),
        Box::new(
            PagedLog::new(
                Arc::clone(&panic_audit),
                manifest(),
                AppendMode::Success,
                CloseMode::Success,
            )
            .with_load_mode(OperationMode::Panic),
        ),
        kernel(),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    assert_eq!(panic_audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replay_adapter_failures_close_once() {
    let (result, audit) = load_with_modes(
        Vec::new(),
        AppendMode::Success,
        OperationMode::Success,
        OperationMode::Known(SessionLogErrorKind::Corrupt),
        CloseMode::Success,
        kernel(),
    )
    .await;
    let error = result.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Corrupt)
    );
    assert_eq!(
        error.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::Corrupt
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let (result, audit) = load_with_modes(
        Vec::new(),
        AppendMode::Success,
        OperationMode::Success,
        OperationMode::Panic,
        CloseMode::Success,
        kernel(),
    )
    .await;
    assert_eq!(
        result.err().unwrap().kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn replay_timeout_closes_once() {
    let audit = Audit::new(Vec::new());
    let pending = ConversationLog::begin_load(
        session_id(),
        Box::new(
            PagedLog::new(
                Arc::clone(&audit),
                manifest(),
                AppendMode::Success,
                CloseMode::Success,
            )
            .with_read_mode(OperationMode::Timeout),
        ),
        {
            let mut value = kernel();
            value.log_operation_timeout = Duration::from_millis(1);
            value
        },
        Box::new(timestamp),
    )
    .await
    .unwrap();
    assert_eq!(pending.manifest(), &manifest());
    let started = audit.take_read_started().await;
    let proof = bindings_validated(&pending);
    let mut load = Box::pin(pending.finish(proof));
    tokio::select! {
        _ = started => {}
        _ = &mut load => panic!("page read completed before starting"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = load.await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replay_accepts_empty_and_multi_page_history_without_repair() {
    let (empty, empty_audit) =
        load_with(vec![page(Vec::new(), None, 0)], AppendMode::Success).await;
    let empty = empty.unwrap();
    assert_eq!(empty.head(), ConversationSeq::ZERO);
    assert_eq!(empty_audit.append_calls.load(Ordering::SeqCst), 0);
    assert_eq!(empty_audit.close_calls.load(Ordering::SeqCst), 0);

    let (loaded, audit) = load_with(completed_pages(), AppendMode::Success).await;
    let mut loaded = loaded.unwrap();
    assert_eq!(loaded.head(), ConversationSeq::new(3));
    assert!(loaded.recovery_plan().is_none());
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 0);
    assert_eq!(audit.read_calls.load(Ordering::SeqCst), 3);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 0);
    let page = loaded.transcript(None, 2).await.unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.next_after, Some(ConversationSeq::new(2)));
    assert!(page.complete);
}

#[tokio::test]
async fn replay_rejects_manifest_identity_and_semantic_or_page_contract_errors() {
    let audit = Audit::new(completed_pages());
    let wrong_id = "ses_00000000000000000000000000000022".parse().unwrap();
    let error = ConversationLog::begin_load(
        wrong_id,
        Box::new(PagedLog::new(
            Arc::clone(&audit),
            manifest(),
            AppendMode::Success,
            CloseMode::Known(SessionLogErrorKind::Conflict),
        )),
        kernel(),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::SessionIdMismatch);
    assert_eq!(error.session_id_mismatch(), Some((wrong_id, session_id())));
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Conflict)
    );
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);

    let mut bad_manifest = manifest();
    bad_manifest.format_version = 2;
    let bad_audit = Audit::new(Vec::new());
    let error = ConversationLog::begin_load(
        session_id(),
        Box::new(PagedLog::new(
            Arc::clone(&bad_audit),
            bad_manifest,
            AppendMode::Success,
            CloseMode::Success,
        )),
        kernel(),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::InvalidManifest);
    assert_eq!(bad_audit.close_calls.load(Ordering::SeqCst), 1);

    let mut bad_spec_manifest = manifest();
    bad_spec_manifest.spec.max_tool_rounds = 0;
    let bad_spec_audit = Audit::new(Vec::new());
    let error = ConversationLog::begin_load(
        session_id(),
        Box::new(PagedLog::new(
            Arc::clone(&bad_spec_audit),
            bad_spec_manifest,
            AppendMode::Success,
            CloseMode::Success,
        )),
        kernel(),
        Box::new(timestamp),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::InvalidManifest);
    assert_eq!(bad_spec_audit.close_calls.load(Ordering::SeqCst), 1);

    let turn = turn_id(3);
    let invalid_pages = vec![
        vec![page(vec![user(2, turn)], None, 2)],
        vec![page(vec![user(1, turn), terminal(2, turn)], None, 2)],
        vec![page(
            vec![user(1, turn), result(2, turn, "missing", "read_file")],
            None,
            2,
        )],
        vec![page(
            vec![
                user(1, turn),
                assistant(2, turn, Vec::new()),
                terminal(3, turn),
                terminal(4, turn),
            ],
            None,
            4,
        )],
        vec![page(
            vec![
                user(1, turn),
                assistant(2, turn, vec![tool_call("call-a", 0, "read_file")]),
                terminal(3, turn),
            ],
            None,
            3,
        )],
        vec![page(vec![user(1, turn)], Some(2), 1)],
        vec![page(vec![user(1, turn)], None, 2)],
    ];
    for pages in invalid_pages {
        let (pending, audit) = begin_default(pages, AppendMode::Success).await;
        let pending = pending.unwrap();
        let proof = bindings_validated(&pending);
        let error = pending.finish(proof).await.err().unwrap();
        assert_eq!(error.kind(), ConversationCommitErrorKind::ReplayInvalid);
        assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn replay_rejects_oversized_empty_cursor_nonadvancing_and_head_drift_pages() {
    let turn = turn_id(4);
    let oversized = vec![user(1, turn); 201];
    for pages in [
        vec![page(oversized, None, 1)],
        vec![page(Vec::new(), Some(1), 1)],
        vec![
            page(vec![user(1, turn)], Some(1), 2),
            page(vec![user(1, turn)], None, 2),
        ],
        vec![
            page(vec![user(1, turn)], Some(1), 2),
            page(vec![assistant(2, turn, Vec::new())], None, 3),
        ],
    ] {
        let (pending, audit) = begin_default(pages, AppendMode::Success).await;
        let pending = pending.unwrap();
        let proof = bindings_validated(&pending);
        let error = pending.finish(proof).await.err().unwrap();
        assert_eq!(error.kind(), ConversationCommitErrorKind::ReplayInvalid);
        assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
    }
}
