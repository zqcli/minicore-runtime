#[path = "support/fake_runtime_bindings.rs"]
pub mod fake_runtime_bindings;
pub mod support;

use std::task::Poll;
use std::time::Duration;

use futures_util::future::{AbortHandle, Abortable};
use minicore_runtime::config::{SessionManifest, Timestamp};
use minicore_runtime::conversation::{
    ConversationEntry, ConversationSeq, TurnExecutionRecord, TurnTerminal, TurnTerminalEntry,
    UserInputRecord, UserMessageEntry,
};
use minicore_runtime::model::Usage;
use minicore_runtime::{
    BoundedText, KernelConfig, SemanticLimits, SessionId, SessionRuntime, SessionRuntimeOptions,
    TurnId,
};

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

fn options(
    fixture: &BindingFixture,
    task_runtime: tokio::runtime::Handle,
) -> SessionRuntimeOptions {
    SessionRuntimeOptions::new(kernel(), fixture.bindings.clone(), task_runtime).unwrap()
}

fn manifest(session_id: SessionId, fixture: &BindingFixture) -> SessionManifest {
    SessionManifest::new(session_id, fixture.spec.clone()).unwrap()
}

fn timestamp(value: &str) -> Timestamp {
    value.parse().unwrap()
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
        created_at: timestamp("2026-08-19T12:34:56.789Z"),
    })
}

fn terminal_entry(turn_id: TurnId) -> ConversationEntry {
    ConversationEntry::TurnTerminal(TurnTerminalEntry {
        seq: ConversationSeq::new(2),
        turn_id,
        terminal: TurnTerminal::Completed,
        usage: Usage::new(1, 2, 3),
        created_at: timestamp("2026-08-19T12:34:57.789Z"),
    })
}

fn stopped_handle() -> tokio::runtime::Handle {
    let stopped = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let handle = stopped.handle().clone();
    stopped.shutdown_background();
    handle
}

async fn finish_cancelled_cleanup(
    inspection: &support::fake_session_log::InspectionHandle,
    close_operation_count: usize,
) {
    tokio::time::advance(Duration::from_secs(10)).await;
    inspection
        .wait_for_operation_count(close_operation_count)
        .await;
    inspection.wait_for_idle().await;
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn pre_poll_caller_cancellation_is_closed_by_existing_watcher() {
    let fixture = fixture("host:model");
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let (abort, registration) = AbortHandle::new_pair();
    let mut create = Box::pin(Abortable::new(
        SessionRuntime::create(
            session(19),
            fixture.spec.clone(),
            Box::new(log),
            options(&fixture, stopped_handle()),
        ),
        registration,
    ));
    assert!(matches!(
        futures_util::poll!(create.as_mut()),
        Poll::Pending
    ));
    assert!(inspection.operations().is_empty());
    abort.abort();
    assert!(matches!(
        futures_util::poll!(create.as_mut()),
        Poll::Ready(Err(_))
    ));
    drop(create);

    inspection.wait_for_operation_count(1).await;
    inspection.wait_for_idle().await;
    assert_eq!(inspection.operations(), vec![Operation::Close]);
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fallback_cleanup_survives_caller_abort_after_close_admission() {
    let fixture = fixture("host:model");
    let mut log = FakeSessionLog::new();
    log.script_close(Script::Delay(Duration::from_secs(60)));
    let inspection = log.inspection();
    let create = tokio::spawn(SessionRuntime::create(
        session(20),
        fixture.spec.clone(),
        Box::new(log),
        options(&fixture, stopped_handle()),
    ));
    inspection.wait_for_operation_count(1).await;
    assert_eq!(inspection.operations(), vec![Operation::Close]);

    create.abort();
    assert!(matches!(
        create.await,
        Err(ref error) if error.is_cancelled()
    ));
    finish_cancelled_cleanup(&inspection, 1).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancellation_during_load_manifest_eventually_closes_without_ready() {
    let fixture = fixture("host:model");
    let session_id = session(21);
    let mut log = FakeSessionLog::with_initial(manifest(session_id, &fixture), Vec::new()).unwrap();
    log.script_load_manifest(Script::Delay(Duration::from_secs(60)));
    let inspection = log.inspection();
    let load = tokio::spawn(SessionRuntime::load(
        session_id,
        Box::new(log),
        options(&fixture, tokio::runtime::Handle::current()),
    ));
    inspection.wait_for_operation_count(1).await;
    assert_eq!(inspection.operations(), vec![Operation::LoadManifest]);

    load.abort();
    assert!(matches!(
        load.await,
        Err(ref error) if error.is_cancelled()
    ));
    finish_cancelled_cleanup(&inspection, 2).await;
    assert_eq!(
        inspection.operations(),
        vec![Operation::LoadManifest, Operation::Close]
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancellation_during_later_replay_page_eventually_closes_without_append() {
    let fixture = fixture("host:model");
    let session_id = session(22);
    let turn_id = turn(22);
    let mut log = FakeSessionLog::with_initial(
        manifest(session_id, &fixture),
        vec![user_entry(&fixture, turn_id), terminal_entry(turn_id)],
    )
    .unwrap();
    log.script_read(Script::Continue);
    log.script_read(Script::Delay(Duration::from_secs(60)));
    let inspection = log.inspection();
    let load = tokio::spawn(SessionRuntime::load(
        session_id,
        Box::new(log),
        options(&fixture, tokio::runtime::Handle::current()),
    ));
    inspection.wait_for_operation_count(3).await;
    assert!(matches!(
        &inspection.operations()[..],
        [
            Operation::LoadManifest,
            Operation::ReadPage { after: None, limit: 1 },
            Operation::ReadPage {
                after: Some(after),
                limit: 1,
            },
        ] if *after == ConversationSeq::new(1)
    ));

    load.abort();
    assert!(matches!(
        load.await,
        Err(ref error) if error.is_cancelled()
    ));
    finish_cancelled_cleanup(&inspection, 4).await;
    assert!(
        !inspection
            .operations()
            .iter()
            .any(|operation| matches!(operation, Operation::Append { .. }))
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancellation_during_recovery_append_eventually_closes_without_ready() {
    let fixture = fixture("host:model");
    let session_id = session(23);
    let mut log = FakeSessionLog::with_initial(
        manifest(session_id, &fixture),
        vec![user_entry(&fixture, turn(23))],
    )
    .unwrap();
    log.script_append(Script::Delay(Duration::from_secs(60)));
    let inspection = log.inspection();
    let load = tokio::spawn(SessionRuntime::load(
        session_id,
        Box::new(log),
        options(&fixture, tokio::runtime::Handle::current()),
    ));
    inspection.wait_for_operation_count(3).await;
    assert!(matches!(
        &inspection.operations()[..],
        [
            Operation::LoadManifest,
            Operation::ReadPage {
                after: None,
                limit: 1
            },
            Operation::Append { .. },
        ]
    ));

    load.abort();
    assert!(matches!(
        load.await,
        Err(ref error) if error.is_cancelled()
    ));
    finish_cancelled_cleanup(&inspection, 4).await;
    assert!(matches!(
        inspection.operations().last(),
        Some(Operation::Close)
    ));
}
