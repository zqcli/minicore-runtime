# MiniCore Runtime

MiniCore Runtime is an embeddable Rust 2024 runtime for multiple durable coding `Session`s. Each `Session` has at most one active `Turn`. The v0.2 core provides a typed `Runtime`, bounded session actors, streaming model calls, ordered tool rounds, durable JSONL conversation recording, context compaction, cancellation, and snapshot-first observation.

The crate is intentionally a core, not a command-line product or server. A host supplies the provider catalog, credentials, workspace root, tool policy, and Tokio runtime handle. The host also decides when a real provider is installed; opening a runtime with an empty provider registry is valid, but creating a session that selects an unavailable model is not.

## Implemented Core

- Checked identifiers for sessions, turns, interactions, and tool calls.
- Typed `RuntimeConfig` and `SessionConfig` with bounded text, capacity, tool, retry, and compaction values.
- Immutable `ProviderRegistry` and `ToolRegistry` values.
- Model-owned OpenAI Responses and Anthropic Messages providers with bounded SSE handling, opaque credentials, delivery-aware errors, and stateless full requests.
- One actor per loaded session with one bounded command mailbox and out-of-band cancellation.
- The four public session states: `Idle`, `Running`, `WaitingForInput`, and `Closing`.
- Snapshot-first subscriptions with bounded event delivery and lag recovery.
- Append-only session metadata and conversation storage with restart repair, partial-tail repair, and located middle-file corruption.
- Current-turn-aware prompt projection and append-only compaction summaries.
- Capability-relative filesystem tools and structured direct-child process execution.
- Truthful interaction claim, cancellation, denial, failure, and terminal settlement.
- Explicit `Runtime::shutdown()` ownership of completion, with asynchronous best-effort cleanup from `Drop`.

## Install and MSRV

The crate targets Rust `1.85` and edition 2024. Use the pinned toolchain for the minimum-version gate:

```bash
rustup run 1.85.0 cargo test --locked
rustup run 1.85.0 cargo clippy --all-targets --all-features --locked -- -D warnings
```

For normal development:

```bash
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

The host application owns provider installation and credential resolution. No provider, API key, ambient network configuration, or default tool set is installed by this crate.

## Typed API

The provider registry must contain a real provider before `create_session` can succeed. This example constructs an OpenAI Responses descriptor and provider from host-supplied endpoint and credential values, registers it, opens the runtime, creates a session, and shuts down explicitly.

```rust,no_run
use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use minicore_runtime::model::{
    fixed_credential_source, ModelDescriptor, ModelLimits, ModelSelection,
    OpenAiResponsesProvider, ProviderRegistry, ReasoningPreference,
};
use minicore_runtime::tools::{ReadFileTool, ToolRegistry};
use minicore_runtime::{RetryPolicy, Runtime, RuntimeConfig, SessionConfig};
use tokio::runtime::Handle;

fn real_provider_registry(
    endpoint: &str,
    credential: &str,
) -> Result<ProviderRegistry, Box<dyn Error>> {
    let selection = ModelSelection::new("openai".parse()?, "coding-model".parse()?);
    let descriptor = ModelDescriptor::new(
        selection.clone(),
        "coding-model",
        ModelLimits::new(Some(128_000), Some(4_096))?,
        BTreeSet::from([ReasoningPreference::Auto]),
    )?;
    let provider = OpenAiResponsesProvider::new_https(
        endpoint,
        fixed_credential_source(credential)?,
        vec![descriptor],
    )?;
    let mut registry = ProviderRegistry::builder();
    registry.register(provider)?;
    Ok(registry.build())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let credential = std::env::var("OPENAI_API_KEY")?;
    let providers = real_provider_registry(
        "https://api.openai.com/v1/responses",
        &credential,
    )?;

    let mut tools = ToolRegistry::builder();
    tools.register(ReadFileTool::new())?;

    let runtime = Runtime::open(
        RuntimeConfig::new(
            PathBuf::from("/absolute/data/root"),
            providers,
            tools.build(),
            "You are a coding assistant.",
            RetryPolicy::new(3, Duration::from_millis(250))?,
        )?,
        Handle::current(),
    )
    .await?;

    let session_id = runtime
        .create_session(SessionConfig::new(
            PathBuf::from("/absolute/workspace/root"),
            ModelSelection::new("openai".parse()?, "coding-model".parse()?),
            "Work only inside the configured workspace.",
            BTreeSet::from(["read_file".parse()?]),
            80_000,
            30_000,
            16,
        )?)
        .await?;

    let _initial_snapshot = runtime.snapshot(session_id)?;
    let events = runtime.subscribe(session_id)?;
    runtime.submit(session_id, "Inspect the repository.".to_owned()).await?;
    drop(events);
    runtime.shutdown().await?;
    Ok(())
}
```

`Runtime::open` returns `RuntimeError`. Session creation, loading, listing, submission, interaction answers, snapshots, subscriptions, transcripts, closing, and deletion use `SessionError`. `Runtime::shutdown` returns `RuntimeError` and is the completion barrier for runtime-owned cleanup.

## Public Modules

| Module | Public responsibility |
| --- | --- |
| `config` | `RuntimeConfig`, `SessionConfig`, `RetryPolicy`, and checked configuration errors |
| `error` | Runtime/session error summaries and public error codes |
| `event` | The public session event-kind catalog |
| `ids` | Checked `SessionId`, `TurnId`, `InteractionId`, and `ToolCallId` |
| `model` | Provider traits, registry, descriptors, requests, responses, credentials, and provider implementations |
| `runtime` | `Runtime` and `SessionSummary` |
| `session` | Events, snapshots, statuses, outcomes, transcript DTOs, and terminal summaries |
| `tools` | Tool traits, registry, policies, interaction types, process policy, and builtins |
| `workspace` | Relative paths, root capability access, directory entries, and workspace errors |

The `agent`, `prompt`, and `storage` modules are implementation-private. Storage, actor, provider transport, and compaction internals are not additional public extension points.

## Builtins

| Tool | Registration | Contract |
| --- | --- | --- |
| `ask_user` | `AskUserTool` | Safe UTF-8 question/answer text is `1..=8192` bytes; choices are optional, `1..=32` when present, and each choice is `1..=1024` bytes. |
| `read_file` | `ReadFileTool::new()` | Reads one UTF-8 file by a workspace-relative path; result is bounded to 256 KiB. |
| `list_directory` | `ListDirectoryTool::new()` | Lists direct entries only, sorted by name, as compact JSON; empty path means the workspace root. |
| `write_file` | `WriteFileTool::new()` | Replaces one UTF-8 file by a workspace-relative path; content is bounded to 256 KiB and never creates directories. |
| `run_command` | `RunCommandTool::new(Arc<ProcessPolicy>)` | Runs one direct executable with structured `program`, `args`, `cwd`, `timeout_ms`, and `env` fields. |

Tool registration is explicit. A session may enable only names present in the frozen registry. The default host policy can register no tools.

## Persistence

For a runtime data directory, the store owns:

```text
<data_dir>/runtime.lock
<data_dir>/sessions/<ses_...>/session.json
<data_dir>/sessions/<ses_...>/conversation.jsonl
```

`runtime.lock` is held by the store worker for the lifetime of the store. Session creation writes a temporary session directory, flushes and synchronizes its files, and renames the directory into place. There are no generations, aliases, or background detached writers in the v2 store.

`session.json` is the checked session configuration. `conversation.jsonl` is append-only semantic history. A completed tool exchange, interaction, terminal outcome, and compaction summary are durable entries; a summary never rewrites or deletes source history. The prompt view may use the latest summary boundary while transcript paging still exposes the retained entries.

## Security Boundaries

Filesystem tools operate through a single capability-backed workspace root. Paths are absolute only at configuration time and are otherwise relative, lexical, bounded, and checked against the captured root. Symlink escape, invalid final targets, read-only access, non-UTF-8 data, and oversized data fail closed.

`run_command` is a different authority boundary. It receives a structured executable and argument list, never a shell string. The workspace-relative `cwd` is validated before spawn, then passed as an ambient host path to the direct child. The child uses host authority; MiniCore does not claim a process sandbox or process-tree sandbox. `ProcessPolicy` controls whether execution is enabled, which programs are allowed, which environment keys may be inherited, timeouts, and output bounds. The implementation clears the environment before adding explicitly allowed values.

Credentials are opaque checked values and are resolved inside each provider attempt. Errors and debug output do not expose credentials, request bodies, endpoint secrets, or model wire names.

## Lifecycle and Shutdown

A host normally opens one `Runtime`, creates or loads sessions, observes a snapshot before consuming events, submits input, answers pending interactions, and closes or deletes sessions when finished. `Runtime::cancel` is synchronous because it only requests cancellation; terminal settlement remains owned by the session actor.

Call `Runtime::shutdown().await` before tearing down the Tokio runtime or its provider resources. The production `SessionActor` awaits `Workspace::shutdown()` during session close. The `Workspace` Drop fallback may block synchronously and is not preferred. The explicit Runtime shutdown result observes every known session actor and workspace shutdown, store worker exit, root-lock release, and other runtime cleanup. Dropping the last runtime owner starts the same cleanup path asynchronously but cannot report its result.

## Testing

The deterministic gates are offline and do not require live credentials:

```bash
./scripts/check.sh
./scripts/check-msrv.sh
cargo test --locked --test v2_acceptance
cargo test --locked --test m14_live_provider_smoke
```

The two live provider smoke tests remain ignored unless explicitly selected with their documented opt-in environment contract. Provider protocol behavior is covered by the P3 suites and the active acceptance provider case; no live network call belongs in the default gate.

## Breaking Change

The v0.2 API and persistence reset is documented in the [final migration guide](docs/migration-v0.1-v0.2.md). It is a breaking change with no compatibility wrappers in this crate. Historical pre-reset design material is retained under `docs/archive/v2/pre-reset/` and is not current authority.
