use std::error::Error;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use minicore_runtime::runtime_interface::{
    AgentQuery, AgentQueryResult, CommandRequest, PageRequest, QueryErrorCode, QueryResult,
    RuntimeCommand, RuntimeQuery, SessionCommand, SessionQuery, SessionQueryResult,
};
use minicore_runtime::wire::CommandId;
use minicore_runtime::{
    CompactionSettings, MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError,
};
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

fn create_second_published_g1_agent(root: &Path) {
    const SOURCE_AGENT_ID: &str = "agt_11111111111111111111111111111111";
    const AGENT_ID: &str = "agt_22222222222222222222222222222222";
    const GENERATION_ONE: &str = "00000000000000000001";

    create_private_file(&root.join("reservations/agents").join(AGENT_ID), b"");
    let entity = root.join("agents").join(AGENT_ID);
    create_private_directory(&entity);
    create_private_file(&entity.join("PUBLISHED"), b"");
    create_private_directory(&entity.join("generations"));
    let generation = entity.join("generations").join(GENERATION_ONE);
    create_private_directory(&generation);
    let head = std::str::from_utf8(include_bytes!(
        "../docs/fixtures/durable-store-v1/agent-head.json"
    ))
    .expect("the authoritative Agent head is UTF-8")
    .replace(SOURCE_AGENT_ID, AGENT_ID)
    .replace("Planner", "Reviewer");
    create_private_file(&generation.join("head.json"), head.as_bytes());
    let definition = std::str::from_utf8(include_bytes!(
        "../docs/fixtures/durable-store-v1/agent-definition.json"
    ))
    .expect("the authoritative Agent definition is UTF-8")
    .replace(SOURCE_AGENT_ID, AGENT_ID);
    create_private_file(&generation.join("definition.json"), definition.as_bytes());
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

fn create_published_g2_deleted_agent_generation(root: &Path) {
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
    .expect("the authoritative Agent status head is UTF-8")
    .replace("\"status\":\"disabled\"", "\"status\":\"deleted\"");
    create_private_file(&generation.join("head.json"), head.as_bytes());
    create_private_file(&generation.join("COMMITTED"), b"");
}

#[cfg(windows)]
fn current_host_session_fixture_uri() -> &'static str {
    "file:///C:/work/project"
}

fn session_definition_fixture_from(fixture: &[u8]) -> Vec<u8> {
    #[cfg(windows)]
    {
        let fixture = std::str::from_utf8(fixture).expect("the authoritative fixture is UTF-8");
        let source = "file:///Users/example/project";
        assert_eq!(fixture.matches(source).count(), 1);
        return fixture
            .replace(source, current_host_session_fixture_uri())
            .into_bytes();
    }
    #[cfg(not(windows))]
    fixture.to_vec()
}

fn session_definition_fixture() -> Vec<u8> {
    session_definition_fixture_from(include_bytes!(
        "../docs/fixtures/durable-store-v1/session-definition.json"
    ))
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

fn create_published_g1_fork_session(root: &Path, conversation: &[u8]) {
    const SESSION_ID: &str = "ses_33333333333333333333333333333333";
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
        include_bytes!("../docs/fixtures/durable-store-v1/fork-session-head.json"),
    );
    create_private_file(
        &generation.join("definition.json"),
        &session_definition_fixture_from(include_bytes!(
            "../docs/fixtures/durable-store-v1/fork-session-definition.json"
        )),
    );
    create_private_file(&generation.join("COMMITTED"), b"");
}

fn create_published_g2_session_definition_generation(root: &Path) {
    const SESSION_ID: &str = "ses_22222222222222222222222222222222";
    const GENERATION_TWO: &str = "00000000000000000002";

    let generation = root
        .join("sessions")
        .join(SESSION_ID)
        .join("generations")
        .join(GENERATION_TWO);
    create_private_directory(&generation);
    create_private_file(
        &generation.join("head.json"),
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-2-definition.json"),
    );
    create_private_file(
        &generation.join("definition.json"),
        &session_definition_fixture_from(include_bytes!(
            "../docs/fixtures/durable-store-v1/session-definition-2.json"
        )),
    );
    create_private_file(&generation.join("COMMITTED"), b"");
}

fn create_published_g2_archived_session_generation(root: &Path) {
    const SESSION_ID: &str = "ses_22222222222222222222222222222222";
    const GENERATION_TWO: &str = "00000000000000000002";

    let generation = root
        .join("sessions")
        .join(SESSION_ID)
        .join("generations")
        .join(GENERATION_TWO);
    create_private_directory(&generation);
    create_private_file(
        &generation.join("head.json"),
        include_bytes!("../docs/fixtures/durable-store-v1/session-head-2-lifecycle.json"),
    );
    create_private_file(&generation.join("COMMITTED"), b"");
}

fn create_corrupt_g2_session_same_lifecycle_generation(root: &Path) {
    const SESSION_ID: &str = "ses_22222222222222222222222222222222";
    const GENERATION_TWO: &str = "00000000000000000002";

    let generation = root
        .join("sessions")
        .join(SESSION_ID)
        .join("generations")
        .join(GENERATION_TWO);
    create_private_directory(&generation);
    let head = std::str::from_utf8(include_bytes!(
        "../docs/fixtures/durable-store-v1/session-head-2-lifecycle.json"
    ))
    .expect("the authoritative fixture is UTF-8")
    .replace("\"lifecycle\":\"archived\"", "\"lifecycle\":\"open\"");
    create_private_file(&generation.join("head.json"), head.as_bytes());
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
async fn public_query_lists_the_durable_agent_catalog() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_second_published_g1_agent(root.path());

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the published Agent catalog opens");
    let response = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(1).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect("the public Agent catalog query succeeds");

    let QueryResult::Agent(AgentQueryResult::Agents(page)) = response.data() else {
        panic!("the Agent query returns an Agent page");
    };
    assert_eq!(page.items().len(), 1);
    let agent = &page.items()[0];
    assert_eq!(
        agent.agent_id().to_string(),
        "agt_11111111111111111111111111111111"
    );
    assert_eq!(agent.definition_revision().to_string(), "ar_1");
    assert_eq!(agent.metadata().revision().to_string(), "amr_1");
    assert_eq!(agent.metadata().name(), "Planner");
    assert_eq!(agent.metadata().description(), None);
    assert_eq!(
        agent.status(),
        minicore_runtime::agent_session_lifecycle::AgentStatus::Enabled
    );
    let cursor = page.next_cursor().expect("the Agent page continues");

    let response = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(Some(cursor), NonZeroU32::new(1).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect("the second Agent catalog page succeeds");
    let QueryResult::Agent(AgentQueryResult::Agents(page)) = response.data() else {
        panic!("the continuation returns an Agent page");
    };
    assert_eq!(page.items().len(), 1);
    assert_eq!(
        page.items()[0].agent_id().to_string(),
        "agt_22222222222222222222222222222222"
    );
    assert_eq!(page.items()[0].metadata().name(), "Reviewer");
    assert!(page.next_cursor().is_none());

    let response = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(1).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect("a fresh Agent page succeeds before restart");
    let QueryResult::Agent(AgentQueryResult::Agents(page)) = response.data() else {
        panic!("the fresh Agent query returns an Agent page");
    };
    let restart_cursor = page.next_cursor().expect("the fresh page continues");

    let error = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(201).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect_err("a page larger than the selected limit is rejected");
    assert_eq!(error.code(), QueryErrorCode::InvalidArgument);

    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Agent catalog reopens");
    let error = reopened
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(Some(restart_cursor), NonZeroU32::new(1).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect_err("a cursor cannot survive Runtime restart");
    assert_eq!(error.code(), QueryErrorCode::StaleCursor);
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_catalog_queries_apply_deleted_and_archived_filters() {
    let agent_root = TempRoot::new();
    create_published_g1_agent_store(agent_root.path());
    create_published_g2_deleted_agent_generation(agent_root.path());

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(agent_root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Deleted Agent catalog opens");
    let response = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(1).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect("the default Agent filter succeeds");
    let QueryResult::Agent(AgentQueryResult::Agents(page)) = response.data() else {
        panic!("the Agent filter returns an Agent page");
    };
    assert!(page.items().is_empty());

    let response = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(1).unwrap()),
            include_deleted: true,
        }))
        .await
        .expect("the inclusive Agent filter succeeds");
    let QueryResult::Agent(AgentQueryResult::Agents(page)) = response.data() else {
        panic!("the inclusive Agent filter returns an Agent page");
    };
    assert_eq!(page.items().len(), 1);
    assert_eq!(
        page.items()[0].status(),
        minicore_runtime::agent_session_lifecycle::AgentStatus::Deleted
    );
    runtime.shutdown().await;

    let session_root = TempRoot::new();
    create_published_g1_agent_store(session_root.path());
    create_published_g1_session(
        session_root.path(),
        include_bytes!("../docs/fixtures/durable-store-v1/fork-source.jsonl"),
    );
    create_published_g2_archived_session_generation(session_root.path());

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(session_root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Archived Session catalog opens");
    let response = runtime
        .query(RuntimeQuery::Session(SessionQuery::ListSessions {
            page: PageRequest::new(None, NonZeroU32::new(1).unwrap()),
            include_archived: false,
        }))
        .await
        .expect("the default Session filter succeeds");
    let QueryResult::Session(SessionQueryResult::Sessions(page)) = response.data() else {
        panic!("the Session filter returns a Session page");
    };
    assert!(page.items().is_empty());

    let response = runtime
        .query(RuntimeQuery::Session(SessionQuery::ListSessions {
            page: PageRequest::new(None, NonZeroU32::new(1).unwrap()),
            include_archived: true,
        }))
        .await
        .expect("the inclusive Session filter succeeds");
    let QueryResult::Session(SessionQueryResult::Sessions(page)) = response.data() else {
        panic!("the inclusive Session filter returns a Session page");
    };
    assert_eq!(page.items().len(), 1);
    assert_eq!(
        page.items()[0].lifecycle(),
        minicore_runtime::runtime_interface::SessionLifecycleView::Archived
    );
    runtime.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_query_pages_the_durable_session_catalog_in_canonical_order() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_published_g1_session(
        root.path(),
        include_bytes!("../docs/fixtures/durable-store-v1/fork-source.jsonl"),
    );
    create_published_g1_fork_session(
        root.path(),
        include_bytes!("../docs/fixtures/durable-store-v1/fork-child.jsonl"),
    );

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the published Session catalog opens");
    let first = runtime
        .query(RuntimeQuery::Session(SessionQuery::ListSessions {
            page: PageRequest::new(None, NonZeroU32::new(1).unwrap()),
            include_archived: false,
        }))
        .await
        .expect("the first Session catalog page succeeds");
    let QueryResult::Session(SessionQueryResult::Sessions(first)) = first.data() else {
        panic!("the Session query returns a Session page");
    };
    assert_eq!(first.items().len(), 1);
    assert_eq!(
        first.items()[0].session_id().to_string(),
        "ses_33333333333333333333333333333333"
    );
    assert!(first.items()[0].forked());
    let cursor = first.next_cursor().expect("the first page continues");

    let error = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(Some(cursor), NonZeroU32::new(1).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect_err("a Session cursor cannot be reused for an Agent query");
    assert_eq!(error.code(), QueryErrorCode::StaleCursor);
    let error = runtime
        .query(RuntimeQuery::Session(SessionQuery::ListSessions {
            page: PageRequest::new(Some(cursor), NonZeroU32::new(1).unwrap()),
            include_archived: true,
        }))
        .await
        .expect_err("a Session cursor cannot be reused with another filter");
    assert_eq!(error.code(), QueryErrorCode::StaleCursor);

    runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Fork {
                source_session_id: "ses_22222222222222222222222222222222".parse().unwrap(),
                anchor: minicore_runtime::agent_session_lifecycle::ForkAnchor::Genesis,
            }),
        ))
        .await
        .expect("a new Fork publishes after the first page snapshot");

    let second = runtime
        .query(RuntimeQuery::Session(SessionQuery::ListSessions {
            page: PageRequest::new(Some(cursor), NonZeroU32::new(1).unwrap()),
            include_archived: false,
        }))
        .await
        .expect("the second Session catalog page succeeds");
    let QueryResult::Session(SessionQueryResult::Sessions(second)) = second.data() else {
        panic!("the continuation returns a Session page");
    };
    assert_eq!(second.items().len(), 1);
    assert_eq!(
        second.items()[0].session_id().to_string(),
        "ses_22222222222222222222222222222222"
    );
    assert!(!second.items()[0].forked());
    assert!(second.next_cursor().is_none());

    let error = runtime
        .query(RuntimeQuery::Session(SessionQuery::ListSessions {
            page: PageRequest::new(Some(cursor), NonZeroU32::new(1).unwrap()),
            include_archived: false,
        }))
        .await
        .expect_err("a consumed cursor is stale");
    assert_eq!(error.code(), QueryErrorCode::StaleCursor);

    runtime.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_query_returns_durable_session_fork_provenance() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_published_g1_session(root.path(), b"source conversation");
    create_published_g1_fork_session(
        root.path(),
        include_bytes!("../docs/fixtures/durable-store-v1/fork-child.jsonl"),
    );

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the published Fork graph opens");
    let child_id = "ses_33333333333333333333333333333333".parse().unwrap();
    let response = runtime
        .query(RuntimeQuery::Session(
            SessionQuery::GetSessionForkProvenance {
                session_id: child_id,
            },
        ))
        .await
        .expect("the Fork provenance query succeeds");
    let QueryResult::Session(SessionQueryResult::ForkProvenance(Some(provenance))) =
        response.data()
    else {
        panic!("the Fork child returns durable provenance");
    };
    assert_eq!(
        provenance.source_session_id().to_string(),
        "ses_22222222222222222222222222222222"
    );
    assert_eq!(
        provenance.source(),
        minicore_runtime::agent_session_lifecycle::ForkSourceKind::RecordedHistory
    );
    assert!(matches!(
        provenance.anchor(),
        minicore_runtime::agent_session_lifecycle::ForkAnchor::AfterUserMessage { item_id }
            if item_id.to_string() == "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    ));

    let source_id = "ses_22222222222222222222222222222222".parse().unwrap();
    let response = runtime
        .query(RuntimeQuery::Session(
            SessionQuery::GetSessionForkProvenance {
                session_id: source_id,
            },
        ))
        .await
        .expect("the ordinary Session provenance query succeeds");
    assert!(matches!(
        response.data(),
        QueryResult::Session(SessionQueryResult::ForkProvenance(None))
    ));

    runtime.shutdown().await;
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
async fn public_runtime_recovers_fork_without_touching_invalid_conversations() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    let source_conversation = b"\xff source invalid conversation bytes\x00\n";
    let child_conversation = b"\xfe child invalid conversation bytes\x00\n";
    create_published_g1_session(root.path(), source_conversation);
    create_published_g1_fork_session(root.path(), child_conversation);
    let source_path = root
        .path()
        .join("sessions/ses_22222222222222222222222222222222/conversation.jsonl");
    let child_path = root
        .path()
        .join("sessions/ses_33333333333333333333333333333333/conversation.jsonl");
    let source_before = fs::read(&source_path).expect("the source conversation is readable");
    let child_before = fs::read(&child_path).expect("the child conversation is readable");

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the public runtime recovers the ordinary source and Fork child");
    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the recovered source and Fork child reopen publicly");
    reopened.shutdown().await;

    assert_eq!(
        fs::read(source_path).expect("the source conversation remains readable"),
        source_before
    );
    assert_eq!(
        fs::read(child_path).expect("the child conversation remains readable"),
        child_before
    );
}

#[tokio::test(flavor = "current_thread")]
async fn public_runtime_redacts_a_fork_with_a_missing_published_source() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_private_file(
        &root
            .path()
            .join("reservations/sessions/ses_22222222222222222222222222222222"),
        b"",
    );
    create_published_g1_fork_session(
        root.path(),
        include_bytes!("../docs/fixtures/durable-store-v1/fork-child.jsonl"),
    );
    let private_root = root.path().to_string_lossy();

    let error = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect_err("a Fork source reservation is not a published Session catalog entry");

    assert_eq!(error, RuntimeInitializationError::DurableStateCorrupt);
    for secret in [
        private_root.as_ref(),
        "ses_22222222222222222222222222222222",
        "ses_33333333333333333333333333333333",
        "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "session-notes",
        "hello",
    ] {
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }
    assert!(Error::source(&error).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn public_runtime_recovers_an_authoritative_ordinary_g1_g2_definition_session_chain() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    let conversation =
        b"\xff arbitrary conversation bytes remain a Store-open opaque payload\x00\n";
    create_published_g1_session(root.path(), conversation);
    create_published_g2_session_definition_generation(root.path());
    let conversation_path = root
        .path()
        .join("sessions/ses_22222222222222222222222222222222/conversation.jsonl");
    let before = fs::read(&conversation_path).expect("the physical conversation is readable");

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the authoritative ordinary G1/G2 definition chain opens publicly");
    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the recovered ordinary G1/G2 definition chain reopens publicly");
    reopened.shutdown().await;

    assert_eq!(
        fs::read(conversation_path).expect("the physical conversation remains readable"),
        before,
        "public Store recovery leaves invalid conversation bytes untouched"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn public_runtime_maps_a_corrupt_g2_session_to_a_redacted_error() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_published_g1_session(root.path(), b"opaque conversation bytes");
    create_corrupt_g2_session_same_lifecycle_generation(root.path());
    let private_root = root.path().to_string_lossy();

    let error = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect_err("a committed same-lifecycle Session generation is corrupt");

    assert_eq!(error, RuntimeInitializationError::DurableStateCorrupt);
    for secret in [private_root.as_ref(), "session-notes", "Project session"] {
        assert!(!format!("{error:?}").contains(secret));
        assert!(!error.to_string().contains(secret));
    }
    assert!(Error::source(&error).is_none());
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
        RuntimeInitializationError::InvalidConfiguration,
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

#[test]
fn runtime_rejects_invalid_compaction_settings_before_opening_storage() {
    let root = TempRoot::new();
    let host = Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the host runtime builds");
    let settings = CompactionSettings {
        summary_min_output_tokens: NonZeroU32::new(2_049).unwrap(),
        ..CompactionSettings::default()
    };

    let error = host
        .block_on(MiniCoreRuntime::open(
            MiniCoreRuntimeConfig::new(root.path().to_owned()).with_compaction_settings(settings),
            host.handle().clone(),
        ))
        .expect_err("invalid compaction settings are rejected");

    assert_eq!(error, RuntimeInitializationError::InvalidConfiguration);
    assert!(!root.path().exists());
}
