#![allow(
    dead_code,
    reason = "M6.1 workspace resolution is a crate-private seam awaiting Session publication"
)]

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::future::Future;
use std::io::{Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use tokio::sync::Notify;

use cap_primitives::fs::FollowSymlinks;
use cap_std::fs::Dir as CapabilityDir;
use thiserror::Error;

use crate::runtime_task::{RuntimeTaskContext, RuntimeTaskError};
use crate::wire::lexical::{LexicalError, validate_stable_symbolic_key};
use crate::wire::{
    CanonicalFileUri, FileUriFamily, ProtocolLimits, SessionId, WorkspaceRelativePath,
    WorkspaceRevision,
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkspaceRootKeyError {
    #[error("workspace root key must be 1..=64 bytes")]
    InvalidLength,
    #[error("workspace root key violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRootKey(Box<str>);

impl WorkspaceRootKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for WorkspaceRootKey {
    type Err = WorkspaceRootKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 64, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => WorkspaceRootKeyError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                WorkspaceRootKeyError::InvalidGrammar
            }
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for WorkspaceRootKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for WorkspaceRootKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RequestedFilesystemAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceSourcePolicy {
    prompt: bool,
    skill: bool,
}

impl WorkspaceSourcePolicy {
    pub const fn new(prompt: bool, skill: bool) -> Self {
        Self { prompt, skill }
    }

    pub const fn prompt(self) -> bool {
        self.prompt
    }

    pub const fn skill(self) -> bool {
        self.skill
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct WorkspaceRootInput {
    key: WorkspaceRootKey,
    path: CanonicalFileUri,
    requested_access: RequestedFilesystemAccess,
    sources: WorkspaceSourcePolicy,
}

impl WorkspaceRootInput {
    pub fn new(
        key: WorkspaceRootKey,
        path: CanonicalFileUri,
        requested_access: RequestedFilesystemAccess,
        sources: WorkspaceSourcePolicy,
    ) -> Self {
        Self {
            key,
            path,
            requested_access,
            sources,
        }
    }

    pub fn key(&self) -> &WorkspaceRootKey {
        &self.key
    }

    pub fn path(&self) -> &CanonicalFileUri {
        &self.path
    }

    pub const fn requested_access(&self) -> RequestedFilesystemAccess {
        self.requested_access
    }

    pub const fn sources(&self) -> WorkspaceSourcePolicy {
        self.sources
    }
}

impl fmt::Debug for WorkspaceRootInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceRootInput")
            .field("key", &self.key)
            .field("path_family", &self.path.family())
            .field("requested_access", &self.requested_access)
            .field("sources", &self.sources)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct WorkspaceCwdSpec {
    root: WorkspaceRootKey,
    relative_path: WorkspaceRelativePath,
}

impl fmt::Debug for WorkspaceCwdSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceCwdSpec")
            .field("root", &self.root)
            .field("is_workspace_root", &self.relative_path.is_root())
            .finish()
    }
}

impl WorkspaceCwdSpec {
    pub fn new(root: WorkspaceRootKey, relative_path: WorkspaceRelativePath) -> Self {
        Self {
            root,
            relative_path,
        }
    }

    pub fn root(&self) -> &WorkspaceRootKey {
        &self.root
    }

    pub fn relative_path(&self) -> &WorkspaceRelativePath {
        &self.relative_path
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkspaceInputError {
    #[error("workspace input has too many roots")]
    TooManyRoots,
    #[error("workspace input contains a duplicate root key")]
    DuplicateRootKey,
    #[error("workspace input contains a duplicate root URI")]
    DuplicateRootUri,
    #[error("workspace cwd references an unknown root")]
    UnknownCwdRoot,
    #[error("workspace absolute path URI exceeds its selected limit")]
    AbsolutePathTooLong,
    #[error("workspace relative path exceeds its selected byte limit")]
    RelativePathTooLong,
    #[error("workspace relative path exceeds its selected segment limit")]
    TooManyRelativePathSegments,
}

#[derive(Clone, Eq, PartialEq)]
pub struct WorkspaceDefinitionInput {
    primary_root: WorkspaceRootInput,
    additional_roots: Vec<WorkspaceRootInput>,
    cwd: WorkspaceCwdSpec,
}

impl WorkspaceDefinitionInput {
    pub fn new(
        primary_root: WorkspaceRootInput,
        additional_roots: Vec<WorkspaceRootInput>,
        cwd: WorkspaceCwdSpec,
    ) -> Result<Self, WorkspaceInputError> {
        Self::new_with_limits(primary_root, additional_roots, cwd, ProtocolLimits::v1_0())
    }

    pub(crate) fn new_with_limits(
        primary_root: WorkspaceRootInput,
        additional_roots: Vec<WorkspaceRootInput>,
        cwd: WorkspaceCwdSpec,
        limits: ProtocolLimits,
    ) -> Result<Self, WorkspaceInputError> {
        let root_count = additional_roots.len().saturating_add(1);
        if root_count > usize::from(limits.workspace.max_workspace_roots) {
            return Err(WorkspaceInputError::TooManyRoots);
        }

        let mut keys = BTreeSet::new();
        let mut uris = BTreeSet::new();
        for root in std::iter::once(&primary_root).chain(&additional_roots) {
            if root.path.as_str().len()
                > usize::try_from(limits.workspace.max_absolute_path_uri_bytes)
                    .unwrap_or(usize::MAX)
            {
                return Err(WorkspaceInputError::AbsolutePathTooLong);
            }
            if !keys.insert(root.key.clone()) {
                return Err(WorkspaceInputError::DuplicateRootKey);
            }
            if !uris.insert(root.path.as_str()) {
                return Err(WorkspaceInputError::DuplicateRootUri);
            }
        }

        let relative = cwd.relative_path.as_str();
        if relative.len()
            > usize::try_from(limits.workspace.max_relative_path_bytes).unwrap_or(usize::MAX)
        {
            return Err(WorkspaceInputError::RelativePathTooLong);
        }
        let segment_count = if relative.is_empty() {
            0
        } else {
            relative.split('/').count()
        };
        if segment_count > usize::from(limits.workspace.max_relative_path_segments) {
            return Err(WorkspaceInputError::TooManyRelativePathSegments);
        }
        if !keys.contains(&cwd.root) {
            return Err(WorkspaceInputError::UnknownCwdRoot);
        }

        Ok(Self {
            primary_root,
            additional_roots,
            cwd,
        })
    }

    pub fn primary_root(&self) -> &WorkspaceRootInput {
        &self.primary_root
    }

    pub fn additional_roots(&self) -> &[WorkspaceRootInput] {
        &self.additional_roots
    }

    pub fn cwd(&self) -> &WorkspaceCwdSpec {
        &self.cwd
    }
}

impl fmt::Debug for WorkspaceDefinitionInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceDefinitionInput")
            .field("primary_root", &self.primary_root)
            .field("additional_root_count", &self.additional_roots.len())
            .field("cwd", &self.cwd)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct WorkspaceRootSpec {
    key: WorkspaceRootKey,
    path: PathBuf,
    requested_access: RequestedFilesystemAccess,
    sources: WorkspaceSourcePolicy,
}

impl WorkspaceRootSpec {
    pub fn key(&self) -> &WorkspaceRootKey {
        &self.key
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn requested_access(&self) -> RequestedFilesystemAccess {
        self.requested_access
    }

    pub const fn sources(&self) -> WorkspaceSourcePolicy {
        self.sources
    }
}

impl fmt::Debug for WorkspaceRootSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceRootSpec")
            .field("key_present", &true)
            .field("requested_access", &self.requested_access)
            .field("sources", &self.sources)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Workspace {
    revision: WorkspaceRevision,
    primary_root: WorkspaceRootSpec,
    additional_roots: Vec<WorkspaceRootSpec>,
    cwd: WorkspaceCwdSpec,
}

impl Workspace {
    pub(crate) fn new(
        revision: WorkspaceRevision,
        primary_root: WorkspaceRootSpec,
        additional_roots: Vec<WorkspaceRootSpec>,
        cwd: WorkspaceCwdSpec,
    ) -> Result<Self, WorkspaceConstructionError> {
        let root_count = additional_roots.len().saturating_add(1);
        if root_count > usize::from(ProtocolLimits::v1_0().workspace.max_workspace_roots) {
            return Err(WorkspaceConstructionError::TooManyRoots);
        }

        let mut keys = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for root in std::iter::once(&primary_root).chain(&additional_roots) {
            if !keys.insert(root.key.clone()) {
                return Err(WorkspaceConstructionError::DuplicateRootKey);
            }
            if !paths.insert(root.path.clone()) {
                return Err(WorkspaceConstructionError::DuplicateNativePath);
            }
        }
        if !keys.contains(cwd.root()) {
            return Err(WorkspaceConstructionError::UnknownCwdRoot);
        }

        Ok(Self {
            revision,
            primary_root,
            additional_roots,
            cwd,
        })
    }

    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub fn primary_root(&self) -> &WorkspaceRootSpec {
        &self.primary_root
    }

    pub fn additional_roots(&self) -> &[WorkspaceRootSpec] {
        &self.additional_roots
    }

    pub const fn cwd(&self) -> &WorkspaceCwdSpec {
        &self.cwd
    }

    /// Resets only the Workspace revision while preserving the exact semantic content for a
    /// newly materialized Session Fork. The caller is the lifecycle owner of that new entity;
    /// this is not a general revision mutation seam.
    pub(crate) fn reset_revision_for_fork(&self) -> Self {
        Self::new(
            WorkspaceRevision::new(
                NonZeroU64::new(1).expect("the fixed initial Workspace revision is non-zero"),
            ),
            self.primary_root.clone(),
            self.additional_roots.clone(),
            self.cwd.clone(),
        )
        .expect("a previously valid Workspace remains valid after its Fork revision reset")
    }
}

impl fmt::Debug for Workspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Workspace")
            .field("revision", &self.revision)
            .field("root_count", &self.additional_roots.len().saturating_add(1))
            .field("additional_root_count", &self.additional_roots.len())
            .field("cwd_is_workspace_root", &self.cwd.relative_path().is_root())
            .finish()
    }
}

/// Compares only the durable Workspace definition content. `WorkspaceRevision` is an owner
/// version, not part of the semantic value it versions.
pub(crate) fn workspaces_have_same_semantic_content(first: &Workspace, second: &Workspace) -> bool {
    first.primary_root() == second.primary_root()
        && first.additional_roots() == second.additional_roots()
        && first.cwd() == second.cwd()
}

/// Validates the typed Workspace owner transition: unchanged semantic content retains exactly
/// its revision, while changed content advances it by exactly one.
pub(crate) fn workspace_revision_transition_is_valid(
    previous: &Workspace,
    next: &Workspace,
) -> bool {
    if workspaces_have_same_semantic_content(previous, next) {
        previous.revision() == next.revision()
    } else {
        previous.revision().get().checked_add(1) == Some(next.revision().get())
    }
}

/// The only Workspace revision materialization owned by the Session definition publisher.
/// Runtime/Workspace lowers and validates the candidate before it crosses this seam; the
/// candidate's revision is therefore only a placeholder. Equivalent content retains the
/// authoritative revision, while changed content advances it exactly once.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionDefinitionWorkspaceMaterializationError {
    #[error("Workspace revision is exhausted")]
    RevisionExhausted,
}

pub(crate) fn materialize_session_definition_workspace(
    current: &Workspace,
    candidate: &Workspace,
) -> Result<Workspace, SessionDefinitionWorkspaceMaterializationError> {
    let revision = if workspaces_have_same_semantic_content(current, candidate) {
        current.revision()
    } else {
        current
            .revision()
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(WorkspaceRevision::new)
            .ok_or(SessionDefinitionWorkspaceMaterializationError::RevisionExhausted)?
    };
    Ok(Workspace::new(
        revision,
        candidate.primary_root().clone(),
        candidate.additional_roots().to_vec(),
        candidate.cwd().clone(),
    )
    .expect(
        "a valid Workspace candidate remains valid when reconstructed with its authoritative revision",
    ))
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkspaceInputLoweringError {
    #[error("workspace input uses an unsupported host path family")]
    UnsupportedHostFamily,
    #[error("workspace native path is not losslessly representable")]
    NativePathNotLossless,
    #[error("workspace native path is invalid")]
    InvalidNativePath,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum WorkspaceConstructionError {
    #[error("workspace has too many roots")]
    TooManyRoots,
    #[error("workspace contains a duplicate root key")]
    DuplicateRootKey,
    #[error("workspace contains a duplicate native path")]
    DuplicateNativePath,
    #[error("workspace cwd references an unknown root")]
    UnknownCwdRoot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspacePathTarget {
    Posix,
    Windows,
}

impl WorkspacePathTarget {
    #[allow(
        dead_code,
        reason = "future runtime lowering selects the current host target"
    )]
    pub(crate) const fn current() -> Self {
        if cfg!(target_family = "windows") {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

#[allow(
    dead_code,
    reason = "future runtime command application uses this Workspace lowering seam"
)]
pub(crate) fn lower_workspace(
    input: WorkspaceDefinitionInput,
    revision: WorkspaceRevision,
    target: WorkspacePathTarget,
) -> Result<Workspace, WorkspaceInputLoweringError> {
    let WorkspaceDefinitionInput {
        primary_root,
        additional_roots,
        cwd,
    } = input;
    let primary_root = lower_root(primary_root, target)?;
    let additional_roots = additional_roots
        .into_iter()
        .map(|root| lower_root(root, target))
        .collect::<Result<Vec<_>, _>>()?;

    Workspace::new(revision, primary_root, additional_roots, cwd)
        .map_err(|_| WorkspaceInputLoweringError::InvalidNativePath)
}

#[allow(
    dead_code,
    reason = "future Store encode uses this exact Workspace URI emission seam"
)]
pub(crate) fn uri_from_spec(
    spec: &WorkspaceRootSpec,
    target: WorkspacePathTarget,
) -> Result<CanonicalFileUri, WorkspaceInputLoweringError> {
    checked_native_uri(&spec.path, target)
}

#[allow(
    dead_code,
    reason = "future runtime command application uses this Workspace lowering helper"
)]
fn lower_root(
    root: WorkspaceRootInput,
    target: WorkspacePathTarget,
) -> Result<WorkspaceRootSpec, WorkspaceInputLoweringError> {
    Ok(WorkspaceRootSpec {
        key: root.key,
        path: lower_uri(&root.path, target)?,
        requested_access: root.requested_access,
        sources: root.sources,
    })
}

#[allow(
    dead_code,
    reason = "future Workspace Store and command paths use this checked lowering helper"
)]
fn lower_uri(
    uri: &CanonicalFileUri,
    target: WorkspacePathTarget,
) -> Result<PathBuf, WorkspaceInputLoweringError> {
    match (target, uri.family()) {
        (WorkspacePathTarget::Posix, FileUriFamily::Posix) => Ok(PathBuf::from(uri.decoded_path())),
        (WorkspacePathTarget::Windows, FileUriFamily::Drive) => {
            Ok(PathBuf::from(uri.decoded_path().replace('/', "\\")))
        }
        (WorkspacePathTarget::Windows, FileUriFamily::Unc) => {
            let authority = uri
                .authority()
                .ok_or(WorkspaceInputLoweringError::InvalidNativePath)?;
            Ok(PathBuf::from(format!(
                "\\\\{authority}\\{}",
                uri.decoded_path().replace('/', "\\")
            )))
        }
        _ => Err(WorkspaceInputLoweringError::UnsupportedHostFamily),
    }
}

#[allow(
    dead_code,
    reason = "future Workspace Store paths use this checked native URI helper"
)]
fn checked_native_uri(
    path: &Path,
    target: WorkspacePathTarget,
) -> Result<CanonicalFileUri, WorkspaceInputLoweringError> {
    let path = path
        .to_str()
        .ok_or(WorkspaceInputLoweringError::NativePathNotLossless)?;
    let uri = uri_from_native_path(path, target)?;
    if lower_uri(&uri, target)?.as_path() != Path::new(path) {
        return Err(WorkspaceInputLoweringError::InvalidNativePath);
    }
    Ok(uri)
}

#[allow(
    dead_code,
    reason = "future Workspace Store paths use this native URI helper"
)]
fn uri_from_native_path(
    path: &str,
    target: WorkspacePathTarget,
) -> Result<CanonicalFileUri, WorkspaceInputLoweringError> {
    let uri = match target {
        WorkspacePathTarget::Posix => {
            if !path.starts_with('/') || path.contains('\\') {
                return Err(WorkspaceInputLoweringError::InvalidNativePath);
            }
            CanonicalFileUri::from_decoded_parts(FileUriFamily::Posix, None, path)
        }
        WorkspacePathTarget::Windows => windows_native_uri(path),
    };
    uri.map_err(|_| WorkspaceInputLoweringError::InvalidNativePath)
}

#[allow(
    dead_code,
    reason = "future Workspace Store paths use this deterministic Windows helper"
)]
fn windows_native_uri(path: &str) -> Result<CanonicalFileUri, crate::wire::PathWireError> {
    if path.contains('/') {
        return Err(crate::wire::PathWireError::InvalidPath);
    }
    if let Some(unc) = path.strip_prefix("\\\\") {
        let (authority, native_path) = unc
            .split_once('\\')
            .ok_or(crate::wire::PathWireError::InvalidPath)?;
        return CanonicalFileUri::from_decoded_parts(
            FileUriFamily::Unc,
            Some(authority),
            &native_path.replace('\\', "/"),
        );
    }

    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_uppercase() || bytes[1] != b':' || bytes[2] != b'\\' {
        return Err(crate::wire::PathWireError::InvalidPath);
    }
    let decoded_path = format!("{}/{}", &path[..2], path[3..].replace('\\', "/"));
    CanonicalFileUri::from_decoded_parts(FileUriFamily::Drive, None, &decoded_path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRootSummaryView {
    key: WorkspaceRootKey,
    requested_access: RequestedFilesystemAccess,
    sources: WorkspaceSourcePolicy,
}

impl WorkspaceRootSummaryView {
    pub const fn new(
        key: WorkspaceRootKey,
        requested_access: RequestedFilesystemAccess,
        sources: WorkspaceSourcePolicy,
    ) -> Self {
        Self {
            key,
            requested_access,
            sources,
        }
    }

    pub const fn key(&self) -> &WorkspaceRootKey {
        &self.key
    }

    pub const fn requested_access(&self) -> RequestedFilesystemAccess {
        self.requested_access
    }

    pub const fn sources(&self) -> WorkspaceSourcePolicy {
        self.sources
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkspaceSummaryError {
    #[error("workspace summary has too many roots")]
    TooManyRoots,
    #[error("workspace summary contains a duplicate root key")]
    DuplicateRootKey,
    #[error("workspace summary cwd references an unknown root")]
    UnknownCwdRoot,
    #[error("workspace summary relative path exceeds its selected byte limit")]
    RelativePathTooLong,
    #[error("workspace summary relative path exceeds its selected segment limit")]
    TooManyRelativePathSegments,
}

#[derive(Clone, Eq, PartialEq)]
pub struct WorkspaceDefinitionSummaryView {
    roots: Vec<WorkspaceRootSummaryView>,
    cwd: WorkspaceCwdSpec,
}

impl WorkspaceDefinitionSummaryView {
    pub fn new(
        primary_root: WorkspaceRootSummaryView,
        additional_roots: Vec<WorkspaceRootSummaryView>,
        cwd: WorkspaceCwdSpec,
    ) -> Result<Self, WorkspaceSummaryError> {
        Self::new_with_limits(primary_root, additional_roots, cwd, ProtocolLimits::v1_0())
    }

    pub(crate) fn new_with_limits(
        primary_root: WorkspaceRootSummaryView,
        additional_roots: Vec<WorkspaceRootSummaryView>,
        cwd: WorkspaceCwdSpec,
        limits: ProtocolLimits,
    ) -> Result<Self, WorkspaceSummaryError> {
        let mut roots = Vec::with_capacity(additional_roots.len().saturating_add(1));
        roots.push(primary_root);
        roots.extend(additional_roots);
        if roots.len() > usize::from(limits.workspace.max_workspace_roots) {
            return Err(WorkspaceSummaryError::TooManyRoots);
        }
        let mut keys = BTreeSet::new();
        for root in &roots {
            if !keys.insert(root.key.clone()) {
                return Err(WorkspaceSummaryError::DuplicateRootKey);
            }
        }
        if !keys.contains(cwd.root()) {
            return Err(WorkspaceSummaryError::UnknownCwdRoot);
        }
        let relative = cwd.relative_path().as_str();
        if relative.len()
            > usize::try_from(limits.workspace.max_relative_path_bytes).unwrap_or(usize::MAX)
        {
            return Err(WorkspaceSummaryError::RelativePathTooLong);
        }
        let segment_count = if relative.is_empty() {
            0
        } else {
            relative.split('/').count()
        };
        if segment_count > usize::from(limits.workspace.max_relative_path_segments) {
            return Err(WorkspaceSummaryError::TooManyRelativePathSegments);
        }
        Ok(Self { roots, cwd })
    }

    pub fn roots(&self) -> &[WorkspaceRootSummaryView] {
        &self.roots
    }

    pub fn primary_root(&self) -> &WorkspaceRootSummaryView {
        &self.roots[0]
    }

    pub fn additional_roots(&self) -> &[WorkspaceRootSummaryView] {
        &self.roots[1..]
    }

    pub const fn cwd(&self) -> &WorkspaceCwdSpec {
        &self.cwd
    }
}

impl fmt::Debug for WorkspaceDefinitionSummaryView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceDefinitionSummaryView")
            .field("roots", &self.roots)
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// A canonical, existing directory path owned by the Workspace resolver.
///
/// The constructor is deliberately private.  A value can only be produced by a
/// `WorkspacePathAdapter` after it has performed its platform-specific checks.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CanonicalWorkspacePath(PathBuf);

impl CanonicalWorkspacePath {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for CanonicalWorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CanonicalWorkspacePath { .. }")
    }
}

impl fmt::Display for CanonicalWorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<canonical-workspace-path>")
    }
}

/// Exact identity of an opened workspace directory, captured with the safe
/// `same_file` handle so candidate equality and revalidation detect root
/// replacement even when the canonical path text is unchanged. The handle is
/// kept behind an `Arc` so the identity stays cloneable without ever duplicating
/// the open file it is bound to.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct WorkspaceRootIdentity(Arc<same_file::Handle>);

impl WorkspaceRootIdentity {
    fn from_file(file: std::fs::File) -> Result<Self, WorkspacePathError> {
        let handle = same_file::Handle::from_file(file).map_err(map_path_io_error)?;
        Ok(Self(Arc::new(handle)))
    }

    fn matches(&self, handle: &same_file::Handle) -> bool {
        &*self.0 == handle
    }
}

impl fmt::Debug for WorkspaceRootIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceRootIdentity { .. }")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum WorkspaceRootRole {
    Primary,
    Additional,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum WorkspaceTrustLevel {
    Trusted,
    Restricted,
    Untrusted,
}

/// A non-zero revision supplied by the current authority decision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct WorkspaceTrustRevision(NonZeroU64);

impl WorkspaceTrustRevision {
    pub(crate) const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WorkspaceRootTrust {
    level: WorkspaceTrustLevel,
    revision: WorkspaceTrustRevision,
}

impl WorkspaceRootTrust {
    pub(crate) const fn new(level: WorkspaceTrustLevel, revision: WorkspaceTrustRevision) -> Self {
        Self { level, revision }
    }

    pub(crate) const fn level(self) -> WorkspaceTrustLevel {
        self.level
    }

    pub(crate) const fn revision(self) -> WorkspaceTrustRevision {
        self.revision
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum WorkspaceFilesystemGrant {
    None,
    ReadOnly,
    ReadWrite,
}

impl WorkspaceFilesystemGrant {
    const fn is_readable(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
enum WorkspacePathError {
    #[error("workspace root is unavailable")]
    Unavailable,
    #[error("workspace root is not a directory")]
    NotDirectory,
    #[error("workspace path canonicalization failed")]
    CanonicalizationFailed,
}

/// The path seam is synchronous because the resolver owns its complete path phase
/// on one owner-tracked blocking worker.
///
/// `open_directory` is the only ambient-authority use on the Workspace path: it
/// opens a declared root capability and captures its exact identity in one trusted
/// step. Everything after this phase resolves strictly capability-relative from the
/// captured `Dir`; model and Tool paths never reopen a root by ambient path.
///
/// `canonicalize_directory_in_root` resolves the cwd inside the exact root
/// capability captured by `open_directory`: production opens the cwd
/// capability-relative through that `Dir` and proves its safe same-file identity
/// against the ambient canonical path, so a root rename/replacement between root
/// capture and cwd proof fails closed instead of binding the cwd to a
/// replacement. Deterministic test adapters may preserve their pure path-text
/// mapping.
trait WorkspacePathAdapter: Send + Sync {
    fn canonicalize_directory(
        &self,
        path: &Path,
    ) -> Result<CanonicalWorkspacePath, WorkspacePathError>;

    fn open_directory(&self, path: &Path) -> Result<OpenedWorkspaceRoot, WorkspacePathError>;

    /// Resolve the cwd directory bound to the exact `opened_root` capability.
    ///
    /// `relative` is the declared cwd path inside the root (empty when the cwd is
    /// the root itself); `ambient_path` is the declared root path joined with the
    /// relative path and supplies only the canonical path text claim. Production
    /// opens the cwd capability-relative through `opened_root.dir` and proves the
    /// opened handle is the same file as the ambient canonical path.
    fn canonicalize_directory_in_root(
        &self,
        opened_root: &OpenedWorkspaceRoot,
        relative: &str,
        ambient_path: &Path,
    ) -> Result<CanonicalWorkspacePath, WorkspacePathError>;
}

/// A declared root resolved on the owner-tracked path phase: the canonical path,
/// the exact safe identity of the opened directory, and the open capability bound
/// to that exact directory. Capability and identity are captured together so
/// candidate equality can detect root replacement, not only equal path text.
#[derive(Clone)]
struct OpenedWorkspaceRoot {
    canonical: CanonicalWorkspacePath,
    identity: WorkspaceRootIdentity,
    dir: Arc<CapabilityDir>,
}

struct LocalWorkspacePathAdapter;

impl WorkspacePathAdapter for LocalWorkspacePathAdapter {
    fn canonicalize_directory(
        &self,
        path: &Path,
    ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
        let canonical = std::fs::canonicalize(path).map_err(map_path_io_error)?;
        let metadata = std::fs::metadata(&canonical).map_err(map_path_io_error)?;
        if !metadata.is_dir() {
            return Err(WorkspacePathError::NotDirectory);
        }
        Ok(CanonicalWorkspacePath::new(canonical))
    }

    fn open_directory(&self, path: &Path) -> Result<OpenedWorkspaceRoot, WorkspacePathError> {
        // Canonicalize first, then open the *declared* path as a capability and prove
        // the opened directory is exactly the canonical directory. If a replacement or
        // race makes them differ, resolution fails instead of silently binding a
        // different root to the workspace.
        let canonical = self.canonicalize_directory(path)?;
        let dir = CapabilityDir::open_ambient_dir(path, cap_std::ambient_authority())
            .map_err(map_path_io_error)?;
        let identity = WorkspaceRootIdentity::from_file(
            dir.try_clone().map_err(map_path_io_error)?.into_std_file(),
        )?;
        let canonical_identity =
            same_file::Handle::from_path(canonical.as_path()).map_err(map_path_io_error)?;
        if !identity.matches(&canonical_identity) {
            return Err(WorkspacePathError::CanonicalizationFailed);
        }
        Ok(OpenedWorkspaceRoot {
            canonical,
            identity,
            dir: Arc::new(dir),
        })
    }

    fn canonicalize_directory_in_root(
        &self,
        opened_root: &OpenedWorkspaceRoot,
        relative: &str,
        ambient_path: &Path,
    ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
        // The ambient canonical text is only a claim about the cwd's location.
        // The cwd is opened capability-relative through the exact captured root
        // and its safe identity is compared with the ambient canonical path
        // identity. A root rename/replacement between root capture and this proof
        // leaves the capability bound to the old directory while the ambient path
        // names the replacement, so the comparison fails closed. A symlink cwd
        // escaping the root fails through cap-std's sandboxed open, never via the
        // ambient canonical text.
        let canonical = self.canonicalize_directory(ambient_path)?;
        let cwd_dir = if relative.is_empty() {
            Arc::clone(&opened_root.dir)
        } else {
            Arc::new(
                opened_root
                    .dir
                    .open_dir(relative)
                    .map_err(map_path_io_error)?,
            )
        };
        let cwd_identity = WorkspaceRootIdentity::from_file(
            cwd_dir
                .try_clone()
                .map_err(map_path_io_error)?
                .into_std_file(),
        )?;
        let canonical_identity =
            same_file::Handle::from_path(canonical.as_path()).map_err(map_path_io_error)?;
        if !cwd_identity.matches(&canonical_identity) {
            return Err(WorkspacePathError::CanonicalizationFailed);
        }
        Ok(canonical)
    }
}

fn map_path_io_error(error: std::io::Error) -> WorkspacePathError {
    match error.kind() {
        std::io::ErrorKind::NotFound => WorkspacePathError::Unavailable,
        std::io::ErrorKind::NotADirectory => WorkspacePathError::NotDirectory,
        _ => WorkspacePathError::CanonicalizationFailed,
    }
}

#[derive(Clone, Eq, PartialEq)]
struct WorkspaceAuthorityRootRequest {
    key: WorkspaceRootKey,
    role: WorkspaceRootRole,
    canonical_path: CanonicalWorkspacePath,
    requested_access: RequestedFilesystemAccess,
    sources: WorkspaceSourcePolicy,
}

impl WorkspaceAuthorityRootRequest {
    fn new(
        key: WorkspaceRootKey,
        role: WorkspaceRootRole,
        canonical_path: CanonicalWorkspacePath,
        requested_access: RequestedFilesystemAccess,
        sources: WorkspaceSourcePolicy,
    ) -> Self {
        Self {
            key,
            role,
            canonical_path,
            requested_access,
            sources,
        }
    }

    pub(crate) fn key(&self) -> &WorkspaceRootKey {
        &self.key
    }

    pub(crate) const fn role(&self) -> WorkspaceRootRole {
        self.role
    }

    pub(crate) fn canonical_path(&self) -> &CanonicalWorkspacePath {
        &self.canonical_path
    }

    pub(crate) const fn requested_access(&self) -> RequestedFilesystemAccess {
        self.requested_access
    }

    pub(crate) const fn sources(&self) -> WorkspaceSourcePolicy {
        self.sources
    }
}

impl fmt::Debug for WorkspaceAuthorityRootRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAuthorityRootRequest")
            .field("role", &self.role)
            .field("requested_access", &self.requested_access)
            .field("sources", &self.sources)
            .finish()
    }
}

#[derive(Clone)]
struct WorkspaceAuthorityRequest {
    session_id: SessionId,
    roots: Vec<WorkspaceAuthorityRootRequest>,
}

impl WorkspaceAuthorityRequest {
    fn new(session_id: SessionId, roots: Vec<WorkspaceAuthorityRootRequest>) -> Self {
        Self { session_id, roots }
    }

    fn session_id(&self) -> SessionId {
        self.session_id
    }

    fn roots(&self) -> &[WorkspaceAuthorityRootRequest] {
        &self.roots
    }
}

impl fmt::Debug for WorkspaceAuthorityRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAuthorityRequest")
            .field("root_count", &self.roots.len())
            .finish()
    }
}

#[derive(Clone)]
struct WorkspaceAuthorityRootDecision {
    request: WorkspaceAuthorityRootRequest,
    trust: WorkspaceRootTrust,
    filesystem_ceiling: WorkspaceFilesystemGrant,
    prompt_source_ceiling: bool,
    skill_source_ceiling: bool,
}

impl WorkspaceAuthorityRootDecision {
    fn new(
        request: WorkspaceAuthorityRootRequest,
        trust: WorkspaceRootTrust,
        filesystem_ceiling: WorkspaceFilesystemGrant,
        prompt_source_ceiling: bool,
        skill_source_ceiling: bool,
    ) -> Self {
        Self {
            request,
            trust,
            filesystem_ceiling,
            prompt_source_ceiling,
            skill_source_ceiling,
        }
    }
}

impl fmt::Debug for WorkspaceAuthorityRootDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAuthorityRootDecision")
            .field("role", &self.request.role)
            .field("trust", &self.trust)
            .field("filesystem_ceiling", &self.filesystem_ceiling)
            .field("prompt_source_ceiling", &self.prompt_source_ceiling)
            .field("skill_source_ceiling", &self.skill_source_ceiling)
            .finish()
    }
}

#[derive(Clone)]
struct WorkspaceAuthorityDecision {
    roots: Vec<WorkspaceAuthorityRootDecision>,
}

impl WorkspaceAuthorityDecision {
    fn new(roots: Vec<WorkspaceAuthorityRootDecision>) -> Self {
        Self { roots }
    }

    fn roots(&self) -> &[WorkspaceAuthorityRootDecision] {
        &self.roots
    }
}

impl fmt::Debug for WorkspaceAuthorityDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAuthorityDecision")
            .field("root_count", &self.roots.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
enum WorkspaceAuthorityError {
    #[error("workspace authority is unavailable")]
    Unavailable,
    #[error("workspace authority denied the request")]
    Denied,
}

type WorkspaceAuthorityFuture = Pin<
    Box<dyn Future<Output = Result<WorkspaceAuthorityDecision, WorkspaceAuthorityError>> + Send>,
>;

trait WorkspaceAuthority: Send + Sync {
    fn authorize(&self, request: WorkspaceAuthorityRequest) -> WorkspaceAuthorityFuture;
}

/// The production M6.1 policy is deliberately fail-closed.  It reports the current
/// roots without treating them as trusted and grants neither filesystem nor source access.
struct RestrictedWorkspaceAuthority;

impl WorkspaceAuthority for RestrictedWorkspaceAuthority {
    fn authorize(&self, request: WorkspaceAuthorityRequest) -> WorkspaceAuthorityFuture {
        Box::pin(async move {
            let revision = WorkspaceTrustRevision::new(
                NonZeroU64::new(1).expect("the restricted authority revision is non-zero"),
            );
            let roots = request
                .roots()
                .iter()
                .map(|root| {
                    WorkspaceAuthorityRootDecision::new(
                        root.clone(),
                        WorkspaceRootTrust::new(WorkspaceTrustLevel::Untrusted, revision),
                        WorkspaceFilesystemGrant::None,
                        false,
                        false,
                    )
                })
                .collect();
            Ok(WorkspaceAuthorityDecision::new(roots))
        })
    }
}

/// The process-local revocation register that makes the production filesystem opt-ins
/// (read-only or read-write) revocable by the existing host Workspace authority
/// invalidation seam.  It is one safe synchronized set of revoked `SessionId`s shared
/// (through clones) by the filesystem authority installed in the resolver and the Runtime
/// owner seam: a revoked Session can never receive a filesystem grant again for this
/// Runtime lifetime.  There is no generic policy store, no trait, no callback, and no
/// public DTO — only this concrete owner-held control.
#[derive(Clone, Default)]
pub(crate) struct WorkspaceFilesystemAccessControl {
    revoked: Arc<Mutex<HashSet<SessionId>>>,
}

/// The established spelling of the same owner-held filesystem control for the existing
/// read opt-in seams; the host invalidation path stays source-compatible while the
/// control itself now covers the write opt-in too.
pub(crate) type WorkspaceReadAccessControl = WorkspaceFilesystemAccessControl;

impl WorkspaceFilesystemAccessControl {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Publishes the permanent filesystem revocation for one Session.  Idempotent: a repeated
    /// revoke inserts the same Session into the same set and never re-grants.  There is no
    /// unrevoke/recovery for this Runtime lifetime.
    pub(crate) fn revoke(&self, session_id: SessionId) {
        self.revoked
            .lock()
            .expect("the process-local revocation set is not poisoned")
            .insert(session_id);
    }

    fn is_revoked(&self, session_id: SessionId) -> bool {
        self.revoked
            .lock()
            .expect("the process-local revocation set is not poisoned")
            .contains(&session_id)
    }
}

/// The production filesystem opt-in policy for trusted host wiring.  It grants an
/// authority ceiling of exactly the configured `ceiling` (`ReadOnly` for the read
/// opt-in, `ReadWrite` for the write opt-in) to every declared root — never beyond
/// it — and keeps Prompt and Skill source ceilings false: filesystem authorization
/// must not silently authorize source discovery.  Trust is `Restricted` (not
/// `Trusted`): the filesystem grant is independent from source trust.  The authority
/// holds the same [`WorkspaceFilesystemAccessControl`] as the Runtime owner seam: once
/// the host revokes one Session, every future resolve/revalidation for that Session
/// grants filesystem `None` (never `AuthorityDenied`, never the ceiling again), which
/// denies the read and write routes together, while Prompt/Skill stay false and trust
/// stays `Restricted`.
struct FilesystemWorkspaceAuthority {
    control: WorkspaceFilesystemAccessControl,
    ceiling: WorkspaceFilesystemGrant,
}

impl WorkspaceAuthority for FilesystemWorkspaceAuthority {
    fn authorize(&self, request: WorkspaceAuthorityRequest) -> WorkspaceAuthorityFuture {
        // The revocation check is synchronous and happens before the future is built, so the
        // decision is exact at authorize time; the resolver never observes a revocation that
        // lands between the check and the returned decision.
        let filesystem_ceiling = if self.control.is_revoked(request.session_id()) {
            WorkspaceFilesystemGrant::None
        } else {
            self.ceiling
        };
        Box::pin(async move {
            let revision = WorkspaceTrustRevision::new(
                NonZeroU64::new(1).expect("the filesystem authority revision is non-zero"),
            );
            let roots = request
                .roots()
                .iter()
                .map(|root| {
                    WorkspaceAuthorityRootDecision::new(
                        root.clone(),
                        WorkspaceRootTrust::new(WorkspaceTrustLevel::Restricted, revision),
                        filesystem_ceiling,
                        false,
                        false,
                    )
                })
                .collect();
            Ok(WorkspaceAuthorityDecision::new(roots))
        })
    }
}

#[cfg(test)]
struct SourceGrantWorkspaceAuthority {
    prompt: Arc<AtomicBool>,
    skill: bool,
}

#[cfg(test)]
impl WorkspaceAuthority for SourceGrantWorkspaceAuthority {
    fn authorize(&self, request: WorkspaceAuthorityRequest) -> WorkspaceAuthorityFuture {
        let prompt = self.prompt.load(Ordering::Acquire);
        let skill = self.skill;
        Box::pin(async move {
            let revision = WorkspaceTrustRevision::new(
                NonZeroU64::new(1).expect("the test authority revision is non-zero"),
            );
            let roots = request
                .roots()
                .iter()
                .map(|root| {
                    WorkspaceAuthorityRootDecision::new(
                        root.clone(),
                        WorkspaceRootTrust::new(WorkspaceTrustLevel::Trusted, revision),
                        requested_filesystem_grant(root.requested_access),
                        prompt,
                        skill,
                    )
                })
                .collect();
            Ok(WorkspaceAuthorityDecision::new(roots))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum WorkspaceResolveError {
    #[error("workspace resolution is closing")]
    Closing,
    #[error("workspace root is unavailable")]
    RootUnavailable,
    #[error("workspace root is not a directory")]
    RootNotDirectory,
    #[error("workspace path canonicalization failed")]
    CanonicalizationFailed,
    #[error("workspace contains a duplicate canonical root")]
    DuplicateRoot,
    #[error("workspace roots overlap")]
    OverlappingRoots,
    #[error("workspace cwd is outside declared roots")]
    CwdOutsideRoots,
    #[error("workspace cwd does not match its declared root")]
    CwdRootMismatch,
    #[error("workspace authority is unavailable")]
    AuthorityUnavailable,
    #[error("workspace authority denied the request")]
    AuthorityDenied,
    #[error("workspace resolver dispatch is unavailable")]
    InternalDispatchUnavailable,
}

fn map_path_error(error: WorkspacePathError) -> WorkspaceResolveError {
    match error {
        WorkspacePathError::Unavailable => WorkspaceResolveError::RootUnavailable,
        WorkspacePathError::NotDirectory => WorkspaceResolveError::RootNotDirectory,
        WorkspacePathError::CanonicalizationFailed => WorkspaceResolveError::CanonicalizationFailed,
    }
}

#[derive(Clone)]
struct DeclaredWorkspaceRoot {
    key: WorkspaceRootKey,
    role: WorkspaceRootRole,
    path: PathBuf,
    requested_access: RequestedFilesystemAccess,
    sources: WorkspaceSourcePolicy,
}

#[derive(Clone)]
struct WorkspacePathPhase {
    opened_roots: Vec<OpenedWorkspaceRoot>,
    canonical_cwd: CanonicalWorkspacePath,
}

fn resolve_path_phase(
    adapter: &dyn WorkspacePathAdapter,
    roots: Vec<DeclaredWorkspaceRoot>,
    cwd_root: WorkspaceRootKey,
    cwd_relative_path: WorkspaceRelativePath,
) -> Result<WorkspacePathPhase, WorkspaceResolveError> {
    let mut opened_roots = Vec::with_capacity(roots.len());
    for root in &roots {
        opened_roots.push(adapter.open_directory(&root.path).map_err(map_path_error)?);
    }

    for (index, opened) in opened_roots.iter().enumerate() {
        if opened_roots[..index]
            .iter()
            .any(|previous| previous.canonical.as_path() == opened.canonical.as_path())
        {
            return Err(WorkspaceResolveError::DuplicateRoot);
        }
    }

    for (index, opened) in opened_roots.iter().enumerate() {
        if opened_roots[..index].iter().any(|previous| {
            previous
                .canonical
                .as_path()
                .starts_with(opened.canonical.as_path())
                || opened
                    .canonical
                    .as_path()
                    .starts_with(previous.canonical.as_path())
        }) {
            return Err(WorkspaceResolveError::OverlappingRoots);
        }
    }

    let cwd_root_index = roots
        .iter()
        .position(|root| root.key == cwd_root)
        .ok_or(WorkspaceResolveError::CwdRootMismatch)?;
    let cwd_native_path = roots[cwd_root_index].path.join(cwd_relative_path.as_str());
    // The cwd proof is bound to the exact captured root capability selected by
    // `cwd_root`, never to matching canonical path text: the cwd is opened
    // capability-relative through that root's `Dir` and proven to be the same
    // file as the ambient canonical path inside the same owner-tracked phase.
    let canonical_cwd = adapter
        .canonicalize_directory_in_root(
            &opened_roots[cwd_root_index],
            cwd_relative_path.as_str(),
            &cwd_native_path,
        )
        .map_err(map_path_error)?;

    let containing_roots = opened_roots
        .iter()
        .enumerate()
        .filter(|(_, opened)| {
            canonical_cwd
                .as_path()
                .starts_with(opened.canonical.as_path())
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if containing_roots.is_empty() {
        return Err(WorkspaceResolveError::CwdOutsideRoots);
    }
    if containing_roots.len() != 1 || roots[containing_roots[0]].key != cwd_root {
        return Err(WorkspaceResolveError::CwdRootMismatch);
    }

    Ok(WorkspacePathPhase {
        opened_roots,
        canonical_cwd,
    })
}

/// One authorized root of a resolved Workspace. Equality deliberately compares the
/// captured safe identity rather than only canonical path text, so candidate
/// revalidation detects root replacement at the same path.
#[derive(Clone)]
pub(crate) struct ResolvedWorkspaceRoot {
    key: WorkspaceRootKey,
    role: WorkspaceRootRole,
    canonical_path: CanonicalWorkspacePath,
    identity: WorkspaceRootIdentity,
    dir: Arc<CapabilityDir>,
    trust: WorkspaceRootTrust,
    filesystem: WorkspaceFilesystemGrant,
    prompt_source: bool,
    skill_source: bool,
}

impl PartialEq for ResolvedWorkspaceRoot {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.role == other.role
            && self.canonical_path == other.canonical_path
            && self.identity == other.identity
            && self.trust == other.trust
            && self.filesystem == other.filesystem
            && self.prompt_source == other.prompt_source
            && self.skill_source == other.skill_source
    }
}

impl Eq for ResolvedWorkspaceRoot {}

impl fmt::Debug for ResolvedWorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedWorkspaceRoot")
            .field("role", &self.role)
            .field("trust", &self.trust)
            .field("filesystem", &self.filesystem)
            .field("prompt_source", &self.prompt_source)
            .field("skill_source", &self.skill_source)
            .finish()
    }
}

fn requested_filesystem_grant(
    requested_access: RequestedFilesystemAccess,
) -> WorkspaceFilesystemGrant {
    match requested_access {
        RequestedFilesystemAccess::ReadOnly => WorkspaceFilesystemGrant::ReadOnly,
        RequestedFilesystemAccess::ReadWrite => WorkspaceFilesystemGrant::ReadWrite,
    }
}

fn intersect_filesystem_grants(
    requested_access: RequestedFilesystemAccess,
    authority_ceiling: WorkspaceFilesystemGrant,
) -> WorkspaceFilesystemGrant {
    match (
        requested_filesystem_grant(requested_access),
        authority_ceiling,
    ) {
        (WorkspaceFilesystemGrant::None, _) | (_, WorkspaceFilesystemGrant::None) => {
            WorkspaceFilesystemGrant::None
        }
        (WorkspaceFilesystemGrant::ReadOnly, WorkspaceFilesystemGrant::ReadOnly)
        | (WorkspaceFilesystemGrant::ReadOnly, WorkspaceFilesystemGrant::ReadWrite)
        | (WorkspaceFilesystemGrant::ReadWrite, WorkspaceFilesystemGrant::ReadOnly) => {
            WorkspaceFilesystemGrant::ReadOnly
        }
        (WorkspaceFilesystemGrant::ReadWrite, WorkspaceFilesystemGrant::ReadWrite) => {
            WorkspaceFilesystemGrant::ReadWrite
        }
    }
}

fn resolve_authority(
    expected_request: &WorkspaceAuthorityRequest,
    decision: WorkspaceAuthorityDecision,
    opened_roots: Vec<OpenedWorkspaceRoot>,
) -> Result<Vec<ResolvedWorkspaceRoot>, WorkspaceResolveError> {
    if decision.roots().len() != expected_request.roots().len()
        || opened_roots.len() != expected_request.roots().len()
    {
        return Err(WorkspaceResolveError::InternalDispatchUnavailable);
    }

    if expected_request
        .roots()
        .iter()
        .zip(decision.roots())
        .any(|(expected, authority)| authority.request != *expected)
    {
        return Err(WorkspaceResolveError::InternalDispatchUnavailable);
    }

    expected_request
        .roots()
        .iter()
        .zip(decision.roots())
        .zip(opened_roots)
        .map(|((root, authority), opened)| {
            let filesystem =
                intersect_filesystem_grants(root.requested_access, authority.filesystem_ceiling);
            Ok(ResolvedWorkspaceRoot {
                key: root.key.clone(),
                role: root.role,
                canonical_path: opened.canonical,
                identity: opened.identity,
                dir: opened.dir,
                trust: authority.trust,
                filesystem,
                prompt_source: root.sources.prompt()
                    && authority.prompt_source_ceiling
                    && filesystem.is_readable(),
                skill_source: root.sources.skill()
                    && authority.skill_source_ceiling
                    && filesystem.is_readable(),
            })
        })
        .collect()
}

fn map_runtime_task_error(
    task_context: &RuntimeTaskContext,
    error: RuntimeTaskError,
) -> WorkspaceResolveError {
    match error {
        RuntimeTaskError::OwnerClosing => WorkspaceResolveError::Closing,
        RuntimeTaskError::WorkerUnavailable => WorkspaceResolveError::InternalDispatchUnavailable,
        RuntimeTaskError::OperationPanicked => {
            task_context.request_closing();
            WorkspaceResolveError::InternalDispatchUnavailable
        }
    }
}

pub(crate) struct WorkspaceResolver {
    task_context: RuntimeTaskContext,
    paths: Arc<dyn WorkspacePathAdapter>,
    authority: Arc<dyn WorkspaceAuthority>,
    #[cfg(test)]
    hooks: Arc<WorkspaceResolverTestHooks>,
}

impl WorkspaceResolver {
    pub(crate) fn new(task_context: RuntimeTaskContext) -> Self {
        Self::new_with_adapters(
            task_context,
            Arc::new(LocalWorkspacePathAdapter),
            Arc::new(RestrictedWorkspaceAuthority),
        )
    }

    /// Production opt-in for trusted host wiring: every declared root receives
    /// an authority ceiling of exactly `ReadOnly` filesystem access. The
    /// requested-access intersection remains authoritative, so `ReadOnly` stays
    /// `ReadOnly` while `ReadWrite` is tightened to `ReadOnly`; `ReadWrite` is
    /// never granted. Prompt/Skill source ceilings stay false, so this opt-in
    /// never silently authorizes source discovery.  The returned
    /// [`WorkspaceReadAccessControl`] is the owner seam: the host publishes a
    /// permanent per-Session filesystem revocation through it, and every later
    /// resolve/revalidation for that Session grants filesystem `None` instead.
    pub(crate) fn new_with_read_access(
        task_context: RuntimeTaskContext,
    ) -> (Self, WorkspaceReadAccessControl) {
        let control = WorkspaceFilesystemAccessControl::new();
        let resolver = Self::new_with_adapters(
            task_context,
            Arc::new(LocalWorkspacePathAdapter),
            Arc::new(FilesystemWorkspaceAuthority {
                control: control.clone(),
                ceiling: WorkspaceFilesystemGrant::ReadOnly,
            }),
        );
        (resolver, control)
    }

    /// Production opt-in for trusted host wiring that may mutate files: every
    /// declared root receives an authority ceiling of exactly `ReadWrite`
    /// filesystem access. The requested-access intersection remains
    /// authoritative, so `ReadOnly` stays `ReadOnly` while a requested
    /// `ReadWrite` can become `ReadWrite`; no root is ever promoted beyond what
    /// its definition requested. Prompt/Skill source ceilings stay false and
    /// trust stays `Restricted`, exactly like the read opt-in. The returned
    /// [`WorkspaceFilesystemAccessControl`] is the same owner seam the read
    /// opt-in exposes (spelled `WorkspaceReadAccessControl` there): the host
    /// publishes a permanent per-Session filesystem revocation through it, and
    /// every later resolve/revalidation for that Session grants filesystem
    /// `None`, which denies the read and write routes together.
    pub(crate) fn new_with_write_access(
        task_context: RuntimeTaskContext,
    ) -> (Self, WorkspaceFilesystemAccessControl) {
        let control = WorkspaceFilesystemAccessControl::new();
        let resolver = Self::new_with_adapters(
            task_context,
            Arc::new(LocalWorkspacePathAdapter),
            Arc::new(FilesystemWorkspaceAuthority {
                control: control.clone(),
                ceiling: WorkspaceFilesystemGrant::ReadWrite,
            }),
        );
        (resolver, control)
    }

    #[cfg(test)]
    pub(crate) fn new_with_source_grants_for_test(
        task_context: RuntimeTaskContext,
        prompt: bool,
        skill: bool,
    ) -> Self {
        Self::new_with_adapters(
            task_context,
            Arc::new(LocalWorkspacePathAdapter),
            Arc::new(SourceGrantWorkspaceAuthority {
                prompt: Arc::new(AtomicBool::new(prompt)),
                skill,
            }),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_mutable_prompt_grant_for_test(
        task_context: RuntimeTaskContext,
        prompt: Arc<AtomicBool>,
    ) -> Self {
        Self::new_with_adapters(
            task_context,
            Arc::new(LocalWorkspacePathAdapter),
            Arc::new(SourceGrantWorkspaceAuthority {
                prompt,
                skill: false,
            }),
        )
    }

    fn new_with_adapters(
        task_context: RuntimeTaskContext,
        paths: Arc<dyn WorkspacePathAdapter>,
        authority: Arc<dyn WorkspaceAuthority>,
    ) -> Self {
        Self {
            task_context,
            paths,
            authority,
            #[cfg(test)]
            hooks: Arc::new(WorkspaceResolverTestHooks::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_hooks(&self) -> WorkspaceResolverTestHooks {
        WorkspaceResolverTestHooks {
            inner: Arc::clone(&self.hooks.inner),
        }
    }

    pub(crate) async fn resolve(
        &self,
        session_id: SessionId,
        workspace: &Workspace,
    ) -> Result<WorkspaceSnapshotCandidate, WorkspaceResolveError> {
        let declared_roots = std::iter::once((&workspace.primary_root, WorkspaceRootRole::Primary))
            .chain(
                workspace
                    .additional_roots
                    .iter()
                    .map(|root| (root, WorkspaceRootRole::Additional)),
            )
            .map(|(root, role)| DeclaredWorkspaceRoot {
                key: root.key.clone(),
                role,
                path: root.path.clone(),
                requested_access: root.requested_access,
                sources: root.sources,
            })
            .collect::<Vec<_>>();
        let cwd_root = workspace.cwd.root().clone();
        let cwd_relative_path = workspace.cwd.relative_path().clone();
        let paths = Arc::clone(&self.paths);
        let roots_for_path = declared_roots.clone();
        let path_job = self.task_context.spawn_blocking_tracked(move || {
            resolve_path_phase(paths.as_ref(), roots_for_path, cwd_root, cwd_relative_path)
        });
        let path_phase = path_job
            .wait()
            .await
            .map_err(|error| map_runtime_task_error(&self.task_context, error))??;

        let request_roots = declared_roots
            .iter()
            .zip(&path_phase.opened_roots)
            .map(|(root, opened)| {
                WorkspaceAuthorityRootRequest::new(
                    root.key.clone(),
                    root.role,
                    opened.canonical.clone(),
                    root.requested_access,
                    root.sources,
                )
            })
            .collect();
        let request = WorkspaceAuthorityRequest::new(session_id, request_roots);
        let expected_request = request.clone();
        let decision = self
            .authority
            .authorize(request)
            .await
            .map_err(|error| match error {
                WorkspaceAuthorityError::Unavailable => WorkspaceResolveError::AuthorityUnavailable,
                WorkspaceAuthorityError::Denied => WorkspaceResolveError::AuthorityDenied,
            })?;
        let resolved_roots =
            match resolve_authority(&expected_request, decision, path_phase.opened_roots) {
                Ok(roots) => roots,
                Err(WorkspaceResolveError::InternalDispatchUnavailable) => {
                    self.task_context.request_closing();
                    return Err(WorkspaceResolveError::InternalDispatchUnavailable);
                }
                Err(error) => return Err(error),
            };

        let candidate = WorkspaceSnapshotCandidate::new(
            session_id,
            workspace.revision,
            resolved_roots,
            path_phase.canonical_cwd,
        );

        #[cfg(test)]
        self.hooks.inner.after_candidate.wait_if_armed().await;

        Ok(candidate)
    }

    pub(crate) async fn revalidate_candidate(
        &self,
        candidate: &WorkspaceSnapshotCandidate,
        workspace: &Workspace,
    ) -> Result<bool, WorkspaceResolveError> {
        let fresh = self.resolve(candidate.session_id, workspace).await?;
        Ok(candidate.has_same_resolution_as(&fresh))
    }
}

/// Test-only deterministic pause at the exact stage where one Load holds its resolved candidate
/// but has not yet performed its final durable recheck.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct WorkspaceResolverTestHooks {
    inner: Arc<WorkspaceResolverTestHooksInner>,
}

#[cfg(test)]
struct WorkspaceResolverTestHooksInner {
    after_candidate: Arc<WorkspaceResolverAsyncBarrier>,
}

#[cfg(test)]
impl WorkspaceResolverTestHooksInner {
    fn new() -> Self {
        Self {
            after_candidate: Arc::new(WorkspaceResolverAsyncBarrier::new()),
        }
    }
}

#[cfg(test)]
impl WorkspaceResolverTestHooks {
    fn new() -> Self {
        Self {
            inner: Arc::new(WorkspaceResolverTestHooksInner::new()),
        }
    }

    pub(crate) fn arm_after_candidate_before_final_recheck(&self) {
        self.inner.after_candidate.arm();
    }

    pub(crate) async fn wait_after_candidate_before_final_recheck(&self) {
        self.inner.after_candidate.wait_until_entered().await;
    }

    pub(crate) fn release_after_candidate_before_final_recheck(&self) {
        self.inner.after_candidate.release();
    }
}

#[cfg(test)]
struct WorkspaceResolverAsyncBarrier {
    armed: AtomicBool,
    entered: AtomicBool,
    released: AtomicBool,
    changed: Notify,
}

#[cfg(test)]
impl WorkspaceResolverAsyncBarrier {
    fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }

    fn arm(&self) {
        self.entered.store(false, Ordering::Release);
        self.released.store(false, Ordering::Release);
        self.armed.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn wait_if_armed(&self) {
        if self
            .armed
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.entered.store(true, Ordering::Release);
        self.changed.notify_waiters();
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    async fn wait_until_entered(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.entered.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct WorkspaceSourceAuthorization {
    root_key: WorkspaceRootKey,
    canonical_root: CanonicalWorkspacePath,
    trust: WorkspaceRootTrust,
}

impl WorkspaceSourceAuthorization {
    pub(crate) fn root_key(&self) -> &WorkspaceRootKey {
        &self.root_key
    }

    pub(crate) const fn trust(&self) -> WorkspaceRootTrust {
        self.trust
    }
}

impl fmt::Debug for WorkspaceSourceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSourceAuthorization")
            .field("trust", &self.trust)
            .finish()
    }
}

#[derive(Clone, Eq, Error, PartialEq)]
pub(crate) enum WorkspaceSourceCaptureError {
    #[error("workspace source location must be a non-root relative path")]
    InvalidRelativeLocation,
    #[error("workspace source is outside an authorized root")]
    SourceOutsideAuthorizedRoot,
    #[error("workspace source kind is not authorized")]
    SourceKindNotAuthorized,
}

impl fmt::Debug for WorkspaceSourceCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AuthorizedPromptSourceRoot {
    key: WorkspaceRootKey,
    canonical_path: CanonicalWorkspacePath,
    trust: WorkspaceRootTrust,
}

impl AuthorizedPromptSourceRoot {
    pub(crate) fn key(&self) -> &WorkspaceRootKey {
        &self.key
    }

    pub(crate) fn canonical_path(&self) -> &CanonicalWorkspacePath {
        &self.canonical_path
    }

    pub(crate) const fn trust(&self) -> WorkspaceRootTrust {
        self.trust
    }
}

impl fmt::Debug for AuthorizedPromptSourceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizedPromptSourceRoot")
            .field("trust", &self.trust)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AuthorizedSkillSourceRoot {
    key: WorkspaceRootKey,
    canonical_path: CanonicalWorkspacePath,
    trust: WorkspaceRootTrust,
}

impl AuthorizedSkillSourceRoot {
    pub(crate) fn key(&self) -> &WorkspaceRootKey {
        &self.key
    }

    pub(crate) fn canonical_path(&self) -> &CanonicalWorkspacePath {
        &self.canonical_path
    }

    pub(crate) const fn trust(&self) -> WorkspaceRootTrust {
        self.trust
    }
}

impl fmt::Debug for AuthorizedSkillSourceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizedSkillSourceRoot")
            .field("trust", &self.trust)
            .finish()
    }
}

struct WorkspaceCandidateCapabilityBasis;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum WorkspaceSnapshotFinishError {
    #[error("workspace snapshot finish authorization mismatch")]
    AuthorizationMismatch,
}

#[derive(Clone)]
pub(crate) struct WorkspacePromptCaptureContext {
    basis: Arc<WorkspaceCandidateCapabilityBasis>,
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    roots: Arc<[AuthorizedPromptSourceRoot]>,
}

impl WorkspacePromptCaptureContext {
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn cwd(&self) -> &CanonicalWorkspacePath {
        &self.cwd
    }

    pub(crate) fn roots(&self) -> &[AuthorizedPromptSourceRoot] {
        &self.roots
    }

    pub(crate) fn capture(
        &self,
        root_key: &WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
        content: Arc<str>,
    ) -> Result<CapturedWorkspacePromptSource, WorkspaceSourceCaptureError> {
        let root = self
            .roots
            .iter()
            .find(|root| root.key == *root_key)
            .ok_or(WorkspaceSourceCaptureError::SourceKindNotAuthorized)?;
        validate_source_relative_location(&relative_location)?;
        Ok(CapturedWorkspacePromptSource {
            relative_location,
            content,
            basis: Arc::clone(&self.basis),
            authorization: WorkspaceSourceAuthorization {
                root_key: root.key.clone(),
                canonical_root: root.canonical_path.clone(),
                trust: root.trust,
            },
        })
    }
}

impl fmt::Debug for WorkspacePromptCaptureContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspacePromptCaptureContext")
            .field("root_count", &self.roots.len())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct WorkspaceSkillCaptureContext {
    basis: Arc<WorkspaceCandidateCapabilityBasis>,
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    roots: Arc<[AuthorizedSkillSourceRoot]>,
}

impl WorkspaceSkillCaptureContext {
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn cwd(&self) -> &CanonicalWorkspacePath {
        &self.cwd
    }

    pub(crate) fn roots(&self) -> &[AuthorizedSkillSourceRoot] {
        &self.roots
    }

    pub(crate) fn capture(
        &self,
        root_key: &WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
        bytes: Arc<[u8]>,
    ) -> Result<CapturedWorkspaceSkillSource, WorkspaceSourceCaptureError> {
        let root = self
            .roots
            .iter()
            .find(|root| root.key == *root_key)
            .ok_or(WorkspaceSourceCaptureError::SourceKindNotAuthorized)?;
        validate_source_relative_location(&relative_location)?;
        Ok(CapturedWorkspaceSkillSource {
            relative_location,
            bytes,
            basis: Arc::clone(&self.basis),
            authorization: WorkspaceSourceAuthorization {
                root_key: root.key.clone(),
                canonical_root: root.canonical_path.clone(),
                trust: root.trust,
            },
        })
    }
}

impl fmt::Debug for WorkspaceSkillCaptureContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSkillCaptureContext")
            .field("root_count", &self.roots.len())
            .finish()
    }
}

fn validate_source_relative_location(
    relative_location: &WorkspaceRelativePath,
) -> Result<(), WorkspaceSourceCaptureError> {
    if relative_location.is_root() {
        return Err(WorkspaceSourceCaptureError::InvalidRelativeLocation);
    }
    let path = Path::new(relative_location.as_str());
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::ParentDir
                    | std::path::Component::CurDir
            )
        })
    {
        return Err(WorkspaceSourceCaptureError::SourceOutsideAuthorizedRoot);
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct CapturedWorkspacePromptSource {
    relative_location: WorkspaceRelativePath,
    content: Arc<str>,
    basis: Arc<WorkspaceCandidateCapabilityBasis>,
    authorization: WorkspaceSourceAuthorization,
}

impl CapturedWorkspacePromptSource {
    pub(crate) fn relative_location(&self) -> &WorkspaceRelativePath {
        &self.relative_location
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    pub(crate) fn content_arc(&self) -> &Arc<str> {
        &self.content
    }

    pub(crate) fn authorization(&self) -> &WorkspaceSourceAuthorization {
        &self.authorization
    }
}

impl fmt::Debug for CapturedWorkspacePromptSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedWorkspacePromptSource")
            .field("has_content", &true)
            .field("authorization", &self.authorization)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct CapturedWorkspaceSkillSource {
    relative_location: WorkspaceRelativePath,
    bytes: Arc<[u8]>,
    basis: Arc<WorkspaceCandidateCapabilityBasis>,
    authorization: WorkspaceSourceAuthorization,
}

impl CapturedWorkspaceSkillSource {
    pub(crate) fn relative_location(&self) -> &WorkspaceRelativePath {
        &self.relative_location
    }

    pub(crate) fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    pub(crate) fn authorization(&self) -> &WorkspaceSourceAuthorization {
        &self.authorization
    }
}

impl fmt::Debug for CapturedWorkspaceSkillSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedWorkspaceSkillSource")
            .field("byte_count", &self.bytes.len())
            .field("authorization", &self.authorization)
            .finish()
    }
}

pub(crate) struct WorkspaceSnapshotCandidate {
    session_id: SessionId,
    revision: WorkspaceRevision,
    roots: Arc<[ResolvedWorkspaceRoot]>,
    cwd: CanonicalWorkspacePath,
    basis: Arc<WorkspaceCandidateCapabilityBasis>,
}

impl WorkspaceSnapshotCandidate {
    #[cfg(test)]
    pub(crate) fn with_revision_for_test(mut self, revision: WorkspaceRevision) -> Self {
        self.revision = revision;
        self
    }

    fn new(
        session_id: SessionId,
        revision: WorkspaceRevision,
        roots: Vec<ResolvedWorkspaceRoot>,
        cwd: CanonicalWorkspacePath,
    ) -> Self {
        Self {
            session_id,
            revision,
            roots: roots.into(),
            cwd,
            basis: Arc::new(WorkspaceCandidateCapabilityBasis),
        }
    }

    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    /// Whether this candidate must pass the final resolver revalidation before
    /// installation: any resolved root that carries a readable filesystem grant or an
    /// authorized Prompt/Skill source reads fresh contents through the async
    /// authority/capture window, so a root replacement in that window could otherwise
    /// publish a stale capability.  Candidates without any readable/source root have no
    /// fresh read authority behind them and remain final without revalidation.
    pub(crate) fn requires_revalidation(&self) -> bool {
        self.roots
            .iter()
            .any(|root| root.filesystem.is_readable() || root.prompt_source || root.skill_source)
    }

    fn has_same_resolution_as(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.revision == other.revision
            && self.roots == other.roots
            && self.cwd == other.cwd
    }

    pub(crate) fn prompt_capture_context(&self) -> WorkspacePromptCaptureContext {
        let roots = self
            .roots
            .iter()
            .filter(|root| root.prompt_source)
            .map(|root| AuthorizedPromptSourceRoot {
                key: root.key.clone(),
                canonical_path: root.canonical_path.clone(),
                trust: root.trust,
            })
            .collect::<Vec<_>>()
            .into();
        WorkspacePromptCaptureContext {
            basis: Arc::clone(&self.basis),
            session_id: self.session_id,
            cwd: self.cwd.clone(),
            roots,
        }
    }

    pub(crate) fn skill_capture_context(&self) -> WorkspaceSkillCaptureContext {
        let roots = self
            .roots
            .iter()
            .filter(|root| root.skill_source)
            .map(|root| AuthorizedSkillSourceRoot {
                key: root.key.clone(),
                canonical_path: root.canonical_path.clone(),
                trust: root.trust,
            })
            .collect::<Vec<_>>()
            .into();
        WorkspaceSkillCaptureContext {
            basis: Arc::clone(&self.basis),
            session_id: self.session_id,
            cwd: self.cwd.clone(),
            roots,
        }
    }

    pub(crate) fn finish(
        self,
        prompt_sources: Arc<[CapturedWorkspacePromptSource]>,
        skill_sources: Arc<[CapturedWorkspaceSkillSource]>,
    ) -> Result<Arc<WorkspaceSnapshot>, WorkspaceSnapshotFinishError> {
        if !self.sources_match_roots(&prompt_sources, &skill_sources) {
            return Err(WorkspaceSnapshotFinishError::AuthorizationMismatch);
        }
        Ok(Arc::new(WorkspaceSnapshot {
            session_id: self.session_id,
            revision: self.revision,
            roots: self.roots,
            cwd: self.cwd,
            prompt_sources,
            skill_sources,
        }))
    }

    fn sources_match_roots(
        &self,
        prompt_sources: &[CapturedWorkspacePromptSource],
        skill_sources: &[CapturedWorkspaceSkillSource],
    ) -> bool {
        prompt_sources.iter().all(|source| {
            validate_source_relative_location(&source.relative_location).is_ok()
                && Arc::ptr_eq(&self.basis, &source.basis)
                && self.roots.iter().any(|root| {
                    root.prompt_source
                        && root.key == source.authorization.root_key
                        && root.canonical_path == source.authorization.canonical_root
                        && root.trust == source.authorization.trust
                })
        }) && skill_sources.iter().all(|source| {
            validate_source_relative_location(&source.relative_location).is_ok()
                && Arc::ptr_eq(&self.basis, &source.basis)
                && self.roots.iter().any(|root| {
                    root.skill_source
                        && root.key == source.authorization.root_key
                        && root.canonical_path == source.authorization.canonical_root
                        && root.trust == source.authorization.trust
                })
        })
    }
}

impl fmt::Debug for WorkspaceSnapshotCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSnapshotCandidate")
            .field("revision", &self.revision)
            .field("root_count", &self.roots.len())
            .finish()
    }
}

#[cfg(test)]
pub(crate) fn prompt_candidate_for_test(
    session_id: SessionId,
    root_keys: Vec<WorkspaceRootKey>,
) -> WorkspaceSnapshotCandidate {
    assert!(
        !root_keys.is_empty(),
        "a Workspace prompt candidate requires a primary root"
    );
    let trust = WorkspaceRootTrust::new(
        WorkspaceTrustLevel::Trusted,
        WorkspaceTrustRevision::new(
            NonZeroU64::new(1).expect("the fixed test trust revision is non-zero"),
        ),
    );
    let (identity, dir) = test_capability_scratch();
    let roots = root_keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| ResolvedWorkspaceRoot {
            role: if index == 0 {
                WorkspaceRootRole::Primary
            } else {
                WorkspaceRootRole::Additional
            },
            canonical_path: CanonicalWorkspacePath::new(PathBuf::from(format!(
                "/minicore-prompt-test/{}",
                key.as_str()
            ))),
            key,
            identity: identity.clone(),
            dir: Arc::clone(&dir),
            trust,
            filesystem: WorkspaceFilesystemGrant::ReadOnly,
            prompt_source: true,
            skill_source: false,
        })
        .collect::<Vec<_>>();
    let cwd = roots[0].canonical_path.clone();
    WorkspaceSnapshotCandidate::new(
        session_id,
        WorkspaceRevision::new(
            NonZeroU64::new(1).expect("the fixed test Workspace revision is non-zero"),
        ),
        roots,
        cwd,
    )
}

/// One shared real directory used as the test-only capability stand-in for fake
/// candidates and fake path adapters. Fake resolutions never read through it, so a
/// single per-process scratch keeps the deterministic tests free of per-path
/// registries without weakening the production capability invariant.
#[cfg(test)]
fn test_capability_scratch() -> (WorkspaceRootIdentity, Arc<CapabilityDir>) {
    use std::sync::OnceLock;

    static SCRATCH: OnceLock<(WorkspaceRootIdentity, Arc<CapabilityDir>)> = OnceLock::new();
    SCRATCH
        .get_or_init(|| {
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-workspace-test-capability-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path)
                .expect("the test capability scratch directory is creatable");
            let dir = CapabilityDir::open_ambient_dir(&path, cap_std::ambient_authority())
                .expect("the test capability scratch directory is openable");
            let identity = WorkspaceRootIdentity::from_file(
                dir.try_clone()
                    .expect("the test capability scratch directory is cloneable")
                    .into_std_file(),
            )
            .expect("the test capability scratch identity is capturable");
            (identity, Arc::new(dir))
        })
        .clone()
}

pub(crate) struct WorkspaceSnapshot {
    session_id: SessionId,
    revision: WorkspaceRevision,
    roots: Arc<[ResolvedWorkspaceRoot]>,
    cwd: CanonicalWorkspacePath,
    prompt_sources: Arc<[CapturedWorkspacePromptSource]>,
    skill_sources: Arc<[CapturedWorkspaceSkillSource]>,
}

impl WorkspaceSnapshot {
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub(crate) fn prompt_context(&self) -> WorkspacePromptContext {
        let primary_root = self
            .roots
            .first()
            .expect("a Workspace always has a primary root")
            .canonical_path
            .clone();
        WorkspacePromptContext {
            session_id: self.session_id,
            cwd: self.cwd.clone(),
            primary_root,
            sources: Arc::clone(&self.prompt_sources),
        }
    }

    pub(crate) fn skill_context(&self) -> WorkspaceSkillContext {
        WorkspaceSkillContext {
            session_id: self.session_id,
            cwd: self.cwd.clone(),
            sources: Arc::clone(&self.skill_sources),
        }
    }

    pub(crate) fn tool_context(&self) -> WorkspaceToolContext {
        WorkspaceToolContext {
            access: WorkspaceAccessView {
                session_id: self.session_id,
                cwd: self.cwd.clone(),
                roots: self
                    .roots
                    .iter()
                    .map(|root| WorkspaceAccessRoot {
                        canonical_path: root.canonical_path.clone(),
                        dir: Arc::clone(&root.dir),
                        filesystem: root.filesystem,
                    })
                    .collect::<Vec<_>>()
                    .into(),
            },
        }
    }

    pub(crate) fn into_resolved(self: Arc<Self>) -> ResolvedWorkspace {
        ResolvedWorkspace { snapshot: self }
    }
}

impl fmt::Debug for WorkspaceSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSnapshot")
            .field("revision", &self.revision)
            .field("root_count", &self.roots.len())
            .field("prompt_source_count", &self.prompt_sources.len())
            .field("skill_source_count", &self.skill_sources.len())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct ResolvedWorkspace {
    snapshot: Arc<WorkspaceSnapshot>,
}

impl ResolvedWorkspace {
    pub(crate) fn snapshot(&self) -> &Arc<WorkspaceSnapshot> {
        &self.snapshot
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.snapshot.session_id()
    }

    pub(crate) fn revision(&self) -> WorkspaceRevision {
        self.snapshot.revision()
    }
}

impl fmt::Debug for ResolvedWorkspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedWorkspace")
            .field("snapshot", &self.snapshot)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct WorkspacePromptContext {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    primary_root: CanonicalWorkspacePath,
    sources: Arc<[CapturedWorkspacePromptSource]>,
}

impl WorkspacePromptContext {
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn cwd(&self) -> &CanonicalWorkspacePath {
        &self.cwd
    }

    pub(crate) fn primary_root(&self) -> &CanonicalWorkspacePath {
        &self.primary_root
    }

    pub(crate) fn sources(&self) -> &[CapturedWorkspacePromptSource] {
        &self.sources
    }
}

impl fmt::Debug for WorkspacePromptContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspacePromptContext")
            .field("source_count", &self.sources.len())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct WorkspaceSkillContext {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    sources: Arc<[CapturedWorkspaceSkillSource]>,
}

impl WorkspaceSkillContext {
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn cwd(&self) -> &CanonicalWorkspacePath {
        &self.cwd
    }

    pub(crate) fn sources(&self) -> &[CapturedWorkspaceSkillSource] {
        &self.sources
    }
}

impl fmt::Debug for WorkspaceSkillContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSkillContext")
            .field("source_count", &self.sources.len())
            .finish()
    }
}

/// One effective filesystem access root projected from a resolved root. The open
/// capability `dir` is bound to the exact directory the resolver verified, so the
/// synchronous authorization below never touches ambient paths.
#[derive(Clone)]
pub(crate) struct WorkspaceAccessRoot {
    canonical_path: CanonicalWorkspacePath,
    dir: Arc<CapabilityDir>,
    filesystem: WorkspaceFilesystemGrant,
}

impl fmt::Debug for WorkspaceAccessRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAccessRoot")
            .field("filesystem", &self.filesystem)
            .finish()
    }
}

/// The narrow filesystem ceiling a loaded WorkspaceSnapshot projects for Tools.
/// It exposes only the captured canonical cwd, the effective access roots with
/// their bound capabilities, and synchronous cwd-relative read, directory-read
/// and write authorization. Roots are non-overlapping and cwd lies within
/// exactly one root, so this slice authorizes only cwd-relative paths;
/// additional roots are not directly addressable through this schema.
#[derive(Clone)]
pub(crate) struct WorkspaceAccessView {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    roots: Arc<[WorkspaceAccessRoot]>,
}

impl WorkspaceAccessView {
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn cwd(&self) -> &CanonicalWorkspacePath {
        &self.cwd
    }

    pub(crate) fn roots(&self) -> &[WorkspaceAccessRoot] {
        &self.roots
    }

    /// Authorizes one cwd-relative read target against the captured facts. The
    /// containing root is the exact root holding the captured canonical cwd; the
    /// cwd's relative position inside that root is prepended to the requested
    /// relative path, and the resulting capability-relative target must be fully
    /// normal. The returned path is the only value a Tool may use to open files.
    pub(crate) fn authorize_read(
        &self,
        relative: &WorkspaceRelativePath,
    ) -> Result<AuthorizedWorkspaceReadPath, WorkspaceAccessError> {
        if relative.is_root() {
            return Err(WorkspaceAccessError::InvalidPath);
        }
        let (root, cwd_in_root) = self.containing_read_root()?;
        let target = cwd_in_root.join(relative.as_str());
        if !target
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(WorkspaceAccessError::InvalidPath);
        }
        Ok(AuthorizedWorkspaceReadPath {
            root_dir: Arc::clone(&root.dir),
            relative: target,
        })
    }

    /// Authorizes one cwd-relative directory-read target with exactly the same
    /// containing root, effective readable grant, cwd-relative prepend and fully
    /// normal containment model as [`WorkspaceAccessView::authorize_read`]. Unlike
    /// file reads the empty relative path is legal: it names the captured cwd
    /// directory itself, including when the cwd is the root. The returned value
    /// is the only one a Tool may use to open a directory.
    pub(crate) fn authorize_read_directory(
        &self,
        relative: &WorkspaceRelativePath,
    ) -> Result<AuthorizedWorkspaceReadDirectory, WorkspaceAccessError> {
        let (root, cwd_in_root) = self.containing_read_root()?;
        let target = cwd_in_root.join(relative.as_str());
        if !target
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(WorkspaceAccessError::InvalidPath);
        }
        Ok(AuthorizedWorkspaceReadDirectory {
            root_dir: Arc::clone(&root.dir),
            relative: target,
        })
    }

    /// Authorizes one cwd-relative write target against the captured facts. The
    /// containing root is the exact root holding the captured canonical cwd and
    /// its effective grant must be exactly `ReadWrite` (a `ReadOnly` or `None`
    /// grant denies the write route while leaving the read route untouched); the
    /// cwd's relative position inside that root is prepended to the requested
    /// relative path, and the resulting capability-relative target must be fully
    /// normal. Empty and root paths are rejected. The returned path is the only
    /// value a Tool may use to prepare a file mutation.
    pub(crate) fn authorize_write(
        &self,
        relative: &WorkspaceRelativePath,
    ) -> Result<AuthorizedWorkspaceWritePath, WorkspaceWriteError> {
        if relative.is_root() {
            return Err(WorkspaceWriteError::InvalidTarget);
        }
        let (root, cwd_in_root) = self.containing_write_root()?;
        let target = cwd_in_root.join(relative.as_str());
        if !target
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(WorkspaceWriteError::InvalidTarget);
        }
        Ok(AuthorizedWorkspaceWritePath {
            root_dir: Arc::clone(&root.dir),
            relative: target,
        })
    }

    /// The single shared basis for cwd-relative write authorization: the exact
    /// root holding the captured canonical cwd plus the cwd's
    /// capability-relative position inside it, or the error that makes the
    /// schema inapplicable. Unlike the read basis, only an effective grant of
    /// exactly `ReadWrite` authorizes; reads stay authorized under `ReadOnly`.
    fn containing_write_root(
        &self,
    ) -> Result<(&WorkspaceAccessRoot, PathBuf), WorkspaceWriteError> {
        let root = self
            .roots
            .iter()
            .find(|root| {
                self.cwd
                    .as_path()
                    .starts_with(root.canonical_path.as_path())
            })
            .ok_or(WorkspaceWriteError::Unavailable)?;
        if !matches!(root.filesystem, WorkspaceFilesystemGrant::ReadWrite) {
            return Err(WorkspaceWriteError::NotAuthorized);
        }
        let cwd_in_root = self
            .cwd
            .as_path()
            .strip_prefix(root.canonical_path.as_path())
            .map_err(|_| WorkspaceWriteError::Unavailable)?;
        Ok((root, cwd_in_root.to_path_buf()))
    }

    /// The single shared basis for cwd-relative read and directory-read
    /// authorization: the exact root holding the captured canonical cwd plus the
    /// cwd's capability-relative position inside it, or the error that makes the
    /// schema inapplicable.
    fn containing_read_root(
        &self,
    ) -> Result<(&WorkspaceAccessRoot, PathBuf), WorkspaceAccessError> {
        let root = self
            .roots
            .iter()
            .find(|root| {
                self.cwd
                    .as_path()
                    .starts_with(root.canonical_path.as_path())
            })
            .ok_or(WorkspaceAccessError::Unavailable)?;
        if !root.filesystem.is_readable() {
            return Err(WorkspaceAccessError::NotAuthorized);
        }
        let cwd_in_root = self
            .cwd
            .as_path()
            .strip_prefix(root.canonical_path.as_path())
            .map_err(|_| WorkspaceAccessError::Unavailable)?;
        Ok((root, cwd_in_root.to_path_buf()))
    }
}

impl fmt::Debug for WorkspaceAccessView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceAccessView")
            .field("root_count", &self.roots.len())
            .finish()
    }
}

/// The workspace projection a ToolSet pins for a loaded Session. It is cloneable
/// and carries no Prompt/Skill source, approval, registry or provider facts.
#[derive(Clone)]
pub(crate) struct WorkspaceToolContext {
    access: WorkspaceAccessView,
}

impl WorkspaceToolContext {
    pub(crate) fn access(&self) -> &WorkspaceAccessView {
        &self.access
    }
}

impl fmt::Debug for WorkspaceToolContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceToolContext")
            .field("access", &self.access)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum WorkspaceAccessError {
    #[error("workspace read access is not authorized")]
    NotAuthorized,
    #[error("workspace read target is invalid")]
    InvalidPath,
    #[error("workspace read target is unavailable")]
    Unavailable,
    #[error("workspace read failed")]
    OpenFailed,
}

fn map_access_io_error(error: std::io::Error) -> WorkspaceAccessError {
    match error.kind() {
        std::io::ErrorKind::NotFound => WorkspaceAccessError::Unavailable,
        _ => WorkspaceAccessError::OpenFailed,
    }
}

/// The only authorized read value a Tool can consume: the open capability bound
/// to the authorized root plus a normalized capability-relative target. It never
/// exposes ambient absolute paths; opening always resolves through the captured
/// `Dir`, which cannot escape the root it was bound to.
#[derive(Clone)]
pub(crate) struct AuthorizedWorkspaceReadPath {
    root_dir: Arc<CapabilityDir>,
    relative: PathBuf,
}

impl AuthorizedWorkspaceReadPath {
    /// The normalized capability-relative target; safe to expose because it
    /// contains no ambient root text.
    #[cfg(test)]
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative
    }

    /// Opens the target read-only and nonblocking through the captured root
    /// capability. A symlink that would leave the root fails at this open; a
    /// FIFO or other special entry cannot block the open (no writer is needed to
    /// pair with it) and is rejected by the caller's regular-file metadata check.
    pub(crate) fn open_nonblocking(&self) -> Result<cap_std::fs::File, WorkspaceAccessError> {
        // The hidden `_cap_fs_ext_nonblock` extension is the only way cap-std
        // exposes O_NONBLOCK without the cap-fs-ext crate; the pinned exact
        // cap-std version makes this stable for this repository.
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        options._cap_fs_ext_nonblock(true);
        self.root_dir
            .open_with(&self.relative, &options)
            .map_err(map_access_io_error)
    }
}

impl fmt::Debug for AuthorizedWorkspaceReadPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizedWorkspaceReadPath { .. }")
    }
}

/// The only authorized directory-read value a Tool can consume: the open
/// capability bound to the authorized root plus a normalized
/// capability-relative target. It never exposes ambient absolute paths; opening
/// always resolves through the captured `Dir`, which cannot escape the root it
/// was bound to.
#[derive(Clone)]
pub(crate) struct AuthorizedWorkspaceReadDirectory {
    root_dir: Arc<CapabilityDir>,
    relative: PathBuf,
}

impl AuthorizedWorkspaceReadDirectory {
    /// The normalized capability-relative target; safe to expose because it
    /// contains no ambient root text.
    #[cfg(test)]
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative
    }

    /// Opens the target directory through the captured root capability. The
    /// empty target names the captured cwd directory itself and clones the bound
    /// capability handle; a non-empty target is opened capability-relative
    /// through the root, so a symlink that would leave the root fails closed at
    /// this open.
    pub(crate) fn open(&self) -> Result<cap_std::fs::Dir, WorkspaceAccessError> {
        if self.relative.as_os_str().is_empty() {
            return self.root_dir.try_clone().map_err(map_access_io_error);
        }
        self.root_dir
            .open_dir(&self.relative)
            .map_err(map_access_io_error)
    }
}

impl fmt::Debug for AuthorizedWorkspaceReadDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizedWorkspaceReadDirectory { .. }")
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum WorkspaceWriteError {
    #[error("workspace file write access is not authorized")]
    NotAuthorized,
    #[error("workspace file write target is invalid")]
    InvalidTarget,
    #[error("workspace file write target is unavailable")]
    Unavailable,
    #[error("workspace file write target is not a regular file")]
    NotRegularFile,
    #[error("workspace file write target open failed")]
    OpenFailed,
    #[error("workspace file write failed")]
    WriteFailed,
}

fn map_write_io_error(error: std::io::Error) -> WorkspaceWriteError {
    match error.kind() {
        std::io::ErrorKind::NotFound => WorkspaceWriteError::Unavailable,
        _ => WorkspaceWriteError::OpenFailed,
    }
}

/// Exact physical identity of one opened file or directory captured for mutation
/// keys, via the safe `same_file` handle (Unix inode / Windows file identity), so
/// direct paths, in-root symlink aliases and hard-link aliases of the same file
/// produce one equal key. Kept behind an `Arc` so the identity stays cloneable
/// without ever duplicating the open handle it was bound to. The identity is
/// private to workspace ownership: only `Eq`/`Hash`/`Debug` are visible here,
/// never the underlying path or handle, and no other module can name the type.
#[derive(Clone, Eq, Hash, PartialEq)]
struct WorkspaceMutationIdentity(Arc<same_file::Handle>);

impl WorkspaceMutationIdentity {
    fn from_file(file: std::fs::File) -> Result<Self, WorkspaceWriteError> {
        let handle =
            same_file::Handle::from_file(file).map_err(|_| WorkspaceWriteError::OpenFailed)?;
        Ok(Self(Arc::new(handle)))
    }
}

impl fmt::Debug for WorkspaceMutationIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceMutationIdentity { .. }")
    }
}

/// The opaque cloneable mutation identity a Session-local queue later serializes
/// on. The internal representation distinguishes one exact physical file that
/// already existed at preparation (opened through the captured root capability)
/// from one exact parent directory plus the normalized final filename of a target
/// that did not exist, but downstream modules can only `Clone`/`Eq`/`Hash`/`Debug`
/// the key: they can never pattern-match the physical identity or the filename,
/// and the key itself carries no ambient path text.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct WorkspaceFileMutationKey {
    kind: WorkspaceFileMutationKeyKind,
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum WorkspaceFileMutationKeyKind {
    Existing(WorkspaceMutationIdentity),
    Create {
        parent: WorkspaceMutationIdentity,
        final_name: PathBuf,
    },
}

impl fmt::Debug for WorkspaceFileMutationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceFileMutationKey { .. }")
    }
}

/// The only authorized write value a Tool can consume: the open capability bound
/// to the authorized root plus a normalized capability-relative target. It never
/// exposes ambient absolute paths; opening always resolves through the captured
/// `Dir`, which cannot escape the root it was bound to.
#[derive(Clone)]
pub(crate) struct AuthorizedWorkspaceWritePath {
    root_dir: Arc<CapabilityDir>,
    relative: PathBuf,
}

impl AuthorizedWorkspaceWritePath {
    /// The normalized capability-relative target; safe to expose because it
    /// contains no ambient root text.
    #[cfg(test)]
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative
    }

    /// Prepares the write target without any mutation: no create, truncate or
    /// write happens here, only capability opens and metadata proofs. An existing
    /// target is opened write-only nonblocking through the captured root (the
    /// final symlink is followed inside cap-std containment, so an escape fails
    /// closed) and its exact physical identity is captured from the opened file;
    /// a missing target falls back to the create shape only when its direct
    /// parent already exists and opens through the same capability — no mkdir,
    /// no intermediate creation.
    pub(crate) fn prepare(&self) -> Result<PreparedWorkspaceWriteTarget, WorkspaceWriteError> {
        match self.open_existing() {
            Ok(file) => {
                let metadata = file.metadata().map_err(map_write_io_error)?;
                if !metadata.is_file() {
                    return Err(WorkspaceWriteError::NotRegularFile);
                }
                let identity = WorkspaceMutationIdentity::from_file(
                    file.try_clone().map_err(map_write_io_error)?.into_std(),
                )?;
                Ok(PreparedWorkspaceWriteTarget {
                    kind: PreparedWorkspaceWriteTargetKind::Existing { file, identity },
                })
            }
            Err(WorkspaceWriteError::Unavailable) => self.prepare_create(),
            Err(error) => Err(error),
        }
    }

    /// Opens the target write-only and nonblocking without truncate or create,
    /// following the final symlink inside cap-std containment. `NotFound` is the
    /// only trigger for the create shape.
    fn open_existing(&self) -> Result<cap_std::fs::File, WorkspaceWriteError> {
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true);
        options._cap_fs_ext_nonblock(true);
        self.root_dir
            .open_with(&self.relative, &options)
            .map_err(map_write_io_error)
    }

    /// The create shape: the direct parent must already exist and open
    /// capability-relative (no mkdir anywhere); its exact identity plus the
    /// normalized final filename form the create key, and later writes open the
    /// final name only through the retained parent capability.
    fn prepare_create(&self) -> Result<PreparedWorkspaceWriteTarget, WorkspaceWriteError> {
        let final_name = self
            .relative
            .file_name()
            .ok_or(WorkspaceWriteError::InvalidTarget)?;
        let parent_relative = self.relative.parent().unwrap_or(Path::new(""));
        let (parent, parent_identity) = if parent_relative.as_os_str().is_empty() {
            let identity = WorkspaceMutationIdentity::from_file(
                self.root_dir
                    .try_clone()
                    .map_err(map_write_io_error)?
                    .into_std_file(),
            )?;
            (Arc::clone(&self.root_dir), identity)
        } else {
            let opened = self
                .root_dir
                .open_dir(parent_relative)
                .map_err(map_write_io_error)?;
            let identity = WorkspaceMutationIdentity::from_file(
                opened
                    .try_clone()
                    .map_err(map_write_io_error)?
                    .into_std_file(),
            )?;
            (Arc::new(opened), identity)
        };
        Ok(PreparedWorkspaceWriteTarget {
            kind: PreparedWorkspaceWriteTargetKind::Create {
                parent,
                parent_identity,
                final_name: PathBuf::from(final_name),
            },
        })
    }
}

impl fmt::Debug for AuthorizedWorkspaceWritePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizedWorkspaceWritePath { .. }")
    }
}

/// The move-only prepared mutation carrier: it retains the exact handles opened at
/// preparation and performs the in-place full replacement later without ever
/// re-opening by path. Downstream modules can only call [`key`](Self::key) for the
/// cloneable opaque identity a Session queue serializes on and
/// [`write`](Self::write) to perform the single full-content replacement; the
/// retained handles and the final filename stay private, and neither method
/// exposes ambient paths.
pub(crate) struct PreparedWorkspaceWriteTarget {
    kind: PreparedWorkspaceWriteTargetKind,
}

enum PreparedWorkspaceWriteTargetKind {
    Existing {
        /// The exact file opened at preparation; the write truncates and rewrites
        /// this very handle, so a path replaced after preparation never redirects
        /// the authorized mutation.
        file: cap_std::fs::File,
        identity: WorkspaceMutationIdentity,
    },
    Create {
        /// The exact parent directory opened at preparation; the write opens the
        /// final name only through this capability.
        parent: Arc<CapabilityDir>,
        parent_identity: WorkspaceMutationIdentity,
        final_name: PathBuf,
    },
}

impl PreparedWorkspaceWriteTarget {
    /// The cloneable opaque key: one exact physical file for an existing target,
    /// or one exact parent directory plus normalized final filename for a target
    /// that did not exist at preparation.
    pub(crate) fn key(&self) -> WorkspaceFileMutationKey {
        let kind = match &self.kind {
            PreparedWorkspaceWriteTargetKind::Existing { identity, .. } => {
                WorkspaceFileMutationKeyKind::Existing(identity.clone())
            }
            PreparedWorkspaceWriteTargetKind::Create {
                parent_identity,
                final_name,
                ..
            } => WorkspaceFileMutationKeyKind::Create {
                parent: parent_identity.clone(),
                final_name: final_name.clone(),
            },
        };
        WorkspaceFileMutationKey { kind }
    }

    /// Performs the in-place full replacement of the prepared target. The
    /// existing shape seeks to offset zero, truncates to zero bytes and writes
    /// the exact content through the retained file handle; the create shape
    /// opens the final name through the retained parent with
    /// create+truncate+write+nonblock and a final-component no-follow option,
    /// proves the opened entry is a regular file, then writes the exact content.
    /// No append, no newline normalization, no fsync, no atomic rename, no
    /// retries; a failed write is a truthful failure that may have partially
    /// replaced the target.
    pub(crate) fn write(&mut self, content: &[u8]) -> Result<(), WorkspaceWriteError> {
        match &mut self.kind {
            PreparedWorkspaceWriteTargetKind::Existing { file, .. } => {
                file.seek(SeekFrom::Start(0))
                    .map_err(|_| WorkspaceWriteError::WriteFailed)?;
                file.set_len(0)
                    .map_err(|_| WorkspaceWriteError::WriteFailed)?;
                file.write_all(content)
                    .map_err(|_| WorkspaceWriteError::WriteFailed)?;
                Ok(())
            }
            PreparedWorkspaceWriteTargetKind::Create {
                parent, final_name, ..
            } => {
                let mut options = cap_std::fs::OpenOptions::new();
                options.write(true);
                options.create(true);
                options.truncate(true);
                options._cap_fs_ext_nonblock(true);
                options._cap_fs_ext_follow(FollowSymlinks::No);
                let mut file = parent
                    .open_with(final_name.as_path(), &options)
                    .map_err(map_write_io_error)?;
                let metadata = file.metadata().map_err(map_write_io_error)?;
                if !metadata.is_file() {
                    return Err(WorkspaceWriteError::NotRegularFile);
                }
                file.write_all(content)
                    .map_err(|_| WorkspaceWriteError::WriteFailed)?;
                Ok(())
            }
        }
    }
}

impl fmt::Debug for PreparedWorkspaceWriteTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedWorkspaceWriteTarget { .. }")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::{Future, poll_fn};
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::{
        AuthorizedWorkspaceReadPath, AuthorizedWorkspaceWritePath, CanonicalWorkspacePath,
        CapturedWorkspacePromptSource, CapturedWorkspaceSkillSource, LocalWorkspacePathAdapter,
        OpenedWorkspaceRoot, PreparedWorkspaceWriteTarget, PreparedWorkspaceWriteTargetKind,
        RequestedFilesystemAccess, RestrictedWorkspaceAuthority, Workspace, WorkspaceAccessError,
        WorkspaceAccessRoot, WorkspaceAccessView, WorkspaceAuthority, WorkspaceAuthorityDecision,
        WorkspaceAuthorityError, WorkspaceAuthorityRequest, WorkspaceAuthorityRootDecision,
        WorkspaceAuthorityRootRequest, WorkspaceConstructionError, WorkspaceCwdSpec,
        WorkspaceDefinitionInput, WorkspaceFileMutationKey, WorkspaceFilesystemGrant,
        WorkspaceInputLoweringError, WorkspacePathAdapter, WorkspacePathError, WorkspacePathTarget,
        WorkspaceResolveError, WorkspaceResolver, WorkspaceRootIdentity, WorkspaceRootInput,
        WorkspaceRootRole, WorkspaceRootSpec, WorkspaceRootTrust, WorkspaceSnapshotFinishError,
        WorkspaceSourceCaptureError, WorkspaceSourcePolicy, WorkspaceTrustLevel,
        WorkspaceTrustRevision, WorkspaceWriteError, checked_native_uri, lower_workspace,
        test_capability_scratch, uri_from_spec,
    };
    use crate::runtime_task::RuntimeTaskContext;
    use crate::wire::{CanonicalFileUri, SessionId, WorkspaceRelativePath, WorkspaceRevision};

    static NEXT_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone)]
    struct DeterministicWorkspacePathAdapter {
        mappings: BTreeMap<PathBuf, Result<CanonicalWorkspacePath, WorkspacePathError>>,
        identity: WorkspaceRootIdentity,
        dir: Arc<cap_std::fs::Dir>,
        entered: Option<Arc<Barrier>>,
        release: Option<Arc<Barrier>>,
        block_once: Arc<AtomicBool>,
        panic: bool,
    }

    impl DeterministicWorkspacePathAdapter {
        fn identity() -> Self {
            let (identity, dir) = test_capability_scratch();
            Self {
                mappings: BTreeMap::new(),
                identity,
                dir,
                entered: None,
                release: None,
                block_once: Arc::new(AtomicBool::new(false)),
                panic: false,
            }
        }

        fn mapping(mut self, input: &str, canonical: &str) -> Self {
            self.mappings.insert(
                PathBuf::from(input),
                Ok(CanonicalWorkspacePath::new(PathBuf::from(canonical))),
            );
            self
        }

        fn blocking(entered: Arc<Barrier>, release: Arc<Barrier>) -> Self {
            let (identity, dir) = test_capability_scratch();
            Self {
                mappings: BTreeMap::new(),
                identity,
                dir,
                entered: Some(entered),
                release: Some(release),
                block_once: Arc::new(AtomicBool::new(true)),
                panic: false,
            }
        }

        fn panicking() -> Self {
            Self {
                panic: true,
                ..Self::identity()
            }
        }
    }

    impl WorkspacePathAdapter for DeterministicWorkspacePathAdapter {
        fn canonicalize_directory(
            &self,
            path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            if self.panic {
                panic!("deterministic path adapter panic payload");
            }
            if self.block_once.swap(false, Ordering::SeqCst) {
                self.entered
                    .as_ref()
                    .expect("a blocking adapter has an entry barrier")
                    .wait();
                self.release
                    .as_ref()
                    .expect("a blocking adapter has a release barrier")
                    .wait();
            }
            self.mappings
                .get(path)
                .cloned()
                .unwrap_or_else(|| Ok(CanonicalWorkspacePath::new(path.to_path_buf())))
        }

        fn open_directory(&self, path: &Path) -> Result<OpenedWorkspaceRoot, WorkspacePathError> {
            // The deterministic adapter substitutes the shared test scratch
            // capability; fake resolutions never read through it.
            let canonical = self.canonicalize_directory(path)?;
            Ok(OpenedWorkspaceRoot {
                canonical,
                identity: self.identity.clone(),
                dir: Arc::clone(&self.dir),
            })
        }

        fn canonicalize_directory_in_root(
            &self,
            _opened_root: &OpenedWorkspaceRoot,
            _relative: &str,
            ambient_path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            // The deterministic adapter keeps its pure declared-path mapping for
            // the cwd claim; it does not emulate the production capability proof.
            self.canonicalize_directory(ambient_path)
        }
    }

    struct ChangingCanonicalWorkspacePathAdapter {
        calls: AtomicUsize,
        identity: WorkspaceRootIdentity,
        dir: Arc<cap_std::fs::Dir>,
    }

    impl ChangingCanonicalWorkspacePathAdapter {
        fn new() -> Self {
            let (identity, dir) = test_capability_scratch();
            Self {
                calls: AtomicUsize::new(0),
                identity,
                dir,
            }
        }
    }

    impl WorkspacePathAdapter for ChangingCanonicalWorkspacePathAdapter {
        fn canonicalize_directory(
            &self,
            _path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            let path = if self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
                "/deterministic/first"
            } else {
                "/deterministic/second"
            };
            Ok(CanonicalWorkspacePath::new(PathBuf::from(path)))
        }

        fn open_directory(&self, path: &Path) -> Result<OpenedWorkspaceRoot, WorkspacePathError> {
            let canonical = self.canonicalize_directory(path)?;
            Ok(OpenedWorkspaceRoot {
                canonical,
                identity: self.identity.clone(),
                dir: Arc::clone(&self.dir),
            })
        }

        fn canonicalize_directory_in_root(
            &self,
            _opened_root: &OpenedWorkspaceRoot,
            _relative: &str,
            ambient_path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            // Delegate so the deterministic call-counting sequence is preserved.
            self.canonicalize_directory(ambient_path)
        }
    }

    /// Wraps the production local adapter and counts how many times the
    /// capability-relative cwd proof ran, so a deterministic test can prove the
    /// production path phase exercises it for a normal nested cwd.
    struct RecordingLocalAdapter {
        inner: LocalWorkspacePathAdapter,
        cwd_proof_calls: Arc<AtomicUsize>,
    }

    impl RecordingLocalAdapter {
        fn new() -> Self {
            Self {
                inner: LocalWorkspacePathAdapter,
                cwd_proof_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn cwd_proof_calls(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.cwd_proof_calls)
        }
    }

    impl WorkspacePathAdapter for RecordingLocalAdapter {
        fn canonicalize_directory(
            &self,
            path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            self.inner.canonicalize_directory(path)
        }

        fn open_directory(&self, path: &Path) -> Result<OpenedWorkspaceRoot, WorkspacePathError> {
            self.inner.open_directory(path)
        }

        fn canonicalize_directory_in_root(
            &self,
            opened_root: &OpenedWorkspaceRoot,
            relative: &str,
            ambient_path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            self.cwd_proof_calls.fetch_add(1, Ordering::SeqCst);
            self.inner
                .canonicalize_directory_in_root(opened_root, relative, ambient_path)
        }
    }

    /// Replaces the declared root with a fresh directory at the same path exactly
    /// between root capture and the cwd proof, simulating the rename/replacement
    /// race the capability-relative identity proof must fail closed against.
    struct ReplacingRootLocalAdapter {
        inner: LocalWorkspacePathAdapter,
        declared_root: PathBuf,
        displaced_root: PathBuf,
        replaced: AtomicBool,
    }

    impl ReplacingRootLocalAdapter {
        fn new(declared_root: PathBuf, displaced_root: PathBuf) -> Self {
            Self {
                inner: LocalWorkspacePathAdapter,
                declared_root,
                displaced_root,
                replaced: AtomicBool::new(false),
            }
        }
    }

    impl WorkspacePathAdapter for ReplacingRootLocalAdapter {
        fn canonicalize_directory(
            &self,
            path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            self.inner.canonicalize_directory(path)
        }

        fn open_directory(&self, path: &Path) -> Result<OpenedWorkspaceRoot, WorkspacePathError> {
            self.inner.open_directory(path)
        }

        fn canonicalize_directory_in_root(
            &self,
            opened_root: &OpenedWorkspaceRoot,
            relative: &str,
            ambient_path: &Path,
        ) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
            if !self.replaced.swap(true, Ordering::SeqCst) {
                // Root capture has finished and the cwd proof is about to run:
                // rename the captured root away and create a replacement at the
                // same path, including the cwd subdirectory, so the ambient
                // canonical text is unchanged while the captured capability still
                // names the displaced directory.
                std::fs::rename(&self.declared_root, &self.displaced_root)
                    .expect("the captured root is displaceable");
                std::fs::create_dir_all(self.declared_root.join(relative))
                    .expect("the replacement cwd directory is creatable");
            }
            self.inner
                .canonicalize_directory_in_root(opened_root, relative, ambient_path)
        }
    }

    #[derive(Clone)]
    struct DeterministicWorkspaceAuthority {
        handler: Arc<
            dyn Fn(
                    WorkspaceAuthorityRequest,
                ) -> Result<WorkspaceAuthorityDecision, WorkspaceAuthorityError>
                + Send
                + Sync,
        >,
    }

    impl DeterministicWorkspaceAuthority {
        fn new<F>(handler: F) -> Self
        where
            F: Fn(
                    WorkspaceAuthorityRequest,
                ) -> Result<WorkspaceAuthorityDecision, WorkspaceAuthorityError>
                + Send
                + Sync
                + 'static,
        {
            Self {
                handler: Arc::new(handler),
            }
        }
    }

    impl WorkspaceAuthority for DeterministicWorkspaceAuthority {
        fn authorize(&self, request: WorkspaceAuthorityRequest) -> super::WorkspaceAuthorityFuture {
            let handler = Arc::clone(&self.handler);
            Box::pin(async move { handler(request) })
        }
    }

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let suffix = NEXT_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-workspace-{label}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("the test temporary directory is creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn initialized_context() -> RuntimeTaskContext {
        RuntimeTaskContext::new(tokio::runtime::Handle::current())
            .await
            .expect("the test runtime has the required timer service")
    }

    fn session_id() -> SessionId {
        "ses_11111111111111111111111111111111"
            .parse()
            .expect("the test session id is canonical")
    }

    fn trust() -> WorkspaceRootTrust {
        WorkspaceRootTrust::new(
            WorkspaceTrustLevel::Trusted,
            WorkspaceTrustRevision::new(
                NonZeroU64::new(7).expect("the test trust revision is non-zero"),
            ),
        )
    }

    fn authority_with_ceiling(
        filesystem: WorkspaceFilesystemGrant,
        prompt: bool,
        skill: bool,
    ) -> Arc<dyn WorkspaceAuthority> {
        Arc::new(DeterministicWorkspaceAuthority::new(move |request| {
            let roots = request
                .roots()
                .iter()
                .map(|root| {
                    WorkspaceAuthorityRootDecision::new(
                        root.clone(),
                        trust(),
                        filesystem,
                        prompt,
                        skill,
                    )
                })
                .collect();
            Ok(WorkspaceAuthorityDecision::new(roots))
        }))
    }

    fn authority_by_key() -> Arc<dyn WorkspaceAuthority> {
        Arc::new(DeterministicWorkspaceAuthority::new(|request| {
            let roots = request
                .roots()
                .iter()
                .map(|root| {
                    let is_primary = root.key.as_str() == "primary";
                    WorkspaceAuthorityRootDecision::new(
                        root.clone(),
                        trust(),
                        WorkspaceFilesystemGrant::ReadWrite,
                        is_primary,
                        !is_primary,
                    )
                })
                .collect();
            Ok(WorkspaceAuthorityDecision::new(roots))
        }))
    }

    fn resolver_with_adapters(
        context: RuntimeTaskContext,
        paths: impl WorkspacePathAdapter + 'static,
        authority: Arc<dyn WorkspaceAuthority>,
    ) -> WorkspaceResolver {
        WorkspaceResolver::new_with_adapters(context, Arc::new(paths), authority)
    }

    fn root_spec_with(
        key: &str,
        path: &str,
        requested_access: RequestedFilesystemAccess,
        sources: WorkspaceSourcePolicy,
    ) -> WorkspaceRootSpec {
        WorkspaceRootSpec {
            key: key.parse().expect("the test root key is canonical"),
            path: PathBuf::from(path),
            requested_access,
            sources,
        }
    }

    fn resolver_workspace(
        primary: (&str, &str, RequestedFilesystemAccess, WorkspaceSourcePolicy),
        additional: Vec<(&str, &str, RequestedFilesystemAccess, WorkspaceSourcePolicy)>,
        cwd_root: &str,
        cwd_relative: &str,
    ) -> Workspace {
        Workspace::new(
            revision(),
            root_spec_with(primary.0, primary.1, primary.2, primary.3),
            additional
                .into_iter()
                .map(|root| root_spec_with(root.0, root.1, root.2, root.3))
                .collect(),
            cwd(cwd_root, cwd_relative),
        )
        .expect("the resolver test workspace is structurally valid")
    }

    async fn poll_once_pending<F>(mut future: Pin<&mut F>) -> bool
    where
        F: Future,
    {
        poll_fn(|context| {
            std::task::Poll::Ready(matches!(
                future.as_mut().poll(context),
                std::task::Poll::Pending
            ))
        })
        .await
    }

    #[test]
    fn explicit_posix_target_lowers_and_roundtrips_a_canonical_root() {
        let root = WorkspaceRootInput::new(
            "repo".parse().unwrap(),
            "file:///work/project".parse::<CanonicalFileUri>().unwrap(),
            RequestedFilesystemAccess::ReadWrite,
            WorkspaceSourcePolicy::new(true, true),
        );
        let input = WorkspaceDefinitionInput::new(
            root,
            Vec::new(),
            WorkspaceCwdSpec::new("repo".parse().unwrap(), WorkspaceRelativePath::default()),
        )
        .unwrap();

        let workspace = lower_workspace(
            input,
            "wr_1".parse::<WorkspaceRevision>().unwrap(),
            WorkspacePathTarget::Posix,
        )
        .unwrap();

        assert_eq!(workspace.primary_root().path(), Path::new("/work/project"));
        assert_eq!(
            uri_from_spec(workspace.primary_root(), WorkspacePathTarget::Posix)
                .unwrap()
                .as_str(),
            "file:///work/project"
        );
    }

    #[test]
    fn explicit_windows_target_lowers_drive_and_unc_roots_and_roundtrips() {
        let input = WorkspaceDefinitionInput::new(
            root_input("drive", "file:///C:/work/project"),
            vec![root_input("unc", "file://server/share/project")],
            cwd("unc", "src"),
        )
        .unwrap();

        let workspace = lower_workspace(input, revision(), WorkspacePathTarget::Windows).unwrap();

        assert_eq!(
            workspace.primary_root().path(),
            Path::new(r"C:\work\project")
        );
        assert_eq!(
            workspace.additional_roots()[0].path(),
            Path::new(r"\\server\share\project")
        );
        assert_eq!(
            uri_from_spec(workspace.primary_root(), WorkspacePathTarget::Windows)
                .unwrap()
                .as_str(),
            "file:///C:/work/project"
        );
        assert_eq!(
            uri_from_spec(
                &workspace.additional_roots()[0],
                WorkspacePathTarget::Windows,
            )
            .unwrap()
            .as_str(),
            "file://server/share/project"
        );
    }

    #[test]
    fn explicit_targets_reject_unsupported_uri_families() {
        assert_eq!(
            lower_workspace(
                input_with_primary("file:///work/project"),
                revision(),
                WorkspacePathTarget::Windows,
            ),
            Err(WorkspaceInputLoweringError::UnsupportedHostFamily),
        );
        assert_eq!(
            lower_workspace(
                input_with_primary("file:///C:/work/project"),
                revision(),
                WorkspacePathTarget::Posix,
            ),
            Err(WorkspaceInputLoweringError::UnsupportedHostFamily),
        );
        assert_eq!(
            lower_workspace(
                input_with_primary("file://server/share/project"),
                revision(),
                WorkspacePathTarget::Posix,
            ),
            Err(WorkspaceInputLoweringError::UnsupportedHostFamily),
        );
    }

    #[test]
    fn posix_lowering_roundtrips_spaces_unicode_percent_and_drive_disambiguation() {
        for (uri, native_path) in [
            (
                "file:///work/a%20b/%E9%A1%B9%E7%9B%AE/100%25",
                "/work/a b/项目/100%",
            ),
            ("file:///C%3A/repo", "/C:/repo"),
        ] {
            let workspace = lower_workspace(
                input_with_primary(uri),
                revision(),
                WorkspacePathTarget::Posix,
            )
            .unwrap();
            assert_eq!(workspace.primary_root().path(), Path::new(native_path));
            assert_eq!(
                uri_from_spec(workspace.primary_root(), WorkspacePathTarget::Posix)
                    .unwrap()
                    .as_str(),
                uri
            );
        }
    }

    #[test]
    fn trusted_native_specs_reject_invalid_paths() {
        for (target, native_path) in [
            (WorkspacePathTarget::Posix, "work/project"),
            (WorkspacePathTarget::Posix, "C:/work/project"),
            (WorkspacePathTarget::Posix, "/work\\project"),
            (WorkspacePathTarget::Posix, "/work//project"),
            (WorkspacePathTarget::Posix, "/work/./project"),
            (WorkspacePathTarget::Posix, "/work/../project"),
            (WorkspacePathTarget::Posix, "/work/project/"),
            (WorkspacePathTarget::Posix, "/work\0project"),
            (WorkspacePathTarget::Windows, "work\\project"),
            (WorkspacePathTarget::Windows, "C:/work/project"),
            (WorkspacePathTarget::Windows, "c:\\work\\project"),
            (WorkspacePathTarget::Windows, "C:\\work\\\\project"),
            (WorkspacePathTarget::Windows, "C:\\work\\.\\project"),
            (WorkspacePathTarget::Windows, "C:\\work\\..\\project"),
            (WorkspacePathTarget::Windows, "C:\\work\\project\\"),
            (WorkspacePathTarget::Windows, "\\server\\share"),
            (WorkspacePathTarget::Windows, "\\\\Server\\share"),
            (WorkspacePathTarget::Windows, "\\\\server\\share\\"),
            (WorkspacePathTarget::Windows, "C:\\work\0project"),
        ] {
            assert_eq!(
                checked_native_uri(Path::new(native_path), target),
                Err(WorkspaceInputLoweringError::InvalidNativePath),
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn trusted_native_specs_reject_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        assert_eq!(
            checked_native_uri(
                Path::new(&OsString::from_vec(vec![b'/', b'w', 0xFF])),
                WorkspacePathTarget::Posix,
            ),
            Err(WorkspaceInputLoweringError::NativePathNotLossless),
        );
    }

    #[test]
    fn workspace_aggregate_keeps_revision_order_and_cwd_and_rejects_invalid_construction() {
        let workspace = Workspace::new(
            revision(),
            root_spec("primary", "/work/primary", WorkspacePathTarget::Posix),
            vec![
                root_spec("first", "/work/first", WorkspacePathTarget::Posix),
                root_spec("second", "/work/second", WorkspacePathTarget::Posix),
            ],
            cwd("second", "nested"),
        )
        .unwrap();
        assert_eq!(workspace.revision(), "wr_1".parse().unwrap());
        assert_eq!(workspace.primary_root().key().as_str(), "primary");
        assert_eq!(workspace.additional_roots()[0].key().as_str(), "first");
        assert_eq!(workspace.additional_roots()[1].key().as_str(), "second");
        assert_eq!(workspace.cwd().root().as_str(), "second");
        assert_eq!(workspace.cwd().relative_path().as_str(), "nested");

        assert_eq!(
            Workspace::new(
                revision(),
                root_spec("primary", "/work/primary", WorkspacePathTarget::Posix),
                vec![root_spec(
                    "primary",
                    "/work/other",
                    WorkspacePathTarget::Posix
                )],
                cwd("primary", ""),
            ),
            Err(WorkspaceConstructionError::DuplicateRootKey),
        );
        assert_eq!(
            Workspace::new(
                revision(),
                root_spec("primary", "/work/primary", WorkspacePathTarget::Posix),
                vec![root_spec(
                    "other",
                    "/work/primary",
                    WorkspacePathTarget::Posix
                )],
                cwd("primary", ""),
            ),
            Err(WorkspaceConstructionError::DuplicateNativePath),
        );
        assert_eq!(
            Workspace::new(
                revision(),
                root_spec("primary", "/work/primary", WorkspacePathTarget::Posix),
                Vec::new(),
                cwd("missing", ""),
            ),
            Err(WorkspaceConstructionError::UnknownCwdRoot),
        );
        assert_eq!(
            Workspace::new(
                revision(),
                root_spec("primary", "/work/primary", WorkspacePathTarget::Posix),
                (1..=16)
                    .map(|index| {
                        root_spec(
                            &format!("root-{index}"),
                            &format!("/work/{index}"),
                            WorkspacePathTarget::Posix,
                        )
                    })
                    .collect(),
                cwd("primary", ""),
            ),
            Err(WorkspaceConstructionError::TooManyRoots),
        );
    }

    #[test]
    fn durable_workspace_debug_and_errors_do_not_expose_root_text() {
        let workspace = lower_workspace(
            input_with_primary("file:///root-secret/private-uri-secret"),
            revision(),
            WorkspacePathTarget::Posix,
        )
        .unwrap();
        let debug = format!("{workspace:?} {:?}", workspace.primary_root());
        assert!(debug.contains("root_count"));
        assert!(debug.contains("cwd_is_workspace_root"));
        assert!(!debug.contains("root-secret"));
        assert!(!debug.contains("private-uri-secret"));
        assert!(!debug.contains("file:///"));

        let error = lower_workspace(
            input_with_primary("file:///root-secret/private-uri-secret"),
            revision(),
            WorkspacePathTarget::Windows,
        )
        .unwrap_err();
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("root-secret"));
        assert!(!error_text.contains("private-uri-secret"));
        assert!(!error_text.contains("file:///"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_resolver_preserves_roles_order_cwd_and_snapshot_binding() {
        let temporary = TempDirectory::new("local-success");
        let primary = temporary.path().join("primary");
        let primary_cwd = primary.join("src");
        let additional = temporary.path().join("additional");
        let additional_cwd = additional.join("nested");
        std::fs::create_dir_all(&primary_cwd).expect("the primary test root is creatable");
        std::fs::create_dir_all(&additional_cwd).expect("the additional test root is creatable");

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                primary.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            vec![root_spec_with(
                "additional",
                additional.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            )],
            cwd("additional", "nested"),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, true, true),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(candidate.revision(), revision());
        assert_eq!(candidate.roots.len(), 2);
        assert_eq!(candidate.roots[0].role, WorkspaceRootRole::Primary);
        assert_eq!(candidate.roots[1].role, WorkspaceRootRole::Additional);
        assert_eq!(candidate.roots[0].key.as_str(), "primary");
        assert_eq!(candidate.roots[1].key.as_str(), "additional");
        assert_eq!(
            candidate.cwd.as_path(),
            std::fs::canonicalize(&additional_cwd).unwrap()
        );

        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let snapshot_copy = Arc::clone(&snapshot);
        assert_eq!(snapshot.session_id(), session_id());
        assert_eq!(snapshot.revision(), revision());
        assert_eq!(snapshot_copy.cwd, snapshot.cwd);
        assert_eq!(snapshot_copy.roots, snapshot.roots);
        assert!(format!("{snapshot:?} {snapshot_copy:?}").contains("root_count"));
        assert!(!format!("{snapshot:?}").contains(primary.to_str().unwrap()));
        assert!(!format!("{snapshot:?}").contains(&session_id().to_string()));

        let resolved = Arc::clone(&snapshot).into_resolved();
        assert_eq!(resolved.session_id(), session_id());
        assert_eq!(resolved.revision(), revision());
        assert_eq!(resolved.snapshot.roots, snapshot.roots);
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_cwd_can_be_in_primary_or_additional_root() {
        let temporary = TempDirectory::new("cwd-roots");
        let primary = temporary.path().join("primary");
        let primary_cwd = primary.join("src");
        let additional = temporary.path().join("additional");
        let additional_cwd = additional.join("nested");
        std::fs::create_dir_all(&primary_cwd).expect("the primary test root is creatable");
        std::fs::create_dir_all(&additional_cwd).expect("the additional test root is creatable");

        let primary_root = root_spec_with(
            "primary",
            primary.to_str().unwrap(),
            RequestedFilesystemAccess::ReadWrite,
            WorkspaceSourcePolicy::new(false, false),
        );
        let additional_root = root_spec_with(
            "additional",
            additional.to_str().unwrap(),
            RequestedFilesystemAccess::ReadWrite,
            WorkspaceSourcePolicy::new(false, false),
        );
        let primary_workspace = Workspace::new(
            revision(),
            primary_root.clone(),
            vec![additional_root.clone()],
            cwd("primary", "src"),
        )
        .unwrap();
        let additional_workspace = Workspace::new(
            revision(),
            primary_root,
            vec![additional_root],
            cwd("additional", "nested"),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let primary_candidate = resolver
            .resolve(session_id(), &primary_workspace)
            .await
            .unwrap();
        let additional_candidate = resolver
            .resolve(session_id(), &additional_workspace)
            .await
            .unwrap();
        assert_eq!(
            primary_candidate.cwd.as_path(),
            std::fs::canonicalize(primary_cwd).unwrap()
        );
        assert_eq!(
            additional_candidate.cwd.as_path(),
            std::fs::canonicalize(additional_cwd).unwrap()
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_nested_cwd_resolves_capability_relative_through_the_captured_root() {
        let temporary = TempDirectory::new("cwd-proof");
        let primary = temporary.path().join("primary");
        let nested = primary.join("src/nested");
        std::fs::create_dir_all(&nested).expect("the nested cwd directory is creatable");

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                primary.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", "src/nested"),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let adapter = RecordingLocalAdapter::new();
        let proof_calls = adapter.cwd_proof_calls();
        let resolver = resolver_with_adapters(
            task_context.clone(),
            adapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(
            candidate.cwd.as_path(),
            std::fs::canonicalize(&nested).unwrap()
        );
        assert_eq!(candidate.roots.len(), 1);
        assert_eq!(
            proof_calls.load(Ordering::SeqCst),
            1,
            "the production path phase must run the capability-relative cwd proof"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn requested_access_and_source_flags_are_tightened_independently() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, true, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let root = &candidate.roots[0];
        assert_eq!(root.filesystem, WorkspaceFilesystemGrant::ReadOnly);
        assert!(root.prompt_source);
        assert!(!root.skill_source);
        assert_eq!(root.trust.level(), WorkspaceTrustLevel::Trusted);
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restricted_production_authority_is_untrusted_and_cannot_widen_access() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            Arc::new(RestrictedWorkspaceAuthority),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let root = &candidate.roots[0];
        assert_eq!(root.trust.level(), WorkspaceTrustLevel::Untrusted);
        assert_eq!(root.filesystem, WorkspaceFilesystemGrant::None);
        assert!(!root.prompt_source);
        assert!(!root.skill_source);
        assert!(candidate.prompt_capture_context().roots().is_empty());
        assert!(candidate.skill_capture_context().roots().is_empty());
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_resolver_remains_fail_closed_with_none_grants() {
        let temporary = TempDirectory::new("default-resolver");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let resolver = WorkspaceResolver::new(task_context.clone());
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let resolved = &candidate.roots[0];
        assert_eq!(resolved.trust.level(), WorkspaceTrustLevel::Untrusted);
        assert_eq!(resolved.filesystem, WorkspaceFilesystemGrant::None);
        assert!(!resolved.prompt_source);
        assert!(!resolved.skill_source);
        assert!(candidate.prompt_capture_context().roots().is_empty());
        assert!(candidate.skill_capture_context().roots().is_empty());
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_access_opt_in_tightens_every_root_to_read_only_and_never_grants_write() {
        let temporary = TempDirectory::new("read-access-ceiling");
        let primary = temporary.path().join("primary");
        let additional = temporary.path().join("additional");
        std::fs::create_dir_all(&primary).expect("the primary test root is creatable");
        std::fs::create_dir_all(&additional).expect("the additional test root is creatable");

        // The primary root requests ReadOnly and the additional root requests
        // ReadWrite; the opt-in ceiling must map both to exactly ReadOnly.
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                primary.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(true, true),
            ),
            vec![root_spec_with(
                "additional",
                additional.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            )],
            cwd("primary", ""),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let (resolver, _control) = WorkspaceResolver::new_with_read_access(task_context.clone());
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(candidate.roots.len(), 2);
        assert_eq!(
            workspace.primary_root().requested_access(),
            RequestedFilesystemAccess::ReadOnly
        );
        assert_eq!(
            workspace.additional_roots()[0].requested_access(),
            RequestedFilesystemAccess::ReadWrite
        );
        for root in candidate.roots.iter() {
            // Requested ReadOnly stays ReadOnly; requested ReadWrite is tightened
            // to ReadOnly. Write never appears.
            assert_eq!(root.filesystem, WorkspaceFilesystemGrant::ReadOnly);
            assert_ne!(root.filesystem, WorkspaceFilesystemGrant::ReadWrite);
            // Source ceilings stay false: the opt-in is filesystem read only.
            assert!(!root.prompt_source);
            assert!(!root.skill_source);
            // Trust is Restricted, not Trusted, at the fixed non-zero revision.
            assert_eq!(root.trust.level(), WorkspaceTrustLevel::Restricted);
            assert_eq!(root.trust.revision().get(), 1);
        }
        assert!(candidate.prompt_capture_context().roots().is_empty());
        assert!(candidate.skill_capture_context().roots().is_empty());
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_access_control_revoke_is_idempotent_and_denies_only_the_revoked_session() {
        let temporary = TempDirectory::new("read-access-revoke");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::write(root.join("notes.txt"), b"read access body").unwrap();
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let revoked_session = session_id();
        let other_session = "ses_22222222222222222222222222222222"
            .parse()
            .expect("the other test session id is canonical");
        let relative = "notes.txt".parse().unwrap();

        let task_context = initialized_context().await;
        let (resolver, control) = WorkspaceResolver::new_with_read_access(task_context.clone());

        // Before any revocation both Sessions resolve exactly ReadOnly and the tool read
        // authorizes through the real temp capability.
        for session in [revoked_session, other_session] {
            let candidate = resolver.resolve(session, &workspace).await.unwrap();
            assert_eq!(
                candidate.roots[0].filesystem,
                WorkspaceFilesystemGrant::ReadOnly
            );
            let snapshot = candidate
                .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
                .unwrap();
            assert!(
                snapshot
                    .tool_context()
                    .access()
                    .authorize_read(&relative)
                    .is_ok()
            );
        }

        // Revocation is idempotent: revoking the same Session twice keeps exactly the same
        // permanent restriction.
        control.revoke(revoked_session);
        control.revoke(revoked_session);

        // After revocation the same Session resolves filesystem None (never AuthorityDenied,
        // never ReadOnly again): Prompt/Skill stay false and trust stays Restricted at the
        // same fixed revision, and the tool read is denied.
        let revoked_candidate = resolver.resolve(revoked_session, &workspace).await.unwrap();
        assert_eq!(
            revoked_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::None
        );
        assert!(!revoked_candidate.roots[0].prompt_source);
        assert!(!revoked_candidate.roots[0].skill_source);
        assert_eq!(
            revoked_candidate.roots[0].trust.level(),
            WorkspaceTrustLevel::Restricted
        );
        assert_eq!(revoked_candidate.roots[0].trust.revision().get(), 1);
        let revoked_snapshot = revoked_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        assert_eq!(
            revoked_snapshot
                .tool_context()
                .access()
                .authorize_read(&relative)
                .unwrap_err(),
            WorkspaceAccessError::NotAuthorized
        );

        // Another Session is untouched: it still resolves exactly ReadOnly and the tool read
        // still authorizes.
        let other_candidate = resolver.resolve(other_session, &workspace).await.unwrap();
        assert_eq!(
            other_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::ReadOnly
        );
        assert_eq!(
            other_candidate.roots[0].trust.level(),
            WorkspaceTrustLevel::Restricted
        );
        let other_snapshot = other_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        assert!(
            other_snapshot
                .tool_context()
                .access()
                .authorize_read(&relative)
                .is_ok()
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_authorization_succeeds_only_with_the_read_access_opt_in_resolver() {
        let temporary = TempDirectory::new("read-access-tool");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::write(root.join("notes.txt"), b"read access body").unwrap();
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let default_resolver = WorkspaceResolver::new(task_context.clone());
        let default_candidate = default_resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap();
        assert_eq!(
            default_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::None
        );
        let default_snapshot = default_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        assert_eq!(
            default_snapshot
                .tool_context()
                .access()
                .authorize_read(&"notes.txt".parse().unwrap())
                .unwrap_err(),
            WorkspaceAccessError::NotAuthorized
        );

        let read_resolver = WorkspaceResolver::new_with_read_access(task_context.clone()).0;
        let read_candidate = read_resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap();
        assert_eq!(
            read_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::ReadOnly
        );
        let read_snapshot = read_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let authorized = read_snapshot
            .tool_context()
            .access()
            .authorize_read(&"notes.txt".parse().unwrap())
            .unwrap();
        // A known regular fixture file: read it through the nonblocking
        // capability open with the test's own `Read::read_to_end`.
        let mut file = authorized
            .open_nonblocking()
            .expect("the regular fixture file opens nonblocking");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)
            .expect("the regular fixture file reads to the end");
        assert_eq!(bytes, b"read access body");
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn canonical_duplicate_via_symlink_alias_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = TempDirectory::new("duplicate-symlink");
        let real = temporary.path().join("real");
        let alias = temporary.path().join("alias");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, &alias).unwrap();
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                real.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            vec![root_spec_with(
                "alias",
                alias.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            )],
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = WorkspaceResolver::new(task_context.clone());
        assert_eq!(
            resolver
                .resolve(session_id(), &workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::DuplicateRoot
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nested_canonical_roots_are_rejected_without_string_prefix_logic() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/project",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            vec![(
                "nested",
                "/deterministic/project-two",
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            )],
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity()
                .mapping("/deterministic/project-two", "/deterministic/project/child"),
            Arc::new(RestrictedWorkspaceAuthority),
        );
        assert_eq!(
            resolver
                .resolve(session_id(), &workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::OverlappingRoots
        );
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn cwd_symlink_escapes_fail_through_the_capability_relative_proof() {
        use std::os::unix::fs::symlink;

        let temporary = TempDirectory::new("cwd-links");
        let primary = temporary.path().join("primary");
        let additional = temporary.path().join("additional");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&additional).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // Both escape flavors are final-component symlinks: one to an absolute
        // path outside the roots, one into the other declared root. Either way
        // the ambient canonical text would resolve, so only the capability-
        // relative open through the captured root can reject them.
        symlink(&outside, primary.join("escape")).unwrap();
        symlink(&additional, primary.join("to-additional")).unwrap();

        let roots = || {
            (
                root_spec_with(
                    "primary",
                    primary.to_str().unwrap(),
                    RequestedFilesystemAccess::ReadWrite,
                    WorkspaceSourcePolicy::new(false, false),
                ),
                vec![root_spec_with(
                    "additional",
                    additional.to_str().unwrap(),
                    RequestedFilesystemAccess::ReadOnly,
                    WorkspaceSourcePolicy::new(false, false),
                )],
            )
        };
        let (primary_root, additional_roots) = roots();
        let escape_workspace = Workspace::new(
            revision(),
            primary_root,
            additional_roots.clone(),
            cwd("primary", "escape"),
        )
        .unwrap();
        let (primary_root, additional_roots) = roots();
        let into_other_root_workspace = Workspace::new(
            revision(),
            primary_root,
            additional_roots,
            cwd("primary", "to-additional"),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let resolver = WorkspaceResolver::new(task_context.clone());
        for workspace in [escape_workspace, into_other_root_workspace] {
            assert_eq!(
                resolver
                    .resolve(session_id(), &workspace)
                    .await
                    .unwrap_err(),
                WorkspaceResolveError::CanonicalizationFailed
            );
        }
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deterministic_cwd_outside_and_mismatch_keep_their_semantics() {
        // The capability proof is production-only; the deterministic adapter
        // keeps its pure path-text mapping, so the containment checks still have
        // deterministic coverage for cwd canonical text that is outside every
        // root or inside a root that is not the declared cwd root.
        let workspace_for = |relative: &str| {
            resolver_workspace(
                (
                    "primary",
                    "/deterministic/primary",
                    RequestedFilesystemAccess::ReadWrite,
                    WorkspaceSourcePolicy::new(false, false),
                ),
                vec![(
                    "additional",
                    "/deterministic/additional",
                    RequestedFilesystemAccess::ReadOnly,
                    WorkspaceSourcePolicy::new(false, false),
                )],
                "primary",
                relative,
            )
        };
        let outside_workspace = workspace_for("x");
        let mismatch_workspace = workspace_for("y");

        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity()
                .mapping("/deterministic/primary/x", "/elsewhere")
                .mapping("/deterministic/primary/y", "/deterministic/additional"),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        assert_eq!(
            resolver
                .resolve(session_id(), &outside_workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::CwdOutsideRoots
        );
        assert_eq!(
            resolver
                .resolve(session_id(), &mismatch_workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::CwdRootMismatch
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cwd_proof_rejects_root_replaced_between_capture_and_proof() {
        let temporary = TempDirectory::new("cwd-replacement-race");
        let primary = temporary.path().join("primary");
        let displaced = temporary.path().join("displaced");
        std::fs::create_dir_all(primary.join("src")).expect("the cwd directory is creatable");

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                primary.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", "src"),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            ReplacingRootLocalAdapter::new(primary.clone(), displaced),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, false, false),
        );
        let error = resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap_err();
        // The canonical path text is unchanged by the replacement, so only the
        // same-file identity proof can detect that the cwd belongs to the
        // displaced captured root rather than the replacement at the same path.
        assert_eq!(error, WorkspaceResolveError::CanonicalizationFailed);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains(temporary.path().to_str().unwrap()));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_and_non_directory_roots_use_typed_errors() {
        let temporary = TempDirectory::new("root-errors");
        let missing = temporary.path().join("missing");
        let missing_workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                missing.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let file = temporary.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        let file_workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                file.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = WorkspaceResolver::new(task_context.clone());
        assert_eq!(
            resolver
                .resolve(session_id(), &missing_workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::RootUnavailable
        );
        assert_eq!(
            resolver
                .resolve(session_id(), &file_workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::RootNotDirectory
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_context_authorizes_cwd_relative_reads_and_binds_the_containing_root() {
        let temporary = TempDirectory::new("tool-read");
        let primary = temporary.path().join("primary");
        let additional = temporary.path().join("additional");
        let additional_cwd = additional.join("nested");
        std::fs::create_dir_all(&primary).expect("the primary test root is creatable");
        std::fs::create_dir_all(&additional_cwd).expect("the additional test root is creatable");
        std::fs::write(additional_cwd.join("data.txt"), b"capability hello").unwrap();
        std::fs::write(primary.join("only-in-primary.txt"), b"unreachable").unwrap();

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                primary.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            vec![root_spec_with(
                "additional",
                additional.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            )],
            cwd("additional", "nested"),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let context = snapshot.tool_context();
        let cloned_context = context.clone();
        let access = cloned_context.access();
        assert_eq!(access.session_id(), session_id());
        assert_eq!(access.roots().len(), 2);
        assert_eq!(
            access.cwd().as_path(),
            std::fs::canonicalize(&additional_cwd).unwrap()
        );

        // The cwd-relative target is resolved inside the exact root containing the
        // captured canonical cwd, with the cwd's position in that root prepended.
        let authorized: AuthorizedWorkspaceReadPath =
            access.authorize_read(&"data.txt".parse().unwrap()).unwrap();
        assert_eq!(authorized.relative_path(), Path::new("nested/data.txt"));
        // The authorized target is a known regular fixture file, so the test reads it
        // through the nonblocking capability open and `Read::read_to_end` itself.
        let mut file = authorized
            .open_nonblocking()
            .expect("the regular fixture file opens nonblocking");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)
            .expect("the regular fixture file reads to the end");
        assert_eq!(bytes, b"capability hello");

        // Other roots are not directly addressable through this cwd-relative schema:
        // the target resolves inside the cwd's root only, where the primary-root file
        // does not exist, so the capability open reports it unavailable.
        let unreachable = access
            .authorize_read(&"only-in-primary.txt".parse().unwrap())
            .unwrap();
        assert_eq!(
            unreachable.relative_path(),
            Path::new("nested/only-in-primary.txt")
        );
        assert_eq!(
            unreachable.open_nonblocking().unwrap_err(),
            WorkspaceAccessError::Unavailable
        );

        // A cwd at the root itself resolves targets directly under that root.
        let root_cwd_workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                primary.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let root_cwd_candidate = resolver
            .resolve(session_id(), &root_cwd_workspace)
            .await
            .unwrap();
        let root_cwd_snapshot = root_cwd_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let root_cwd_tool_context = root_cwd_snapshot.tool_context();
        let root_cwd_access = root_cwd_tool_context.access();
        let root_authorized = root_cwd_access
            .authorize_read(&"only-in-primary.txt".parse().unwrap())
            .unwrap();
        assert_eq!(
            root_authorized.relative_path(),
            Path::new("only-in-primary.txt")
        );
        // A known regular fixture file: read it through the nonblocking capability
        // open with the test's own `Read::read_to_end`.
        let mut file = root_authorized
            .open_nonblocking()
            .expect("the regular fixture file opens nonblocking");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)
            .expect("the regular fixture file reads to the end");
        assert_eq!(bytes, b"unreachable");

        let debug = format!("{context:?} {access:?} {authorized:?}");
        assert!(!debug.contains(temporary.path().to_str().unwrap()));
        assert!(!debug.contains("nested/data.txt"));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_authorization_denies_without_a_readable_grant_and_rejects_root_paths() {
        let temporary = TempDirectory::new("tool-deny");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::write(root.join("notes.txt"), b"secret body").unwrap();

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::None, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        let error = access
            .authorize_read(&"notes.txt".parse().unwrap())
            .unwrap_err();
        assert_eq!(error, WorkspaceAccessError::NotAuthorized);
        assert_eq!(
            access
                .authorize_read(&WorkspaceRelativePath::default())
                .unwrap_err(),
            WorkspaceAccessError::InvalidPath
        );
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("notes.txt"));
        assert!(!error_text.contains(root.to_str().unwrap()));
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn capability_read_rejects_symlink_escape_outside_the_root() {
        use std::os::unix::fs::symlink;

        let temporary = TempDirectory::new("tool-escape");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::create_dir_all(&outside).expect("the outside directory is creatable");
        std::fs::write(outside.join("secret.txt"), b"outside secret").unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        let authorized = access
            .authorize_read(&"escape/secret.txt".parse().unwrap())
            .unwrap();
        let error = authorized.open_nonblocking().unwrap_err();
        assert_eq!(error, WorkspaceAccessError::OpenFailed);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("secret"));
        assert!(!error_text.contains("escape"));
        // The target file itself exists and is readable through ordinary std access.
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"outside secret"
        );
        task_context.shutdown().await;
    }

    fn listed_directory_names(dir: &cap_std::fs::Dir) -> Vec<String> {
        let mut names = dir
            .entries()
            .expect("the directory lists its entries")
            .map(|entry| {
                entry
                    .expect("a directory entry is readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_directory_authorization_opens_cwd_and_nested_directories() {
        let temporary = TempDirectory::new("directory-read");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("the nested directory is creatable");
        std::fs::write(root.join("notes.txt"), b"top").unwrap();
        std::fs::write(root.join("sub/data.txt"), b"data").unwrap();

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        // With the cwd at the root the empty target names the captured cwd
        // directory and opens to list the real direct entries.
        let cwd_dir = access
            .authorize_read_directory(&WorkspaceRelativePath::default())
            .unwrap();
        assert_eq!(cwd_dir.relative_path(), Path::new(""));
        assert_eq!(
            listed_directory_names(&cwd_dir.open().unwrap()),
            vec!["notes.txt".to_string(), "sub".to_string()]
        );

        // A relative nested directory also opens through the same root capability.
        let nested = access
            .authorize_read_directory(&"sub".parse().unwrap())
            .unwrap();
        assert_eq!(nested.relative_path(), Path::new("sub"));
        assert_eq!(
            listed_directory_names(&nested.open().unwrap()),
            vec!["data.txt".to_string()]
        );

        // Debug is fixed redacted and leaks no path text.
        assert_eq!(
            format!("{cwd_dir:?}"),
            "AuthorizedWorkspaceReadDirectory { .. }"
        );
        let debug = format!("{access:?} {cwd_dir:?} {nested:?}");
        assert!(!debug.contains(temporary.path().to_str().unwrap()));
        assert!(!debug.contains("notes.txt"));
        assert!(!debug.contains("data.txt"));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_directory_authorization_binds_empty_to_cwd_inside_the_root() {
        let temporary = TempDirectory::new("directory-read-cwd");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(root.join("nested").join("deep"))
            .expect("the nested directory is creatable");
        std::fs::write(root.join("top.txt"), b"top").unwrap();
        std::fs::write(root.join("nested/inside.txt"), b"inside").unwrap();
        std::fs::write(root.join("nested/deep/bottom.txt"), b"bottom").unwrap();

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", "nested"),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        // The empty target binds to the captured cwd, not the root: the listed
        // entries are the cwd's own, and the capability-relative target carries
        // the cwd's position in the root.
        let cwd_dir = access
            .authorize_read_directory(&WorkspaceRelativePath::default())
            .unwrap();
        assert_eq!(cwd_dir.relative_path(), Path::new("nested"));
        assert_eq!(
            listed_directory_names(&cwd_dir.open().unwrap()),
            vec!["deep".to_string(), "inside.txt".to_string()]
        );

        // A nested relative target is prepended with the cwd's position.
        let deep = access
            .authorize_read_directory(&"deep".parse().unwrap())
            .unwrap();
        assert_eq!(deep.relative_path(), Path::new("nested/deep"));
        assert_eq!(
            listed_directory_names(&deep.open().unwrap()),
            vec!["bottom.txt".to_string()]
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_directory_authorization_denies_without_a_readable_grant() {
        let temporary = TempDirectory::new("directory-deny");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::None, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        // No readable grant denies even the empty (cwd) target.
        assert_eq!(
            access
                .authorize_read_directory(&WorkspaceRelativePath::default())
                .unwrap_err(),
            WorkspaceAccessError::NotAuthorized
        );
        assert_eq!(
            access
                .authorize_read_directory(&"sub".parse().unwrap())
                .unwrap_err(),
            WorkspaceAccessError::NotAuthorized
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_directory_authorization_error_semantics_are_frozen() {
        let (_, dir) = test_capability_scratch();
        let root = WorkspaceAccessRoot {
            canonical_path: CanonicalWorkspacePath::new(PathBuf::from("/deterministic/project")),
            dir,
            filesystem: WorkspaceFilesystemGrant::ReadOnly,
        };

        // A cwd outside every captured root makes the read basis unavailable.
        let outside = WorkspaceAccessView {
            session_id: session_id(),
            cwd: CanonicalWorkspacePath::new(PathBuf::from("/deterministic/elsewhere")),
            roots: Arc::from(vec![root.clone()]),
        };
        assert_eq!(
            outside
                .authorize_read_directory(&WorkspaceRelativePath::default())
                .unwrap_err(),
            WorkspaceAccessError::Unavailable
        );

        // A cwd whose position inside its root is not fully normal is invalid.
        let non_normal = WorkspaceAccessView {
            session_id: session_id(),
            cwd: CanonicalWorkspacePath::new(PathBuf::from("/deterministic/project/../outside")),
            roots: Arc::from(vec![root]),
        };
        assert_eq!(
            non_normal
                .authorize_read_directory(&"sub".parse().unwrap())
                .unwrap_err(),
            WorkspaceAccessError::InvalidPath
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn capability_directory_read_rejects_symlink_escape_outside_the_root() {
        use std::os::unix::fs::symlink;

        let temporary = TempDirectory::new("tool-dir-escape");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::create_dir_all(&outside).expect("the outside directory is creatable");
        std::fs::write(outside.join("secret.txt"), b"outside secret").unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        // The target text is fully normal, so authorization succeeds, but the
        // capability open fails closed on the escaping symlink.
        let authorized = access
            .authorize_read_directory(&"escape".parse().unwrap())
            .unwrap();
        let error = authorized.open().unwrap_err();
        assert_eq!(error, WorkspaceAccessError::OpenFailed);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("secret"));
        assert!(!error_text.contains("escape"));
        // The target directory itself is still readable through ordinary std access.
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"outside secret"
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn candidate_revalidation_detects_root_replacement_identity() {
        let temporary = TempDirectory::new("root-replacement");
        let root = temporary.path().join("root");
        let displaced = temporary.path().join("displaced");
        std::fs::create_dir_all(&root).expect("the test root is creatable");

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let task_context = initialized_context().await;
        let resolver = WorkspaceResolver::new(task_context.clone());
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();

        // Replace the root with a different directory at the same path: the canonical
        // path text is unchanged, but the captured safe identity must differ.
        std::fs::rename(&root, &displaced).expect("the old root is displaceable");
        std::fs::create_dir_all(&root).expect("the replacement root is creatable");

        let fresh = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(
            candidate.roots[0].canonical_path,
            fresh.roots[0].canonical_path
        );
        assert_ne!(candidate.roots[0].identity, fresh.roots[0].identity);
        assert!(
            !resolver
                .revalidate_candidate(&candidate, &workspace)
                .await
                .unwrap()
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn candidate_requires_revalidation_tracks_readable_grants_and_sources() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;

        // Default fail-closed resolution (filesystem None, no sources) stays final.
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_with_ceiling(WorkspaceFilesystemGrant::None, true, true),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(
            candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::None
        );
        assert!(!candidate.roots[0].prompt_source);
        assert!(!candidate.roots[0].skill_source);
        assert!(!candidate.requires_revalidation());

        // A readable filesystem grant alone (Prompt/Skill sources still false) must
        // revalidate: this is the production read_file authority shape.
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(
            candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::ReadOnly
        );
        assert!(!candidate.roots[0].prompt_source);
        assert!(!candidate.roots[0].skill_source);
        assert!(candidate.requires_revalidation());

        // A granted Prompt source requires revalidation even when no filesystem grant
        // read of its own would be fresh without the revalidation boundary.
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, true, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert!(candidate.roots[0].prompt_source);
        assert!(!candidate.roots[0].skill_source);
        assert!(candidate.requires_revalidation());

        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_and_skill_capture_contexts_cannot_cross_authorize() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            vec![(
                "additional",
                "/deterministic/additional",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            )],
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_by_key(),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let prompt_context = candidate.prompt_capture_context();
        let skill_context = candidate.skill_capture_context();
        assert_eq!(prompt_context.roots().len(), 1);
        assert_eq!(skill_context.roots().len(), 1);
        let primary_key = "primary".parse().unwrap();
        let additional_key = "additional".parse().unwrap();
        assert_eq!(
            prompt_context
                .capture(
                    &primary_key,
                    "instructions.md".parse().unwrap(),
                    Arc::<str>::from("immutable prompt body"),
                )
                .as_ref()
                .unwrap()
                .content(),
            "immutable prompt body"
        );
        assert!(matches!(
            prompt_context.capture(
                &additional_key,
                "instructions.md".parse().unwrap(),
                Arc::<str>::from("wrong kind"),
            ),
            Err(WorkspaceSourceCaptureError::SourceKindNotAuthorized)
        ));
        assert!(matches!(
            prompt_context.capture(
                &primary_key,
                WorkspaceRelativePath::default(),
                Arc::<str>::from("root location is not a source"),
            ),
            Err(WorkspaceSourceCaptureError::InvalidRelativeLocation)
        ));

        let prompt = prompt_context
            .capture(
                &primary_key,
                "instructions.md".parse().unwrap(),
                Arc::<str>::from("immutable prompt body"),
            )
            .unwrap();
        let skill = skill_context
            .capture(
                &additional_key,
                "skill.bin".parse().unwrap(),
                Arc::<[u8]>::from(vec![1_u8, 2, 3]),
            )
            .unwrap();
        assert!(matches!(
            skill_context.capture(
                &primary_key,
                "skill.bin".parse().unwrap(),
                Arc::<[u8]>::from(vec![9_u8]),
            ),
            Err(WorkspaceSourceCaptureError::SourceKindNotAuthorized)
        ));
        let snapshot = candidate
            .finish(
                Arc::<[CapturedWorkspacePromptSource]>::from(vec![prompt]),
                Arc::<[CapturedWorkspaceSkillSource]>::from(vec![skill]),
            )
            .unwrap();
        assert_eq!(snapshot.prompt_sources.len(), 1);
        assert_eq!(snapshot.skill_sources.len(), 1);
        assert_eq!(
            snapshot.prompt_sources[0].content(),
            "immutable prompt body"
        );
        assert_eq!(snapshot.skill_sources[0].bytes().as_ref(), &[1, 2, 3]);
        assert_eq!(
            snapshot.prompt_sources[0]
                .authorization()
                .root_key()
                .as_str(),
            "primary"
        );
        assert_eq!(
            snapshot.skill_sources[0]
                .authorization()
                .root_key()
                .as_str(),
            "additional"
        );
        let prompt_projection = snapshot.prompt_context();
        let skill_projection = snapshot.skill_context();
        assert_eq!(prompt_projection.sources().len(), 1);
        assert_eq!(skill_projection.sources().len(), 1);
        let debug = format!("{snapshot:?} {:?}", snapshot.prompt_sources);
        assert!(!debug.contains("immutable prompt body"));
        assert!(!debug.contains("skill.bin"));
        assert!(!debug.contains("/deterministic"));
        assert!(!debug.contains(&session_id().to_string()));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn candidate_capability_basis_rejects_sources_from_an_identical_candidate() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, true, false),
        );
        let candidate_a = resolver.resolve(session_id(), &workspace).await.unwrap();
        let candidate_b = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(candidate_a.roots, candidate_b.roots);
        assert_eq!(candidate_a.cwd, candidate_b.cwd);

        let source = candidate_a
            .prompt_capture_context()
            .capture(
                &"primary".parse().unwrap(),
                "instructions.md".parse().unwrap(),
                Arc::<str>::from("private prompt body"),
            )
            .unwrap();
        let error = candidate_b
            .finish(
                Arc::<[CapturedWorkspacePromptSource]>::from(vec![source]),
                Arc::from(Vec::new()),
            )
            .unwrap_err();
        assert_eq!(error, WorkspaceSnapshotFinishError::AuthorizationMismatch);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("instructions.md"));
        assert!(!error_text.contains("private prompt body"));
        assert!(!error_text.contains("/deterministic/primary"));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn candidate_revalidation_detects_changed_authority_facts() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_authority = Arc::clone(&calls);
        let authority = Arc::new(DeterministicWorkspaceAuthority::new(move |request| {
            let prompt = calls_for_authority.fetch_add(1, Ordering::SeqCst) == 0;
            let roots = request
                .roots()
                .iter()
                .map(|root| {
                    WorkspaceAuthorityRootDecision::new(
                        root.clone(),
                        trust(),
                        WorkspaceFilesystemGrant::ReadWrite,
                        prompt,
                        false,
                    )
                })
                .collect();
            Ok(WorkspaceAuthorityDecision::new(roots))
        }));
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority,
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(candidate.prompt_capture_context().roots().len(), 1);
        assert!(
            !resolver
                .revalidate_candidate(&candidate, &workspace)
                .await
                .unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn candidate_revalidation_detects_changed_canonical_paths() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/declared/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            ChangingCanonicalWorkspacePathAdapter::new(),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, true, false),
        );
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert!(
            !resolver
                .revalidate_candidate(&candidate, &workspace)
                .await
                .unwrap()
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authorized_capture_cannot_be_installed_into_an_unauthorized_candidate() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let authorized_resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, true, false),
        );
        let unauthorized_resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority_with_ceiling(WorkspaceFilesystemGrant::None, false, false),
        );
        let authorized_candidate = authorized_resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap();
        let unauthorized_candidate = unauthorized_resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap();
        assert!(!unauthorized_candidate.roots[0].prompt_source);
        let source = authorized_candidate
            .prompt_capture_context()
            .capture(
                &"primary".parse().unwrap(),
                "instructions.md".parse().unwrap(),
                Arc::<str>::from("authorized prompt body"),
            )
            .unwrap();

        let error = unauthorized_candidate
            .finish(
                Arc::<[CapturedWorkspacePromptSource]>::from(vec![source]),
                Arc::from(Vec::new()),
            )
            .unwrap_err();
        assert_eq!(error, WorkspaceSnapshotFinishError::AuthorizationMismatch);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("instructions.md"));
        assert!(!error_text.contains("authorized prompt body"));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_unavailable_and_denied_remain_distinct() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        for expected in [
            (
                WorkspaceAuthorityError::Unavailable,
                WorkspaceResolveError::AuthorityUnavailable,
            ),
            (
                WorkspaceAuthorityError::Denied,
                WorkspaceResolveError::AuthorityDenied,
            ),
        ] {
            let task_context = initialized_context().await;
            let authority = Arc::new(DeterministicWorkspaceAuthority::new(move |_| {
                Err(expected.0)
            }));
            let resolver = resolver_with_adapters(
                task_context.clone(),
                DeterministicWorkspacePathAdapter::identity(),
                authority,
            );
            assert_eq!(
                resolver
                    .resolve(session_id(), &workspace)
                    .await
                    .unwrap_err(),
                expected.1
            );
            task_context.shutdown().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adapter_decision_shape_mismatch_closes_owner_and_redacts_error() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let authority = Arc::new(DeterministicWorkspaceAuthority::new(|request| {
            let expected = &request.roots()[0];
            let stale = WorkspaceAuthorityRootRequest::new(
                "wrong-key".parse().unwrap(),
                expected.role,
                expected.canonical_path.clone(),
                expected.requested_access,
                expected.sources,
            );
            Ok(WorkspaceAuthorityDecision::new(vec![
                WorkspaceAuthorityRootDecision::new(
                    stale,
                    trust(),
                    WorkspaceFilesystemGrant::None,
                    false,
                    false,
                ),
            ]))
        }));
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority,
        );
        let error = resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap_err();
        assert_eq!(error, WorkspaceResolveError::InternalDispatchUnavailable);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("wrong-key"));
        assert_eq!(
            resolver
                .resolve(session_id(), &workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::Closing
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_authority_request_cannot_reuse_key_and_role_after_root_changes() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let authority = Arc::new(DeterministicWorkspaceAuthority::new(|request| {
            let expected = &request.roots()[0];
            let stale = WorkspaceAuthorityRootRequest::new(
                expected.key.clone(),
                expected.role,
                CanonicalWorkspacePath::new(PathBuf::from("/stale/canonical/root")),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            );
            Ok(WorkspaceAuthorityDecision::new(vec![
                WorkspaceAuthorityRootDecision::new(
                    stale,
                    trust(),
                    WorkspaceFilesystemGrant::ReadWrite,
                    true,
                    true,
                ),
            ]))
        }));
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::identity(),
            authority,
        );
        let error = resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap_err();
        assert_eq!(error, WorkspaceResolveError::InternalDispatchUnavailable);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("/stale/canonical/root"));
        assert_eq!(
            resolver
                .resolve(session_id(), &workspace)
                .await
                .unwrap_err(),
            WorkspaceResolveError::Closing
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_panic_closes_owner_and_redacts_panic_payload() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::panicking(),
            Arc::new(RestrictedWorkspaceAuthority),
        );
        let error = resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap_err();
        assert_eq!(error, WorkspaceResolveError::InternalDispatchUnavailable);
        let error_text = format!("{error:?} {error}");
        assert!(!error_text.contains("deterministic path adapter panic payload"));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_path_wait_keeps_owner_tracked_work_until_named_barrier_release() {
        let workspace = resolver_workspace(
            (
                "primary",
                "/deterministic/primary",
                RequestedFilesystemAccess::ReadOnly,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            "primary",
            "",
        );
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let entered_for_waiter = Arc::clone(&entered);
        let entered_waiter = thread::spawn(move || {
            entered_for_waiter.wait();
        });
        let task_context = initialized_context().await;
        let resolver = resolver_with_adapters(
            task_context.clone(),
            DeterministicWorkspacePathAdapter::blocking(Arc::clone(&entered), Arc::clone(&release)),
            Arc::new(RestrictedWorkspaceAuthority),
        );
        let mut resolve = Box::pin(resolver.resolve(session_id(), &workspace));
        assert!(poll_once_pending(resolve.as_mut()).await);
        entered_waiter
            .join()
            .expect("the named entry barrier released");
        drop(resolve);

        let mut shutdown = Box::pin(task_context.shutdown());
        assert!(poll_once_pending(shutdown.as_mut()).await);
        release.wait();
        shutdown.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_resolver_intersects_requested_access_and_read_resolver_stays_read_only() {
        let temporary = TempDirectory::new("write-access-intersection");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::write(root.join("notes.txt"), b"body").unwrap();
        let workspace_for = |requested_access| {
            Workspace::new(
                revision(),
                root_spec_with(
                    "primary",
                    root.to_str().unwrap(),
                    requested_access,
                    WorkspaceSourcePolicy::new(true, true),
                ),
                Vec::new(),
                cwd("primary", ""),
            )
            .unwrap()
        };
        let read_only_workspace = workspace_for(RequestedFilesystemAccess::ReadOnly);
        let read_write_workspace = workspace_for(RequestedFilesystemAccess::ReadWrite);
        let relative = "notes.txt".parse().unwrap();

        let task_context = initialized_context().await;

        // The read opt-in resolver keeps its exact ReadOnly ceiling: a requested ReadWrite is
        // tightened to ReadOnly, reads stay authorized, and write authorization is denied.
        let (read_resolver, _read_control) =
            WorkspaceResolver::new_with_read_access(task_context.clone());
        let read_candidate = read_resolver
            .resolve(session_id(), &read_write_workspace)
            .await
            .unwrap();
        assert_eq!(
            read_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::ReadOnly
        );
        let read_snapshot = read_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = read_snapshot.tool_context();
        let read_access = tool_context.access();
        assert!(read_access.authorize_read(&relative).is_ok());
        assert_eq!(
            read_access.authorize_write(&relative).unwrap_err(),
            WorkspaceWriteError::NotAuthorized
        );

        // The write opt-in resolver raises the ceiling to ReadWrite, but the
        // requested-access intersection stays authoritative: a requested ReadOnly root still
        // ends exactly ReadOnly with the same Restricted trust / false source ceilings, and
        // its write route is denied.
        let (write_resolver, _write_control) =
            WorkspaceResolver::new_with_write_access(task_context.clone());
        let readonly_candidate = write_resolver
            .resolve(session_id(), &read_only_workspace)
            .await
            .unwrap();
        assert_eq!(
            readonly_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::ReadOnly
        );
        assert!(!readonly_candidate.roots[0].prompt_source);
        assert!(!readonly_candidate.roots[0].skill_source);
        assert_eq!(
            readonly_candidate.roots[0].trust.level(),
            WorkspaceTrustLevel::Restricted
        );
        let readonly_snapshot = readonly_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        assert_eq!(
            readonly_snapshot
                .tool_context()
                .access()
                .authorize_write(&relative)
                .unwrap_err(),
            WorkspaceWriteError::NotAuthorized
        );

        // A requested ReadWrite root under the write resolver resolves exactly ReadWrite
        // (Prompt/Skill still false, trust Restricted at the fixed revision), and both the
        // read and the write route authorize.
        let write_candidate = write_resolver
            .resolve(session_id(), &read_write_workspace)
            .await
            .unwrap();
        assert_eq!(
            write_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::ReadWrite
        );
        assert!(!write_candidate.roots[0].prompt_source);
        assert!(!write_candidate.roots[0].skill_source);
        assert_eq!(
            write_candidate.roots[0].trust.level(),
            WorkspaceTrustLevel::Restricted
        );
        assert_eq!(write_candidate.roots[0].trust.revision().get(), 1);
        let write_snapshot = write_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = write_snapshot.tool_context();
        let write_access = tool_context.access();
        assert!(write_access.authorize_read(&relative).is_ok());
        assert!(write_access.authorize_write(&relative).is_ok());
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn filesystem_revocation_denies_read_and_write_together() {
        let temporary = TempDirectory::new("write-access-revoke");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::write(root.join("notes.txt"), b"body").unwrap();
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();
        let relative = "notes.txt".parse().unwrap();

        let task_context = initialized_context().await;
        let (resolver, control) = WorkspaceResolver::new_with_write_access(task_context.clone());

        // Before revocation the Session resolves exactly ReadWrite and both routes authorize.
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(
            candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::ReadWrite
        );
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();
        assert!(access.authorize_read(&relative).is_ok());
        assert!(access.authorize_write(&relative).is_ok());

        // Revocation is idempotent and permanent: the same Session now resolves filesystem
        // None (never AuthorityDenied, never ReadWrite again), Prompt/Skill stay false and
        // trust stays Restricted, and read and write are denied together.
        control.revoke(session_id());
        control.revoke(session_id());
        let revoked_candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        assert_eq!(
            revoked_candidate.roots[0].filesystem,
            WorkspaceFilesystemGrant::None
        );
        assert!(!revoked_candidate.roots[0].prompt_source);
        assert!(!revoked_candidate.roots[0].skill_source);
        assert_eq!(
            revoked_candidate.roots[0].trust.level(),
            WorkspaceTrustLevel::Restricted
        );
        let revoked_snapshot = revoked_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = revoked_snapshot.tool_context();
        let revoked_access = tool_context.access();
        assert_eq!(
            revoked_access.authorize_read(&relative).unwrap_err(),
            WorkspaceAccessError::NotAuthorized
        );
        assert_eq!(
            revoked_access.authorize_write(&relative).unwrap_err(),
            WorkspaceWriteError::NotAuthorized
        );
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_authorization_denies_read_only_grants_and_root_paths_and_prepends_cwd() {
        let temporary = TempDirectory::new("write-deny");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(root.join("nested")).expect("the nested cwd is creatable");
        std::fs::write(root.join("notes.txt"), b"secret body").unwrap();
        std::fs::write(root.join("nested/data.txt"), b"nested body").unwrap();

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", "nested"),
        )
        .unwrap();
        let task_context = initialized_context().await;

        // A ReadOnly effective grant (the read opt-in shape) denies the write route while
        // leaving the read route untouched: reads are not weakened by the new check.
        let read_only_resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadOnly, false, false),
        );
        let read_only_candidate = read_only_resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap();
        let read_only_snapshot = read_only_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = read_only_snapshot.tool_context();
        let read_only_access = tool_context.access();
        assert!(
            read_only_access
                .authorize_read(&"data.txt".parse().unwrap())
                .is_ok()
        );
        assert_eq!(
            read_only_access
                .authorize_write(&"data.txt".parse().unwrap())
                .unwrap_err(),
            WorkspaceWriteError::NotAuthorized
        );

        // An exactly-ReadWrite grant authorizes the write route, resolves the target inside
        // the exact root holding the captured cwd (cwd position prepended), and the prepared
        // existing carrier replaces the real file through the capability.
        let write_resolver = resolver_with_adapters(
            task_context.clone(),
            LocalWorkspacePathAdapter,
            authority_with_ceiling(WorkspaceFilesystemGrant::ReadWrite, false, false),
        );
        let write_candidate = write_resolver
            .resolve(session_id(), &workspace)
            .await
            .unwrap();
        let write_snapshot = write_candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = write_snapshot.tool_context();
        let write_access = tool_context.access();
        let authorized: AuthorizedWorkspaceWritePath = write_access
            .authorize_write(&"data.txt".parse().unwrap())
            .unwrap();
        assert_eq!(authorized.relative_path(), Path::new("nested/data.txt"));
        let mut prepared = authorized.prepare().unwrap();
        assert_eq!(prepared.key(), prepared.key());
        prepared.write(b"replaced nested body").unwrap();
        assert_eq!(
            std::fs::read(root.join("nested/data.txt")).unwrap(),
            b"replaced nested body"
        );

        // Empty and root paths are invalid write targets.
        assert_eq!(
            write_access
                .authorize_write(&WorkspaceRelativePath::default())
                .unwrap_err(),
            WorkspaceWriteError::InvalidTarget
        );
        let debug = format!("{authorized:?} {prepared:?}");
        assert!(!debug.contains("nested/data.txt"));
        assert!(!debug.contains(temporary.path().to_str().unwrap()));
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn existing_write_target_identity_is_physical_across_symlink_and_hard_link_aliases() {
        use std::os::unix::fs::symlink;

        let temporary = TempDirectory::new("write-existing-identity");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::write(root.join("target.txt"), b"original body").unwrap();
        symlink("target.txt", root.join("alias.txt")).unwrap();
        std::fs::hard_link(root.join("target.txt"), root.join("hard.txt")).unwrap();
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let (resolver, _control) = WorkspaceResolver::new_with_write_access(task_context.clone());
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();
        let prepared_direct = access
            .authorize_write(&"target.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();
        let prepared_alias = access
            .authorize_write(&"alias.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();
        let prepared_hard = access
            .authorize_write(&"hard.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();

        // All three names open to the same physical file: identical opaque mutation keys.
        let direct_key: WorkspaceFileMutationKey = prepared_direct.key();
        let alias_key = prepared_alias.key();
        let hard_key = prepared_hard.key();
        assert_eq!(direct_key, alias_key);
        assert_eq!(direct_key, hard_key);

        // Preparation mutates nothing: the body is unchanged after all three prepares.
        assert_eq!(
            std::fs::read(root.join("target.txt")).unwrap(),
            b"original body"
        );

        // Writing through the direct carrier replaces the shared physical content; the alias
        // and hard-link carriers observe the same replacement through the same key.
        let mut direct = prepared_direct;
        direct.write(b"replaced through direct").unwrap();
        assert_eq!(
            std::fs::read(root.join("target.txt")).unwrap(),
            b"replaced through direct"
        );
        assert_eq!(
            std::fs::read(root.join("hard.txt")).unwrap(),
            b"replaced through direct"
        );

        // Writing through the alias carrier replaces the same physical file, and the
        // truncate-first order drops the old tail: the shorter body is exact.
        let mut alias = prepared_alias;
        alias.write(b"short").unwrap();
        assert_eq!(std::fs::read(root.join("alias.txt")).unwrap(), b"short");

        // Empty content is a truthful full replacement to zero bytes.
        let mut hard = prepared_hard;
        hard.write(b"").unwrap();
        assert_eq!(std::fs::read(root.join("hard.txt")).unwrap(), b"");

        // Key Debug is opaque and redacts paths and physical identity.
        let debug = format!("{direct_key:?} {alias_key:?} {hard_key:?}");
        assert!(!debug.contains("target.txt"));
        assert!(!debug.contains(temporary.path().to_str().unwrap()));
        task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_write_target_key_binds_exact_parent_and_final_name() {
        let temporary = TempDirectory::new("write-create-key");
        let root = temporary.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("the sub directory is creatable");
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let (resolver, _control) = WorkspaceResolver::new_with_write_access(task_context.clone());
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        // The same missing target prepared twice binds the same parent identity plus the
        // same normalized final filename, so the create keys are equal even though each
        // preparation re-opens the parent separately.
        let first: PreparedWorkspaceWriteTarget = access
            .authorize_write(&"sub/missing.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();
        let second = access
            .authorize_write(&"sub/missing.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(first.key(), second.key());

        // Preparation does no create: the target still does not exist after preparing.
        assert!(!root.join("sub/missing.txt").exists());
        assert!(!root.join("missing.txt").exists());

        // A different final filename in the same parent is a different key.
        let other_name = access
            .authorize_write(&"sub/other.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();
        assert_ne!(first.key(), other_name.key());

        // The same final filename under a different parent (the root) is a different key.
        let other_parent = access
            .authorize_write(&"missing.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();
        assert_ne!(first.key(), other_parent.key());

        // The create write creates the file through the retained parent capability and a
        // later write is a full replacement (including empty content).
        let mut created = first;
        created.write(b"created body").unwrap();
        assert_eq!(
            std::fs::read(root.join("sub/missing.txt")).unwrap(),
            b"created body"
        );
        created.write(b"replaced").unwrap();
        assert_eq!(
            std::fs::read(root.join("sub/missing.txt")).unwrap(),
            b"replaced"
        );
        created.write(b"").unwrap();
        assert_eq!(std::fs::read(root.join("sub/missing.txt")).unwrap(), b"");

        // A regular file that appears after preparation is opened by the same overwrite
        // semantics: no create-new-only surprise, the full content is replaced.
        let mut appeared = other_name;
        std::fs::write(root.join("sub/other.txt"), b"appeared body").unwrap();
        appeared.write(b"overwrote it").unwrap();
        assert_eq!(
            std::fs::read(root.join("sub/other.txt")).unwrap(),
            b"overwrote it"
        );
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn write_preparation_fails_closed_for_missing_parent_special_directory_and_escape() {
        use std::os::unix::fs::symlink;

        let temporary = TempDirectory::new("write-prepare-fail");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(root.join("sub")).expect("the sub directory is creatable");
        std::fs::create_dir_all(&outside).expect("the outside directory is creatable");
        std::fs::write(root.join("file.txt"), b"a regular file").unwrap();
        std::fs::write(outside.join("secret.txt"), b"outside secret").unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let fifo = root.join("gate.fifo");
        let made = std::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&fifo)
            .status()
            .expect("the mkfifo command runs");
        assert!(made.success(), "the test FIFO is creatable");

        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let (resolver, _control) = WorkspaceResolver::new_with_write_access(task_context.clone());
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();
        let error_for = |relative: &str| {
            access
                .authorize_write(&relative.parse().unwrap())
                .unwrap()
                .prepare()
                .unwrap_err()
        };

        // A missing parent directory fails closed: nothing is created, no mkdir anywhere.
        assert_eq!(
            error_for("no-such-dir/missing.txt"),
            WorkspaceWriteError::Unavailable
        );
        // A directory target cannot be opened write-only.
        assert_eq!(error_for("sub"), WorkspaceWriteError::OpenFailed);
        // A regular file used as an intermediate path component is not a directory.
        assert_eq!(
            error_for("file.txt/child.txt"),
            WorkspaceWriteError::OpenFailed
        );
        // A FIFO is a special entry: the nonblocking write-only open fails closed and the
        // preparation never hangs on it.
        assert_eq!(error_for("gate.fifo"), WorkspaceWriteError::OpenFailed);
        // A symlink whose target lies outside the root fails closed at the capability open,
        // both as an intermediate component and as the final component.
        assert_eq!(
            error_for("escape/secret.txt"),
            WorkspaceWriteError::OpenFailed
        );
        assert_eq!(error_for("escape"), WorkspaceWriteError::OpenFailed);
        // The outside file was never touched.
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"outside secret"
        );
        task_context.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn create_write_rejects_a_final_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let temporary = TempDirectory::new("write-create-no-follow");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&root).expect("the test root is creatable");
        std::fs::create_dir_all(&outside).expect("the outside directory is creatable");
        std::fs::write(outside.join("victim.txt"), b"victim body").unwrap();
        let workspace = Workspace::new(
            revision(),
            root_spec_with(
                "primary",
                root.to_str().unwrap(),
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(false, false),
            ),
            Vec::new(),
            cwd("primary", ""),
        )
        .unwrap();

        let task_context = initialized_context().await;
        let (resolver, _control) = WorkspaceResolver::new_with_write_access(task_context.clone());
        let candidate = resolver.resolve(session_id(), &workspace).await.unwrap();
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .unwrap();
        let tool_context = snapshot.tool_context();
        let access = tool_context.access();

        // The target is missing at preparation, so the carrier is the create shape bound to
        // the root parent and the final filename. The key is opaque, so this workspace-local
        // test inspects the private internal shape directly; downstream code must rely on
        // key equality and write behavior instead.
        let mut prepared: PreparedWorkspaceWriteTarget = access
            .authorize_write(&"target.txt".parse().unwrap())
            .unwrap()
            .prepare()
            .unwrap();
        assert!(matches!(
            prepared.kind,
            PreparedWorkspaceWriteTargetKind::Create { .. }
        ));

        // Between preparation and the write an external process replaces the final name with
        // a symlink pointing outside the root. The create open uses a final-component
        // no-follow option, so the write fails closed instead of writing through the link.
        symlink(outside.join("victim.txt"), root.join("target.txt")).unwrap();
        assert_eq!(
            prepared.write(b"attacker payload").unwrap_err(),
            WorkspaceWriteError::OpenFailed
        );
        assert_eq!(
            std::fs::read(outside.join("victim.txt")).unwrap(),
            b"victim body"
        );
        assert!(
            std::fs::symlink_metadata(root.join("target.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        task_context.shutdown().await;
    }

    fn revision() -> WorkspaceRevision {
        "wr_1".parse().unwrap()
    }

    fn root_input(key: &str, uri: &str) -> WorkspaceRootInput {
        WorkspaceRootInput::new(
            key.parse().unwrap(),
            uri.parse::<CanonicalFileUri>().unwrap(),
            RequestedFilesystemAccess::ReadWrite,
            WorkspaceSourcePolicy::new(true, true),
        )
    }

    fn input_with_primary(uri: &str) -> WorkspaceDefinitionInput {
        WorkspaceDefinitionInput::new(root_input("repo", uri), Vec::new(), cwd("repo", "")).unwrap()
    }

    fn cwd(root: &str, relative_path: &str) -> WorkspaceCwdSpec {
        WorkspaceCwdSpec::new(root.parse().unwrap(), relative_path.parse().unwrap())
    }

    fn root_spec(key: &str, path: &str, _target: WorkspacePathTarget) -> WorkspaceRootSpec {
        WorkspaceRootSpec {
            key: key.parse().unwrap(),
            path: PathBuf::from(path),
            requested_access: RequestedFilesystemAccess::ReadWrite,
            sources: WorkspaceSourcePolicy::new(true, true),
        }
    }
}
