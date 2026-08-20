//! M14 live provider smoke tests: explicit opt-in, real-network smoke through
//! the current public typed Runtime path.
//!
//! These two ignored tests are intentionally offline by default. They require
//! their exact opt-in variable and documented environment contract before any
//! network client is opened. Failure text never includes endpoint, API model,
//! credential, request body, or response body values.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use minicore_runtime::{
    AnthropicMessagesProvider, CredentialSource, CredentialSourceFuture, ModelDescriptor,
    ModelLimits, ModelSelection, OpenAiResponsesProvider, ProviderCredential,
    ProviderEndpointPolicy, ProviderRegistry, ReasoningPreference, RetryPolicy, Runtime,
    RuntimeConfig, SessionConfig, SessionEvent, SessionStatus, ToolRegistry, TranscriptEntry,
    TurnId, TurnOutcome,
};
use tokio::runtime::Handle;

const OPENAI_OPT_IN: &str = "MINICORE_LIVE_OPENAI_OPT_IN";
const OPENAI_ENDPOINT: &str = "MINICORE_LIVE_OPENAI_ENDPOINT";
const OPENAI_API_MODEL: &str = "MINICORE_LIVE_OPENAI_API_MODEL";
const OPENAI_CREDENTIAL: &str = "MINICORE_LIVE_OPENAI_CREDENTIAL";

const ANTHROPIC_OPT_IN: &str = "MINICORE_LIVE_ANTHROPIC_OPT_IN";
const ANTHROPIC_ENDPOINT: &str = "MINICORE_LIVE_ANTHROPIC_ENDPOINT";
const ANTHROPIC_API_MODEL: &str = "MINICORE_LIVE_ANTHROPIC_API_MODEL";
const ANTHROPIC_CREDENTIAL: &str = "MINICORE_LIVE_ANTHROPIC_CREDENTIAL";
const ANTHROPIC_VERSION: &str = "MINICORE_LIVE_ANTHROPIC_VERSION";

/// This is an operational bound for a hung real network stream, not an
/// absence proof. The tests are ignored and never run in default gates.
const LIVE_SMOKE_TIMEOUT: Duration = Duration::from_secs(120);
const SMOKE_PROMPT: &str = "Reply with exactly one word: ok";

/// Reads and parses credentials only when the provider asks for a credential.
/// The source owns no credential value in the test process after construction;
/// missing or invalid values resolve to `None`.
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

#[derive(Debug)]
enum SmokeFailure {
    Message(&'static str),
    Terminal(TurnOutcome),
}

fn require_opt_in(name: &str) {
    if std::env::var(name).as_deref() != Ok("1") {
        panic!("{name} must be exactly 1 to run this live provider smoke test");
    }
}

fn require_nonsecret(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("missing required environment variable {name}"))
}

fn require_credential_present(name: &str) {
    if std::env::var_os(name).is_none_or(|value| value.is_empty()) {
        panic!("missing required environment variable {name}");
    }
}

fn smoke_selection(provider: &str) -> Result<ModelSelection, SmokeFailure> {
    Ok(ModelSelection::new(
        provider
            .parse()
            .map_err(|_| SmokeFailure::Message("static smoke provider id is invalid"))?,
        "smoke"
            .parse()
            .map_err(|_| SmokeFailure::Message("static smoke model id is invalid"))?,
    ))
}

fn smoke_descriptor(provider: &str, api_model: &str) -> Result<ModelDescriptor, SmokeFailure> {
    let selection = smoke_selection(provider)?;
    ModelDescriptor::new(
        selection,
        api_model,
        ModelLimits::new(None, Some(64))
            .map_err(|_| SmokeFailure::Message("live model limits are invalid"))?,
        BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
    )
    .map_err(|_| SmokeFailure::Message("live model descriptor is invalid"))
}

fn build_openai_registry(
    endpoint: &str,
    api_model: &str,
) -> Result<(ProviderRegistry, ModelSelection), SmokeFailure> {
    let descriptor = smoke_descriptor("openai-live", api_model)?;
    let provider = OpenAiResponsesProvider::new(
        endpoint,
        ProviderEndpointPolicy::HttpsOnly,
        Arc::new(EnvCredentialSource {
            variable: OPENAI_CREDENTIAL,
        }),
        vec![descriptor],
    )
    .map_err(|_| SmokeFailure::Message("OpenAI live provider configuration is invalid"))?;
    let mut registry = ProviderRegistry::builder();
    registry
        .register(provider)
        .map_err(|_| SmokeFailure::Message("OpenAI live provider registration failed"))?;
    Ok((registry.build(), smoke_selection("openai-live")?))
}

fn build_anthropic_registry(
    endpoint: &str,
    api_model: &str,
    version: &str,
) -> Result<(ProviderRegistry, ModelSelection), SmokeFailure> {
    let descriptor = smoke_descriptor("anthropic-live", api_model)?;
    let provider = AnthropicMessagesProvider::new(
        endpoint,
        ProviderEndpointPolicy::HttpsOnly,
        version,
        Arc::new(EnvCredentialSource {
            variable: ANTHROPIC_CREDENTIAL,
        }),
        vec![descriptor],
    )
    .map_err(|_| SmokeFailure::Message("Anthropic live provider configuration is invalid"))?;
    let mut registry = ProviderRegistry::builder();
    registry
        .register(provider)
        .map_err(|_| SmokeFailure::Message("Anthropic live provider registration failed"))?;
    Ok((registry.build(), smoke_selection("anthropic-live")?))
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit opt-in live OpenAI Responses smoke test; requires the documented environment contract and is never run by default"]
async fn openai_responses_live_smoke() {
    require_opt_in(OPENAI_OPT_IN);
    let endpoint = require_nonsecret(OPENAI_ENDPOINT);
    let api_model = require_nonsecret(OPENAI_API_MODEL);
    require_credential_present(OPENAI_CREDENTIAL);
    let (providers, selection) = build_openai_registry(&endpoint, &api_model)
        .unwrap_or_else(|_| panic!("OpenAI live provider setup failed"));
    run_live_smoke(providers, selection).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit opt-in live Anthropic Messages smoke test; requires the documented environment contract and is never run by default"]
async fn anthropic_messages_live_smoke() {
    require_opt_in(ANTHROPIC_OPT_IN);
    let endpoint = require_nonsecret(ANTHROPIC_ENDPOINT);
    let api_model = require_nonsecret(ANTHROPIC_API_MODEL);
    let version = require_nonsecret(ANTHROPIC_VERSION);
    require_credential_present(ANTHROPIC_CREDENTIAL);
    let (providers, selection) = build_anthropic_registry(&endpoint, &api_model, &version)
        .unwrap_or_else(|_| panic!("Anthropic live provider setup failed"));
    run_live_smoke(providers, selection).await;
}

async fn run_live_smoke(providers: ProviderRegistry, selection: ModelSelection) {
    let root = TempRoot::new();
    let workspace = TempWorkspace::new();
    let retry_policy = RetryPolicy::new(1, Duration::ZERO)
        .unwrap_or_else(|_| panic!("live retry policy is invalid"));
    let config = RuntimeConfig::new(
        root.path().to_owned(),
        providers,
        ToolRegistry::default(),
        "live provider smoke instructions",
        retry_policy,
    )
    .unwrap_or_else(|_| panic!("live Runtime configuration is invalid"));
    let runtime = Runtime::open(config, Handle::current())
        .await
        .unwrap_or_else(|_| panic!("typed Runtime open failed"));

    let result = run_smoke_flow(&runtime, workspace.path(), selection).await;
    let shutdown = runtime.shutdown().await;
    if shutdown.is_err() {
        panic!("typed Runtime shutdown failed");
    }
    match result {
        Ok(()) => {}
        Err(SmokeFailure::Message(message)) => panic!("{message}"),
        Err(SmokeFailure::Terminal(outcome)) => {
            panic!("live provider Turn outcome was not Completed: {outcome:?}")
        }
    }
}

async fn run_smoke_flow(
    runtime: &Runtime,
    workspace: &Path,
    selection: ModelSelection,
) -> Result<(), SmokeFailure> {
    let session = SessionConfig::new(
        workspace.to_owned(),
        selection,
        "live provider smoke system prompt",
        BTreeSet::new(),
        1_000_000,
        900_000,
        8,
    )
    .map_err(|_| SmokeFailure::Message("live Session configuration is invalid"))?;
    let session_id = runtime
        .create_session(session)
        .await
        .map_err(|_| SmokeFailure::Message("typed Session creation failed"))?;
    let mut events = runtime
        .subscribe(session_id)
        .map_err(|_| SmokeFailure::Message("typed Session subscription failed"))?;
    match events.recv().await {
        Some(SessionEvent::Snapshot(snapshot))
            if snapshot.session_id() == session_id && snapshot.status() == SessionStatus::Idle => {}
        _ => {
            return Err(SmokeFailure::Message(
                "Session subscription was not snapshot-first",
            ));
        }
    }

    let turn_id = runtime
        .submit(session_id, SMOKE_PROMPT.to_owned())
        .await
        .map_err(|_| SmokeFailure::Message("typed Turn submission failed"))?;
    let outcome = tokio::time::timeout(
        LIVE_SMOKE_TIMEOUT,
        wait_for_turn_terminal(&mut events, turn_id),
    )
    .await
    .map_err(|_| SmokeFailure::Message("live Turn exceeded the operational wait bound"))??;
    if outcome != TurnOutcome::Completed {
        return Err(SmokeFailure::Terminal(outcome));
    }

    let page = runtime
        .transcript(session_id, None, 200)
        .await
        .map_err(|_| SmokeFailure::Message("typed transcript read failed"))?;
    let mut has_user = false;
    let mut has_assistant = false;
    let mut has_terminal = false;
    for entry in page.entries() {
        match entry {
            TranscriptEntry::User {
                text,
                turn_id: current,
                ..
            } if *current == turn_id && text == SMOKE_PROMPT => has_user = true,
            TranscriptEntry::Assistant {
                text: Some(text),
                turn_id: current,
                ..
            } if *current == turn_id && !text.is_empty() => has_assistant = true,
            TranscriptEntry::Terminal {
                turn_id: current,
                outcome: TurnOutcome::Completed,
                ..
            } if *current == turn_id => has_terminal = true,
            _ => {}
        }
    }
    if !has_user {
        return Err(SmokeFailure::Message(
            "transcript did not preserve the smoke prompt",
        ));
    }
    if !has_assistant {
        return Err(SmokeFailure::Message(
            "transcript did not contain Assistant text",
        ));
    }
    if !has_terminal {
        return Err(SmokeFailure::Message(
            "transcript did not contain a Completed terminal",
        ));
    }
    let snapshot = runtime
        .snapshot(session_id)
        .map_err(|_| SmokeFailure::Message("typed Session snapshot read failed"))?;
    if snapshot.status() != SessionStatus::Idle
        || snapshot.last_terminal().map(|terminal| terminal.turn_id) != Some(turn_id)
    {
        return Err(SmokeFailure::Message(
            "final Session snapshot was not terminal Idle",
        ));
    }
    if snapshot.usage().input_tokens().is_none() && snapshot.usage().output_tokens().is_none() {
        return Err(SmokeFailure::Message(
            "provider reported no known usage field",
        ));
    }
    Ok(())
}

async fn wait_for_turn_terminal(
    events: &mut minicore_runtime::SessionEventStream,
    turn_id: TurnId,
) -> Result<TurnOutcome, SmokeFailure> {
    loop {
        let event = events.recv().await.ok_or(SmokeFailure::Message(
            "Session stream closed before Turn terminal",
        ))?;
        if let SessionEvent::TurnFinished {
            turn_id: current,
            outcome,
        } = event
        {
            if current == turn_id {
                return Ok(outcome);
            }
        }
    }
}

#[test]
fn live_smoke_source_uses_only_the_current_typed_runtime_surface() {
    let source = include_str!("../tests/m14_live_provider_smoke.rs");
    let forbidden = [
        ["MiniCore", "Runtime"].concat(),
        ["agent_session", "_lifecycle"].concat(),
        ["runtime", "_interface"].concat(),
        ["model", "_gateway"].concat(),
        ["dis", "patch"].concat(),
        ["que", "ry"].concat(),
        ["w", "ire"].concat(),
    ];
    for value in forbidden {
        assert!(
            !source.contains(&value),
            "legacy live smoke surface: {value}"
        );
    }
}

static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        loop {
            let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
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
                .expect("temporary durable root cleanup must succeed");
        } else if self.path.exists() {
            std::fs::remove_file(&self.path)
                .expect("temporary durable root file cleanup must succeed");
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
            let path = std::env::temp_dir().join(format!(
                "minicore-live-provider-smoke-workspace-{}-{suffix}",
                std::process::id()
            ));
            if !path.exists() {
                std::fs::create_dir_all(path.join("src"))
                    .expect("temporary workspace creation must succeed");
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
            std::fs::remove_dir_all(&self.path).expect("temporary workspace cleanup must succeed");
        } else if self.path.exists() {
            std::fs::remove_file(&self.path)
                .expect("temporary workspace file cleanup must succeed");
        }
    }
}
