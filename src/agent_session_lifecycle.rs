use std::fmt;
use std::num::NonZeroU32;

use thiserror::Error;

use crate::model_gateway::{ModelSelection, ReasoningPreference};
use crate::prompt::{AgentPromptSelection, SessionPromptSelection};
use crate::wire::lexical::{LexicalError, normalize_newlines, validate_safe_text};
use crate::wire::{
    AgentId, AgentMetadataRevision, AgentRevision, ItemId, ProtocolLimits,
    SessionDefinitionRevision, SessionId, SessionMetadataRevision, Timestamp,
};
use crate::workspace::{Workspace, workspaces_have_same_semantic_content};

#[derive(Clone, Eq, PartialEq)]
pub struct AgentDefinition {
    agent_id: AgentId,
    revision: AgentRevision,
    prompts: AgentPromptSelection,
    created_at: Timestamp,
}

impl AgentDefinition {
    pub const fn new(
        agent_id: AgentId,
        revision: AgentRevision,
        prompts: AgentPromptSelection,
        created_at: Timestamp,
    ) -> Self {
        Self {
            agent_id,
            revision,
            prompts,
            created_at,
        }
    }

    pub const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    pub const fn revision(&self) -> AgentRevision {
        self.revision
    }

    pub const fn prompts(&self) -> &AgentPromptSelection {
        &self.prompts
    }

    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }
}

impl fmt::Debug for AgentDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentDefinition")
            .field("prompt_count", &self.prompts.enabled().len())
            .finish()
    }
}

/// Compares the durable execution content of two Agent definitions while deliberately excluding
/// immutable identity, revision, and definition timestamp facts.
pub(crate) fn agent_definitions_have_same_canonical_execution_content(
    first: &AgentDefinition,
    second: &AgentDefinition,
) -> bool {
    first.prompts() == second.prompts()
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AgentMetadataError {
    #[error("agent metadata name must be non-empty")]
    EmptyName,
    #[error("agent metadata exceeds its selected text limit")]
    TextTooLong,
    #[error("agent metadata contains an unsafe control character")]
    UnsafeText,
}

#[derive(Clone, Eq, PartialEq)]
pub struct AgentMetadata {
    revision: AgentMetadataRevision,
    name: Box<str>,
    description: Option<Box<str>>,
    updated_at: Timestamp,
}

impl AgentMetadata {
    pub fn new<N, D>(
        revision: AgentMetadataRevision,
        name: N,
        description: Option<D>,
        updated_at: Timestamp,
    ) -> Result<Self, AgentMetadataError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        let limits = ProtocolLimits::v1_0().text;
        let name = normalize_agent_metadata_text(
            name.as_ref(),
            usize::from(limits.max_display_name_bytes),
            false,
        )?;
        let description = description
            .map(|value| {
                normalize_agent_metadata_text(
                    value.as_ref(),
                    usize::try_from(limits.max_description_bytes).unwrap_or(usize::MAX),
                    true,
                )
            })
            .transpose()?;
        Ok(Self {
            revision,
            name,
            description,
            updated_at,
        })
    }

    pub const fn revision(&self) -> AgentMetadataRevision {
        self.revision
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub const fn updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

impl fmt::Debug for AgentMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentMetadata")
            .field("name_present", &true)
            .field("description_present", &self.description.is_some())
            .finish()
    }
}

/// Compares canonical metadata content while deliberately excluding its CAS revision and
/// millisecond-truncated update timestamp.
pub(crate) fn agent_metadata_has_same_canonical_content(
    first: &AgentMetadata,
    second: &AgentMetadata,
) -> bool {
    first.name() == second.name() && first.description() == second.description()
}

fn normalize_agent_metadata_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<Box<str>, AgentMetadataError> {
    normalize_safe_metadata_text(value, maximum, allow_empty).map_err(|error| match error {
        LexicalError::Empty => AgentMetadataError::EmptyName,
        LexicalError::TooLong => AgentMetadataError::TextTooLong,
        LexicalError::InvalidGrammar | LexicalError::UnsafeText => AgentMetadataError::UnsafeText,
    })
}

fn normalize_safe_metadata_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<Box<str>, LexicalError> {
    let value = normalize_newlines(value);
    validate_safe_text(&value, maximum, allow_empty)?;
    Ok(value.into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStatus {
    Enabled,
    Disabled,
    Deleted,
}

/// Returns whether one persisted Agent status may directly follow another in a durable
/// generation. `Deleted` is terminal, and same-status writes are canonical no-ops.
pub(crate) const fn is_legal_agent_status_transition(
    previous: AgentStatus,
    next: AgentStatus,
) -> bool {
    matches!(
        (previous, next),
        (AgentStatus::Enabled, AgentStatus::Disabled)
            | (AgentStatus::Disabled, AgentStatus::Enabled)
            | (
                AgentStatus::Enabled | AgentStatus::Disabled,
                AgentStatus::Deleted
            )
    )
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AgentRevisionRef {
    agent_id: AgentId,
    revision: AgentRevision,
}

impl AgentRevisionRef {
    pub const fn new(agent_id: AgentId, revision: AgentRevision) -> Self {
        Self { agent_id, revision }
    }

    pub const fn agent_id(self) -> AgentId {
        self.agent_id
    }

    pub const fn revision(self) -> AgentRevision {
        self.revision
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionModelConfig {
    selection: ModelSelection,
    reasoning: ReasoningPreference,
    max_output_tokens: Option<NonZeroU32>,
}

impl SessionModelConfig {
    pub const fn new(
        selection: ModelSelection,
        reasoning: ReasoningPreference,
        max_output_tokens: Option<NonZeroU32>,
    ) -> Self {
        Self {
            selection,
            reasoning,
            max_output_tokens,
        }
    }

    pub const fn selection(&self) -> &ModelSelection {
        &self.selection
    }

    pub const fn reasoning(&self) -> ReasoningPreference {
        self.reasoning
    }

    pub const fn max_output_tokens(&self) -> Option<NonZeroU32> {
        self.max_output_tokens
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionDefinition {
    session_id: SessionId,
    revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    workspace: Workspace,
    model: SessionModelConfig,
    prompts: SessionPromptSelection,
    created_at: Timestamp,
}

impl SessionDefinition {
    #[allow(
        clippy::too_many_arguments,
        reason = "a Session definition atomically owns its seven durable facts"
    )]
    pub const fn new(
        session_id: SessionId,
        revision: SessionDefinitionRevision,
        agent: AgentRevisionRef,
        workspace: Workspace,
        model: SessionModelConfig,
        prompts: SessionPromptSelection,
        created_at: Timestamp,
    ) -> Self {
        Self {
            session_id,
            revision,
            agent,
            workspace,
            model,
            prompts,
            created_at,
        }
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn revision(&self) -> SessionDefinitionRevision {
        self.revision
    }

    pub const fn agent(&self) -> AgentRevisionRef {
        self.agent
    }

    pub const fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub const fn model(&self) -> &SessionModelConfig {
        &self.model
    }

    pub const fn prompts(&self) -> &SessionPromptSelection {
        &self.prompts
    }

    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }
}

impl fmt::Debug for SessionDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionDefinition")
            .field("revision", &self.revision)
            .field("agent", &"redacted")
            .field("workspace", &"redacted")
            .field("model", &"redacted")
            .field("prompt_count", &self.prompts.enabled().len())
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Compares the execution-affecting content of Session definitions. Session identity,
/// definition revision, WorkspaceRevision, and definition timestamp are owner/version facts,
/// deliberately excluded from canonical no-op detection.
pub(crate) fn session_definitions_have_same_canonical_execution_content(
    first: &SessionDefinition,
    second: &SessionDefinition,
) -> bool {
    first.agent() == second.agent()
        && workspaces_have_same_semantic_content(first.workspace(), second.workspace())
        && first.model() == second.model()
        && first.prompts() == second.prompts()
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SessionMetadataError {
    #[error("session metadata name must be non-empty")]
    EmptyName,
    #[error("session metadata exceeds its selected text limit")]
    TextTooLong,
    #[error("session metadata contains an unsafe control character")]
    UnsafeText,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionMetadata {
    revision: SessionMetadataRevision,
    name: Option<Box<str>>,
    description: Option<Box<str>>,
    updated_at: Timestamp,
}

impl SessionMetadata {
    pub fn new<N, D>(
        revision: SessionMetadataRevision,
        name: Option<N>,
        description: Option<D>,
        updated_at: Timestamp,
    ) -> Result<Self, SessionMetadataError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        let limits = ProtocolLimits::v1_0().text;
        let name = name
            .map(|value| {
                normalize_session_metadata_text(
                    value.as_ref(),
                    usize::from(limits.max_display_name_bytes),
                    false,
                )
            })
            .transpose()?;
        let description = description
            .map(|value| {
                normalize_session_metadata_text(
                    value.as_ref(),
                    usize::try_from(limits.max_description_bytes).unwrap_or(usize::MAX),
                    true,
                )
            })
            .transpose()?;
        Ok(Self {
            revision,
            name,
            description,
            updated_at,
        })
    }

    pub const fn revision(&self) -> SessionMetadataRevision {
        self.revision
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub const fn updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

impl fmt::Debug for SessionMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionMetadata")
            .field("name_present", &self.name.is_some())
            .field("description_present", &self.description.is_some())
            .finish()
    }
}

/// Compares canonical Session metadata content while excluding its independent CAS revision and
/// millisecond-truncated update timestamp.
pub(crate) fn session_metadata_has_same_canonical_content(
    first: &SessionMetadata,
    second: &SessionMetadata,
) -> bool {
    first.name() == second.name() && first.description() == second.description()
}

fn normalize_session_metadata_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<Box<str>, SessionMetadataError> {
    normalize_safe_metadata_text(value, maximum, allow_empty).map_err(|error| match error {
        LexicalError::Empty => SessionMetadataError::EmptyName,
        LexicalError::TooLong => SessionMetadataError::TextTooLong,
        LexicalError::InvalidGrammar | LexicalError::UnsafeText => SessionMetadataError::UnsafeText,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycle {
    Open,
    Archived,
    Deleted,
}

/// Returns whether one persisted ordinary Session lifecycle may directly follow another.
/// `Deleted` is terminal and same-lifecycle writes are canonical no-ops.
pub(crate) const fn is_legal_session_lifecycle_transition(
    previous: SessionLifecycle,
    next: SessionLifecycle,
) -> bool {
    matches!(
        (previous, next),
        (SessionLifecycle::Open, SessionLifecycle::Archived)
            | (
                SessionLifecycle::Archived,
                SessionLifecycle::Open | SessionLifecycle::Deleted
            )
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkSourceKind {
    LiveSnapshot,
    RecordedHistory,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ForkAnchor {
    Genesis,
    BeforeUserMessage { item_id: ItemId },
    AfterUserMessage { item_id: ItemId },
    BeforeFinalAgentMessage { item_id: ItemId },
    AfterFinalAgentMessage { item_id: ItemId },
}

impl fmt::Debug for ForkAnchor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Genesis => "Genesis",
            Self::BeforeUserMessage { .. } => "BeforeUserMessage",
            Self::AfterUserMessage { .. } => "AfterUserMessage",
            Self::BeforeFinalAgentMessage { .. } => "BeforeFinalAgentMessage",
            Self::AfterFinalAgentMessage { .. } => "AfterFinalAgentMessage",
        };
        formatter.write_str(name)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionForkProvenance {
    source_session_id: SessionId,
    source: ForkSourceKind,
    anchor: ForkAnchor,
}

impl SessionForkProvenance {
    pub const fn new(
        source_session_id: SessionId,
        source: ForkSourceKind,
        anchor: ForkAnchor,
    ) -> Self {
        Self {
            source_session_id,
            source,
            anchor,
        }
    }

    pub const fn source_session_id(&self) -> SessionId {
        self.source_session_id
    }

    pub const fn source(&self) -> ForkSourceKind {
        self.source
    }

    pub const fn anchor(&self) -> &ForkAnchor {
        &self.anchor
    }
}

impl fmt::Debug for SessionForkProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionForkProvenance")
            .field("source_session", &"redacted")
            .field("source", &self.source)
            .field("anchor", &self.anchor)
            .finish()
    }
}
