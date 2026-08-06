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
        AgentRevisionRef, SealedAgentCreateAttempt, SealedSessionCreateAttempt,
        SealedSessionForkAttempt, SessionDefinition, SessionForkAttemptError, SessionModelConfig,
    };
    use crate::model_gateway::{ModelSelection, ReasoningPreference};
    use crate::prompt::{AgentPromptSelection, SessionPromptSelection};
    use crate::wire::{
        AgentId, AgentRevision, CanonicalFileUri, SessionDefinitionRevision, SessionId,
        WorkspaceRevision,
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
