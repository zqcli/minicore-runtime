#[path = "support/fake_runtime_bindings.rs"]
pub mod fake_runtime_bindings;
pub mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use minicore_runtime::config::{SessionManifest, Timestamp};
use minicore_runtime::conversation::{
    ConversationEntry, ConversationSeq, TurnExecutionRecord, TurnTerminal, UserInputRecord,
    UserMessageEntry,
};
use minicore_runtime::error::{
    EventStreamTakenError, SessionLogErrorKind, SessionOpenErrorKind, SessionShutdownError,
};
use minicore_runtime::{
    BoundedText, KernelConfig, SemanticLimits, SessionId, SessionRuntime, SessionRuntimeOptions,
    TurnId,
};
use tokio::sync::mpsc::error::TryRecvError;

use fake_runtime_bindings::{BindingFixture, fixture};
use support::fake_session_log::{FakeSessionLog, Operation, Script};

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn turn(value: u8) -> TurnId {
    format!("trn_{value:032}").parse().unwrap()
}

fn kernel() -> KernelConfig {
    KernelConfig {
        shutdown_timeout: Duration::from_secs(5),
        log_operation_timeout: Duration::from_secs(10),
        limits: SemanticLimits {
            max_replay_page_size: 1,
            ..SemanticLimits::default()
        },
        ..KernelConfig::default_checked().unwrap()
    }
}

fn options(fixture: &BindingFixture) -> SessionRuntimeOptions {
    SessionRuntimeOptions::new(
        kernel(),
        fixture.bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap()
}

fn manifest(session_id: SessionId, fixture: &BindingFixture) -> SessionManifest {
    SessionManifest::new(session_id, fixture.spec.clone()).unwrap()
}

fn user_entry(fixture: &BindingFixture, turn_id: TurnId) -> ConversationEntry {
    ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(1),
        turn_id,
        input: UserInputRecord::new(BoundedText::new("user").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(
            fixture.spec.model.clone(),
            fixture.spec.reasoning,
            fixture.spec.max_tool_rounds,
        )
        .unwrap(),
        created_at: "2026-08-19T12:34:56.789Z".parse::<Timestamp>().unwrap(),
    })
}

fn assert_send<T: Send>() {}

#[tokio::test(flavor = "current_thread")]
async fn options_validate_kernel_and_expose_readonly_redacted_configuration() {
    let fixture = fixture("host:model");
    let invalid = KernelConfig {
        event_capacity: 0,
        ..kernel()
    };
    let error = SessionRuntimeOptions::new(
        invalid,
        fixture.bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), SessionOpenErrorKind::InvalidConfiguration);

    let options = options(&fixture);
    assert_eq!(options.kernel().event_capacity, kernel().event_capacity);
    assert!(
        options
            .bindings()
            .tools
            .specs_for(&Default::default())
            .is_empty()
    );
    let _ = options.task_runtime();
    let debug = format!("{options:?}");
    assert!(!debug.contains("host:model"));
    assert!(!debug.contains("system"));
    assert_send::<SessionRuntimeOptions>();
    assert_send::<SessionRuntime>();
}

#[tokio::test(flavor = "current_thread")]
async fn create_initializes_zero_head_returns_no_snapshot_and_shutdown_is_a_barrier() {
    let fixture = fixture("host:model");
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let session_id = session(1);
    let mut owner = SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();

    assert_eq!(owner.session_id(), session_id);
    assert_ne!(owner.instance_id().as_bytes(), &[0; 16]);
    assert_eq!(inspection.head(), ConversationSeq::ZERO);
    assert_eq!(inspection.manifest().unwrap().session_id, session_id);
    assert_eq!(inspection.operations(), vec![Operation::Initialize]);
    assert_eq!(fixture.descriptor_calls.load(Ordering::SeqCst), 1);

    let mut events = owner.take_events().unwrap();
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(
        owner.take_events().unwrap_err(),
        EventStreamTakenError::AlreadyTaken
    );
    owner.shutdown().await.unwrap();
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(events.recv().await, None);
    assert_eq!(inspection.active_mutable_operations(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn load_repairs_before_ready_and_model_mismatch_closes_before_replay() {
    let session_id = session(2);
    let turn_id = turn(2);
    let stored_fixture = fixture("host:model");
    let log = FakeSessionLog::with_initial(
        manifest(session_id, &stored_fixture),
        vec![user_entry(&stored_fixture, turn_id)],
    )
    .unwrap();
    let inspection = log.inspection();
    let mut owner = SessionRuntime::load(session_id, Box::new(log), options(&stored_fixture))
        .await
        .unwrap();
    assert_eq!(
        inspection.operations(),
        vec![
            Operation::LoadManifest,
            Operation::ReadPage {
                after: None,
                limit: 1,
            },
            Operation::Append {
                expected_head: ConversationSeq::new(1),
                entries: inspection.entries()[1..].to_vec(),
            },
        ]
    );
    assert_eq!(inspection.head(), ConversationSeq::new(2));
    assert!(matches!(
        inspection.entries().last(),
        Some(ConversationEntry::TurnTerminal(entry))
            if entry.turn_id == turn_id && entry.terminal == TurnTerminal::CancelledByRestart
    ));
    assert_eq!(
        owner.take_events().unwrap().try_recv(),
        Err(TryRecvError::Empty)
    );
    owner.shutdown().await.unwrap();

    let mismatched = fixture("other:model");
    let log = FakeSessionLog::with_initial(
        manifest(session_id, &stored_fixture),
        vec![user_entry(&stored_fixture, turn(20))],
    )
    .unwrap();
    let inspection = log.inspection();
    let error = match SessionRuntime::load(session_id, Box::new(log), options(&mismatched)).await {
        Err(error) => error,
        Ok(owner) => {
            owner.shutdown().await.unwrap();
            panic!("model mismatch unexpectedly loaded")
        }
    };
    assert_eq!(error.kind(), SessionOpenErrorKind::BindingMismatch);
    assert_eq!(
        inspection.operations(),
        vec![Operation::LoadManifest, Operation::Close]
    );
    assert!(!inspection.operations().iter().any(|operation| matches!(
        operation,
        Operation::ReadPage { .. } | Operation::Append { .. }
    )));
    assert_eq!(inspection.close_count(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn open_errors_preserve_identity_log_and_secondary_close_distinctions() {
    let fixture = fixture("host:model");
    let actual = session(3);
    let expected = session(4);
    let mut wrong = FakeSessionLog::with_initial(manifest(actual, &fixture), Vec::new()).unwrap();
    wrong.script_close(Script::Error(SessionLogErrorKind::Unavailable));
    let inspection = wrong.inspection();
    let error = match SessionRuntime::load(expected, Box::new(wrong), options(&fixture)).await {
        Err(error) => error,
        Ok(owner) => {
            owner.shutdown().await.unwrap();
            panic!("wrong session identity unexpectedly loaded")
        }
    };
    assert_eq!(error.kind(), SessionOpenErrorKind::SessionIdMismatch);
    assert_eq!(error.session_id_mismatch(), Some((expected, actual)));
    assert!(error.log_error().is_none());
    assert!(error.secondary_diagnostic().is_some());
    assert_eq!(inspection.close_count(), 1);
    assert!(!format!("{error:?} {error}").contains("fake session log"));

    let mut failed = FakeSessionLog::new();
    failed.script_initialize(Script::Error(SessionLogErrorKind::Conflict));
    let inspection = failed.inspection();
    let error = match SessionRuntime::create(
        session(5),
        fixture.spec.clone(),
        Box::new(failed),
        options(&fixture),
    )
    .await
    {
        Err(error) => error,
        Ok(owner) => {
            owner.shutdown().await.unwrap();
            panic!("failed initialize unexpectedly opened")
        }
    };
    assert_eq!(error.kind(), SessionOpenErrorKind::Log);
    assert_eq!(
        error.log_error().unwrap().kind(),
        SessionLogErrorKind::Conflict
    );
    assert_eq!(inspection.close_count(), 1);

    let mut invalid_manifest = manifest(session(5), &fixture);
    invalid_manifest.format_version = SessionManifest::FORMAT_VERSION + 1;
    let invalid = FakeSessionLog::with_initial(invalid_manifest, Vec::new()).unwrap();
    let inspection = invalid.inspection();
    let error = match SessionRuntime::load(session(5), Box::new(invalid), options(&fixture)).await {
        Err(error) => error,
        Ok(owner) => {
            owner.shutdown().await.unwrap();
            panic!("invalid manifest unexpectedly loaded")
        }
    };
    assert_eq!(error.kind(), SessionOpenErrorKind::InvalidManifest);
    assert_eq!(inspection.close_count(), 1);
}

#[test]
fn stopped_task_runtime_maps_actor_start_failure_and_closes_unstarted_log() {
    let stopped = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let stopped_handle = stopped.handle().clone();
    drop(stopped);
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    caller.block_on(async {
        let fixture = fixture("host:model");
        let log = FakeSessionLog::new();
        let inspection = log.inspection();
        let options =
            SessionRuntimeOptions::new(kernel(), fixture.bindings.clone(), stopped_handle).unwrap();
        let error =
            match SessionRuntime::create(session(13), fixture.spec.clone(), Box::new(log), options)
                .await
            {
                Err(error) => error,
                Ok(owner) => {
                    owner.shutdown().await.unwrap();
                    panic!("stopped task runtime unexpectedly opened")
                }
            };
        assert_eq!(error.kind(), SessionOpenErrorKind::ActorStartFailed);
        assert_eq!(inspection.operations(), vec![Operation::Close]);
        assert_eq!(inspection.close_count(), 1);
    });
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_uncertainty_and_shutdown_close_errors_stay_typed() {
    let recovery_fixture = fixture("host:model");
    let session_id = session(6);
    let mut uncertain = FakeSessionLog::with_initial(
        manifest(session_id, &recovery_fixture),
        vec![user_entry(&recovery_fixture, turn(6))],
    )
    .unwrap();
    uncertain.script_append(Script::UnknownOutcome { committed: false });
    let inspection = uncertain.inspection();
    let error =
        match SessionRuntime::load(session_id, Box::new(uncertain), options(&recovery_fixture))
            .await
        {
            Err(error) => error,
            Ok(owner) => {
                owner.shutdown().await.unwrap();
                panic!("uncertain recovery unexpectedly loaded")
            }
        };
    assert_eq!(error.kind(), SessionOpenErrorKind::RecoveryUncertain);
    assert_eq!(
        error.log_error().unwrap().kind(),
        SessionLogErrorKind::UnknownOutcome
    );
    assert_eq!(inspection.close_count(), 1);

    for (kind, durability) in [
        (SessionLogErrorKind::Unavailable, false),
        (SessionLogErrorKind::UnknownOutcome, true),
    ] {
        let fixture = fixture("host:model");
        let mut log = FakeSessionLog::new();
        log.script_close(Script::Error(kind));
        let owner = SessionRuntime::create(
            session(7),
            fixture.spec.clone(),
            Box::new(log),
            options(&fixture),
        )
        .await
        .unwrap();
        let error = owner.shutdown().await.unwrap_err();
        assert_eq!(
            matches!(error, SessionShutdownError::Durability(_)),
            durability
        );
        assert_eq!(
            matches!(error, SessionShutdownError::LogClose(_)),
            !durability
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_timeout_aborts_and_awaits_the_same_owner_task() {
    let fixture = fixture("host:model");
    let mut log = FakeSessionLog::new();
    log.script_close(Script::Delay(Duration::from_secs(60)));
    let inspection = log.inspection();
    let mut owner = SessionRuntime::create(
        session(8),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    let mut events = owner.take_events().unwrap();
    let shutdown = tokio::spawn(owner.shutdown());
    inspection.wait_for_operation_count(2).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(
        shutdown.await.unwrap(),
        Err(SessionShutdownError::Timeout(_))
    ));
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
    assert_eq!(events.recv().await, None);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn dropped_and_cancelled_open_owners_close_without_orphans() {
    let fixture = fixture("host:model");
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let owner = SessionRuntime::create(
        session(9),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture),
    )
    .await
    .unwrap();
    drop(owner);
    inspection.wait_for_operation_count(2).await;
    assert_eq!(inspection.close_count(), 1);

    let mut create_log = FakeSessionLog::new();
    create_log.script_initialize(Script::Delay(Duration::from_secs(60)));
    let create_inspection = create_log.inspection();
    let create = tokio::spawn(SessionRuntime::create(
        session(10),
        fixture.spec.clone(),
        Box::new(create_log),
        options(&fixture),
    ));
    create_inspection.wait_for_operation_count(1).await;
    create.abort();
    assert!(matches!(
        create.await,
        Err(ref error) if error.is_cancelled()
    ));
    tokio::time::advance(Duration::from_secs(10)).await;
    create_inspection.wait_for_operation_count(2).await;
    assert_eq!(create_inspection.close_count(), 1);
    assert_eq!(create_inspection.active_mutable_operations(), 0);

    let mut load_log =
        FakeSessionLog::with_initial(manifest(session(11), &fixture), Vec::new()).unwrap();
    load_log.script_read(Script::Delay(Duration::from_secs(60)));
    let load_inspection = load_log.inspection();
    let load = tokio::spawn(SessionRuntime::load(
        session(11),
        Box::new(load_log),
        options(&fixture),
    ));
    load_inspection.wait_for_operation_count(2).await;
    load.abort();
    assert!(matches!(
        load.await,
        Err(ref error) if error.is_cancelled()
    ));
    tokio::time::advance(Duration::from_secs(10)).await;
    load_inspection.wait_for_operation_count(3).await;
    assert_eq!(load_inspection.close_count(), 1);
    assert_eq!(load_inspection.active_mutable_operations(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn two_same_id_owners_open_concurrently_with_isolated_cancellation() {
    let fixture = fixture("host:model");
    let session_id = session(12);
    let mut first_log = FakeSessionLog::new();
    first_log.script_initialize(Script::Delay(Duration::from_secs(5)));
    let first_inspection = first_log.inspection();
    let mut second_log = FakeSessionLog::new();
    second_log.script_initialize(Script::Delay(Duration::from_secs(5)));
    let second_inspection = second_log.inspection();
    let first = tokio::spawn(SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(first_log),
        options(&fixture),
    ));
    let second = tokio::spawn(SessionRuntime::create(
        session_id,
        fixture.spec.clone(),
        Box::new(second_log),
        options(&fixture),
    ));
    first_inspection.wait_for_operation_count(1).await;
    second_inspection.wait_for_operation_count(1).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert_eq!(first.session_id(), second.session_id());
    assert_ne!(first.instance_id(), second.instance_id());

    drop(first);
    first_inspection.wait_for_operation_count(2).await;
    assert_eq!(first_inspection.close_count(), 1);
    assert_eq!(second_inspection.close_count(), 0);
    second.shutdown().await.unwrap();
    assert_eq!(second_inspection.close_count(), 1);
}
