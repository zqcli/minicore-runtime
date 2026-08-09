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
use crate::workspace::{
    Workspace, materialize_session_definition_workspace, workspaces_have_same_semantic_content,
};

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

/// Lifecycle-owned semantic input to one ordinary Session definition CAS. Workspace candidates
/// have already been host-lowered and validated by the Runtime/Workspace owner. Their revision is
/// deliberately not trusted here: the Session owner materializes the authoritative current-or-
/// next WorkspaceRevision only after it has read the current head under the Session gate.
pub(crate) struct SealedSessionDefinitionAttempt {
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    workspace: Option<Workspace>,
    model: Option<SessionModelConfig>,
    prompts: Option<SessionPromptSelection>,
    owner_timestamp: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionDefinitionDecisionError {
    #[error("Session definition compare-and-swap is stale")]
    StaleRevision,
    #[error("Session is archived")]
    SessionArchived,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session definition revision is exhausted")]
    RevisionExhausted,
}

#[allow(
    clippy::large_enum_variant,
    reason = "the sealed decision keeps the complete immutable candidate in one owner value"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionDefinitionDecision {
    NoChange,
    Publish(SessionDefinition),
}

impl SealedSessionDefinitionAttempt {
    #[allow(
        dead_code,
        reason = "the public Session definition command constructor consumes this sealed seam"
    )]
    pub(crate) fn new(
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        workspace: Option<Workspace>,
        model: Option<SessionModelConfig>,
        prompts: Option<SessionPromptSelection>,
        owner_timestamp: Timestamp,
    ) -> Self {
        Self {
            session_id,
            expected_revision,
            workspace,
            model,
            prompts,
            owner_timestamp,
        }
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) const fn expected_revision(&self) -> SessionDefinitionRevision {
        self.expected_revision
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) const fn owner_timestamp(&self) -> Timestamp {
        self.owner_timestamp
    }

    /// Decides the ordinary definition CAS in its authoritative order: expected revision,
    /// Open lifecycle, complete replacement application, canonical no-op, then checked next
    /// SessionDefinitionRevision and WorkspaceRevision materialization.
    pub(crate) fn decide(
        &self,
        current_lifecycle: SessionLifecycle,
        current_definition: &SessionDefinition,
    ) -> Result<SessionDefinitionDecision, SessionDefinitionDecisionError> {
        if current_definition.revision() != self.expected_revision {
            return Err(SessionDefinitionDecisionError::StaleRevision);
        }
        match current_lifecycle {
            SessionLifecycle::Open => {}
            SessionLifecycle::Archived => {
                return Err(SessionDefinitionDecisionError::SessionArchived);
            }
            SessionLifecycle::Deleted => {
                return Err(SessionDefinitionDecisionError::SessionDeleted);
            }
        }

        let candidate_workspace = self
            .workspace
            .clone()
            .unwrap_or_else(|| current_definition.workspace().clone());
        let patched_definition = SessionDefinition::new(
            current_definition.session_id(),
            current_definition.revision(),
            current_definition.agent(),
            candidate_workspace,
            self.model
                .clone()
                .unwrap_or_else(|| current_definition.model().clone()),
            self.prompts
                .clone()
                .unwrap_or_else(|| current_definition.prompts().clone()),
            current_definition.created_at(),
        );
        if session_definitions_have_same_canonical_execution_content(
            &patched_definition,
            current_definition,
        ) {
            return Ok(SessionDefinitionDecision::NoChange);
        }

        let next_revision = current_definition
            .revision()
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(SessionDefinitionRevision::new)
            .ok_or(SessionDefinitionDecisionError::RevisionExhausted)?;
        let workspace = self
            .workspace
            .as_ref()
            .map(|candidate| {
                materialize_session_definition_workspace(current_definition.workspace(), candidate)
            })
            .transpose()
            .map_err(
                |crate::workspace::SessionDefinitionWorkspaceMaterializationError::RevisionExhausted| {
                    SessionDefinitionDecisionError::RevisionExhausted
                },
            )?
            .unwrap_or_else(|| current_definition.workspace().clone());
        Ok(SessionDefinitionDecision::Publish(SessionDefinition::new(
            current_definition.session_id(),
            next_revision,
            current_definition.agent(),
            workspace,
            patched_definition.model().clone(),
            patched_definition.prompts().clone(),
            self.owner_timestamp,
        )))
    }
}

impl fmt::Debug for SealedSessionDefinitionAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedSessionDefinitionAttempt")
            .field("session_id", &"redacted")
            .field("expected_revision", &self.expected_revision)
            .field("workspace_present", &self.workspace.is_some())
            .field("model_present", &self.model.is_some())
            .field("prompts_present", &self.prompts.is_some())
            .field("owner_timestamp", &"redacted")
            .finish()
    }
}

/// Lifecycle-owned semantic input to the explicit same-Agent Session upgrade path. `None`
/// resolves the authoritative current Agent revision under the Agent read gate; `Some` pins the
/// exact retained revision supplied by the caller. It is intentionally separate from the closed
/// ordinary definition patch so an AgentRevisionRef cannot be smuggled through that path.
pub(crate) struct SealedSessionAgentUpgradeAttempt {
    session_id: SessionId,
    expected_revision: SessionDefinitionRevision,
    target: Option<AgentRevisionRef>,
    owner_timestamp: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionAgentUpgradeDecisionError {
    #[error("Session definition compare-and-swap is stale")]
    StaleRevision,
    #[error("Session is archived")]
    SessionArchived,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session upgrade targets another Agent")]
    AgentMismatch,
    #[error("Agent is disabled")]
    AgentDisabled,
    #[error("Agent is deleted")]
    AgentDeleted,
    #[error("Agent revision is unavailable")]
    RevisionUnavailable,
    #[error("Session definition revision is exhausted")]
    RevisionExhausted,
}

#[allow(
    clippy::large_enum_variant,
    reason = "the sealed decision keeps the complete immutable candidate in one owner value"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionAgentUpgradeDecision {
    NoChange,
    Publish(SessionDefinition),
}

impl SealedSessionAgentUpgradeAttempt {
    #[allow(
        dead_code,
        reason = "the public Session upgrade command constructor consumes this sealed seam"
    )]
    pub(crate) const fn new(
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        target: Option<AgentRevisionRef>,
        owner_timestamp: Timestamp,
    ) -> Self {
        Self {
            session_id,
            expected_revision,
            target,
            owner_timestamp,
        }
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn target(&self) -> Option<AgentRevisionRef> {
        self.target
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) const fn expected_revision(&self) -> SessionDefinitionRevision {
        self.expected_revision
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) const fn owner_timestamp(&self) -> Timestamp {
        self.owner_timestamp
    }

    /// Resolves and validates the exact target while both private lifecycle gates are held. The
    /// caller supplies only authoritative Agent facts observed under the Agent read gate: its
    /// current revision, status, and retained-definition membership for an explicit target.
    pub(crate) fn decide(
        &self,
        current_lifecycle: SessionLifecycle,
        current_definition: &SessionDefinition,
        agent_status: AgentStatus,
        agent_current_revision: AgentRevision,
        target_is_retained: bool,
    ) -> Result<SessionAgentUpgradeDecision, SessionAgentUpgradeDecisionError> {
        if current_definition.revision() != self.expected_revision {
            return Err(SessionAgentUpgradeDecisionError::StaleRevision);
        }
        match current_lifecycle {
            SessionLifecycle::Open => {}
            SessionLifecycle::Archived => {
                return Err(SessionAgentUpgradeDecisionError::SessionArchived);
            }
            SessionLifecycle::Deleted => {
                return Err(SessionAgentUpgradeDecisionError::SessionDeleted);
            }
        }

        let target = self.target.unwrap_or_else(|| {
            AgentRevisionRef::new(
                current_definition.agent().agent_id(),
                agent_current_revision,
            )
        });
        if target.agent_id() != current_definition.agent().agent_id() {
            return Err(SessionAgentUpgradeDecisionError::AgentMismatch);
        }
        match agent_status {
            AgentStatus::Enabled => {}
            AgentStatus::Disabled => {
                return Err(SessionAgentUpgradeDecisionError::AgentDisabled);
            }
            AgentStatus::Deleted => {
                return Err(SessionAgentUpgradeDecisionError::AgentDeleted);
            }
        }
        if !target_is_retained {
            return Err(SessionAgentUpgradeDecisionError::RevisionUnavailable);
        }
        if target == current_definition.agent() {
            return Ok(SessionAgentUpgradeDecision::NoChange);
        }
        let next_revision = current_definition
            .revision()
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(SessionDefinitionRevision::new)
            .ok_or(SessionAgentUpgradeDecisionError::RevisionExhausted)?;
        Ok(SessionAgentUpgradeDecision::Publish(
            SessionDefinition::new(
                current_definition.session_id(),
                next_revision,
                target,
                current_definition.workspace().clone(),
                current_definition.model().clone(),
                current_definition.prompts().clone(),
                self.owner_timestamp,
            ),
        ))
    }
}

impl fmt::Debug for SealedSessionAgentUpgradeAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedSessionAgentUpgradeAttempt")
            .field("session_id", &"redacted")
            .field("expected_revision", &self.expected_revision)
            .field("target_present", &self.target.is_some())
            .field("owner_timestamp", &"redacted")
            .finish()
    }
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

    fn with_revision(&self, revision: SessionMetadataRevision, updated_at: Timestamp) -> Self {
        Self {
            revision,
            name: self.name.clone(),
            description: self.description.clone(),
            updated_at,
        }
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

/// The sealed name half of a Session metadata patch. Its representation is private so a caller
/// cannot forge an invalid canonical Set value or confuse Keep with Clear.
#[allow(
    dead_code,
    reason = "the sealed Session metadata patch is consumed by the pending Session command surface"
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SessionMetadataNamePatch {
    value: SessionMetadataNamePatchValue,
}

#[allow(
    dead_code,
    reason = "the sealed Session metadata patch is consumed by the pending Session command surface"
)]
#[derive(Clone, Eq, PartialEq)]
enum SessionMetadataNamePatchValue {
    Keep,
    Set(Box<str>),
    Clear,
}

#[allow(
    dead_code,
    reason = "the sealed Session metadata patch is consumed by the pending Session command surface"
)]
impl SessionMetadataNamePatch {
    pub(crate) const fn keep() -> Self {
        Self {
            value: SessionMetadataNamePatchValue::Keep,
        }
    }

    pub(crate) fn set<N>(raw: N) -> Result<Self, SessionMetadataError>
    where
        N: AsRef<str>,
    {
        let limits = ProtocolLimits::v1_0().text;
        let value = normalize_session_metadata_text(
            raw.as_ref(),
            usize::from(limits.max_display_name_bytes),
            false,
        )?;
        Ok(Self {
            value: SessionMetadataNamePatchValue::Set(value),
        })
    }

    pub(crate) const fn clear() -> Self {
        Self {
            value: SessionMetadataNamePatchValue::Clear,
        }
    }

    fn apply_to(&self, current: Option<&str>) -> Option<Box<str>> {
        match &self.value {
            SessionMetadataNamePatchValue::Keep => current.map(Into::into),
            SessionMetadataNamePatchValue::Set(value) => Some(value.clone()),
            SessionMetadataNamePatchValue::Clear => None,
        }
    }
}

impl fmt::Debug for SessionMetadataNamePatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.value {
            SessionMetadataNamePatchValue::Keep => "Keep",
            SessionMetadataNamePatchValue::Set(_) => "Set",
            SessionMetadataNamePatchValue::Clear => "Clear",
        };
        formatter
            .debug_struct("SessionMetadataNamePatch")
            .field("kind", &kind)
            .field("value", &"redacted")
            .finish()
    }
}

/// The sealed description half of a Session metadata patch. Set permits the canonical empty
/// description, while the representation still keeps Keep distinct from Clear.
#[allow(
    dead_code,
    reason = "the sealed Session metadata patch is consumed by the pending Session command surface"
)]
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SessionMetadataDescriptionPatch {
    value: SessionMetadataDescriptionPatchValue,
}

#[allow(
    dead_code,
    reason = "the sealed Session metadata patch is consumed by the pending Session command surface"
)]
#[derive(Clone, Eq, PartialEq)]
enum SessionMetadataDescriptionPatchValue {
    Keep,
    Set(Box<str>),
    Clear,
}

#[allow(
    dead_code,
    reason = "the sealed Session metadata patch is consumed by the pending Session command surface"
)]
impl SessionMetadataDescriptionPatch {
    pub(crate) const fn keep() -> Self {
        Self {
            value: SessionMetadataDescriptionPatchValue::Keep,
        }
    }

    pub(crate) fn set<D>(raw: D) -> Result<Self, SessionMetadataError>
    where
        D: AsRef<str>,
    {
        let limits = ProtocolLimits::v1_0().text;
        let value = normalize_session_metadata_text(
            raw.as_ref(),
            usize::try_from(limits.max_description_bytes).unwrap_or(usize::MAX),
            true,
        )?;
        Ok(Self {
            value: SessionMetadataDescriptionPatchValue::Set(value),
        })
    }

    pub(crate) const fn clear() -> Self {
        Self {
            value: SessionMetadataDescriptionPatchValue::Clear,
        }
    }

    fn apply_to(&self, current: Option<&str>) -> Option<Box<str>> {
        match &self.value {
            SessionMetadataDescriptionPatchValue::Keep => current.map(Into::into),
            SessionMetadataDescriptionPatchValue::Set(value) => Some(value.clone()),
            SessionMetadataDescriptionPatchValue::Clear => None,
        }
    }
}

impl fmt::Debug for SessionMetadataDescriptionPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.value {
            SessionMetadataDescriptionPatchValue::Keep => "Keep",
            SessionMetadataDescriptionPatchValue::Set(_) => "Set",
            SessionMetadataDescriptionPatchValue::Clear => "Clear",
        };
        formatter
            .debug_struct("SessionMetadataDescriptionPatch")
            .field("kind", &kind)
            .field("value", &"redacted")
            .finish()
    }
}

/// Lifecycle-owned semantic input to one Session metadata CAS. It carries only the Session lookup
/// key, expected current metadata revision, canonical patch intent, and owner timestamp. Storage
/// generation, paths, markers, command identity, and publication handles do not cross this seam.
pub(crate) struct SealedSessionMetadataAttempt {
    session_id: SessionId,
    expected_revision: SessionMetadataRevision,
    name: SessionMetadataNamePatch,
    description: SessionMetadataDescriptionPatch,
    owner_timestamp: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionMetadataDecisionError {
    #[error("Session metadata compare-and-swap is stale")]
    StaleRevision,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session metadata revision is exhausted")]
    RevisionExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionMetadataDecision {
    NoChange,
    Publish(SessionMetadata),
}

impl SealedSessionMetadataAttempt {
    #[allow(
        dead_code,
        reason = "the public Session metadata command constructor consumes this sealed seam"
    )]
    pub(crate) fn new(
        session_id: SessionId,
        expected_revision: SessionMetadataRevision,
        name: SessionMetadataNamePatch,
        description: SessionMetadataDescriptionPatch,
        owner_timestamp: Timestamp,
    ) -> Self {
        Self {
            session_id,
            expected_revision,
            name,
            description,
            owner_timestamp,
        }
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) const fn expected_revision(&self) -> SessionMetadataRevision {
        self.expected_revision
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) const fn owner_timestamp(&self) -> Timestamp {
        self.owner_timestamp
    }

    /// Decides the semantic CAS in its authoritative order: expected metadata revision, terminal
    /// lifecycle, patch application against authoritative current metadata, canonical no-op, then
    /// checked next revision and metadata materialization.
    pub(crate) fn decide(
        &self,
        current_lifecycle: SessionLifecycle,
        current_metadata: &SessionMetadata,
    ) -> Result<SessionMetadataDecision, SessionMetadataDecisionError> {
        let current_revision = current_metadata.revision();
        if current_revision != self.expected_revision {
            return Err(SessionMetadataDecisionError::StaleRevision);
        }
        if current_lifecycle == SessionLifecycle::Deleted {
            return Err(SessionMetadataDecisionError::SessionDeleted);
        }
        let patched_metadata = SessionMetadata {
            revision: current_revision,
            name: self.name.apply_to(current_metadata.name()),
            description: self.description.apply_to(current_metadata.description()),
            updated_at: current_metadata.updated_at(),
        };
        if session_metadata_has_same_canonical_content(&patched_metadata, current_metadata) {
            return Ok(SessionMetadataDecision::NoChange);
        }
        let next_revision = current_revision
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(SessionMetadataRevision::new)
            .ok_or(SessionMetadataDecisionError::RevisionExhausted)?;
        Ok(SessionMetadataDecision::Publish(
            patched_metadata.with_revision(next_revision, self.owner_timestamp),
        ))
    }
}

impl fmt::Debug for SealedSessionMetadataAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedSessionMetadataAttempt")
            .field("session_id", &"redacted")
            .field("expected_revision", &self.expected_revision)
            .field("name_patch", &"redacted")
            .field("description_patch", &"redacted")
            .field("owner_timestamp", &"redacted")
            .finish()
    }
}

/// The lifecycle-owned, pre-identity input to one Session Fork attempt.
///
/// The source Session identity, selected source kind, public anchor, and child-local creation
/// timestamp are captured here. DurableState supplies the child identity and the exact source
/// definition after actor serialization; physical storage and source conversation facts do not
/// cross this seam.
pub(crate) struct SealedSessionForkAttempt {
    source_session_id: SessionId,
    source: ForkSourceKind,
    anchor: ForkAnchor,
    child_created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionForkAttemptError {
    #[error("fork source definition does not match the captured source")]
    SourceDefinitionMismatch,
}

impl SealedSessionForkAttempt {
    pub(crate) const fn new(
        source_session_id: SessionId,
        source: ForkSourceKind,
        anchor: ForkAnchor,
        child_created_at: Timestamp,
    ) -> Self {
        Self {
            source_session_id,
            source,
            anchor,
            child_created_at,
        }
    }

    #[allow(
        dead_code,
        reason = "the public Session Fork command constructor consumes this sealed lifecycle seam"
    )]
    pub(crate) const fn recorded_genesis(
        source_session_id: SessionId,
        child_created_at: Timestamp,
    ) -> Self {
        Self::new(
            source_session_id,
            ForkSourceKind::RecordedHistory,
            ForkAnchor::Genesis,
            child_created_at,
        )
    }

    pub(crate) const fn source_session_id(&self) -> SessionId {
        self.source_session_id
    }

    pub(crate) const fn source(&self) -> ForkSourceKind {
        self.source
    }

    pub(crate) const fn anchor(&self) -> &ForkAnchor {
        &self.anchor
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
        .expect("empty Fork child metadata is always valid");
        let definition = SessionDefinition::new(
            child_session_id,
            definition_revision,
            source_definition.agent(),
            workspace,
            source_definition.model().clone(),
            source_definition.prompts().clone(),
            self.child_created_at,
        );
        let provenance =
            SessionForkProvenance::new(self.source_session_id, self.source, self.anchor.clone());
        Ok((definition, metadata, provenance))
    }
}

impl fmt::Debug for SealedSessionForkAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedSessionForkAttempt")
            .field("source_session", &"redacted")
            .field("source", &self.source)
            .field("anchor", &self.anchor)
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

/// The only semantic actions admitted by an existing-head Session lifecycle mutation. Keeping the
/// action set closed prevents a caller from manufacturing an arbitrary lifecycle target such as
/// `Open -> Deleted` and accidentally treating it as a valid mutation.
#[allow(
    dead_code,
    reason = "the pending Session lifecycle command constructs these sealed actions"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionLifecycleAction {
    Archive,
    Unarchive,
    Delete,
}

impl SessionLifecycleAction {
    const fn target(self) -> SessionLifecycle {
        match self {
            Self::Archive => SessionLifecycle::Archived,
            Self::Unarchive => SessionLifecycle::Open,
            Self::Delete => SessionLifecycle::Deleted,
        }
    }
}

/// Lifecycle-owned semantic input to one existing-head Session lifecycle mutation. It deliberately
/// carries no expected lifecycle token: the authoritative actor reads current state after taking
/// the Session gate. Residency is not represented here because Archive/Delete's Unloaded
/// precondition belongs to the future Runtime/residency owner, not to this durable-only seam.
pub(crate) struct SealedSessionLifecycleAttempt {
    session_id: SessionId,
    action: SessionLifecycleAction,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionLifecycleDecisionError {
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session lifecycle transition is invalid")]
    InvalidTransition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionLifecycleDecision {
    NoChange,
    Publish(SessionLifecycle),
}

#[allow(
    dead_code,
    reason = "the pending Session lifecycle command consumes this sealed attempt"
)]
impl SealedSessionLifecycleAttempt {
    const fn new(session_id: SessionId, action: SessionLifecycleAction) -> Self {
        Self { session_id, action }
    }

    pub(crate) const fn archive(session_id: SessionId) -> Self {
        Self::new(session_id, SessionLifecycleAction::Archive)
    }

    pub(crate) const fn unarchive(session_id: SessionId) -> Self {
        Self::new(session_id, SessionLifecycleAction::Unarchive)
    }

    pub(crate) const fn delete(session_id: SessionId) -> Self {
        Self::new(session_id, SessionLifecycleAction::Delete)
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[cfg(test)]
    const fn action(&self) -> SessionLifecycleAction {
        self.action
    }

    /// Decides the closed lifecycle action matrix against the current state supplied by the
    /// authoritative DurableState actor after it has acquired the Session gate.
    pub(crate) fn decide(
        &self,
        current_lifecycle: SessionLifecycle,
    ) -> Result<SessionLifecycleDecision, SessionLifecycleDecisionError> {
        if current_lifecycle == SessionLifecycle::Deleted {
            return Err(SessionLifecycleDecisionError::SessionDeleted);
        }
        let target = self.action.target();
        if current_lifecycle == target {
            return Ok(SessionLifecycleDecision::NoChange);
        }
        if !is_legal_session_lifecycle_transition(current_lifecycle, target) {
            return Err(SessionLifecycleDecisionError::InvalidTransition);
        }
        Ok(SessionLifecycleDecision::Publish(target))
    }
}

impl fmt::Debug for SealedSessionLifecycleAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedSessionLifecycleAttempt")
            .field("session_id", &"redacted")
            .field("action", &self.action)
            .finish()
    }
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

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::{
        AgentDefinitionDecision, AgentMetadataDecision, AgentMetadataDecisionError,
        AgentMetadataDescriptionPatch, AgentRevisionRef, AgentStatus, AgentStatusAttemptError,
        SealedAgentCreateAttempt, SealedAgentDefinitionAttempt, SealedAgentMetadataAttempt,
        SealedAgentStatusAttempt, SealedSessionAgentUpgradeAttempt, SealedSessionCreateAttempt,
        SealedSessionDefinitionAttempt, SealedSessionForkAttempt, SealedSessionLifecycleAttempt,
        SealedSessionMetadataAttempt, SessionAgentUpgradeDecision,
        SessionAgentUpgradeDecisionError, SessionDefinition, SessionDefinitionDecision,
        SessionDefinitionDecisionError, SessionForkAttemptError, SessionLifecycle,
        SessionLifecycleAction, SessionLifecycleDecision, SessionLifecycleDecisionError,
        SessionMetadataDecision, SessionMetadataDecisionError, SessionMetadataDescriptionPatch,
        SessionMetadataNamePatch, SessionModelConfig,
    };
    use crate::model_gateway::{ModelSelection, ReasoningPreference};
    use crate::prompt::{AgentPromptSelection, SessionPromptSelection};
    use crate::wire::{
        AgentId, AgentMetadataRevision, AgentRevision, CanonicalFileUri, SessionDefinitionRevision,
        SessionId, WorkspaceRevision,
    };
    use crate::workspace::{
        RequestedFilesystemAccess, Workspace, WorkspaceCwdSpec, WorkspaceDefinitionInput,
        WorkspacePathTarget, WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy,
        lower_workspace,
    };

    #[test]
    fn sealed_session_lifecycle_action_matrix_is_closed_and_redacted() {
        let session_id: SessionId = "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let cases = [
            (
                SessionLifecycle::Open,
                SessionLifecycleAction::Archive,
                Ok(SessionLifecycleDecision::Publish(
                    SessionLifecycle::Archived,
                )),
            ),
            (
                SessionLifecycle::Archived,
                SessionLifecycleAction::Archive,
                Ok(SessionLifecycleDecision::NoChange),
            ),
            (
                SessionLifecycle::Deleted,
                SessionLifecycleAction::Archive,
                Err(SessionLifecycleDecisionError::SessionDeleted),
            ),
            (
                SessionLifecycle::Open,
                SessionLifecycleAction::Unarchive,
                Ok(SessionLifecycleDecision::NoChange),
            ),
            (
                SessionLifecycle::Archived,
                SessionLifecycleAction::Unarchive,
                Ok(SessionLifecycleDecision::Publish(SessionLifecycle::Open)),
            ),
            (
                SessionLifecycle::Deleted,
                SessionLifecycleAction::Unarchive,
                Err(SessionLifecycleDecisionError::SessionDeleted),
            ),
            (
                SessionLifecycle::Open,
                SessionLifecycleAction::Delete,
                Err(SessionLifecycleDecisionError::InvalidTransition),
            ),
            (
                SessionLifecycle::Archived,
                SessionLifecycleAction::Delete,
                Ok(SessionLifecycleDecision::Publish(SessionLifecycle::Deleted)),
            ),
            (
                SessionLifecycle::Deleted,
                SessionLifecycleAction::Delete,
                Err(SessionLifecycleDecisionError::SessionDeleted),
            ),
        ];
        for (current, action, expected) in cases {
            let attempt = SealedSessionLifecycleAttempt::new(session_id, action);
            assert_eq!(attempt.session_id(), session_id);
            assert_eq!(attempt.action(), action);
            assert_eq!(attempt.decide(current), expected);
        }

        let archive = SealedSessionLifecycleAttempt::archive(session_id);
        let debug = format!("{archive:?}");
        assert!(!debug.contains("ses_aaaaaaaa"));
        assert!(!debug.contains("expected"));
        assert!(!debug.contains("stale"));
    }

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
    fn sealed_session_metadata_patch_preserves_intents_and_orders_cas() {
        let session_id: SessionId = "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let revision = super::SessionMetadataRevision::new(NonZeroU64::new(1).unwrap());
        let current_timestamp = "2026-08-03T10:00:00.123Z".parse().unwrap();
        let owner_timestamp = "2026-08-03T10:00:05.000Z".parse().unwrap();
        let current = super::SessionMetadata::new(
            revision,
            Some("Planner"),
            Some("Description"),
            current_timestamp,
        )
        .unwrap();
        let changed = SealedSessionMetadataAttempt::new(
            session_id,
            revision,
            SessionMetadataNamePatch::set("Revised\r\nname").unwrap(),
            SessionMetadataDescriptionPatch::set("Revised\rdescription").unwrap(),
            owner_timestamp,
        );
        let debug = format!("{changed:?}");
        for secret in ["ses_aaaaaaaa", "Revised", "10:00:05"] {
            assert!(
                !debug.contains(secret),
                "metadata attempt leaked {secret:?}"
            );
        }
        assert_eq!(changed.expected_revision(), revision);
        assert_eq!(changed.owner_timestamp(), owner_timestamp);

        let stale = super::SessionMetadata::new(
            super::SessionMetadataRevision::new(NonZeroU64::new(2).unwrap()),
            Some("Planner"),
            Some("Description"),
            current_timestamp,
        )
        .unwrap();
        assert_eq!(
            changed
                .decide(SessionLifecycle::Deleted, &stale)
                .unwrap_err(),
            SessionMetadataDecisionError::StaleRevision
        );
        assert_eq!(
            changed
                .decide(SessionLifecycle::Deleted, &current)
                .unwrap_err(),
            SessionMetadataDecisionError::SessionDeleted
        );
        let SessionMetadataDecision::Publish(published) = changed
            .decide(SessionLifecycle::Archived, &current)
            .unwrap()
        else {
            panic!("changed Session metadata publishes");
        };
        assert_eq!(published.revision().get(), 2);
        assert_eq!(published.name(), Some("Revised\nname"));
        assert_eq!(published.description(), Some("Revised\ndescription"));
        assert_eq!(published.updated_at(), owner_timestamp);

        for (name, description, expected_name, expected_description) in [
            (
                SessionMetadataNamePatch::keep(),
                SessionMetadataDescriptionPatch::keep(),
                Some("Planner"),
                Some("Description"),
            ),
            (
                SessionMetadataNamePatch::clear(),
                SessionMetadataDescriptionPatch::keep(),
                None,
                Some("Description"),
            ),
            (
                SessionMetadataNamePatch::keep(),
                SessionMetadataDescriptionPatch::clear(),
                Some("Planner"),
                None,
            ),
            (
                SessionMetadataNamePatch::set("Planner").unwrap(),
                SessionMetadataDescriptionPatch::set("").unwrap(),
                Some("Planner"),
                Some(""),
            ),
        ] {
            let attempt = SealedSessionMetadataAttempt::new(
                session_id,
                revision,
                name,
                description,
                owner_timestamp,
            );
            let decision = attempt.decide(SessionLifecycle::Open, &current).unwrap();
            if expected_name == current.name() && expected_description == current.description() {
                assert_eq!(decision, SessionMetadataDecision::NoChange);
            } else {
                let SessionMetadataDecision::Publish(metadata) = decision else {
                    panic!("the Session metadata patch changes canonical content");
                };
                assert_eq!(metadata.name(), expected_name);
                assert_eq!(metadata.description(), expected_description);
            }
        }

        let equivalent = SealedSessionMetadataAttempt::new(
            session_id,
            revision,
            SessionMetadataNamePatch::set("Planner").unwrap(),
            SessionMetadataDescriptionPatch::set("Description").unwrap(),
            owner_timestamp,
        );
        assert_eq!(
            equivalent
                .decide(SessionLifecycle::Deleted, &current)
                .unwrap_err(),
            SessionMetadataDecisionError::SessionDeleted
        );

        let exhausted = super::SessionMetadata::new(
            super::SessionMetadataRevision::new(NonZeroU64::new(u64::MAX).unwrap()),
            Some("Planner"),
            Some("Description"),
            current_timestamp,
        )
        .unwrap();
        assert_eq!(
            changed
                .decide(SessionLifecycle::Open, &exhausted)
                .unwrap_err(),
            SessionMetadataDecisionError::StaleRevision
        );
        let exhausted_changed = SealedSessionMetadataAttempt::new(
            session_id,
            exhausted.revision(),
            SessionMetadataNamePatch::set("Different").unwrap(),
            SessionMetadataDescriptionPatch::keep(),
            owner_timestamp,
        );
        assert_eq!(
            exhausted_changed
                .decide(SessionLifecycle::Open, &exhausted)
                .unwrap_err(),
            SessionMetadataDecisionError::RevisionExhausted
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

    fn definition_test_workspace(revision: u64, location: &str) -> Workspace {
        let uri = if cfg!(windows) {
            format!("file:///C:/workspace/{location}")
        } else {
            format!("file:///workspace/{location}")
        };
        lower_workspace(
            WorkspaceDefinitionInput::new(
                WorkspaceRootInput::new(
                    "repo".parse().unwrap(),
                    uri.parse().unwrap(),
                    RequestedFilesystemAccess::ReadWrite,
                    WorkspaceSourcePolicy::new(true, true),
                ),
                Vec::new(),
                WorkspaceCwdSpec::new("repo".parse().unwrap(), "src".parse().unwrap()),
            )
            .unwrap(),
            WorkspaceRevision::new(NonZeroU64::new(revision).unwrap()),
            WorkspacePathTarget::current(),
        )
        .unwrap()
    }

    fn definition_test_model(model: &str) -> SessionModelConfig {
        SessionModelConfig::new(
            ModelSelection::new("provider".parse().unwrap(), model.parse().unwrap()),
            ReasoningPreference::Auto,
            Some(NonZeroU32::new(2048).unwrap()),
        )
    }

    fn definition_test_prompts(values: &[&str]) -> SessionPromptSelection {
        SessionPromptSelection::new(values.iter().map(|value| value.parse().unwrap()).collect())
            .unwrap()
    }

    fn definition_test_current(
        session_revision: u64,
        agent_revision: u64,
        workspace: Workspace,
        model: &str,
        prompts: &[&str],
    ) -> SessionDefinition {
        SessionDefinition::new(
            "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
            SessionDefinitionRevision::new(NonZeroU64::new(session_revision).unwrap()),
            AgentRevisionRef::new(
                "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap(),
                AgentRevision::new(NonZeroU64::new(agent_revision).unwrap()),
            ),
            workspace,
            definition_test_model(model),
            definition_test_prompts(prompts),
            "2026-08-03T10:00:00.123Z".parse().unwrap(),
        )
    }

    #[test]
    fn sealed_session_definition_patch_matrix_is_authoritative_and_redacted() {
        let session_id: SessionId = "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let current_workspace = definition_test_workspace(1, "current-secret-path");
        let current = definition_test_current(
            1,
            1,
            current_workspace.clone(),
            "model-current-secret",
            &["prompt-current-secret"],
        );
        let owner_timestamp = "2026-08-03T10:00:05.000Z".parse().unwrap();
        let empty = SealedSessionDefinitionAttempt::new(
            session_id,
            current.revision(),
            None,
            None,
            None,
            owner_timestamp,
        );

        assert_eq!(
            empty
                .decide(SessionLifecycle::Deleted, &current)
                .unwrap_err(),
            SessionDefinitionDecisionError::SessionDeleted
        );
        let stale_current = definition_test_current(
            2,
            1,
            current_workspace.clone(),
            "model-current-secret",
            &["prompt-current-secret"],
        );
        assert_eq!(
            empty
                .decide(SessionLifecycle::Deleted, &stale_current)
                .unwrap_err(),
            SessionDefinitionDecisionError::StaleRevision,
            "stale wins before lifecycle and no-op"
        );
        assert_eq!(
            empty
                .decide(SessionLifecycle::Archived, &current)
                .unwrap_err(),
            SessionDefinitionDecisionError::SessionArchived
        );
        assert_eq!(
            empty.decide(SessionLifecycle::Open, &current).unwrap(),
            SessionDefinitionDecision::NoChange
        );

        let prompt_only = SealedSessionDefinitionAttempt::new(
            session_id,
            current.revision(),
            None,
            None,
            Some(definition_test_prompts(&[
                "prompt-current-secret",
                "prompt-new",
            ])),
            owner_timestamp,
        )
        .decide(SessionLifecycle::Open, &current)
        .unwrap();
        let model_only = SealedSessionDefinitionAttempt::new(
            session_id,
            current.revision(),
            None,
            Some(definition_test_model("model-new")),
            None,
            owner_timestamp,
        )
        .decide(SessionLifecycle::Open, &current)
        .unwrap();
        let workspace_only = SealedSessionDefinitionAttempt::new(
            session_id,
            current.revision(),
            Some(definition_test_workspace(99, "workspace-new")),
            None,
            None,
            owner_timestamp,
        )
        .decide(SessionLifecycle::Open, &current)
        .unwrap();
        let combined = SealedSessionDefinitionAttempt::new(
            session_id,
            current.revision(),
            Some(definition_test_workspace(42, "workspace-combined")),
            Some(definition_test_model("model-combined")),
            Some(definition_test_prompts(&["prompt-combined"])),
            owner_timestamp,
        )
        .decide(SessionLifecycle::Open, &current)
        .unwrap();

        let SessionDefinitionDecision::Publish(prompt_only) = prompt_only else {
            panic!("prompt-only replacement publishes");
        };
        assert_eq!(prompt_only.revision().get(), 2);
        assert_eq!(
            prompt_only.workspace().revision(),
            current.workspace().revision()
        );
        let SessionDefinitionDecision::Publish(model_only) = model_only else {
            panic!("model-only replacement publishes");
        };
        assert_eq!(model_only.revision().get(), 2);
        assert_eq!(
            model_only.workspace().revision(),
            current.workspace().revision()
        );
        let SessionDefinitionDecision::Publish(workspace_only) = workspace_only else {
            panic!("Workspace-only replacement publishes");
        };
        assert_eq!(workspace_only.revision().get(), 2);
        assert_eq!(workspace_only.workspace().revision().get(), 2);
        let SessionDefinitionDecision::Publish(combined) = combined else {
            panic!("combined replacement publishes");
        };
        assert_eq!(combined.revision().get(), 2);
        assert_eq!(combined.workspace().revision().get(), 2);

        let equivalent_workspace_with_model_change = SealedSessionDefinitionAttempt::new(
            session_id,
            current.revision(),
            Some(definition_test_workspace(777, "current-secret-path")),
            Some(definition_test_model("model-equivalent-workspace")),
            None,
            owner_timestamp,
        )
        .decide(SessionLifecycle::Open, &current)
        .unwrap();
        let SessionDefinitionDecision::Publish(equivalent_workspace_with_model_change) =
            equivalent_workspace_with_model_change
        else {
            panic!("the model change publishes");
        };
        assert_eq!(
            equivalent_workspace_with_model_change
                .workspace()
                .revision(),
            current.workspace().revision(),
            "an arbitrary candidate revision never replaces the authoritative Workspace revision"
        );

        let exhausted_session = definition_test_current(
            u64::MAX,
            1,
            current_workspace.clone(),
            "model-current-secret",
            &["prompt-current-secret"],
        );
        let changed_model = SealedSessionDefinitionAttempt::new(
            session_id,
            exhausted_session.revision(),
            None,
            Some(definition_test_model("model-after-exhaustion")),
            None,
            owner_timestamp,
        );
        assert_eq!(
            changed_model
                .decide(SessionLifecycle::Open, &exhausted_session)
                .unwrap_err(),
            SessionDefinitionDecisionError::RevisionExhausted
        );
        let exhausted_empty = SealedSessionDefinitionAttempt::new(
            session_id,
            exhausted_session.revision(),
            None,
            None,
            None,
            owner_timestamp,
        );
        assert_eq!(
            exhausted_empty.decide(SessionLifecycle::Open, &exhausted_session),
            Ok(SessionDefinitionDecision::NoChange)
        );

        let exhausted_workspace = definition_test_current(
            1,
            1,
            definition_test_workspace(u64::MAX, "current-secret-path"),
            "model-current-secret",
            &["prompt-current-secret"],
        );
        assert_eq!(
            SealedSessionDefinitionAttempt::new(
                session_id,
                exhausted_workspace.revision(),
                Some(definition_test_workspace(1, "workspace-overflow")),
                None,
                None,
                owner_timestamp,
            )
            .decide(SessionLifecycle::Open, &exhausted_workspace)
            .unwrap_err(),
            SessionDefinitionDecisionError::RevisionExhausted,
            "only a changed Workspace asks for a next WorkspaceRevision"
        );
        assert!(matches!(
            SealedSessionDefinitionAttempt::new(
                session_id,
                exhausted_workspace.revision(),
                Some(definition_test_workspace(99, "current-secret-path")),
                Some(definition_test_model("model-after-workspace-overflow")),
                None,
                owner_timestamp,
            )
            .decide(SessionLifecycle::Open, &exhausted_workspace),
            Ok(SessionDefinitionDecision::Publish(_))
        ));

        let debug = format!(
            "{:?}",
            SealedSessionDefinitionAttempt::new(
                session_id,
                current.revision(),
                Some(definition_test_workspace(1, "debug-path-secret")),
                Some(definition_test_model("debug-model-secret")),
                Some(definition_test_prompts(&["debug-prompt-secret"])),
                "2026-08-03T10:00:05.999Z".parse().unwrap(),
            )
        );
        for secret in [
            "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "debug-model-secret",
            "debug-prompt-secret",
            "debug-path-secret",
            "10:00:05.999",
        ] {
            assert!(
                !debug.contains(secret),
                "definition debug leaked {secret:?}"
            );
        }
    }

    #[test]
    fn sealed_session_agent_upgrade_matrix_preserves_target_and_gate_ordering_facts() {
        let session_id: SessionId = "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let agent_id: AgentId = "agt_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
        let other_agent_id: AgentId = "agt_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap();
        let current = definition_test_current(
            1,
            1,
            definition_test_workspace(1, "upgrade-workspace"),
            "upgrade-model",
            &["upgrade-prompt"],
        );
        let owner_timestamp = "2026-08-03T10:00:05.000Z".parse().unwrap();
        let current_agent = AgentRevision::new(NonZeroU64::new(2).unwrap());
        let historical_agent = current.agent().revision();
        let latest = SealedSessionAgentUpgradeAttempt::new(
            session_id,
            current.revision(),
            None,
            owner_timestamp,
        );

        assert_eq!(
            SealedSessionAgentUpgradeAttempt::new(
                session_id,
                "sdr_2".parse().unwrap(),
                None,
                owner_timestamp,
            )
            .decide(
                SessionLifecycle::Deleted,
                &current,
                AgentStatus::Deleted,
                current_agent,
                false,
            )
            .unwrap_err(),
            SessionAgentUpgradeDecisionError::StaleRevision,
            "stale wins before lifecycle, status, and retention"
        );
        assert_eq!(
            latest
                .decide(
                    SessionLifecycle::Archived,
                    &current,
                    AgentStatus::Enabled,
                    current_agent,
                    true,
                )
                .unwrap_err(),
            SessionAgentUpgradeDecisionError::SessionArchived
        );
        assert_eq!(
            latest
                .decide(
                    SessionLifecycle::Deleted,
                    &current,
                    AgentStatus::Enabled,
                    current_agent,
                    true,
                )
                .unwrap_err(),
            SessionAgentUpgradeDecisionError::SessionDeleted
        );
        assert_eq!(
            SealedSessionAgentUpgradeAttempt::new(
                session_id,
                current.revision(),
                Some(AgentRevisionRef::new(other_agent_id, current_agent,)),
                owner_timestamp,
            )
            .decide(
                SessionLifecycle::Open,
                &current,
                AgentStatus::Deleted,
                current_agent,
                false,
            )
            .unwrap_err(),
            SessionAgentUpgradeDecisionError::AgentMismatch,
            "Agent identity wins before status and retention"
        );

        for status in [AgentStatus::Disabled, AgentStatus::Deleted] {
            for target in [
                None,
                Some(AgentRevisionRef::new(agent_id, current_agent)),
                Some(AgentRevisionRef::new(agent_id, historical_agent)),
                Some(current.agent()),
            ] {
                assert_eq!(
                    SealedSessionAgentUpgradeAttempt::new(
                        session_id,
                        current.revision(),
                        target,
                        owner_timestamp,
                    )
                    .decide(
                        SessionLifecycle::Open,
                        &current,
                        status,
                        current_agent,
                        true
                    )
                    .unwrap_err(),
                    match status {
                        AgentStatus::Disabled => SessionAgentUpgradeDecisionError::AgentDisabled,
                        AgentStatus::Deleted => SessionAgentUpgradeDecisionError::AgentDeleted,
                        AgentStatus::Enabled => unreachable!(),
                    }
                );
            }
        }

        assert_eq!(
            SealedSessionAgentUpgradeAttempt::new(
                session_id,
                current.revision(),
                Some(AgentRevisionRef::new(
                    agent_id,
                    AgentRevision::new(NonZeroU64::new(3).unwrap()),
                )),
                owner_timestamp,
            )
            .decide(
                SessionLifecycle::Open,
                &current,
                AgentStatus::Enabled,
                current_agent,
                false,
            )
            .unwrap_err(),
            SessionAgentUpgradeDecisionError::RevisionUnavailable
        );

        let exact_current = SealedSessionAgentUpgradeAttempt::new(
            session_id,
            current.revision(),
            Some(current.agent()),
            owner_timestamp,
        )
        .decide(
            SessionLifecycle::Open,
            &current,
            AgentStatus::Enabled,
            current_agent,
            true,
        )
        .unwrap();
        assert_eq!(exact_current, SessionAgentUpgradeDecision::NoChange);
        assert!(matches!(
            latest
                .decide(
                    SessionLifecycle::Open,
                    &current,
                    AgentStatus::Enabled,
                    current_agent,
                    true,
                )
                .unwrap(),
            SessionAgentUpgradeDecision::Publish(_)
        ));

        let SessionAgentUpgradeDecision::Publish(upgraded) = latest
            .decide(
                SessionLifecycle::Open,
                &current,
                AgentStatus::Enabled,
                current_agent,
                true,
            )
            .unwrap()
        else {
            panic!("the latest Agent revision publishes");
        };
        assert_eq!(upgraded.revision().get(), 2);
        assert_eq!(
            upgraded.agent(),
            AgentRevisionRef::new(agent_id, current_agent)
        );
        let rollback = SealedSessionAgentUpgradeAttempt::new(
            session_id,
            upgraded.revision(),
            Some(current.agent()),
            "2026-08-03T10:00:06.000Z".parse().unwrap(),
        )
        .decide(
            SessionLifecycle::Open,
            &upgraded,
            AgentStatus::Enabled,
            current_agent,
            true,
        )
        .unwrap();
        let SessionAgentUpgradeDecision::Publish(rollback) = rollback else {
            panic!("the retained historical Agent revision publishes");
        };
        assert_eq!(rollback.revision().get(), 3);
        assert_eq!(rollback.agent(), current.agent());

        let exhausted = definition_test_current(
            u64::MAX,
            1,
            definition_test_workspace(1, "upgrade-workspace"),
            "upgrade-model",
            &["upgrade-prompt"],
        );
        let exhausted_latest = SealedSessionAgentUpgradeAttempt::new(
            session_id,
            exhausted.revision(),
            None,
            owner_timestamp,
        );
        assert_eq!(
            exhausted_latest
                .decide(
                    SessionLifecycle::Open,
                    &exhausted,
                    AgentStatus::Enabled,
                    current_agent,
                    true,
                )
                .unwrap_err(),
            SessionAgentUpgradeDecisionError::RevisionExhausted
        );

        let debug = format!(
            "{:?}",
            SealedSessionAgentUpgradeAttempt::new(
                session_id,
                current.revision(),
                Some(AgentRevisionRef::new(agent_id, current_agent)),
                "2026-08-03T10:00:05.999Z".parse().unwrap(),
            )
        );
        assert!(!debug.contains("ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!debug.contains("10:00:05.999"));
    }
}
