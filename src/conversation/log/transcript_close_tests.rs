use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::load_support::*;
use super::*;
use crate::storage::SessionLogErrorKind;

#[tokio::test]
async fn transcript_is_confirmed_bounded_and_validates_page_contract() {
    let audit = Audit::new(Vec::new());
    let mut log = initialized_log(
        Arc::clone(&audit),
        AppendMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    let turn = turn_id(5);
    log.append_validated(vec![
        user_draft(turn),
        final_draft(turn),
        terminal_draft(turn),
    ])
    .await
    .unwrap();
    let first = log.transcript(None, 2).await.unwrap();
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.next_after, Some(ConversationSeq::new(2)));
    assert!(first.complete);
    let second = log.transcript(first.next_after, 2).await.unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.next_after, None);
    assert_eq!(second.observed_head, ConversationSeq::new(3));

    let reads = audit.read_calls.load(Ordering::SeqCst);
    assert!(log.transcript(None, 0).await.is_err());
    assert_eq!(audit.read_calls.load(Ordering::SeqCst), reads);
    let mut small_kernel = kernel();
    small_kernel.limits.max_transcript_page_size = 1;
    let small_audit = Audit::new(Vec::new());
    let mut small = initialized_log(
        Arc::clone(&small_audit),
        AppendMode::Success,
        CloseMode::Success,
        small_kernel,
    )
    .await;
    assert!(small.transcript(None, 2).await.is_err());
    assert_eq!(small_audit.read_calls.load(Ordering::SeqCst), 0);

    audit
        .pages
        .lock()
        .await
        .push_back(page(vec![user(1, turn)], None, 4));
    assert_eq!(
        log.transcript(None, 2).await.err().unwrap().kind(),
        ConversationCommitErrorKind::TranscriptContractViolation
    );
}

#[tokio::test]
async fn transcript_accepts_empty_tail_and_rejects_empty_wrong_head_or_slice_mismatch() {
    let empty_audit = Audit::new(vec![page(Vec::new(), None, 0)]);
    let mut empty = initialized_log(
        Arc::clone(&empty_audit),
        AppendMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    let empty_page = empty
        .transcript(Some(ConversationSeq::ZERO), 2)
        .await
        .unwrap();
    assert!(empty_page.entries.is_empty());
    assert_eq!(empty_page.next_after, None);
    assert_eq!(empty_page.observed_head, ConversationSeq::ZERO);
    assert!(empty_page.complete);

    let wrong_head_audit = Audit::new(vec![page(Vec::new(), None, 1)]);
    let mut wrong_head = initialized_log(
        Arc::clone(&wrong_head_audit),
        AppendMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    assert_eq!(
        wrong_head.transcript(None, 2).await.err().unwrap().kind(),
        ConversationCommitErrorKind::TranscriptContractViolation
    );

    let audit = Audit::new(Vec::new());
    let mut log = initialized_log(
        Arc::clone(&audit),
        AppendMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    let turn = turn_id(7);
    log.append_validated(vec![
        user_draft(turn),
        final_draft(turn),
        terminal_draft(turn),
    ])
    .await
    .unwrap();
    let tail = log
        .transcript(Some(ConversationSeq::new(3)), 2)
        .await
        .unwrap();
    assert!(tail.entries.is_empty());
    assert_eq!(tail.next_after, None);
    assert_eq!(tail.observed_head, ConversationSeq::new(3));
    assert!(tail.complete);
    audit
        .pages
        .lock()
        .await
        .push_back(page(vec![user(1, turn_id(9))], Some(1), 3));
    assert_eq!(
        log.transcript(None, 2).await.err().unwrap().kind(),
        ConversationCommitErrorKind::TranscriptProjectionMismatch
    );
}

#[tokio::test]
async fn transcript_unknown_latches_and_falls_back_to_confirmed_projection() {
    let audit = Audit::new(Vec::new());
    let mut log = initialized_log(
        Arc::clone(&audit),
        AppendMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    let turn = turn_id(8);
    log.append_validated(vec![
        user_draft(turn),
        final_draft(turn),
        terminal_draft(turn),
    ])
    .await
    .unwrap();
    let confirmed = log.projection().entries().to_vec();
    audit.force_read_unknown();
    let error = log.transcript(None, 2).await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::DurabilityUnknown);
    let reads = audit.read_calls.load(Ordering::SeqCst);
    let first = log.transcript(None, 2).await.unwrap();
    assert_eq!(first.entries, confirmed[..2]);
    assert_eq!(first.next_after, Some(ConversationSeq::new(2)));
    assert_eq!(first.observed_head, ConversationSeq::new(3));
    assert!(!first.complete);
    assert_eq!(audit.read_calls.load(Ordering::SeqCst), reads);
    let second = log.transcript(first.next_after, 2).await.unwrap();
    assert_eq!(second.entries, confirmed[2..]);
    assert_eq!(second.next_after, None);
    assert_eq!(second.observed_head, ConversationSeq::new(3));
    assert!(!second.complete);
    assert_eq!(audit.read_calls.load(Ordering::SeqCst), reads);
    assert!(log.transcript(None, 0).await.is_err());
    assert_eq!(
        log.transcript(Some(ConversationSeq::new(4)), 2)
            .await
            .err()
            .unwrap()
            .kind(),
        ConversationCommitErrorKind::TranscriptCursor
    );
    assert_eq!(audit.read_calls.load(Ordering::SeqCst), reads);
}

#[tokio::test]
async fn closed_transcript_rejects_before_adapter_io() {
    let audit = Audit::new(Vec::new());
    let mut log = initialized_log(
        Arc::clone(&audit),
        AppendMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    log.close().await.unwrap();
    let reads = audit.read_calls.load(Ordering::SeqCst);
    let error = log.transcript(None, 0).await.err().unwrap();
    assert_eq!(error.kind(), ConversationCommitErrorKind::Closed);
    assert_eq!(audit.read_calls.load(Ordering::SeqCst), reads);
}

#[tokio::test]
async fn close_is_once_safe_and_allowed_after_unknown() {
    let audit = Audit::new(Vec::new());
    let mut log = initialized_log(
        Arc::clone(&audit),
        AppendMode::Success,
        CloseMode::Success,
        kernel(),
    )
    .await;
    assert!(log.close().await.is_ok());
    assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        log.close().await.err().unwrap().kind(),
        ConversationCommitErrorKind::Closed
    );

    for mode in [
        CloseMode::Known(SessionLogErrorKind::Unavailable),
        CloseMode::Panic,
    ] {
        let audit = Audit::new(Vec::new());
        let mut log =
            initialized_log(Arc::clone(&audit), AppendMode::Success, mode, kernel()).await;
        let error = log.close().await.err().unwrap();
        let expected = if matches!(mode, CloseMode::Panic) {
            ConversationCommitErrorKind::DurabilityUnknown
        } else {
            ConversationCommitErrorKind::Log(SessionLogErrorKind::Unavailable)
        };
        assert_eq!(error.kind(), expected);
        assert_eq!(audit.close_calls.load(Ordering::SeqCst), 1);
    }

    let audit = Audit::new(Vec::new());
    let mut log = initialized_log(
        Arc::clone(&audit),
        AppendMode::Unknown,
        CloseMode::Success,
        kernel(),
    )
    .await;
    let turn = turn_id(6);
    assert!(log.append_validated(vec![user_draft(turn)]).await.is_err());
    assert!(log.close().await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn close_timeout_is_safe_and_not_repeatable() {
    let audit = Audit::new(Vec::new());
    let mut log = initialized_log(
        Arc::clone(&audit),
        AppendMode::Success,
        CloseMode::Timeout,
        kernel(),
    )
    .await;
    let started = audit.take_close_started().await;
    let mut close = Box::pin(log.close());
    tokio::select! {
        _ = started => {}
        result = &mut close => panic!("close completed before adapter started: {result:?}"),
    }
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(
        close.await.err().unwrap().kind(),
        ConversationCommitErrorKind::DurabilityUnknown
    );
    assert_eq!(
        log.close().await.err().unwrap().kind(),
        ConversationCommitErrorKind::Closed
    );
}
