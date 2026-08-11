//! M14 live provider smoke tests: explicit opt-in, real-network smoke through the
//! public host/runtime path.
//!
//! Two `#[ignore]`d tests drive the full public path — config -> `open` ->
//! catalog resolution -> Agent/Session Create/Load -> Submit -> env-backed dynamic
//! `CredentialSource` -> direct provider adapter -> real protocol terminal —
//! against the real OpenAI Responses and Anthropic Messages APIs.
//!
//! These tests are excluded from `./scripts/check.sh`, `./scripts/check-msrv.sh`,
//! and the default `cargo test`: they are never executed unless explicitly run
//! with `--ignored`, and even then every test requires its provider's opt-in env
//! var to be exactly `1` plus the full documented nonsecret env set (see
//! `tests/README.md`). Failure messages name only the missing variable; they
//! never print a variable's value, the credential, the endpoint, the API model
//! env value, or any request/response body. Temp directories clean up via `Drop`.

use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use minicore_runtime::agent_session_lifecycle::{
    NewAgentDefinition, NewAgentMetadata, SessionModelConfig,
};
use minicore_runtime::model_gateway::{
    CredentialSource, CredentialSourceFuture, ModelProviderConfig, ModelProviderDescriptor,
    ModelSelection, ProviderCredential, ProviderEndpointPolicy, ReasoningPreference,
};
use minicore_runtime::prompt::{
    AgentPromptSelection, PromptBodyIntent, PromptIntent, SessionPromptSelection, TextIntent,
};
use minicore_runtime::runtime::EventStream;
use minicore_runtime::runtime_interface::{
    AgentCommand, CommandCompletion, CommandOutcome, CommandRequest, EventFrame,
    NewSessionDefinition, NewSessionMetadata, RuntimeCommand, SessionCommand, SessionEventDetail,
    SnapshotResponse, StateEventMsg, SubscriptionRequest, SubscriptionScope, TurnCommand,
    TurnTerminalView,
};
use minicore_runtime::wire::{
    AgentId, CanonicalFileUri, CommandId, FileUriFamily, SessionId, TurnId,
};
use minicore_runtime::workspace::{
    RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspaceRootInput,
    WorkspaceRootKey, WorkspaceSourcePolicy,
};
use minicore_runtime::{MiniCoreRuntime, MiniCoreRuntimeConfig};

const OPENAI_OPT_IN: &str = "MINICORE_LIVE_OPENAI_OPT_IN";
const OPENAI_ENDPOINT: &str = "MINICORE_LIVE_OPENAI_ENDPOINT";
const OPENAI_API_MODEL: &str = "MINICORE_LIVE_OPENAI_API_MODEL";
const OPENAI_CREDENTIAL: &str = "MINICORE_LIVE_OPENAI_CREDENTIAL";

const ANTHROPIC_OPT_IN: &str = "MINICORE_LIVE_ANTHROPIC_OPT_IN";
const ANTHROPIC_ENDPOINT: &str = "MINICORE_LIVE_ANTHROPIC_ENDPOINT";
const ANTHROPIC_API_MODEL: &str = "MINICORE_LIVE_ANTHROPIC_API_MODEL";
const ANTHROPIC_CREDENTIAL: &str = "MINICORE_LIVE_ANTHROPIC_CREDENTIAL";
const ANTHROPIC_VERSION: &str = "MINICORE_LIVE_ANTHROPIC_VERSION";

/// The explicit live wait bound is a pure operational bound (a hung network or
/// dead stream must not block forever), not an absence proof.
const LIVE_SMOKE_TIMEOUT: Duration = Duration::from_secs(120);

const SMOKE_PROMPT: &str = "Reply with exactly one word: ok";

/// The env-backed dynamic `CredentialSource` installed through the public host
/// API. `resolve()` only constructs and returns its future: the `std::env::var`
/// read and the `ProviderCredential` parsing happen entirely inside that future,
/// with no synchronous resolution and no spawned/detached task. A missing or
/// unparseable credential resolves to `None` (typed `AuthMissing`/`NotSent`) and
/// is never printed.
#[derive(Clone)]
struct EnvCredentialSource {
    variable: &'static str,
}

impl CredentialSource for EnvCredentialSource {
    fn resolve(&self) -> CredentialSourceFuture<'_> {
        let variable = self.variable;
        Box::pin(async move {
            let raw = std::env::var(variable).ok()?;
            raw.parse::<ProviderCredential>().ok()
        })
    }
}

/// The opt-in must be exactly `1`; any other value (or absence) aborts the test
/// with a message that names only the variable.
fn require_opt_in(name: &str) {
    match std::env::var(name).as_deref() {
        Ok("1") => {}
        _ => panic!("{name} must be exactly 1 to run this live provider smoke test"),
    }
}

/// Required nonsecret environment variables have no defaults. The value is
/// consumed but never printed.
fn require_nonsecret(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("missing required environment variable {name}"))
}

/// The credential variable is required to be present; only its presence is
/// checked here, never its value (parsing happens inside the credential source
/// at resolution time).
fn require_credential_present(name: &str) {
    if std::env::var_os(name).is_none_or(|value| value.is_empty()) {
        panic!("missing required environment variable {name}");
    }
}

/// The stable `ModelSelection` deliberately differs from the private API wire
/// model name: the durable identity is `{provider_id}/smoke`, while the wire
/// name is the env-provided real model. The descriptor is deliberately
/// conservative: version 1, Provider-default reasoning, Standard service class,
/// no structured output, default max output 64, bytes-per-token 4.
fn smoke_descriptor(provider_id: &str, api_model: &str) -> ModelProviderDescriptor {
    ModelProviderDescriptor::new(
        ModelSelection::new(
            provider_id
                .parse()
                .expect("the static smoke provider id parses"),
            "smoke".parse().expect("the static smoke model id parses"),
        ),
        NonZeroU64::new(1).expect("the static smoke descriptor version is nonzero"),
        api_model,
        NonZeroU32::new(64).expect("the static smoke default max output is nonzero"),
        NonZeroU32::new(4).expect("the static smoke bytes-per-token rate is nonzero"),
    )
    .expect("the smoke descriptor validates")
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit opt-in live OpenAI Responses smoke test: requires MINICORE_LIVE_OPENAI_OPT_IN=1 and the full documented env set with real credentials; excluded from ./scripts/check.sh, ./scripts/check-msrv.sh, and default cargo test"]
async fn openai_responses_live_smoke() {
    require_opt_in(OPENAI_OPT_IN);
    let endpoint = require_nonsecret(OPENAI_ENDPOINT);
    let api_model = require_nonsecret(OPENAI_API_MODEL);
    require_credential_present(OPENAI_CREDENTIAL);

    let config = ModelProviderConfig::openai_responses(
        &endpoint,
        ProviderEndpointPolicy::HttpsOnly,
        Arc::new(EnvCredentialSource {
            variable: OPENAI_CREDENTIAL,
        }),
        vec![smoke_descriptor("openai-live", &api_model)],
    )
    .expect("the OpenAI live installation validates under HttpsOnly");

    run_live_smoke(config, "openai-live").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit opt-in live Anthropic Messages smoke test: requires MINICORE_LIVE_ANTHROPIC_OPT_IN=1 and the full documented env set with real credentials; excluded from ./scripts/check.sh, ./scripts/check-msrv.sh, and default cargo test"]
async fn anthropic_messages_live_smoke() {
    require_opt_in(ANTHROPIC_OPT_IN);
    let endpoint = require_nonsecret(ANTHROPIC_ENDPOINT);
    let api_model = require_nonsecret(ANTHROPIC_API_MODEL);
    let version = require_nonsecret(ANTHROPIC_VERSION);
    require_credential_present(ANTHROPIC_CREDENTIAL);

    let config = ModelProviderConfig::anthropic_messages(
        &endpoint,
        ProviderEndpointPolicy::HttpsOnly,
        &version,
        Arc::new(EnvCredentialSource {
            variable: ANTHROPIC_CREDENTIAL,
        }),
        vec![smoke_descriptor("anthropic-live", &api_model)],
    )
    .expect("the Anthropic live installation validates under HttpsOnly");

    run_live_smoke(config, "anthropic-live").await;
}

/// Opens the Runtime with the installed live provider, runs the smoke flow, and
/// always shuts the Runtime down — including on every failure path. No helper
/// prints the credential, request/response bodies, endpoint, or API model env
/// value.
async fn run_live_smoke(config: ModelProviderConfig, provider_id: &str) {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let runtime = MiniCoreRuntime::open(
        MiniCoreRuntimeConfig::new(root.path().to_owned()).with_model_provider(config),
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_or_else(|error| panic!("live Runtime open failed: {error:?}"));

    let selection = smoke_selection(provider_id);
    let outcome = run_smoke_flow(&runtime, workspace.path(), selection).await;
    runtime.shutdown().await;
    if let Err(message) = outcome {
        panic!("{message}");
    }
}

fn smoke_selection(provider_id: &str) -> ModelSelection {
    // The session model selection is rebuilt identically to the installed
    // descriptor's stable selection; the per-test provider id is fixed.
    ModelSelection::new(
        provider_id
            .parse()
            .expect("the static smoke provider id parses"),
        "smoke".parse().expect("the static smoke model id parses"),
    )
}

async fn run_smoke_flow(
    runtime: &MiniCoreRuntime,
    workspace: &Path,
    selection: ModelSelection,
) -> Result<(), String> {
    let agent_id = create_agent(runtime).await?;
    let session_id = create_session(runtime, agent_id, workspace, selection).await?;
    load_session(runtime, session_id).await?;

    let mut events = runtime
        .subscribe(SubscriptionRequest::new(
            SubscriptionScope::Session { session_id },
            false,
        ))
        .await
        .map_err(|_| "the Session-scope subscription failed to open".to_string())?;
    match events.recv().await {
        Some(EventFrame::Snapshot(SnapshotResponse::Session(_))) => {}
        _ => return Err("the Session subscription must start with its snapshot".to_string()),
    }

    let turn_id = submit_turn(runtime, session_id).await?;
    tokio::time::timeout(LIVE_SMOKE_TIMEOUT, wait_for_turn_terminal(&mut events, turn_id))
        .await
        .map_err(|_| {
            format!("the live Turn did not reach its terminal event within the {LIVE_SMOKE_TIMEOUT:?} operational bound")
        })?
}

async fn create_agent(runtime: &MiniCoreRuntime) -> Result<AgentId, String> {
    let response = runtime
        .dispatch(CommandRequest::new(
            generate_command_id()?,
            RuntimeCommand::Agent(AgentCommand::Create {
                definition: NewAgentDefinition::new(
                    AgentPromptSelection::new(Vec::new())
                        .map_err(|_| "the Agent prompt selection failed to validate".to_string())?,
                ),
                metadata: NewAgentMetadata::new("Live Smoke Agent", None::<&str>)
                    .map_err(|_| "the Agent metadata failed to validate".to_string())?,
            }),
        ))
        .await
        .map_err(|_| "Agent Create dispatch failed".to_string())?;
    match response.completion() {
        CommandCompletion::Completed {
            outcome: CommandOutcome::AgentCreated { agent_id, .. },
            output: None,
        } => Ok(*agent_id),
        completion => Err(format!(
            "Agent Create returned an unexpected completion: {completion:?}"
        )),
    }
}

async fn create_session(
    runtime: &MiniCoreRuntime,
    agent_id: AgentId,
    workspace: &Path,
    selection: ModelSelection,
) -> Result<SessionId, String> {
    let response = runtime
        .dispatch(CommandRequest::new(
            generate_command_id()?,
            RuntimeCommand::Session(SessionCommand::Create {
                agent_id,
                definition: Box::new(NewSessionDefinition::new(
                    workspace_input(workspace)?,
                    SessionModelConfig::new(selection, ReasoningPreference::Auto, None),
                    SessionPromptSelection::new(Vec::new()).map_err(|_| {
                        "the Session prompt selection failed to validate".to_string()
                    })?,
                )),
                metadata: NewSessionMetadata::new(None::<&str>, None::<&str>)
                    .map_err(|_| "the Session metadata failed to validate".to_string())?,
            }),
        ))
        .await
        .map_err(|_| "Session Create dispatch failed".to_string())?;
    match response.completion() {
        CommandCompletion::Completed {
            outcome: CommandOutcome::CommandOutput,
            output: Some(output),
        } => output
            .text()
            .parse::<SessionId>()
            .map_err(|_| "Session Create returned an unparseable Session id".to_string()),
        completion => Err(format!(
            "Session Create returned an unexpected completion: {completion:?}"
        )),
    }
}

async fn load_session(runtime: &MiniCoreRuntime, session_id: SessionId) -> Result<(), String> {
    let response = runtime
        .dispatch(CommandRequest::new(
            generate_command_id()?,
            RuntimeCommand::Session(SessionCommand::Load { session_id }),
        ))
        .await
        .map_err(|_| "Session Load dispatch failed".to_string())?;
    match response.completion() {
        CommandCompletion::Completed {
            outcome: CommandOutcome::CommandOutput,
            output: Some(output),
        } if output.text() == "session loaded" => Ok(()),
        completion => Err(format!(
            "Session Load returned an unexpected completion: {completion:?}"
        )),
    }
}

async fn submit_turn(runtime: &MiniCoreRuntime, session_id: SessionId) -> Result<TurnId, String> {
    let response =
        runtime
            .dispatch(CommandRequest::new(
                generate_command_id()?,
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id,
                    intent: PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new(SMOKE_PROMPT).map_err(|_| {
                            "the smoke prompt intent failed to validate".to_string()
                        })?),
                        Vec::new(),
                    )
                    .map_err(|_| "the smoke prompt intent failed to validate".to_string())?,
                }),
            ))
            .await
            .map_err(|_| "Turn Submit dispatch failed".to_string())?;
    match response.completion() {
        CommandCompletion::Completed {
            outcome: CommandOutcome::TurnStarted { turn_id },
            output: None,
        } => Ok(*turn_id),
        completion => Err(format!(
            "Turn Submit returned an unexpected completion: {completion:?}"
        )),
    }
}

/// Waits for the exact Turn terminal event of the submitted TurnId. A Completed
/// terminal is success; Failed/Interrupted panic with only the typed,
/// payload-free enum info (`TurnFailureView`/`TurnInterruptionView` carry no
/// payload, so this can never echo a secret or a body).
async fn wait_for_turn_terminal(events: &mut EventStream, turn_id: TurnId) -> Result<(), String> {
    loop {
        let Some(frame) = events.recv().await else {
            return Err("the Session event stream closed before the Turn terminal".to_string());
        };
        let EventFrame::State(event) = frame else {
            continue;
        };
        let StateEventMsg::Session { .. } = event.msg() else {
            continue;
        };
        let Some(SessionEventDetail::TurnTerminal {
            turn_id: completed_turn,
            terminal,
        }) = event.msg().session_detail()
        else {
            continue;
        };
        if completed_turn != turn_id {
            continue;
        }
        match terminal {
            TurnTerminalView::Completed { .. } => return Ok(()),
            TurnTerminalView::Failed { reason, .. } => {
                return Err(format!("live provider Turn failed: {reason:?}"));
            }
            TurnTerminalView::Interrupted { reason, .. } => {
                return Err(format!("live provider Turn interrupted: {reason:?}"));
            }
        }
    }
}

fn generate_command_id() -> Result<CommandId, String> {
    CommandId::generate().map_err(|_| "command id generation failed".to_string())
}

fn workspace_uri(path: &Path) -> Result<CanonicalFileUri, String> {
    #[cfg(windows)]
    {
        let native = path
            .to_str()
            .ok_or_else(|| "the temporary Windows workspace path is not UTF-8".to_string())?;
        let native = native.strip_prefix("\\\\?\\").unwrap_or(native);
        if let Some(unc) = native
            .strip_prefix("UNC\\")
            .or_else(|| native.strip_prefix("\\\\"))
        {
            let (authority, native_path) = unc
                .split_once('\\')
                .ok_or_else(|| "the temporary Windows UNC workspace path is invalid".to_string())?;
            return CanonicalFileUri::from_decoded_parts(
                FileUriFamily::Unc,
                Some(authority),
                &native_path.replace('\\', "/"),
            )
            .map_err(|_| "the temporary Windows UNC workspace URI is invalid".to_string());
        }

        let bytes = native.as_bytes();
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || bytes[2] != b'\\'
        {
            return Err("the temporary Windows drive workspace path is invalid".to_string());
        }
        let drive = char::from(bytes[0]).to_ascii_uppercase();
        let tail = native[3..].replace('\\', "/");
        let decoded_path = if tail.is_empty() {
            format!("{drive}:/")
        } else {
            format!("{drive}:/{tail}")
        };
        return CanonicalFileUri::from_decoded_parts(FileUriFamily::Drive, None, &decoded_path)
            .map_err(|_| "the temporary Windows drive workspace URI is invalid".to_string());
    }
    #[cfg(not(windows))]
    {
        let decoded_path = path
            .to_str()
            .ok_or_else(|| "the temporary POSIX workspace path is not UTF-8".to_string())?;
        CanonicalFileUri::from_decoded_parts(FileUriFamily::Posix, None, decoded_path)
            .map_err(|_| "the temporary POSIX workspace URI is invalid".to_string())
    }
}

fn workspace_input(path: &Path) -> Result<WorkspaceDefinitionInput, String> {
    let key: WorkspaceRootKey = "repo"
        .parse()
        .map_err(|_| "the static Workspace root key is invalid".to_string())?;
    WorkspaceDefinitionInput::new(
        WorkspaceRootInput::new(
            key.clone(),
            workspace_uri(path)?,
            RequestedFilesystemAccess::ReadWrite,
            WorkspaceSourcePolicy::new(false, false),
        ),
        Vec::new(),
        WorkspaceCwdSpec::new(
            key,
            "src"
                .parse()
                .map_err(|_| "the static Workspace cwd is invalid".to_string())?,
        ),
    )
    .map_err(|_| "the temporary Workspace input failed to validate".to_string())
}

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
                "minicore-live-provider-smoke-root-{}-{suffix}",
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
        if self.path.is_dir() {
            std::fs::remove_dir_all(&self.path)
                .expect("the temporary durable root is removed deterministically");
        } else if self.path.exists() {
            std::fs::remove_file(&self.path)
                .expect("the temporary durable root file is removed deterministically");
        }
    }
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
                "minicore-live-provider-smoke-workspace-{}-{suffix}",
                std::process::id()
            ));
            if !path.exists() {
                std::fs::create_dir_all(path.join("src"))
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
            std::fs::remove_dir_all(&self.path)
                .expect("the temporary Workspace root is removed deterministically");
        } else if self.path.exists() {
            std::fs::remove_file(&self.path)
                .expect("the temporary Workspace file is removed deterministically");
        }
    }
}
