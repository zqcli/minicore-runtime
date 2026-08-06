use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

use thiserror::Error;

use crate::model_gateway::{ModelSelection, ReasoningPreference};
use crate::prompt::{AgentPromptSelection, SessionPromptSelection};
use crate::wire::lexical::{LexicalError, normalize_newlines, validate_safe_text};
use crate::wire::{
    AgentId, AgentMetadataRevision, AgentRevision, IdGenerationError, ItemId, ProtocolLimits,
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

    fn with_revision(&self, revision: AgentMetadataRevision, updated_at: Timestamp) -> Self {
        Self {
            revision,
            name: self.name.clone(),
            description: self.description.clone(),
            updated_at,
        }
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

/// The lifecycle-owned, pre-identity input to one durable Agent create attempt.
///
/// It deliberately owns no identity, storage, status, path, marker, or command facts. The
/// durable owner supplies the candidate identity only after this value has been sealed.
pub(crate) struct SealedAgentCreateAttempt {
    prompts: AgentPromptSelection,
    metadata: AgentMetadata,
}

impl SealedAgentCreateAttempt {
    #[allow(
        dead_code,
        reason = "the sealed attempt constructor is consumed by the pending command surface"
    )]
    pub(crate) fn new<N, D>(
        prompts: AgentPromptSelection,
        name: N,
        description: Option<D>,
        created_at: Timestamp,
    ) -> Result<Self, AgentMetadataError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        let metadata_revision = AgentMetadataRevision::new(
            NonZeroU64::new(1).expect("the fixed initial Agent metadata revision is non-zero"),
        );
        let metadata = AgentMetadata::new(metadata_revision, name, description, created_at)?;
        Ok(Self { prompts, metadata })
    }

    pub(crate) fn generate_candidate(&self) -> Result<AgentId, IdGenerationError> {
        AgentId::generate()
    }

    pub(crate) fn materialize(&self, agent_id: AgentId) -> (AgentDefinition, AgentMetadata) {
        let revision = AgentRevision::new(
            NonZeroU64::new(1).expect("the fixed initial Agent revision is non-zero"),
        );
        (
            AgentDefinition::new(
                agent_id,
                revision,
                self.prompts.clone(),
                self.metadata.updated_at(),
            ),
            self.metadata.clone(),
        )
    }
}

impl fmt::Debug for SealedAgentCreateAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedAgentCreateAttempt")
            .field("prompt_count", &self.prompts.enabled().len())
            .field("metadata", &"redacted")
            .field("created_at", &self.metadata.updated_at())
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

/// Lifecycle-owned semantic input to one Agent status CAS. It carries no storage generation,
/// path, timestamp, marker, command identity, or publication handle.
pub(crate) struct SealedAgentStatusAttempt {
    agent_id: AgentId,
    expected_status: AgentStatus,
    target_status: AgentStatus,
}

#[allow(
    dead_code,
    reason = "the public Agent status command constructor consumes this sealed error"
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AgentStatusAttemptError {
    #[error("usable Agent status mutation cannot target deleted")]
    InvalidUsableTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentStatusDecision {
    NoChange,
    Publish(AgentStatus),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AgentStatusDecisionError {
    #[error("Agent status compare-and-swap is stale")]
    StaleStatus,
    #[error("Agent is deleted")]
    AgentDeleted,
    #[error("Agent status transition is invalid")]
    InvalidTransition,
}

impl SealedAgentStatusAttempt {
    #[allow(
        dead_code,
        reason = "the public Agent status command constructor consumes this sealed seam"
    )]
    pub(crate) fn set_usable(
        agent_id: AgentId,
        expected_status: AgentStatus,
        target_status: AgentStatus,
    ) -> Result<Self, AgentStatusAttemptError> {
        if target_status == AgentStatus::Deleted {
            return Err(AgentStatusAttemptError::InvalidUsableTarget);
        }
        Ok(Self {
            agent_id,
            expected_status,
            target_status,
        })
    }

    #[allow(
        dead_code,
        reason = "the public Agent delete command constructor consumes this sealed seam"
    )]
    pub(crate) const fn delete(agent_id: AgentId, expected_status: AgentStatus) -> Self {
        Self {
            agent_id,
            expected_status,
            target_status: AgentStatus::Deleted,
        }
    }

    pub(crate) const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    #[cfg(test)]
    pub(crate) const fn expected_status(&self) -> AgentStatus {
        self.expected_status
    }

    #[cfg(test)]
    pub(crate) const fn target_status(&self) -> AgentStatus {
        self.target_status
    }

    pub(crate) fn decide(
        &self,
        current_status: AgentStatus,
    ) -> Result<AgentStatusDecision, AgentStatusDecisionError> {
        if current_status != self.expected_status {
            return Err(AgentStatusDecisionError::StaleStatus);
        }
        if current_status == AgentStatus::Deleted {
            return Err(AgentStatusDecisionError::AgentDeleted);
        }
        if current_status == self.target_status {
            return Ok(AgentStatusDecision::NoChange);
        }
        if !is_legal_agent_status_transition(current_status, self.target_status) {
            return Err(AgentStatusDecisionError::InvalidTransition);
        }
        Ok(AgentStatusDecision::Publish(self.target_status))
    }
}

impl fmt::Debug for SealedAgentStatusAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedAgentStatusAttempt")
            .field("agent_id", &"redacted")
            .field("expected_status", &self.expected_status)
            .field("target_status", &self.target_status)
            .finish()
    }
}

/// Lifecycle-owned semantic input to one Agent definition CAS. It carries only the Agent lookup
/// key, the expected current definition revision, the requested prompt selection, and the owner
/// timestamp. Storage generation, paths, markers, command identity, and publication handles do
/// not cross this seam.
pub(crate) struct SealedAgentDefinitionAttempt {
    agent_id: AgentId,
    expected_revision: AgentRevision,
    target_prompts: AgentPromptSelection,
    owner_timestamp: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AgentDefinitionDecisionError {
    #[error("Agent definition compare-and-swap is stale")]
    StaleRevision,
    #[error("Agent is deleted")]
    AgentDeleted,
    #[error("Agent definition revision is exhausted")]
    RevisionExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentDefinitionDecision {
    NoChange,
    Publish(AgentDefinition),
}

impl SealedAgentDefinitionAttempt {
    #[allow(
        dead_code,
        reason = "the public Agent definition command constructor consumes this sealed seam"
    )]
    pub(crate) fn new(
        agent_id: AgentId,
        expected_revision: AgentRevision,
        target_prompts: AgentPromptSelection,
        owner_timestamp: Timestamp,
    ) -> Self {
        Self {
            agent_id,
            expected_revision,
            target_prompts,
            owner_timestamp,
        }
    }

    pub(crate) const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    #[cfg(test)]
    pub(crate) const fn expected_revision(&self) -> AgentRevision {
        self.expected_revision
    }

    #[cfg(test)]
    pub(crate) const fn owner_timestamp(&self) -> Timestamp {
        self.owner_timestamp
    }

    #[cfg(test)]
    pub(crate) fn target_prompts(&self) -> &AgentPromptSelection {
        &self.target_prompts
    }

    /// Decides the semantic CAS in its authoritative order: expected revision, terminal status,
    /// canonical no-op, then checked next revision and materialization.
    pub(crate) fn decide(
        &self,
        current_revision: AgentRevision,
        current_status: AgentStatus,
        current_definition: &AgentDefinition,
    ) -> Result<AgentDefinitionDecision, AgentDefinitionDecisionError> {
        if current_revision != self.expected_revision {
            return Err(AgentDefinitionDecisionError::StaleRevision);
        }
        if current_status == AgentStatus::Deleted {
            return Err(AgentDefinitionDecisionError::AgentDeleted);
        }
        if current_definition.prompts() == &self.target_prompts {
            return Ok(AgentDefinitionDecision::NoChange);
        }
        let next_revision = current_revision
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(AgentRevision::new)
            .ok_or(AgentDefinitionDecisionError::RevisionExhausted)?;
        Ok(AgentDefinitionDecision::Publish(AgentDefinition::new(
            self.agent_id,
            next_revision,
            self.target_prompts.clone(),
            self.owner_timestamp,
        )))
    }
}

impl fmt::Debug for SealedAgentDefinitionAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedAgentDefinitionAttempt")
            .field("agent_id", &"redacted")
            .field("expected_revision", &self.expected_revision)
            .field("target_prompts", &"redacted")
            .field("owner_timestamp", &"redacted")
            .finish()
    }
}

/// The sealed description half of an Agent metadata patch. Its representation is private so a
/// caller cannot forge an invalid canonical Set value or confuse Keep with Clear.
#[allow(
    dead_code,
    reason = "the sealed description patch is consumed by the pending Agent command surface"
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AgentMetadataDescriptionPatch {
    value: AgentMetadataDescriptionPatchValue,
}

#[allow(
    dead_code,
    reason = "the sealed description patch is consumed by the pending Agent command surface"
)]
#[derive(Clone, Eq, PartialEq)]
enum AgentMetadataDescriptionPatchValue {
    Keep,
    Set(Box<str>),
    Clear,
}

#[allow(
    dead_code,
    reason = "the sealed description patch is consumed by the pending Agent command surface"
)]
impl AgentMetadataDescriptionPatch {
    pub(crate) const fn keep() -> Self {
        Self {
            value: AgentMetadataDescriptionPatchValue::Keep,
        }
    }

    pub(crate) fn set<D>(raw: D) -> Result<Self, AgentMetadataError>
    where
        D: AsRef<str>,
    {
        let limits = ProtocolLimits::v1_0().text;
        let value = normalize_agent_metadata_text(
            raw.as_ref(),
            usize::try_from(limits.max_description_bytes).unwrap_or(usize::MAX),
            true,
        )?;
        Ok(Self {
            value: AgentMetadataDescriptionPatchValue::Set(value),
        })
    }

    pub(crate) const fn clear() -> Self {
        Self {
            value: AgentMetadataDescriptionPatchValue::Clear,
        }
    }

    fn apply_to(&self, current: Option<&str>) -> Option<Box<str>> {
        match &self.value {
            AgentMetadataDescriptionPatchValue::Keep => current.map(|value| value.into()),
            AgentMetadataDescriptionPatchValue::Set(value) => Some(value.clone()),
            AgentMetadataDescriptionPatchValue::Clear => None,
        }
    }
}

impl fmt::Debug for AgentMetadataDescriptionPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.value {
            AgentMetadataDescriptionPatchValue::Keep => "Keep",
            AgentMetadataDescriptionPatchValue::Set(_) => "Set",
            AgentMetadataDescriptionPatchValue::Clear => "Clear",
        };
        formatter
            .debug_struct("AgentMetadataDescriptionPatch")
            .field("kind", &kind)
            .field("value", &"redacted")
            .finish()
    }
}

/// Lifecycle-owned semantic input to one Agent metadata CAS. It carries only the Agent lookup
/// key, expected current metadata revision, canonical patch intent, and owner timestamp. Storage
/// generation, paths, markers, command identity, and publication handles do not cross this seam.
pub(crate) struct SealedAgentMetadataAttempt {
    agent_id: AgentId,
    expected_revision: AgentMetadataRevision,
    name: Option<Box<str>>,
    description: AgentMetadataDescriptionPatch,
    owner_timestamp: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AgentMetadataDecisionError {
    #[error("Agent metadata compare-and-swap is stale")]
    StaleRevision,
    #[error("Agent is deleted")]
    AgentDeleted,
    #[error("Agent metadata revision is exhausted")]
    RevisionExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentMetadataDecision {
    NoChange,
    Publish(AgentMetadata),
}

impl SealedAgentMetadataAttempt {
    #[allow(
        dead_code,
        reason = "the public Agent metadata command constructor consumes this sealed seam"
    )]
    pub(crate) fn new(
        agent_id: AgentId,
        expected_revision: AgentMetadataRevision,
        name: Option<String>,
        description: AgentMetadataDescriptionPatch,
        owner_timestamp: Timestamp,
    ) -> Result<Self, AgentMetadataError> {
        let limits = ProtocolLimits::v1_0().text;
        let name = name
            .map(|value| {
                normalize_agent_metadata_text(
                    &value,
                    usize::from(limits.max_display_name_bytes),
                    false,
                )
            })
            .transpose()?;
        Ok(Self {
            agent_id,
            expected_revision,
            name,
            description,
            owner_timestamp,
        })
    }

    pub(crate) const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    #[cfg(test)]
    pub(crate) const fn expected_revision(&self) -> AgentMetadataRevision {
        self.expected_revision
    }

    #[cfg(test)]
    pub(crate) const fn owner_timestamp(&self) -> Timestamp {
        self.owner_timestamp
    }

    /// Decides the semantic CAS in its authoritative order: expected metadata revision, terminal
    /// status, patch application against authoritative current metadata, canonical no-op, then
    /// checked next revision and metadata materialization.
    pub(crate) fn decide(
        &self,
        current_status: AgentStatus,
        current_metadata: &AgentMetadata,
    ) -> Result<AgentMetadataDecision, AgentMetadataDecisionError> {
        let current_revision = current_metadata.revision();
        if current_revision != self.expected_revision {
            return Err(AgentMetadataDecisionError::StaleRevision);
        }
        if current_status == AgentStatus::Deleted {
            return Err(AgentMetadataDecisionError::AgentDeleted);
        }
        let patched_metadata = AgentMetadata {
            revision: current_metadata.revision(),
            name: self
                .name
                .clone()
                .unwrap_or_else(|| current_metadata.name.clone()),
            description: self.description.apply_to(current_metadata.description()),
            updated_at: current_metadata.updated_at(),
        };
        if agent_metadata_has_same_canonical_content(&patched_metadata, current_metadata) {
            return Ok(AgentMetadataDecision::NoChange);
        }
        let next_revision = current_revision
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(AgentMetadataRevision::new)
            .ok_or(AgentMetadataDecisionError::RevisionExhausted)?;
        Ok(AgentMetadataDecision::Publish(
            patched_metadata.with_revision(next_revision, self.owner_timestamp),
        ))
    }
}

impl fmt::Debug for SealedAgentMetadataAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedAgentMetadataAttempt")
            .field("agent_id", &"redacted")
            .field("expected_revision", &self.expected_revision)
            .field("name_patch", &"redacted")
            .field("description_patch", &"redacted")
            .field("owner_timestamp", &"redacted")
            .finish()
    }
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

/// The lifecycle-owned, pre-identity input to one recorded-history Genesis Fork attempt.
///
/// The source Session identity and child-local creation timestamp are the only facts captured
/// here. DurableState supplies the child identity and the exact source definition after actor
/// serialization; physical storage, publication and source conversation facts do not cross this
/// seam.
pub(crate) struct SealedSessionForkAttempt {
    source_session_id: SessionId,
    child_created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionForkAttemptError {
    #[error("fork source definition does not match the captured source")]
    SourceDefinitionMismatch,
}

impl SealedSessionForkAttempt {
    #[allow(
        dead_code,
        reason = "the public Session Fork command constructor consumes this sealed lifecycle seam"
    )]
    pub(crate) const fn recorded_genesis(
        source_session_id: SessionId,
        child_created_at: Timestamp,
    ) -> Self {
        Self {
            source_session_id,
            child_created_at,
        }
    }

    pub(crate) const fn source_session_id(&self) -> SessionId {
        self.source_session_id
    }

    pub(crate) fn generate_candidate(&self) -> Result<SessionId, IdGenerationError> {
        SessionId::generate()
    }

    pub(crate) fn materialize(
        &self,
        child_session_id: SessionId,
        source_definition: &SessionDefinition,
    ) -> Result<(SessionDefinition, SessionMetadata, SessionForkProvenance), SessionForkAttemptError>
    {
        if source_definition.session_id() != self.source_session_id {
            return Err(SessionForkAttemptError::SourceDefinitionMismatch);
        }
        let definition_revision = SessionDefinitionRevision::new(
            NonZeroU64::new(1).expect("the fixed initial Session definition revision is non-zero"),
        );
        let metadata_revision = SessionMetadataRevision::new(
            NonZeroU64::new(1).expect("the fixed initial Session metadata revision is non-zero"),
        );
        let workspace = source_definition.workspace().reset_revision_for_fork();
        let metadata = SessionMetadata::new(
            metadata_revision,
            None::<&str>,
            None::<&str>,
            self.child_created_at,
        )
        .expect("empty Genesis Fork metadata is always valid");
        let definition = SessionDefinition::new(
            child_session_id,
            definition_revision,
            source_definition.agent(),
            workspace,
            source_definition.model().clone(),
            source_definition.prompts().clone(),
            self.child_created_at,
        );
        let provenance = SessionForkProvenance::recorded_genesis(self.source_session_id);
        Ok((definition, metadata, provenance))
    }
}

impl fmt::Debug for SealedSessionForkAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedSessionForkAttempt")
            .field("source_session", &"redacted")
            .field("child_created_at", &"redacted")
            .finish()
    }
}

/// The lifecycle-owned, pre-identity input to one durable ordinary Session create attempt.
///
/// The requested AgentId is only a lookup key. The durable owner supplies the assigned SessionId
/// and pins the current AgentRevisionRef after actor serialization has acquired the Agent gate.
/// No storage, path, generation, marker, or CommandId fact belongs in this value.
pub(crate) struct SealedSessionCreateAttempt {
    requested_agent_id: AgentId,
    workspace: Workspace,
    model: SessionModelConfig,
    prompts: SessionPromptSelection,
    metadata: SessionMetadata,
}

#[allow(
    dead_code,
    reason = "the lifecycle seam is consumed by the pending Session command surface"
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionCreateAttemptError {
    #[error("session create workspace must start at revision one")]
    InvalidWorkspaceRevision,
    #[error(transparent)]
    InvalidMetadata(#[from] SessionMetadataError),
}

impl SealedSessionCreateAttempt {
    #[allow(
        clippy::too_many_arguments,
        reason = "one sealed Session create owns its fixed parent-independent candidate fragments"
    )]
    #[allow(
        dead_code,
        reason = "the sealed attempt constructor is consumed by the pending command surface"
    )]
    pub(crate) fn new<N, D>(
        requested_agent_id: AgentId,
        workspace: Workspace,
        model: SessionModelConfig,
        prompts: SessionPromptSelection,
        name: Option<N>,
        description: Option<D>,
        created_at: Timestamp,
    ) -> Result<Self, SessionCreateAttemptError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        if workspace.revision().get() != 1 {
            return Err(SessionCreateAttemptError::InvalidWorkspaceRevision);
        }
        let metadata_revision = SessionMetadataRevision::new(
            NonZeroU64::new(1).expect("the fixed initial Session metadata revision is non-zero"),
        );
        let metadata = SessionMetadata::new(metadata_revision, name, description, created_at)?;
        Ok(Self {
            requested_agent_id,
            workspace,
            model,
            prompts,
            metadata,
        })
    }

    pub(crate) const fn requested_agent_id(&self) -> AgentId {
        self.requested_agent_id
    }

    pub(crate) fn generate_candidate(&self) -> Result<SessionId, IdGenerationError> {
        SessionId::generate()
    }

    pub(crate) fn materialize(
        &self,
        session_id: SessionId,
        agent: AgentRevisionRef,
    ) -> (SessionDefinition, SessionMetadata) {
        let revision = SessionDefinitionRevision::new(
            NonZeroU64::new(1).expect("the fixed initial Session definition revision is non-zero"),
        );
        (
            SessionDefinition::new(
                session_id,
                revision,
                agent,
                self.workspace.clone(),
                self.model.clone(),
                self.prompts.clone(),
                self.metadata.updated_at(),
            ),
            self.metadata.clone(),
        )
    }
}

impl fmt::Debug for SealedSessionCreateAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedSessionCreateAttempt")
            .field("requested_agent", &"redacted")
            .field("workspace", &"redacted")
            .field("model", &"redacted")
            .field("prompt_count", &self.prompts.enabled().len())
            .field("metadata", &"redacted")
            .field("created_at", &self.metadata.updated_at())
            .finish()
    }
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
    pub(crate) const fn recorded_genesis(source_session_id: SessionId) -> Self {
        Self {
            source_session_id,
            source: ForkSourceKind::RecordedHistory,
            anchor: ForkAnchor::Genesis,
        }
    }

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

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::{
        AgentDefinitionDecision, AgentMetadataDecision, AgentMetadataDecisionError,
        AgentMetadataDescriptionPatch, AgentRevisionRef, AgentStatus, AgentStatusAttemptError,
        SealedAgentCreateAttempt, SealedAgentDefinitionAttempt, SealedAgentMetadataAttempt,
        SealedAgentStatusAttempt, SealedSessionCreateAttempt, SealedSessionForkAttempt,
        SessionDefinition, SessionForkAttemptError, SessionModelConfig,
    };
    use crate::model_gateway::{ModelSelection, ReasoningPreference};
    use crate::prompt::{AgentPromptSelection, SessionPromptSelection};
    use crate::wire::{
        AgentId, AgentMetadataRevision, AgentRevision, CanonicalFileUri, SessionDefinitionRevision,
        SessionId, WorkspaceRevision,
    };
    use crate::workspace::{
        RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspacePathTarget,
        WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy, lower_workspace,
    };

    #[test]
    fn sealed_agent_create_attempt_fixes_initial_revisions_and_redacts_input() {
        let prompts = AgentPromptSelection::new(
            ["base", "safety"]
                .into_iter()
                .map(|value| value.parse().unwrap())
                .collect(),
        )
        .unwrap();
        let timestamp = "2026-08-03T10:00:00.123Z".parse().unwrap();
        let attempt = SealedAgentCreateAttempt::new(
            prompts,
            "Planner secret",
            Some("Description secret"),
            timestamp,
        )
        .unwrap();

        let debug = format!("{attempt:?}");
        assert!(!debug.contains("Planner"));
        assert!(!debug.contains("Description"));
        assert!(!debug.contains("base"));
        assert!(!debug.contains("safety"));

        let agent_id: AgentId = "agt_11111111111111111111111111111111".parse().unwrap();
        let (definition, metadata) = attempt.materialize(agent_id);
        assert_eq!(definition.agent_id(), agent_id);
        assert_eq!(definition.revision().to_string(), "ar_1");
        assert_eq!(definition.created_at(), timestamp);
        assert_eq!(
            definition
                .prompts()
                .enabled()
                .iter()
                .map(|prompt| prompt.as_str())
                .collect::<Vec<_>>(),
            ["base", "safety"]
        );
        assert_eq!(metadata.revision().to_string(), "amr_1");
        assert_eq!(metadata.name(), "Planner secret");
        assert_eq!(metadata.description(), Some("Description secret"));
        assert_eq!(metadata.updated_at(), timestamp);
    }

    #[test]
    fn sealed_agent_status_attempt_separates_usable_status_and_delete_and_redacts_identity() {
        let agent_id: AgentId = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let usable = SealedAgentStatusAttempt::set_usable(
            agent_id,
            AgentStatus::Enabled,
            AgentStatus::Disabled,
        )
        .unwrap();
        assert_eq!(usable.agent_id(), agent_id);
        assert_eq!(usable.expected_status(), AgentStatus::Enabled);
        assert_eq!(usable.target_status(), AgentStatus::Disabled);
        assert!(!format!("{usable:?}").contains("agt_aaaaaaaa"));
        assert_eq!(
            SealedAgentStatusAttempt::set_usable(
                agent_id,
                AgentStatus::Enabled,
                AgentStatus::Deleted,
            )
            .unwrap_err(),
            AgentStatusAttemptError::InvalidUsableTarget
        );

        let delete = SealedAgentStatusAttempt::delete(agent_id, AgentStatus::Disabled);
        assert_eq!(delete.target_status(), AgentStatus::Deleted);
    }

    #[test]
    fn sealed_agent_definition_attempt_orders_cas_checks_and_redacts_semantics() {
        let agent_id: AgentId = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let first_revision = AgentRevision::new(NonZeroU64::new(1).unwrap());
        let current_prompts = AgentPromptSelection::new(vec!["base".parse().unwrap()]).unwrap();
        let target_prompts = AgentPromptSelection::new(
            ["base", "code-review"]
                .into_iter()
                .map(|value| value.parse().unwrap())
                .collect(),
        )
        .unwrap();
        let owner_timestamp = "2026-08-03T10:00:05.000Z".parse().unwrap();
        let current = super::AgentDefinition::new(
            agent_id,
            first_revision,
            current_prompts,
            "2026-08-03T10:00:00.123Z".parse().unwrap(),
        );
        let attempt = SealedAgentDefinitionAttempt::new(
            agent_id,
            first_revision,
            target_prompts.clone(),
            owner_timestamp,
        );

        assert_eq!(attempt.expected_revision(), first_revision);
        assert_eq!(attempt.target_prompts(), &target_prompts);
        assert_eq!(attempt.owner_timestamp(), owner_timestamp);
        let debug = format!("{attempt:?}");
        assert!(!debug.contains("agt_aaaaaaaa"));
        assert!(!debug.contains("code-review"));
        assert!(!debug.contains("10:00:05"));

        assert_eq!(
            attempt
                .decide(
                    AgentRevision::new(NonZeroU64::new(2).unwrap()),
                    AgentStatus::Deleted,
                    &current,
                )
                .unwrap_err(),
            super::AgentDefinitionDecisionError::StaleRevision
        );
        assert_eq!(
            attempt
                .decide(first_revision, AgentStatus::Deleted, &current)
                .unwrap_err(),
            super::AgentDefinitionDecisionError::AgentDeleted
        );
        assert_eq!(
            SealedAgentDefinitionAttempt::new(
                agent_id,
                first_revision,
                current.prompts().clone(),
                owner_timestamp,
            )
            .decide(first_revision, AgentStatus::Deleted, &current)
            .unwrap_err(),
            super::AgentDefinitionDecisionError::AgentDeleted,
            "Deleted remains terminal even when the target is canonically unchanged"
        );

        let no_op = SealedAgentDefinitionAttempt::new(
            agent_id,
            first_revision,
            current.prompts().clone(),
            owner_timestamp,
        )
        .decide(first_revision, AgentStatus::Enabled, &current)
        .unwrap();
        assert_eq!(no_op, AgentDefinitionDecision::NoChange);

        let published = attempt
            .decide(first_revision, AgentStatus::Disabled, &current)
            .unwrap();
        let AgentDefinitionDecision::Publish(definition) = published else {
            panic!("changed prompt selection publishes a new definition");
        };
        assert_eq!(definition.agent_id(), agent_id);
        assert_eq!(definition.revision().get(), 2);
        assert_eq!(definition.prompts(), &target_prompts);
        assert_eq!(definition.created_at(), owner_timestamp);
    }

    #[test]
    fn sealed_agent_metadata_attempt_preserves_patch_intent_and_orders_cas() {
        let agent_id: AgentId = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let first_revision = AgentMetadataRevision::new(NonZeroU64::new(1).unwrap());
        let current_timestamp = "2026-08-03T10:00:00.123Z".parse().unwrap();
        let owner_timestamp = "2026-08-03T10:00:05.000Z".parse().unwrap();
        let current = super::AgentMetadata::new(
            first_revision,
            "Planner",
            Some("Description"),
            current_timestamp,
        )
        .unwrap();
        let attempt = SealedAgentMetadataAttempt::new(
            agent_id,
            first_revision,
            Some("Planner revised\r\nsecret".to_owned()),
            AgentMetadataDescriptionPatch::set("Description revised\rsecret").unwrap(),
            owner_timestamp,
        )
        .unwrap();
        let stale_current = super::AgentMetadata::new(
            AgentMetadataRevision::new(NonZeroU64::new(2).unwrap()),
            "Planner",
            Some("Description"),
            current_timestamp,
        )
        .unwrap();

        assert_eq!(attempt.expected_revision(), first_revision);
        assert_eq!(attempt.owner_timestamp(), owner_timestamp);
        let debug = format!("{attempt:?}");
        for secret in [
            "agt_aaaaaaaa",
            "Planner revised",
            "Description revised",
            "10:00:05",
        ] {
            assert!(
                !debug.contains(secret),
                "metadata attempt leaked {secret:?}"
            );
        }

        assert_eq!(
            attempt
                .decide(AgentStatus::Deleted, &stale_current)
                .unwrap_err(),
            AgentMetadataDecisionError::StaleRevision
        );
        assert_eq!(
            attempt.decide(AgentStatus::Deleted, &current).unwrap_err(),
            AgentMetadataDecisionError::AgentDeleted
        );

        let AgentMetadataDecision::Publish(metadata) =
            attempt.decide(AgentStatus::Disabled, &current).unwrap()
        else {
            panic!("changed canonical metadata publishes a new revision");
        };
        assert_eq!(metadata.revision().get(), 2);
        assert_eq!(metadata.name(), "Planner revised\nsecret");
        assert_eq!(metadata.description(), Some("Description revised\nsecret"));
        assert_eq!(metadata.updated_at(), owner_timestamp);
    }

    #[test]
    fn sealed_agent_metadata_patch_applies_each_intent_and_no_ops_after_cas() {
        let agent_id: AgentId = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let revision = AgentMetadataRevision::new(NonZeroU64::new(1).unwrap());
        let timestamp = "2026-08-03T10:00:00.123Z".parse().unwrap();
        let owner_timestamp = "2026-08-03T10:00:05.000Z".parse().unwrap();
        let current =
            super::AgentMetadata::new(revision, "Planner", Some("Description"), timestamp).unwrap();

        let AgentMetadataDecision::Publish(name_only) = SealedAgentMetadataAttempt::new(
            agent_id,
            revision,
            Some("Planner revised\r\n".to_owned()),
            AgentMetadataDescriptionPatch::keep(),
            owner_timestamp,
        )
        .unwrap()
        .decide(AgentStatus::Enabled, &current)
        .unwrap() else {
            panic!("a name-only patch changes metadata");
        };
        assert_eq!(name_only.name(), "Planner revised\n");
        assert_eq!(name_only.description(), Some("Description"));

        let AgentMetadataDecision::Publish(description_set) = SealedAgentMetadataAttempt::new(
            agent_id,
            revision,
            None,
            AgentMetadataDescriptionPatch::set("Updated description").unwrap(),
            owner_timestamp,
        )
        .unwrap()
        .decide(AgentStatus::Enabled, &current)
        .unwrap() else {
            panic!("a description Set patch changes metadata");
        };
        assert_eq!(description_set.name(), "Planner");
        assert_eq!(description_set.description(), Some("Updated description"));

        let AgentMetadataDecision::Publish(description_clear) = SealedAgentMetadataAttempt::new(
            agent_id,
            revision,
            None,
            AgentMetadataDescriptionPatch::clear(),
            owner_timestamp,
        )
        .unwrap()
        .decide(AgentStatus::Enabled, &current)
        .unwrap() else {
            panic!("a description Clear patch changes metadata");
        };
        assert_eq!(description_clear.name(), "Planner");
        assert_eq!(description_clear.description(), None);

        for (name, description) in [
            (None, AgentMetadataDescriptionPatch::keep()),
            (
                Some("Planner".to_owned()),
                AgentMetadataDescriptionPatch::set("Description").unwrap(),
            ),
        ] {
            assert_eq!(
                SealedAgentMetadataAttempt::new(
                    agent_id,
                    revision,
                    name,
                    description,
                    owner_timestamp
                )
                .unwrap()
                .decide(AgentStatus::Enabled, &current)
                .unwrap(),
                AgentMetadataDecision::NoChange
            );
        }

        let empty = SealedAgentMetadataAttempt::new(
            agent_id,
            revision,
            None,
            AgentMetadataDescriptionPatch::keep(),
            owner_timestamp,
        )
        .unwrap();
        let stale_current = super::AgentMetadata::new(
            AgentMetadataRevision::new(NonZeroU64::new(2).unwrap()),
            "Planner",
            Some("Description"),
            timestamp,
        )
        .unwrap();
        assert_eq!(
            empty
                .decide(AgentStatus::Enabled, &stale_current)
                .unwrap_err(),
            AgentMetadataDecisionError::StaleRevision,
            "stale wins even for an empty patch"
        );
        assert_eq!(
            empty.decide(AgentStatus::Deleted, &current).unwrap_err(),
            AgentMetadataDecisionError::AgentDeleted,
            "Deleted wins even for an empty patch"
        );

        let equivalent = SealedAgentMetadataAttempt::new(
            agent_id,
            revision,
            Some("Planner".to_owned()),
            AgentMetadataDescriptionPatch::set("Description").unwrap(),
            owner_timestamp,
        )
        .unwrap();
        assert_eq!(
            equivalent
                .decide(AgentStatus::Deleted, &current)
                .unwrap_err(),
            AgentMetadataDecisionError::AgentDeleted,
            "Deleted wins even for a canonical-equivalent patch"
        );
    }

    #[test]
    fn sealed_agent_metadata_patch_reports_revision_exhaustion_only_for_a_change() {
        let agent_id: AgentId = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let exhausted_revision = AgentMetadataRevision::new(NonZeroU64::new(u64::MAX).unwrap());
        let current = super::AgentMetadata::new(
            exhausted_revision,
            "Planner",
            None::<&str>,
            "2026-08-03T10:00:00.123Z".parse().unwrap(),
        )
        .unwrap();
        let changed = SealedAgentMetadataAttempt::new(
            agent_id,
            exhausted_revision,
            Some("another name".to_owned()),
            AgentMetadataDescriptionPatch::keep(),
            "2026-08-03T10:00:05.000Z".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(
            changed.decide(AgentStatus::Enabled, &current).unwrap_err(),
            AgentMetadataDecisionError::RevisionExhausted
        );

        let empty = SealedAgentMetadataAttempt::new(
            agent_id,
            exhausted_revision,
            None,
            AgentMetadataDescriptionPatch::keep(),
            "2026-08-03T10:00:05.000Z".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(
            empty.decide(AgentStatus::Enabled, &current).unwrap(),
            AgentMetadataDecision::NoChange
        );
    }

    #[test]
    fn sealed_session_fork_attempt_records_only_genesis_source_facts_and_redacts_source_identity() {
        let source_session: SessionId = "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let timestamp = "2026-08-03T10:03:00.000Z".parse().unwrap();
        let attempt = SealedSessionForkAttempt::recorded_genesis(source_session, timestamp);

        assert_eq!(attempt.source_session_id(), source_session);
        let debug = format!("{attempt:?}");
        assert!(!debug.contains("ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!debug.contains("10:03:00"));

        let source_id: SessionId = "ses_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap();
        assert_ne!(attempt.source_session_id(), source_id);
    }

    #[test]
    fn sealed_recorded_genesis_fork_resets_child_revisions_and_copies_source_semantics() {
        let source_session: SessionId = "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let child_session: SessionId = "ses_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap();
        let root_key: WorkspaceRootKey = "repo".parse().unwrap();
        let root_uri: CanonicalFileUri = if cfg!(windows) {
            "file:///C:/work/project".parse().unwrap()
        } else {
            "file:///Users/example/project".parse().unwrap()
        };
        let workspace = lower_workspace(
            WorkspaceDefinitionInput::new(
                WorkspaceRootInput::new(
                    root_key,
                    root_uri,
                    RequestedFilesystemAccess::ReadWrite,
                    WorkspaceSourcePolicy::new(true, true),
                ),
                Vec::new(),
                WorkspaceCwdSpec::new("repo".parse().unwrap(), "src".parse().unwrap()),
            )
            .unwrap(),
            WorkspaceRevision::new(NonZeroU64::new(7).unwrap()),
            WorkspacePathTarget::current(),
        )
        .unwrap();
        let source_definition = SessionDefinition::new(
            source_session,
            SessionDefinitionRevision::new(NonZeroU64::new(9).unwrap()),
            AgentRevisionRef::new(
                "agt_11111111111111111111111111111111".parse().unwrap(),
                AgentRevision::new(NonZeroU64::new(3).unwrap()),
            ),
            workspace,
            SessionModelConfig::new(
                ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
                ReasoningPreference::Auto,
                Some(NonZeroU32::new(4096).unwrap()),
            ),
            SessionPromptSelection::new(vec!["base".parse().unwrap()]).unwrap(),
            "2026-08-03T10:01:00.456Z".parse().unwrap(),
        );
        let child_created_at = "2026-08-03T10:03:00.000Z".parse().unwrap();
        let attempt = SealedSessionForkAttempt::recorded_genesis(source_session, child_created_at);
        let (definition, metadata, provenance) = attempt
            .materialize(child_session, &source_definition)
            .unwrap();

        assert_eq!(definition.session_id(), child_session);
        assert_eq!(definition.revision().get(), 1);
        assert_eq!(definition.workspace().revision().get(), 1);
        assert_eq!(definition.agent(), source_definition.agent());
        assert_eq!(definition.model(), source_definition.model());
        assert_eq!(definition.prompts(), source_definition.prompts());
        assert_eq!(definition.created_at(), child_created_at);
        assert_eq!(metadata.revision().get(), 1);
        assert_eq!(metadata.name(), None);
        assert_eq!(metadata.description(), None);
        assert_eq!(metadata.updated_at(), child_created_at);
        assert_eq!(provenance.source_session_id(), source_session);
        assert_eq!(provenance.source(), super::ForkSourceKind::RecordedHistory);
        assert_eq!(provenance.anchor(), &super::ForkAnchor::Genesis);

        let other_source = SessionDefinition::new(
            "ses_cccccccccccccccccccccccccccccccc".parse().unwrap(),
            source_definition.revision(),
            source_definition.agent(),
            source_definition.workspace().clone(),
            source_definition.model().clone(),
            source_definition.prompts().clone(),
            source_definition.created_at(),
        );
        assert_eq!(
            attempt.materialize(child_session, &other_source),
            Err(SessionForkAttemptError::SourceDefinitionMismatch)
        );
    }

    #[test]
    fn sealed_session_create_attempt_fixes_initial_revisions_and_redacts_input() {
        let requested_agent: AgentId = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let root_key: WorkspaceRootKey = "repo".parse().unwrap();
        let root_uri: CanonicalFileUri = if cfg!(windows) {
            "file:///C:/private/session-secret-workspace"
                .parse()
                .unwrap()
        } else {
            "file:///private/session-secret-workspace".parse().unwrap()
        };
        let input = WorkspaceDefinitionInput::new(
            WorkspaceRootInput::new(
                root_key.clone(),
                root_uri,
                RequestedFilesystemAccess::ReadWrite,
                WorkspaceSourcePolicy::new(true, true),
            ),
            Vec::new(),
            WorkspaceCwdSpec::new("repo".parse().unwrap(), "src/private".parse().unwrap()),
        )
        .unwrap();
        let workspace = lower_workspace(
            input,
            WorkspaceRevision::new(NonZeroU64::new(1).unwrap()),
            WorkspacePathTarget::current(),
        )
        .unwrap();
        let model = SessionModelConfig::new(
            ModelSelection::new(
                "private-provider".parse().unwrap(),
                "private-model".parse().unwrap(),
            ),
            ReasoningPreference::High,
            None,
        );
        let prompts =
            SessionPromptSelection::new(vec!["private-session-prompt".parse().unwrap()]).unwrap();
        let timestamp = "2026-08-03T10:01:00.456Z".parse().unwrap();
        let attempt = SealedSessionCreateAttempt::new(
            requested_agent,
            workspace,
            model,
            prompts,
            Some("private session name"),
            Some("private session description"),
            timestamp,
        )
        .unwrap();

        let debug = format!("{attempt:?}");
        for secret in [
            "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "private/session-secret-workspace",
            "file:///",
            "private-provider",
            "private-model",
            "private-session-prompt",
            "private session name",
            "private session description",
        ] {
            assert!(
                !debug.contains(secret),
                "sealed attempt debug leaked {secret:?}"
            );
        }
        let session_id: SessionId = "ses_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap();
        let agent_revision = AgentRevision::new(NonZeroU64::new(1).unwrap());
        let (definition, metadata) = attempt.materialize(
            session_id,
            AgentRevisionRef::new(requested_agent, agent_revision),
        );
        assert_eq!(definition.session_id(), session_id);
        assert_eq!(definition.revision().to_string(), "sdr_1");
        assert_eq!(definition.agent().revision(), agent_revision);
        assert_eq!(definition.workspace().revision().to_string(), "wr_1");
        assert_eq!(definition.created_at(), timestamp);
        assert_eq!(metadata.revision().to_string(), "smr_1");
        assert_eq!(metadata.updated_at(), timestamp);
    }
}
