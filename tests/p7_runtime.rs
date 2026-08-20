use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use minicore_runtime::model::{
    AssistantPart, ModelCallContext, ModelDescriptor, ModelFinishReason, ModelFuture, ModelLimits,
    ModelProvider, ModelRequest, ModelResponse, ModelSelection, ProviderId, ProviderRegistry,
    ReasoningPreference, ToolCall,
};
use minicore_runtime::tools::{
    AskUserTool, Tool, ToolContext, ToolError, ToolFuture, ToolName, ToolOutput, ToolRegistry,
    ToolSpec,
};
use minicore_runtime::{
    RetryPolicy, Runtime, RuntimeConfig, SessionConfig, SessionEvent, SessionEventStream,
    SessionId, SessionStatus, TurnOutcome,
};
use tokio::runtime::Handle;

struct EchoProvider {
    id: ProviderId,
    descriptor: ModelDescriptor,
    responses: Arc<Mutex<VecDeque<ModelResponse>>>,
}

struct EchoTool {
    name: ToolName,
}

struct UncooperativeTool {
    name: ToolName,
    started: Arc<AtomicUsize>,
    cancellations: Arc<AtomicUsize>,
}

impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.name.clone(),
            "echoes input",
            serde_json::json!({"type": "object"}),
        )
        .unwrap()
    }

    fn execute<'a>(&'a self, _ctx: ToolContext<'a>, args: serde_json::Value) -> ToolFuture<'a> {
        Box::pin(
            async move { ToolOutput::success(args.to_string()).map_err(|_| ToolError::Internal) },
        )
    }
}

impl Tool for UncooperativeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name.clone(), "uncooperative", serde_json::json!({})).unwrap()
    }

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, _args: serde_json::Value) -> ToolFuture<'a> {
        let cancellation = ctx.cancellation().clone();
        let started = Arc::clone(&self.started);
        let cancellations = Arc::clone(&self.cancellations);
        Box::pin(async move {
            started.fetch_add(1, Ordering::SeqCst);
            cancellation.cancelled().await;
            cancellations.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<ToolOutput, ToolError>>().await
        })
    }
}

impl ModelProvider for EchoProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn models(&self) -> &[ModelDescriptor] {
        std::slice::from_ref(&self.descriptor)
    }

    fn generate(&self, _request: ModelRequest, _ctx: ModelCallContext) -> ModelFuture<'_> {
        let response = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or_else(|| {
                ModelResponse::new(
                    vec![AssistantPart::Text("echo response".to_owned())],
                    ModelFinishReason::Stop,
                    None,
                )
                .unwrap()
            });
        Box::pin(async move { Ok(response) })
    }
}

fn paths(label: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("minicore-p7-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    (root, workspace)
}

fn selection() -> ModelSelection {
    ModelSelection::new("echo".parse().unwrap(), "echo-model".parse().unwrap())
}

fn text_response(text: &str) -> ModelResponse {
    ModelResponse::new(
        vec![AssistantPart::Text(text.to_owned())],
        ModelFinishReason::Stop,
        None,
    )
    .unwrap()
}

fn tool_response(name: &str, arguments: serde_json::Value) -> ModelResponse {
    let call = ToolCall::new(
        minicore_runtime::ToolCallId::new("call-0").unwrap(),
        name.parse().unwrap(),
        arguments,
        0,
    )
    .unwrap();
    ModelResponse::new(
        vec![AssistantPart::ToolCall(call)],
        ModelFinishReason::ToolCalls,
        None,
    )
    .unwrap()
}

fn provider_registry() -> ProviderRegistry {
    provider_registry_with(VecDeque::from([text_response("echo response")]))
}

fn provider_registry_with(responses: VecDeque<ModelResponse>) -> ProviderRegistry {
    let selection = selection();
    let descriptor = ModelDescriptor::new(
        selection,
        "echo-api-model",
        ModelLimits::default(),
        BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
    )
    .unwrap();
    let provider = EchoProvider {
        id: "echo".parse().unwrap(),
        descriptor,
        responses: Arc::new(Mutex::new(responses)),
    };
    let mut builder = ProviderRegistry::builder();
    builder.register(provider).unwrap();
    builder.build()
}

fn runtime_config(root: &Path) -> RuntimeConfig {
    runtime_config_with(root, provider_registry(), ToolRegistry::default())
}

fn runtime_config_with(
    root: &Path,
    providers: ProviderRegistry,
    tools: ToolRegistry,
) -> RuntimeConfig {
    RuntimeConfig::new(
        root.to_path_buf(),
        providers,
        tools,
        "coding",
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    )
    .unwrap()
}

fn session_config(workspace: &Path) -> SessionConfig {
    SessionConfig::new(
        workspace.to_path_buf(),
        selection(),
        "system",
        BTreeSet::new(),
        1_000,
        999,
        4,
    )
    .unwrap()
}

async fn wait_finished(stream: &mut SessionEventStream) -> TurnOutcome {
    loop {
        if let SessionEvent::TurnFinished { outcome, .. } = stream.recv().await.unwrap() {
            return outcome;
        }
    }
}

async fn blocked_runtime(
    label: &str,
) -> (
    Runtime,
    PathBuf,
    Vec<SessionId>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let (root, workspace) = paths(label);
    let workspace_two = root.join("workspace-two");
    fs::create_dir_all(&workspace_two).unwrap();
    let started = Arc::new(AtomicUsize::new(0));
    let cancellations = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::builder();
    tools
        .register(UncooperativeTool {
            name: "block".parse().unwrap(),
            started: Arc::clone(&started),
            cancellations: Arc::clone(&cancellations),
        })
        .unwrap();
    let config = RuntimeConfig::builder(
        root.clone(),
        provider_registry_with(VecDeque::from([
            tool_response("block", serde_json::json!({})),
            tool_response("block", serde_json::json!({})),
        ])),
        tools.build(),
        "coding",
        RetryPolicy::new(1, Duration::ZERO).unwrap(),
    )
    .shutdown_timeout(Duration::from_millis(100))
    .build()
    .unwrap();
    let runtime = Runtime::open(config, Handle::current()).await.unwrap();
    let tool: ToolName = "block".parse().unwrap();
    let mut ids = Vec::new();
    for workspace_root in [workspace, workspace_two] {
        let id = runtime
            .create_session(
                SessionConfig::new(
                    workspace_root,
                    selection(),
                    "system",
                    BTreeSet::from([tool.clone()]),
                    1_000,
                    999,
                    4,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        runtime.submit(id, "block".to_owned()).await.unwrap();
        ids.push(id);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if started.load(Ordering::SeqCst) == ids.len() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    (runtime, root, ids, started, cancellations)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_create_submit_transcript_close_reload_delete_and_reopen() {
    let (root, workspace) = paths("lifecycle");
    let runtime = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    assert!(matches!(
        Runtime::open(runtime_config(&root), Handle::current()).await,
        Err(minicore_runtime::RuntimeError::InvalidConfiguration)
    ));
    let id = runtime
        .create_session(session_config(&workspace))
        .await
        .unwrap();
    let mut events = runtime.subscribe(id).unwrap();
    assert!(
        matches!(events.recv().await, Some(SessionEvent::Snapshot(snapshot)) if snapshot.status() == SessionStatus::Idle)
    );
    let turn_id = runtime.submit(id, "hello".to_owned()).await.unwrap();
    let outcome = wait_finished(&mut events).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    let snapshot = runtime.snapshot(id).unwrap();
    assert_eq!(snapshot.active_turn(), None);
    assert!(snapshot.conversation_seq() >= 3);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert_eq!(page.entries().len(), 3);
    assert!(matches!(
        &page.entries()[0],
        minicore_runtime::TranscriptEntry::User { turn_id: current, text, .. }
            if *current == turn_id && text == "hello"
    ));
    assert!(matches!(
        &page.entries()[1],
        minicore_runtime::TranscriptEntry::Assistant { text: Some(text), tool_calls, .. }
            if text == "echo response" && tool_calls.is_empty()
    ));
    assert!(matches!(
        &page.entries()[2],
        minicore_runtime::TranscriptEntry::Terminal {
            outcome: TurnOutcome::Completed,
            ..
        }
    ));
    assert_eq!(runtime.close_session(id).await, Ok(()));
    assert_eq!(
        runtime.snapshot(id),
        Err(minicore_runtime::SessionError::NotFound)
    );
    let unloaded = runtime.transcript(id, Some(1), 2).await.unwrap();
    assert_eq!(unloaded.entries().len(), 2);
    assert!(
        runtime
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .all(|summary| !summary.loaded)
    );
    runtime.load_session(id).await.unwrap();
    assert!(
        runtime
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|summary| summary.session_id == id && summary.loaded)
    );
    runtime.close_session(id).await.unwrap();
    runtime.delete_session(id).await.unwrap();
    assert!(runtime.list_sessions().await.unwrap().is_empty());
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let reopened = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    assert!(reopened.list_sessions().await.unwrap().is_empty());
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_unpolled_shutdown_starts_runtime_cleanup() {
    let (root, _workspace) = paths("unpolled-shutdown");
    let runtime = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    let shutdown = runtime.shutdown();
    drop(shutdown);
    drop(runtime);

    let mut reopened = None;
    for _ in 0..1_000 {
        match Runtime::open(runtime_config(&root), Handle::current()).await {
            Ok(value) => {
                reopened = Some(value);
                break;
            }
            Err(minicore_runtime::RuntimeError::InvalidConfiguration) => {
                tokio::task::yield_now().await;
            }
            Err(error) => panic!("unexpected runtime reopen error: {error:?}"),
        }
    }
    let reopened = reopened.expect("unpolled shutdown did not release the root lock");
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_runtime_shutdown_is_an_immediate_reopen_barrier() {
    for index in 0..20 {
        let (root, workspace) = paths(&format!("shutdown-barrier-{index}"));
        let runtime = Runtime::open(runtime_config(&root), Handle::current())
            .await
            .unwrap();
        runtime
            .create_session(session_config(&workspace))
            .await
            .unwrap();
        runtime.shutdown().await.unwrap();
        drop(runtime);

        let reopened = Runtime::open(runtime_config(&root), Handle::current())
            .await
            .unwrap();
        reopened.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_non_last_runtime_clone_does_not_request_shutdown() {
    let (root, workspace) = paths("clone");
    let runtime = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    let retained = runtime.clone();
    let id = runtime
        .create_session(session_config(&workspace))
        .await
        .unwrap();
    drop(runtime);
    retained.submit(id, "still open".to_owned()).await.unwrap();
    retained.close_session(id).await.unwrap();
    retained.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn create_failure_after_store_commit_leaves_unloaded_durable_session() {
    let (root, _workspace) = paths("create-failure");
    let runtime = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    let missing_workspace = root.join("missing-workspace");
    let result = runtime
        .create_session(session_config(&missing_workspace))
        .await;
    assert_eq!(result, Err(minicore_runtime::SessionError::Unavailable));
    let sessions = runtime.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(!sessions[0].loaded);
    runtime
        .delete_session(sessions[0].session_id)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_rejects_bad_config_and_strict_transcript_pages() {
    let (root, workspace) = paths("bounds");
    assert!(
        RuntimeConfig::new(
            root.join("."),
            ProviderRegistry::default(),
            ToolRegistry::default(),
            "coding",
            RetryPolicy::new(1, Duration::ZERO).unwrap(),
        )
        .is_err()
    );
    let runtime = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    let id = runtime
        .create_session(session_config(&workspace))
        .await
        .unwrap();
    assert_eq!(
        runtime.transcript(id, None, 0).await,
        Err(minicore_runtime::SessionError::InvalidInput)
    );
    assert_eq!(
        runtime.transcript(id, None, 201).await,
        Err(minicore_runtime::SessionError::InvalidInput)
    );
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_load_reservation_and_loaded_delete_are_typed() {
    let (root, workspace) = paths("load-race");
    let runtime = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    let id = runtime
        .create_session(session_config(&workspace))
        .await
        .unwrap();
    assert_eq!(
        runtime.load_session(id).await,
        Err(minicore_runtime::SessionError::AlreadyLoaded)
    );
    runtime.close_session(id).await.unwrap();
    let (first, second) = tokio::join!(runtime.load_session(id), runtime.load_session(id));
    assert!(matches!(
        (first, second),
        (
            Ok(()),
            Err(minicore_runtime::SessionError::Busy
                | minicore_runtime::SessionError::AlreadyLoaded)
        ) | (
            Err(minicore_runtime::SessionError::Busy
                | minicore_runtime::SessionError::AlreadyLoaded),
            Ok(())
        )
    ));
    assert_eq!(
        runtime.delete_session(id).await,
        Err(minicore_runtime::SessionError::Busy)
    );
    runtime.close_session(id).await.unwrap();
    runtime.delete_session(id).await.unwrap();
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_close_and_shutdown_share_completion_and_remove_session() {
    let (root, workspace) = paths("close");
    let runtime = Arc::new(
        Runtime::open(runtime_config(&root), Handle::current())
            .await
            .unwrap(),
    );
    let id = runtime
        .create_session(session_config(&workspace))
        .await
        .unwrap();
    let first = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.close_session(id).await })
    };
    let second = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.shutdown().await })
    };
    assert_eq!(first.await.unwrap(), Ok(()));
    assert_eq!(second.await.unwrap(), Ok(()));
    assert_eq!(runtime.shutdown().await, Ok(()));
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_runs_custom_echo_tool_and_projects_safe_tool_transcript() {
    let (root, workspace) = paths("echo-tool");
    let mut tools = ToolRegistry::builder();
    tools
        .register(EchoTool {
            name: "echo".parse().unwrap(),
        })
        .unwrap();
    let runtime = Runtime::open(
        runtime_config_with(
            &root,
            provider_registry_with(VecDeque::from([
                tool_response("echo", serde_json::json!({"value": "hello"})),
                text_response("done"),
            ])),
            tools.build(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(
            SessionConfig::new(
                workspace.clone(),
                selection(),
                "system",
                BTreeSet::from(["echo".parse().unwrap()]),
                1_000,
                999,
                4,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let mut events = runtime.subscribe(id).unwrap();
    let _ = events.recv().await;
    runtime.submit(id, "run echo".to_owned()).await.unwrap();
    assert!(matches!(
        events.recv().await,
        Some(SessionEvent::TurnStarted { .. })
    ));
    let mut saw_tool = false;
    loop {
        match events.recv().await.unwrap() {
            SessionEvent::ToolStarted { .. } | SessionEvent::ToolFinished { .. } => saw_tool = true,
            SessionEvent::TurnFinished {
                outcome: TurnOutcome::Completed,
                ..
            } => break,
            _ => {}
        }
    }
    assert!(saw_tool);
    let page = runtime.transcript(id, None, 200).await.unwrap();
    assert!(page.entries().iter().any(|entry| matches!(
        entry,
        minicore_runtime::TranscriptEntry::ToolResult { text, .. }
            if text.contains("hello")
    )));
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_ask_user_wrong_answer_preserves_pending_then_correct_resumes() {
    let (root, workspace) = paths("ask-user");
    let mut tools = ToolRegistry::builder();
    tools.register(AskUserTool).unwrap();
    let runtime = Runtime::open(
        runtime_config_with(
            &root,
            provider_registry_with(VecDeque::from([
                tool_response("ask_user", serde_json::json!({"question": "Allow?"})),
                text_response("approved"),
            ])),
            tools.build(),
        ),
        Handle::current(),
    )
    .await
    .unwrap();
    let id = runtime
        .create_session(
            SessionConfig::new(
                workspace,
                selection(),
                "system",
                BTreeSet::from(["ask_user".parse().unwrap()]),
                1_000,
                999,
                4,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let mut events = runtime.subscribe(id).unwrap();
    let _ = events.recv().await;
    runtime.submit(id, "ask".to_owned()).await.unwrap();
    let interaction_id = loop {
        if let SessionEvent::InputRequested { question, .. } = events.recv().await.unwrap() {
            break question.interaction_id();
        }
    };
    assert_eq!(
        runtime
            .answer(
                id,
                minicore_runtime::InteractionId::new().unwrap(),
                minicore_runtime::tools::UserAnswer::new("allow").unwrap(),
            )
            .await,
        Err(minicore_runtime::SessionError::InteractionMismatch)
    );
    runtime
        .answer(
            id,
            interaction_id,
            minicore_runtime::tools::UserAnswer::new("allow").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wait_finished(&mut events).await, TurnOutcome::Completed);
    runtime.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_close_waiter_does_not_lose_manager_join_owner() {
    let (runtime, root, ids, _, cancellations) = blocked_runtime("dropped-close").await;
    tokio::time::pause();
    let id = ids[0];
    let first_runtime = runtime.clone();
    let first = tokio::spawn(async move { first_runtime.close_session(id).await });
    for _ in 0..1_000 {
        if cancellations.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(cancellations.load(Ordering::SeqCst) > 0);
    first.abort();
    let _ = first.await;
    tokio::time::advance(Duration::from_millis(101)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(runtime.close_session(id).await, Ok(()));
    runtime.load_session(id).await.unwrap();
    runtime.close_session(id).await.unwrap();
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let reopened = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    assert!(
        reopened
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|summary| { summary.session_id == ids[1] && !summary.loaded })
    );
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_shutdown_waiter_does_not_poison_retained_cleanup() {
    let (runtime, root, ids, _, cancellations) = blocked_runtime("dropped-shutdown").await;
    tokio::time::pause();
    let first_runtime = runtime.clone();
    let first = tokio::spawn(async move { first_runtime.shutdown().await });
    for _ in 0..1_000 {
        if cancellations.load(Ordering::SeqCst) == ids.len() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(cancellations.load(Ordering::SeqCst), ids.len());
    first.abort();
    let _ = first.await;
    tokio::time::advance(Duration::from_millis(101)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let second = tokio::spawn(async move { runtime.shutdown().await });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(second.is_finished());
    assert_eq!(second.await.unwrap(), Ok(()));
    let reopened = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    let sessions = reopened.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), ids.len());
    assert!(sessions.iter().all(|summary| !summary.loaded));
    for summary in sessions {
        reopened.delete_session(summary.session_id).await.unwrap();
    }
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_signals_all_sessions_before_any_close_drain() {
    let (runtime, root, ids, _, cancellations) = blocked_runtime("signal-all").await;
    tokio::time::pause();
    let shutdown = tokio::spawn(async move { runtime.shutdown().await });
    for _ in 0..1_000 {
        if cancellations.load(Ordering::SeqCst) == ids.len() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(cancellations.load(Ordering::SeqCst), ids.len());
    tokio::time::advance(Duration::from_millis(101)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(shutdown.is_finished());
    assert_eq!(shutdown.await.unwrap(), Ok(()));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn closing_rejects_load_delete_list_and_create() {
    let (runtime, root, ids, _, cancellations) = blocked_runtime("closing-admission").await;
    tokio::time::pause();
    let shutdown = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.shutdown().await }
    });
    for _ in 0..1_000 {
        if cancellations.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        runtime.load_session(ids[0]).await,
        Err(minicore_runtime::SessionError::Closing)
    );
    assert_eq!(
        runtime.delete_session(ids[0]).await,
        Err(minicore_runtime::SessionError::Closing)
    );
    assert_eq!(
        runtime.list_sessions().await,
        Err(minicore_runtime::SessionError::Closing)
    );
    let workspace = root.join("late-workspace");
    fs::create_dir_all(&workspace).unwrap();
    assert_eq!(
        runtime.create_session(session_config(&workspace)).await,
        Err(minicore_runtime::SessionError::Closing)
    );
    tokio::time::advance(Duration::from_millis(101)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(shutdown.await.unwrap(), Ok(()));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_and_create_closing_races_retain_or_reject_without_live_actors() {
    let (root, workspace) = paths("admission-race");
    let second_workspace = root.join("second-workspace");
    fs::create_dir_all(&second_workspace).unwrap();
    let runtime = Arc::new(
        Runtime::open(runtime_config(&root), Handle::current())
            .await
            .unwrap(),
    );
    let id = runtime
        .create_session(session_config(&workspace))
        .await
        .unwrap();
    runtime.close_session(id).await.unwrap();
    let load_runtime = Arc::clone(&runtime);
    let load = tokio::spawn(async move { load_runtime.load_session(id).await });
    let create_runtime = Arc::clone(&runtime);
    let create = tokio::spawn(async move {
        create_runtime
            .create_session(session_config(&second_workspace))
            .await
    });
    let shutdown_runtime = Arc::clone(&runtime);
    let shutdown = tokio::spawn(async move { shutdown_runtime.shutdown().await });
    let load_result = load.await.unwrap();
    let create_result = create.await.unwrap();
    assert!(matches!(
        load_result,
        Ok(()) | Err(minicore_runtime::SessionError::Closing)
    ));
    assert!(matches!(
        create_result,
        Ok(_) | Err(minicore_runtime::SessionError::Closing)
    ));
    assert_eq!(shutdown.await.unwrap(), Ok(()));
    drop(runtime);
    let reopened = Runtime::open(runtime_config(&root), Handle::current())
        .await
        .unwrap();
    assert!(
        reopened
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .all(|summary| !summary.loaded)
    );
    reopened.shutdown().await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_shutdown_callers_receive_the_same_result() {
    let (root, workspace) = paths("shutdown-same-result");
    let runtime = Arc::new(
        Runtime::open(runtime_config(&root), Handle::current())
            .await
            .unwrap(),
    );
    runtime
        .create_session(session_config(&workspace))
        .await
        .unwrap();
    let first = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.shutdown().await })
    };
    let second = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.shutdown().await })
    };
    let first_result = first.await.unwrap();
    let second_result = second.await.unwrap();
    assert_eq!(first_result, second_result);
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn checked_config_builder_enforces_timeout_and_capacity_bounds() {
    let root = PathBuf::from("/tmp/minicore-p7-config-bounds");
    let retry = RetryPolicy::new(1, Duration::ZERO).unwrap();
    assert!(
        RuntimeConfig::builder(
            root.clone(),
            ProviderRegistry::default(),
            ToolRegistry::default(),
            "coding",
            retry,
        )
        .shutdown_timeout(Duration::ZERO)
        .build()
        .is_err()
    );
    assert!(
        RuntimeConfig::builder(
            root,
            ProviderRegistry::default(),
            ToolRegistry::default(),
            "coding",
            retry,
        )
        .capacities(0, 64, 64)
        .build()
        .is_err()
    );
    assert!(
        SessionConfig::new(
            PathBuf::from("/tmp/minicore-p7-workspace-bounds"),
            selection(),
            "system",
            BTreeSet::new(),
            10,
            10,
            4,
        )
        .is_err()
    );
}

#[test]
fn public_summary_and_transcript_do_not_contain_runtime_paths_or_prompts() {
    let summary = minicore_runtime::SessionSummary {
        session_id: SessionId::new().unwrap(),
        model: selection(),
        loaded: false,
    };
    let encoded = serde_json::to_string(&summary).unwrap();
    assert!(!encoded.contains("workspace"));
    assert!(!encoded.contains("system"));
}
