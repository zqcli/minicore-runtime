use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::task::Context;

use minicore_runtime::{
    DirectoryEntryKind, RelativePath, RelativePathError, Workspace, WorkspaceAccess, WorkspaceError,
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "minicore-p2-workspace-{label}-{}-{sequence}",
            std::process::id()
        ));
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

fn relative(value: &str) -> RelativePath {
    value.parse().expect("test path is valid")
}

fn assert_redacted(error: WorkspaceError, forbidden: &[&str]) {
    let display = error.to_string();
    let debug = format!("{error:?}");
    for secret in forbidden {
        if secret.is_empty() {
            continue;
        }
        assert!(
            !display.contains(secret),
            "display leaked {secret:?}: {display}"
        );
        assert!(!debug.contains(secret), "debug leaked {secret:?}: {debug}");
    }
}

fn assert_no_temporary_files(root: &Path) {
    let leftovers = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".minicore-workspace-tmp-"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "temporary siblings remain: {leftovers:?}"
    );
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("directory symlink is creatable");
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).expect("directory symlink is creatable");
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("file symlink is creatable");
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).expect("file symlink is creatable");
}

#[test]
fn relative_path_accepts_exact_boundaries_without_normalizing() {
    let empty = RelativePath::new("").unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.as_str(), "");

    let max_bytes = "x".repeat(4096);
    let max_utf8_bytes = "é".repeat(2048);
    let max_segments = std::iter::repeat_n("x", 256).collect::<Vec<_>>().join("/");
    for value in [
        "src/main.rs",
        "中文/文件.txt",
        &max_bytes,
        &max_utf8_bytes,
        &max_segments,
    ] {
        let path = RelativePath::new(value).unwrap();
        assert_eq!(path.as_str(), value);
    }
}

#[test]
fn relative_path_rejects_unsafe_text_and_platform_prefixes() {
    let invalid = [
        "a/",
        "/a",
        "a//b",
        ".",
        "..",
        "a/./b",
        "a/../b",
        "a\\b",
        "a\0b",
        "a\nb",
        "a\tb",
        &"x".repeat(4097),
        &std::iter::repeat_n("x", 257).collect::<Vec<_>>().join("/"),
        "C:",
        "C:relative",
        "C:/absolute",
        "c:\\absolute",
        "\\\\server\\share",
        "//server/share",
        "\\?\\C:\\absolute",
    ];

    for value in invalid {
        let error = RelativePath::new(value).expect_err("unsafe path was accepted");
        assert!(!error.to_string().contains(value));
    }

    assert!(matches!(
        RelativePath::new("x".repeat(4097)),
        Err(RelativePathError::TooLong)
    ));
}

#[test]
fn relative_path_serde_is_checked_and_preserves_text() {
    for value in ["", "a/b", "a-b", "中文/文件.txt"] {
        let path = relative(value);
        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(serde_json::from_str::<RelativePath>(&json).unwrap(), path);
        assert_eq!(
            serde_json::from_str::<RelativePath>(&json)
                .unwrap()
                .as_str(),
            value
        );
    }

    for json in [
        "null",
        "1",
        "[]",
        "\"a//b\"",
        "\"../secret\"",
        "\"a\\\\b\"",
        "\"a\\u0000b\"",
    ] {
        assert!(
            serde_json::from_str::<RelativePath>(json).is_err(),
            "accepted {json}"
        );
    }
}

#[test]
fn workspace_open_rejects_missing_files_and_root_symlinks() {
    let temporary = TempDir::new("open");
    let missing = temporary.path().join("missing");
    let missing_error = Workspace::open(&missing, WorkspaceAccess::ReadOnly).unwrap_err();
    assert_redacted(
        missing_error,
        &["missing", temporary.path().to_str().unwrap()],
    );

    let file = temporary.path().join("file");
    fs::write(&file, b"not a directory").unwrap();
    let file_error = Workspace::open(&file, WorkspaceAccess::ReadOnly).unwrap_err();
    assert_redacted(file_error, &["file", temporary.path().to_str().unwrap()]);

    #[cfg(any(unix, windows))]
    {
        let root = temporary.path().join("root");
        let link = temporary.path().join("root-link");
        fs::create_dir(&root).unwrap();
        create_directory_symlink(&root, &link);
        let symlink_error = Workspace::open(&link, WorkspaceAccess::ReadOnly).unwrap_err();
        assert_redacted(
            symlink_error,
            &["root-link", temporary.path().to_str().unwrap()],
        );
    }
}

#[test]
fn workspace_root_requires_an_absolute_lexically_clean_path() {
    let temporary = TempDir::new("root-path");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();

    for path in [
        Path::new("."),
        Path::new("relative-root"),
        &root.join("."),
        &root.join(".."),
    ] {
        assert_eq!(
            Workspace::open(path, WorkspaceAccess::ReadOnly).unwrap_err(),
            WorkspaceError::InvalidPath,
            "accepted root path {path:?}"
        );
    }

    #[cfg(any(unix, windows))]
    {
        let link = temporary.path().join("link");
        create_directory_symlink(&root, &link);
        assert_eq!(
            Workspace::open(link.join("."), WorkspaceAccess::ReadOnly).unwrap_err(),
            WorkspaceError::InvalidPath
        );
        assert_eq!(
            Workspace::open(&link, WorkspaceAccess::ReadOnly).unwrap_err(),
            WorkspaceError::RootSymlink
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn opened_workspace_capability_remains_bound_across_root_replacement() {
    let temporary = TempDir::new("root-replacement");
    let root = temporary.path().join("root");
    let displaced = temporary.path().join("displaced");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("target.txt"), b"old").unwrap();

    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
    fs::rename(&root, &displaced).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("target.txt"), b"replacement").unwrap();

    workspace
        .write_text(&relative("target.txt"), "bound")
        .await
        .unwrap();
    assert_eq!(fs::read(displaced.join("target.txt")).unwrap(), b"bound");
    assert_eq!(fs::read(root.join("target.txt")).unwrap(), b"replacement");
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_reads_regular_utf8_files_with_max_plus_one_detection() {
    let temporary = TempDir::new("read");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("exact.txt"), b"hello").unwrap();
    fs::write(root.join("empty.txt"), b"").unwrap();
    fs::write(root.join("binary.txt"), [0xff, 0xfe]).unwrap();
    fs::create_dir(root.join("directory")).unwrap();

    let workspace = Workspace::open(&root, WorkspaceAccess::ReadOnly).unwrap();
    assert_eq!(
        workspace
            .read_text(&relative("exact.txt"), 5)
            .await
            .unwrap(),
        "hello"
    );
    assert_eq!(
        workspace
            .read_text(&relative("empty.txt"), 0)
            .await
            .unwrap(),
        ""
    );

    let too_large = workspace
        .read_text(&relative("exact.txt"), 4)
        .await
        .unwrap_err();
    assert_redacted(too_large, &["exact.txt", root.to_str().unwrap()]);

    let invalid_utf8 = workspace
        .read_text(&relative("binary.txt"), 2)
        .await
        .unwrap_err();
    assert_redacted(invalid_utf8, &["binary.txt", root.to_str().unwrap()]);

    let directory_error = workspace
        .read_text(&relative("directory"), 10)
        .await
        .unwrap_err();
    assert_redacted(directory_error, &["directory", root.to_str().unwrap()]);

    let empty_error = workspace
        .read_text(&RelativePath::default(), 10)
        .await
        .unwrap_err();
    assert_redacted(empty_error, &[root.to_str().unwrap()]);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_read_rejects_escape_and_symlink_escape() {
    let temporary = TempDir::new("read-escape");
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"outside secret").unwrap();
    create_directory_symlink(&outside, &root.join("escape"));

    assert!(RelativePath::new("../secret.txt").is_err());
    let workspace = Workspace::open(&root, WorkspaceAccess::ReadOnly).unwrap();
    let path = "escape/secret.txt";
    let error = workspace
        .read_text(&relative(path), 1024)
        .await
        .unwrap_err();
    assert_redacted(error, &[path, "secret", outside.to_str().unwrap()]);
    assert_eq!(
        fs::read(outside.join("secret.txt")).unwrap(),
        b"outside secret"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_lists_sorted_direct_entries_without_following_entry_symlinks() {
    let temporary = TempDir::new("list");
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("z-file"), b"z").unwrap();
    fs::write(root.join("a-file"), b"a").unwrap();
    fs::create_dir(root.join("m-directory")).unwrap();
    fs::write(outside.join("secret"), b"secret").unwrap();
    create_file_symlink(&outside.join("secret"), &root.join("b-link"));

    let workspace = Workspace::open(&root, WorkspaceAccess::ReadOnly).unwrap();
    let entries = workspace.list(&RelativePath::default(), 16).await.unwrap();
    let names = entries.iter().map(|entry| entry.name()).collect::<Vec<_>>();
    assert_eq!(names, vec!["a-file", "b-link", "m-directory", "z-file"]);
    assert_eq!(entries[0].kind(), DirectoryEntryKind::File);
    assert_eq!(entries[1].kind(), DirectoryEntryKind::Symlink);
    assert_eq!(entries[2].kind(), DirectoryEntryKind::Directory);
    assert_eq!(entries[3].kind(), DirectoryEntryKind::File);

    let too_many = workspace
        .list(&RelativePath::default(), 2)
        .await
        .unwrap_err();
    assert_redacted(too_many, &[root.to_str().unwrap()]);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_listing_uses_exact_entry_and_total_bounds_and_limit_semantics() {
    assert_eq!(Workspace::MAX_LIST_ENTRIES, 1_000);
    assert_eq!(Workspace::MAX_LIST_NAME_BYTES, 4_096);
    assert_eq!(Workspace::MAX_LIST_TOTAL_NAME_BYTES, 262_144);

    let temporary = TempDir::new("list-bounds");
    let root = temporary.path().join("root");
    let exact = root.join("exact");
    let overflow = root.join("overflow");
    let empty = root.join("empty");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&exact).unwrap();
    fs::create_dir(&overflow).unwrap();
    fs::create_dir(&empty).unwrap();
    for index in 0..1_000 {
        fs::write(exact.join(format!("entry-{index:04}")), []).unwrap();
    }
    for index in 0..1_001 {
        fs::write(overflow.join(format!("entry-{index:04}")), []).unwrap();
    }

    let workspace = Workspace::open(&root, WorkspaceAccess::ReadOnly).unwrap();
    assert_eq!(
        workspace
            .list(&relative("exact"), Workspace::MAX_LIST_ENTRIES)
            .await
            .unwrap()
            .len(),
        1_000
    );
    assert_eq!(
        workspace
            .list(&RelativePath::default(), Workspace::MAX_LIST_ENTRIES + 1)
            .await
            .unwrap_err(),
        WorkspaceError::ListingTooLarge
    );
    assert_eq!(
        workspace
            .list(&relative("overflow"), Workspace::MAX_LIST_ENTRIES)
            .await
            .unwrap_err(),
        WorkspaceError::ListingTooLarge
    );
    assert!(
        workspace
            .list(&relative("empty"), 0)
            .await
            .unwrap()
            .is_empty()
    );
    fs::write(empty.join("one"), []).unwrap();
    assert_eq!(
        workspace.list(&relative("empty"), 0).await.unwrap_err(),
        WorkspaceError::ListingTooLarge
    );
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_list_rejects_requested_symlink_escape_and_non_directory_targets() {
    let temporary = TempDir::new("list-escape");
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("file.txt"), b"file").unwrap();
    fs::create_dir(outside.join("nested")).unwrap();
    create_directory_symlink(&outside, &root.join("escape"));

    let workspace = Workspace::open(&root, WorkspaceAccess::ReadOnly).unwrap();
    for path in ["escape", "file.txt"] {
        let error = workspace.list(&relative(path), 16).await.unwrap_err();
        assert_redacted(error, &[path, outside.to_str().unwrap()]);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_writes_atomically_replaces_creates_and_allows_empty_content() {
    let temporary = TempDir::new("write");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("replace.txt"), b"old content").unwrap();

    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
    workspace
        .write_text(&relative("replace.txt"), "new content")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("replace.txt")).unwrap(), b"new content");

    workspace
        .write_text(&relative("created.txt"), "created")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("created.txt")).unwrap(), b"created");

    workspace
        .write_text(&relative("empty.txt"), "")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("empty.txt")).unwrap(), b"");

    let max_content = "x".repeat(Workspace::MAX_WRITE_BYTES);
    workspace
        .write_text(&relative("max.txt"), &max_content)
        .await
        .unwrap();
    assert_eq!(
        fs::metadata(root.join("max.txt")).unwrap().len(),
        Workspace::MAX_WRITE_BYTES as u64
    );
    let too_large = "x".repeat(Workspace::MAX_WRITE_BYTES + 1);
    assert!(
        workspace
            .write_text(&relative("too-large.txt"), &too_large)
            .await
            .is_err()
    );
    assert!(!root.join("too-large.txt").exists());
    assert_no_temporary_files(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_shutdown_rejects_new_work_before_filesystem_access() {
    let temporary = TempDir::new("shutdown-reject");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("existing.txt"), b"existing").unwrap();
    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();

    workspace.shutdown().await.unwrap();

    assert_eq!(
        workspace
            .read_text(&relative("existing.txt"), 64)
            .await
            .unwrap_err(),
        WorkspaceError::Closing
    );
    assert_eq!(
        workspace
            .list(&RelativePath::default(), 16)
            .await
            .unwrap_err(),
        WorkspaceError::Closing
    );
    assert_eq!(
        workspace
            .write_text(&relative("created.txt"), "must not write")
            .await
            .unwrap_err(),
        WorkspaceError::Closing
    );
    assert!(!root.join("created.txt").exists());
    assert_no_temporary_files(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_shutdown_drains_a_dropped_admitted_atomic_write() {
    let temporary = TempDir::new("shutdown-dropped");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
    let path = relative("dropped.txt");
    let content = "x".repeat(Workspace::MAX_WRITE_BYTES);
    let mut operation = Box::pin(workspace.write_text(&path, &content));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);

    let _admission_probe = operation.as_mut().poll(&mut context);
    drop(operation);
    workspace.shutdown().await.unwrap();

    assert_eq!(
        fs::read(root.join("dropped.txt")).unwrap(),
        content.as_bytes()
    );
    assert_no_temporary_files(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_shutdown_drains_multiple_concurrent_jobs_and_shutdowns_idempotently() {
    let temporary = TempDir::new("shutdown-many");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
    let paths = [
        relative("one.txt"),
        relative("two.txt"),
        relative("three.txt"),
        relative("four.txt"),
    ];
    let content = "x".repeat(Workspace::MAX_WRITE_BYTES);
    let writes = async {
        tokio::join!(
            workspace.write_text(&paths[0], &content),
            workspace.write_text(&paths[1], &content),
            workspace.write_text(&paths[2], &content),
            workspace.write_text(&paths[3], &content),
        )
    };

    let (results, first_shutdown, second_shutdown) =
        tokio::join!(writes, workspace.shutdown(), workspace.shutdown());

    assert_eq!(first_shutdown, Ok(()));
    assert_eq!(second_shutdown, Ok(()));
    for result in [results.0, results.1, results.2, results.3] {
        assert_eq!(result, Ok(()));
    }
    for path in ["one.txt", "two.txt", "three.txt", "four.txt"] {
        assert_eq!(fs::read(root.join(path)).unwrap(), content.as_bytes());
    }
    assert_no_temporary_files(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_shutdown_is_idempotent() {
    let temporary = TempDir::new("shutdown-idempotent");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    let workspace = Workspace::open(&root, WorkspaceAccess::ReadOnly).unwrap();

    let (first, second) = tokio::join!(workspace.shutdown(), workspace.shutdown());

    assert_eq!(first, Ok(()));
    assert_eq!(second, Ok(()));
    assert_eq!(workspace.shutdown().await, Ok(()));
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_the_last_workspace_handle_keeps_the_admitted_worker_owned() {
    let temporary = TempDir::new("shutdown-last-handle");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
    let path = relative("last-handle.txt");
    let content = "x".repeat(Workspace::MAX_WRITE_BYTES);
    let mut operation = Box::pin(workspace.write_text(&path, &content));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);

    let _admission_probe = operation.as_mut().poll(&mut context);
    drop(operation);
    drop(workspace);

    let target = root.join("last-handle.txt");
    let mut completed = false;
    for _ in 0..100_000 {
        match fs::read(&target) {
            Ok(bytes) => {
                assert_eq!(bytes, content.as_bytes());
                completed = true;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::thread::yield_now();
            }
            Err(error) => panic!("filesystem observation failed: {error}"),
        }
    }
    assert!(completed, "owner-tracked worker did not settle the write");
    assert_no_temporary_files(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_replacement_exposes_only_complete_old_or_new_contents() {
    let temporary = TempDir::new("atomic-reader");
    let root = temporary.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("target.txt"), b"old-content").unwrap();
    let workspace = Arc::new(Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap());
    let reader_workspace = Arc::clone(&workspace);
    let start = Arc::new(Barrier::new(2));
    let reader_start = Arc::clone(&start);
    let reader = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        reader_start.wait();
        runtime.block_on(async move {
            for _ in 0..512 {
                let value = reader_workspace
                    .read_text(&relative("target.txt"), 64)
                    .await
                    .unwrap();
                assert!(value == "old-content" || value == "new-content");
            }
        });
    });

    start.wait();
    for index in 0..128 {
        let content = if index % 2 == 0 {
            "new-content"
        } else {
            "old-content"
        };
        workspace
            .write_text(&relative("target.txt"), content)
            .await
            .unwrap();
    }
    reader.join().unwrap();
    assert_no_temporary_files(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_write_rejects_read_only_missing_parents_directories_and_symlinks() {
    let temporary = TempDir::new("write-reject");
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("file.txt"), b"original").unwrap();
    fs::create_dir(root.join("directory")).unwrap();
    fs::write(outside.join("secret.txt"), b"outside original").unwrap();
    create_file_symlink(&outside.join("secret.txt"), &root.join("escape.txt"));
    create_directory_symlink(&outside, &root.join("escape-dir"));

    let read_only = Workspace::open(&root, WorkspaceAccess::ReadOnly).unwrap();
    let read_only_error = read_only
        .write_text(&relative("file.txt"), "must not write")
        .await
        .unwrap_err();
    assert_redacted(read_only_error, &["file.txt", root.to_str().unwrap()]);
    assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"original");
    assert_no_temporary_files(&root);

    let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
    for path in [
        "",
        "missing/file.txt",
        "directory",
        "escape.txt",
        "escape-dir/file.txt",
    ] {
        let error = workspace
            .write_text(&relative(path), "must not write")
            .await
            .unwrap_err();
        assert_redacted(error, &["secret", outside.to_str().unwrap()]);
        assert_no_temporary_files(&root);
    }
    assert_eq!(
        fs::read(outside.join("secret.txt")).unwrap(),
        b"outside original"
    );
    assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"original");
}

#[test]
fn workspace_public_source_has_no_legacy_or_unsafe_path_seams() {
    let sources = [
        include_str!("../src/workspace/mod.rs"),
        include_str!("../src/workspace/path.rs"),
        include_str!("../src/workspace/root.rs"),
    ];
    for source in sources {
        assert!(!source.contains("crate::wire"));
        assert!(!source.contains("canonicalize"));
        assert!(!source.contains("allow(dead_code"));
        assert!(!source.contains("tokio::spawn"));
        assert!(!source.contains("spawn_blocking"));
        assert!(!source.contains("JoinSet"));
        assert!(!source.contains("tokio::task::JoinHandle"));
    }

    let root_source = include_str!("../src/workspace/root.rs");
    assert!(root_source.contains("CleanupFailed"));
    assert!(root_source.contains("WorkerFailed"));
    assert!(root_source.contains("Closing"));
    assert!(root_source.contains("oneshot::channel"));
    assert!(root_source.contains("BlockingOwner"));
    assert!(root_source.contains("BlockingState"));
    assert!(root_source.contains("std::sync::mpsc"));
    assert!(root_source.contains("sender: Option"));
    assert!(root_source.contains("std::thread::JoinHandle<()>"));
    assert!(root_source.contains("std::thread::Builder::new"));
    assert!(root_source.contains("WorkerExitGuard"));
    assert!(root_source.contains("impl Drop for BlockingOwner"));
    assert!(root_source.contains("handle.join()"));
    assert!(root_source.contains("state.sender.take()"));
    assert!(root_source.contains("state.handle.take()"));
    assert_eq!(root_source.matches("std::thread::Builder::new").count(), 1);
    assert_eq!(root_source.matches(".spawn(move || worker_loop").count(), 1);
    assert!(!root_source.contains("tokio::spawn"));
    assert!(!root_source.contains("spawn_blocking"));
    assert!(!root_source.contains("JoinSet"));
    assert!(!root_source.contains("active_jobs"));
    assert!(!root_source.contains("AtomicUsize"));
    assert!(!root_source.contains("Notify"));
    assert!(!root_source.contains("AbortHandle"));
    assert!(!root_source.contains("tokio::task::JoinHandle"));
    assert!(!root_source.contains("let _ = parent.remove_file"));
    let open_position = root_source
        .find("open_root_capability(root)")
        .expect("root capture call is present");
    let metadata_position = root_source
        .find("symlink_metadata(root)")
        .expect("symlink classification is present");
    assert!(
        open_position < metadata_position,
        "symlink metadata must classify only after no-follow capture fails"
    );
    let entry_start = root_source
        .find("pub struct DirectoryEntry")
        .expect("directory entry is present");
    let entry_end = root_source[entry_start..]
        .find("impl DirectoryEntry")
        .map(|offset| entry_start + offset)
        .expect("directory entry impl is present");
    assert!(!root_source[entry_start..entry_end].contains("Deserialize"));

    let path_source = include_str!("../src/workspace/path.rs");
    assert!(!path_source.contains("Path::components"));
    assert!(!path_source.contains("Component::Prefix"));
    assert!(!path_source.contains("use std::path::{Component"));

    let lib = include_str!("../src/lib.rs");
    assert!(lib.contains("pub mod workspace;"));
    assert!(lib.contains("pub use workspace::"));
}

#[test]
fn cleanup_and_worker_errors_are_redacted() {
    for error in [WorkspaceError::CleanupFailed, WorkspaceError::WorkerFailed] {
        assert_redacted(error, &["target.txt", "/outside/secret"]);
    }
}
