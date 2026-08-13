//! The production `list_directory` builtin: one closed, default-off, Runtime-owned Tool that
//! lists the direct entries of exactly one directory relative to the Workspace cwd.
//!
//! The builtin is immutable after construction and travels through the existing residency
//! ToolSet capture path.  This first slice is deliberately narrow: one required `path`
//! string argument (the empty path is the cwd itself, a legal listing target), no
//! recursion, no file-content reads, no options, no absolute paths, no additional-root
//! addressing.  The builtin itself stays narrow: production composition belongs to the
//! closed `ProductionToolConfig`, which fixes exactly the three frozen builtins in one
//! order (`ask_user` → `read_file` → `list_directory`) — there is no generic registry and
//! no dynamic composition.
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
//!   `workspace directory access is denied` when `WorkspaceAccessView::authorize_read_directory`
//!   refuses the exact cwd-relative path: no readable grant or a path outside the cwd's
//!   containing root.  Every authorization error collapses to this one frozen, bounded,
//!   non-secret text; the denial happens synchronously before constructing start.
//!
//! The started executor never uses an ambient path, `std::fs::read_dir`, or `canonicalize`:
//! it opens the exact `AuthorizedWorkspaceReadDirectory` the authorization granted with the
//! capability open (resolving through the captured `cap_std::fs::Dir`, which cannot leave
//! the root it was bound to — a symlink escape fails at that open), enumerates only direct
//! entries, converts each entry name as UTF-8 (any non-UTF-8 name is an unsupported entry),
//! classifies each entry by its own `DirEntry::file_type` without following entry symlinks,
//! enforces the closed bounds, sorts deterministically, and renders one compact JSON Text
//! part of the exact shape `{"entries":[{"name":"...","type":"file"},...]}`.  It returns
//! exactly one Text part on success, with the exact frozen `Completed + Failed` texts
//! otherwise:
//!
//! - `directory could not be listed` for a missing path, a non-directory entry, an open or
//!   iteration error, a file-type error on a checked in-bound entry, or a symlink escape
//!   denied at the capability open;
//! - `directory contains an unsupported entry name` for any checked in-bound entry name that
//!   is not valid UTF-8;
//! - `directory listing is too large` for more than 256 direct entries (the 257th item is
//!   consumed only to detect the overflow) or retained UTF-8 name bytes summing beyond
//!   8,192 (or the final JSON beyond 65,536 bytes / not safe Text, which fails closed the
//!   same way).
//!
//! The rendered `ListingJson` shape always serializes by construction (bounded Strings and
//! fixed static-str kinds); if that internal invariant ever broke, the blocking operation
//! would panic and the tracked runtime would settle `Abandoned { RuntimeFailure }` — never
//! the too-large text.
//!
//! Cancellation is cooperative: once start has happened, a cancellation that already won
//! (or races the scheduling) before any blocking job is created settles the exact frozen
//! `Completed + Cancelled` text `directory listing was cancelled` — zero I/O is proven, so
//! the truthful disposition is Cancelled with that one Text part, never OutcomeUnknown.
//! Once the tracked job is scheduled it is never dropped or detached — a cancellation
//! arriving while the listing runs keeps awaiting the same tracked job to its bounded
//! settlement (one capability open plus at most 257 direct entries) and preserves the
//! truthful result, so a known success/failure is never rewritten.  Only a
//! `RuntimeTaskError` (owner closing, worker unavailable, operation panic) settles
//! `Abandoned { RuntimeFailure }`; the signal-before-start case is owned by the ToolSet's
//! gate slot, which settles its own PreExecution Cancelled result before this executor is
//! ever constructed.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError, TrackedBlockingJob};
use crate::wire::lexical::validate_safe_text;
use crate::wire::{BoundedJsonObject, WorkspaceRelativePath};
use crate::workspace::{AuthorizedWorkspaceReadDirectory, WorkspaceToolContext};

use super::{
    ToolAbandonReason, ToolCancellationObserver, ToolCapabilityClass, ToolDefinition,
    ToolExecutionMode, ToolExecutionPlan, ToolExecutionRequest, ToolExecutionResult,
    ToolExecutionStart, ToolPermissionSet, ToolResultContent, ToolResultDisposition,
    ToolSandboxContract, ToolSet, ToolSetInner, ToolSpec,
};

/// The exact production builtin ToolName.  `pub(super)` because the composed production
/// ToolSet routes exactly this frozen name.
pub(super) const LIST_DIRECTORY_NAME: &str = "list_directory";

/// The exact production description disclosed for the builtin: listing one directory's
/// direct entries relative to the Workspace working directory.  Frozen; asserted verbatim
/// in module tests.
const LIST_DIRECTORY_DESCRIPTION: &str = "List direct entries in one directory relative to the workspace working directory and return sorted JSON names and types. Use for discovering files and subdirectories without reading file contents.";

/// The exact frozen PreExecution Failed text for every parse or semantic argument failure.
const INVALID_ARGUMENTS_TEXT: &str = "tool arguments are invalid";

/// The exact frozen PreExecution Denied text for every authorization failure (no readable
/// grant or a path outside the cwd's containing root).
const DIRECTORY_ACCESS_DENIED_TEXT: &str = "workspace directory access is denied";

/// The exact frozen Completed Cancelled text for a cancellation that won after start but
/// before any blocking job was created: zero I/O is proven, so the truthful disposition is
/// Cancelled with this one Text part, never OutcomeUnknown.
const DIRECTORY_CANCELLED_TEXT: &str = "directory listing was cancelled";

/// The exact frozen Completed Failed text for a missing/not-directory/open/iteration error,
/// an in-bound entry file-type error (or a symlink escape denied at the capability open).
const DIRECTORY_UNLISTABLE_TEXT: &str = "directory could not be listed";

/// The exact frozen Completed Failed text for any checked in-bound entry name that is not
/// valid UTF-8.
const DIRECTORY_UNSUPPORTED_NAME_TEXT: &str = "directory contains an unsupported entry name";

/// The exact frozen Completed Failed text for any closed bound violation: more than 256
/// direct entries, retained UTF-8 name bytes summing beyond 8,192, or a final JSON beyond
/// 65,536 bytes / not safe Text.
const DIRECTORY_TOO_LARGE_TEXT: &str = "directory listing is too large";

/// The closed direct-entry bound: at most 256 entries are preserved on success.
const MAX_DIRECT_ENTRIES: usize = 256;

/// One past the entry bound: the 257th item is consumed only to detect the overflow, then
/// the listing stops immediately.
const MAX_ENTRIES_PLUS_ONE: usize = MAX_DIRECT_ENTRIES + 1;

/// The closed retained-name bound: the raw UTF-8 bytes of all retained entry names sum to
/// at most 8,192.
const MAX_NAME_BYTES_TOTAL: usize = 8192;

/// The closed one-part content bound: the rendered JSON Text part is at most 65,536 bytes
/// and safe Text.
const MAX_LISTING_JSON_BYTES: usize = 65_536;

/// The closed input schema disclosed for the builtin.  Structural guidance only: the
/// `WorkspaceRelativePath` grammar (canonical cwd-relative path, no absolute/dot segments,
/// at most 4,096 bytes) is enforced by the semantic constructor, never by this schema; the
/// empty path is the cwd itself and is a legal listing target, so no `minLength` is
/// disclosed.
const LIST_DIRECTORY_SCHEMA: &str = r#"{
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
            name: LIST_DIRECTORY_NAME
                .parse()
                .expect("the frozen list_directory ToolName is valid"),
            description: Arc::from(LIST_DIRECTORY_DESCRIPTION),
            input_schema: LIST_DIRECTORY_SCHEMA
                .parse()
                .expect("the frozen list_directory schema is valid"),
        },
        // One bounded direct-entry listing per call; the definition does not impose Serial
        // execution semantics on unrelated operations in the composed production ToolSet.
        mode: ToolExecutionMode::Parallel,
    }
}

/// The outer ToolSet sandbox contract of the list_directory builtin: available exactly for
/// `FilesystemRead`.  The composed production ToolSet keeps this exact contract, so the
/// list_directory route's Execute plan is admitted exactly once against the same ceiling.
pub(super) fn sandbox() -> ToolSandboxContract {
    ToolSandboxContract::available([ToolCapabilityClass::FilesystemRead])
}

/// Builds the exact immutable production `list_directory` ToolSet: one definition, one
/// matching spec, the builtin planner pinned to the exact captured Workspace tool context
/// and task context, and the available sandbox contract enforcing exactly `FilesystemRead`.
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

/// The synchronous pre-start plan for one exact `list_directory` call: a valid, authorized
/// call plans the Execute shape carrying exactly `FilesystemRead` and a move-only start
/// factory; every parse/semantic failure plans the frozen `PreExecution + Failed` result;
/// every authorization failure plans the frozen `PreExecution + Denied` result before any
/// start factory exists.  `pub(super)` because the composed production ToolSet routes
/// exactly this frozen planner.
pub(super) fn plan(
    workspace: &WorkspaceToolContext,
    task_context: &RuntimeTaskContext,
    request: ToolExecutionRequest,
) -> ToolExecutionPlan {
    let arguments = match parse_arguments(request.call().arguments()) {
        Ok(arguments) => arguments,
        Err(()) => return invalid_arguments_plan(),
    };
    // Authorize the exact cwd-relative directory synchronously, before any start factory
    // exists.  Every WorkspaceAccessError (no grant, outside the cwd's root, or an
    // unavailable basis) collapses to the one frozen Denied text.
    let target = match workspace.access().authorize_read_directory(&arguments.path) {
        Ok(target) => target,
        Err(_) => return denied_plan(),
    };
    let task_context = task_context.clone();
    ToolExecutionPlan::Execute {
        permissions: ToolPermissionSet::new([ToolCapabilityClass::FilesystemRead]),
        start: ToolExecutionStart::new(move |observer| {
            Box::pin(execute_list(target, task_context, observer))
        }),
    }
}

fn invalid_arguments_plan() -> ToolExecutionPlan {
    ToolExecutionPlan::PreExecution(pre_execution_failed(INVALID_ARGUMENTS_TEXT))
}

fn denied_plan() -> ToolExecutionPlan {
    ToolExecutionPlan::PreExecution(ToolExecutionResult::PreExecution {
        disposition: ToolResultDisposition::Denied,
        content: ToolResultContent::from_text_parts(vec![DIRECTORY_ACCESS_DENIED_TEXT.to_owned()])
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
struct ListDirectoryArguments {
    path: WorkspaceRelativePath,
}

fn parse_arguments(arguments: &BoundedJsonObject) -> Result<ListDirectoryArguments, ()> {
    serde_json::from_str(arguments.canonical_json()).map_err(|_| ())
}

/// The cooperative-cancellation executor for one started listing.
///
/// Once start has happened (this executor exists), a cancellation that already won (or
/// races the scheduling) before any blocking job is created wins via a biased select: the
/// scheduling branch completes synchronously once polled, so a cancellation observed first
/// means no blocking job is ever created and there is nothing to clean up — the truthful
/// settlement is the exact frozen `Completed + Cancelled` text
/// `directory listing was cancelled` because zero I/O is proven.  The signal-before-start
/// case is owned by the ToolSet's gate slot, which settles its own PreExecution Cancelled
/// result before this executor is ever constructed.
///
/// Once the tracked job is scheduled it is never dropped or detached: a cancellation
/// arriving while the listing runs keeps awaiting the same tracked job to its bounded
/// settlement and preserves the truthful result, so a known success/failure is never
/// rewritten.  Only a `RuntimeTaskError` settles `Abandoned { RuntimeFailure }`.
async fn execute_list(
    target: AuthorizedWorkspaceReadDirectory,
    task_context: RuntimeTaskContext,
    observer: ToolCancellationObserver,
) -> ToolExecutionResult {
    let job = tokio::select! {
        biased;
        _ = observer.cancelled() => {
            return ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Cancelled,
                content: ToolResultContent::from_text_parts(vec![DIRECTORY_CANCELLED_TEXT.to_owned()])
                    .expect("the frozen cancelled text is a valid bounded part"),
            };
        }
        scheduled = schedule_list_job(target, task_context) => scheduled,
    };
    let outcome = tokio::select! {
        biased;
        _ = observer.cancelled() => job.wait().await,
        settled = job.wait() => settled,
    };
    bind_list_outcome(outcome)
}

async fn schedule_list_job(
    target: AuthorizedWorkspaceReadDirectory,
    task_context: RuntimeTaskContext,
) -> TrackedBlockingJob<ListDirectoryOutcome> {
    task_context.spawn_blocking_tracked(move || list_direct_entries(target))
}

/// The bounded outcome of one tracked blocking listing: every directory-processing decision
/// (capability open, direct enumeration, UTF-8 name conversion, file-type classification,
/// bounds, sorting, compact JSON render, safe-Text validation) happens inside the blocking
/// closure so the executor only maps a settled outcome.
#[derive(Clone)]
enum ListDirectoryOutcome {
    /// A bounded listing's exact compact JSON: one safe Text part at most 65,536 bytes.
    Listing(Arc<str>),
    /// Missing path, non-directory entry, any open/iteration error, an in-bound entry
    /// file-type error, or a symlink escape denied at the capability open.
    Unlistable,
    /// Any checked in-bound entry name that is not valid UTF-8.
    UnsupportedName,
    /// More than 256 direct entries, retained name bytes beyond 8,192, or a final JSON
    /// beyond the one-part content bound / not safe Text.
    TooLarge,
}

/// The bounded blocking listing: one capability-relative open plus at most 257 direct
/// entries consumed.  The open resolves through the authorized root's captured
/// `cap_std::fs::Dir`, so it can never leave the root; a symlink escape fails at that open.
/// Only direct entries are read — never recursively, never a file's content — and each
/// entry is classified by its own `DirEntry::file_type` (lstat semantics, so entry
/// symlinks are reported as `symlink`, never followed).  A proven `Ok` 257th item settles
/// overflow immediately without checking its name/type; otherwise the retained UTF-8 name
/// bytes of the in-bound entries are budgeted: each name is
/// UTF-8-checked and measured as a borrow (`to_str`) against the cumulative 8,192 budget
/// before any copy, so the listing stops immediately once the budget is exceeded, before
/// any further iteration or any copy of an over-budget name.  The budget governs only
/// those retained copies — the transient `OsString` cap-std/OS constructs for one
/// `DirEntry::file_name` is not controlled here.
fn list_direct_entries(target: AuthorizedWorkspaceReadDirectory) -> ListDirectoryOutcome {
    let dir = match target.open() {
        Ok(dir) => dir,
        Err(_) => return ListDirectoryOutcome::Unlistable,
    };
    let mut entries = match dir.entries() {
        Ok(entries) => entries,
        Err(_) => return ListDirectoryOutcome::Unlistable,
    };
    let mut collected: Vec<(String, &'static str)> = Vec::with_capacity(MAX_DIRECT_ENTRIES);
    let mut name_bytes_total: usize = 0;
    // The listing consumes at most MAX_ENTRIES_PLUS_ONE direct entries: the 257th item is
    // read only to detect the overflow, then the listing stops immediately, without
    // iterating any further.  The consumed item is classified first: an iteration error
    // on any item — the 257th included — settles Unlistable per the frozen contract, and
    // only a proven 257th Ok entry settles TooLarge.
    for _ in 0..MAX_ENTRIES_PLUS_ONE {
        let item = match entries.next() {
            None => break,
            Some(item) => item,
        };
        let entry = match classify_next_item(item, collected.len()) {
            Ok(entry) => entry,
            Err(outcome) => return outcome,
        };
        // The entry name is checked and budgeted as a borrow before any copy: invalid
        // UTF-8 settles UnsupportedName, an over-budget length settles TooLarge, and only
        // an in-budget name is copied (`to_owned`) into the collected set below.
        let file_name = entry.file_name();
        let name = match file_name.to_str() {
            Some(name) => name,
            None => return ListDirectoryOutcome::UnsupportedName,
        };
        name_bytes_total = match name_bytes_total.checked_add(name.len()) {
            Some(total) if total <= MAX_NAME_BYTES_TOTAL => total,
            _ => return ListDirectoryOutcome::TooLarge,
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => return ListDirectoryOutcome::Unlistable,
        };
        let kind = if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else if file_type.is_symlink() {
            "symlink"
        } else {
            "other"
        };
        collected.push((name.to_owned(), kind));
    }
    // Deterministic byte-ascending order over the retained UTF-8 names; directory entry
    // names are unique, so the ordering is total.
    collected.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let listing = ListingJson {
        entries: collected
            .into_iter()
            .map(|(name, entry_type)| EntryJson { name, entry_type })
            .collect(),
    };
    // The closed shape serializes by construction — bounded String names and fixed
    // static-str kinds cannot fail serde_json serialization — so a failure here is an
    // internal invariant: the panic is caught by the tracked runtime and settles
    // `Abandoned { RuntimeFailure }`, never the too-large outcome.  The byte and safe-Text
    // gates below still fail closed as the frozen too-large text.
    let json =
        serde_json::to_string(&listing).expect("the closed ListingJson shape always serializes");
    if json.len() > MAX_LISTING_JSON_BYTES
        || validate_safe_text(&json, MAX_LISTING_JSON_BYTES, true).is_err()
    {
        // The rendered JSON must be one safe Text part within the one-part content bound;
        // any violation fails closed as the frozen too-large text.
        return ListDirectoryOutcome::TooLarge;
    }
    ListDirectoryOutcome::Listing(json.into())
}

/// Classifies one consumed iterator item against the direct-entry bound.  An iteration
/// error always settles `Unlistable` — even on the 257th item, because only a proven `Ok`
/// entry past the bound proves the overflow; a proven `Ok` entry once 256 are already
/// collected settles `TooLarge`; anything else passes through unchanged.  The caller
/// already consumed the item, so this never iterates beyond the 257th.
fn classify_next_item<T, E>(
    item: Result<T, E>,
    collected_len: usize,
) -> Result<T, ListDirectoryOutcome> {
    match item {
        Err(_) => Err(ListDirectoryOutcome::Unlistable),
        Ok(_) if collected_len >= MAX_DIRECT_ENTRIES => Err(ListDirectoryOutcome::TooLarge),
        Ok(entry) => Ok(entry),
    }
}

/// The exact compact JSON shape of one entry: fixed field order `name`, then `type`.
#[derive(Serialize)]
struct EntryJson {
    name: String,
    #[serde(rename = "type")]
    entry_type: &'static str,
}

/// The exact compact JSON shape of one listing: `{"entries":[{name,type},...]}` with the
/// preserved entries in deterministic byte order.
#[derive(Serialize)]
struct ListingJson {
    entries: Vec<EntryJson>,
}

fn bind_list_outcome(
    outcome: Result<ListDirectoryOutcome, RuntimeTaskError>,
) -> ToolExecutionResult {
    match outcome {
        Ok(ListDirectoryOutcome::Listing(json)) => {
            // The JSON already passed the owner's safe-Text and byte gates inside the
            // blocking closure, so a failure here is an invariant that fails closed.
            match ToolResultContent::from_text_parts(vec![json.as_ref().to_owned()]) {
                Ok(content) => ToolExecutionResult::Completed {
                    disposition: ToolResultDisposition::Succeeded,
                    content,
                },
                Err(_) => ToolExecutionResult::Abandoned {
                    reason: ToolAbandonReason::RuntimeFailure,
                },
            }
        }
        Ok(ListDirectoryOutcome::Unlistable) => completed_failed(DIRECTORY_UNLISTABLE_TEXT),
        Ok(ListDirectoryOutcome::UnsupportedName) => {
            completed_failed(DIRECTORY_UNSUPPORTED_NAME_TEXT)
        }
        Ok(ListDirectoryOutcome::TooLarge) => completed_failed(DIRECTORY_TOO_LARGE_TEXT),
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

impl fmt::Debug for ListDirectoryOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listing(_) => formatter.write_str("ListDirectoryOutcome::Listing(..)"),
            Self::Unlistable => formatter.write_str("ListDirectoryOutcome::Unlistable"),
            Self::UnsupportedName => formatter.write_str("ListDirectoryOutcome::UnsupportedName"),
            Self::TooLarge => formatter.write_str("ListDirectoryOutcome::TooLarge"),
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
    use crate::wire::{SessionId, WorkspaceRevision};
    use crate::workspace::{
        RequestedFilesystemAccess, Workspace, WorkspaceCwdSpec, WorkspaceDefinitionInput,
        WorkspacePathTarget, WorkspaceResolver, WorkspaceRootInput, WorkspaceSourcePolicy,
        WorkspaceToolContext, lower_workspace, native_path_uri_for_test,
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
                "minicore-runtime-list-directory-{label}-{}-{suffix}",
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

    /// Creates one directory (creating parents) inside the test root.
    fn create_dir(root: &Path, relative: &str) {
        std::fs::create_dir_all(root.join(relative)).expect("the test directory is creatable");
    }

    fn request_for(arguments: &str) -> ToolExecutionRequest {
        ToolExecutionRequest::new(
            ITEM_ID.parse().unwrap(),
            ToolCall::new(
                "call_list".parse().unwrap(),
                LIST_DIRECTORY_NAME.parse().unwrap(),
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
        let uri = native_path_uri_for_test(root);
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
            WorkspacePathTarget::current(),
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

    /// Asserts one exact one-part JSON success: `Succeeded` with exactly the expected
    /// compact JSON text.
    fn assert_exact_listing(outcome: &ToolExecutionOutcome, expected: &str) {
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == expected
        ));
    }

    /// Asserts the frozen one-part Failed text.
    fn assert_failed_text(outcome: &ToolExecutionOutcome, expected: &str) {
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == expected
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builtin_defines_exactly_list_directory_with_the_frozen_description_and_closed_schema()
    {
        let temporary = TempDir::new("definition");
        write(temporary.path(), "f.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        let definitions = set.definitions();
        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];
        assert_eq!(definition.name().as_str(), LIST_DIRECTORY_NAME);
        assert_eq!(definition.mode(), ToolExecutionMode::Parallel);
        // The frozen description is documented by this assertion: any edit to the disclosed
        // description must be reflected here deliberately.
        assert_eq!(
            definition.spec.description.as_ref(),
            LIST_DIRECTORY_DESCRIPTION
        );

        // The prompt view discloses exactly the same single spec (name, description,
        // closed schema); planner and sandbox internals never enter the model context.
        let view = set.prompt_view();
        assert!(!view.is_empty());
        assert_eq!(view.specs().len(), 1);
        assert_eq!(view.specs()[0].name().as_str(), LIST_DIRECTORY_NAME);
        assert_eq!(view.specs()[0].description(), LIST_DIRECTORY_DESCRIPTION);

        // The disclosed schema is exactly the frozen schema: canonical bytes round-trip to
        // the same semantic value and stay within the bounded schema limit.
        let schema = view.specs()[0].input_schema();
        assert_eq!(
            schema.canonical_json(),
            LIST_DIRECTORY_SCHEMA
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
            "the empty cwd path is a legal listing target, not refused by the schema"
        );
        assert_eq!(
            root.get("recursive"),
            None,
            "no recursion/options/absolute/additional-root addressing is disclosed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_tool_set_stays_empty_and_list_directory_is_a_production_opt_in_builtin() {
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
    async fn list_directory_declares_exactly_filesystem_read_parallel_mode_and_an_exact_sandbox_contract()
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
            // The semantic WorkspaceRelativePath grammar: absolute, dot, drive, backslash,
            // and control-char shapes all fail the path constructor.
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
        // root but grants no filesystem access, so every directory path is denied.
        let task_context = initialized_context().await;
        let no_grant_resolver = WorkspaceResolver::new(task_context.clone());
        let no_grant_workspace =
            resolve_context(no_grant_resolver, workspace_spec(temporary.path(), "")).await;
        let no_grant_set = build_tool_set(no_grant_workspace, task_context.clone());
        for arguments in [
            r#"{"path":"notes.txt"}"#,
            r#"{"path":"anything.txt"}"#,
            r#"{"path":""}"#,
        ] {
            let request = request_for(arguments);
            let result = plan_failure(&no_grant_set, &request);
            assert_eq!(
                result,
                ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Denied,
                    content: ToolResultContent::from_text_parts(vec![
                        DIRECTORY_ACCESS_DENIED_TEXT.to_owned()
                    ])
                    .unwrap(),
                },
                "{arguments} must settle the frozen denied pre-execution result without a grant"
            );
        }

        // A denied path never produces a start factory: the exact request's gate still
        // accepts its single reservation and start exactly like a never-touched gate.
        let denied_request = request_for(r#"{"path":"notes.txt"}"#);
        assert!(matches!(
            no_grant_set.plan(&denied_request),
            Some(ToolExecutionPlan::PreExecution(_))
        ));
        let gate = ToolStartGate::new(denied_request.clone());
        assert!(gate.reserve(&denied_request).unwrap().start().is_ok());

        // With a readable grant, the empty path (the cwd itself) is a legal listing target
        // and plans Execute — unlike a read target, a directory is always listable.
        let granted = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(granted, task_context.clone());
        assert_plans_execute(&set, &request_for(r#"{"path":""}"#));

        // The same directory is listable through the granted set: denial is exact per path.
        assert_plans_execute(&set, &request_for(r#"{"path":"notes.txt"}"#));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_cwd_and_nested_listing_settle_sorted_exact_json() {
        let temporary = TempDir::new("listing");
        create_dir(temporary.path(), "docs");
        write(temporary.path(), "docs/readme.md", b"# readme");
        create_dir(temporary.path(), "docs/sub");
        write(temporary.path(), "zeta.md", b"z");
        write(temporary.path(), "Alpha.txt", b"A");

        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        // The empty path lists the cwd itself; the entries are sorted by UTF-8 name bytes
        // (uppercase "Alpha.txt" sorts before lowercase "docs"/"zeta.md"), with exact
        // `file`/`directory` types in the exact compact shape.
        let outcome = execute(Arc::clone(&set), request_for(r#"{"path":""}"#)).await;
        assert_exact_listing(
            &outcome,
            r#"{"entries":[{"name":"Alpha.txt","type":"file"},{"name":"docs","type":"directory"},{"name":"zeta.md","type":"file"}]}"#,
        );

        // A nested directory lists its own direct entries only: no recursion, no
        // readme.md content.
        let nested = execute(Arc::clone(&set), request_for(r#"{"path":"docs"}"#)).await;
        assert_exact_listing(
            &nested,
            r#"{"entries":[{"name":"readme.md","type":"file"},{"name":"sub","type":"directory"}]}"#,
        );

        // An empty directory succeeds with the exact empty listing shape.
        create_dir(temporary.path(), "empty");
        let empty = execute(set, request_for(r#"{"path":"empty"}"#)).await;
        assert_exact_listing(&empty, r#"{"entries":[]}"#);
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_exact_256_entry_boundary_succeeds_and_257_is_too_large() {
        let temporary = TempDir::new("boundary");
        // 256 fixed-width names: lexicographic byte order equals numeric order, so the
        // expected JSON is fully deterministic.
        let mut expected = String::from(r#"{"entries":["#);
        for index in 0..MAX_DIRECT_ENTRIES {
            let name = format!("f{index:03}");
            write(temporary.path(), &name, b"x");
            if index > 0 {
                expected.push(',');
            }
            expected.push_str(&format!(r#"{{"name":"{name}","type":"file"}}"#));
        }
        expected.push_str("]}");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        let outcome = execute(Arc::clone(&set), request_for(r#"{"path":""}"#)).await;
        assert_exact_listing(&outcome, &expected);

        // The 257th direct entry is consumed only to detect the overflow and settles the
        // frozen too-large text immediately, without listing anything.
        write(temporary.path(), "f256", b"x");
        let overflow = execute(set, request_for(r#"{"path":""}"#)).await;
        assert_failed_text(&overflow, DIRECTORY_TOO_LARGE_TEXT);
        task_context.shutdown().await;
    }

    #[test]
    fn the_257th_item_that_errors_settles_unlistable_not_too_large() {
        // The pure classifier pins the exact ordering the loop relies on: an iteration
        // error on the 257th item is still an iteration error and settles Unlistable, and
        // only a proven 257th Ok entry settles TooLarge.  A real filesystem cannot
        // naturally inject a mid-iteration readdir error on exactly the 257th item, so
        // the ordering is pinned here instead of through the cap-std iterator.
        let err: Result<(), &'static str> = Err("iteration failed");
        assert!(matches!(
            classify_next_item(err, MAX_DIRECT_ENTRIES),
            Err(ListDirectoryOutcome::Unlistable)
        ));
        // A proven 257th entry settles TooLarge...
        let ok: Result<(), &'static str> = Ok(());
        assert!(matches!(
            classify_next_item(ok, MAX_DIRECT_ENTRIES),
            Err(ListDirectoryOutcome::TooLarge)
        ));
        // ...while the first 256 entries pass through untouched.
        assert!(matches!(classify_next_item(ok, 0), Ok(())));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserved_name_bytes_over_8192_settle_too_large() {
        let temporary = TempDir::new("name-budget");
        // 32 unique 255-byte names sum to 8,160 preserved bytes: inside the 8,192 budget,
        // so the listing succeeds with all 32 entries.
        for index in 0..32 {
            let name = format!("{}{index:02}", "n".repeat(253));
            assert_eq!(name.len(), 255);
            write(temporary.path(), &name, b"x");
        }
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        let outcome = execute(Arc::clone(&set), request_for(r#"{"path":""}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1
                && serde_json::from_str::<serde_json::Value>(content.parts()[0].as_text())
                    .expect("the listing is valid JSON")
                    .as_object()
                    .expect("the listing is an object")
                    .get("entries")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|entries| entries.len() == 32)
        ));

        // 33 unique 255-byte names sum to 8,415 preserved bytes: past the 8,192 budget
        // while still far below 256 entries, so the listing stops immediately with the
        // frozen too-large text.
        let name = format!("{}{}", "m".repeat(254), "!");
        assert_eq!(name.len(), 255);
        write(temporary.path(), &name, b"x");
        let overflow = execute(set, request_for(r#"{"path":""}"#)).await;
        assert_failed_text(&overflow, DIRECTORY_TOO_LARGE_TEXT);
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_and_not_directory_paths_settle_failed_unlistable() {
        let temporary = TempDir::new("missing");
        write(temporary.path(), "real.txt", b"x");
        create_dir(temporary.path(), "docs");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());

        // Missing paths are authorized (they are normal cwd-relative targets inside the
        // root) and fail at the capability open; a regular file opens as a non-directory
        // and is rejected.  Both settle Completed + Failed, never a denial.
        for arguments in [
            r#"{"path":"missing"}"#,
            r#"{"path":"deep/missing"}"#,
            r#"{"path":"real.txt"}"#,
        ] {
            let outcome = execute(Arc::clone(&set), request_for(arguments)).await;
            assert_failed_text(&outcome, DIRECTORY_UNLISTABLE_TEXT);
        }
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn non_utf8_entry_names_settle_failed_unsupported() {
        use std::os::unix::ffi::OsStringExt;

        let temporary = TempDir::new("non-utf8");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        // A directory entry whose raw name bytes are not valid UTF-8.  Some filesystems
        // (for example macOS APFS, which enforces UTF-8 normalization) reject such a name
        // at creation with EILSEQ: on those platforms the fixture cannot exist, so the
        // test proves the handling on filesystems that allow raw non-UTF-8 names (for
        // example Linux ext4) and is skipped where the platform forbids the fixture.
        let bad_name = std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'x']);
        let bad_path = root.join(&bad_name);
        if std::fs::write(&bad_path, b"x").is_err() {
            return;
        }
        write(&root, "ok.txt", b"x");

        let task_context = initialized_context().await;
        let resolver =
            WorkspaceResolver::new_with_source_grants_for_test(task_context.clone(), false, false);
        let workspace_context = resolve_context(resolver, workspace_spec(&root, "")).await;
        let set = build_tool_set(workspace_context, task_context.clone());

        let outcome = execute(set, request_for(r#"{"path":""}"#)).await;
        assert_failed_text(&outcome, DIRECTORY_UNSUPPORTED_NAME_TEXT);
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn symlink_entries_classify_as_symlink_without_following() {
        let temporary = TempDir::new("symlinks");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::create_dir_all(&outside).expect("the outside directory is creatable");
        std::fs::write(root.join("file.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("file.txt", root.join("inner-link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape-link")).unwrap();

        let task_context = initialized_context().await;
        let resolver =
            WorkspaceResolver::new_with_source_grants_for_test(task_context.clone(), false, false);
        let workspace_context = resolve_context(resolver, workspace_spec(&root, "")).await;
        let set = build_tool_set(workspace_context, task_context.clone());

        // Entry symlinks are classified by their own file type (lstat semantics): never
        // followed, so even the escape link is reported as `symlink`, not an error, and
        // its outside target is never opened.
        let outcome = execute(set, request_for(r#"{"path":""}"#)).await;
        assert_exact_listing(
            &outcome,
            r#"{"entries":[{"name":"escape-link","type":"symlink"},{"name":"file.txt","type":"file"},{"name":"inner-link","type":"symlink"}]}"#,
        );
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn special_entries_classify_as_other() {
        let temporary = TempDir::new("other");
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
        write(&root, "a.txt", b"x");

        let task_context = initialized_context().await;
        let resolver =
            WorkspaceResolver::new_with_source_grants_for_test(task_context.clone(), false, false);
        let workspace_context = resolve_context(resolver, workspace_spec(&root, "")).await;
        let set = build_tool_set(workspace_context, task_context.clone());

        // A FIFO is neither file, directory, nor symlink: it classifies as `other` from
        // the entry's own file type without ever opening it.
        let outcome = execute(set, request_for(r#"{"path":""}"#)).await;
        assert_exact_listing(
            &outcome,
            r#"{"entries":[{"name":"a.txt","type":"file"},{"name":"gate.fifo","type":"other"}]}"#,
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_scheduling_settles_the_exact_cancelled_text() {
        let temporary = TempDir::new("cancel-before");
        create_dir(temporary.path(), "real");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());
        // A missing path: any scheduled listing would settle Completed + Failed unlistable,
        // so the Cancelled outcome below proves no blocking job was ever created (and
        // hence zero capability opens).
        let request = request_for(r#"{"path":"missing"}"#);

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
                && content.parts()[0].as_text() == DIRECTORY_CANCELLED_TEXT
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_the_job_is_scheduled_keeps_awaiting_it_and_preserves_the_result() {
        // A known real directory fixture whose truthful result is a one-part success: once
        // the tracked blocking job exists, a later cancellation must keep awaiting the
        // exact same job and preserve that result, never rewrite it to Cancelled.
        let temporary = TempDir::new("cancel-after-schedule");
        write(temporary.path(), "real.txt", b"x");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        let set = build_tool_set(workspace, task_context.clone());
        let request = request_for(r#"{"path":""}"#);

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
        assert_exact_listing(
            &outcome,
            r#"{"entries":[{"name":"real.txt","type":"file"}]}"#,
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tracked_runtime_failure_after_start_settles_abandoned_runtime_failure() {
        let temporary = TempDir::new("runtime-failure");
        create_dir(temporary.path(), "real");
        let task_context = initialized_context().await;
        let workspace = granted_context(&task_context, temporary.path()).await;
        // The one-shot seam: the next admitted blocking job joins as an immediate worker
        // cancellation without ever running its operation closure, so the I/O outcome is
        // unknown and must settle Abandoned RuntimeFailure.  Armed after the workspace
        // resolution (which itself admits a tracked blocking job), so it targets the
        // listing.
        task_context.inject_next_blocking_job_join_failure();
        let set = build_tool_set(workspace, task_context.clone());

        let outcome = execute(set, request_for(r#"{"path":""}"#)).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            } if item_id == ITEM_ID.parse().unwrap()
                && tool_call_id == "call_list".parse().unwrap()
        ));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_tool_name_plans_none_and_listings_never_construct_a_start_factory() {
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
        // A valid list_directory request still plans Execute through the same set.
        assert_plans_execute(&set, &request_for(r#"{"path":"f.txt"}"#));
    }
}
