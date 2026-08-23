use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use super::*;
use crate::config::{CompactionConfig, SessionManifest};
use crate::conversation::{ConversationEntry, ConversationSeq};
use crate::model::{
    Model, ModelCallContext, ModelDescriptor, ModelRequest, ModelStartFuture, ReasoningPreference,
};
use crate::storage::{AppendReceipt, ConversationPage, LogFuture};
use crate::tools::ToolSet;
use crate::value::BoundedText;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum PayloadPanicPoint {
    CleanupBeforeTake,
    OwnerSpawn,
}

type PayloadPanicKey = (usize, PayloadPanicPoint);
type PayloadPanicScripts = std::sync::Mutex<std::collections::HashSet<PayloadPanicKey>>;

fn script_payload_panic(payload: &SharedOpenPayload, point: PayloadPanicPoint) {
    let key = (Arc::as_ptr(payload) as usize, point);
    let mut scripts = payload_panic_scripts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    scripts.insert(key);
}

pub(super) fn take_scripted_payload_panic(
    payload: &SharedOpenPayload,
    point: PayloadPanicPoint,
) -> bool {
    let key = (Arc::as_ptr(payload) as usize, point);
    payload_panic_scripts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&key)
}

fn payload_panic_scripts() -> &'static PayloadPanicScripts {
    static SCRIPTS: std::sync::OnceLock<PayloadPanicScripts> = std::sync::OnceLock::new();
    SCRIPTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[test]
fn owner_source_has_exact_fields_and_no_manager_handle_or_serde() {
    let source = include_str!("../runtime.rs");
    assert!(!source.contains("#[derive(Clone)]\npub struct SessionRuntime"));
    assert!(!source.contains("#[derive(Clone)]\npub struct SessionRuntimeOptions"));
    for required in [
        "`task_runtime` must be timer-enabled, alive, and actively driven",
        "task_runtime: Handle",
        "owner_task: Option<JoinHandle<SessionActorExit>>",
        "cleanup_watchers: Vec<JoinHandle<Option<SessionOpenError>>>",
        "self.owner_task.as_mut()",
        "fn spawn_watcher(",
        "guard.spawn_watcher(task_runtime, payload, payload_claimed)",
        "guard.spawn_watcher(&current, payload, payload_claimed)",
        "runtime.spawn(watch_unclaimed_payload(",
        "_ = payload_claimed.cancelled() => None",
        "_ = owner_cancel.cancelled() => cleanup_shared_payload(payload).await",
        "self.cleanup_watchers.push(task)",
        "for task in &mut self.cleanup_watchers",
    ] {
        assert!(source.contains(required), "owner source misses {required}");
    }
    for forbidden in [
        "pub fn handle(",
        "HashMap",
        "BTreeMap",
        "SessionManager",
        "SessionHandle",
        "Serialize",
        "Deserialize",
    ] {
        assert!(
            !source.contains(forbidden),
            "owner source contains {forbidden}"
        );
    }
    let owner_fields = source
        .split_once("pub struct SessionRuntime {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(fields, _)| fields)
        .unwrap();
    for forbidden in ["SessionLog", "Store", "Workspace", "bindings", "spec"] {
        assert!(!owner_fields.contains(forbidden));
    }
    assert!(!source.contains("start_cleanup"));
    let open = source
        .split_once("async fn open(")
        .and_then(|(_, rest)| rest.split_once("impl Drop for SessionRuntime"))
        .map(|(body, _)| body)
        .unwrap();
    let watchers = open.find("OpenGuard::new(").unwrap();
    let owner = open.find("spawn_owner(").unwrap();
    let first_await = open.find(".await").unwrap();
    assert!(watchers < owner && owner < first_await);
    let ready = open
        .split_once("Ok(Ok(OpenReady")
        .map(|(_, body)| body)
        .unwrap();
    let watcher_join = ready.find("guard.await_watchers().await").unwrap();
    let owner_transfer = ready.find("guard.disarm()").unwrap();
    assert!(watcher_join < owner_transfer);
    assert_eq!(source.matches("close_unstarted().await").count(), 1);
    let cleanup_task = source
        .split_once("async fn cleanup_shared_payload")
        .map(|(_, body)| body)
        .unwrap();
    assert!(cleanup_task.contains("close_unstarted().await"));
    let watcher = source
        .split_once("async fn watch_unclaimed_payload")
        .and_then(|(_, rest)| rest.split_once("async fn cleanup_shared_payload"))
        .map(|(body, _)| body)
        .unwrap();
    assert!(watcher.contains("if !current_runtime_has_timer()"));
    let shutdown = source
        .split_once("pub async fn shutdown")
        .and_then(|(_, rest)| rest.split_once("async fn open("))
        .map(|(body, _)| body)
        .unwrap();
    let shutdown = shutdown.split_whitespace().collect::<Vec<_>>().join("");
    let construct_error = shutdown
        .split_once("lettimeout=matchconstruct_shutdown_timeout(")
        .and_then(|(_, rest)| rest.split_once("matchtimeout.await{"))
        .map(|(constructor_match, _)| constructor_match)
        .and_then(|constructor_match| constructor_match.split_once("Err(())=>{"))
        .map(|(_, error_arm)| error_arm)
        .unwrap();
    let abort = construct_error.find("task.abort()").unwrap();
    let await_abort = construct_error.find("let_=task.await").unwrap();
    let terminated = construct_error
        .find("returnErr(SessionShutdownError::actor_terminated())")
        .unwrap();
    assert!(abort < await_abort && await_abort < terminated);

    let timeout_error = shutdown
        .split_once("matchtimeout.await{")
        .and_then(|(_, timeout_match)| timeout_match.split_once("Err(_)=>{"))
        .map(|(_, error_arm)| error_arm)
        .unwrap();
    let abort = timeout_error.find("task.abort()").unwrap();
    let await_abort = timeout_error.find("let_=task.await").unwrap();
    let timeout = timeout_error
        .find("Err(SessionShutdownError::timeout())")
        .unwrap();
    assert!(abort < await_abort && await_abort < timeout);
    let timeout_constructor = source
        .split_once("fn construct_shutdown_timeout")
        .and_then(|(_, rest)| rest.split_once("pub(super) fn map_actor_exit"))
        .map(|(body, _)| body)
        .unwrap();
    assert!(timeout_constructor.contains("let _entered = runtime.enter()"));
    assert!(timeout_constructor.contains("tokio::time::timeout(timeout, task)"));
    assert!(!timeout_constructor.contains(".await"));
    let guard_drop = source
        .split_once("impl Drop for OpenGuard")
        .and_then(|(_, rest)| rest.split_once("fn spawn_owner"))
        .map(|(body, _)| body)
        .unwrap();
    assert!(guard_drop.contains("self.owner_cancel.cancel()"));
    for forbidden in ["spawn", ".take()", "block", ".await", "forget"] {
        assert!(!guard_drop.contains(forbidden));
    }
    let lib = include_str!("../../lib.rs");
    assert!(!lib.contains("mod runtime;"));
    assert!(
        !std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/runtime")
            .exists()
    );
}

#[test]
fn actor_exit_mapping_keeps_open_failure_distinct_from_successful_close() {
    assert!(map_actor_exit(SessionActorExit::Closed).is_ok());
    assert!(matches!(
        map_actor_exit(SessionActorExit::OpenFailed),
        Err(SessionShutdownError::ActorTerminated(_))
    ));
    assert!(matches!(
        map_actor_exit(SessionActorExit::Panicked),
        Err(SessionShutdownError::ActorTerminated(_))
    ));
}

struct CleanupModel(ModelDescriptor);

impl Model for CleanupModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.0
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        panic!("cleanup must not start the model")
    }
}

struct CleanupLog(Arc<AtomicUsize>);

impl SessionLog for CleanupLog {
    fn initialize<'a>(&'a mut self, _manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        panic!("cleanup must not initialize the log")
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        panic!("cleanup must not load the manifest")
    }

    fn read_page<'a>(
        &'a mut self,
        _after: Option<ConversationSeq>,
        _limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        panic!("cleanup must not replay the log")
    }

    fn append<'a>(
        &'a mut self,
        _expected_head: ConversationSeq,
        _entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        panic!("cleanup must not append the log")
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

fn cleanup_payload(close_count: Arc<AtomicUsize>, task_runtime: Handle) -> SharedOpenPayload {
    let spec = SessionSpec::new(
        "host:model".parse().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    let model: Arc<dyn Model> = Arc::new(CleanupModel(ModelDescriptor {
        model_ref: spec.model.clone(),
        context_window: 1,
        supported_reasoning: BTreeSet::from([ReasoningPreference::Auto]),
        supports_tools: false,
    }));
    let bindings =
        SessionBindings::new(model, ToolSet::builder().build().unwrap(), None, None, None);
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        bindings,
        task_runtime,
    )
    .unwrap();
    OpenPayload::shared(
        OpenRequest::Create {
            session_id: "ses_00000000000000000000000000000001".parse().unwrap(),
            spec,
        },
        Box::new(CleanupLog(close_count)),
        options,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn cleanup_task_panic_before_payload_take_leaves_fallback_close_available() {
    let task_runtime = Handle::current();
    let close_count = Arc::new(AtomicUsize::new(0));
    let payload = cleanup_payload(Arc::clone(&close_count), task_runtime.clone());
    script_payload_panic(&payload, PayloadPanicPoint::CleanupBeforeTake);
    let cancel = CancellationToken::new();
    let payload_claimed = CancellationToken::new();
    let mut guard = OpenGuard::new(cancel, &task_runtime, &payload, &payload_claimed);
    guard.cancel();
    let error = guard.await_watchers().await.unwrap();
    assert_eq!(
        error.kind(),
        crate::error::SessionOpenErrorKind::ActorStartFailed
    );
    assert_eq!(close_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn owner_spawn_panic_with_payload_is_closed_once_by_existing_watcher() {
    let task_runtime = Handle::current();
    let close_count = Arc::new(AtomicUsize::new(0));
    let payload = cleanup_payload(Arc::clone(&close_count), task_runtime.clone());
    script_payload_panic(&payload, PayloadPanicPoint::OwnerSpawn);
    let cancel = CancellationToken::new();
    let payload_claimed = CancellationToken::new();
    let mut guard = OpenGuard::new(cancel.clone(), &task_runtime, &payload, &payload_claimed);
    let (ready, _receiver) = oneshot::channel();
    assert!(
        spawn_owner(
            &task_runtime,
            Arc::clone(&payload),
            cancel,
            payload_claimed,
            ready,
        )
        .is_err()
    );
    guard.cancel();
    let error = guard.await_watchers().await.unwrap();
    assert_eq!(
        error.kind(),
        crate::error::SessionOpenErrorKind::ActorStartFailed
    );
    assert_eq!(close_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn payload_claimed_ready_path_joins_every_watcher() {
    let task_runtime = Handle::current();
    let close_count = Arc::new(AtomicUsize::new(0));
    let payload = cleanup_payload(Arc::clone(&close_count), task_runtime.clone());
    let cancel = CancellationToken::new();
    let payload_claimed = CancellationToken::new();
    let mut guard = OpenGuard::new(cancel, &task_runtime, &payload, &payload_claimed);
    assert_eq!(guard.cleanup_watchers.len(), 2);
    let claimed = take_shared_payload(&payload).unwrap();
    payload_claimed.cancel();
    assert!(guard.await_watchers().await.is_none());
    assert!(guard.cleanup_watchers.is_empty());
    assert_eq!(close_count.load(Ordering::SeqCst), 0);
    let claimed = Arc::new(std::sync::Mutex::new(Some(claimed)));
    assert!(cleanup_shared_payload(claimed).await.is_some());
    assert_eq!(close_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_while_join_is_pending_leaves_owner_task_to_self_clean() {
    let cancel = CancellationToken::new();
    let completed = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let task_completed = std::sync::Arc::clone(&completed);
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        task_cancel.cancelled().await;
        task_completed.add_permits(1);
        SessionActorExit::OpenFailed
    });
    let payload = std::sync::Arc::new(std::sync::Mutex::new(None));
    let runtime = Handle::current();
    let payload_claimed = CancellationToken::new();
    let mut guard = OpenGuard::new(cancel, &runtime, &payload, &payload_claimed);
    guard.set_owner_task(task);
    {
        let join = guard.join_owner();
        tokio::pin!(join);
        assert!(matches!(futures_util::poll!(join.as_mut()), Poll::Pending));
    }
    drop(guard);
    let permit = completed.acquire_owned().await.unwrap();
    permit.forget();
}

#[test]
fn no_time_current_watcher_never_claims_payload_from_configured_watcher() {
    let configured = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .unwrap();
    let configured_handle = configured.handle().clone();
    let (started, started_rx) = std::sync::mpsc::sync_channel(0);
    let (release, release_rx) = std::sync::mpsc::sync_channel(0);
    let blocker = configured_handle.spawn(async move {
        started.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    started_rx.recv().unwrap();

    let close_count = Arc::new(AtomicUsize::new(0));
    let payload = cleanup_payload(Arc::clone(&close_count), configured_handle.clone());
    let no_time = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = no_time.block_on(async {
        let cancel = CancellationToken::new();
        let payload_claimed = CancellationToken::new();
        let mut guard = OpenGuard::new(cancel, &configured_handle, &payload, &payload_claimed);
        assert_eq!(guard.cleanup_watchers.len(), 2);
        guard.cancel();
        let current_watcher = guard.cleanup_watchers.pop().unwrap();
        assert!(matches!(current_watcher.await, Ok(None)));
        assert!(
            payload
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
        );
        release.send(()).unwrap();
        guard.await_watchers().await.unwrap()
    });
    assert_eq!(
        error.kind(),
        crate::error::SessionOpenErrorKind::ActorStartFailed
    );
    assert_eq!(close_count.load(Ordering::SeqCst), 1);
    configured.block_on(blocker).unwrap();
}
