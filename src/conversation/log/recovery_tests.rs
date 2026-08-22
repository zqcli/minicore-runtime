use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::load_support::*;
use super::*;
use crate::storage::SessionLogErrorKind;
use crate::tools::ToolResultOutcome;

#[tokio::test]
async fn recovery_repairs_no_tools_with_one_exact_terminal_batch() {
    let (pending, audit) = begin_default(active_pages(Vec::new()), AppendMode::Success).await;
    let pending = pending.unwrap();
    let proof = bindings_validated(&pending);
    let log = pending.finish(proof).await.unwrap();
    assert_eq!(log.head(), ConversationSeq::new(3));
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 0);
    let batches = audit.batches.lock().await;
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), 1);
    assert!(matches!(
        &batches[0][0],
        ConversationEntry::TurnTerminal(entry)
            if entry.seq == ConversationSeq::new(3)
                && entry.turn_id == turn_id(2)
                && entry.terminal == TurnTerminal::CancelledByRestart
                && entry.usage == Usage::default()
    ));
}

#[tokio::test]
async fn recovery_repairs_tools_in_order_with_exact_cancelled_entries() {
    let calls = vec![
        tool_call("call-a", 0, "read_file"),
        tool_call("call-b", 1, "write_file"),
    ];
    let (pending, audit) = begin_default(active_pages(calls), AppendMode::Success).await;
    let pending = pending.unwrap();
    let proof = bindings_validated(&pending);
    let log = pending.finish(proof).await.unwrap();
    assert_eq!(log.recovery_plan(), None);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    let batches = audit.batches.lock().await;
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), 3);
    assert!(matches!(
        &batches[0][0],
        ConversationEntry::ToolResult(entry)
            if entry.seq == ConversationSeq::new(3)
                && entry.turn_id == turn_id(2)
                && entry.tool_call_id.as_str() == "call-a"
                && entry.tool_name.as_str() == "read_file"
                && entry.outcome == ToolResultOutcome::Cancelled
                && entry.content.as_str() == "tool call cancelled by restart"
    ));
    assert!(matches!(
        &batches[0][1],
        ConversationEntry::ToolResult(entry)
            if entry.seq == ConversationSeq::new(4)
                && entry.turn_id == turn_id(2)
                && entry.tool_call_id.as_str() == "call-b"
                && entry.tool_name.as_str() == "write_file"
                && entry.outcome == ToolResultOutcome::Cancelled
                && entry.content.as_str() == "tool call cancelled by restart"
    ));
    assert!(matches!(
        &batches[0][2],
        ConversationEntry::TurnTerminal(entry)
            if entry.seq == ConversationSeq::new(5)
                && entry.turn_id == turn_id(2)
                && entry.terminal == TurnTerminal::CancelledByRestart
                && entry.usage == Usage::default()
    ));
}

#[tokio::test]
async fn recovery_content_is_nonempty_ascii_and_obeys_custom_output_limits() {
    for maximum in [1, 8] {
        let calls = vec![tool_call("call-a", 0, "read_file")];
        let (pending, audit) = begin_with_modes(
            active_pages(calls),
            AppendMode::Success,
            OperationMode::Success,
            OperationMode::Success,
            CloseMode::Success,
            kernel_with_output_limit(maximum),
        )
        .await;
        let pending = pending.unwrap();
        let proof = bindings_validated(&pending);
        let log = pending.finish(proof).await.unwrap();
        assert_eq!(log.head(), ConversationSeq::new(4));
        assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
        let batches = audit.batches.lock().await;
        let content = match &batches[0][0] {
            ConversationEntry::ToolResult(entry) => entry.content.as_str(),
            _ => panic!("recovery must append a cancelled tool result first"),
        };
        let expected = &"tool call cancelled by restart"[..maximum];
        assert_eq!(content, expected);
        assert!(!content.is_empty());
        assert!(content.is_ascii());
        assert!(content.len() <= maximum);
    }
}

#[tokio::test]
async fn recovery_known_failure_preserves_primary_log_and_secondary_close_error() {
    let (pending, audit) = begin_with_modes(
        active_pages(vec![tool_call("call-a", 0, "read_file")]),
        AppendMode::Known(SessionLogErrorKind::Unavailable),
        OperationMode::Success,
        OperationMode::Success,
        CloseMode::Known(SessionLogErrorKind::Conflict),
        kernel(),
    )
    .await;
    let pending = pending.unwrap();
    let proof = bindings_validated(&pending);
    let error = pending.finish(proof).await.err().unwrap();
    assert_eq!(
        error.kind(),
        ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
    );
    assert_eq!(
        error.primary_log_error().unwrap().kind(),
        SessionLogErrorKind::Unavailable
    );
    assert_eq!(
        error
            .primary_log_error()
            .unwrap()
            .diagnostic()
            .message
            .as_str(),
        "paged log error"
    );
    assert_eq!(
        error.secondary_close_error().unwrap().kind(),
        SessionLogErrorKind::Conflict
    );
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Conflict)
    );
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recovery_unknown_and_bad_receipt_preserve_uncertain_primary_and_close_once() {
    for mode in [AppendMode::Unknown, AppendMode::BadReceipt] {
        let (pending, audit) = begin_default(
            active_pages(vec![tool_call("call-a", 0, "read_file")]),
            mode,
        )
        .await;
        let pending = pending.unwrap();
        let proof = bindings_validated(&pending);
        let error = pending.finish(proof).await.err().unwrap();
        assert_eq!(error.kind(), ConversationCommitErrorKind::RecoveryUncertain);
        if matches!(mode, AppendMode::Unknown) {
            assert_eq!(
                error.primary_log_error().unwrap().kind(),
                SessionLogErrorKind::UnknownOutcome
            );
        } else {
            assert!(error.primary_log_error().is_none());
        }
        assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
        assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
    }

    let (pending, audit) = begin_with_modes(
        active_pages(vec![tool_call("call-a", 0, "read_file")]),
        AppendMode::Unknown,
        OperationMode::Success,
        OperationMode::Success,
        CloseMode::Panic,
        kernel(),
    )
    .await;
    let pending = pending.unwrap();
    let proof = bindings_validated(&pending);
    let error = pending.finish(proof).await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::RecoveryUncertain);
    assert_eq!(
        error.secondary_close_kind(),
        Some(SessionLogErrorKind::Internal)
    );
    assert!(matches!(
        error.secondary_close_outcome(),
        Some(ConversationCloseOutcome::Panic)
    ));
    assert!(error.secondary_close_error().is_none());
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recovery_append_panic_is_uncertain_and_closes_once() {
    let (pending, audit) = begin_with_modes(
        active_pages(vec![tool_call("call-a", 0, "read_file")]),
        AppendMode::Panic,
        OperationMode::Success,
        OperationMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    let pending = pending.unwrap();
    let proof = bindings_validated(&pending);
    let error = pending.finish(proof).await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::RecoveryUncertain);
    assert!(error.primary_log_error().is_none());
    assert!(error.secondary_close_outcome().is_none());
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn recovery_timeout_is_uncertain_without_retry_and_closes_once() {
    let audit = Audit::new(active_pages(vec![tool_call("call-a", 0, "read_file")]));
    let pending = ConversationLog::begin_load(
        session_id(),
        Box::new(PagedLog::new(
            Arc::clone(&audit),
            manifest(),
            AppendMode::Timeout,
            CloseMode::Success,
        )),
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
    let started = audit.take_append_started().await;
    let proof = bindings_validated(&pending);
    let mut finish = Box::pin(pending.finish(proof));
    tokio::select! {
        _ = started => {}
        _ = &mut finish => panic!("repair completed before append started"),
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = finish.await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::RecoveryUncertain);
    assert_eq!(audit.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
}
