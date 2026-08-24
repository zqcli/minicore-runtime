pub mod support;

use std::sync::Arc;
use std::time::Duration;

use minicore_runtime::conversation::{ConversationSeq, TurnTerminal};
use minicore_runtime::error::{
    DiagnosticCategory, DiagnosticCode, SessionError, SessionLogErrorKind, TurnWaitError,
};
use minicore_runtime::model::{Model, ModelRef};
use minicore_runtime::session::{SessionEvent, SessionHealth, SessionStatus};
use minicore_runtime::storage::ConversationPage;
use minicore_runtime::tools::ToolSet;
use minicore_runtime::{
    KernelConfig, SessionBindings, SessionRuntime, SessionRuntimeOptions, TurnOptions, UserInput,
};

use support::fake_session_log::{FakeSessionLog, Script};
use support::transcript_runtime::{TestModel, create_runtime, session, test_spec};

#[tokio::test(flavor = "current_thread")]
async fn transcript_store_conflict_returns_log_conflict_and_degrades_session() {
    let mut log = FakeSessionLog::new();
    log.script_read(Script::Error(SessionLogErrorKind::Conflict));

    let (runtime, handle, _inspection, _events) = create_runtime(session(1), log).await;

    let transcript_error = handle.transcript(None, 32).await.unwrap_err();
    match transcript_error {
        SessionError::TranscriptUnavailable(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::LogConflict);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected TranscriptUnavailable with LogConflict, got: {other:?}"),
    }

    match handle.state().health {
        SessionHealth::Degraded { diagnostic } => {
            assert_eq!(diagnostic.code, DiagnosticCode::LogConflict);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected SessionHealth::Degraded, got: {other:?}"),
    }

    let submit_error = handle
        .submit(UserInput::text("hello").unwrap(), TurnOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(submit_error, SessionError::Degraded(_)));

    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transcript_store_corrupt_emits_health_changed_and_degrades_session() {
    let mut log = FakeSessionLog::new();
    log.script_read(Script::Error(SessionLogErrorKind::Corrupt));

    let (runtime, handle, _inspection, mut events) = create_runtime(session(2), log).await;

    let transcript_error = handle.transcript(None, 32).await.unwrap_err();
    match transcript_error {
        SessionError::TranscriptUnavailable(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::LogCorrupt);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected TranscriptUnavailable with LogCorrupt, got: {other:?}"),
    }

    let mut health_event_found = false;
    while let Ok(Some(envelope)) =
        tokio::time::timeout(Duration::from_millis(200), events.recv()).await
    {
        if let SessionEvent::HealthChanged { health } = envelope.event {
            if matches!(health, SessionHealth::Degraded { .. }) {
                health_event_found = true;
                break;
            }
        }
    }
    assert!(
        health_event_found,
        "HealthChanged event must be emitted when degraded"
    );

    let submit_error = handle
        .submit(UserInput::text("hello").unwrap(), TurnOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(submit_error, SessionError::Degraded(_)));

    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transcript_observed_head_mismatch_degrades_session() {
    let log = FakeSessionLog::new();
    let (runtime, handle, inspection, _events) = create_runtime(session(3), log).await;

    let turn_handle = handle
        .submit(UserInput::text("first").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let outcome = turn_handle.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::Completed);

    let fake_page = ConversationPage {
        entries: vec![],
        next_after: None,
        observed_head: ConversationSeq::new(99),
    };
    inspection.script_read(Script::Page(fake_page));

    let transcript_err = handle.transcript(None, 32).await.unwrap_err();
    match transcript_err {
        SessionError::TranscriptUnavailable(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::LogCorrupt);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected TranscriptUnavailable, got: {other:?}"),
    }
    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));

    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transcript_page_contract_violation_degrades_session() {
    let log = FakeSessionLog::new();
    let (runtime, handle, inspection, _events) = create_runtime(session(4), log).await;

    let bad_page = ConversationPage {
        entries: vec![],
        next_after: None,
        observed_head: ConversationSeq::new(10),
    };
    inspection.script_read(Script::Page(bad_page));

    let transcript_err = handle.transcript(None, 32).await.unwrap_err();
    match transcript_err {
        SessionError::TranscriptUnavailable(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::LogCorrupt);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected TranscriptUnavailable, got: {other:?}"),
    }
    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));

    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transcript_caller_invalid_cursor_and_limit_return_invalid_input_and_remain_healthy() {
    let log = FakeSessionLog::new();
    let (runtime, handle, _inspection, _events) = create_runtime(session(5), log).await;

    let invalid_cursor_err = handle
        .transcript(Some(ConversationSeq::new(999)), 32)
        .await
        .unwrap_err();
    match invalid_cursor_err {
        SessionError::InvalidInput(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::InvalidConfiguration);
            assert_eq!(diagnostic.category, DiagnosticCategory::Configuration);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected InvalidInput for invalid cursor, got: {other:?}"),
    }
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let zero_limit_err = handle.transcript(None, 0).await.unwrap_err();
    match zero_limit_err {
        SessionError::InvalidInput(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::InvalidConfiguration);
            assert_eq!(diagnostic.category, DiagnosticCategory::Configuration);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected InvalidInput for zero limit, got: {other:?}"),
    }
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let excessive_limit_err = handle.transcript(None, 5_000).await.unwrap_err();
    match excessive_limit_err {
        SessionError::InvalidInput(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::InvalidConfiguration);
            assert_eq!(diagnostic.category, DiagnosticCategory::Configuration);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected InvalidInput for excessive limit, got: {other:?}"),
    }
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let turn_handle = handle
        .submit(UserInput::text("valid").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let outcome = turn_handle.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::Completed);

    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transcript_temporary_store_unavailable_returns_retryable_and_remains_healthy() {
    let mut log = FakeSessionLog::new();
    log.script_read(Script::Error(SessionLogErrorKind::Unavailable));

    let (runtime, handle, _inspection, _events) = create_runtime(session(6), log).await;

    let transcript_error = handle.transcript(None, 32).await.unwrap_err();
    match transcript_error {
        SessionError::TranscriptUnavailable(diagnostic) => {
            assert!(diagnostic.retryable);
        }
        other => panic!("expected TranscriptUnavailable, got: {other:?}"),
    }
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let page = handle.transcript(None, 32).await.unwrap();
    assert_eq!(page.entries.len(), 0);
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let turn_handle = handle
        .submit(UserInput::text("valid").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let outcome = turn_handle.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::Completed);

    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transcript_store_closed_and_internal_preserve_healthy_session() {
    let mut log = FakeSessionLog::new();
    log.script_read(Script::Error(SessionLogErrorKind::Closed));
    log.script_read(Script::Error(SessionLogErrorKind::Internal));

    let (runtime, handle, _inspection, _events) = create_runtime(session(7), log).await;

    let closed_err = handle.transcript(None, 32).await.unwrap_err();
    assert!(
        matches!(closed_err, SessionError::Closed),
        "expected Closed, got: {closed_err:?}"
    );
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let internal_err = handle.transcript(None, 32).await.unwrap_err();
    match internal_err {
        SessionError::TranscriptUnavailable(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::Internal);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected TranscriptUnavailable(Internal), got: {other:?}"),
    }
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let page = handle.transcript(None, 32).await.unwrap();
    assert_eq!(page.entries.len(), 0);
    assert_eq!(handle.state().health, SessionHealth::Healthy);

    let turn_handle = handle
        .submit(UserInput::text("valid").unwrap(), TurnOptions::default())
        .await
        .unwrap();
    let outcome = turn_handle.wait().await.unwrap();
    assert_eq!(outcome.terminal, TurnTerminal::Completed);

    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn degraded_session_permits_state_read_and_shutdown_while_rejecting_submit() {
    let mut log = FakeSessionLog::new();
    log.script_read(Script::Error(SessionLogErrorKind::Conflict));

    let (runtime, handle, inspection, _events) = create_runtime(session(8), log).await;

    let _ = handle.transcript(None, 32).await;

    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));

    let watch = handle.watch_state();
    assert!(matches!(
        watch.borrow().health,
        SessionHealth::Degraded { .. }
    ));

    let state = handle.state();
    assert_eq!(state.status, SessionStatus::Idle);

    let submit_error = handle
        .submit(UserInput::text("reject").unwrap(), TurnOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(submit_error, SessionError::Degraded(_)));

    runtime.shutdown().await.unwrap();
    assert_eq!(inspection.close_count(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn active_turn_transcript_conflict_degrades_cancels_and_prevents_settlement() {
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let model_ref: ModelRef = "host:blocking-model".parse().unwrap();
    let model: Arc<dyn Model> = Arc::new(TestModel::blocking(
        model_ref.clone(),
        Arc::clone(&started),
        Arc::clone(&release),
    ));

    let spec = test_spec(model_ref);
    let bindings = SessionBindings::new(model, ToolSet::default(), None, None, None);
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        bindings,
        tokio::runtime::Handle::current(),
    )
    .unwrap();

    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(session(9), spec, Box::new(log), options)
        .await
        .unwrap();

    let handle = runtime.handle();

    let turn_handle = handle
        .submit(
            UserInput::text("start turn").unwrap(),
            TurnOptions::default(),
        )
        .await
        .unwrap();

    started.acquire().await.unwrap().forget();

    assert_eq!(handle.state().status, SessionStatus::Running);
    assert_eq!(handle.state().conversation_seq, ConversationSeq::new(1));

    inspection.script_read(Script::Error(SessionLogErrorKind::Conflict));

    let transcript_err = handle.transcript(None, 32).await.unwrap_err();
    match transcript_err {
        SessionError::TranscriptUnavailable(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::LogConflict);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
            assert!(!diagnostic.retryable);
        }
        other => panic!("expected TranscriptUnavailable(LogConflict), got: {other:?}"),
    }

    assert!(matches!(
        handle.state().health,
        SessionHealth::Degraded { .. }
    ));

    let wait_err = turn_handle.wait().await.unwrap_err();
    match wait_err {
        TurnWaitError::DurabilityUnavailable(diagnostic) => {
            assert_eq!(diagnostic.code, DiagnosticCode::LogConflict);
            assert_eq!(diagnostic.category, DiagnosticCategory::Storage);
        }
        other => panic!("expected TurnWaitError::DurabilityUnavailable, got: {other:?}"),
    }

    assert_eq!(inspection.head(), ConversationSeq::new(1));

    let submit_err = handle
        .submit(UserInput::text("rejected").unwrap(), TurnOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(submit_err, SessionError::Degraded(_)));

    runtime.shutdown().await.unwrap();
    assert_eq!(inspection.close_count(), 1);
}
