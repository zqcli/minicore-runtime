//! The production `write_file` builtin: one closed, default-off, Runtime-owned Tool that
//! writes exactly one UTF-8 text file relative to the Workspace cwd, replacing its full
//! contents or creating the file when its parent directory exists.
//!
//! The builtin is immutable after construction and travels through the existing residency
//! ToolSet capture path.  This slice is deliberately narrow: two required string
//! arguments (`path` capped at 4,096 bytes, `content` capped at 16,384 bytes), no
//! append, no atomic replace, no fsync, no mkdir, no multi-file mutation, no absolute
//! paths.  The builtin itself stays narrow: production composition belongs to the closed
//! `ProductionToolConfig`, which fixes exactly the four frozen builtins in one order
//! (`ask_user` → `read_file` → `list_directory` → `write_file`) — there is no generic
//! registry and no dynamic composition.
//!
//! A call plans synchronously to one of exactly three shapes:
//!
//! - `ToolExecutionPlan::FileMutation` for a valid, authorized call: the exact
//!   cwd-relative `WorkspaceRelativePath` is parsed by the strict serde mirror (the
//!   semantic `WorkspaceRelativePath` grammar is the authority, never the schema
//!   guidance), the exact `content` bytes pass the safe-text gate (at most 16,384 bytes,
//!   empty allowed, no newline normalization — a carriage return is unsafe text and is
//!   rejected, never silently rewritten), and the path is authorized synchronously
//!   through the Workspace tool context *before* any preparation factory exists.  The
//!   plan carries exactly `ToolCapabilityClass::FilesystemWrite`, the move-only
//!   preparation factory, and the exact Session-local mutation queue captured at
//!   admission; the ToolSet's sandbox contract is available for `FilesystemWrite`, so
//!   admission revalidates the same single class.  The planner performs no I/O and no
//!   await: preparation happens later, exactly once, in the round owner's mutation
//!   preparation phase.
//! - `ToolExecutionPlan::PreExecution` with the frozen `Failed` text
//!   `tool arguments are invalid` for any parse or semantic argument failure.
//! - `ToolExecutionPlan::PreExecution` with the frozen `Denied` text
//!   `workspace file write access is denied` when
//!   `WorkspaceAccessView::authorize_write` refuses the exact cwd-relative path: no
//!   `ReadWrite` grant, a path outside the cwd's containing root, or an empty/root path.
//!   Every authorization error collapses to this one frozen, bounded, non-secret text;
//!   the denial happens synchronously before constructing any preparation.
//!
//! The preparation factory schedules exactly one owner-tracked blocking job that
//! prepares the authorized target without any mutation (capability opens and metadata
//! proofs only — no create, truncate or write) and hands the move-only
//! [`PreparedWorkspaceWriteTarget`] through a private single-consumer slot; the job
//! settlement carries only a cloneable redacted status and the opaque
//! [`WorkspaceFileMutationKey`], so no file, directory, or path handle is ever cloned or
//! exposed.  An ordinary Workspace/OS preparation failure settles
//! `PreparedToolExecution::Unstarted` with the frozen `PreExecution + Failed` text
//! `file could not be written`; a `RuntimeTaskError`, an operation panic/join
//! uncertainty, or a poisoned handoff slot is an invariant that settles the explicit
//! `Abandoned { RuntimeFailure }` shape instead.
//!
//! The started executor never uses an ambient path or `std::fs::write`: before any
//! blocking write job exists, a cancellation that already won (or races this scheduling
//! point) wins via the biased select and settles the exact frozen `Completed + Cancelled`
//! text `file write was cancelled` — zero mutation is proven.  Otherwise the move-only
//! prepared target is consumed exactly once from the handoff into one tracked write job
//! that calls `PreparedWorkspaceWriteTarget::write(content.as_bytes())` — the exact
//! captured `Dir` capability resolves the target, the existing shape rewrites the exact
//! opened file from offset zero, and the create shape opens the final name through the
//! retained parent with a final-component no-follow option, so a symlink escape or a
//! late symlink/directory entry fails closed.  Success settles exactly one Text part
//! `file written`; a Workspace write error after scheduling settles exactly one Text
//! part `file could not be written` (a partial/truncated file is possible and is never
//! retried).  The write job, once scheduled, is never dropped or detached — a
//! cancellation arriving while the write runs keeps awaiting the same job to its
//! settlement and preserves the truthful result, so a known success/failure is never
//! rewritten.  Only a `RuntimeTaskError` (owner closing, worker unavailable, operation
//! panic) or a missing/poisoned handoff settles `Abandoned { RuntimeFailure }`; the
//! signal-before-start case is owned by the ToolSet's gate slot, which settles its own
//! PreExecution Cancelled result before this executor is ever constructed.

use std::fmt;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError};
use crate::wire::lexical::validate_safe_text;
use crate::wire::{BoundedJsonObject, WorkspaceRelativePath};
use crate::workspace::{
    AuthorizedWorkspaceWritePath, PreparedWorkspaceWriteTarget, WorkspaceFileMutationKey,
    WorkspaceToolContext,
};

use super::{
    PreparedToolExecution, SessionFileMutationQueue, ToolAbandonReason, ToolCancellationObserver,
    ToolCapabilityClass, ToolDefinition, ToolExecutionMode, ToolExecutionPlan,
    ToolExecutionPreparation, ToolExecutionRequest, ToolExecutionResult, ToolExecutionStart,
    ToolPermissionSet, ToolResultContent, ToolResultDisposition, ToolSandboxContract, ToolSet,
    ToolSetInner, ToolSpec,
};

/// The exact production builtin ToolName.  `pub(super)` because the composed production
/// ToolSet routes exactly this frozen name.
pub(super) const WRITE_FILE_NAME: &str = "write_file";

/// The exact production description disclosed for the builtin: writing one UTF-8 text
/// file relative to the Workspace working directory.  Frozen; asserted verbatim in
/// module tests.
const WRITE_FILE_DESCRIPTION: &str = "Write UTF-8 text to one file relative to the workspace working directory, replacing its full contents or creating the file when its parent directory exists.";

/// The exact frozen PreExecution Failed text for every parse or semantic argument
/// failure.
const INVALID_ARGUMENTS_TEXT: &str = "tool arguments are invalid";

/// The exact frozen PreExecution Denied text for every authorization failure (no
/// `ReadWrite` grant, a path outside the cwd's containing root, or an empty/root path).
const FILE_ACCESS_DENIED_TEXT: &str = "workspace file write access is denied";

/// The exact frozen Completed Cancelled text for a cancellation that won after start but
/// before any blocking write job was created: zero mutation is proven, so the truthful
/// disposition is Cancelled with this one Text part, never OutcomeUnknown.
const FILE_CANCELLED_TEXT: &str = "file write was cancelled";

/// The exact frozen Completed/PreExecution Failed text for an ordinary preparation
/// failure and for a Workspace write error after scheduling (the target may already be
/// truncated or partially written; there is no retry).
const FILE_UNWRITABLE_TEXT: &str = "file could not be written";

/// The exact frozen Completed Succeeded text for one full replacement.
const FILE_WRITTEN_TEXT: &str = "file written";

/// The closed content bound: at most 16,384 bytes of safe UTF-8 text, empty allowed.
/// This is the exact `BoundedJsonObject` decoded-string ceiling, so a larger content
/// string can never be a valid tool-call argument in the first place; the safe-text gate
/// stays as the planner's own defense in depth.
const MAX_CONTENT_BYTES: usize = 16_384;

/// The closed input schema disclosed for the builtin.  Structural guidance only: the
/// `WorkspaceRelativePath` grammar (canonical cwd-relative path, no absolute/dot
/// segments, at most 4,096 bytes) is enforced by the semantic constructor, never by this
/// schema, and the 16,384-byte content ceiling is enforced by the wire decode and the
/// planner's safe-text gate.
const WRITE_FILE_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "maxLength": 4096
    },
    "content": {
      "type": "string",
      "maxLength": 16384
    }
  },
  "required": ["path", "content"],
  "additionalProperties": false
}"#;

/// The exact frozen production definition/spec pair: the single source shared by the
/// standalone builtin ToolSet and the composed production ToolSet, so the disclosed
/// definition and spec are byte-identical in both selections.
pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        spec: ToolSpec {
            name: WRITE_FILE_NAME
                .parse()
                .expect("the frozen write_file ToolName is valid"),
            description: Arc::from(WRITE_FILE_DESCRIPTION),
            input_schema: WRITE_FILE_SCHEMA
                .parse()
                .expect("the frozen write_file schema is valid"),
        },
        // Same-physical-target sibling calls serialize through the Session-local mutation
        // queue while different targets stay independent, so the definition does not
        // impose Serial execution semantics on unrelated operations in the composed
        // production ToolSet.
        mode: ToolExecutionMode::Parallel,
    }
}

/// The outer ToolSet sandbox contract of the write_file builtin: available exactly for
/// `FilesystemWrite`.  The composed production ToolSet widens this to the exact union
/// required by the enabled workspace routes, so the write route's FileMutation plan is
/// admitted exactly once against that ceiling.
pub(super) fn sandbox() -> ToolSandboxContract {
    ToolSandboxContract::available([ToolCapabilityClass::FilesystemWrite])
}

/// Builds the exact immutable production `write_file` ToolSet: one definition, one
/// matching spec, the builtin planner pinned to the exact captured Workspace tool
/// context, task context, and Session-local mutation queue, and the available sandbox
/// contract enforcing exactly `FilesystemWrite`.
pub(super) fn build_tool_set(
    workspace: WorkspaceToolContext,
    task_context: RuntimeTaskContext,
    queue: Arc<SessionFileMutationQueue>,
) -> Arc<ToolSet> {
    let definition = definition();
    let specs: Arc<[ToolSpec]> = Arc::from([definition.spec.clone()]);
    let definitions: Arc<[ToolDefinition]> = Arc::from([definition]);
    let planner: Arc<super::ToolPlanner> =
        Arc::new(move |request| plan(&workspace, &task_context, &queue, request));
    Arc::new(ToolSet {
        inner: Arc::new(ToolSetInner {
            definitions,
            specs,
            planner: Some(planner),
            sandbox: sandbox(),
        }),
    })
}

/// The synchronous pre-start plan for one exact `write_file` call: a valid, authorized
/// call plans the FileMutation shape carrying exactly `FilesystemWrite`, the move-only
/// preparation factory (which performs no I/O and no await at plan time), and the exact
/// Session queue captured at admission; every parse/semantic failure plans the frozen
/// `PreExecution + Failed` result; every authorization failure plans the frozen
/// `PreExecution + Denied` result before any preparation factory exists.  `pub(super)`
/// because the composed production ToolSet routes exactly this frozen planner.
pub(super) fn plan(
    workspace: &WorkspaceToolContext,
    task_context: &RuntimeTaskContext,
    queue: &Arc<SessionFileMutationQueue>,
    request: ToolExecutionRequest,
) -> ToolExecutionPlan {
    let arguments = match parse_arguments(request.call().arguments()) {
        Ok(arguments) => arguments,
        Err(()) => return invalid_arguments_plan(),
    };
    // The exact content bytes are validated as safe UTF-8 text (at most 16,384 bytes,
    // empty allowed) with no newline normalization: a carriage return is unsafe text and
    // is rejected here, never silently rewritten to a newline.
    if validate_safe_text(&arguments.content, MAX_CONTENT_BYTES, true).is_err() {
        return invalid_arguments_plan();
    }
    // Authorize the exact cwd-relative path synchronously, before any preparation
    // factory exists.  Every WorkspaceWriteError (no exact ReadWrite grant, outside the
    // cwd's root, an empty/root path, or an unavailable basis) collapses to the one
    // frozen Denied text.
    let target = match workspace.access().authorize_write(&arguments.path) {
        Ok(target) => target,
        Err(_) => return denied_plan(),
    };
    let task_context = task_context.clone();
    let queue = Arc::clone(queue);
    ToolExecutionPlan::FileMutation {
        permissions: ToolPermissionSet::new([ToolCapabilityClass::FilesystemWrite]),
        prepare: ToolExecutionPreparation::new(move || {
            Box::pin(prepare_write(target, arguments.content, task_context))
        }),
        queue,
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
/// rejected, and the semantic `WorkspaceRelativePath` constructor stays the path
/// authority while `content` carries the exact decoded string bytes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArguments {
    path: WorkspaceRelativePath,
    content: String,
}

fn parse_arguments(arguments: &BoundedJsonObject) -> Result<WriteFileArguments, ()> {
    serde_json::from_str(arguments.canonical_json()).map_err(|_| ())
}

/// The private single-consumer handoff for the move-only prepared write target: the
/// preparation job stores it, and the started write consumes it exactly once.  A
/// poisoned lock or an already-consumed/missing target is an invariant that fails
/// closed to `Abandoned { RuntimeFailure }`.
type PreparedTargetHandoff = Arc<Mutex<Option<PreparedWorkspaceWriteTarget>>>;

/// Takes the move-only prepared target out of the handoff exactly once.  Only this short
/// critical section touches the slot: no arbitrary work ever runs while the handoff
/// mutex is held, and a poisoned lock reads as a missing target (RuntimeFailure).
fn take_prepared_target(handoff: &PreparedTargetHandoff) -> Option<PreparedWorkspaceWriteTarget> {
    handoff.lock().ok().and_then(|mut slot| slot.take())
}

/// The cloneable, redacted settlement status of one tracked preparation job: only the
/// opaque key and the frozen marks travel in the job settlement — the move-only
/// prepared target itself is handed off through the private single-consumer slot and is
/// never cloned or exposed.
#[derive(Clone)]
enum PreparationStatus {
    /// The target prepared and its carrier is stored in the handoff; the key is the
    /// exact opaque identity the Session queue serializes on.
    Ready { key: WorkspaceFileMutationKey },
    /// An ordinary Workspace/OS preparation failure (missing parent, non-regular entry,
    /// open failure): settles the frozen `PreExecution + Failed` text.
    Failed,
    /// The handoff slot was poisoned: the prepared target could not be stored, an
    /// invariant that settles `Abandoned { RuntimeFailure }`.
    Invariant,
}

/// The move-only preparation for one exact write: schedules exactly one owner-tracked
/// blocking job that prepares the authorized target (capability opens and metadata
/// proofs only — no create, truncate or write) and waits that same job to full
/// settlement.  A ready result carries the exact opaque key plus a start factory that
/// captures the single prepared-target handoff, the exact content, and the task context;
/// an ordinary preparation failure settles the frozen unstarted failure, while a
/// `RuntimeTaskError`, an operation panic/join uncertainty, or the handoff invariant
/// settles the explicit unstarted `Abandoned { RuntimeFailure }`.
async fn prepare_write(
    target: AuthorizedWorkspaceWritePath,
    content: String,
    task_context: RuntimeTaskContext,
) -> PreparedToolExecution {
    let handoff: PreparedTargetHandoff = Arc::new(Mutex::new(None));
    let handoff_in_job = Arc::clone(&handoff);
    let job = task_context.spawn_blocking_tracked(move || {
        let prepared = match target.prepare() {
            Ok(prepared) => prepared,
            Err(_) => return PreparationStatus::Failed,
        };
        let key = prepared.key();
        // The single short store; no arbitrary work while the handoff mutex is held.  A
        // poisoned lock is an invariant that fails closed to RuntimeFailure.
        let mut slot = match handoff_in_job.lock() {
            Ok(slot) => slot,
            Err(_) => return PreparationStatus::Invariant,
        };
        *slot = Some(prepared);
        PreparationStatus::Ready { key }
    });
    match job.wait().await {
        Ok(PreparationStatus::Ready { key }) => PreparedToolExecution::Ready {
            key,
            start: ToolExecutionStart::new(move |observer| {
                Box::pin(execute_write(handoff, content, task_context, observer))
            }),
        },
        Ok(PreparationStatus::Failed) => {
            PreparedToolExecution::Unstarted(pre_execution_failed(FILE_UNWRITABLE_TEXT))
        }
        Ok(PreparationStatus::Invariant) | Err(_) => abandoned_preparation(),
    }
}

fn abandoned_preparation() -> PreparedToolExecution {
    PreparedToolExecution::Unstarted(ToolExecutionResult::Abandoned {
        reason: ToolAbandonReason::RuntimeFailure,
    })
}

/// The bounded settled outcome of one tracked write: the full replacement succeeded, or
/// the Workspace write failed after scheduling (the target may already be truncated or
/// partially written — the failure is truthful and never retried).
#[derive(Clone)]
enum WriteOutcome {
    Succeeded,
    Failed,
}

/// The cooperative-cancellation executor for one started write.
///
/// Once start has happened (this executor exists), a cancellation that already won (or
/// races the scheduling) before any blocking write job is created wins via a biased
/// select: the proceed branch completes synchronously once polled, so a cancellation
/// observed first means no write job is ever created and there is nothing to clean up —
/// the truthful settlement is the exact frozen `Completed + Cancelled` text
/// `file write was cancelled` because zero mutation is proven.  The signal-before-start
/// case is owned by the ToolSet's gate slot, which settles its own PreExecution
/// Cancelled result before this executor is ever constructed.
///
/// Otherwise the move-only prepared target is consumed exactly once from the handoff
/// into one tracked write job; a missing/poisoned handoff is an invariant that fails
/// closed to the identity-bound `Abandoned { RuntimeFailure }`.  The write job, once
/// scheduled, is never dropped or detached: a cancellation arriving while the write runs
/// keeps awaiting the same tracked job to its settlement and preserves the truthful
/// result, so a known success/failure is never rewritten.  Only a `RuntimeTaskError`
/// settles `Abandoned { RuntimeFailure }`.
async fn execute_write(
    handoff: PreparedTargetHandoff,
    content: String,
    task_context: RuntimeTaskContext,
    observer: ToolCancellationObserver,
) -> ToolExecutionResult {
    tokio::select! {
        biased;
        _ = observer.cancelled() => {
            return ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Cancelled,
                content: ToolResultContent::from_text_parts(vec![FILE_CANCELLED_TEXT.to_owned()])
                    .expect("the frozen cancelled text is a valid bounded part"),
            };
        }
        _ = std::future::ready(()) => {}
    }
    let Some(mut target) = take_prepared_target(&handoff) else {
        return ToolExecutionResult::Abandoned {
            reason: ToolAbandonReason::RuntimeFailure,
        };
    };
    let job = task_context.spawn_blocking_tracked(move || match target.write(content.as_bytes()) {
        Ok(()) => WriteOutcome::Succeeded,
        Err(_) => WriteOutcome::Failed,
    });
    let outcome = tokio::select! {
        biased;
        _ = observer.cancelled() => job.wait().await,
        settled = job.wait() => settled,
    };
    bind_write_outcome(outcome)
}

fn bind_write_outcome(outcome: Result<WriteOutcome, RuntimeTaskError>) -> ToolExecutionResult {
    match outcome {
        Ok(WriteOutcome::Succeeded) => ToolExecutionResult::Completed {
            disposition: ToolResultDisposition::Succeeded,
            content: ToolResultContent::from_text_parts(vec![FILE_WRITTEN_TEXT.to_owned()])
                .expect("the frozen success text is a valid bounded part"),
        },
        Ok(WriteOutcome::Failed) => completed_failed(FILE_UNWRITABLE_TEXT),
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

impl fmt::Debug for PreparationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready { .. } => formatter.write_str("PreparationStatus::Ready(..)"),
            Self::Failed => formatter.write_str("PreparationStatus::Failed"),
            Self::Invariant => formatter.write_str("PreparationStatus::Invariant"),
        }
    }
}

impl fmt::Debug for WriteOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeeded => formatter.write_str("WriteOutcome::Succeeded"),
            Self::Failed => formatter.write_str("WriteOutcome::Failed"),
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
        PreparedToolExecution, SessionFileMutationQueue, ToolAbandonReason, ToolCall,
        ToolCancellationHandle, ToolCapabilityClass, ToolExecutionMode, ToolExecutionOutcome,
        ToolExecutionPlan, ToolExecutionRequest, ToolExecutionResult, ToolExecutionStart,
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
    const SESSION_ID: &str = "ses_44444444444444444444444444444444";

    static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-write-file-{label}-{}-{suffix}",
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
                "call_write".parse().unwrap(),
                WRITE_FILE_NAME.parse().unwrap(),
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

    /// Lowers one real temp-dir Workspace (one primary root with the given requested
    /// access, cwd at `cwd_relative` inside it) through the production lowering path.
    fn workspace_spec(
        root: &Path,
        cwd_relative: &str,
        access: RequestedFilesystemAccess,
    ) -> Workspace {
        let uri: CanonicalFileUri = format!("file://{}", root.display())
            .parse()
            .expect("the test root is a canonical native file URI");
        let root_input = WorkspaceRootInput::new(
            "primary".parse().expect("the test root key is canonical"),
            uri,
            access,
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

    /// A ReadWrite-grant tool context over one real temp-dir root, cwd at the root: the
    /// write opt-in authority ceiling intersected with a ReadWrite-requested root.
    async fn granted_context(
        task_context: &RuntimeTaskContext,
        root: &Path,
    ) -> WorkspaceToolContext {
        let (resolver, _control) = WorkspaceResolver::new_with_write_access(task_context.clone());
        resolve_context(
            resolver,
            workspace_spec(root, "", RequestedFilesystemAccess::ReadWrite),
        )
        .await
    }

    /// A ReadOnly-grant tool context: the read-only authority ceiling keeps a
    /// ReadWrite-requested root at ReadOnly, so the write route is denied.
    async fn read_only_context(
        task_context: &RuntimeTaskContext,
        root: &Path,
    ) -> WorkspaceToolContext {
        let (resolver, _control) = WorkspaceResolver::new_with_read_access(task_context.clone());
        resolve_context(
            resolver,
            workspace_spec(root, "", RequestedFilesystemAccess::ReadWrite),
        )
        .await
    }

    /// A no-grant tool context: the restricted default authority resolves the root but
    /// grants no filesystem access, so every write path is denied.
    async fn no_grant_context(
        task_context: &RuntimeTaskContext,
        root: &Path,
    ) -> WorkspaceToolContext {
        let resolver = WorkspaceResolver::new(task_context.clone());
        resolve_context(
            resolver,
            workspace_spec(root, "", RequestedFilesystemAccess::ReadWrite),
        )
        .await
    }

    /// Plans one call and returns the frozen PreExecution result; panics on any other
    /// shape.
    fn plan_failure(set: &ToolSet, request: &ToolExecutionRequest) -> ToolExecutionResult {
        match set.plan(request) {
            Some(ToolExecutionPlan::PreExecution(result)) => result,
            _plan => panic!(
                "expected a PreExecution plan for arguments {}",
                request.call().arguments().canonical_json()
            ),
        }
    }

    /// Plans one call and returns its move-only preparation factory; panics on any other
    /// shape.
    fn plan_preparation(set: &ToolSet, request: &ToolExecutionRequest) -> ToolExecutionPreparation {
        match set.plan(request) {
            Some(ToolExecutionPlan::FileMutation { prepare, .. }) => prepare,
            Some(ToolExecutionPlan::PreExecution(result)) => {
                panic!("expected a FileMutation plan, got {result:?}")
            }
            _plan => panic!("expected a FileMutation plan"),
        }
    }

    /// Awaits one preparation factory to full settlement and returns the ready start
    /// factory; panics on any unstarted settlement.
    async fn prepared_start(prepare: ToolExecutionPreparation) -> ToolExecutionStart {
        match prepare.prepare().await {
            PreparedToolExecution::Ready { start, .. } => start,
            PreparedToolExecution::Unstarted(result) => {
                panic!("expected a ready preparation, got {result:?}")
            }
        }
    }

    /// Runs one ready start factory through the exact proof path to its identity-bound
    /// outcome, isolated by one spawn like the Session Execution slot's consuming drive.
    /// `cancelled` pre-cancels the operation's own pair so the started executor observes
    /// it from its first poll.
    async fn run_started(
        set: Arc<ToolSet>,
        request: ToolExecutionRequest,
        start: ToolExecutionStart,
        cancelled: bool,
    ) -> ToolExecutionOutcome {
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (handle, observer) = ToolCancellationHandle::new();
        if cancelled {
            handle.cancel();
        }
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        tokio::spawn(run).await.expect("the started run settles")
    }

    /// Drives one FileMutation plan through preparation and the exact proof path to its
    /// identity-bound outcome.
    async fn execute(set: Arc<ToolSet>, request: ToolExecutionRequest) -> ToolExecutionOutcome {
        let start = prepared_start(plan_preparation(&set, &request)).await;
        run_started(set, request, start, false).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builtin_defines_exactly_write_file_with_the_frozen_description_and_closed_schema() {
        let temporary = TempDir::new("definition");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );

        let definitions = set.definitions();
        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];
        assert_eq!(definition.name().as_str(), WRITE_FILE_NAME);
        assert_eq!(definition.mode(), ToolExecutionMode::Parallel);
        // The frozen description is documented by this assertion: any edit to the
        // disclosed description must be reflected here deliberately.
        assert_eq!(definition.spec.description.as_ref(), WRITE_FILE_DESCRIPTION);

        // The prompt view discloses exactly the same single spec (name, description,
        // closed schema); planner and sandbox internals never enter the model context.
        let view = set.prompt_view();
        assert!(!view.is_empty());
        assert_eq!(view.specs().len(), 1);
        assert_eq!(view.specs()[0].name().as_str(), WRITE_FILE_NAME);
        assert_eq!(view.specs()[0].description(), WRITE_FILE_DESCRIPTION);

        // The disclosed schema is exactly the frozen schema: canonical bytes round-trip
        // to the same semantic value and stay within the bounded schema limit.
        let schema = view.specs()[0].input_schema();
        assert_eq!(
            schema.canonical_json(),
            WRITE_FILE_SCHEMA
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

        // The canonical disclosure is a closed object with exactly two required strings:
        // `path` capped at 4,096 bytes and `content` capped at 16,384 bytes.
        let canonical: serde_json::Value =
            serde_json::from_str(schema.canonical_json()).expect("the schema is valid JSON");
        let root = canonical.as_object().expect("the schema root is an object");
        assert_eq!(
            root.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            root.get("required"),
            Some(&serde_json::json!(["path", "content"]))
        );
        let properties = root
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("the schema discloses properties");
        assert_eq!(
            properties.len(),
            2,
            "the builtin discloses exactly two properties"
        );
        assert_eq!(
            properties.get("path"),
            Some(&serde_json::json!({"type": "string", "maxLength": 4096}))
        );
        assert_eq!(
            properties.get("content"),
            Some(&serde_json::json!({"type": "string", "maxLength": 16384}))
        );
        assert_eq!(
            root.get("minLength"),
            None,
            "the empty content is legal and the empty path is refused by Workspace authorization, not by the schema"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_tool_set_stays_empty_and_write_file_is_a_production_opt_in_builtin() {
        assert!(ToolSet::empty().definitions().is_empty());
        assert!(ToolSet::empty().prompt_view().is_empty());
        let temporary = TempDir::new("opt-in");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        assert_eq!(set.definitions().len(), 1);
        assert_eq!(set.prompt_view().specs().len(), 1);
        assert_eq!(set.definitions()[0].name().as_str(), WRITE_FILE_NAME);
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_file_declares_exactly_filesystem_write_parallel_mode_and_an_exact_sandbox_contract()
     {
        let temporary = TempDir::new("sandbox");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );

        assert_eq!(set.definitions()[0].mode(), ToolExecutionMode::Parallel);

        // The FileMutation plan's final permission set is exactly FilesystemWrite.
        let request = request_for(r#"{"path":"f.txt","content":"x"}"#);
        match set.plan(&request) {
            Some(ToolExecutionPlan::FileMutation { permissions, .. }) => {
                assert_eq!(
                    permissions,
                    ToolPermissionSet::new([ToolCapabilityClass::FilesystemWrite])
                );
                assert!(permissions.contains(ToolCapabilityClass::FilesystemWrite));
                assert!(!permissions.contains(ToolCapabilityClass::FilesystemRead));
                assert!(!permissions.contains(ToolCapabilityClass::Network));
                assert!(!permissions.contains(ToolCapabilityClass::Process));
            }
            _ => panic!("the valid call plans a FileMutation shape"),
        }

        // The captured sandbox contract is available exactly for FilesystemWrite, so the
        // planner's own admission passes and every other class fails closed.
        let sandbox = &set.inner.sandbox;
        assert_eq!(
            *sandbox,
            ToolSandboxContract::available([ToolCapabilityClass::FilesystemWrite])
        );
        assert!(
            sandbox
                .admit(ToolPermissionSet::new([
                    ToolCapabilityClass::FilesystemWrite
                ]))
                .is_ok()
        );
        assert!(matches!(
            sandbox.admit(ToolPermissionSet::new([
                ToolCapabilityClass::FilesystemRead
            ])),
            Err(ToolSandboxAdmissionError::CapabilityGap { .. })
        ));
        assert!(
            set.plan(&request)
                .is_some_and(|plan| matches!(plan, ToolExecutionPlan::FileMutation { .. }))
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn parse_and_semantic_failures_settle_the_frozen_preexecution_failed_result() {
        let temporary = TempDir::new("invalid");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        let invalid = vec![
            // Structural parse failures at every layer.
            "{}".to_owned(),
            r#"{"path":"f.txt"}"#.to_owned(),
            r#"{"content":"x"}"#.to_owned(),
            r#"{"path":"f.txt","content":"x","extra":1}"#.to_owned(),
            r#"{"Path":"f.txt","content":"x"}"#.to_owned(),
            r#"{"path":null,"content":"x"}"#.to_owned(),
            r#"{"path":1,"content":"x"}"#.to_owned(),
            r#"{"path":true,"content":"x"}"#.to_owned(),
            r#"{"path":[],"content":"x"}"#.to_owned(),
            r#"{"path":{},"content":"x"}"#.to_owned(),
            r#"{"path":"f.txt","content":null}"#.to_owned(),
            r#"{"path":"f.txt","content":1}"#.to_owned(),
            r#"{"path":"f.txt","content":true}"#.to_owned(),
            r#"{"path":"f.txt","content":[]}"#.to_owned(),
            r#"{"path":"f.txt","content":{}}"#.to_owned(),
            // The semantic WorkspaceRelativePath grammar: absolute, dot, empty, drive,
            // backslash, and control-char shapes all fail the path constructor.
            r#"{"path":"/etc/passwd","content":"x"}"#.to_owned(),
            r#"{"path":"../x","content":"x"}"#.to_owned(),
            r#"{"path":"..","content":"x"}"#.to_owned(),
            r#"{"path":".","content":"x"}"#.to_owned(),
            r#"{"path":"a/../b","content":"x"}"#.to_owned(),
            r#"{"path":"./a","content":"x"}"#.to_owned(),
            r#"{"path":"a//b","content":"x"}"#.to_owned(),
            r#"{"path":"a/","content":"x"}"#.to_owned(),
            r#"{"path":"/a","content":"x"}"#.to_owned(),
            r#"{"path":"C:/x","content":"x"}"#.to_owned(),
            r#"{"path":"a:b","content":"x"}"#.to_owned(),
            r#"{"path":"a\\b","content":"x"}"#.to_owned(),
            r#"{"path":"a\u0001b","content":"x"}"#.to_owned(),
            // Unsafe content: control characters fail the safe-text gate with no newline
            // normalization (a carriage return is rejected, never silently rewritten).
            r#"{"path":"f.txt","content":"a\u0001b"}"#.to_owned(),
            r#"{"path":"f.txt","content":"a\r\nb"}"#.to_owned(),
            // The 4,096-byte and 256-segment semantic path gates.
            format!(r#"{{"path":"{}","content":"x"}}"#, "x".repeat(4_097)),
            format!(r#"{{"path":"{}","content":"x"}}"#, "a/".repeat(256) + "a"),
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

        // The exact 4,096-byte path and 256-segment path, the exact 16,384-byte content,
        // a 16,384-byte multi-byte content, and the empty content stay valid: each plans
        // FileMutation, never invalid arguments.
        assert!(matches!(
            set.plan(&request_for(&format!(
                r#"{{"path":"{}","content":"x"}}"#,
                "x".repeat(4_096)
            ))),
            Some(ToolExecutionPlan::FileMutation { .. })
        ));
        assert!(matches!(
            set.plan(&request_for(&format!(
                r#"{{"path":"{}","content":"x"}}"#,
                "a/".repeat(255) + "a"
            ))),
            Some(ToolExecutionPlan::FileMutation { .. })
        ));
        assert!(matches!(
            set.plan(&request_for(&format!(
                r#"{{"path":"f.txt","content":"{}"}}"#,
                "x".repeat(16_384)
            ))),
            Some(ToolExecutionPlan::FileMutation { .. })
        ));
        assert!(matches!(
            set.plan(&request_for(&format!(
                r#"{{"path":"f.txt","content":"{}"}}"#,
                "é".repeat(8_192)
            ))),
            Some(ToolExecutionPlan::FileMutation { .. })
        ));
        assert!(matches!(
            set.plan(&request_for(r#"{"path":"f.txt","content":""}"#)),
            Some(ToolExecutionPlan::FileMutation { .. })
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_16384_byte_content_boundary_is_exact_and_oversize_is_closed_at_the_wire() {
        let temporary = TempDir::new("boundary");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;

        // A content string beyond 16,384 decoded bytes can never be a valid tool-call
        // arguments object at all: the wire decode gate rejects it before any planner
        // exists (both ASCII and multi-byte UTF-8).
        let oversized_ascii = format!(r#"{{"path":"f.txt","content":"{}"}}"#, "x".repeat(16_385));
        assert!(
            oversized_ascii
                .parse::<crate::wire::BoundedJsonObject>()
                .is_err(),
            "16,385 ASCII bytes cannot be a tool-call arguments string"
        );
        let oversized_multibyte =
            format!(r#"{{"path":"f.txt","content":"{}"}}"#, "é".repeat(8_193));
        assert!(
            oversized_multibyte
                .parse::<crate::wire::BoundedJsonObject>()
                .is_err(),
            "16,386 multi-byte UTF-8 bytes cannot be a tool-call arguments string"
        );

        // The planner's own safe-text gate independently enforces the same byte bound as
        // defense in depth, and the exact 16,384-byte boundary (ASCII and multi-byte)
        // plus the empty content pass it.
        assert!(validate_safe_text(&"x".repeat(16_385), MAX_CONTENT_BYTES, true).is_err());
        assert!(validate_safe_text(&"é".repeat(8_193), MAX_CONTENT_BYTES, true).is_err());
        assert!(validate_safe_text(&"x".repeat(16_384), MAX_CONTENT_BYTES, true).is_ok());
        assert!(validate_safe_text(&"é".repeat(8_192), MAX_CONTENT_BYTES, true).is_ok());
        assert!(validate_safe_text("", MAX_CONTENT_BYTES, true).is_ok());
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authorization_failures_settle_the_frozen_preexecution_denied_result() {
        let temporary = TempDir::new("denied");
        write(temporary.path(), "notes.txt", b"secret body");

        // No filesystem grant: the production default authority (fail-closed) resolves
        // the root but grants no filesystem access, so every write path is denied.
        let task_context = initialized_context().await;
        let no_grant = no_grant_context(&task_context, temporary.path()).await;
        let no_grant_set = ToolSet::write_file_builtin(
            no_grant,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        for arguments in [
            r#"{"path":"notes.txt","content":"x"}"#,
            r#"{"path":"anything.txt","content":"y"}"#,
        ] {
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

        // A ReadOnly authority ceiling keeps a ReadWrite-requested root at ReadOnly: the
        // requested-access intersection stays authoritative, so the write route is denied
        // even though the read route would be granted.
        let read_only = read_only_context(&task_context, temporary.path()).await;
        let read_only_set = ToolSet::write_file_builtin(
            read_only,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        let request = request_for(r#"{"path":"notes.txt","content":"x"}"#);
        let result = plan_failure(&read_only_set, &request);
        assert_eq!(
            result,
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec![
                    FILE_ACCESS_DENIED_TEXT.to_owned()
                ])
                .unwrap(),
            },
            "the ReadOnly ceiling must settle the frozen denied pre-execution result"
        );

        // The root path itself is refused by the Workspace authorization (empty/root
        // paths are never write targets) and collapses to the same frozen denied text.
        let granted = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            granted,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        let root_request = request_for(r#"{"path":"","content":"x"}"#);
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

        // A denied path never produces a preparation factory: the exact request's gate
        // still accepts its single reservation and start exactly like a never-touched
        // gate.
        let gate = ToolStartGate::new(root_request.clone());
        assert!(gate.reserve(&root_request).unwrap().start().is_ok());

        // The same file is writable through the granted set: denial is exact per path.
        assert!(
            set.plan(&request_for(r#"{"path":"notes.txt","content":"x"}"#))
                .is_some_and(|plan| matches!(plan, ToolExecutionPlan::FileMutation { .. }))
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_existing_replacement_writes_exact_bytes_and_empty_replacement_truncates() {
        let temporary = TempDir::new("success");
        write(
            temporary.path(),
            "note.txt",
            b"old body\nwith a trailing line",
        );
        write(temporary.path(), "empty.txt", b"old");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace.clone(),
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );

        // The ready preparation returns the exact opaque key of the opened physical
        // file: it equals the key computed from the directly prepared authorized target,
        // so the plan-driven preparation opens the same file the write later replaces.
        let request =
            request_for(r#"{"path":"note.txt","content":"line one\nline two\tand more"}"#);
        let key = match plan_preparation(&set, &request).prepare().await {
            PreparedToolExecution::Ready { key, .. } => key,
            PreparedToolExecution::Unstarted(result) => {
                panic!("expected a ready preparation, got {result:?}")
            }
        };
        let direct_key = workspace
            .access()
            .authorize_write(&"note.txt".parse().unwrap())
            .expect("the fixture target authorizes")
            .prepare()
            .expect("the fixture target prepares")
            .key();
        assert_eq!(key, direct_key);

        // The existing target is replaced in place with the exact content bytes: the
        // JSON-escaped newline and tab decode to their exact bytes, with no newline
        // normalization anywhere.
        let outcome = execute(Arc::clone(&set), request).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
            } if item_id == ITEM_ID.parse().unwrap()
                && tool_call_id == "call_write".parse().unwrap()
                && content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_WRITTEN_TEXT
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("note.txt")).unwrap(),
            b"line one\nline two\tand more"
        );

        // An empty replacement truncates the existing target to zero bytes.
        let outcome = execute(set, request_for(r#"{"path":"empty.txt","content":""}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_WRITTEN_TEXT
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("empty.txt")).unwrap(),
            b""
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_succeeds_only_when_the_direct_parent_exists_without_any_mkdir() {
        let temporary = TempDir::new("create");
        std::fs::create_dir_all(temporary.path().join("docs"))
            .expect("the test parent directory is creatable");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );

        // A missing target whose direct parent already exists is created with the exact
        // content through the retained parent capability.
        let outcome = execute(
            Arc::clone(&set),
            request_for(r#"{"path":"docs/new.txt","content":"created"}"#),
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
                && content.parts()[0].as_text() == FILE_WRITTEN_TEXT
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("docs/new.txt")).unwrap(),
            b"created"
        );

        // A missing target whose parent does not exist fails preparation with the frozen
        // pre-execution failure text: no mkdir anywhere, nothing is created.
        let request = request_for(r#"{"path":"missing/deep.txt","content":"x"}"#);
        let prepare = plan_preparation(&set, &request);
        let settled = prepare.prepare().await;
        assert!(
            matches!(
                settled,
                PreparedToolExecution::Unstarted(result)
                    if matches!(result, ToolExecutionResult::PreExecution {
                        disposition: ToolResultDisposition::Failed,
                        ref content,
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == FILE_UNWRITABLE_TEXT)
            ),
            "the missing parent settles the frozen pre-execution failure"
        );
        assert!(
            !temporary.path().join("missing/deep.txt").exists(),
            "no file is created without its parent"
        );
        assert!(
            !temporary.path().join("missing").exists(),
            "no directory is ever created"
        );
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn symlink_alias_replaces_the_same_physical_file_and_create_rejects_a_final_symlink() {
        use std::os::unix::fs::symlink;

        let temporary = TempDir::new("symlink");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::write(root.join("real.txt"), b"original").unwrap();
        symlink("real.txt", root.join("alias.txt")).unwrap();

        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, &root).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );

        // The in-root symlink alias is a normal-looking cwd-relative path, so
        // authorization grants it; the capability open follows the final symlink inside
        // containment, so the write replaces the exact physical file the alias names.
        let outcome = execute(
            Arc::clone(&set),
            request_for(r#"{"path":"alias.txt","content":"replaced through alias"}"#),
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
                && content.parts()[0].as_text() == FILE_WRITTEN_TEXT
        ));
        assert_eq!(
            std::fs::read(root.join("real.txt")).unwrap(),
            b"replaced through alias"
        );
        assert!(
            std::fs::symlink_metadata(root.join("alias.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // A dangling final symlink at a create target: preparation observes NotFound
        // (the link's target is missing) and binds the create shape to the root parent
        // plus the final name, but the started write opens the final name with a
        // final-component no-follow option, so it fails closed instead of following the
        // link and creating the target elsewhere.
        symlink("elsewhere.txt", root.join("dangling.txt")).unwrap();
        let outcome = execute(
            set,
            request_for(r#"{"path":"dangling.txt","content":"must not escape"}"#),
        )
        .await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_UNWRITABLE_TEXT
        ));
        assert!(
            !root.join("elsewhere.txt").exists(),
            "the no-follow write never creates the link target"
        );
        assert!(
            std::fs::symlink_metadata(root.join("dangling.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the dangling symlink itself is left untouched"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preparation_ordinary_failure_settles_the_frozen_preexecution_failed_result() {
        let temporary = TempDir::new("prepare-failure");
        std::fs::create_dir_all(temporary.path().join("docs"))
            .expect("the test directory is creatable");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );

        // A directory is a valid cwd-relative path but cannot be opened as a write
        // target: the ordinary preparation failure settles the frozen pre-execution
        // failed text, never an Abandoned outcome.
        let request = request_for(r#"{"path":"docs","content":"x"}"#);
        let prepare = plan_preparation(&set, &request);
        let settled = prepare.prepare().await;
        assert!(
            matches!(
                settled,
                PreparedToolExecution::Unstarted(result)
                    if matches!(result, ToolExecutionResult::PreExecution {
                        disposition: ToolResultDisposition::Failed,
                        ref content,
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == FILE_UNWRITABLE_TEXT)
            ),
            "the ordinary preparation failure settles the frozen pre-execution failure"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preparation_runtime_task_error_and_operation_panic_settle_abandoned_runtime_failure() {
        let temporary = TempDir::new("prepare-runtime");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let request = request_for(r#"{"path":"f.txt","content":"x"}"#);

        // A preparation join failure (owner closing, worker unavailable) makes the
        // outcome unknowable and settles the explicit unstarted Abandoned RuntimeFailure,
        // never a fabricated pre-execution result.
        let set = ToolSet::write_file_builtin(
            workspace.clone(),
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        task_context.inject_next_blocking_job_join_failure();
        let prepare = plan_preparation(&set, &request);
        assert!(
            matches!(
                prepare.prepare().await,
                PreparedToolExecution::Unstarted(result)
                    if matches!(result, ToolExecutionResult::Abandoned {
                        reason: ToolAbandonReason::RuntimeFailure,
                    })
            ),
            "a preparation join failure settles the explicit Abandoned RuntimeFailure"
        );

        // An operation panic / post-operation join failure after the preparation ran
        // settles the same explicit Abandoned RuntimeFailure.
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        task_context.inject_next_blocking_job_post_operation_panic();
        let prepare = plan_preparation(&set, &request);
        assert!(
            matches!(
                prepare.prepare().await,
                PreparedToolExecution::Unstarted(result)
                    if matches!(result, ToolExecutionResult::Abandoned {
                        reason: ToolAbandonReason::RuntimeFailure,
                    })
            ),
            "a preparation panic settles the explicit Abandoned RuntimeFailure"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn started_write_with_a_missing_target_handoff_settles_abandoned_runtime_failure() {
        let temporary = TempDir::new("missing-handoff");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        // A consumed/empty handoff is an invariant: the started write fails closed to the
        // identity-bound Abandoned RuntimeFailure instead of fabricating any result.
        let (_handle, observer) = ToolCancellationHandle::new();
        let outcome = execute_write(
            Arc::new(Mutex::new(None)),
            "must not be written".to_owned(),
            task_context.clone(),
            observer,
        )
        .await;
        assert!(matches!(
            outcome,
            ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
            }
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_write_job_scheduling_proves_zero_mutation() {
        let temporary = TempDir::new("cancel-before");
        write(temporary.path(), "keep.txt", b"original body");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        // A valid replacement: any scheduled write would overwrite the file, so the
        // Cancelled outcome below proves no blocking write job was ever created.
        let request = request_for(r#"{"path":"keep.txt","content":"replacement"}"#);

        let start = prepared_start(plan_preparation(&set, &request)).await;
        let outcome = run_started(set, request, start, true).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Cancelled,
                ref content,
            } if item_id == ITEM_ID.parse().unwrap()
                && tool_call_id == "call_write".parse().unwrap()
                && content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_CANCELLED_TEXT
        ));
        // Zero mutation: the file still carries its original bytes.
        assert_eq!(
            std::fs::read(temporary.path().join("keep.txt")).unwrap(),
            b"original body"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_the_write_job_is_scheduled_keeps_awaiting_it_and_preserves_the_result()
     {
        let temporary = TempDir::new("cancel-after-schedule");
        write(temporary.path(), "real.txt", b"old");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        let request = request_for(r#"{"path":"real.txt","content":"new body"}"#);

        let start = prepared_start(plan_preparation(&set, &request)).await;
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");

        // The preparation has settled, so arming the one-shot entry gate now targets the
        // exact next blocking admission: the tracked write job.  One poll drives the
        // executor through the biased pre-scheduling select and schedules that job; the
        // gate then deterministically proves the worker entered the spawned job before
        // its operation closure — no sleeps, timeouts, or blind polling.
        let gate = task_context.arm_next_blocking_job_entry_gate();
        let mut run = std::pin::pin!(run);
        let first = std::future::poll_fn(|cx| std::task::Poll::Ready(run.as_mut().poll(cx))).await;
        assert!(
            first.is_pending(),
            "the executor awaits the tracked write job's settlement"
        );
        // The exact write job is now provably scheduled and held at the gate before its
        // operation closure: signal the cancellation while it is in flight, then release
        // it.
        gate.wait_until_entered();
        handle.cancel();
        gate.release();

        // The executor kept awaiting the same tracked job to its settlement and preserved
        // the truthful result: a known success is never rewritten by the cancellation.
        let outcome = run.await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == FILE_WRITTEN_TEXT
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("real.txt")).unwrap(),
            b"new body"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracked_runtime_failure_after_start_settles_abandoned_runtime_failure() {
        let temporary = TempDir::new("runtime-failure");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        let request = request_for(r#"{"path":"f.txt","content":"new body"}"#);
        // The preparation settles first; the one-shot seam then targets the started write
        // job, which joins as an immediate worker cancellation without ever running its
        // operation closure, so the write outcome is unknown and must settle Abandoned
        // RuntimeFailure.
        let start = prepared_start(plan_preparation(&set, &request)).await;
        task_context.inject_next_blocking_job_join_failure();
        let outcome = run_started(set, request, start, false).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            } if item_id == ITEM_ID.parse().unwrap()
                && tool_call_id == "call_write".parse().unwrap()
        ));
        // The aborted write job never ran its operation closure: zero mutation.
        assert_eq!(std::fs::read(temporary.path().join("f.txt")).unwrap(), b"x");
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_tool_name_plans_none_and_writes_never_construct_a_preparation_factory() {
        let temporary = TempDir::new("unknown");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = ToolSet::write_file_builtin(
            workspace,
            task_context.clone(),
            SessionFileMutationQueue::new(),
        );
        let unknown = ToolExecutionRequest::new(
            ITEM_ID.parse().unwrap(),
            ToolCall::new(
                "call_other".parse().unwrap(),
                "other_tool".parse().unwrap(),
                r#"{"path":"f.txt","content":"x"}"#.parse().unwrap(),
                0,
            ),
        );
        assert!(set.plan(&unknown).is_none());
        // A valid write_file request still plans FileMutation through the same set.
        assert!(
            set.plan(&request_for(r#"{"path":"f.txt","content":"x"}"#))
                .is_some_and(|plan| matches!(plan, ToolExecutionPlan::FileMutation { .. }))
        );
        task_context.shutdown().await;
    }
}
