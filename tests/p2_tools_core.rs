use std::collections::BTreeSet;
use std::fs;
use std::future::{Future, poll_fn};
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use minicore_runtime::{
    AllowConfiguredTools, InteractionClient, InteractionId, InteractionReceiver,
    InteractionRequest, SessionId, Tool, ToolCallId, ToolContext, ToolContextView, ToolDecision,
    ToolError, ToolFuture, ToolName, ToolOutput, ToolPolicy, ToolRegistry, ToolRegistryBuilder,
    ToolRequest, ToolSpec, TurnId, UserAnswer, Workspace, WorkspaceAccess,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn echo_spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        ToolName::from_str(name).unwrap(),
        description,
        json!({"type": "object"}),
    )
    .unwrap()
}

struct EchoTool {
    spec: ToolSpec,
    spec_calls: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
}

impl EchoTool {
    fn new(name: &str, description: &str) -> Self {
        Self {
            spec: echo_spec(name, description),
            spec_calls: Arc::new(AtomicUsize::new(0)),
            executions: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        self.spec_calls.fetch_add(1, Ordering::SeqCst);
        self.spec.clone()
    }

    fn execute<'a>(&'a self, _ctx: ToolContext<'a>, args: serde_json::Value) -> ToolFuture<'a> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Box::pin(
            async move { ToolOutput::success(args.to_string()).map_err(|_| ToolError::Internal) },
        )
    }
}

struct MutableSpecTool {
    name: ToolName,
    version: Arc<AtomicUsize>,
    spec_calls: Arc<AtomicUsize>,
}

impl MutableSpecTool {
    fn new(name: &str) -> Self {
        Self {
            name: ToolName::from_str(name).unwrap(),
            version: Arc::new(AtomicUsize::new(1)),
            spec_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Tool for MutableSpecTool {
    fn spec(&self) -> ToolSpec {
        self.spec_calls.fetch_add(1, Ordering::SeqCst);
        let version = self.version.load(Ordering::SeqCst);
        ToolSpec::new(
            self.name.clone(),
            format!("version-{version}"),
            json!({"type": "object"}),
        )
        .unwrap()
    }

    fn execute<'a>(&'a self, _ctx: ToolContext<'a>, _args: serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async { Err(ToolError::Internal) })
    }
}

struct PanicSpecTool;

impl Tool for PanicSpecTool {
    fn spec(&self) -> ToolSpec {
        panic!("private spec payload must not escape")
    }

    fn execute<'a>(&'a self, _ctx: ToolContext<'a>, _args: serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async { Err(ToolError::Internal) })
    }
}

fn workspace(label: &str) -> (PathBuf, Workspace) {
    let root =
        std::env::temp_dir().join(format!("minicore-p2-tools-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
    (root, workspace)
}

fn enabled(name: &ToolName) -> BTreeSet<ToolName> {
    BTreeSet::from([name.clone()])
}

async fn poll_until_pending<F>(future: &mut Pin<Box<F>>)
where
    F: Future,
{
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("ask_user completed before the request was handled"),
    })
    .await;
}

async fn poll_until_ready<F>(future: &mut Pin<Box<F>>) -> F::Output
where
    F: Future,
{
    poll_fn(|cx| future.as_mut().poll(cx)).await
}

#[test]
fn registry_registers_dynamic_tools_freezes_specs_and_returns_sorted_specs() {
    let mutable = MutableSpecTool::new("z_tool");
    let spec_calls = Arc::clone(&mutable.spec_calls);
    let version = Arc::clone(&mutable.version);
    let mut builder = ToolRegistry::builder();
    builder.register(mutable).unwrap();
    assert_eq!(spec_calls.load(Ordering::SeqCst), 1);
    version.store(2, Ordering::SeqCst);

    builder.register(EchoTool::new("a_tool", "first")).unwrap();
    let registry = builder.build();
    let enabled = BTreeSet::from([
        ToolName::from_str("z_tool").unwrap(),
        ToolName::from_str("a_tool").unwrap(),
    ]);
    let specs = registry.specs(&enabled).unwrap();
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name().as_str())
            .collect::<Vec<_>>(),
        vec!["a_tool", "z_tool"]
    );
    assert_eq!(specs[1].description(), "version-1");
    assert_eq!(spec_calls.load(Ordering::SeqCst), 1);
    assert!(
        registry
            .get(&ToolName::from_str("a_tool").unwrap())
            .is_some()
    );
}

#[test]
fn registry_rejects_duplicate_panic_and_unknown_tools_and_allows_empty_default() {
    let mut builder = ToolRegistry::builder();
    builder
        .register(EchoTool::new("same_tool", "first"))
        .unwrap();
    assert_eq!(
        builder
            .register(EchoTool::new("same_tool", "second"))
            .unwrap_err(),
        ToolError::DuplicateTool
    );
    assert_eq!(
        ToolRegistryBuilder::default()
            .register(PanicSpecTool)
            .unwrap_err(),
        ToolError::Panicked
    );

    let registry = ToolRegistry::default();
    assert!(
        registry
            .get(&ToolName::from_str("missing").unwrap())
            .is_none()
    );
    let unknown = BTreeSet::from([ToolName::from_str("missing").unwrap()]);
    assert_eq!(
        registry.specs(&unknown).unwrap_err(),
        ToolError::UnknownTool
    );
}

#[tokio::test]
async fn registered_tool_executes_with_context_and_registry_clones_are_concurrent_safe() {
    let (root, workspace) = workspace("execute");
    let echo = EchoTool::new("echo", "echo args");
    let executions = Arc::clone(&echo.executions);
    let mut builder = ToolRegistry::builder();
    builder.register(echo).unwrap();
    let registry = builder.build();
    let registry_a = registry.clone();
    let registry_b = registry.clone();
    let name = ToolName::from_str("echo").unwrap();
    let thread_a = std::thread::spawn(move || registry_a.get(&name).is_some());
    let name = ToolName::from_str("echo").unwrap();
    let thread_b = std::thread::spawn(move || registry_b.get(&name).is_some());
    assert!(thread_a.join().unwrap());
    assert!(thread_b.join().unwrap());

    let (interactions, _receiver) = InteractionClient::channel();
    let cancellation = CancellationToken::new();
    let context = ToolContext::new(
        SessionId::new().unwrap(),
        TurnId::new().unwrap(),
        &workspace,
        cancellation.clone(),
        &interactions,
    )
    .unwrap();
    let output = registry
        .get(&ToolName::from_str("echo").unwrap())
        .unwrap()
        .execute(context, json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(output.text(), r#"{"message":"hello"}"#);
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    workspace.shutdown().await.unwrap();
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn allow_configured_policy_and_tool_decisions_are_checked_and_serde_safe() {
    let session_id = SessionId::new().unwrap();
    let turn_id = TurnId::new().unwrap();
    let name = ToolName::from_str("echo").unwrap();
    let call_id = ToolCallId::from_str("call_1!").unwrap();
    let args = json!({"message": "hello"});
    let enabled = enabled(&name);
    let context = ToolContextView::new(session_id, turn_id, &enabled);
    let request = ToolRequest::new(&call_id, &name, &args, 0);
    let policy = AllowConfiguredTools::new();
    assert_eq!(policy.decide(&request, &context), ToolDecision::Allow);

    let disabled_name = ToolName::from_str("other").unwrap();
    let disabled_request = ToolRequest::new(&call_id, &disabled_name, &args, 0);
    let denied = policy.decide(&disabled_request, &context);
    assert!(matches!(denied, ToolDecision::Deny { .. }));
    denied.validate().unwrap();

    let decisions = [
        ToolDecision::Allow,
        ToolDecision::deny("tool is not enabled").unwrap(),
        ToolDecision::ask("Choose a file", Some(vec!["one".into(), "two".into()])).unwrap(),
    ];
    for decision in decisions {
        let json = serde_json::to_string(&decision).unwrap();
        assert_eq!(
            serde_json::from_str::<ToolDecision>(&json).unwrap(),
            decision
        );
    }
    assert!(ToolDecision::deny("").is_err());
    assert!(ToolDecision::ask("", None).is_err());
    assert!(ToolDecision::ask("bad\nquestion", None).is_err());
    assert!(
        serde_json::from_value::<ToolDecision>(json!({
            "decision": "deny",
            "data": {"reason": ""}
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolDecision>(json!({
            "decision": "ask",
            "data": {"question": "q", "choices": [""]}
        }))
        .is_err()
    );
}

#[tokio::test]
async fn interaction_request_has_owner_generated_question_and_answers() {
    let (client, mut receiver) = InteractionClient::channel();
    let cancellation = CancellationToken::new();
    let task_client = client.clone();
    let task = tokio::spawn(async move {
        task_client
            .ask_user(
                TurnId::new().unwrap(),
                "Pick a file",
                Some(vec!["one".into(), "two".into()]),
                cancellation,
            )
            .await
    });
    let request: InteractionRequest = receiver.recv().await.unwrap();
    let interaction_id = InteractionId::new().unwrap();
    let question = request.user_question(interaction_id).unwrap();
    assert_eq!(question.interaction_id(), interaction_id);
    assert_eq!(question.question(), "Pick a file");
    request.respond(UserAnswer::new("one").unwrap()).unwrap();
    assert_eq!(task.await.unwrap().unwrap().text(), "one");
}

#[tokio::test]
async fn interaction_client_reports_busy_cancelled_and_closed_without_custom_service() {
    let (client, mut receiver) = InteractionClient::channel();
    let first_cancel = CancellationToken::new();
    let first_client = client.clone();
    let first_cancellation = first_cancel.clone();
    let first = tokio::spawn(async move {
        first_client
            .ask_user(TurnId::new().unwrap(), "first", None, first_cancellation)
            .await
    });
    let first_request = receiver.recv().await.unwrap();
    assert_eq!(
        client
            .ask_user(
                TurnId::new().unwrap(),
                "second",
                None,
                CancellationToken::new(),
            )
            .await,
        Err(ToolError::InteractionBusy)
    );
    first_cancel.cancel();
    assert_eq!(first.await.unwrap(), Err(ToolError::Cancelled));
    drop(first_request);

    let (closed_client, closed_receiver) = InteractionClient::channel();
    drop(closed_receiver);
    assert_eq!(
        closed_client
            .ask_user(
                TurnId::new().unwrap(),
                "closed",
                None,
                CancellationToken::new(),
            )
            .await,
        Err(ToolError::InteractionClosed)
    );
}

#[tokio::test]
async fn cancellation_before_receiver_dequeue_releases_the_single_slot() {
    let (client, mut receiver) = InteractionClient::channel();
    let first_cancel = CancellationToken::new();
    let mut first = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "stale before dequeue",
        None,
        first_cancel.clone(),
    ));
    poll_until_pending(&mut first).await;
    first_cancel.cancel();
    assert_eq!(first.await, Err(ToolError::Cancelled));

    let mut second = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "accepted after cancellation",
        None,
        CancellationToken::new(),
    ));
    poll_until_pending(&mut second).await;
    let request = receiver.recv().await.unwrap();
    assert_eq!(request.question(), "accepted after cancellation");
    request
        .respond(UserAnswer::new("accepted").unwrap())
        .unwrap();
    assert_eq!(second.await.unwrap().text(), "accepted");
}

#[tokio::test]
async fn dropping_an_undequeued_ask_user_future_releases_the_single_slot() {
    let (client, mut receiver) = InteractionClient::channel();
    let mut first = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "dropped before dequeue",
        None,
        CancellationToken::new(),
    ));
    poll_until_pending(&mut first).await;
    drop(first);

    let mut second = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "accepted after drop",
        None,
        CancellationToken::new(),
    ));
    poll_until_pending(&mut second).await;
    let request = receiver.recv().await.unwrap();
    assert_eq!(request.question(), "accepted after drop");
    request
        .respond(UserAnswer::new("accepted").unwrap())
        .unwrap();
    assert_eq!(second.await.unwrap().text(), "accepted");
}

#[tokio::test]
async fn cancellation_after_dequeue_invalidates_request_and_releases_the_slot() {
    let (client, mut receiver) = InteractionClient::channel();
    let first_cancel = CancellationToken::new();
    let mut first = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "stale after dequeue",
        None,
        first_cancel.clone(),
    ));
    poll_until_pending(&mut first).await;
    let request = receiver.recv().await.unwrap();
    first_cancel.cancel();
    assert_eq!(first.await, Err(ToolError::Cancelled));

    assert_eq!(
        request
            .user_question(InteractionId::new().unwrap())
            .unwrap_err(),
        ToolError::InteractionClosed
    );
    assert_eq!(
        request
            .respond(UserAnswer::new("stale").unwrap())
            .unwrap_err(),
        ToolError::InteractionClosed
    );

    let mut second = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "accepted after stale request",
        None,
        CancellationToken::new(),
    ));
    poll_until_pending(&mut second).await;
    let request = receiver.recv().await.unwrap();
    request
        .respond(UserAnswer::new("accepted").unwrap())
        .unwrap();
    assert_eq!(second.await.unwrap().text(), "accepted");
}

#[tokio::test]
async fn live_dequeued_request_keeps_second_request_busy_until_resolution() {
    let (client, mut receiver) = InteractionClient::channel();
    let mut first = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "live request",
        None,
        CancellationToken::new(),
    ));
    poll_until_pending(&mut first).await;
    let request = receiver.recv().await.unwrap();
    assert_eq!(
        client
            .ask_user(
                TurnId::new().unwrap(),
                "must remain busy",
                None,
                CancellationToken::new(),
            )
            .await,
        Err(ToolError::InteractionBusy)
    );
    request
        .respond(UserAnswer::new("resolved").unwrap())
        .unwrap();
    assert_eq!(first.await.unwrap().text(), "resolved");
}

#[tokio::test]
async fn reply_and_cancellation_have_one_winner_across_repeated_barrier_races() {
    for iteration in 0..128 {
        let (client, mut receiver) = InteractionClient::channel();
        let cancellation = CancellationToken::new();
        let waiter_client = client.clone();
        let waiter_cancellation = cancellation.clone();
        let waiter = tokio::spawn(async move {
            waiter_client
                .ask_user(
                    TurnId::new().unwrap(),
                    format!("race-{iteration}"),
                    None,
                    waiter_cancellation,
                )
                .await
        });
        let request = receiver.recv().await.unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let cancel_barrier = Arc::clone(&barrier);
        let cancel_task = tokio::spawn(async move {
            cancel_barrier.wait().await;
            cancellation.cancel();
        });

        let respond_barrier = Arc::clone(&barrier);
        let response_task = tokio::spawn(async move {
            respond_barrier.wait().await;
            request.respond(UserAnswer::new("race answer").unwrap())
        });

        barrier.wait().await;
        cancel_task.await.unwrap();
        let response_result = response_task.await.unwrap();
        let waiter_result = waiter.await.unwrap();
        match (response_result, waiter_result) {
            (Ok(()), Ok(answer)) => assert_eq!(answer.text(), "race answer"),
            (Err(ToolError::InteractionClosed), Err(ToolError::Cancelled)) => {}
            (response, waiter) => panic!(
                "inconsistent reply/cancellation race at iteration {iteration}: response={response:?}, waiter={waiter:?}"
            ),
        }
        drop(receiver);
    }
}

#[tokio::test]
async fn receiver_and_client_lifecycles_close_without_a_tokio_queue() {
    let (client, receiver): (InteractionClient, InteractionReceiver) = InteractionClient::channel();
    let mut waiting = Box::pin(client.ask_user(
        TurnId::new().unwrap(),
        "receiver will close",
        None,
        CancellationToken::new(),
    ));
    poll_until_pending(&mut waiting).await;
    drop(receiver);
    assert_eq!(
        poll_until_ready(&mut waiting).await,
        Err(ToolError::InteractionClosed)
    );

    let (last_client, mut receiver) = InteractionClient::channel();
    let clone = last_client.clone();
    let mut waiting = Box::pin(receiver.recv());
    poll_until_pending(&mut waiting).await;
    drop(last_client);
    drop(clone);
    let result = poll_until_ready(&mut waiting).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn tool_context_getters_and_ask_user_delegate_to_interaction_client() {
    let (root, workspace) = workspace("context");
    let (client, mut receiver) = InteractionClient::channel();
    let session_id = SessionId::new().unwrap();
    let turn_id = TurnId::new().unwrap();
    let cancellation = CancellationToken::new();
    let context = ToolContext::new(
        session_id,
        turn_id,
        &workspace,
        cancellation.clone(),
        &client,
    )
    .unwrap();
    assert_eq!(context.session_id(), session_id);
    assert_eq!(context.turn_id(), turn_id);
    assert!(std::ptr::eq(context.workspace(), &workspace));
    assert!(!context.cancellation().is_cancelled());
    assert!(std::ptr::eq(context.interactions(), &client));

    let mut task = Box::pin(context.ask_user("question", None));
    let request = tokio::select! {
        request = receiver.recv() => request.unwrap(),
        result = &mut task => panic!("interaction completed before its request was handled: {result:?}"),
    };
    request.reject(ToolError::InvalidInteraction).unwrap();
    assert_eq!(task.await, Err(ToolError::InvalidInteraction));

    workspace.shutdown().await.unwrap();
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn p2_tool_sources_have_no_p2_plus_or_legacy_owner_coupling() {
    for source in [
        include_str!("../src/tools/mod.rs"),
        include_str!("../src/tools/types.rs"),
        include_str!("../src/tools/registry.rs"),
        include_str!("../src/tools/policy.rs"),
        include_str!("../src/tools/context.rs"),
    ] {
        for forbidden in [
            "crate::wire",
            "crate::tools::",
            "crate::workspace::",
            "crate::model_gateway",
            "crate::runtime",
            "crate::session_",
            "ToolExecutionPlan",
            "ToolStartGate",
            "ToolSet",
            "SessionFileMutationQueue",
            "mpsc",
            "spawn_blocking",
            "tokio::spawn",
            "allow(dead_code",
        ] {
            assert!(!source.contains(forbidden), "found forbidden {forbidden}");
        }
        assert!(!source.contains("::*"));
    }

    let lib = include_str!("../src/lib.rs");
    let tests = include_str!("../tests/p2_tools_core.rs");
    assert!(lib.contains("#[path = \"tools/mod.rs\"]\npub(crate) mod tools_v2;"));
    assert!(lib.contains("pub use tools_v2::{"));
    assert!(lib.contains("InteractionReceiver"));
    assert!(!lib.contains("pub use tools_v2::*"));
    assert!(!lib.contains("pub mod registry;"));
    assert!(!lib.contains("pub mod policy;"));
    assert!(!lib.contains("pub mod context;"));
    assert!(!tests.contains(&["tokio::time", "::timeout"].concat()));
    assert!(!tests.contains(&["tokio::time", "::sleep"].concat()));
}
