use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use minicore_runtime::{RelativePath, Workspace, WorkspaceAccess, WorkspaceError};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "minicore-p7-command-cwd-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn command_cwd_returns_only_validated_root_relative_directories() {
    let temporary = TempDir::new("valid");
    fs::create_dir(temporary.path().join("nested")).unwrap();
    fs::write(temporary.path().join("file"), b"file").unwrap();
    let workspace = Workspace::open(temporary.path(), WorkspaceAccess::ReadOnly).unwrap();

    assert_eq!(workspace.command_cwd(None).await.unwrap(), temporary.path());
    assert_eq!(
        workspace
            .command_cwd(Some(&RelativePath::new("nested").unwrap()))
            .await
            .unwrap(),
        temporary.path().join("nested")
    );
    assert_eq!(
        workspace
            .command_cwd(Some(&RelativePath::new("missing").unwrap()))
            .await
            .unwrap_err(),
        WorkspaceError::NotFound
    );
    assert_eq!(
        workspace
            .command_cwd(Some(&RelativePath::new("file").unwrap()))
            .await
            .unwrap_err(),
        WorkspaceError::NotRegularFile
    );
    assert!(RelativePath::new("../outside").is_err());
    assert!(RelativePath::new("/outside").is_err());

    workspace.shutdown().await.unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "current_thread")]
async fn command_cwd_rejects_symlink_components() {
    let temporary = TempDir::new("symlink");
    let outside = TempDir::new("outside");
    fs::create_dir(outside.path().join("real")).unwrap();
    symlink_dir(&outside.path().join("real"), &temporary.path().join("link"));
    let workspace = Workspace::open(temporary.path(), WorkspaceAccess::ReadOnly).unwrap();
    let error = workspace
        .command_cwd(Some(&RelativePath::new("link").unwrap()))
        .await
        .unwrap_err();
    assert_eq!(error, WorkspaceError::IsSymlink);
    workspace.shutdown().await.unwrap();
}

#[test]
fn p7_sources_expose_only_the_requested_process_surface() {
    let process = include_str!("../src/tools/process.rs");
    let run_command = include_str!("../src/tools/builtins/run_command.rs");
    let builtins = include_str!("../src/tools/builtins/mod.rs");
    let tools = include_str!("../src/tools/mod.rs");

    for source in [process, run_command, builtins, tools] {
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
            "cmd /c",
            "sh -c",
        ] {
            assert!(!source.contains(forbidden), "found forbidden {forbidden}");
        }
    }
    assert!(process.contains("pub struct ProcessPolicy"));
    assert!(process.contains("pub enum ProgramPolicy"));
    assert!(run_command.contains("pub struct RunCommandTool"));
    assert!(run_command.contains("tokio::process::Command"));
    assert!(tools.contains("ProcessPolicy"));
    assert!(tools.contains("ProgramPolicy"));
    assert!(builtins.contains("RunCommandTool"));
}

#[test]
fn command_cwd_and_run_command_document_only_trusted_host_pre_spawn_validation() {
    let compact = |source: &str| {
        source
            .split_whitespace()
            .collect::<String>()
            .replace('/', "")
            .to_ascii_lowercase()
    };
    let workspace = compact(include_str!("../src/workspace/root.rs"));
    let run_command = compact(include_str!("../src/tools/builtins/run_command.rs"));

    for source in [&workspace, &run_command] {
        for forbidden in [
            "sandboxedchild",
            "childprocessisconfined",
            "retaineddirectoryidentity",
            "capabilityidentityistransferred",
            "identity-boundprocess",
            "toctou-safe",
            "anti-toctouguarantee",
            "preventspost-validationreplacement",
        ] {
            assert!(
                !source.contains(forbidden),
                "found forbidden claim {forbidden}"
            );
        }
    }
    assert!(workspace.contains("pre-spawnvalidation"));
    assert!(workspace.contains("trusted,non-adversarialhostfilesystem"));
    assert!(workspace.contains("doesnotprovideaprocesssandbox"));
    assert!(run_command.contains("ambienthostauthority"));
    assert!(run_command.contains("command::current_dir"));
    assert!(run_command.contains("trusted,non-adversarialhostfilesystem"));
    assert!(run_command.contains("doesnotprovideacapabilityidentityorprocesssandbox"));
}

#[test]
fn production_tokio_keeps_the_exact_pinned_process_features() {
    let cargo = include_str!("../Cargo.toml");
    assert!(cargo.contains(
        "tokio = { version = \"1.53.1\", default-features = false, features = [\"macros\", \"rt\", \"sync\", \"time\", \"process\", \"io-util\"] }"
    ));
}
