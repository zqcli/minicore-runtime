use std::error::Error;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use minicore_runtime::agent_session_lifecycle::{
    AgentDefinitionPatch, AgentMetadataPatch, AgentRevisionRef, AgentStatus, AgentUsableStatus,
    NewAgentDefinition, NewAgentMetadata, OptionalTextPatch, SessionMetadataPatch,
    SessionModelConfig,
};
use minicore_runtime::model_gateway::{ModelSelection, ReasoningPreference};
use minicore_runtime::prompt::{AgentPromptSelection, SessionPromptSelection};
use minicore_runtime::runtime_interface::{
    AgentCommand, AgentQuery, AgentQueryResult, CommandCompletion, CommandErrorCode,
    CommandOutcome, CommandRequest, EventFrame, EventRoute, NewSessionDefinition,
    NewSessionMetadata, PageRequest, PublicSubject, QueryErrorCode, QueryResult, RetryAdvice,
    RuntimeCommand, RuntimeEventDetail, RuntimeQuery, RuntimeStateEventKind, SessionCommand,
    SessionDefinitionPatch, SessionExecutionView, SessionLifecycleView, SessionQuery,
    SessionQueryResult, SessionStateEventKind, SnapshotRequest, SnapshotResponse, StateEventMsg,
    SubscriptionRequest, SubscriptionScope,
};
use minicore_runtime::wire::{
    CanonicalFileUri, CommandId, SessionDefinitionRevision, SessionId, SessionMetadataRevision,
};
use minicore_runtime::workspace::{
    RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspaceRootInput,
    WorkspaceRootKey, WorkspaceSourcePolicy,
};
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
async fn public_agent_lifecycle_publishes_real_changes_and_survives_restart() {
    let root = TempRoot::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    let create_id = CommandId::generate().unwrap();
    let created = runtime
        .dispatch(CommandRequest::new(
            create_id,
            RuntimeCommand::Agent(AgentCommand::Create {
                definition: NewAgentDefinition::new(AgentPromptSelection::new(Vec::new()).unwrap()),
                metadata: NewAgentMetadata::new(
                    "Lifecycle Agent",
                    Some("public lifecycle coverage"),
                )
                .unwrap(),
            }),
        ))
        .await
        .expect("Agent Create dispatches");
    let CommandCompletion::Completed {
        outcome:
            CommandOutcome::AgentCreated {
                agent_id,
                definition_revision,
                metadata_revision,
            },
        output: None,
    } = created.completion()
    else {
        panic!("Agent Create returns its typed outcome");
    };
    let agent_id = *agent_id;
    assert_eq!(definition_revision.get(), 1);
    assert_eq!(metadata_revision.get(), 1);

    let Some(EventFrame::State(created_event)) = events.recv().await else {
        panic!("Agent Create publishes one StateEvent");
    };
    assert_eq!(created_event.command_id(), Some(create_id));
    assert_eq!(created_event.route(), EventRoute::Agent { agent_id });
    let StateEventMsg::Runtime {
        kind: RuntimeStateEventKind::AgentCreated,
        detail: Some(RuntimeEventDetail::AgentChanged { agent }),
        ..
    } = created_event.msg()
    else {
        panic!("Agent Create publishes its safe Agent summary");
    };
    assert_eq!(agent.agent_id(), agent_id);
    assert_eq!(agent.status(), AgentStatus::Enabled);

    let disable_id = CommandId::generate().unwrap();
    let disabled = runtime
        .dispatch(CommandRequest::new(
            disable_id,
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status: AgentStatus::Enabled,
                status: AgentUsableStatus::Disabled,
            }),
        ))
        .await
        .expect("Agent Disable dispatches");
    assert!(matches!(
        disabled.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentStatusChanged {
                status: AgentStatus::Disabled
            },
            output: None,
        }
    ));
    let Some(EventFrame::State(disabled_event)) = events.recv().await else {
        panic!("Agent Disable publishes one StateEvent");
    };
    assert_eq!(disabled_event.command_id(), Some(disable_id));
    assert!(matches!(
        disabled_event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::AgentStatusChanged,
            detail: Some(RuntimeEventDetail::AgentChanged { agent }),
            ..
        } if agent.status() == AgentStatus::Disabled
    ));

    let repeated_disable = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status: AgentStatus::Disabled,
                status: AgentUsableStatus::Disabled,
            }),
        ))
        .await
        .expect("idempotent Agent Disable dispatches");
    assert!(matches!(
        repeated_disable.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "idempotent Agent status changes do not publish"
    );

    let enabled = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status: AgentStatus::Disabled,
                status: AgentUsableStatus::Enabled,
            }),
        ))
        .await
        .expect("Agent Enable dispatches");
    assert!(matches!(
        enabled.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentStatusChanged {
                status: AgentStatus::Enabled
            },
            output: None,
        }
    ));
    assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

    let stale = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status: AgentStatus::Disabled,
                status: AgentUsableStatus::Disabled,
            }),
        ))
        .await
        .expect("stale Agent status dispatches");
    assert!(matches!(
        stale.completion(),
        CommandCompletion::Rejected(error)
            if error.code() == CommandErrorCode::StaleRevision
                && error.subject().is_some_and(|subject| {
                    matches!(subject, minicore_runtime::runtime_interface::PublicSubject::Agent(id) if *id == agent_id)
                })
    ));

    let delete_id = CommandId::generate().unwrap();
    let deleted = runtime
        .dispatch(CommandRequest::new(
            delete_id,
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id,
                expected_status: AgentStatus::Enabled,
            }),
        ))
        .await
        .expect("Agent Delete dispatches");
    assert!(matches!(
        deleted.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentDeleted,
            output: None,
        }
    ));
    let Some(EventFrame::State(deleted_event)) = events.recv().await else {
        panic!("Agent Delete publishes one StateEvent");
    };
    assert_eq!(deleted_event.command_id(), Some(delete_id));
    assert!(matches!(
        deleted_event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::AgentStatusChanged,
            detail: Some(RuntimeEventDetail::AgentChanged { agent }),
            ..
        } if agent.status() == AgentStatus::Deleted
    ));

    let repeated_delete = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id,
                expected_status: AgentStatus::Deleted,
            }),
        ))
        .await
        .expect("idempotent Agent Delete dispatches");
    assert!(matches!(
        repeated_delete.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentDeleted,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "idempotent Agent Delete does not publish"
    );

    let after_delete = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status: AgentStatus::Deleted,
                status: AgentUsableStatus::Enabled,
            }),
        ))
        .await
        .expect("deleted Agent status dispatches");
    assert!(matches!(
        after_delete.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::AgentDeleted
    ));

    let missing = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id: "agt_ffffffffffffffffffffffffffffffff".parse().unwrap(),
                expected_status: AgentStatus::Enabled,
            }),
        ))
        .await
        .expect("missing Agent Delete dispatches");
    assert!(matches!(
        missing.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::NotFound
    ));

    let visible = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(10).unwrap()),
            include_deleted: false,
        }))
        .await
        .expect("the ordinary Agent catalog query succeeds");
    let QueryResult::Agent(AgentQueryResult::Agents(visible)) = visible.data() else {
        panic!("the Agent query returns an Agent page");
    };
    assert!(visible.items().is_empty());

    let inclusive = runtime
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(10).unwrap()),
            include_deleted: true,
        }))
        .await
        .expect("the inclusive Agent catalog query succeeds");
    let QueryResult::Agent(AgentQueryResult::Agents(inclusive)) = inclusive.data() else {
        panic!("the inclusive Agent query returns an Agent page");
    };
    assert_eq!(inclusive.items().len(), 1);
    assert_eq!(inclusive.items()[0].agent_id(), agent_id);
    assert_eq!(inclusive.items()[0].status(), AgentStatus::Deleted);

    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime reopens");
    let recovered = reopened
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(10).unwrap()),
            include_deleted: true,
        }))
        .await
        .expect("the recovered Agent catalog query succeeds");
    let QueryResult::Agent(AgentQueryResult::Agents(recovered)) = recovered.data() else {
        panic!("the recovered Agent query returns an Agent page");
    };
    assert_eq!(recovered.items().len(), 1);
    assert_eq!(recovered.items()[0].agent_id(), agent_id);
    assert_eq!(recovered.items()[0].status(), AgentStatus::Deleted);
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_agent_definition_and_metadata_cas_publish_and_survive_restart() {
    let root = TempRoot::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    let missing_agent_id = "agt_ffffffffffffffffffffffffffffffff".parse().unwrap();
    for command in [
        RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
            agent_id: missing_agent_id,
            expected_revision: "ar_1".parse().unwrap(),
            patch: AgentDefinitionPatch::new(None),
        }),
        RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
            agent_id: missing_agent_id,
            expected_revision: "amr_1".parse().unwrap(),
            patch: AgentMetadataPatch::new(None::<&str>, OptionalTextPatch::keep()).unwrap(),
        }),
    ] {
        let response = runtime
            .dispatch(CommandRequest::new(CommandId::generate().unwrap(), command))
            .await
            .expect("missing Agent update dispatches");
        assert!(matches!(
            response.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::NotFound
                    && error.subject()
                        == Some(&minicore_runtime::runtime_interface::PublicSubject::Agent(
                            missing_agent_id
                        ))
        ));
    }

    let created = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::Create {
                definition: NewAgentDefinition::new(AgentPromptSelection::new(Vec::new()).unwrap()),
                metadata: NewAgentMetadata::new("Planner", Some("Initial description")).unwrap(),
            }),
        ))
        .await
        .expect("Agent Create dispatches");
    let CommandCompletion::Completed {
        outcome: CommandOutcome::AgentCreated { agent_id, .. },
        output: None,
    } = created.completion()
    else {
        panic!("Agent Create returns its typed outcome");
    };
    let agent_id = *agent_id;
    assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

    let definition_id = CommandId::generate().unwrap();
    let definition = runtime
        .dispatch(CommandRequest::new(
            definition_id,
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision: "ar_1".parse().unwrap(),
                patch: AgentDefinitionPatch::new(Some(
                    AgentPromptSelection::new(vec!["base".parse().unwrap()]).unwrap(),
                )),
            }),
        ))
        .await
        .expect("Agent definition update dispatches");
    assert!(matches!(
        definition.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentDefinitionUpdated {
                definition_revision
            },
            output: None,
        } if definition_revision.get() == 2
    ));
    let Some(EventFrame::State(definition_event)) = events.recv().await else {
        panic!("Agent definition update publishes one StateEvent");
    };
    assert_eq!(definition_event.command_id(), Some(definition_id));
    assert!(matches!(
        definition_event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::AgentDefinitionUpdated,
            detail: Some(RuntimeEventDetail::AgentChanged { agent }),
            ..
        } if agent.definition_revision().get() == 2
            && agent.metadata().revision().get() == 1
            && agent.status() == AgentStatus::Enabled
    ));

    let definition_noop = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision: "ar_2".parse().unwrap(),
                patch: AgentDefinitionPatch::new(None),
            }),
        ))
        .await
        .expect("empty Agent definition patch dispatches");
    assert!(matches!(
        definition_noop.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    let stale_definition = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision: "ar_1".parse().unwrap(),
                patch: AgentDefinitionPatch::new(None),
            }),
        ))
        .await
        .expect("stale Agent definition update dispatches");
    assert!(matches!(
        stale_definition.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::StaleRevision
    ));

    let metadata_id = CommandId::generate().unwrap();
    let metadata = runtime
        .dispatch(CommandRequest::new(
            metadata_id,
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision: "amr_1".parse().unwrap(),
                patch: AgentMetadataPatch::new(
                    Some("Planner v2"),
                    OptionalTextPatch::set("Revised description").unwrap(),
                )
                .unwrap(),
            }),
        ))
        .await
        .expect("Agent metadata update dispatches");
    assert!(matches!(
        metadata.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 2
    ));
    let Some(EventFrame::State(metadata_event)) = events.recv().await else {
        panic!("Agent metadata update publishes one StateEvent");
    };
    assert_eq!(metadata_event.command_id(), Some(metadata_id));
    assert!(matches!(
        metadata_event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::AgentMetadataUpdated,
            detail: Some(RuntimeEventDetail::AgentChanged { agent }),
            ..
        } if agent.definition_revision().get() == 2
            && agent.metadata().revision().get() == 2
            && agent.metadata().name() == "Planner v2"
            && agent.metadata().description() == Some("Revised description")
    ));

    let metadata_noop = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision: "amr_2".parse().unwrap(),
                patch: AgentMetadataPatch::new(None::<&str>, OptionalTextPatch::keep()).unwrap(),
            }),
        ))
        .await
        .expect("empty Agent metadata patch dispatches");
    assert!(matches!(
        metadata_noop.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    let stale_metadata = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision: "amr_1".parse().unwrap(),
                patch: AgentMetadataPatch::new(None::<&str>, OptionalTextPatch::keep()).unwrap(),
            }),
        ))
        .await
        .expect("stale Agent metadata update dispatches");
    assert!(matches!(
        stale_metadata.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::StaleRevision
    ));

    let cleared = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision: "amr_2".parse().unwrap(),
                patch: AgentMetadataPatch::new(None::<&str>, OptionalTextPatch::clear()).unwrap(),
            }),
        ))
        .await
        .expect("Agent description clear dispatches");
    assert!(matches!(
        cleared.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 3
    ));
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::State(event))
            if matches!(
                event.msg(),
                StateEventMsg::Runtime {
                    kind: RuntimeStateEventKind::AgentMetadataUpdated,
                    detail: Some(RuntimeEventDetail::AgentChanged { agent }),
                    ..
                } if agent.metadata().description().is_none()
            )
    ));

    runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status: AgentStatus::Enabled,
                status: AgentUsableStatus::Disabled,
            }),
        ))
        .await
        .expect("Agent Disable dispatches");
    assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

    let disabled_definition = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision: "ar_2".parse().unwrap(),
                patch: AgentDefinitionPatch::new(Some(
                    AgentPromptSelection::new(vec!["disabled".parse().unwrap()]).unwrap(),
                )),
            }),
        ))
        .await
        .expect("disabled Agent definition update dispatches");
    assert!(matches!(
        disabled_definition.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentDefinitionUpdated {
                definition_revision
            },
            output: None,
        } if definition_revision.get() == 3
    ));
    assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

    let disabled_metadata = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision: "amr_3".parse().unwrap(),
                patch: AgentMetadataPatch::new(Some("Disabled Planner"), OptionalTextPatch::keep())
                    .unwrap(),
            }),
        ))
        .await
        .expect("disabled Agent metadata update dispatches");
    assert!(matches!(
        disabled_metadata.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 4
    ));
    assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

    runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id,
                expected_status: AgentStatus::Disabled,
            }),
        ))
        .await
        .expect("Agent Delete dispatches");
    assert!(matches!(events.recv().await, Some(EventFrame::State(_))));

    let deleted_definition = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision: "ar_3".parse().unwrap(),
                patch: AgentDefinitionPatch::new(None),
            }),
        ))
        .await
        .expect("deleted Agent definition update dispatches");
    assert!(matches!(
        deleted_definition.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::AgentDeleted
    ));
    let deleted_metadata = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision: "amr_4".parse().unwrap(),
                patch: AgentMetadataPatch::new(None::<&str>, OptionalTextPatch::keep()).unwrap(),
            }),
        ))
        .await
        .expect("deleted Agent metadata update dispatches");
    assert!(matches!(
        deleted_metadata.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::AgentDeleted
    ));

    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime reopens");
    let recovered = reopened
        .query(RuntimeQuery::Agent(AgentQuery::ListAgents {
            page: PageRequest::new(None, NonZeroU32::new(10).unwrap()),
            include_deleted: true,
        }))
        .await
        .expect("the recovered Agent catalog query succeeds");
    let QueryResult::Agent(AgentQueryResult::Agents(recovered)) = recovered.data() else {
        panic!("the recovered Agent query returns an Agent page");
    };
    assert_eq!(recovered.items().len(), 1);
    let agent = &recovered.items()[0];
    assert_eq!(agent.agent_id(), agent_id);
    assert_eq!(agent.definition_revision().get(), 3);
    assert_eq!(agent.metadata().revision().get(), 4);
    assert_eq!(agent.metadata().name(), "Disabled Planner");
    assert!(agent.metadata().description().is_none());
    assert_eq!(agent.status(), AgentStatus::Deleted);
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
async fn runtime_subscription_publishes_session_forked_after_its_snapshot() {
    let root = TempRoot::new();
    create_published_g1_agent_store(root.path());
    create_published_g1_session(
        root.path(),
        include_bytes!("../docs/fixtures/durable-store-v1/fork-source.jsonl"),
    );

    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Fork source opens");
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    let command_id = CommandId::generate().unwrap();
    let response = runtime
        .dispatch(CommandRequest::new(
            command_id,
            RuntimeCommand::Session(SessionCommand::Fork {
                source_session_id: "ses_22222222222222222222222222222222".parse().unwrap(),
                anchor: minicore_runtime::agent_session_lifecycle::ForkAnchor::Genesis,
            }),
        ))
        .await
        .expect("the Fork command dispatches");
    let CommandCompletion::Completed {
        outcome: CommandOutcome::SessionForked { session_id, .. },
        output: None,
    } = response.completion()
    else {
        panic!("the Fork command returns its typed outcome");
    };

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .expect("the Runtime event publisher responds")
        .expect("the Runtime subscription remains open");
    let EventFrame::State(event) = event else {
        panic!("the second Runtime frame is a StateEvent");
    };
    assert_eq!(event.command_id(), Some(command_id));
    assert_eq!(
        event.route(),
        EventRoute::Session {
            session_id: *session_id
        }
    );
    let StateEventMsg::Runtime {
        kind,
        detail: Some(RuntimeEventDetail::SessionChanged { session }),
        ..
    } = event.msg()
    else {
        panic!("SessionForked carries one safe Session summary");
    };
    assert_eq!(*kind, RuntimeStateEventKind::SessionForked);
    assert_eq!(session.session_id(), *session_id);
    assert!(session.forked());

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

struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new() -> Self {
        loop {
            let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
            assert_ne!(suffix, 0, "test workspace suffix must be nonzero");
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-foundation-workspace-{}-{suffix}",
                std::process::id()
            ));
            if !path.exists() {
                fs::create_dir_all(path.join("src"))
                    .expect("the temporary Workspace root is created");
                return Self { path };
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        if self.path.is_dir() {
            fs::remove_dir_all(&self.path)
                .expect("the temporary Workspace root is removed deterministically");
        } else if self.path.exists() {
            fs::remove_file(&self.path)
                .expect("the temporary Workspace file is removed deterministically");
        }
    }
}

fn workspace_uri(path: &Path) -> CanonicalFileUri {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().replace('\\', "/");
        let path = path.strip_prefix('/').unwrap_or(&path);
        return format!("file:///{path}")
            .parse()
            .expect("temporary Windows URI");
    }
    #[cfg(not(windows))]
    {
        format!("file://{}", path.to_str().expect("temporary path is UTF-8"))
            .parse()
            .expect("temporary POSIX URI")
    }
}

fn workspace_input(path: &Path) -> WorkspaceDefinitionInput {
    let key: WorkspaceRootKey = "repo".parse().unwrap();
    WorkspaceDefinitionInput::new(
        WorkspaceRootInput::new(
            key.clone(),
            workspace_uri(path),
            RequestedFilesystemAccess::ReadWrite,
            WorkspaceSourcePolicy::new(false, false),
        ),
        Vec::new(),
        WorkspaceCwdSpec::new(key, "src".parse().unwrap()),
    )
    .unwrap()
}

fn session_model_config() -> minicore_runtime::agent_session_lifecycle::SessionModelConfig {
    minicore_runtime::agent_session_lifecycle::SessionModelConfig::new(
        ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
        ReasoningPreference::Auto,
        Some(NonZeroU32::new(4096).unwrap()),
    )
}

fn command_output(response: &minicore_runtime::runtime_interface::CommandResponse) -> &str {
    match response.completion() {
        CommandCompletion::Completed {
            outcome: CommandOutcome::CommandOutput,
            output: Some(output),
        } => output.text(),
        completion => panic!("expected command output, got {completion:?}"),
    }
}

async fn create_public_agent(runtime: &MiniCoreRuntime) -> minicore_runtime::wire::AgentId {
    let created = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::Create {
                definition: NewAgentDefinition::new(AgentPromptSelection::new(Vec::new()).unwrap()),
                metadata: NewAgentMetadata::new("Test Agent", None::<&str>).unwrap(),
            }),
        ))
        .await
        .expect("Agent Create dispatches");
    let CommandCompletion::Completed {
        outcome: CommandOutcome::AgentCreated { agent_id, .. },
        output: None,
    } = created.completion()
    else {
        panic!("Agent Create returns its typed outcome");
    };
    *agent_id
}

async fn create_public_session(runtime: &MiniCoreRuntime, workspace_root: &Path) -> SessionId {
    let agent_id = create_public_agent(runtime).await;
    let created = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Create {
                agent_id,
                definition: Box::new(NewSessionDefinition::new(
                    workspace_input(workspace_root),
                    session_model_config(),
                    SessionPromptSelection::new(Vec::new()).unwrap(),
                )),
                metadata: NewSessionMetadata::new(None::<&str>, None::<&str>).unwrap(),
            }),
        ))
        .await
        .expect("public Session Create dispatches");
    command_output(&created).parse().unwrap()
}

async fn load_public_session(runtime: &MiniCoreRuntime, session_id: SessionId) {
    let load = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Load { session_id }),
        ))
        .await
        .expect("public Load dispatches");
    assert_eq!(command_output(&load), "session loaded");
}

fn session_metadata_update_command(
    session_id: SessionId,
    expected_revision: SessionMetadataRevision,
    name: OptionalTextPatch,
    description: OptionalTextPatch,
) -> RuntimeCommand {
    RuntimeCommand::Session(SessionCommand::UpdateMetadata {
        session_id,
        expected_revision,
        patch: SessionMetadataPatch::new(name, description).unwrap(),
    })
}

fn listed_session(
    page: &minicore_runtime::runtime_interface::Page<
        minicore_runtime::runtime_interface::SessionSummary,
    >,
    session_id: SessionId,
) -> &minicore_runtime::runtime_interface::SessionSummary {
    page.items()
        .iter()
        .find(|session| session.session_id() == session_id)
        .unwrap_or_else(|| panic!("the Session catalog page contains {session_id}"))
}

async fn list_sessions(
    runtime: &MiniCoreRuntime,
    cursor: Option<minicore_runtime::wire::PageCursor>,
    limit: u32,
) -> minicore_runtime::runtime_interface::Page<minicore_runtime::runtime_interface::SessionSummary>
{
    let response = runtime
        .query(RuntimeQuery::Session(SessionQuery::ListSessions {
            page: PageRequest::new(cursor, NonZeroU32::new(limit).unwrap()),
            include_archived: false,
        }))
        .await
        .expect("the Session catalog query succeeds");
    let QueryResult::Session(SessionQueryResult::Sessions(page)) = response.data() else {
        panic!("the Session query returns a Session page");
    };
    page.clone()
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_metadata_cas_on_unloaded_open_sessions_publishes_and_refreshes_catalog() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    let first = create_public_session(&runtime, workspace.path()).await;
    // Create publishes Runtime-scope events on the live subscription; drain until the
    // SessionCreated frame (and its AgentCreated frame) has passed.
    loop {
        match events.recv().await {
            Some(EventFrame::State(event))
                if event.msg().runtime_kind() == Some(RuntimeStateEventKind::SessionCreated) =>
            {
                break;
            }
            Some(EventFrame::State(_)) => {}
            other => panic!("unexpected frame while draining creates: {other:?}"),
        }
    }

    let missing = "ses_ffffffffffffffffffffffffffffffff".parse().unwrap();
    let rejected = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                missing,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the missing Session metadata update dispatches");
    assert!(matches!(
        rejected.completion(),
        CommandCompletion::Rejected(error)
            if error.code() == CommandErrorCode::NotFound
                && error.subject() == Some(&PublicSubject::Session(missing))
    ));

    let first_list = list_sessions(&runtime, None, 1).await;
    assert_eq!(first_list.items().len(), 1);
    assert!(
        first_list
            .items()
            .iter()
            .all(|session| session.metadata().revision().get() == 1
                && session.metadata().name().is_none())
    );

    let update_id = CommandId::generate().unwrap();
    let updated = runtime
        .dispatch(CommandRequest::new(
            update_id,
            session_metadata_update_command(
                first,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::set("Renamed Session").unwrap(),
                OptionalTextPatch::set("A renamed Session").unwrap(),
            ),
        ))
        .await
        .expect("the Session metadata update dispatches");
    assert!(matches!(
        updated.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 2
    ));
    let Some(EventFrame::State(event)) = events.recv().await else {
        panic!("the Session metadata update publishes one Runtime StateEvent");
    };
    assert_eq!(event.command_id(), Some(update_id));
    assert!(matches!(
        event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionMetadataUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } if session.session_id() == first
            && session.metadata().revision().get() == 2
            && session.metadata().name() == Some("Renamed Session")
            && session.metadata().description() == Some("A renamed Session")
            && session.lifecycle() == SessionLifecycleView::Open
    ));

    // A fresh ListSessions token reflects the changed metadata.
    let fresh = list_sessions(&runtime, None, 10).await;
    let refreshed = listed_session(&fresh, first);
    assert_eq!(refreshed.metadata().revision().get(), 2);
    assert_eq!(refreshed.metadata().name(), Some("Renamed Session"));

    // Stale wins over an equivalent patch, then the equivalent patch is a no-op without events.
    let stale = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                first,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the stale Session metadata update dispatches");
    assert!(matches!(
        stale.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::StaleRevision
    ));
    let noop = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                first,
                "smr_2".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the equivalent Session metadata update dispatches");
    assert!(matches!(
        noop.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "a Session metadata no-op publishes no Runtime event"
    );

    let cleared = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                first,
                "smr_2".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::clear(),
            ),
        ))
        .await
        .expect("the Session description clear dispatches");
    assert!(matches!(
        cleared.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 3
    ));
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::State(event))
            if matches!(
                event.msg(),
                StateEventMsg::Runtime {
                    kind: RuntimeStateEventKind::SessionMetadataUpdated,
                    detail: Some(RuntimeEventDetail::SessionChanged { session }),
                    ..
                } if session.session_id() == first && session.metadata().description().is_none()
            )
    ));

    // The pre-update cursor continuation is covered by the standalone catalog cursor snapshot
    // test; a fresh ListSessions token already proved the changed metadata above.

    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime reopens with the changed Session metadata");
    let recovered = list_sessions(&reopened, None, 10).await;
    let recovered = listed_session(&recovered, first);
    assert_eq!(recovered.metadata().revision().get(), 3);
    assert_eq!(recovered.metadata().name(), Some("Renamed Session"));
    assert_eq!(recovered.metadata().description(), None);
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_metadata_cas_allows_archived_rejects_deleted_and_preserves_conversation_bytes()
 {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let archived_session = create_public_session(&runtime, workspace.path()).await;
    let deleted_session = create_public_session(&runtime, workspace.path()).await;
    let loaded_session = create_public_session(&runtime, workspace.path()).await;
    let conversation_path = root
        .path()
        .join("sessions")
        .join(loaded_session.to_string())
        .join("conversation.jsonl");
    let before = fs::read(&conversation_path).expect("the conversation is readable after Create");

    let archived = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Archive {
                session_id: archived_session,
            }),
        ))
        .await
        .expect("Archive dispatches");
    assert!(matches!(
        archived.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionArchived,
            output: None,
        }
    ));
    let archived_update = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                archived_session,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::set("Archived v2").unwrap(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the Archived Session metadata update dispatches");
    assert!(matches!(
        archived_update.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 2
    ));

    // A Session cannot transition Open -> Deleted; the closed matrix requires Archive first.
    let archived_deleted = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Archive {
                session_id: deleted_session,
            }),
        ))
        .await
        .expect("Archive dispatches");
    assert!(matches!(
        archived_deleted.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionArchived,
            output: None,
        }
    ));
    let deleted = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Delete {
                session_id: deleted_session,
            }),
        ))
        .await
        .expect("Delete dispatches");
    assert!(matches!(
        deleted.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDeleted,
            output: None,
        }
    ));
    let deleted_update = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                deleted_session,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the Deleted Session metadata update dispatches");
    assert!(matches!(
        deleted_update.completion(),
        CommandCompletion::Rejected(error)
            if error.code() == CommandErrorCode::SessionDeleted
                && error.retry()
                    == minicore_runtime::runtime_interface::RetryAdvice::DoNotRetry
    ));
    let deleted_after_archive = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Delete {
                session_id: archived_session,
            }),
        ))
        .await
        .expect("the Archived Session Delete dispatches");
    assert!(matches!(
        deleted_after_archive.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDeleted,
            output: None,
        }
    ));
    let deleted_archived_update = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                archived_session,
                "smr_2".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the deleted Archived Session metadata update dispatches");
    assert!(matches!(
        deleted_archived_update.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::SessionDeleted
    ));

    // A real loaded path: Load, metadata update, Unload, then reopen.  The physical conversation
    // JSONL bytes must be untouched by the metadata CAS.
    load_public_session(&runtime, loaded_session).await;
    let loaded_update = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                loaded_session,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::set("Loaded v3").unwrap(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the loaded Session metadata update dispatches");
    assert!(matches!(
        loaded_update.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 2
    ));
    runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Unload {
                session_id: loaded_session,
            }),
        ))
        .await
        .expect("Unload dispatches");

    runtime.shutdown().await;
    assert_eq!(
        fs::read(&conversation_path).expect("the conversation remains readable"),
        before,
        "Session metadata updates never touch conversation JSONL bytes"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_metadata_cas_while_loaded_publishes_exact_runtime_and_session_events() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let session_id = create_public_session(&runtime, workspace.path()).await;
    load_public_session(&runtime, session_id).await;

    let mut runtime_events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        runtime_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));
    let mut session_events = runtime
        .subscribe(SubscriptionRequest::new(
            SubscriptionScope::Session { session_id },
            false,
        ))
        .await
        .expect("the Session subscription opens");
    assert!(matches!(
        session_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
    ));

    let first_id = CommandId::generate().unwrap();
    let first_update = runtime
        .dispatch(CommandRequest::new(
            first_id,
            session_metadata_update_command(
                session_id,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::set("Loaded v2").unwrap(),
                OptionalTextPatch::set("").unwrap(),
            ),
        ))
        .await
        .expect("the loaded Session metadata update dispatches");
    assert!(matches!(
        first_update.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 2
    ));

    let Some(EventFrame::State(runtime_event)) = runtime_events.recv().await else {
        panic!("the loaded update publishes one Runtime StateEvent");
    };
    assert_eq!(runtime_event.command_id(), Some(first_id));
    assert!(matches!(
        runtime_event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionMetadataUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } if session.session_id() == session_id
            && session.metadata().revision().get() == 2
            && session.metadata().name() == Some("Loaded v2")
            && session.metadata().description() == Some("")
    ));

    let Some(EventFrame::State(session_event)) = session_events.recv().await else {
        panic!("the loaded update publishes one Session StateEvent");
    };
    assert_eq!(session_event.command_id(), Some(first_id));
    assert_eq!(session_event.timestamp(), runtime_event.timestamp());
    assert_eq!(session_event.route(), EventRoute::Session { session_id });
    assert_eq!(
        session_event.msg().session_kind(),
        Some(SessionStateEventKind::SessionMetadataUpdated)
    );
    assert!(session_event.msg().session_detail().is_none());
    let snapshot = session_event.msg().session_snapshot().unwrap();
    assert_eq!(snapshot.metadata().revision().get(), 2);
    assert_eq!(snapshot.metadata().name(), Some("Loaded v2"));
    assert_eq!(snapshot.metadata().description(), Some(""));
    assert_eq!(
        snapshot.execution(),
        minicore_runtime::runtime_interface::SessionExecutionView::Idle
    );

    // A second consecutive update carries its own exact revision on both streams.
    let second_id = CommandId::generate().unwrap();
    let second_update = runtime
        .dispatch(CommandRequest::new(
            second_id,
            session_metadata_update_command(
                session_id,
                "smr_2".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::clear(),
            ),
        ))
        .await
        .expect("the second loaded Session metadata update dispatches");
    assert!(matches!(
        second_update.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
            output: None,
        } if metadata_revision.get() == 3
    ));
    assert!(matches!(
        runtime_events.recv().await,
        Some(EventFrame::State(event))
            if matches!(
                event.msg(),
                StateEventMsg::Runtime {
                    kind: RuntimeStateEventKind::SessionMetadataUpdated,
                    detail: Some(RuntimeEventDetail::SessionChanged { session }),
                    ..
                } if session.session_id() == session_id
                    && session.metadata().revision().get() == 3
                    && session.metadata().description().is_none()
            )
    ));
    assert!(matches!(
        session_events.recv().await,
        Some(EventFrame::State(event))
            if event.msg().session_kind() == Some(SessionStateEventKind::SessionMetadataUpdated)
                && event.msg().session_snapshot().unwrap().metadata().revision().get() == 3
                && event.msg().session_snapshot().unwrap().metadata().name() == Some("Loaded v2")
    ));

    // A loaded no-op publishes nothing on either stream.
    let noop = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                session_id,
                "smr_3".parse().unwrap(),
                OptionalTextPatch::keep(),
                OptionalTextPatch::keep(),
            ),
        ))
        .await
        .expect("the loaded equivalent Session metadata update dispatches");
    assert!(matches!(
        noop.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), runtime_events.recv())
            .await
            .is_err(),
        "a loaded Session metadata no-op publishes no Runtime event"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), session_events.recv())
            .await
            .is_err(),
        "a loaded Session metadata no-op publishes no Session event"
    );

    // The snapshot request observes the installed executor metadata.
    let observed = runtime
        .snapshot(SnapshotRequest::Session { session_id })
        .await
        .expect("the loaded Session snapshot is available");
    let SnapshotResponse::Session(observed) = observed else {
        panic!("the Session snapshot request returns a Session snapshot");
    };
    assert_eq!(observed.metadata().revision().get(), 3);
    assert_eq!(observed.metadata().name(), Some("Loaded v2"));
    assert_eq!(observed.metadata().description(), None);

    runtime.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_session_metadata_cas_with_one_expected_revision_wins_exactly_once() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let session_id = create_public_session(&runtime, workspace.path()).await;

    let (first, second) = tokio::join!(
        runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                session_id,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::set("Concurrent A").unwrap(),
                OptionalTextPatch::keep(),
            ),
        )),
        runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_metadata_update_command(
                session_id,
                "smr_1".parse().unwrap(),
                OptionalTextPatch::set("Concurrent B").unwrap(),
                OptionalTextPatch::keep(),
            ),
        )),
    );
    let first = first.expect("the first concurrent update dispatches");
    let second = second.expect("the second concurrent update dispatches");

    let mut updated = 0;
    let mut stale = 0;
    for response in [first, second] {
        match response.completion() {
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
                output: None,
            } => {
                assert_eq!(metadata_revision.get(), 2);
                updated += 1;
            }
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::StaleRevision =>
            {
                stale += 1;
            }
            completion => panic!("unexpected concurrent completion: {completion:?}"),
        }
    }
    assert_eq!(
        (updated, stale),
        (1, 1),
        "one concurrent CAS wins and the other is stale"
    );

    let page = list_sessions(&runtime, None, 10).await;
    let winner = listed_session(&page, session_id);
    assert_eq!(winner.metadata().revision().get(), 2);
    assert!(matches!(
        winner.metadata().name(),
        Some("Concurrent A") | Some("Concurrent B")
    ));

    runtime.shutdown().await;
}

fn changed_session_model_config() -> SessionModelConfig {
    SessionModelConfig::new(
        ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
        ReasoningPreference::High,
        Some(NonZeroU32::new(2048).unwrap()),
    )
}

fn session_definition_update_command(
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    workspace: Option<WorkspaceDefinitionInput>,
    model: Option<SessionModelConfig>,
    prompts: Option<SessionPromptSelection>,
) -> RuntimeCommand {
    RuntimeCommand::Session(SessionCommand::UpdateDefinition {
        session_id,
        expected_revision,
        patch: SessionDefinitionPatch::new(workspace, model, prompts),
    })
}

fn session_agent_upgrade_command(
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    target: Option<AgentRevisionRef>,
) -> RuntimeCommand {
    RuntimeCommand::Session(SessionCommand::UpgradeAgentRevision {
        session_id,
        expected_revision,
        target,
    })
}

async fn create_public_session_for_agent(
    runtime: &MiniCoreRuntime,
    agent_id: minicore_runtime::wire::AgentId,
    workspace_root: &Path,
) -> SessionId {
    let created = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Create {
                agent_id,
                definition: Box::new(NewSessionDefinition::new(
                    workspace_input(workspace_root),
                    session_model_config(),
                    SessionPromptSelection::new(Vec::new()).unwrap(),
                )),
                metadata: NewSessionMetadata::new(None::<&str>, None::<&str>).unwrap(),
            }),
        ))
        .await
        .expect("public Session Create dispatches");
    command_output(&created).parse().unwrap()
}

async fn update_public_agent_definition_to_second_revision(
    runtime: &MiniCoreRuntime,
    agent_id: minicore_runtime::wire::AgentId,
) {
    let definition = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision: "ar_1".parse().unwrap(),
                patch: AgentDefinitionPatch::new(Some(
                    AgentPromptSelection::new(vec!["base".parse().unwrap()]).unwrap(),
                )),
            }),
        ))
        .await
        .expect("Agent definition update dispatches");
    assert!(matches!(
        definition.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentDefinitionUpdated {
                definition_revision
            },
            output: None,
        } if definition_revision.get() == 2
    ));
}

fn workspace_input_with_cwd(path: &Path, relative_path: &str) -> WorkspaceDefinitionInput {
    let key: WorkspaceRootKey = "repo".parse().unwrap();
    WorkspaceDefinitionInput::new(
        WorkspaceRootInput::new(
            key.clone(),
            workspace_uri(path),
            RequestedFilesystemAccess::ReadWrite,
            WorkspaceSourcePolicy::new(false, false),
        ),
        Vec::new(),
        WorkspaceCwdSpec::new(key, relative_path.parse().unwrap()),
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_definition_cas_on_unloaded_open_sessions_publishes_and_recovers() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    let session_id = create_public_session(&runtime, workspace.path()).await;
    loop {
        match events.recv().await {
            Some(EventFrame::State(event))
                if event.msg().runtime_kind() == Some(RuntimeStateEventKind::SessionCreated) =>
            {
                break;
            }
            Some(EventFrame::State(_)) => {}
            other => panic!("unexpected frame while draining creates: {other:?}"),
        }
    }

    let missing = "ses_ffffffffffffffffffffffffffffffff".parse().unwrap();
    let rejected = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                missing,
                "sdr_1".parse().unwrap(),
                None,
                Some(changed_session_model_config()),
                None,
            ),
        ))
        .await
        .expect("the missing Session definition update dispatches");
    assert!(matches!(
        rejected.completion(),
        CommandCompletion::Rejected(error)
            if error.code() == CommandErrorCode::NotFound
                && error.subject() == Some(&PublicSubject::Session(missing))
    ));

    let first_id = CommandId::generate().unwrap();
    let updated = runtime
        .dispatch(CommandRequest::new(
            first_id,
            session_definition_update_command(
                session_id,
                "sdr_1".parse().unwrap(),
                None,
                Some(changed_session_model_config()),
                None,
            ),
        ))
        .await
        .expect("the unloaded Session definition update dispatches");
    assert!(matches!(
        updated.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDefinitionUpdated { definition_revision },
            output: None,
        } if definition_revision.get() == 2
    ));
    let Some(EventFrame::State(event)) = events.recv().await else {
        panic!("the definition update publishes one Runtime StateEvent");
    };
    assert_eq!(event.command_id(), Some(first_id));
    assert!(matches!(
        event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionDefinitionUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } if session.session_id() == session_id
            && session.definition_revision().get() == 2
            && session.lifecycle() == SessionLifecycleView::Open
    ));

    // Stale beats an otherwise-empty no-op patch.
    let stale = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                session_id,
                "sdr_1".parse().unwrap(),
                None,
                None,
                None,
            ),
        ))
        .await
        .expect("the stale Session definition update dispatches");
    assert!(matches!(
        stale.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::StaleRevision
    ));
    // Empty and canonical-equivalent patches are NoChange without events.
    let empty = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                session_id,
                "sdr_2".parse().unwrap(),
                None,
                None,
                None,
            ),
        ))
        .await
        .expect("the empty Session definition patch dispatches");
    assert!(matches!(
        empty.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    let equivalent = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                session_id,
                "sdr_2".parse().unwrap(),
                Some(workspace_input(workspace.path())),
                Some(changed_session_model_config()),
                Some(SessionPromptSelection::new(Vec::new()).unwrap()),
            ),
        ))
        .await
        .expect("the canonical-equivalent Session definition patch dispatches");
    assert!(matches!(
        equivalent.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "a Session definition no-op publishes no Runtime event"
    );

    // Only Open Sessions accept an ordinary definition update.
    let archived_session = create_public_session(&runtime, workspace.path()).await;
    let archived = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Archive {
                session_id: archived_session,
            }),
        ))
        .await
        .expect("Archive dispatches");
    assert!(matches!(
        archived.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionArchived,
            output: None,
        }
    ));
    let archived_update = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                archived_session,
                "sdr_1".parse().unwrap(),
                None,
                Some(changed_session_model_config()),
                None,
            ),
        ))
        .await
        .expect("the Archived Session definition update dispatches");
    assert!(matches!(
        archived_update.completion(),
        CommandCompletion::Rejected(error)
            if error.code() == CommandErrorCode::SessionArchived
                && error.retry()
                    == minicore_runtime::runtime_interface::RetryAdvice::UserActionRequired
    ));
    let deleted_session = create_public_session(&runtime, workspace.path()).await;
    let archive_first = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Archive {
                session_id: deleted_session,
            }),
        ))
        .await
        .expect("Archive dispatches");
    assert!(matches!(
        archive_first.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionArchived,
            output: None,
        }
    ));
    let deleted = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Delete {
                session_id: deleted_session,
            }),
        ))
        .await
        .expect("Delete dispatches");
    assert!(matches!(
        deleted.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDeleted,
            output: None,
        }
    ));
    let deleted_update = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                deleted_session,
                "sdr_1".parse().unwrap(),
                None,
                Some(changed_session_model_config()),
                None,
            ),
        ))
        .await
        .expect("the Deleted Session definition update dispatches");
    assert!(matches!(
        deleted_update.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::SessionDeleted
    ));

    runtime.shutdown().await;

    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime reopens");
    let page = list_sessions(&reopened, None, 10).await;
    let recovered = listed_session(&page, session_id);
    assert_eq!(recovered.definition_revision().get(), 2);
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_definition_cas_while_loaded_publishes_exact_runtime_and_session_events() {
    let root = TempRoot::new();
    let first_workspace = TempWorkspace::new();
    let second_workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let session_id = create_public_session(&runtime, first_workspace.path()).await;
    load_public_session(&runtime, session_id).await;

    let mut runtime_events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        runtime_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));
    let mut session_events = runtime
        .subscribe(SubscriptionRequest::new(
            SubscriptionScope::Session { session_id },
            false,
        ))
        .await
        .expect("the Session subscription opens");
    assert!(matches!(
        session_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
    ));

    // A loaded Idle Workspace change increments both revisions and publishes exact events.
    let second_workspace_input = workspace_input_with_cwd(second_workspace.path(), "");
    let first_id = CommandId::generate().unwrap();
    let workspace_update = runtime
        .dispatch(CommandRequest::new(
            first_id,
            session_definition_update_command(
                session_id,
                "sdr_1".parse().unwrap(),
                Some(second_workspace_input.clone()),
                None,
                None,
            ),
        ))
        .await
        .expect("the loaded Workspace definition update dispatches");
    assert!(matches!(
        workspace_update.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDefinitionUpdated { definition_revision },
            output: None,
        } if definition_revision.get() == 2
    ));

    let Some(EventFrame::State(runtime_event)) = runtime_events.recv().await else {
        panic!("the loaded update publishes one Runtime StateEvent");
    };
    assert_eq!(runtime_event.command_id(), Some(first_id));
    assert!(matches!(
        runtime_event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionDefinitionUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } if session.session_id() == session_id
            && session.definition_revision().get() == 2
    ));

    let Some(EventFrame::State(session_event)) = session_events.recv().await else {
        panic!("the loaded update publishes one Session StateEvent");
    };
    assert_eq!(session_event.command_id(), Some(first_id));
    assert_eq!(session_event.route(), EventRoute::Session { session_id });
    assert_eq!(
        session_event.msg().session_kind(),
        Some(SessionStateEventKind::SessionDefinitionUpdated)
    );
    assert!(session_event.msg().session_detail().is_none());
    let snapshot = session_event.msg().session_snapshot().unwrap();
    assert_eq!(snapshot.definition().revision().get(), 2);
    assert_eq!(
        snapshot.definition().created_at(),
        session_event.timestamp()
    );
    assert_eq!(
        snapshot
            .definition()
            .workspace()
            .cwd()
            .relative_path()
            .as_str(),
        ""
    );
    assert_eq!(snapshot.execution(), SessionExecutionView::Idle);

    // A second consecutive future-only update carries its own exact revision on both streams
    // while preserving the installed Workspace.
    let second_id = CommandId::generate().unwrap();
    let model_update = runtime
        .dispatch(CommandRequest::new(
            second_id,
            session_definition_update_command(
                session_id,
                "sdr_2".parse().unwrap(),
                None,
                Some(changed_session_model_config()),
                None,
            ),
        ))
        .await
        .expect("the second loaded Session definition update dispatches");
    assert!(matches!(
        model_update.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDefinitionUpdated { definition_revision },
            output: None,
        } if definition_revision.get() == 3
    ));
    assert!(matches!(
        runtime_events.recv().await,
        Some(EventFrame::State(event))
            if matches!(
                event.msg(),
                StateEventMsg::Runtime {
                    kind: RuntimeStateEventKind::SessionDefinitionUpdated,
                    detail: Some(RuntimeEventDetail::SessionChanged { session }),
                    ..
                } if session.session_id() == session_id
                    && session.definition_revision().get() == 3
            )
    ));
    assert!(matches!(
        session_events.recv().await,
        Some(EventFrame::State(event))
            if event.msg().session_kind()
                == Some(SessionStateEventKind::SessionDefinitionUpdated)
                && event.msg().session_snapshot().unwrap().definition().revision().get() == 3
                && event
                    .msg()
                    .session_snapshot()
                    .unwrap()
                    .definition()
                    .model()
                    .reasoning()
                    == ReasoningPreference::High
                && event
                    .msg()
                    .session_snapshot()
                    .unwrap()
                    .definition()
                    .workspace()
                    .cwd()
                    .relative_path()
                    .as_str()
                    == ""
    ));

    // A loaded no-op publishes nothing on either stream.
    let noop = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                session_id,
                "sdr_3".parse().unwrap(),
                Some(second_workspace_input),
                Some(changed_session_model_config()),
                Some(SessionPromptSelection::new(Vec::new()).unwrap()),
            ),
        ))
        .await
        .expect("the loaded canonical-equivalent Session definition update dispatches");
    assert!(matches!(
        noop.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), runtime_events.recv())
            .await
            .is_err(),
        "a loaded Session definition no-op publishes no Runtime event"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), session_events.recv())
            .await
            .is_err(),
        "a loaded Session definition no-op publishes no Session event"
    );

    // The snapshot request observes the installed executor definition.
    let observed = runtime
        .snapshot(SnapshotRequest::Session { session_id })
        .await
        .expect("the loaded Session snapshot is available");
    let SnapshotResponse::Session(observed) = observed else {
        panic!("the Session snapshot request returns a Session snapshot");
    };
    assert_eq!(observed.definition().revision().get(), 3);
    assert_eq!(
        observed.definition().model().reasoning(),
        ReasoningPreference::High
    );

    runtime.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_session_definition_cas_with_one_expected_revision_wins_exactly_once() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let session_id = create_public_session(&runtime, workspace.path()).await;

    let (first, second) = tokio::join!(
        runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                session_id,
                "sdr_1".parse().unwrap(),
                None,
                Some(changed_session_model_config()),
                None,
            ),
        )),
        runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_definition_update_command(
                session_id,
                "sdr_1".parse().unwrap(),
                None,
                None,
                Some(SessionPromptSelection::new(vec!["base".parse().unwrap()]).unwrap()),
            ),
        )),
    );
    let first = first.expect("the first concurrent update dispatches");
    let second = second.expect("the second concurrent update dispatches");

    let mut updated = 0;
    let mut stale = 0;
    for response in [first, second] {
        match response.completion() {
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::SessionDefinitionUpdated {
                        definition_revision,
                    },
                output: None,
            } => {
                assert_eq!(definition_revision.get(), 2);
                updated += 1;
            }
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::StaleRevision =>
            {
                stale += 1;
            }
            completion => panic!("unexpected concurrent completion: {completion:?}"),
        }
    }
    assert_eq!(
        (updated, stale),
        (1, 1),
        "one concurrent CAS wins and the other is stale"
    );

    let page = list_sessions(&runtime, None, 10).await;
    let winner = listed_session(&page, session_id);
    assert_eq!(winner.definition_revision().get(), 2);

    runtime.shutdown().await;
}

#[test]
fn session_agent_upgrade_command_debug_is_safe_and_stable() {
    let command = RuntimeCommand::Session(SessionCommand::UpgradeAgentRevision {
        session_id: "ses_22222222222222222222222222222222".parse().unwrap(),
        expected_revision: "sdr_1".parse().unwrap(),
        target: Some(AgentRevisionRef::new(
            "agt_11111111111111111111111111111111".parse().unwrap(),
            "ar_1".parse().unwrap(),
        )),
    });
    let debug = format!("{command:?}");
    assert!(debug.contains("UpgradeAgentRevision"), "{debug}");
    assert!(
        debug.contains("ses_22222222222222222222222222222222"),
        "{debug}"
    );
    assert!(!debug.contains("secret"), "{debug}");
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_agent_upgrade_pins_current_rolls_back_and_survives_restart() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    let agent_id = create_public_agent(&runtime).await;
    let session_id = create_public_session_for_agent(&runtime, agent_id, workspace.path()).await;
    loop {
        match events.recv().await {
            Some(EventFrame::State(event))
                if event.msg().runtime_kind() == Some(RuntimeStateEventKind::SessionCreated) =>
            {
                break;
            }
            Some(EventFrame::State(_)) => {}
            other => panic!("unexpected frame while draining creates: {other:?}"),
        }
    }
    let conversation_path = root
        .path()
        .join("sessions")
        .join(session_id.to_string())
        .join("conversation.jsonl");
    let conversation_before = fs::read(&conversation_path).expect("the conversation is readable");

    update_public_agent_definition_to_second_revision(&runtime, agent_id).await;
    loop {
        match events.recv().await {
            Some(EventFrame::State(event))
                if event.msg().runtime_kind()
                    == Some(RuntimeStateEventKind::AgentDefinitionUpdated) =>
            {
                break;
            }
            Some(EventFrame::State(_)) => {}
            other => panic!("unexpected frame while draining the Agent update: {other:?}"),
        }
    }

    // None pins the Agent current revision (ar_2) at a checked successor definition revision.
    let upgrade_id = CommandId::generate().unwrap();
    let upgraded = runtime
        .dispatch(CommandRequest::new(
            upgrade_id,
            session_agent_upgrade_command(session_id, "sdr_1".parse().unwrap(), None),
        ))
        .await
        .expect("the Session Agent upgrade dispatches");
    assert!(matches!(
        upgraded.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDefinitionUpdated { definition_revision },
            output: None,
        } if definition_revision.get() == 2
    ));
    let Some(EventFrame::State(event)) = events.recv().await else {
        panic!("the Agent upgrade publishes one Runtime StateEvent");
    };
    assert_eq!(event.command_id(), Some(upgrade_id));
    assert!(matches!(
        event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionDefinitionUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } if session.session_id() == session_id && session.definition_revision().get() == 2
    ));

    // The pin is observable in the loaded Session snapshot.
    load_public_session(&runtime, session_id).await;
    let observed = runtime
        .snapshot(SnapshotRequest::Session { session_id })
        .await
        .expect("the loaded Session snapshot is available");
    let SnapshotResponse::Session(observed) = observed else {
        panic!("the Session snapshot request returns a Session snapshot");
    };
    assert_eq!(observed.definition().revision().get(), 2);
    assert_eq!(observed.definition().agent().agent_id(), agent_id);
    assert_eq!(observed.definition().agent().revision().get(), 2);
    let unload = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Unload { session_id }),
        ))
        .await
        .expect("Unload dispatches");
    assert_eq!(command_output(&unload), "session unloaded");

    // A fresh subscription observes only the no-op, stale, and rollback below; the
    // earlier Load/Unload events stay on the previous stream.
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    // The same pin is a canonical no-op without events; stale wins before the no-op.
    let noop = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_agent_upgrade_command(session_id, "sdr_2".parse().unwrap(), None),
        ))
        .await
        .expect("the same-pin upgrade dispatches");
    assert!(matches!(
        noop.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "a same-pin upgrade publishes no Runtime event"
    );
    let stale = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_agent_upgrade_command(session_id, "sdr_1".parse().unwrap(), None),
        ))
        .await
        .expect("the stale upgrade dispatches");
    assert!(matches!(
        stale.completion(),
        CommandCompletion::Rejected(error) if error.code() == CommandErrorCode::StaleRevision
    ));

    // An explicit exact target rolls back to the retained ar_1.
    let rollback_id = CommandId::generate().unwrap();
    let rolled_back = runtime
        .dispatch(CommandRequest::new(
            rollback_id,
            session_agent_upgrade_command(
                session_id,
                "sdr_2".parse().unwrap(),
                Some(AgentRevisionRef::new(agent_id, "ar_1".parse().unwrap())),
            ),
        ))
        .await
        .expect("the explicit rollback dispatches");
    assert!(matches!(
        rolled_back.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDefinitionUpdated { definition_revision },
            output: None,
        } if definition_revision.get() == 3
    ));
    let Some(EventFrame::State(event)) = events.recv().await else {
        panic!("the rollback publishes one Runtime StateEvent");
    };
    assert_eq!(event.command_id(), Some(rollback_id));
    assert!(matches!(
        event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionDefinitionUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } if session.definition_revision().get() == 3
    ));
    assert_eq!(
        fs::read(&conversation_path).unwrap(),
        conversation_before,
        "Agent upgrades never write the conversation"
    );

    // Restart retains the exact pin (the rollback to ar_1 at sdr_3).
    runtime.shutdown().await;
    let reopened = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime reopens with the upgraded Session definition");
    let page = list_sessions(&reopened, None, 10).await;
    let recovered = listed_session(&page, session_id);
    assert_eq!(recovered.definition_revision().get(), 3);
    load_public_session(&reopened, session_id).await;
    let observed = reopened
        .snapshot(SnapshotRequest::Session { session_id })
        .await
        .expect("the recovered Session snapshot is available");
    let SnapshotResponse::Session(observed) = observed else {
        panic!("the recovered Session snapshot request returns a Session snapshot");
    };
    assert_eq!(observed.definition().revision().get(), 3);
    assert_eq!(observed.definition().agent().agent_id(), agent_id);
    assert_eq!(
        observed.definition().agent().revision().get(),
        1,
        "restart retains the exact pinned Agent revision"
    );
    reopened.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_agent_upgrade_maps_agent_and_session_errors_without_events() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let agent_id = create_public_agent(&runtime).await;
    let session_id = create_public_session_for_agent(&runtime, agent_id, workspace.path()).await;
    let other_agent = create_public_agent(&runtime).await;
    let archived_session =
        create_public_session_for_agent(&runtime, agent_id, workspace.path()).await;
    let deleted_session =
        create_public_session_for_agent(&runtime, agent_id, workspace.path()).await;
    let archived = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Archive {
                session_id: archived_session,
            }),
        ))
        .await
        .expect("Archive dispatches");
    assert!(matches!(
        archived.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionArchived,
            output: None,
        }
    ));
    let archived_for_delete = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Archive {
                session_id: deleted_session,
            }),
        ))
        .await
        .expect("Archive dispatches for the later Delete");
    assert!(matches!(
        archived_for_delete.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionArchived,
            output: None,
        }
    ));
    let deleted = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Delete {
                session_id: deleted_session,
            }),
        ))
        .await
        .expect("Delete dispatches");
    assert!(matches!(
        deleted.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDeleted,
            output: None,
        }
    ));
    let disabled_agent = create_public_agent(&runtime).await;
    let disabled_session =
        create_public_session_for_agent(&runtime, disabled_agent, workspace.path()).await;
    let deleted_agent = create_public_agent(&runtime).await;
    let deleted_agent_session =
        create_public_session_for_agent(&runtime, deleted_agent, workspace.path()).await;
    let disabled = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id: disabled_agent,
                expected_status: AgentStatus::Enabled,
                status: AgentUsableStatus::Disabled,
            }),
        ))
        .await
        .expect("Agent Disable dispatches");
    assert!(matches!(
        disabled.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentStatusChanged { .. },
            output: None,
        }
    ));
    let deleted_agent_disabled = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id: deleted_agent,
                expected_status: AgentStatus::Enabled,
                status: AgentUsableStatus::Disabled,
            }),
        ))
        .await
        .expect("Agent Disable dispatches for the later Delete");
    assert!(matches!(
        deleted_agent_disabled.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentStatusChanged { .. },
            output: None,
        }
    ));
    let deleted_agent_response = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id: deleted_agent,
                expected_status: AgentStatus::Disabled,
            }),
        ))
        .await
        .expect("Agent Delete dispatches");
    assert!(matches!(
        deleted_agent_response.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentDeleted,
            output: None,
        }
    ));

    // A fresh subscription observes only the failures below.
    let mut events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));

    let session_subject = Some(PublicSubject::Session(session_id));
    let mut rejected = Vec::new();
    for (command, expected_code, expected_retry, expected_subject) in [
        (
            session_agent_upgrade_command(
                session_id,
                "sdr_1".parse().unwrap(),
                Some(AgentRevisionRef::new(other_agent, "ar_1".parse().unwrap())),
            ),
            CommandErrorCode::InvalidArgument,
            RetryAdvice::DoNotRetry,
            session_subject.clone(),
        ),
        (
            session_agent_upgrade_command(
                session_id,
                "sdr_1".parse().unwrap(),
                Some(AgentRevisionRef::new(agent_id, "ar_99".parse().unwrap())),
            ),
            CommandErrorCode::NotFound,
            RetryAdvice::RefreshAndRetry,
            session_subject.clone(),
        ),
        (
            session_agent_upgrade_command(disabled_session, "sdr_1".parse().unwrap(), None),
            CommandErrorCode::AgentDisabled,
            RetryAdvice::UserActionRequired,
            Some(PublicSubject::Session(disabled_session)),
        ),
        (
            session_agent_upgrade_command(
                deleted_agent_session,
                "sdr_1".parse().unwrap(),
                Some(AgentRevisionRef::new(
                    deleted_agent,
                    "ar_1".parse().unwrap(),
                )),
            ),
            CommandErrorCode::AgentDeleted,
            RetryAdvice::DoNotRetry,
            Some(PublicSubject::Session(deleted_agent_session)),
        ),
        (
            session_agent_upgrade_command(archived_session, "sdr_1".parse().unwrap(), None),
            CommandErrorCode::SessionArchived,
            RetryAdvice::UserActionRequired,
            Some(PublicSubject::Session(archived_session)),
        ),
        (
            session_agent_upgrade_command(deleted_session, "sdr_1".parse().unwrap(), None),
            CommandErrorCode::SessionDeleted,
            RetryAdvice::DoNotRetry,
            Some(PublicSubject::Session(deleted_session)),
        ),
    ] {
        let response = runtime
            .dispatch(CommandRequest::new(CommandId::generate().unwrap(), command))
            .await
            .expect("the failing Agent upgrade dispatches");
        let CommandCompletion::Rejected(error) = response.completion() else {
            panic!("the failing upgrade is rejected: {response:?}");
        };
        assert_eq!(error.code(), expected_code);
        assert_eq!(error.retry(), expected_retry);
        assert_eq!(error.subject(), expected_subject.as_ref());
        rejected.push(response);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "a rejected Agent upgrade publishes no Runtime event"
        );
    }
    assert_eq!(rejected.len(), 6);

    // The archived/deleted Sessions and the pinned definition are untouched by failures.
    let page = list_sessions(&runtime, None, 10).await;
    let pinned = listed_session(&page, session_id);
    assert_eq!(pinned.definition_revision().get(), 1);
    runtime.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_session_agent_upgrade_while_loaded_publishes_exact_runtime_and_session_events() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let agent_id = create_public_agent(&runtime).await;
    let session_id = create_public_session_for_agent(&runtime, agent_id, workspace.path()).await;
    update_public_agent_definition_to_second_revision(&runtime, agent_id).await;
    load_public_session(&runtime, session_id).await;

    let mut runtime_events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        runtime_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));
    let mut session_events = runtime
        .subscribe(SubscriptionRequest::new(
            SubscriptionScope::Session { session_id },
            false,
        ))
        .await
        .expect("the Session subscription opens");
    assert!(matches!(
        session_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
    ));

    let upgrade_id = CommandId::generate().unwrap();
    let upgraded = runtime
        .dispatch(CommandRequest::new(
            upgrade_id,
            session_agent_upgrade_command(session_id, "sdr_1".parse().unwrap(), None),
        ))
        .await
        .expect("the loaded Session Agent upgrade dispatches");
    assert!(matches!(
        upgraded.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::SessionDefinitionUpdated { definition_revision },
            output: None,
        } if definition_revision.get() == 2
    ));

    let Some(EventFrame::State(runtime_event)) = runtime_events.recv().await else {
        panic!("the loaded upgrade publishes one Runtime StateEvent");
    };
    assert_eq!(runtime_event.command_id(), Some(upgrade_id));
    assert!(matches!(
        runtime_event.msg(),
        StateEventMsg::Runtime {
            kind: RuntimeStateEventKind::SessionDefinitionUpdated,
            detail: Some(RuntimeEventDetail::SessionChanged { session }),
            ..
        } if session.session_id() == session_id && session.definition_revision().get() == 2
    ));

    let Some(EventFrame::State(session_event)) = session_events.recv().await else {
        panic!("the loaded upgrade publishes one Session StateEvent");
    };
    assert_eq!(session_event.command_id(), Some(upgrade_id));
    assert_eq!(session_event.timestamp(), runtime_event.timestamp());
    assert_eq!(session_event.route(), EventRoute::Session { session_id });
    assert_eq!(
        session_event.msg().session_kind(),
        Some(SessionStateEventKind::SessionDefinitionUpdated)
    );
    assert!(session_event.msg().session_detail().is_none());
    let snapshot = session_event.msg().session_snapshot().unwrap();
    assert_eq!(snapshot.definition().revision().get(), 2);
    assert_eq!(snapshot.definition().agent().agent_id(), agent_id);
    assert_eq!(snapshot.definition().agent().revision().get(), 2);
    assert_eq!(
        snapshot.definition().created_at(),
        session_event.timestamp()
    );
    assert_eq!(snapshot.execution(), SessionExecutionView::Idle);
    assert_eq!(
        snapshot
            .definition()
            .workspace()
            .cwd()
            .relative_path()
            .as_str(),
        "src"
    );

    // The snapshot request observes the exact installed definition with the same Workspace.
    let observed = runtime
        .snapshot(SnapshotRequest::Session { session_id })
        .await
        .expect("the loaded Session snapshot is available");
    let SnapshotResponse::Session(observed) = observed else {
        panic!("the Session snapshot request returns a Session snapshot");
    };
    assert_eq!(observed.definition().revision().get(), 2);
    assert_eq!(observed.definition().agent().revision().get(), 2);
    assert_eq!(
        observed
            .definition()
            .workspace()
            .cwd()
            .relative_path()
            .as_str(),
        "src"
    );

    // A loaded same-pin no-op publishes nothing on either stream.
    let noop = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_agent_upgrade_command(session_id, "sdr_2".parse().unwrap(), None),
        ))
        .await
        .expect("the loaded same-pin upgrade dispatches");
    assert!(matches!(
        noop.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::NoChange,
            output: None,
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), runtime_events.recv())
            .await
            .is_err(),
        "a loaded same-pin upgrade publishes no Runtime event"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), session_events.recv())
            .await
            .is_err(),
        "a loaded same-pin upgrade publishes no Session event"
    );

    runtime.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_session_agent_upgrade_with_one_expected_revision_wins_exactly_once() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let agent_id = create_public_agent(&runtime).await;
    let session_id = create_public_session_for_agent(&runtime, agent_id, workspace.path()).await;
    update_public_agent_definition_to_second_revision(&runtime, agent_id).await;

    let (first, second) = tokio::join!(
        runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_agent_upgrade_command(session_id, "sdr_1".parse().unwrap(), None),
        )),
        runtime.dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            session_agent_upgrade_command(session_id, "sdr_1".parse().unwrap(), None),
        )),
    );
    let first = first.expect("the first concurrent upgrade dispatches");
    let second = second.expect("the second concurrent upgrade dispatches");

    let mut updated = 0;
    let mut stale = 0;
    for response in [first, second] {
        match response.completion() {
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::SessionDefinitionUpdated {
                        definition_revision,
                    },
                output: None,
            } => {
                assert_eq!(definition_revision.get(), 2);
                updated += 1;
            }
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::StaleRevision =>
            {
                stale += 1;
            }
            completion => panic!("unexpected concurrent completion: {completion:?}"),
        }
    }
    assert_eq!(
        (updated, stale),
        (1, 1),
        "one concurrent Agent upgrade wins and the other is stale"
    );

    let page = list_sessions(&runtime, None, 10).await;
    let winner = listed_session(&page, session_id);
    assert_eq!(winner.definition_revision().get(), 2);

    runtime.shutdown().await;
}

fn now_truncated_to_millis() -> time::OffsetDateTime {
    let now = time::OffsetDateTime::now_utc();
    let nanoseconds = now.nanosecond();
    now.replace_nanosecond(nanoseconds - (nanoseconds % 1_000_000))
        .expect("truncating a nanosecond component stays within its valid range")
}

#[tokio::test(flavor = "current_thread")]
async fn public_loaded_workspace_reload_returns_workspace_reloaded_and_publishes_session_event() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let session_id = create_public_session(&runtime, workspace.path()).await;
    load_public_session(&runtime, session_id).await;

    let mut runtime_events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        runtime_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));
    let mut session_events = runtime
        .subscribe(SubscriptionRequest::new(
            SubscriptionScope::Session { session_id },
            false,
        ))
        .await
        .expect("the Session subscription opens");
    assert!(matches!(
        session_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
    ));
    let conversation_path = root
        .path()
        .join("sessions")
        .join(session_id.to_string())
        .join("conversation.jsonl");
    let conversation_before = fs::read(&conversation_path).expect("the conversation is readable");

    let reload_id = CommandId::generate().unwrap();
    let before = now_truncated_to_millis();
    let reloaded = runtime
        .dispatch(CommandRequest::new(
            reload_id,
            RuntimeCommand::Session(SessionCommand::ReloadWorkspace { session_id }),
        ))
        .await
        .expect("the loaded Workspace reload dispatches");
    let after = now_truncated_to_millis();
    assert!(matches!(
        reloaded.completion(),
        CommandCompletion::Completed {
            outcome: CommandOutcome::WorkspaceReloaded,
            output: None,
        }
    ));

    // The Session-scope subscriber receives the exact WorkspaceReloaded event: the owner
    // command id, the single sampled timestamp, the matching Session route with a null detail,
    // and the exact post-reload loaded Ready Idle snapshot.
    let Some(EventFrame::State(session_event)) = session_events.recv().await else {
        panic!("the reload publishes one Session StateEvent");
    };
    assert_eq!(session_event.command_id(), Some(reload_id));
    assert_eq!(session_event.route(), EventRoute::Session { session_id });
    assert_eq!(
        session_event.msg().session_kind(),
        Some(SessionStateEventKind::SessionWorkspaceReloaded)
    );
    assert!(session_event.msg().session_detail().is_none());
    let timestamp = session_event.timestamp().as_datetime();
    assert!(timestamp >= before && timestamp <= after);
    let snapshot = session_event.msg().session_snapshot().unwrap();
    assert_eq!(snapshot.session_id(), session_id);
    assert_eq!(snapshot.definition().revision().get(), 1);
    assert_eq!(
        snapshot
            .definition()
            .workspace()
            .cwd()
            .relative_path()
            .as_str(),
        "src"
    );
    assert_eq!(snapshot.execution(), SessionExecutionView::Idle);
    assert_eq!(snapshot.metadata().revision().get(), 1);
    // A reload keeps the durable definition createdAt; it is not the reload timestamp.
    assert_ne!(
        snapshot.definition().created_at(),
        session_event.timestamp()
    );

    // The Runtime-scope subscriber receives no reload event, and no other Session event follows.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), runtime_events.recv())
            .await
            .is_err(),
        "a Workspace reload publishes no Runtime event"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), session_events.recv())
            .await
            .is_err(),
        "a Workspace reload publishes exactly one Session event"
    );

    // The reload changed no durable fact: definition revision, metadata, and conversation bytes
    // are all preserved.
    let observed = runtime
        .snapshot(SnapshotRequest::Session { session_id })
        .await
        .expect("the loaded Session snapshot is available");
    let SnapshotResponse::Session(observed) = observed else {
        panic!("the Session snapshot request returns a Session snapshot");
    };
    assert_eq!(observed.definition().revision().get(), 1);
    assert_eq!(observed.metadata().revision().get(), 1);
    assert_eq!(
        fs::read(&conversation_path).expect("the conversation is readable"),
        conversation_before,
        "a Workspace reload never writes the conversation"
    );

    runtime.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn public_workspace_reload_errors_preserve_state_and_map_typed_failures() {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()),
        Handle::current(),
    )
    .await
    .expect("the Runtime opens");
    let session_id = create_public_session(&runtime, workspace.path()).await;
    load_public_session(&runtime, session_id).await;

    let mut runtime_events = runtime
        .subscribe(SubscriptionRequest::new(SubscriptionScope::Runtime, false))
        .await
        .expect("the Runtime subscription opens");
    assert!(matches!(
        runtime_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Runtime(_)))
    ));
    let mut session_events = runtime
        .subscribe(SubscriptionRequest::new(
            SubscriptionScope::Session { session_id },
            false,
        ))
        .await
        .expect("the Session subscription opens");
    assert!(matches!(
        session_events.recv().await,
        Some(EventFrame::Snapshot(SnapshotResponse::Session(_)))
    ));

    // A root that is a plain file fails validation: ReloadValidationFailed with
    // UserActionRequired, no event on either stream, and the old snapshot preserved.
    fs::remove_dir_all(workspace.path()).expect("the Workspace root is removed");
    fs::write(workspace.path(), b"not a directory")
        .expect("the Workspace root becomes a plain file");
    let rejected = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::ReloadWorkspace { session_id }),
        ))
        .await
        .expect("the invalid Workspace reload dispatches");
    assert!(matches!(
        rejected.completion(),
        CommandCompletion::Rejected(error)
            if error.code() == CommandErrorCode::ReloadValidationFailed
                && error.retry()
                    == minicore_runtime::runtime_interface::RetryAdvice::UserActionRequired
                && error.subject() == Some(&PublicSubject::Session(session_id))
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), runtime_events.recv())
            .await
            .is_err(),
        "a failed reload publishes no Runtime event"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), session_events.recv())
            .await
            .is_err(),
        "a failed reload publishes no Session event"
    );
    let preserved = runtime
        .snapshot(SnapshotRequest::Session { session_id })
        .await
        .expect("the loaded Session snapshot is still available");
    let SnapshotResponse::Session(preserved) = preserved else {
        panic!("the Session snapshot request returns a Session snapshot");
    };
    assert_eq!(preserved.definition().revision().get(), 1);
    assert_eq!(preserved.execution(), SessionExecutionView::Idle);
    assert_eq!(
        preserved
            .definition()
            .workspace()
            .cwd()
            .relative_path()
            .as_str(),
        "src"
    );

    // Unloaded and missing Sessions map to SessionNotLoaded with UserActionRequired.
    let unload = runtime
        .dispatch(CommandRequest::new(
            CommandId::generate().unwrap(),
            RuntimeCommand::Session(SessionCommand::Unload { session_id }),
        ))
        .await
        .expect("Unload dispatches");
    assert_eq!(command_output(&unload), "session unloaded");
    for target in [
        session_id,
        "ses_ffffffffffffffffffffffffffffffff".parse().unwrap(),
    ] {
        let not_loaded = runtime
            .dispatch(CommandRequest::new(
                CommandId::generate().unwrap(),
                RuntimeCommand::Session(SessionCommand::ReloadWorkspace { session_id: target }),
            ))
            .await
            .expect("the unloaded Workspace reload dispatches");
        assert!(matches!(
            not_loaded.completion(),
            CommandCompletion::Rejected(error)
                if error.code() == CommandErrorCode::SessionNotLoaded
                    && error.retry()
                        == minicore_runtime::runtime_interface::RetryAdvice::UserActionRequired
                    && error.subject() == Some(&PublicSubject::Session(target))
        ));
    }

    runtime.shutdown().await;
}
