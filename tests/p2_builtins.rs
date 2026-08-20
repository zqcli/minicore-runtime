use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use minicore_runtime::{
    AskUserTool, InteractionClient, InteractionReceiver, ListDirectoryTool, ReadFileTool,
    RelativePath, Tool, ToolContext, ToolError, ToolName, ToolRegistry, ToolSpec, TurnId,
    UserAnswer, Workspace, WorkspaceAccess, WriteFileTool,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "minicore-p2-builtins-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temporary directory is creatable");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn make_workspace(label: &str, access: WorkspaceAccess) -> (TempDir, Workspace) {
    let temporary = TempDir::new(label);
    let workspace = Workspace::open(temporary.path(), access).expect("workspace opens");
    (temporary, workspace)
}

fn context<'a>(
    workspace: &'a Workspace,
    cancellation: CancellationToken,
    interactions: &'a InteractionClient,
) -> ToolContext<'a> {
    ToolContext::new(
        minicore_runtime::SessionId::new().unwrap(),
        TurnId::new().unwrap(),
        workspace,
        cancellation,
        interactions,
    )
    .unwrap()
}

async fn run<T: Tool>(
    tool: &T,
    workspace: &Workspace,
    interactions: &InteractionClient,
    cancellation: CancellationToken,
    args: Value,
) -> Result<minicore_runtime::ToolOutput, ToolError> {
    tool.execute(context(workspace, cancellation, interactions), args)
        .await
}

fn assert_failure(output: &minicore_runtime::ToolOutput, text: &str) {
    assert!(output.is_error());
    assert_eq!(output.text(), text);
}

fn assert_success(output: &minicore_runtime::ToolOutput, text: &str) {
    assert!(!output.is_error());
    assert_eq!(output.text(), text);
}

fn assert_receiver_pending(receiver: &mut InteractionReceiver) {
    let mut future = Box::pin(receiver.recv());
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
}

fn assert_closed_schema(spec: &ToolSpec, required: &[&str]) {
    let schema = spec.input_schema();
    assert_eq!(schema.get("type"), Some(&json!("object")));
    assert_eq!(schema.get("additionalProperties"), Some(&json!(false)));
    assert_eq!(
        schema.get("required"),
        Some(&Value::Array(
            required.iter().map(|name| json!(name)).collect(),
        ))
    );
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("file symlink is creatable");
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).expect("file symlink is creatable");
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("directory symlink is creatable");
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).expect("directory symlink is creatable");
}

#[test]
fn builtins_are_explicitly_registered_in_the_fixed_host_order_and_specs_are_closed() {
    let mut builder = ToolRegistry::builder();
    builder.register(AskUserTool::new()).unwrap();
    builder.register(ReadFileTool::new()).unwrap();
    builder.register(ListDirectoryTool::new()).unwrap();
    builder.register(WriteFileTool::new()).unwrap();
    let registry = builder.build();

    let enabled = ["ask_user", "read_file", "list_directory", "write_file"]
        .into_iter()
        .map(|name| ToolName::from_str(name).unwrap())
        .collect();
    let specs = registry.specs(&enabled).unwrap();
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name().as_str())
            .collect::<Vec<_>>(),
        vec!["ask_user", "list_directory", "read_file", "write_file"]
    );

    let ask = AskUserTool::new().spec();
    assert_eq!(ask.name().as_str(), "ask_user");
    assert_closed_schema(&ask, &["question"]);
    assert_eq!(ask.input_schema()["properties"]["question"]["minLength"], 1);
    assert_eq!(ask.input_schema()["properties"]["choices"]["minItems"], 1);
    assert_eq!(ask.input_schema()["properties"]["choices"]["maxItems"], 32);

    let read = ReadFileTool::new().spec();
    assert_eq!(read.name().as_str(), "read_file");
    assert_closed_schema(&read, &["path"]);
    assert_eq!(read.input_schema()["properties"]["path"]["minLength"], 1);
    assert_eq!(
        read.input_schema()["properties"]["path"]["maxLength"],
        RelativePath::MAX_BYTES
    );

    let list = ListDirectoryTool::new().spec();
    assert_eq!(list.name().as_str(), "list_directory");
    assert_closed_schema(&list, &["path"]);
    assert_eq!(
        list.input_schema()["properties"]["path"].get("minLength"),
        None
    );

    let write = WriteFileTool::new().spec();
    assert_eq!(write.name().as_str(), "write_file");
    assert_closed_schema(&write, &["path", "content"]);
    assert_eq!(write.input_schema()["properties"]["path"]["minLength"], 1);
    assert_eq!(
        write.input_schema()["properties"]["content"]["maxLength"],
        Workspace::MAX_WRITE_BYTES
    );

    let empty = ToolRegistry::default();
    assert!(
        empty
            .get(&ToolName::from_str("read_file").unwrap())
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn read_list_write_succeed_with_exact_model_visible_outputs() {
    let (temporary, workspace) = make_workspace("success", WorkspaceAccess::ReadWrite);
    fs::write(temporary.path().join("z.txt"), b"z body").unwrap();
    fs::write(temporary.path().join("a.txt"), b"a body").unwrap();
    fs::create_dir(temporary.path().join("dir")).unwrap();
    let (interactions, _receiver) = InteractionClient::channel();

    let read = run(
        &ReadFileTool::new(),
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "a.txt"}),
    )
    .await
    .unwrap();
    assert_success(&read, "a body");

    let list = run(
        &ListDirectoryTool::new(),
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": ""}),
    )
    .await
    .unwrap();
    assert_success(
        &list,
        r#"[{"name":"a.txt","kind":"file"},{"name":"dir","kind":"directory"},{"name":"z.txt","kind":"file"}]"#,
    );

    let write = run(
        &WriteFileTool::new(),
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "dir/new.txt", "content": "new body"}),
    )
    .await
    .unwrap();
    assert_success(&write, "file written");
    assert_eq!(
        fs::read(temporary.path().join("dir/new.txt")).unwrap(),
        b"new body"
    );

    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn argument_shapes_are_closed_and_reject_invalid_paths_before_workspace_io() {
    let (temporary, workspace) = make_workspace("arguments", WorkspaceAccess::ReadWrite);
    fs::write(temporary.path().join("safe.txt"), b"safe").unwrap();
    let (interactions, _receiver) = InteractionClient::channel();
    let read = ReadFileTool::new();
    let list = ListDirectoryTool::new();
    let write = WriteFileTool::new();

    for args in [
        json!({}),
        json!({"path": "safe.txt", "extra": true}),
        json!({"path": null}),
        json!({"path": 1}),
        json!({"path": ""}),
        json!({"path": "../safe.txt"}),
        json!({"path": "/etc/passwd"}),
        json!({"path": "a\\b"}),
        json!({"path": "a\u{0}b"}),
    ] {
        let output = run(
            &read,
            &workspace,
            &interactions,
            CancellationToken::new(),
            args,
        )
        .await
        .unwrap();
        assert_failure(&output, "tool arguments are invalid");
    }

    for args in [
        json!({"path": "../"}),
        json!({"path": "/"}),
        json!({"path": "a\\b", "extra": 1}),
        json!({"path": 1}),
    ] {
        let output = run(
            &list,
            &workspace,
            &interactions,
            CancellationToken::new(),
            args,
        )
        .await
        .unwrap();
        assert_failure(&output, "tool arguments are invalid");
    }

    for args in [
        json!({"path": "", "content": "x"}),
        json!({"path": "../safe.txt", "content": "x"}),
        json!({"path": "/tmp/safe.txt", "content": "x"}),
        json!({"path": "safe.txt"}),
        json!({"content": "x"}),
        json!({"path": "safe.txt", "content": "x", "extra": true}),
        json!({"path": "safe.txt", "content": null}),
        json!({"path": "safe.txt", "content": 1}),
    ] {
        let output = run(
            &write,
            &workspace,
            &interactions,
            CancellationToken::new(),
            args,
        )
        .await
        .unwrap();
        assert_failure(&output, "tool arguments are invalid");
    }
    assert_eq!(
        fs::read(temporary.path().join("safe.txt")).unwrap(),
        b"safe"
    );
    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn read_file_maps_utf8_size_missing_and_output_failures_without_leaks() {
    let (temporary, workspace) = make_workspace("read-errors", WorkspaceAccess::ReadWrite);
    fs::write(temporary.path().join("bad.bin"), [0xff, 0xfe]).unwrap();
    fs::write(
        temporary.path().join("boundary.txt"),
        vec![b'x'; Workspace::MAX_READ_BYTES],
    )
    .unwrap();
    fs::write(
        temporary.path().join("exact.txt"),
        vec![b'x'; Workspace::MAX_READ_BYTES + 1],
    )
    .unwrap();
    fs::write(temporary.path().join("missing-parent.txt"), b"x").unwrap();
    fs::create_dir(temporary.path().join("folder")).unwrap();
    let (interactions, _receiver) = InteractionClient::channel();
    let tool = ReadFileTool::new();

    let boundary = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "boundary.txt"}),
    )
    .await
    .unwrap();
    assert!(!boundary.is_error());
    assert_eq!(boundary.text().len(), Workspace::MAX_READ_BYTES);

    let invalid_utf8 = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "bad.bin"}),
    )
    .await
    .unwrap();
    assert_failure(&invalid_utf8, "file is not valid UTF-8");

    let too_large = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "exact.txt"}),
    )
    .await
    .unwrap();
    assert_failure(&too_large, "file is too large");

    for path in ["missing.txt", "folder"] {
        let output = run(
            &tool,
            &workspace,
            &interactions,
            CancellationToken::new(),
            json!({"path": path}),
        )
        .await
        .unwrap();
        assert_failure(&output, "file could not be read");
    }

    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn list_directory_is_sorted_classified_and_bound_to_one_thousand_entries() {
    let (temporary, workspace) = make_workspace("list-bounds", WorkspaceAccess::ReadWrite);
    fs::write(temporary.path().join("z-file"), []).unwrap();
    fs::write(temporary.path().join("a-file"), []).unwrap();
    fs::create_dir(temporary.path().join("m-directory")).unwrap();
    let outside = TempDir::new("list-outside");
    fs::write(outside.path().join("secret"), b"secret").unwrap();
    symlink_file(
        &outside.path().join("secret"),
        &temporary.path().join("b-link"),
    );
    let exact = temporary.path().join("exact");
    let overflow = temporary.path().join("overflow");
    fs::create_dir(&exact).unwrap();
    fs::create_dir(&overflow).unwrap();
    for index in 0..Workspace::MAX_LIST_ENTRIES {
        fs::write(exact.join(format!("entry-{index:04}")), []).unwrap();
    }
    for index in 0..=Workspace::MAX_LIST_ENTRIES {
        fs::write(overflow.join(format!("entry-{index:04}")), []).unwrap();
    }
    let (interactions, _receiver) = InteractionClient::channel();
    let tool = ListDirectoryTool::new();

    let listing = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": ""}),
    )
    .await
    .unwrap();
    assert_success(
        &listing,
        r#"[{"name":"a-file","kind":"file"},{"name":"b-link","kind":"symlink"},{"name":"exact","kind":"directory"},{"name":"m-directory","kind":"directory"},{"name":"overflow","kind":"directory"},{"name":"z-file","kind":"file"}]"#,
    );

    let exact_listing = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "exact"}),
    )
    .await
    .unwrap();
    assert!(!exact_listing.is_error());
    assert_eq!(
        serde_json::from_str::<Vec<Value>>(exact_listing.text())
            .unwrap()
            .len(),
        Workspace::MAX_LIST_ENTRIES
    );

    let overflow_listing = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "overflow"}),
    )
    .await
    .unwrap();
    assert_failure(&overflow_listing, "directory listing is too large");

    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn list_directory_maps_missing_and_unsupported_entries_without_reading_content() {
    let (temporary, workspace) = make_workspace("list-errors", WorkspaceAccess::ReadWrite);
    fs::write(temporary.path().join("file.txt"), b"not a directory").unwrap();
    let (interactions, _receiver) = InteractionClient::channel();
    let tool = ListDirectoryTool::new();

    for path in ["missing", "file.txt"] {
        let output = run(
            &tool,
            &workspace,
            &interactions,
            CancellationToken::new(),
            json!({"path": path}),
        )
        .await
        .unwrap();
        assert_failure(&output, "directory could not be listed");
    }

    workspace.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn list_directory_maps_a_non_utf8_entry_name_to_the_fixed_failure() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let (temporary, workspace) = make_workspace("list-name", WorkspaceAccess::ReadWrite);
    let name = OsString::from_vec(vec![0xff]);
    fs::write(temporary.path().join(&name), []).unwrap();
    let (interactions, _receiver) = InteractionClient::channel();
    let output = run(
        &ListDirectoryTool::new(),
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": ""}),
    )
    .await
    .unwrap();
    assert_failure(&output, "directory contains an unsupported entry name");
    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn list_directory_rejects_compact_output_over_the_tool_output_bound() {
    let (temporary, workspace) = make_workspace("list-output-bound", WorkspaceAccess::ReadWrite);
    let directory = temporary.path().join("large");
    fs::create_dir(&directory).unwrap();
    for index in 0..Workspace::MAX_LIST_ENTRIES {
        let name = format!("{index:04}{}", "x".repeat(251));
        assert_eq!(name.len(), 255);
        fs::write(directory.join(name), []).unwrap();
    }
    let (interactions, _receiver) = InteractionClient::channel();
    let output = run(
        &ListDirectoryTool::new(),
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "large"}),
    )
    .await
    .unwrap();
    assert_failure(&output, "directory listing is too large");
    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn write_file_is_atomic_bounded_read_only_safe_and_reports_fixed_errors() {
    let (temporary, workspace) = make_workspace("write-errors", WorkspaceAccess::ReadWrite);
    fs::write(temporary.path().join("replace.txt"), b"old").unwrap();
    fs::create_dir(temporary.path().join("directory")).unwrap();
    let (interactions, _receiver) = InteractionClient::channel();
    let tool = WriteFileTool::new();

    let exact = "x".repeat(Workspace::MAX_WRITE_BYTES);
    let output = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "replace.txt", "content": exact}),
    )
    .await
    .unwrap();
    assert_success(&output, "file written");
    assert_eq!(
        fs::read(temporary.path().join("replace.txt"))
            .unwrap()
            .len(),
        Workspace::MAX_WRITE_BYTES
    );

    let oversized = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "oversized.txt", "content": "x".repeat(Workspace::MAX_WRITE_BYTES + 1)}),
    )
    .await
    .unwrap();
    assert_failure(&oversized, "tool arguments are invalid");
    assert!(!temporary.path().join("oversized.txt").exists());

    let missing_parent = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "missing/child.txt", "content": "x"}),
    )
    .await
    .unwrap();
    assert_failure(&missing_parent, "file could not be written");

    let directory = run(
        &tool,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "directory", "content": "x"}),
    )
    .await
    .unwrap();
    assert_failure(&directory, "file could not be written");

    workspace.shutdown().await.unwrap();

    let (temporary, read_only) = make_workspace("read-only", WorkspaceAccess::ReadOnly);
    fs::write(temporary.path().join("locked.txt"), b"locked").unwrap();
    let output = run(
        &tool,
        &read_only,
        &interactions,
        CancellationToken::new(),
        json!({"path": "locked.txt", "content": "changed"}),
    )
    .await
    .unwrap();
    assert_failure(&output, "workspace is read-only");
    assert_eq!(
        fs::read(temporary.path().join("locked.txt")).unwrap(),
        b"locked"
    );
    read_only.shutdown().await.unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "current_thread")]
async fn capability_symlink_escape_and_final_symlink_write_are_denied_without_outside_mutation() {
    let (temporary, workspace) = make_workspace("symlink", WorkspaceAccess::ReadWrite);
    let outside = TempDir::new("symlink-outside");
    fs::write(outside.path().join("secret.txt"), b"outside").unwrap();
    fs::create_dir(outside.path().join("dir")).unwrap();
    symlink_file(
        &outside.path().join("secret.txt"),
        &temporary.path().join("escape.txt"),
    );
    symlink_dir(
        &outside.path().join("dir"),
        &temporary.path().join("escape-dir"),
    );
    fs::write(temporary.path().join("target.txt"), b"inside").unwrap();
    symlink_file(
        &outside.path().join("secret.txt"),
        &temporary.path().join("final.txt"),
    );
    let (interactions, _receiver) = InteractionClient::channel();
    let read = ReadFileTool::new();
    let list = ListDirectoryTool::new();
    let write = WriteFileTool::new();

    let read_output = run(
        &read,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "escape.txt"}),
    )
    .await
    .unwrap();
    assert!(read_output.is_error());
    assert!(["workspace access is denied", "file could not be read"].contains(&read_output.text()));

    let list_output = run(
        &list,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "escape-dir"}),
    )
    .await
    .unwrap();
    assert!(list_output.is_error());
    assert!(
        [
            "workspace access is denied",
            "directory could not be listed"
        ]
        .contains(&list_output.text())
    );

    let write_output = run(
        &write,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "escape.txt", "content": "changed"}),
    )
    .await
    .unwrap();
    assert!(write_output.is_error());
    assert!(
        ["workspace access is denied", "file could not be written"].contains(&write_output.text())
    );

    let final_output = run(
        &write,
        &workspace,
        &interactions,
        CancellationToken::new(),
        json!({"path": "final.txt", "content": "must not escape"}),
    )
    .await
    .unwrap();
    assert_failure(&final_output, "workspace access is denied");
    assert_eq!(
        fs::read(outside.path().join("secret.txt")).unwrap(),
        b"outside"
    );
    assert_eq!(
        fs::read(temporary.path().join("target.txt")).unwrap(),
        b"inside"
    );
    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_filesystem_admission_returns_cancelled_and_does_zero_write_io() {
    let (temporary, workspace) = make_workspace("cancel-before-io", WorkspaceAccess::ReadWrite);
    let (interactions, _receiver) = InteractionClient::channel();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let output = run(
        &WriteFileTool::new(),
        &workspace,
        &interactions,
        cancellation,
        json!({"path": "must-not-exist.txt", "content": "no io"}),
    )
    .await;
    assert_eq!(output, Err(ToolError::Cancelled));
    assert!(!temporary.path().join("must-not-exist.txt").exists());
    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ask_user_returns_verbatim_answer_and_propagates_cancellation_and_pending_errors() {
    let (_temporary, workspace) = make_workspace("ask", WorkspaceAccess::ReadOnly);
    let tool = AskUserTool::new();
    let (interactions, mut receiver) = InteractionClient::channel();

    let mut first = Box::pin(tool.execute(
        context(&workspace, CancellationToken::new(), &interactions),
        json!({"question": "Choose", "choices": ["one", "two"]}),
    ));
    let request = tokio::select! {
        request = receiver.recv() => request.unwrap(),
        result = &mut first => panic!("ask_user completed too early: {result:?}"),
    };
    request
        .respond(UserAnswer::new("verbatim answer").unwrap())
        .unwrap();
    let answer = first.await.unwrap();
    assert_success(&answer, "verbatim answer");

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = tool
        .execute(
            context(&workspace, cancellation, &interactions),
            json!({"question": "cancel me"}),
        )
        .await;
    assert_eq!(cancelled, Err(ToolError::Cancelled));

    let mut pending = Box::pin(tool.execute(
        context(&workspace, CancellationToken::new(), &interactions),
        json!({"question": "pending"}),
    ));
    let pending_request = tokio::select! {
        request = receiver.recv() => request.unwrap(),
        result = &mut pending => panic!("ask_user completed too early: {result:?}"),
    };
    let busy = tool
        .execute(
            context(&workspace, CancellationToken::new(), &interactions),
            json!({"question": "second"}),
        )
        .await;
    assert_eq!(busy, Err(ToolError::InteractionBusy));
    pending_request
        .respond(UserAnswer::new("done").unwrap())
        .unwrap();
    assert_eq!(pending.await.unwrap().text(), "done");

    workspace.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ask_user_invalid_arguments_are_failure_outputs_and_never_create_interactions() {
    let (_temporary, workspace) = make_workspace("ask-invalid", WorkspaceAccess::ReadOnly);
    let tool = AskUserTool::new();
    let (interactions, mut receiver) = InteractionClient::channel();
    for args in [
        json!({}),
        json!({"question": ""}),
        json!({"question": "q", "choices": []}),
        json!({"question": "q", "choices": null}),
        json!({"question": "q", "choices": [""]}),
        json!({"question": "q", "choices": ["a"], "extra": true}),
        json!({"question": 1}),
    ] {
        let output = run(
            &tool,
            &workspace,
            &interactions,
            CancellationToken::new(),
            args,
        )
        .await
        .unwrap();
        assert_failure(&output, "tool arguments are invalid");
        assert_receiver_pending(&mut receiver);
    }
    workspace.shutdown().await.unwrap();
}

#[test]
fn builtin_sources_have_only_v2_tool_and_workspace_dependencies() {
    for source in [
        include_str!("../src/tools/builtins/mod.rs"),
        include_str!("../src/tools/builtins/ask_user.rs"),
        include_str!("../src/tools/builtins/path_args.rs"),
        include_str!("../src/tools/builtins/read_file.rs"),
        include_str!("../src/tools/builtins/list_directory.rs"),
        include_str!("../src/tools/builtins/write_file.rs"),
    ] {
        for forbidden in [
            "crate::wire",
            "ToolExecutionPlan",
            "ToolStartGate",
            "ToolSet",
            "SessionFileMutationQueue",
            "tokio::spawn",
            "spawn_blocking",
            "canonicalize",
            "std::fs",
            "allow(dead_code",
        ] {
            assert!(!source.contains(forbidden), "found forbidden {forbidden}");
        }
    }
}
