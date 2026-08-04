use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use minicore_runtime::{MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError};
use tokio::runtime::{Builder, Handle};

static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        loop {
            let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
            assert_ne!(suffix, 0, "test root suffix must be nonzero");
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-foundation-{}-{suffix}",
                std::process::id()
            ));
            if !path.exists() {
                return Self { path };
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if self.path.exists() {
            fs::remove_dir_all(&self.path).expect("the test root is removed deterministically");
        }
    }
}

fn create_existing_private_root(path: &Path) {
    fs::create_dir(path).expect("the existing test root is created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("the existing test root receives its required private mode");
    }
}

fn create_private_directory(path: &Path) {
    fs::create_dir(path).expect("the Store V1 directory is created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("the Store V1 directory receives its private mode");
    }
}

fn create_private_file(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("the Store V1 file is created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("the Store V1 file receives its private mode");
    }
}

fn create_published_g1_agent_store(root: &Path) {
    const AGENT_ID: &str = "agt_11111111111111111111111111111111";
    const GENERATION_ONE: &str = "00000000000000000001";

    create_existing_private_root(root);
    create_private_file(&root.join(".minicore.lock"), b"");
    create_private_file(&root.join("MINICORE_STORE_V1"), b"");
    create_private_directory(&root.join("reservations"));
    create_private_directory(&root.join("reservations/agents"));
    create_private_directory(&root.join("reservations/sessions"));
    create_private_file(&root.join("reservations/agents").join(AGENT_ID), b"");
    create_private_directory(&root.join("agents"));
    create_private_directory(&root.join("sessions"));

    let entity = root.join("agents").join(AGENT_ID);
    create_private_directory(&entity);
    create_private_file(&entity.join("PUBLISHED"), b"");
    create_private_directory(&entity.join("generations"));
    let generation = entity.join("generations").join(GENERATION_ONE);
    create_private_directory(&generation);
    create_private_file(
        &generation.join("head.json"),
        include_bytes!("../docs/fixtures/durable-store-v1/agent-head.json"),
    );
    create_private_file(
        &generation.join("definition.json"),
        include_bytes!("../docs/fixtures/durable-store-v1/agent-definition.json"),
    );
    create_private_file(&generation.join("COMMITTED"), b"");
}

#[tokio::test(flavor = "current_thread")]
async fn nonexistent_root_opens_shuts_down_and_reopens() {
    let root = TempRoot::new();

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("a nonexistent root bootstraps");
    assert!(
        !format!("{runtime:?}").contains(root.path().to_string_lossy().as_ref()),
        "runtime debug output redacts its durable root"
    );
    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the closed store reopens");
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn published_committed_g1_agent_store_opens_shuts_down_and_reopens() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the published G1 Agent store opens through the public runtime");
    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the recovered G1 Agent store reopens after shutdown");
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_is_idempotent_and_releases_the_root_lease() {
    let root = TempRoot::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the runtime opens");

    tokio::join!(runtime.shutdown(), runtime.shutdown());
    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("shutdown releases the root lease");
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_runtime_reports_store_in_use_until_the_first_shuts_down() {
    let root = TempRoot::new();
    let first = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the first runtime owns the root lease");

    let second = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await;
    assert!(matches!(
        second,
        Err(RuntimeInitializationError::StoreInUse)
    ));

    first.shutdown().await;

    let second = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("a runtime can acquire the released lease");
    second.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_current_thread_host_with_time_opens_the_runtime() {
    let root = TempRoot::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("a current-thread host with time is supported");

    runtime.shutdown().await;
}

#[test]
fn a_multi_thread_host_with_time_opens_the_runtime() {
    let root = TempRoot::new();
    let host = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("the host runtime builds");

    host.block_on(async {
        let runtime = MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            host.handle().clone(),
        )
        .await
        .expect("a multi-thread host with time is supported");
        runtime.shutdown().await;
    });
}

#[test]
fn a_missing_time_driver_returns_a_typed_error_without_panicking() {
    let root = TempRoot::new();
    let host = Builder::new_current_thread()
        .build()
        .expect("the host runtime builds without a time driver");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        host.block_on(MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            host.handle().clone(),
        ))
    }));

    assert!(matches!(
        result,
        Ok(Err(
            RuntimeInitializationError::RuntimeDependencyUnavailable
        ))
    ));
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn an_existing_root_with_a_wrong_unix_mode_is_storage_unavailable_and_redacted() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempRoot::new();
    create_existing_private_root(root.path());
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755))
        .expect("the root receives the wrong Unix mode");
    let unsafe_text = root.path().to_string_lossy();

    let error = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect_err("a root with the wrong Unix mode is rejected");

    assert_eq!(error, RuntimeInitializationError::StorageUnavailable);
    assert!(!format!("{error:?}").contains(unsafe_text.as_ref()));
    assert!(!error.to_string().contains(unsafe_text.as_ref()));
    assert!(Error::source(&error).is_none());
}

#[test]
fn runtime_initialization_errors_and_debug_output_are_redacted() {
    let root = TempRoot::new();
    let unsafe_text = root.path().to_string_lossy();
    let config = MiniCoreRuntimeConfig::new(root.path().to_owned());

    assert!(!format!("{config:?}").contains(unsafe_text.as_ref()));

    create_existing_private_root(root.path());
    let unknown = root.path().join("foreign-store-data");
    fs::write(&unknown, b"must produce a real initialization error")
        .expect("the unknown markerless entry is created");
    let host = Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the host runtime builds");
    let actual_error = host
        .block_on(MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()),
            host.handle().clone(),
        ))
        .expect_err("the unknown markerless root is rejected");

    assert_eq!(
        actual_error,
        RuntimeInitializationError::UnsupportedStoreFormat
    );
    assert!(!format!("{actual_error:?}").contains(unsafe_text.as_ref()));
    assert!(!actual_error.to_string().contains(unsafe_text.as_ref()));
    assert!(Error::source(&actual_error).is_none());

    for error in [
        RuntimeInitializationError::RuntimeDependencyUnavailable,
        RuntimeInitializationError::StoreInUse,
        RuntimeInitializationError::UnsupportedStoreFormat,
        RuntimeInitializationError::DurableStateCorrupt,
        RuntimeInitializationError::DurableStateTooLarge,
        RuntimeInitializationError::StorageUnavailable,
    ] {
        assert!(!format!("{error:?}").contains(unsafe_text.as_ref()));
        assert!(!error.to_string().contains(unsafe_text.as_ref()));
        assert!(!format!("{error:?}").contains("No such file"));
        assert!(!error.to_string().contains("No such file"));
        assert!(Error::source(&error).is_none());
    }
}
