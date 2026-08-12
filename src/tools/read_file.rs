//! The production `read_file` builtin: one closed, default-off, Runtime-owned Tool that
//! reads exactly one UTF-8 text file relative to the Workspace cwd.
//!
//! The builtin is immutable after construction and travels through the existing residency
//! ToolSet capture path.  This first slice is deliberately narrow: one required `path`
//! string argument, no offset/range/encoding/options, no absolute paths, no writes, no
//! binary/base64 output, no mutation queues.  The builtin itself stays narrow: production
//! composition belongs to the closed `ProductionToolConfig`, which fixes exactly the three
//! frozen builtins in one order (`ask_user` → `read_file` → `list_directory`) — there is no
//! generic registry and no dynamic composition.
//!
//! A call plans synchronously to one of exactly three shapes:
//!
//! - `ToolExecutionPlan::Execute` for a valid, authorized call: the exact cwd-relative
//!   `WorkspaceRelativePath` is parsed by the strict serde mirror (the semantic
//!   `WorkspaceRelativePath` grammar is the authority, never the schema guidance) and
//!   authorized synchronously through the Workspace tool context *before* any start factory
//!   exists.  The plan carries exactly `ToolCapabilityClass::FilesystemRead` and a move-only
//!   start factory; the ToolSet's sandbox contract is available exactly for
//!   `FilesystemRead`, so admission revalidates the same single class.
//! - `ToolExecutionPlan::PreExecution` with the frozen `Failed` text
//!   `tool arguments are invalid` for any parse or semantic argument failure.
//! - `ToolExecutionPlan::PreExecution` with the frozen `Denied` text
//!   `workspace file access is denied` when `WorkspaceAccessView::authorize_read` refuses
//!   the exact cwd-relative path: no readable grant, a path outside the cwd's containing
//!   root, or the root path itself (the cwd is a directory, never a read target).  Every
//!   authorization error collapses to this one frozen, bounded, non-secret text; the
//!   denial happens synchronously before constructing start.
//!
//! The started executor never uses an ambient path, `std::fs::read`, or `canonicalize`: it
//! opens the exact `AuthorizedWorkspaceReadPath` the authorization granted with the
//! nonblocking capability open (resolving through the captured `cap_std::fs::Dir`, which
//! cannot leave the root it was bound to — a symlink escape fails at that open), rejects
//! any non-regular entry by its fstat metadata (a FIFO's nonblocking open returns
//! immediately without any writer to pair with it, so a WouldBlock/nonregular open can
//! never hang), rejects an already-oversize metadata size before reading, and reads at
//! most 65,537 bytes, so oversize detection needs no unbounded allocation.  A regular
//! file's content is validated as UTF-8 and as one safe Text part (the same
//! `ToolResultContent` contract the owner enforces) and returned as exactly one Text part
//! on success, with the exact frozen `Completed + Failed` texts otherwise:
//!
//! - `file could not be read` for a missing path, a directory or other non-regular entry
//!   (including a FIFO), a WouldBlock or any other open/read error, a symlink escape
//!   denied at the capability open, or valid UTF-8 the protocol cannot disclose as one
//!   safe Text part;
//! - `file is not valid UTF-8` for invalid UTF-8;
//! - `file is too large` for content beyond 65,536 bytes.
//!
//! Cancellation is cooperative: once start has happened, a cancellation that already won
//! (or races the scheduling) before any blocking job is created settles the exact frozen
//! `Completed + Cancelled` text `file read was cancelled` — zero I/O is proven, so the
//! truthful disposition is Cancelled with that one Text part, never OutcomeUnknown.  Once
//! the tracked job is scheduled it is never dropped or detached — a cancellation arriving
//! while the read runs (even while the nonblocking capability open is registered) keeps
//! awaiting the same tracked job to its bounded settlement (one nonblocking open plus at
//! most 65,537 bytes read) and preserves the truthful result, so a known success/failure
//! is never rewritten.  Only a `RuntimeTaskError` (owner closing, worker unavailable,
//! operation panic) settles `Abandoned { RuntimeFailure }`; the signal-before-start case
//! is owned by the ToolSet's gate slot, which settles its own PreExecution Cancelled
//! result before this executor is ever constructed.

use std::fmt;
use std::io::Read;
use std::sync::Arc;

use serde::Deserialize;

use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedBlockingJob};
use crate::wire::lexical::validate_safe_text;
use crate::wire::{BoundedJsonObject, WorkspaceRelativePath};
use crate::workspace::{AuthorizedWorkspaceReadPath, WorkspaceToolContext};

use super::{
    ToolAbandonReason, ToolCancellationObserver, ToolCapabilityClass, ToolDefinition,
    ToolExecutionMode, ToolExecutionPlan, ToolExecutionRequest, ToolExecutionResult,
    ToolExecutionStart, ToolPermissionSet, ToolResultContent, ToolResultDisposition,
    ToolSandboxContract, ToolSet, ToolSetInner, ToolSpec,
};

/// The exact production builtin ToolName.  `pub(super)` because the composed production
/// ToolSet routes exactly this frozen name.
pub(super) const READ_FILE_NAME: &str = "read_file";

/// The exact production description disclosed for the builtin: reading one UTF-8 text file
/// relative to the Workspace working directory.  Frozen; asserted verbatim in module tests.
const READ_FILE_DESCRIPTION: &str = "Read one UTF-8 text file relative to the workspace working directory and return its full contents as a single text part. Use for reading source code, configuration, and other text files inside the workspace.";

/// The exact frozen PreExecution Failed text for every parse or semantic argument failure.
const INVALID_ARGUMENTS_TEXT: &str = "tool arguments are invalid";

/// The exact frozen PreExecution Denied text for every authorization failure (no readable
/// grant, a path outside the cwd's containing root, or the root path itself).
const FILE_ACCESS_DENIED_TEXT: &str = "workspace file access is denied";

/// The exact frozen Completed Cancelled text for a cancellation that won after start but
/// before any blocking job was created: zero I/O is proven, so the truthful disposition is
/// Cancelled with this one Text part, never OutcomeUnknown.
const FILE_CANCELLED_TEXT: &str = "file read was cancelled";

/// The exact frozen Completed Failed text for a missing/non-regular/read error (or valid
/// UTF-8 the protocol cannot disclose as one safe Text part).
const FILE_UNREADABLE_TEXT: &str = "file could not be read";

/// The exact frozen Completed Failed text for content that is not valid UTF-8.
const FILE_NOT_UTF8_TEXT: &str = "file is not valid UTF-8";

/// The exact frozen Completed Failed text for content beyond the one-part content bound.
const FILE_TOO_LARGE_TEXT: &str = "file is too large";

/// The closed content bound: exactly one Text part of at most 65,536 bytes on success.
const MAX_FILE_CONTENT_BYTES: usize = 65_536;

/// One byte beyond the content bound: reading at most 65,537 bytes detects oversize without
/// unbounded allocation.
const MAX_READ_BYTES: usize = MAX_FILE_CONTENT_BYTES + 1;

/// The closed input schema disclosed for the builtin.  Structural guidance only: the
/// `WorkspaceRelativePath` grammar (canonical cwd-relative path, no absolute/dot segments,
/// at most 4,096 bytes) is enforced by the semantic constructor, never by this schema; the
/// empty path is the cwd itself and is refused by the Workspace authorization as a read
/// target, so no `minLength` is disclosed.
const READ_FILE_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "maxLength": 4096
    }
  },
  "required": ["path"],
  "additionalProperties": false
}"#;

/// The exact frozen production definition/spec pair: the single source shared by the
/// standalone builtin ToolSet and the composed production ToolSet, so the disclosed
/// definition and spec are byte-identical in both selections.
pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        spec: ToolSpec {
            name: READ_FILE_NAME
                .parse()
                .expect("the frozen read_file ToolName is valid"),
            description: Arc::from(READ_FILE_DESCRIPTION),
            input_schema: READ_FILE_SCHEMA
                .parse()
                .expect("the frozen read_file schema is valid"),
        },
        // One bounded regular-file read per call; the definition does not impose Serial
        // execution semantics on unrelated operations in the composed production ToolSet.
        mode: ToolExecutionMode::Parallel,
    }
}

/// The outer ToolSet sandbox contract of the read_file builtin: available exactly for
/// `FilesystemRead`.  The composed production ToolSet keeps this exact contract, so the
/// read_file route's Execute plan is admitted exactly once against the same ceiling.
pub(super) fn sandbox() -> ToolSandboxContract {
    ToolSandboxContract::available([ToolCapabilityClass::FilesystemRead])
}

/// Builds the exact immutable production `read_file` ToolSet: one definition, one matching
/// spec, the builtin planner pinned to the exact captured Workspace tool context and task
/// context, and the available sandbox contract enforcing exactly `FilesystemRead`.
pub(super) fn build_tool_set(
    workspace: WorkspaceToolContext,
    task_context: RuntimeTaskContext,
) -> Arc<ToolSet> {
    let definition = definition();
    let specs: Arc<[ToolSpec]> = Arc::from([definition.spec.clone()]);
    let definitions: Arc<[ToolDefinition]> = Arc::from([definition]);
    let planner: Arc<super::ToolPlanner> =
        Arc::new(move |request| plan(&workspace, &task_context, request));
    Arc::new(ToolSet {
        inner: Arc::new(ToolSetInner {
            definitions,
            specs,
            planner: Some(planner),
            sandbox: sandbox(),
        }),
    })
}

/// The synchronous pre-start plan for one exact `read_file` call: a valid, authorized call
/// plans the Execute shape carrying exactly `FilesystemRead` and a move-only start factory;
/// every parse/semantic failure plans the frozen `PreExecution + Failed` result; every
/// authorization failure plans the frozen `PreExecution + Denied` result before any start
/// factory exists.  `pub(super)` because the composed production ToolSet routes exactly this
/// frozen planner.
pub(super) fn plan(
    workspace: &WorkspaceToolContext,
    task_context: &RuntimeTaskContext,
    request: ToolExecutionRequest,
) -> ToolExecutionPlan {
    let arguments = match parse_arguments(request.call().arguments()) {
        Ok(arguments) => arguments,
        Err(()) => return invalid_arguments_plan(),
    };
    // Authorize the exact cwd-relative path synchronously, before any start factory exists.
    // Every WorkspaceAccessError (no grant, outside the cwd's root, the root path itself,
    // or an unavailable basis) collapses to the one frozen Denied text.
    let target = match workspace.access().authorize_read(&arguments.path) {
        Ok(target) => target,
        Err(_) => return denied_plan(),
    };
    let task_context = task_context.clone();
    ToolExecutionPlan::Execute {
        permissions: ToolPermissionSet::new([ToolCapabilityClass::FilesystemRead]),
        start: ToolExecutionStart::new(move |observer| {
            Box::pin(execute_read(target, task_context, observer))
        }),
    }
}

fn invalid_arguments_plan() -> ToolExecutionPlan {
    ToolExecutionPlan::PreExecution(pre_execution_failed(INVALID_ARGUMENTS_TEXT))
}

fn denied_plan() -> ToolExecutionPlan {
    ToolExecutionPlan::PreExecution(ToolExecutionResult::PreExecution {
        disposition: ToolResultDisposition::Denied,
        content: ToolResultContent::from_text_parts(vec![FILE_ACCESS_DENIED_TEXT.to_owned()])
            .expect("the frozen denied text is a valid bounded part"),
    })
}

fn pre_execution_failed(text: &str) -> ToolExecutionResult {
    ToolExecutionResult::PreExecution {
        disposition: ToolResultDisposition::Failed,
        content: ToolResultContent::from_text_parts(vec![text.to_owned()])
            .expect("the frozen failure texts are valid bounded parts"),
    }
}

/// The strict private serde mirror of the closed arguments object: unknown fields are
/// rejected, and the semantic `WorkspaceRelativePath` constructor stays the path authority.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArguments {
    path: WorkspaceRelativePath,
}

fn parse_arguments(arguments: &BoundedJsonObject) -> Result<ReadFileArguments, ()> {
    serde_json::from_str(arguments.canonical_json()).map_err(|_| ())
}

/// The cooperative-cancellation executor for one started read.
///
/// Once start has happened (this executor exists), a cancellation that already won (or
/// races the scheduling) before any blocking job is created wins via a biased select: the
/// scheduling branch completes synchronously once polled, so a cancellation observed first
/// means no blocking job is ever created and there is nothing to clean up — the truthful
/// settlement is the exact frozen `Completed + Cancelled` text `file read was cancelled`
/// because zero I/O is proven.  The signal-before-start case is owned by the ToolSet's
/// gate slot, which settles its own PreExecution Cancelled result before this executor is
/// ever constructed.
///
/// Once the tracked job is scheduled it is never dropped or detached: a cancellation
/// arriving while the read runs (even while the nonblocking capability open is
/// registered) keeps awaiting the same tracked job to its bounded settlement and
/// preserves the truthful result, so a known success/failure is never rewritten.  Only a
/// `RuntimeTaskError` settles `Abandoned { RuntimeFailure }`.
async fn execute_read(
    target: AuthorizedWorkspaceReadPath,
    task_context: RuntimeTaskContext,
    observer: ToolCancellationObserver,
) -> ToolExecutionResult {
    let job = tokio::select! {
        biased;
        _ = observer.cancelled() => {
            return ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Cancelled,
                content: ToolResultContent::from_text_parts(vec![FILE_CANCELLED_TEXT.to_owned()])
                    .expect("the frozen cancelled text is a valid bounded part"),
            };
        }
        scheduled = schedule_read_job(target, task_context) => scheduled,
    };
    let outcome = tokio::select! {
        biased;
        _ = observer.cancelled() => job.wait().await,
        settled = job.wait() => settled,
    };
    bind_read_outcome(outcome)
}

async fn schedule_read_job(
    target: AuthorizedWorkspaceReadPath,
    task_context: RuntimeTaskContext,
) -> TrackedBlockingJob<ReadFileOutcome> {
    task_context.spawn_blocking_tracked(move || read_regular_file(target))
}

/// The bounded outcome of one tracked blocking read: every file-processing decision
/// (capability open, regular-file rejection, bounded read, oversize, UTF-8, safe-Text)
/// happens inside the blocking closure so the executor only maps a settled outcome.
#[derive(Clone)]
enum ReadFileOutcome {
    /// A regular file's content: valid UTF-8 and safe Text, at most 65,536 bytes.
    Content(Arc<str>),
    /// Missing path, directory/non-regular entry, a symlink escape denied at the
    /// capability open, any open/read error, or valid UTF-8 the protocol cannot disclose as
    /// one safe Text part.
    Unreadable,
    NotUtf8,
    TooLarge,
}

/// The bounded blocking read: one capability-relative nonblocking open plus at most 65,537
/// bytes read. The open resolves through the authorized root's captured `cap_std::fs::Dir`,
/// so it can never leave the root; O_NONBLOCK means a FIFO or other special entry cannot
/// block the open waiting for a writer. The entry is rejected by its fstat metadata as
/// non-regular (or as already oversize, when the metadata size alone exceeds the bound)
/// before any read; the read stops the moment the content bound is exceeded, so oversize
/// detection never allocates beyond the fixed 65,537-byte budget. A WouldBlock or any
/// other open/read error reads as frozen unreadable.
fn read_regular_file(target: AuthorizedWorkspaceReadPath) -> ReadFileOutcome {
    let mut file = match target.open_nonblocking() {
        Ok(file) => file,
        // The nonblocking open never waits for a writer, so any error (including
        // WouldBlock) settles the frozen unreadable text without ever hanging.
        Err(_) => return ReadFileOutcome::Unreadable,
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return ReadFileOutcome::Unreadable,
    };
    if !metadata.is_file() {
        // A directory, FIFO, socket, device, or other special entry is not a read target.
        return ReadFileOutcome::Unreadable;
    }
    if metadata.len() > MAX_FILE_CONTENT_BYTES as u64 {
        // The metadata size alone already exceeds the one-part content bound: reject
        // without reading a byte. The bounded loop below stays the growth-proof backstop.
        return ReadFileOutcome::TooLarge;
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(MAX_READ_BYTES);
    let mut buffer = [0_u8; 4096];
    loop {
        let wanted = MAX_READ_BYTES - bytes.len();
        let take = wanted.min(buffer.len());
        let count = match file.read(&mut buffer[..take]) {
            Ok(count) => count,
            Err(_) => return ReadFileOutcome::Unreadable,
        };
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > MAX_FILE_CONTENT_BYTES {
            return ReadFileOutcome::TooLarge;
        }
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return ReadFileOutcome::NotUtf8,
    };
    if validate_safe_text(&text, MAX_FILE_CONTENT_BYTES, true).is_err() {
        // Valid UTF-8 the protocol cannot disclose as one safe Text part reads as
        // unreadable: the frozen texts do not distinguish it, and a Text part is the only
        // success shape this first slice produces.
        return ReadFileOutcome::Unreadable;
    }
    ReadFileOutcome::Content(text.into())
}

fn bind_read_outcome(outcome: Result<ReadFileOutcome, RuntimeTaskError>) -> ToolExecutionResult {
    match outcome {
        Ok(ReadFileOutcome::Content(text)) => {
            // The content already passed the owner's safe-Text and byte gates inside the
            // blocking closure, so a failure here is an invariant that fails closed.
            match ToolResultContent::from_text_parts(vec![text.as_ref().to_owned()]) {
                Ok(content) => ToolExecutionResult::Completed {
                    disposition: ToolResultDisposition::Succeeded,
                    content,
                },
                Err(_) => ToolExecutionResult::Abandoned {
                    reason: ToolAbandonReason::RuntimeFailure,
                },
            }
        }
        Ok(ReadFileOutcome::Unreadable) => completed_failed(FILE_UNREADABLE_TEXT),
        Ok(ReadFileOutcome::NotUtf8) => completed_failed(FILE_NOT_UTF8_TEXT),
        Ok(ReadFileOutcome::TooLarge) => completed_failed(FILE_TOO_LARGE_TEXT),
        Err(_) => ToolExecutionResult::Abandoned {
            reason: ToolAbandonReason::RuntimeFailure,
        },
    }
}

fn completed_failed(text: &str) -> ToolExecutionResult {
    ToolExecutionResult::Completed {
        disposition: ToolResultDisposition::Failed,
        content: ToolResultContent::from_text_parts(vec![text.to_owned()])
            .expect("the frozen failure texts are valid bounded parts"),
    }
}

impl fmt::Debug for ReadFileOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Content(_) => formatter.write_str("ReadFileOutcome::Content(..)"),
            Self::Unreadable => formatter.write_str("ReadFileOutcome::Unreadable"),
            Self::NotUtf8 => formatter.write_str("ReadFileOutcome::NotUtf8"),
            Self::TooLarge => formatter.write_str("ReadFileOutcome::TooLarge"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::runtime_task::RuntimeTaskContext;
    use crate::tools::{
        ToolAbandonReason, ToolCall, ToolCancellationHandle, ToolCapabilityClass,
        ToolExecutionOutcome, ToolExecutionPlan, ToolExecutionRequest, ToolExecutionStart,
        ToolOutcomeSource, ToolPermissionSet, ToolResultContent, ToolResultDisposition,
        ToolSandboxAdmissionError, ToolSandboxContract, ToolSet, ToolStartGate,
    };
    use crate::wire::{CanonicalFileUri, SessionId, WorkspaceRevision};
    use crate::workspace::{
        RequestedFilesystemAccess, Workspace, WorkspaceCwdSpec, WorkspaceDefinitionInput,
        WorkspacePathTarget, WorkspaceResolver, WorkspaceRootInput, WorkspaceSourcePolicy,
        WorkspaceToolContext, lower_workspace,
    };

    use super::*;

    const ITEM_ID: &str = "itm_00000000000000000000000000000001";
    const SESSION_ID: &str = "ses_11111111111111111111111111111111";

    static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-read-file-{label}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("the test temporary directory is creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Writes one file (creating parents) inside the test root.
    fn write(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the file parent is creatable");
        }
        std::fs::write(path, bytes).expect("the test file is writable");
    }

    fn request_for(arguments: &str) -> ToolExecutionRequest {
        ToolExecutionRequest::new(
            ITEM_ID.parse().unwrap(),
            ToolCall::new(
                "call_read".parse().unwrap(),
                READ_FILE_NAME.parse().unwrap(),
                arguments.parse().unwrap(),
                0,
            ),
        )
    }

    async fn initialized_context() -> RuntimeTaskContext {
        RuntimeTaskContext::new(tokio::runtime::Handle::current())
            .await
            .expect("the test runtime provides tracked task admission")
    }

    fn session_id() -> SessionId {
        SESSION_ID
            .parse()
            .expect("the test session id is canonical")
    }

    /// Lowers one real temp-dir Workspace (one primary root, cwd at `cwd_relative` inside
    /// it) through the production lowering path.
    fn workspace_spec(root: &Path, cwd_relative: &str) -> Workspace {
        let uri: CanonicalFileUri = format!("file://{}", root.display())
            .parse()
            .expect("the test root is a canonical native file URI");
        let root_input = WorkspaceRootInput::new(
            "primary".parse().expect("the test root key is canonical"),
            uri,
            RequestedFilesystemAccess::ReadOnly,
            WorkspaceSourcePolicy::new(false, false),
        );
        let input = WorkspaceDefinitionInput::new(
            root_input,
            Vec::new(),
            WorkspaceCwdSpec::new(
                "primary".parse().expect("the test root key is canonical"),
                cwd_relative
                    .parse()
                    .expect("the test cwd relative path is canonical"),
            ),
        )
        .expect("the test workspace definition is valid");
        lower_workspace(
            input,
            WorkspaceRevision::new(NonZeroU64::new(1).expect("the test revision is non-zero")),
            WorkspacePathTarget::Posix,
        )
        .expect("the test workspace lowers")
    }

    /// Resolves the workspace through the resolver into the published snapshot's tool
    /// context.
    async fn resolve_context(
        resolver: WorkspaceResolver,
        workspace: Workspace,
    ) -> WorkspaceToolContext {
        let candidate = resolver
            .resolve(session_id(), &workspace)
            .await
            .expect("the test workspace resolves");
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .expect("the test snapshot finishes");
        snapshot.tool_context()
    }

    /// A readable-grant tool context over one real temp-dir root, cwd at the root.
    async fn granted_context(
        task_context: &RuntimeTaskContext,
        root: &Path,
    ) -> WorkspaceToolContext {
        let resolver =
            WorkspaceResolver::new_with_source_grants_for_test(task_context.clone(), false, false);
        resolve_context(resolver, workspace_spec(root, "")).await
    }

    /// Plans one call and returns the frozen PreExecution result; panics on any other shape.
    fn plan_failure(set: &ToolSet, request: &ToolExecutionRequest) -> ToolExecutionResult {
        match set.plan(request) {
            Some(ToolExecutionPlan::PreExecution(result)) => result,
            _plan => panic!(
                "expected a PreExecution plan for arguments {}",
                request.call().arguments().canonical_json()
            ),
        }
    }

    /// Plans one call and panics unless it produces an Execute plan with exactly the
    /// `FilesystemRead` permission set.
    fn assert_plans_execute(set: &ToolSet, request: &ToolExecutionRequest) {
        match set.plan(request) {
            Some(ToolExecutionPlan::Execute { permissions, .. }) => {
                assert_eq!(
                    permissions,
                    ToolPermissionSet::new([ToolCapabilityClass::FilesystemRead])
                );
            }
            _plan => panic!(
                "expected an Execute plan for arguments {}",
                request.call().arguments().canonical_json()
            ),
        }
    }

    /// Plans one call and returns its move-only start factory; panics on any other shape.
    fn plan_start(set: &ToolSet, request: &ToolExecutionRequest) -> ToolExecutionStart {
        match set.plan(request) {
            Some(ToolExecutionPlan::Execute { start, .. }) => start,
            Some(ToolExecutionPlan::PreExecution(result)) => {
                panic!("expected an Execute plan, got {result:?}")
            }
            _plan => panic!("expected an Execute plan"),
        }
    }

    /// Drives one Execute plan through the exact proof path to its identity-bound outcome,
    /// isolated by one spawn like the Session Execution slot's consuming drive.
    async fn execute(set: Arc<ToolSet>, request: ToolExecutionRequest) -> ToolExecutionOutcome {
        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (_handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        tokio::spawn(run).await.expect("the started run settles")
    }

    fn outcome_content(outcome: &ToolExecutionOutcome) -> &ToolResultContent {
        match outcome {
            ToolExecutionOutcome::Completed { content, .. } => content,
            _ => panic!("expected a Completed outcome"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builtin_defines_exactly_read_file_with_the_frozen_description_and_closed_schema() {
        let temporary = TempDir::new("definition");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        let definitions = set.definitions();
        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];
        assert_eq!(definition.name().as_str(), READ_FILE_NAME);
        assert_eq!(definition.mode(), ToolExecutionMode::Parallel);
        // The frozen description is documented by this assertion: any edit to the disclosed
        // description must be reflected here deliberately.
        assert_eq!(definition.spec.description.as_ref(), READ_FILE_DESCRIPTION);

        // The prompt view discloses exactly the same single spec (name, description,
        // closed schema); planner and sandbox internals never enter the model context.
        let view = set.prompt_view();
        assert!(!view.is_empty());
        assert_eq!(view.specs().len(), 1);
        assert_eq!(view.specs()[0].name().as_str(), READ_FILE_NAME);
        assert_eq!(view.specs()[0].description(), READ_FILE_DESCRIPTION);

        // The disclosed schema is exactly the frozen schema: canonical bytes round-trip to
        // the same semantic value and stay within the bounded schema limit.
        let schema = view.specs()[0].input_schema();
        assert_eq!(
            schema.canonical_json(),
            READ_FILE_SCHEMA
                .parse::<crate::wire::BoundedJsonSchema>()
                .unwrap()
                .canonical_json()
        );
        assert!(
            schema.canonical_bytes().len()
                <= crate::wire::ProtocolLimits::v1_0()
                    .embedded_json
                    .schema
                    .max_encoded_bytes as usize
        );

        // The canonical disclosure is a closed object with exactly one required `path`
        // string capped at 4,096 bytes (the semantic `WorkspaceRelativePath` byte gate).
        let canonical: serde_json::Value =
            serde_json::from_str(schema.canonical_json()).expect("the schema is valid JSON");
        let root = canonical.as_object().expect("the schema root is an object");
        assert_eq!(
            root.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(root.get("required"), Some(&serde_json::json!(["path"])));
        let properties = root
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("the schema discloses properties");
        assert_eq!(
            properties.len(),
            1,
            "the first slice discloses exactly one property"
        );
        assert_eq!(
            properties.get("path"),
            Some(&serde_json::json!({"type": "string", "maxLength": 4096}))
        );
        assert_eq!(
            root.get("minLength"),
            None,
            "the empty cwd path is refused by Workspace authorization, not by the schema"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_tool_set_stays_empty_and_read_file_is_a_production_opt_in_builtin() {
        assert!(ToolSet::empty().definitions().is_empty());
        assert!(ToolSet::empty().prompt_view().is_empty());
        let temporary = TempDir::new("opt-in");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());
        assert_eq!(set.definitions().len(), 1);
        assert_eq!(set.prompt_view().specs().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_file_declares_exactly_filesystem_read_parallel_mode_and_an_exact_sandbox_contract()
     {
        let temporary = TempDir::new("sandbox");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        assert_eq!(set.definitions()[0].mode(), ToolExecutionMode::Parallel);

        // The Execute plan's final permission set is exactly FilesystemRead.
        let request = request_for(r#"{"path":"f.txt"}"#);
        match set.plan(&request) {
            Some(ToolExecutionPlan::Execute { permissions, .. }) => {
                assert_eq!(
                    permissions,
                    ToolPermissionSet::new([ToolCapabilityClass::FilesystemRead])
                );
                assert!(permissions.contains(ToolCapabilityClass::FilesystemRead));
                assert!(!permissions.contains(ToolCapabilityClass::FilesystemWrite));
                assert!(!permissions.contains(ToolCapabilityClass::Network));
                assert!(!permissions.contains(ToolCapabilityClass::Process));
            }
            _ => panic!("the valid call plans an Execute shape"),
        }

        // The captured sandbox contract is available exactly for FilesystemRead, so the
        // planner's own admission passes and every other class fails closed.
        let sandbox = &set.inner.sandbox;
        assert_eq!(
            *sandbox,
            ToolSandboxContract::available([ToolCapabilityClass::FilesystemRead])
        );
        assert!(
            sandbox
                .admit(ToolPermissionSet::new([
                    ToolCapabilityClass::FilesystemRead
                ]))
                .is_ok()
        );
        assert!(matches!(
            sandbox.admit(ToolPermissionSet::new([ToolCapabilityClass::Process])),
            Err(ToolSandboxAdmissionError::CapabilityGap { .. })
        ));
        assert!(
            set.plan(&request)
                .is_some_and(|plan| matches!(plan, ToolExecutionPlan::Execute { .. }))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn parse_and_semantic_failures_settle_the_frozen_preexecution_failed_result() {
        let temporary = TempDir::new("invalid");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());
        let invalid = vec![
            // Structural parse failures at every layer.
            "{}".to_owned(),
            r#"{"path":"f.txt","extra":1}"#.to_owned(),
            r#"{"Path":"f.txt"}"#.to_owned(),
            r#"{"path":null}"#.to_owned(),
            r#"{"path":1}"#.to_owned(),
            r#"{"path":true}"#.to_owned(),
            r#"{"path":[]}"#.to_owned(),
            r#"{"path":{}}"#.to_owned(),
            // The semantic WorkspaceRelativePath grammar: absolute, dot, empty, drive,
            // backslash, and control-char shapes all fail the path constructor.
            r#"{"path":"/etc/passwd"}"#.to_owned(),
            r#"{"path":"../x"}"#.to_owned(),
            r#"{"path":".."}"#.to_owned(),
            r#"{"path":"."}"#.to_owned(),
            r#"{"path":"a/../b"}"#.to_owned(),
            r#"{"path":"./a"}"#.to_owned(),
            r#"{"path":"a//b"}"#.to_owned(),
            r#"{"path":"a/"}"#.to_owned(),
            r#"{"path":"/a"}"#.to_owned(),
            r#"{"path":"C:/x"}"#.to_owned(),
            r#"{"path":"a:b"}"#.to_owned(),
            r#"{"path":"a\\b"}"#.to_owned(),
            r#"{"path":"a\u0001b"}"#.to_owned(),
            // The 4,096-byte and 256-segment semantic gates.
            format!(r#"{{"path":"{}"}}"#, "x".repeat(4_097)),
            format!(r#"{{"path":"{}"}}"#, "a/".repeat(256) + "a"),
        ];

        for (index, arguments) in invalid.iter().enumerate() {
            let request = request_for(arguments);
            let result = plan_failure(&set, &request);
            assert_eq!(
                result,
                ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Failed,
                    content: ToolResultContent::from_text_parts(vec![
                        INVALID_ARGUMENTS_TEXT.to_owned()
                    ])
                    .unwrap(),
                },
                "arguments #{index} {arguments:?} must settle the frozen failed pre-execution result"
            );
        }

        // The exact 4,096-byte path and 256-segment path stay valid (the semantic
        // constructor is the authority): both authorize (all-normal components inside the
        // cwd's root) and plan Execute, never invalid arguments.
        assert_plans_execute(
            &set,
            &request_for(&format!(r#"{{"path":"{}"}}"#, "x".repeat(4_096))),
        );
        assert_plans_execute(
            &set,
            &request_for(&format!(r#"{{"path":"{}"}}"#, "a/".repeat(255) + "a")),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authorization_failures_settle_the_frozen_preexecution_denied_result() {
        let temporary = TempDir::new("denied");
        write(temporary.path(), "notes.txt", b"secret body");

        // No readable grant: the production default authority (fail-closed) resolves the
        // root but grants no filesystem access, so every read path is denied.
        let task_context = initialized_context().await;
        let no_grant_resolver = WorkspaceResolver::new(task_context.clone());
        let no_grant_workspace =
            resolve_context(no_grant_resolver, workspace_spec(temporary.path(), "")).await;
        let no_grant_set = build_tool_set(no_grant_workspace, task_context.clone());
        for arguments in [r#"{"path":"notes.txt"}"#, r#"{"path":"anything.txt"}"#] {
            let request = request_for(arguments);
            let result = plan_failure(&no_grant_set, &request);
            assert_eq!(
                result,
                ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Denied,
                    content: ToolResultContent::from_text_parts(vec![
                        FILE_ACCESS_DENIED_TEXT.to_owned()
                    ])
                    .unwrap(),
                },
                "{arguments} must settle the frozen denied pre-execution result without a grant"
            );
        }

        // With a readable grant, the root path itself is refused by the Workspace
        // authorization (the cwd is a directory, never a read target) and collapses to the
        // same frozen denied text.
        let granted = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(granted, task_context.clone());
        let root_request = request_for(r#"{"path":""}"#);
        let result = plan_failure(&set, &root_request);
        assert_eq!(
            result,
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec![
                    FILE_ACCESS_DENIED_TEXT.to_owned()
                ])
                .unwrap(),
            },
            "the root path must settle the frozen denied pre-execution result"
        );

        // A denied path never produces a start factory: the exact request's gate still
        // accepts its single reservation and start exactly like a never-touched gate.
        let gate = ToolStartGate::new(root_request.clone());
        assert!(gate.reserve(&root_request).unwrap().start().is_ok());

        // The same file is readable through the granted set: denial is exact per path.
        assert_plans_execute(&set, &request_for(r#"{"path":"notes.txt"}"#));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_authorized_plan_executes_and_reads_exactly_one_text_part() {
        let temporary = TempDir::new("success");
        write(temporary.path(), "src/lib.rs", b"fn main() {}\n");
        write(temporary.path(), "notes.md", b"# Notes\n\nbody");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        let outcome = execute(Arc::clone(&set), request_for(r#"{"path":"src/lib.rs"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
            } if item_id == ITEM_ID.parse().unwrap()
                && tool_call_id == "call_read".parse().unwrap()
                && content.parts().len() == 1
                && content.parts()[0].as_text() == "fn main() {}\n"
        ));

        // A second path through the same set reads its own exact content.
        let second = execute(set, request_for(r#"{"path":"notes.md"}"#)).await;
        assert_eq!(
            outcome_content(&second).parts()[0].as_text(),
            "# Notes\n\nbody"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_exact_65536_byte_boundary_succeeds_and_65537_is_too_large() {
        let temporary = TempDir::new("boundary");
        let boundary = "x".repeat(MAX_FILE_CONTENT_BYTES);
        assert_eq!(boundary.len(), 65_536);
        let multibyte = "é".repeat(32_768);
        assert_eq!(multibyte.len(), 65_536);
        let oversized = "x".repeat(MAX_READ_BYTES);
        write(temporary.path(), "boundary.txt", boundary.as_bytes());
        write(temporary.path(), "multibyte.txt", multibyte.as_bytes());
        write(temporary.path(), "oversized.txt", oversized.as_bytes());
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        // Exactly 65,536 bytes (ASCII and multi-byte UTF-8) succeeds as one Text part.
        for (path, expected) in [
            ("boundary.txt", boundary.as_str()),
            ("multibyte.txt", multibyte.as_str()),
        ] {
            let outcome = execute(
                Arc::clone(&set),
                request_for(&format!(r#"{{"path":"{path}"}}"#)),
            )
            .await;
            assert!(matches!(
                outcome,
                ToolExecutionOutcome::Completed {
                    source: ToolOutcomeSource::Executed,
                    disposition: ToolResultDisposition::Succeeded,
                    ref content,
                    ..
                } if content.parts().len() == 1
                    && content.parts()[0].as_text() == expected
            ));
        }

        // 65,537 bytes is detected without reading further and settles the frozen text.
        let outcome = execute(set, request_for(r#"{"path":"oversized.txt"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_TOO_LARGE_TEXT
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_utf8_and_unsafe_text_settle_the_frozen_failed_texts() {
        let temporary = TempDir::new("utf8");
        let invalid_utf8: Vec<u8> = vec![0xc3, 0x28, 0xff, 0xfe, 0x00];
        let unsafe_text = "control \u{0001} byte".as_bytes().to_vec();
        write(temporary.path(), "bad.txt", &invalid_utf8);
        write(temporary.path(), "unsafe.txt", &unsafe_text);
        write(temporary.path(), "empty.txt", b"");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        let outcome = execute(Arc::clone(&set), request_for(r#"{"path":"bad.txt"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_NOT_UTF8_TEXT
        ));

        // Valid UTF-8 the protocol cannot disclose as one safe Text part reads as
        // unreadable: exactly one Text part is the only success shape of this slice.
        let outcome = execute(Arc::clone(&set), request_for(r#"{"path":"unsafe.txt"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_UNREADABLE_TEXT
        ));

        // An empty regular file succeeds as exactly one empty Text part.
        let outcome = execute(set, request_for(r#"{"path":"empty.txt"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text().is_empty()
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_files_and_directories_settle_failed_unreadable() {
        let temporary = TempDir::new("missing");
        write(temporary.path(), "real.txt", b"x");
        std::fs::create_dir_all(temporary.path().join("docs"))
            .expect("the test directory is creatable");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        // Missing paths are authorized (they are normal cwd-relative targets inside the
        // root) and fail at the capability open, so they settle Completed + Failed, never
        // a denial.  A directory opens but is rejected as non-regular.
        for arguments in [
            r#"{"path":"missing.txt"}"#,
            r#"{"path":"deep/missing.txt"}"#,
            r#"{"path":"docs"}"#,
        ] {
            let outcome = execute(Arc::clone(&set), request_for(arguments)).await;
            assert!(
                matches!(
                    outcome,
                    ToolExecutionOutcome::Completed {
                        source: ToolOutcomeSource::Executed,
                        disposition: ToolResultDisposition::Failed,
                        ref content,
                        ..
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == FILE_UNREADABLE_TEXT
                ),
                "{arguments} must settle the frozen unreadable result"
            );
        }
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn outside_root_symlink_escape_is_denied_at_the_capability_open() {
        let temporary = TempDir::new("escape");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::create_dir_all(&outside).expect("the outside directory is creatable");
        std::fs::write(outside.join("secret.txt"), b"outside secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let task_context = initialized_context().await;
        let resolver =
            WorkspaceResolver::new_with_source_grants_for_test(task_context.clone(), false, false);
        let workspace_context = resolve_context(resolver, workspace_spec(&root, "")).await;
        let set = build_tool_set(workspace_context, task_context.clone());

        // The escape path is a normal-looking cwd-relative target, so authorization grants
        // it; the capability open then denies the escape (the open resolves through the
        // captured root Dir and cannot leave it), settling the frozen unreadable text.
        let outcome = execute(set, request_for(r#"{"path":"escape/secret.txt"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_UNREADABLE_TEXT
        ));

        // The secret itself is untouched and still reachable only through ambient std
        // access: the capability containment did the denying, never my read path.
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"outside secret"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_scheduling_settles_the_exact_cancelled_text() {
        let temporary = TempDir::new("cancel-before");
        write(temporary.path(), "real.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());
        // A missing path: any scheduled read would settle Completed + Failed unreadable, so
        // the Cancelled outcome below proves no blocking job was ever created.
        let request = request_for(r#"{"path":"missing.txt"}"#);

        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        // The operation's own pair is already cancelled before the executor future is ever
        // constructed: the biased select must win after start with the exact frozen
        // Cancelled text, without scheduling any blocking work.  The owner binds it as
        // Executed because start already happened.
        let (handle, observer) = ToolCancellationHandle::new();
        handle.cancel();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        let outcome = tokio::spawn(run).await.expect("the run settles");

        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Cancelled,
                ref content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_CANCELLED_TEXT
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_the_job_is_scheduled_keeps_awaiting_it_and_preserves_the_result() {
        // A known regular fixture file whose truthful result is a one-part success: once
        // the tracked blocking job exists, a later cancellation must keep awaiting the
        // exact same job and preserve that result, never rewrite it to Cancelled.
        let temporary = TempDir::new("cancel-after-schedule");
        write(temporary.path(), "real.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());
        let request = request_for(r#"{"path":"real.txt"}"#);

        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");

        // One deterministic poll drives the executor through the first biased select, so
        // the tracked blocking job provably exists and the executor is awaiting its
        // settlement before the cancellation is signalled — no sleeps or blind polling.
        let mut run = std::pin::pin!(run);
        let first = std::future::poll_fn(|cx| std::task::Poll::Ready(run.as_mut().poll(cx))).await;
        assert!(
            first.is_pending(),
            "the executor awaits the tracked job's settlement"
        );
        handle.cancel();

        let outcome = std::future::poll_fn(|cx| run.as_mut().poll(cx)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == "x"
        ));
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_fifo_is_rejected_promptly_without_any_writer() {
        // A FIFO is the old hang: an ordinary blocking read-open waits forever for a
        // writer. The nonblocking capability open returns immediately (no writer exists),
        // and the fstat regular-file check rejects the entry, settling the frozen
        // unreadable text. A direct awaited execution is enough: the old implementation
        // would hang it forever, so the prompt return itself proves the fix.
        let temporary = TempDir::new("fifo");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        let fifo = root.join("gate.fifo");
        let fifo_make = fifo.clone();
        let made = std::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&fifo_make)
            .status()
            .expect("the mkfifo command runs");
        assert!(made.success(), "the test FIFO is creatable");

        let task_context = initialized_context().await;
        let resolver =
            WorkspaceResolver::new_with_source_grants_for_test(task_context.clone(), false, false);
        let workspace_context = resolve_context(resolver, workspace_spec(&root, "")).await;
        let set = build_tool_set(workspace_context, task_context.clone());

        let outcome = execute(set, request_for(r#"{"path":"gate.fifo"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_UNREADABLE_TEXT
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracked_runtime_failure_after_start_settles_abandoned_runtime_failure() {
        let temporary = TempDir::new("runtime-failure");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        // The one-shot seam: the next admitted blocking job joins as an immediate worker
        // cancellation without ever running its operation closure, so the I/O outcome is
        // unknown and must settle Abandoned RuntimeFailure.  Armed after the workspace
        // resolution (which itself admits a tracked blocking job), so it targets the read.
        task_context.inject_next_blocking_job_join_failure();
        let set = build_tool_set(workspace, task_context.clone());

        let outcome = execute(set, request_for(r#"{"path":"f.txt"}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            } if item_id == ITEM_ID.parse().unwrap()
                && tool_call_id == "call_read".parse().unwrap()
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_tool_name_plans_none_and_reads_never_construct_a_start_factory() {
        let temporary = TempDir::new("unknown");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());
        let unknown = ToolExecutionRequest::new(
            ITEM_ID.parse().unwrap(),
            ToolCall::new(
                "call_other".parse().unwrap(),
                "other_tool".parse().unwrap(),
                r#"{"path":"f.txt"}"#.parse().unwrap(),
                0,
            ),
        );
        assert!(set.plan(&unknown).is_none());
        // A valid read_file request still plans Execute through the same set.
        assert_plans_execute(&set, &request_for(r#"{"path":"f.txt"}"#));
    }
}
