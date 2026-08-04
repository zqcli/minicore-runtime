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

fn create_published_g2_definition_generation(root: &Path) {
    const AGENT_ID: &str = "agt_11111111111111111111111111111111";
    const GENERATION_TWO: &str = "00000000000000000002";

    let generation = root
        .join("agents")
        .join(AGENT_ID)
        .join("generations")
        .join(GENERATION_TWO);
    create_private_directory(&generation);
    create_private_file(
        &generation.join("head.json"),
        include_bytes!("../docs/fixtures/durable-store-v1/agent-head-2-definition.json"),
    );
    create_private_file(
        &generation.join("definition.json"),
        include_bytes!("../docs/fixtures/durable-store-v1/agent-definition-2.json"),
    );
    create_private_file(&generation.join("COMMITTED"), b"");
}

fn session_definition_fixture() -> Vec<u8> {
    let fixture = include_bytes!("../docs/fixtures/durable-store-v1/session-definition.json");
    #[cfg(windows)]
    {
        let fixture = std::str::from_utf8(fixture).expect("the authoritative fixture is UTF-8");
        return fixture
            .replacen(
                "file:///Users/example/project",
                "file:///C:/work/project",
                1,
            )
            .into_bytes();
    }
    #[cfg(not(windows))]
    fixture.to_vec()
}

fn create_published_g1_session(root: &Path, conversation: &[u8]) {
    const SESSION_ID: &str = "ses_22222222222222222222222222222222";
    const GENERATION_ONE: &str = "00000000000000000001";

    create_private_file(&root.join("reservations/sessions").join(SESSION_ID), b"");
    let entity = root.join("sessions").join(SESSION_ID);
    create_private_directory(&entity);
    create_private_file(&entity.join("PUBLISHED"), b"");
    create_private_file(&entity.join("conversation.jsonl"), conversation);
    create_private_directory(&entity.join("generations"));
    let generation = entity.join("generations").join(GENERATION_ONE);
    create_private_directory(&generation);
    create_private_file(
        &generation.join("head.json"),
        include_bytes!("../docs/fixtures/durable-store-v1/session-head.json"),
    );
    create_private_file(
        &generation.join("definition.json"),
        &session_definition_fixture(),
    );
    create_private_file(&generation.join("COMMITTED"), b"");
}

fn create_corrupt_g2_same_status_generation(root: &Path) {
    const AGENT_ID: &str = "agt_11111111111111111111111111111111";
    const GENERATION_TWO: &str = "00000000000000000002";

    let generation = root
        .join("agents")
        .join(AGENT_ID)
        .join("generations")
        .join(GENERATION_TWO);
    create_private_directory(&generation);
    let head = std::str::from_utf8(include_bytes!(
        "../docs/fixtures/durable-store-v1/agent-head-2-status.json"
    ))
    .expect("the authoritative fixture is UTF-8")
    .replace("\"status\":\"disabled\"", "\"status\":\"enabled\"");
    create_private_file(&generation.join("head.json"), head.as_bytes());
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
async fn public_runtime_recovers_an_ordinary_g1_session_and_preserves_invalid_conversation_bytes() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    let conversation = b"\xff arbitrary conversation bytes are not parsed at Store open\x00\n";
    create_published_g1_session(root.path(), conversation);
    let conversation_path = root
        .path()
        .join("sessions/ses_22222222222222222222222222222222/conversation.jsonl");
    let before = fs::read(&conversation_path).expect("the physical conversation is readable");

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("an ordinary published G1 Session opens through the public runtime");
    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the recovered G1 Session store reopens after shutdown");
    reopened.shutdown().await;

    assert_eq!(
        fs::read(conversation_path).expect("the conversation remains readable"),
        before,
        "store recovery leaves all conversation bytes untouched"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn public_runtime_maps_a_session_with_a_missing_agent_ref_to_redacted_corruption() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_published_g1_session(root.path(), b"arbitrary");
    fs::remove_dir_all(
        root.path()
            .join("agents/agt_11111111111111111111111111111111"),
    )
    .expect("the Agent entity is removed while its orphan reservation remains");
    let private_root = root.path().to_string_lossy();

    let error = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect_err("a Session cannot substitute a missing Agent ref with a current Agent");

    assert_eq!(error, RuntimeInitializationError::DurableStateCorrupt);
    assert!(!format!("{error:?}").contains(private_root.as_ref()));
    assert!(!error.to_string().contains(private_root.as_ref()));
    assert!(!format!("{error:?}").contains("session-notes"));
    assert!(!error.to_string().contains("session-notes"));
    assert!(Error::source(&error).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn published_committed_g1_g2_definition_agent_store_opens_shuts_down_and_reopens() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_published_g2_definition_generation(root.path());

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the public runtime recovers the authoritative G1/G2 definition chain");
    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the public runtime reopens the recovered G1/G2 definition chain");
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_runtime_maps_a_corrupt_same_status_no_op_and_redacts_it() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_corrupt_g2_same_status_generation(root.path());
    let private_root = root.path().to_string_lossy();

    let error = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect_err("a committed same-status generation is corrupt rather than a G1 fallback");

    assert_eq!(error, RuntimeInitializationError::DurableStateCorrupt);
    assert!(!format!("{error:?}").contains(private_root.as_ref()));
    assert!(!error.to_string().contains(private_root.as_ref()));
    assert!(!format!("{error:?}").contains("Planner"));
    assert!(!error.to_string().contains("Planner"));
    assert!(Error::source(&error).is_none());
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
