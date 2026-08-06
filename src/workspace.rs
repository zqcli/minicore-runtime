use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use thiserror::Error;

use crate::wire::lexical::{LexicalError, validate_stable_symbolic_key};
use crate::wire::{
    CanonicalFileUri, FileUriFamily, ProtocolLimits, WorkspaceRelativePath, WorkspaceRevision,
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        RequestedFilesystemAccess, Workspace, WorkspaceConstructionError, WorkspaceCwdSpec,
        WorkspaceDefinitionInput, WorkspaceInputLoweringError, WorkspacePathTarget,
        WorkspaceRootInput, WorkspaceRootSpec, WorkspaceSourcePolicy, checked_native_uri,
        lower_workspace, uri_from_spec,
    };
    use crate::wire::{CanonicalFileUri, WorkspaceRelativePath, WorkspaceRevision};

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
