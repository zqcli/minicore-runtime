use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::wire::lexical::{LexicalError, validate_stable_symbolic_key};
use crate::wire::{CanonicalFileUri, ProtocolLimits, WorkspaceRelativePath};

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
