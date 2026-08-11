use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;

use crate::agent_session_lifecycle::AgentRevisionRef;
use crate::compaction::{
    CompactionSummaryBudget, CompactionSummaryDirective, CompactionSummarySourceView,
};
use crate::live_conversation::{ConversationRevision, LiveConversationView};
use crate::model_gateway::{
    ModelCallPurpose, OutputContract, ReasoningContent, TokenEstimator, TurnModelRef,
    TurnModelSnapshot,
};
use crate::skills::{SkillId, SkillPromptView};
use crate::tools::{ToolCallId, ToolName, ToolPromptView, ToolResultContent, ToolSet};
use crate::wire::lexical::{
    LexicalError, canonical_json_string_len, normalize_newlines, validate_safe_text,
    validate_stable_symbolic_key,
};
use crate::wire::{
    BoundedJsonObject, ProtocolLimits, SessionDefinitionRevision, SessionId, WorkspaceRelativePath,
};
use crate::workspace::{
    CapturedWorkspacePromptSource, WorkspacePromptCaptureContext, WorkspacePromptContext,
    WorkspaceRootKey,
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PromptValueError {
    #[error("prompt text must be non-empty")]
    EmptyText,
    #[error("prompt text or aggregate exceeds its byte limit")]
    TextTooLong,
    #[error("prompt text contains unsafe control characters")]
    UnsafeText,
    #[error("prompt intent has too many skills")]
    TooManySkills,
    #[error("prompt intent contains a duplicate skill")]
    DuplicateSkill,
    #[error("message part count is outside the supported range")]
    InvalidPartCount,
    #[error("prompt contribution stamp is invalid")]
    InvalidContributionStamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PromptIdError {
    #[error("prompt id must be 1..=128 bytes")]
    InvalidLength,
    #[error("prompt id violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PromptId(Box<str>);

impl PromptId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PromptId {
    type Err = PromptIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 128, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => PromptIdError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                PromptIdError::InvalidGrammar
            }
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for PromptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for PromptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SessionPromptSelectionError {
    #[error("session prompt selection has too many entries")]
    TooManyPrompts,
    #[error("session prompt selection contains a duplicate prompt")]
    DuplicatePrompt,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AgentPromptSelectionError {
    #[error("agent prompt selection has too many entries")]
    TooManyPrompts,
    #[error("agent prompt selection contains a duplicate prompt")]
    DuplicatePrompt,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionPromptSelection {
    enabled: BTreeSet<PromptId>,
}

impl SessionPromptSelection {
    pub fn new(enabled: Vec<PromptId>) -> Result<Self, SessionPromptSelectionError> {
        Self::new_with_maximum(
            enabled,
            usize::try_from(ProtocolLimits::v1_0().transport.max_array_items).unwrap_or(usize::MAX),
        )
    }

    pub(crate) fn new_with_maximum(
        enabled: Vec<PromptId>,
        maximum: usize,
    ) -> Result<Self, SessionPromptSelectionError> {
        prompt_selection_set(enabled, maximum)
            .map_err(|error| match error {
                PromptSelectionError::TooManyPrompts => SessionPromptSelectionError::TooManyPrompts,
                PromptSelectionError::DuplicatePrompt => {
                    SessionPromptSelectionError::DuplicatePrompt
                }
            })
            .map(|enabled| Self { enabled })
    }

    pub fn enabled(&self) -> &BTreeSet<PromptId> {
        &self.enabled
    }
}

impl fmt::Debug for SessionPromptSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionPromptSelection")
            .field("enabled_count", &self.enabled.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AgentPromptSelection {
    enabled: BTreeSet<PromptId>,
}

impl AgentPromptSelection {
    pub fn new(enabled: Vec<PromptId>) -> Result<Self, AgentPromptSelectionError> {
        Self::new_with_maximum(
            enabled,
            usize::try_from(ProtocolLimits::v1_0().transport.max_array_items).unwrap_or(usize::MAX),
        )
    }

    pub(crate) fn new_with_maximum(
        enabled: Vec<PromptId>,
        maximum: usize,
    ) -> Result<Self, AgentPromptSelectionError> {
        prompt_selection_set(enabled, maximum)
            .map_err(|error| match error {
                PromptSelectionError::TooManyPrompts => AgentPromptSelectionError::TooManyPrompts,
                PromptSelectionError::DuplicatePrompt => AgentPromptSelectionError::DuplicatePrompt,
            })
            .map(|enabled| Self { enabled })
    }

    pub fn enabled(&self) -> &BTreeSet<PromptId> {
        &self.enabled
    }
}

impl fmt::Debug for AgentPromptSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentPromptSelection")
            .field("enabled_count", &self.enabled.len())
            .finish()
    }
}

#[derive(Clone, Copy)]
enum PromptSelectionError {
    TooManyPrompts,
    DuplicatePrompt,
}

fn prompt_selection_set(
    enabled: Vec<PromptId>,
    maximum: usize,
) -> Result<BTreeSet<PromptId>, PromptSelectionError> {
    if enabled.len() > maximum {
        return Err(PromptSelectionError::TooManyPrompts);
    }
    let original_len = enabled.len();
    let enabled = enabled.into_iter().collect::<BTreeSet<_>>();
    if enabled.len() != original_len {
        return Err(PromptSelectionError::DuplicatePrompt);
    }
    Ok(enabled)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptErrorKind {
    SourceDiscovery,
    ContentLoad,
    DuplicateKey,
    PromptUnavailable,
    InvalidRole,
    RequiredPromptMissing,
    InvalidIntent,
    InvalidContribution,
    ContextLimitExceeded,
    /// An internal invariant, never an ordinary availability or validation outcome.  The
    /// selection-availability seam uses this kind for an owner mismatch so the caller can
    /// distinguish an internal invariant from an ordinary `PromptUnavailable`.
    Internal,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct PromptError {
    kind: PromptErrorKind,
}

impl PromptError {
    const fn new(kind: PromptErrorKind) -> Self {
        Self { kind }
    }

    pub(crate) const fn invalid_contribution() -> Self {
        Self::new(PromptErrorKind::InvalidContribution)
    }

    #[allow(
        dead_code,
        reason = "the Prompt owner exposes this redacted kind to its future Turn mapper"
    )]
    pub(crate) const fn kind(self) -> PromptErrorKind {
        self.kind
    }
}

impl fmt::Debug for PromptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptError")
            .field("kind", &self.kind)
            .finish()
    }
}

impl fmt::Display for PromptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("prompt operation failed")
    }
}

impl std::error::Error for PromptError {}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[allow(
    dead_code,
    reason = "source adapters use this closed failure in the pending filesystem adapters"
)]
pub(crate) enum PromptSourceError {
    #[error("prompt source is unavailable")]
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptRole {
    System,
    User,
}

#[derive(Clone, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "source adapters construct these two trusted provenance families"
)]
pub(crate) enum PromptSourceProvenance {
    Runtime(Box<str>),
    User(Box<str>),
}

#[derive(Clone)]
pub(crate) struct PromptSourceDefinition {
    id: PromptId,
    key: Box<str>,
    name: Box<str>,
    description: Option<Box<str>>,
    role: PromptRole,
    content: Arc<str>,
    provenance: PromptSourceProvenance,
}

#[allow(
    dead_code,
    reason = "source adapters construct complete candidate facts"
)]
impl PromptSourceDefinition {
    #[allow(
        clippy::too_many_arguments,
        reason = "the source fact mirrors one complete Prompt definition candidate"
    )]
    pub(crate) fn new(
        id: PromptId,
        key: impl Into<Box<str>>,
        name: impl Into<Box<str>>,
        description: Option<Box<str>>,
        role: PromptRole,
        content: Arc<str>,
        provenance: PromptSourceProvenance,
    ) -> Self {
        Self {
            id,
            key: key.into(),
            name: name.into(),
            description,
            role,
            content,
            provenance,
        }
    }
}

impl fmt::Debug for PromptSourceDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptSourceDefinition")
            .field("role", &self.role)
            .field("has_description", &self.description.is_some())
            .field("content_bytes", &self.content.len())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct WorkspacePromptSource {
    root_key: WorkspaceRootKey,
    relative_location: WorkspaceRelativePath,
    content: Arc<str>,
}

#[allow(
    dead_code,
    reason = "Workspace source adapters construct captured candidate facts"
)]
impl WorkspacePromptSource {
    pub(crate) fn new(
        root_key: WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
        content: Arc<str>,
    ) -> Self {
        Self {
            root_key,
            relative_location,
            content,
        }
    }
}

impl fmt::Debug for WorkspacePromptSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspacePromptSource")
            .field("content_bytes", &self.content.len())
            .finish()
    }
}

pub(crate) type PromptSourceFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<PromptSourceDefinition>, PromptSourceError>> + Send + 'a>,
>;

pub(crate) trait PromptSourceAdapter: Send + Sync {
    fn discover(&self) -> PromptSourceFuture<'_>;
}

pub(crate) type WorkspacePromptSourceFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<WorkspacePromptSource>, PromptSourceError>> + Send + 'a>,
>;

pub(crate) trait WorkspacePromptSourceAdapter: Send + Sync {
    fn capture<'a>(
        &'a self,
        context: &'a WorkspacePromptCaptureContext,
    ) -> WorkspacePromptSourceFuture<'a>;
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PromptKey(Box<str>);

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PromptSourceId(Box<str>);

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum PromptProvenance {
    Runtime(PromptSourceId),
    User(PromptSourceId),
}

#[derive(Clone)]
pub(crate) struct PromptContent {
    text: Arc<str>,
}

impl PromptContent {
    fn materialize(text: Arc<str>) -> Result<Self, LexicalError> {
        let text = if text.contains('\r') {
            Arc::<str>::from(normalize_newlines(&text))
        } else {
            text
        };
        validate_safe_text(&text, usize::MAX, false)?;
        Ok(Self { text })
    }

    #[allow(
        dead_code,
        reason = "M6.2 Prompt assembly reads materialized section content through this getter"
    )]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    fn into_arc(self) -> Arc<str> {
        self.text
    }
}

impl fmt::Debug for PromptContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptContent")
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct PromptDefinition {
    id: PromptId,
    key: PromptKey,
    name: Arc<str>,
    description: Option<Arc<str>>,
    role: PromptRole,
    content: PromptContent,
    provenance: PromptProvenance,
}

impl PromptDefinition {
    fn materialize(source: PromptSourceDefinition) -> Result<Self, PromptError> {
        let PromptSourceDefinition {
            id,
            key,
            name,
            description,
            role,
            content,
            provenance,
        } = source;
        validate_stable_symbolic_key(&key, 128, false)
            .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?;
        let name = normalize_prompt_metadata(
            name,
            usize::from(ProtocolLimits::v1_0().text.max_display_name_bytes),
            false,
        )?;
        let description = description
            .map(|description| {
                normalize_prompt_metadata(
                    description,
                    usize::try_from(ProtocolLimits::v1_0().text.max_description_bytes)
                        .unwrap_or(usize::MAX),
                    true,
                )
            })
            .transpose()?;
        let content = PromptContent::materialize(content)
            .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?;
        let provenance = match provenance {
            PromptSourceProvenance::Runtime(source_id) => {
                PromptProvenance::Runtime(prompt_source_id(source_id)?)
            }
            PromptSourceProvenance::User(source_id) => {
                PromptProvenance::User(prompt_source_id(source_id)?)
            }
        };
        Ok(Self {
            id,
            key: PromptKey(key),
            name,
            description,
            role,
            content,
            provenance,
        })
    }

    pub(crate) fn id(&self) -> &PromptId {
        &self.id
    }
}

impl fmt::Debug for PromptDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptDefinition")
            .field("role", &self.role)
            .field("name_bytes", &self.name.len())
            .field("has_description", &self.description.is_some())
            .field("content_bytes", &self.content.text.len())
            .finish()
    }
}

struct PromptServiceOwner;

pub(crate) struct PromptResourceView {
    owner: Arc<PromptServiceOwner>,
    definitions: BTreeMap<PromptId, Arc<PromptDefinition>>,
}

impl PromptResourceView {
    fn materialize(
        owner: Arc<PromptServiceOwner>,
        sources: Vec<PromptSourceDefinition>,
    ) -> Result<Arc<Self>, PromptError> {
        let mut definitions = BTreeMap::new();
        for source in sources {
            let definition = Arc::new(PromptDefinition::materialize(source)?);
            if definitions.contains_key(definition.id()) {
                return Err(PromptError::new(PromptErrorKind::DuplicateKey));
            }
            definitions.insert(definition.id().clone(), definition);
        }
        Ok(Arc::new(Self { owner, definitions }))
    }

    #[allow(
        dead_code,
        reason = "Turn capture and Prompt tests inspect the immutable candidate size"
    )]
    pub(crate) fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    fn definition(&self, id: &PromptId) -> Option<&Arc<PromptDefinition>> {
        self.definitions.get(id)
    }
}

impl fmt::Debug for PromptResourceView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptResourceView")
            .field("definition_count", &self.definition_count())
            .finish()
    }
}

#[allow(
    dead_code,
    reason = "the PromptService foundation is consumed by the pending Runtime shared roots"
)]
pub(crate) struct PromptService {
    owner: Arc<PromptServiceOwner>,
    required_policy: PromptContent,
    base_prompt: Option<PromptContent>,
    shared_sources: Arc<[Arc<dyn PromptSourceAdapter>]>,
    workspace_sources: Arc<[Arc<dyn WorkspacePromptSourceAdapter>]>,
}

#[allow(
    dead_code,
    reason = "the PromptService foundation is consumed by the pending Runtime shared roots"
)]
impl PromptService {
    pub(crate) fn new(
        required_policy: Arc<str>,
        base_prompt: Option<Arc<str>>,
        shared_sources: Vec<Arc<dyn PromptSourceAdapter>>,
        workspace_sources: Vec<Arc<dyn WorkspacePromptSourceAdapter>>,
    ) -> Result<Self, PromptError> {
        let required_policy = PromptContent::materialize(required_policy).map_err(|error| {
            PromptError::new(if error == LexicalError::Empty {
                PromptErrorKind::RequiredPromptMissing
            } else {
                PromptErrorKind::ContentLoad
            })
        })?;
        let base_prompt = base_prompt
            .map(PromptContent::materialize)
            .transpose()
            .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?;
        Ok(Self {
            owner: Arc::new(PromptServiceOwner),
            required_policy,
            base_prompt,
            shared_sources: shared_sources.into(),
            workspace_sources: workspace_sources.into(),
        })
    }

    pub(crate) async fn initialize(&self) -> Result<Arc<PromptResourceView>, PromptError> {
        self.build_candidate().await
    }

    pub(crate) async fn build_reload_candidate(
        &self,
    ) -> Result<Arc<PromptResourceView>, PromptError> {
        self.build_candidate().await
    }

    async fn build_candidate(&self) -> Result<Arc<PromptResourceView>, PromptError> {
        let mut sources = Vec::new();
        for adapter in &*self.shared_sources {
            sources.extend(
                adapter
                    .discover()
                    .await
                    .map_err(|_| PromptError::new(PromptErrorKind::SourceDiscovery))?,
            );
        }
        PromptResourceView::materialize(Arc::clone(&self.owner), sources)
    }

    pub(crate) async fn capture_workspace_sources(
        &self,
        context: WorkspacePromptCaptureContext,
    ) -> Result<Arc<[CapturedWorkspacePromptSource]>, PromptError> {
        if context.roots().is_empty() {
            return Ok(Arc::from([]));
        }
        if self.workspace_sources.is_empty() {
            return Err(PromptError::new(PromptErrorKind::SourceDiscovery));
        }

        let mut sources = Vec::new();
        for adapter in &*self.workspace_sources {
            sources.extend(
                adapter
                    .capture(&context)
                    .await
                    .map_err(|_| PromptError::new(PromptErrorKind::SourceDiscovery))?,
            );
        }
        sources.sort_by(|left, right| {
            left.relative_location
                .cmp(&right.relative_location)
                .then_with(|| left.root_key.cmp(&right.root_key))
        });
        if sources.windows(2).any(|pair| {
            pair[0].root_key == pair[1].root_key
                && pair[0].relative_location == pair[1].relative_location
        }) {
            return Err(PromptError::new(PromptErrorKind::DuplicateKey));
        }

        let mut captured = Vec::with_capacity(sources.len());
        for source in sources {
            let content = PromptContent::materialize(source.content)
                .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?;
            captured.push(
                context
                    .capture(
                        &source.root_key,
                        source.relative_location,
                        content.into_arc(),
                    )
                    .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?,
            );
        }
        Ok(captured.into())
    }

    /// Verifies one exact Agent+Session Prompt selection against the given resource view for the
    /// `for_turn` selection stage only: `Ok(true)` means every selected Prompt resolves with the
    /// exact expected role/provenance and no duplicate resolved key.  The three ordinary
    /// selection failures (missing Prompt, wrong role, duplicate resolved key) degrade to
    /// `Ok(false)`; an owner mismatch or any other Prompt failure is an internal invariant that
    /// the caller must surface through its existing fatal/internal path, never as a fabricated
    /// `PromptUnavailable`.  It deliberately reuses `resolve_selected_definitions` and exposes no
    /// definitions.
    pub(crate) fn selection_available(
        &self,
        resources: &PromptResourceView,
        agent_prompts: &AgentPromptSelection,
        session_prompts: &SessionPromptSelection,
    ) -> Result<bool, PromptError> {
        if !Arc::ptr_eq(&self.owner, &resources.owner) {
            return Err(PromptError::new(PromptErrorKind::Internal));
        }
        match resolve_selected_definitions(
            resources,
            agent_prompts.enabled(),
            PromptRole::System,
            true,
        ) {
            Ok(_) => {}
            Err(error) => return selection_availability(error),
        }
        match resolve_selected_definitions(
            resources,
            session_prompts.enabled(),
            PromptRole::User,
            false,
        ) {
            Ok(_) => Ok(true),
            Err(error) => selection_availability(error),
        }
    }

    pub(crate) fn for_turn(
        &self,
        context: PromptTurnContext,
    ) -> Result<Arc<PromptSet>, PromptError> {
        if !Arc::ptr_eq(&self.owner, &context.resources.owner) {
            return Err(PromptError::new(PromptErrorKind::PromptUnavailable));
        }
        if context.workspace.session_id() != context.session_id {
            return Err(PromptError::new(PromptErrorKind::InvalidContribution));
        }

        let agent_definitions = resolve_selected_definitions(
            &context.resources,
            context.agent_prompts.enabled(),
            PromptRole::System,
            true,
        )?;
        let session_definitions = resolve_selected_definitions(
            &context.resources,
            context.session_prompts.enabled(),
            PromptRole::User,
            false,
        )?;

        let mut system = Vec::with_capacity(
            1 + usize::from(self.base_prompt.is_some()) + agent_definitions.len(),
        );
        system.push(PromptSection::runtime_required(
            self.required_policy.clone(),
        ));
        if let Some(base_prompt) = &self.base_prompt {
            system.push(PromptSection::runtime_base(base_prompt.clone()));
        }
        system.extend(agent_definitions.iter().cloned().map(PromptSection::agent));

        let mut user_context =
            Vec::with_capacity(session_definitions.len() + context.workspace.sources().len());
        user_context.extend(
            session_definitions
                .iter()
                .cloned()
                .map(PromptSection::session),
        );
        let mut workspace_sources = context.workspace.sources().to_vec();
        workspace_sources.sort_by(|left, right| {
            left.relative_location()
                .cmp(right.relative_location())
                .then_with(|| {
                    left.authorization()
                        .root_key()
                        .cmp(right.authorization().root_key())
                })
        });
        for source in workspace_sources {
            let content = PromptContent::materialize(Arc::clone(source.content_arc()))
                .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?;
            user_context.push(PromptSection::workspace(source, content));
        }

        Ok(Arc::new(PromptSet {
            agent: context.agent,
            session_id: context.session_id,
            session_revision: context.session_revision,
            resources: context.resources,
            profile: PromptProfile {
                system: system.into(),
                user_context: user_context.into(),
            },
            tools: context.tools,
            skills: context.skills,
            model: context.model,
        }))
    }
}

impl fmt::Debug for PromptService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptService")
            .field("has_required_policy", &true)
            .field("has_base_prompt", &self.base_prompt.is_some())
            .field("shared_source_count", &self.shared_sources.len())
            .field("workspace_source_count", &self.workspace_sources.len())
            .finish()
    }
}

#[allow(
    dead_code,
    reason = "the private TurnExecutionContext constructor supplies this captured input"
)]
pub(crate) struct PromptTurnContext {
    agent: AgentRevisionRef,
    session_id: SessionId,
    session_revision: SessionDefinitionRevision,
    resources: Arc<PromptResourceView>,
    agent_prompts: AgentPromptSelection,
    session_prompts: SessionPromptSelection,
    workspace: WorkspacePromptContext,
    tools: ToolPromptView,
    skills: SkillPromptView,
    model: Arc<TurnModelSnapshot>,
}

#[allow(
    dead_code,
    reason = "the private TurnExecutionContext constructor supplies this captured input"
)]
impl PromptTurnContext {
    #[allow(
        clippy::too_many_arguments,
        reason = "one Turn capture atomically binds the exact Prompt inputs"
    )]
    pub(crate) fn new(
        agent: AgentRevisionRef,
        session_id: SessionId,
        session_revision: SessionDefinitionRevision,
        resources: Arc<PromptResourceView>,
        agent_prompts: AgentPromptSelection,
        session_prompts: SessionPromptSelection,
        workspace: WorkspacePromptContext,
        tools: ToolPromptView,
        skills: SkillPromptView,
        model: Arc<TurnModelSnapshot>,
    ) -> Self {
        Self {
            agent,
            session_id,
            session_revision,
            resources,
            agent_prompts,
            session_prompts,
            workspace,
            tools,
            skills,
            model,
        }
    }
}

impl fmt::Debug for PromptTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptTurnContext")
            .field("resource_count", &self.resources.definition_count())
            .field("agent_prompt_count", &self.agent_prompts.enabled().len())
            .field(
                "session_prompt_count",
                &self.session_prompts.enabled().len(),
            )
            .field("workspace_source_count", &self.workspace.sources().len())
            .field("tools_empty", &self.tools.is_empty())
            .field("skills_empty", &self.skills.is_empty())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptSectionKind {
    RuntimeRequired,
    RuntimeBase,
    Agent,
    Session,
    Workspace,
}

#[derive(Clone)]
enum PromptSectionSource {
    RuntimeRequired,
    RuntimeBase,
    Agent(Arc<PromptDefinition>),
    Session(Arc<PromptDefinition>),
    Workspace(CapturedWorkspacePromptSource),
}

impl PromptSectionSource {
    const fn kind(&self) -> PromptSectionKind {
        match self {
            Self::RuntimeRequired => PromptSectionKind::RuntimeRequired,
            Self::RuntimeBase => PromptSectionKind::RuntimeBase,
            Self::Agent(_) => PromptSectionKind::Agent,
            Self::Session(_) => PromptSectionKind::Session,
            Self::Workspace(_) => PromptSectionKind::Workspace,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PromptSection {
    source: PromptSectionSource,
    content: PromptContent,
}

impl PromptSection {
    fn runtime_required(content: PromptContent) -> Self {
        Self {
            source: PromptSectionSource::RuntimeRequired,
            content,
        }
    }

    fn runtime_base(content: PromptContent) -> Self {
        Self {
            source: PromptSectionSource::RuntimeBase,
            content,
        }
    }

    fn agent(definition: Arc<PromptDefinition>) -> Self {
        Self {
            content: definition.content.clone(),
            source: PromptSectionSource::Agent(definition),
        }
    }

    fn session(definition: Arc<PromptDefinition>) -> Self {
        Self {
            content: definition.content.clone(),
            source: PromptSectionSource::Session(definition),
        }
    }

    fn workspace(source: CapturedWorkspacePromptSource, content: PromptContent) -> Self {
        Self {
            source: PromptSectionSource::Workspace(source),
            content,
        }
    }

    pub(crate) const fn kind(&self) -> PromptSectionKind {
        self.source.kind()
    }

    #[allow(
        dead_code,
        reason = "M6.2 assembly reads the already-fixed section role"
    )]
    pub(crate) const fn role(&self) -> PromptRole {
        match self.kind() {
            PromptSectionKind::RuntimeRequired
            | PromptSectionKind::RuntimeBase
            | PromptSectionKind::Agent => PromptRole::System,
            PromptSectionKind::Session | PromptSectionKind::Workspace => PromptRole::User,
        }
    }

    #[allow(
        dead_code,
        reason = "M6.2 assembly reads materialized section text through this getter"
    )]
    pub(crate) fn text(&self) -> &str {
        self.content.text()
    }
}

impl fmt::Debug for PromptSection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source_retained = match &self.source {
            PromptSectionSource::RuntimeRequired | PromptSectionSource::RuntimeBase => false,
            PromptSectionSource::Agent(definition) | PromptSectionSource::Session(definition) => {
                Arc::strong_count(definition) > 0
            }
            PromptSectionSource::Workspace(source) => !source.content().is_empty(),
        };
        formatter
            .debug_struct("PromptSection")
            .field("kind", &self.kind())
            .field("content_bytes", &self.content.text.len())
            .field("source_retained", &source_retained)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct PromptProfile {
    system: Arc<[PromptSection]>,
    user_context: Arc<[PromptSection]>,
}

#[allow(
    dead_code,
    reason = "M6.2 final model assembly consumes the fixed Prompt profile"
)]
impl PromptProfile {
    pub(crate) fn system(&self) -> &[PromptSection] {
        &self.system
    }

    pub(crate) fn user_context(&self) -> &[PromptSection] {
        &self.user_context
    }
}

impl fmt::Debug for PromptProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptProfile")
            .field("system_sections", &self.system.len())
            .field("user_sections", &self.user_context.len())
            .finish()
    }
}

pub(crate) struct PromptAssemblyInput<'a> {
    kind: PromptAssemblyInputKind<'a>,
}

enum PromptAssemblyInputKind<'a> {
    AgentRun {
        conversation: &'a LiveConversationView,
        output_contract: Option<&'a OutputContract>,
    },
    CompactionSummary {
        source: &'a CompactionSummarySourceView,
        directive: &'a CompactionSummaryDirective,
        budget: &'a CompactionSummaryBudget,
    },
}

#[allow(
    dead_code,
    reason = "the immediately adjacent M7 ActiveTurnTask constructs AgentRun assembly input"
)]
impl<'a> PromptAssemblyInput<'a> {
    pub(crate) const fn agent_run(
        conversation: &'a LiveConversationView,
        output_contract: Option<&'a OutputContract>,
    ) -> Self {
        Self {
            kind: PromptAssemblyInputKind::AgentRun {
                conversation,
                output_contract,
            },
        }
    }

    pub(crate) const fn compaction_summary(
        source: &'a CompactionSummarySourceView,
        directive: &'a CompactionSummaryDirective,
        budget: &'a CompactionSummaryBudget,
    ) -> Self {
        Self {
            kind: PromptAssemblyInputKind::CompactionSummary {
                source,
                directive,
                budget,
            },
        }
    }
}

pub(crate) struct AssembledModelContext {
    system: Arc<[PromptSection]>,
    messages: Arc<[ModelMessage]>,
    tools: ToolPromptView,
    output_contract: Option<OutputContract>,
    assembly_proof: PromptAssemblyProof,
}

impl AssembledModelContext {
    #[allow(
        dead_code,
        reason = "read by the adjacent M14 OpenAI Responses adapter encoder"
    )]
    pub(crate) fn system(&self) -> &[PromptSection] {
        &self.system
    }

    #[allow(
        dead_code,
        reason = "read by the adjacent M14 OpenAI Responses adapter encoder"
    )]
    pub(crate) fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    pub(crate) const fn output_contract(&self) -> Option<&OutputContract> {
        self.output_contract.as_ref()
    }

    pub(crate) fn tools_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub(crate) fn tools(&self) -> &[crate::tools::ToolSpec] {
        self.tools.specs()
    }

    pub(crate) const fn assembly_proof(&self) -> &PromptAssemblyProof {
        &self.assembly_proof
    }
}

impl fmt::Debug for AssembledModelContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssembledModelContext")
            .field("system_sections", &self.system.len())
            .field("messages", &self.messages.len())
            .field("tools", &self.tools.specs().len())
            .field("has_output_contract", &self.output_contract.is_some())
            .finish()
    }
}

pub(crate) struct PromptAssemblyProof {
    purpose: ModelCallPurpose,
    turn_model: TurnModelRef,
    source_revision: ConversationRevision,
    output_contract: Option<OutputContract>,
    compaction_summary_budget: Option<CompactionSummaryBudgetProof>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompactionSummaryBudgetProof {
    max_output_tokens: NonZeroU32,
    budget: CompactionSummaryBudget,
}

impl CompactionSummaryBudgetProof {
    pub(crate) const fn max_output_tokens(&self) -> NonZeroU32 {
        self.max_output_tokens
    }

    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "read by adjacent M10 plan/request arbitration and proof diagnostics"
        )
    )]
    pub(crate) const fn budget(&self) -> &CompactionSummaryBudget {
        &self.budget
    }
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "validated by the adjacent M7 ModelCallRequest constructor"
    )
)]
impl PromptAssemblyProof {
    pub(crate) const fn purpose(&self) -> ModelCallPurpose {
        self.purpose
    }

    pub(crate) const fn turn_model(&self) -> &TurnModelRef {
        &self.turn_model
    }

    pub(crate) const fn source_revision(&self) -> ConversationRevision {
        self.source_revision
    }

    pub(crate) const fn output_contract(&self) -> Option<&OutputContract> {
        self.output_contract.as_ref()
    }

    pub(crate) const fn compaction_summary_budget(&self) -> Option<&CompactionSummaryBudgetProof> {
        self.compaction_summary_budget.as_ref()
    }
}

impl fmt::Debug for PromptAssemblyProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptAssemblyProof")
            .field("purpose", &self.purpose)
            .field("turn_model", &self.turn_model)
            .field("source_revision", &self.source_revision)
            .field("has_output_contract", &self.output_contract.is_some())
            .field(
                "has_compaction_summary_budget",
                &self.compaction_summary_budget.is_some(),
            )
            .finish()
    }
}

#[allow(
    dead_code,
    reason = "the PromptSet profile is consumed by M6.2 final model-context assembly"
)]
pub(crate) struct PromptSet {
    agent: AgentRevisionRef,
    session_id: SessionId,
    session_revision: SessionDefinitionRevision,
    resources: Arc<PromptResourceView>,
    profile: PromptProfile,
    tools: ToolPromptView,
    skills: SkillPromptView,
    model: Arc<TurnModelSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AgentRunCompactionAssemblyBasis {
    fixed_input_tokens: u64,
    rolling_summary_message_overhead_tokens: u64,
    estimator: TokenEstimator,
}

impl AgentRunCompactionAssemblyBasis {
    pub(crate) const fn fixed_input_tokens(self) -> u64 {
        self.fixed_input_tokens
    }

    pub(crate) const fn rolling_summary_message_overhead_tokens(self) -> u64 {
        self.rolling_summary_message_overhead_tokens
    }

    pub(crate) const fn estimator(self) -> TokenEstimator {
        self.estimator
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        fixed_input_tokens: u64,
        rolling_summary_message_overhead_tokens: u64,
        estimator: TokenEstimator,
    ) -> Self {
        Self {
            fixed_input_tokens,
            rolling_summary_message_overhead_tokens,
            estimator,
        }
    }
}

#[derive(Clone)]
#[allow(
    dead_code,
    reason = "structural fields are consumed by the adjacent M10 summary assembly proof slice"
)]
pub(crate) struct CompactionSummaryAssemblyBasis {
    fixed_prompt_tokens: u64,
    system_sections: Arc<[PromptSection]>,
    output_contract: OutputContract,
    estimator: TokenEstimator,
}

#[allow(
    dead_code,
    reason = "structural getters are consumed by the adjacent M10 summary assembly proof slice"
)]
impl CompactionSummaryAssemblyBasis {
    pub(crate) const fn fixed_prompt_tokens(&self) -> u64 {
        self.fixed_prompt_tokens
    }

    pub(crate) fn system_sections(&self) -> &[PromptSection] {
        &self.system_sections
    }

    pub(crate) const fn output_contract(&self) -> &OutputContract {
        &self.output_contract
    }

    pub(crate) const fn estimator(&self) -> TokenEstimator {
        self.estimator
    }

    #[cfg(test)]
    pub(crate) fn for_test(fixed_prompt_tokens: u64, estimator: TokenEstimator) -> Self {
        Self {
            fixed_prompt_tokens,
            system_sections: Arc::from([]),
            output_contract: OutputContract::NoToolCalls,
            estimator,
        }
    }
}

#[allow(
    dead_code,
    reason = "the PromptSet profile is consumed by M6.2 final model-context assembly"
)]
impl PromptSet {
    pub(crate) const fn agent(&self) -> AgentRevisionRef {
        self.agent
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn session_revision(&self) -> SessionDefinitionRevision {
        self.session_revision
    }

    pub(crate) const fn profile(&self) -> &PromptProfile {
        &self.profile
    }

    pub(crate) fn compose_user_message(
        &self,
        intent: PromptIntent,
    ) -> Result<CanonicalUserMessage, PromptError> {
        let PromptIntent { body, skills } = intent;
        if !skills.is_empty() {
            return Err(PromptError::new(PromptErrorKind::PromptUnavailable));
        }
        let PromptBodyIntent::Text(text) = body else {
            return Err(PromptError::new(PromptErrorKind::InvalidIntent));
        };
        let content = MessageContent::text(text.text())
            .map_err(|_| PromptError::new(PromptErrorKind::InvalidIntent))?;
        let message = MessageRecord::new(vec![content])
            .map_err(|_| PromptError::new(PromptErrorKind::InvalidIntent))?;
        CanonicalUserMessage::new(message, Vec::new())
            .map_err(|_| PromptError::new(PromptErrorKind::InvalidIntent))
    }

    pub(crate) fn agent_run_compaction_assembly_basis(
        &self,
    ) -> Result<AgentRunCompactionAssemblyBasis, PromptError> {
        let mut messages = Vec::with_capacity(self.profile.user_context.len());
        for section in &*self.profile.user_context {
            messages.push(
                ModelMessage::unstamped_user_text(Arc::from(section.text()))
                    .map_err(|_| PromptError::new(PromptErrorKind::InvalidContribution))?,
            );
        }
        let bytes = canonical_model_context_bytes(
            &self.profile.system,
            &messages,
            self.tools.specs(),
            None,
        )
        .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        let estimator = self.model.token_estimator();
        let fixed_input_tokens = estimator
            .checked_estimate_utf8_bytes(
                u64::try_from(bytes)
                    .map_err(|_| PromptError::new(PromptErrorKind::ContextLimitExceeded))?,
            )
            .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        let rolling_summary_message_overhead_tokens = estimator
            .checked_estimate_utf8_bytes(rolling_summary_message_envelope_bytes())
            .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        Ok(AgentRunCompactionAssemblyBasis {
            fixed_input_tokens,
            rolling_summary_message_overhead_tokens,
            estimator,
        })
    }

    pub(crate) fn compaction_summary_assembly_basis(
        &self,
    ) -> Result<CompactionSummaryAssemblyBasis, PromptError> {
        let system_sections: Arc<[PromptSection]> = self
            .profile
            .system
            .iter()
            .filter(|section| section.kind() == PromptSectionKind::RuntimeRequired)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        if system_sections.len() != 1 {
            return Err(PromptError::new(PromptErrorKind::RequiredPromptMissing));
        }
        let output_contract = OutputContract::NoToolCalls;
        let bytes =
            canonical_model_context_bytes(&system_sections, &[], &[], Some(&output_contract))
                .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        let estimator = self.model.token_estimator();
        let fixed_prompt_tokens = estimator
            .checked_estimate_utf8_bytes(
                u64::try_from(bytes)
                    .map_err(|_| PromptError::new(PromptErrorKind::ContextLimitExceeded))?,
            )
            .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        Ok(CompactionSummaryAssemblyBasis {
            fixed_prompt_tokens,
            system_sections,
            output_contract,
            estimator,
        })
    }

    pub(crate) fn assemble(
        &self,
        input: PromptAssemblyInput<'_>,
    ) -> Result<AssembledModelContext, PromptError> {
        match input.kind {
            PromptAssemblyInputKind::AgentRun {
                conversation,
                output_contract,
            } => self.assemble_agent_run(conversation, output_contract),
            PromptAssemblyInputKind::CompactionSummary {
                source,
                directive,
                budget,
            } => self.assemble_compaction_summary(source, directive, budget),
        }
    }

    fn assemble_agent_run(
        &self,
        conversation: &LiveConversationView,
        output_contract: Option<&OutputContract>,
    ) -> Result<AssembledModelContext, PromptError> {
        if !self.skills.is_empty() {
            return Err(PromptError::new(PromptErrorKind::PromptUnavailable));
        }

        let mut messages =
            Vec::with_capacity(self.profile.user_context.len() + conversation.messages().len());
        for section in &*self.profile.user_context {
            messages.push(
                ModelMessage::unstamped_user_text(Arc::from(section.text()))
                    .map_err(|_| PromptError::new(PromptErrorKind::InvalidContribution))?,
            );
        }
        messages.extend(conversation.messages().iter().cloned());
        let output_contract = output_contract.cloned();

        let canonical_bytes = canonical_model_context_bytes(
            &self.profile.system,
            &messages,
            self.tools.specs(),
            output_contract.as_ref(),
        )
        .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        let estimated_tokens = self
            .model
            .token_estimator()
            .estimate_utf8_bytes(canonical_bytes);
        if self
            .model
            .limits()
            .context_window_tokens()
            .is_some_and(|limit| estimated_tokens > u64::from(limit.get()))
        {
            return Err(PromptError::new(PromptErrorKind::ContextLimitExceeded));
        }

        Ok(AssembledModelContext {
            system: Arc::clone(&self.profile.system),
            messages: messages.into(),
            tools: self.tools.clone(),
            output_contract: output_contract.clone(),
            assembly_proof: PromptAssemblyProof {
                purpose: ModelCallPurpose::AgentRun,
                turn_model: self.model.turn_model_ref(),
                source_revision: conversation.revision(),
                output_contract,
                compaction_summary_budget: None,
            },
        })
    }

    fn assemble_compaction_summary(
        &self,
        source: &CompactionSummarySourceView,
        directive: &CompactionSummaryDirective,
        budget: &CompactionSummaryBudget,
    ) -> Result<AssembledModelContext, PromptError> {
        let basis = self.compaction_summary_assembly_basis()?;
        let estimator = self.model.token_estimator();
        let directive_tokens = directive
            .message()
            .compaction_estimated_tokens(estimator)
            .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        let source_tokens = source.messages().iter().try_fold(0_u64, |total, message| {
            total.checked_add(message.compaction_estimated_tokens(estimator)?)
        });
        let Some(source_tokens) = source_tokens else {
            return Err(PromptError::new(PromptErrorKind::ContextLimitExceeded));
        };
        if basis.fixed_prompt_tokens() != budget.fixed_prompt_tokens()
            || directive_tokens != budget.directive_tokens()
            || source_tokens != budget.reduced_source_tokens()
        {
            return Err(PromptError::new(PromptErrorKind::InvalidContribution));
        }
        let required_tokens = budget
            .fixed_prompt_tokens()
            .checked_add(budget.reduced_source_tokens())
            .and_then(|total| total.checked_add(budget.directive_tokens()))
            .and_then(|total| total.checked_add(u64::from(budget.safety_reserve_tokens().get())))
            .and_then(|total| total.checked_add(u64::from(budget.max_output_tokens().get())))
            .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        if self
            .model
            .limits()
            .context_window_tokens()
            .is_none_or(|limit| required_tokens > u64::from(limit.get()))
        {
            return Err(PromptError::new(PromptErrorKind::ContextLimitExceeded));
        }

        let mut messages = Vec::with_capacity(source.messages().len() + 1);
        messages.extend(source.messages().iter().cloned());
        messages.push(directive.message().clone());
        let output_contract = Some(basis.output_contract().clone());
        let canonical_bytes = canonical_model_context_bytes(
            basis.system_sections(),
            &messages,
            &[],
            output_contract.as_ref(),
        )
        .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        let estimated_tokens = estimator
            .checked_estimate_utf8_bytes(
                u64::try_from(canonical_bytes)
                    .map_err(|_| PromptError::new(PromptErrorKind::ContextLimitExceeded))?,
            )
            .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        let estimated_budget_input = budget
            .fixed_prompt_tokens()
            .checked_add(budget.reduced_source_tokens())
            .and_then(|total| total.checked_add(budget.directive_tokens()))
            .ok_or_else(|| PromptError::new(PromptErrorKind::ContextLimitExceeded))?;
        if estimated_tokens > estimated_budget_input {
            return Err(PromptError::new(PromptErrorKind::InvalidContribution));
        }

        Ok(AssembledModelContext {
            system: basis.system_sections.clone(),
            messages: messages.into(),
            tools: ToolSet::empty().prompt_view(),
            output_contract: output_contract.clone(),
            assembly_proof: PromptAssemblyProof {
                purpose: ModelCallPurpose::CompactionSummary,
                turn_model: self.model.turn_model_ref(),
                source_revision: source.source_revision(),
                output_contract,
                compaction_summary_budget: Some(CompactionSummaryBudgetProof {
                    max_output_tokens: budget.max_output_tokens(),
                    budget: budget.clone(),
                }),
            },
        })
    }
}

fn canonical_model_context_bytes(
    system: &[PromptSection],
    messages: &[ModelMessage],
    tools: &[crate::tools::ToolSpec],
    output_contract: Option<&OutputContract>,
) -> Option<usize> {
    let mut bytes = r#"{"system":["#.len();
    for (index, section) in system.iter().enumerate() {
        add_separator(&mut bytes, index)?;
        add_literal(&mut bytes, r#"{"role":"system","content":"#)?;
        add_json_string(&mut bytes, section.text())?;
        add_literal(&mut bytes, "}")?;
    }
    add_literal(&mut bytes, r#"],"messages":["#)?;
    for (index, message) in messages.iter().enumerate() {
        add_separator(&mut bytes, index)?;
        add_model_message(&mut bytes, message)?;
    }
    add_literal(&mut bytes, r#"],"tools":["#)?;
    for (index, definition) in tools.iter().enumerate() {
        add_separator(&mut bytes, index)?;
        add_literal(&mut bytes, r#"{"name":"#)?;
        add_json_string(&mut bytes, definition.name().as_str())?;
        add_literal(&mut bytes, r#","description":"#)?;
        add_json_string(&mut bytes, definition.description())?;
        add_literal(&mut bytes, r#","inputSchema":"#)?;
        add_literal(&mut bytes, definition.input_schema().canonical_bytes())?;
        add_literal(&mut bytes, "}")?;
    }
    add_literal(&mut bytes, r#"],"outputContract":"#)?;
    match output_contract {
        Some(OutputContract::NoToolCalls) => add_json_string(&mut bytes, "no_tool_calls")?,
        Some(OutputContract::Structured(contract)) => {
            // Count the structured contract fact exactly: the type literal, the nullable name
            // (escaped as a JSON string when present), and the canonical schema bytes.
            add_literal(&mut bytes, r#"{"type":"structured","name":"#)?;
            match contract.name() {
                Some(name) => add_json_string(&mut bytes, name)?,
                None => add_literal(&mut bytes, "null")?,
            }
            add_literal(&mut bytes, r#","schema":"#)?;
            add_literal(&mut bytes, contract.schema().canonical_bytes())?;
            add_literal(&mut bytes, "}")?;
        }
        None => add_literal(&mut bytes, "null")?,
    }
    add_literal(&mut bytes, "}")?;
    Some(bytes)
}

fn rolling_summary_message_envelope_bytes() -> u64 {
    u64::try_from(r#",{"role":"user","content":[""]}"#.len())
        .expect("rolling summary envelope length fits u64")
}

fn add_model_message(bytes: &mut usize, message: &ModelMessage) -> Option<()> {
    match message.as_ref() {
        ModelMessageRef::User { content } => {
            add_literal(bytes, r#"{"role":"user","content":["#)?;
            for (index, part) in content.iter().enumerate() {
                add_separator(bytes, index)?;
                add_json_string(bytes, part.as_text())?;
            }
            add_literal(bytes, "]}")
        }
        ModelMessageRef::Assistant { content } => {
            add_literal(bytes, r#"{"role":"assistant","content":["#)?;
            for (index, block) in content.iter().enumerate() {
                add_separator(bytes, index)?;
                add_assistant_content(bytes, block)?;
            }
            add_literal(bytes, "]}")
        }
        ModelMessageRef::Tool {
            tool_call_id,
            content,
        } => {
            add_literal(bytes, r#"{"role":"tool","toolCallId":"#)?;
            add_json_string(bytes, tool_call_id.as_str())?;
            add_literal(bytes, r#","content":["#)?;
            for (index, part) in content.parts().iter().enumerate() {
                add_separator(bytes, index)?;
                add_json_string(bytes, part.as_text())?;
            }
            add_literal(bytes, "]}")
        }
    }
}

fn add_assistant_content(bytes: &mut usize, content: &ModelAssistantContent) -> Option<()> {
    match content.as_ref() {
        ModelAssistantContentRef::Reasoning(reasoning) => {
            add_literal(bytes, r#"{"type":"reasoning""#)?;
            add_optional_json_field(bytes, "text", reasoning.text())?;
            add_optional_json_field(bytes, "summary", reasoning.summary())?;
            add_optional_json_field(bytes, "encrypted", reasoning.encrypted())?;
            add_optional_json_field(bytes, "signature", reasoning.signature())?;
            add_optional_json_field(
                bytes,
                "providerItemId",
                reasoning.provider_item_id().map(|id| id.as_str()),
            )?;
            add_literal(bytes, "}")
        }
        ModelAssistantContentRef::Text(text) => {
            add_literal(bytes, r#"{"type":"text","text":"#)?;
            add_json_string(bytes, text)?;
            add_literal(bytes, "}")
        }
        ModelAssistantContentRef::ToolCall {
            tool_call_id,
            name,
            arguments,
        } => {
            add_literal(bytes, r#"{"type":"tool_call","toolCallId":"#)?;
            add_json_string(bytes, tool_call_id.as_str())?;
            add_literal(bytes, r#","name":"#)?;
            add_json_string(bytes, name.as_str())?;
            add_literal(bytes, r#","arguments":"#)?;
            add_literal(bytes, arguments.canonical_bytes())?;
            add_literal(bytes, "}")
        }
    }
}

fn add_optional_json_field(bytes: &mut usize, name: &str, value: Option<&str>) -> Option<()> {
    let Some(value) = value else {
        return Some(());
    };
    add_literal(bytes, ",")?;
    add_json_string(bytes, name)?;
    add_literal(bytes, ":")?;
    add_json_string(bytes, value)
}

fn add_separator(bytes: &mut usize, index: usize) -> Option<()> {
    if index > 0 {
        add_literal(bytes, ",")?;
    }
    Some(())
}

fn add_json_string(bytes: &mut usize, value: &str) -> Option<()> {
    *bytes = bytes.checked_add(canonical_json_string_len(value)?)?;
    Some(())
}

fn add_literal(bytes: &mut usize, value: impl AsRef<[u8]>) -> Option<()> {
    *bytes = bytes.checked_add(value.as_ref().len())?;
    Some(())
}

impl fmt::Debug for PromptSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptSet")
            .field("resource_count", &self.resources.definition_count())
            .field("profile", &self.profile)
            .field("tools_empty", &self.tools.is_empty())
            .field("skills_empty", &self.skills.is_empty())
            .finish()
    }
}

fn normalize_prompt_metadata(
    value: Box<str>,
    maximum: usize,
    allow_empty: bool,
) -> Result<Arc<str>, PromptError> {
    let value = normalize_newlines(&value);
    validate_safe_text(&value, maximum, allow_empty)
        .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?;
    Ok(value.into())
}

fn prompt_source_id(value: Box<str>) -> Result<PromptSourceId, PromptError> {
    validate_stable_symbolic_key(&value, 128, false)
        .map_err(|_| PromptError::new(PromptErrorKind::ContentLoad))?;
    Ok(PromptSourceId(value))
}

fn resolve_selected_definitions(
    resources: &PromptResourceView,
    selection: &BTreeSet<PromptId>,
    expected_role: PromptRole,
    require_runtime_provenance: bool,
) -> Result<Vec<Arc<PromptDefinition>>, PromptError> {
    let mut definitions = selection
        .iter()
        .map(|id| {
            resources
                .definition(id)
                .cloned()
                .ok_or(PromptError::new(PromptErrorKind::PromptUnavailable))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if definitions.iter().any(|definition| {
        definition.role != expected_role
            || (require_runtime_provenance
                && !matches!(definition.provenance, PromptProvenance::Runtime(_)))
    }) {
        return Err(PromptError::new(PromptErrorKind::InvalidRole));
    }
    definitions.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.provenance.cmp(&right.provenance))
    });
    if definitions
        .windows(2)
        .any(|pair| pair[0].key == pair[1].key)
    {
        return Err(PromptError::new(PromptErrorKind::DuplicateKey));
    }
    Ok(definitions)
}

/// Classifies one selection-stage resolution failure for the selection-availability seam: the
/// three ordinary selection failures degrade to `Ok(false)`; every other kind (which the
/// selection stage cannot produce on an installed view) is an internal invariant.
fn selection_availability(error: PromptError) -> Result<bool, PromptError> {
    match error.kind() {
        PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::DuplicateKey => Ok(false),
        PromptErrorKind::SourceDiscovery
        | PromptErrorKind::ContentLoad
        | PromptErrorKind::RequiredPromptMissing
        | PromptErrorKind::InvalidIntent
        | PromptErrorKind::InvalidContribution
        | PromptErrorKind::ContextLimitExceeded
        | PromptErrorKind::Internal => Err(error),
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TextIntent(Box<str>);

impl TextIntent {
    pub fn new(text: impl AsRef<str>) -> Result<Self, PromptValueError> {
        let maximum = ProtocolLimits::v1_0().text.max_text_intent_bytes as usize;
        Self::new_with_maximum(text, maximum)
    }

    pub(crate) fn new_with_maximum(
        text: impl AsRef<str>,
        maximum: usize,
    ) -> Result<Self, PromptValueError> {
        let text = normalize_text_intent(text.as_ref(), maximum)?;
        Ok(Self(text.into()))
    }

    pub fn text(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillIntent {
    skill_id: SkillId,
}

impl SkillIntent {
    pub fn new(skill_id: SkillId) -> Self {
        Self { skill_id }
    }

    pub const fn skill_id(&self) -> &SkillId {
        &self.skill_id
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum PromptBodyIntent {
    Empty,
    Text(TextIntent),
}

#[derive(Clone, Eq, PartialEq)]
pub struct PromptIntent {
    body: PromptBodyIntent,
    skills: Arc<[SkillIntent]>,
}

impl PromptIntent {
    pub fn new(body: PromptBodyIntent, skills: Vec<SkillIntent>) -> Result<Self, PromptValueError> {
        let maximum = ProtocolLimits::v1_0().prompt.max_skills_per_intent as usize;
        Self::new_with_maximum_skills(body, skills, maximum)
    }

    pub(crate) fn new_with_maximum_skills(
        body: PromptBodyIntent,
        skills: Vec<SkillIntent>,
        maximum: usize,
    ) -> Result<Self, PromptValueError> {
        validate_skill_intent_count(skills.len(), maximum)?;
        let unique = skills
            .iter()
            .map(SkillIntent::skill_id)
            .collect::<BTreeSet<_>>();
        if unique.len() != skills.len() {
            return Err(PromptValueError::DuplicateSkill);
        }
        Ok(Self {
            body,
            skills: skills.into(),
        })
    }

    pub const fn body(&self) -> &PromptBodyIntent {
        &self.body
    }

    pub fn skills(&self) -> &[SkillIntent] {
        &self.skills
    }
}

pub(crate) fn validate_skill_intent_count(
    count: usize,
    maximum: usize,
) -> Result<(), PromptValueError> {
    if count > maximum {
        return Err(PromptValueError::TooManySkills);
    }
    Ok(())
}

pub(crate) fn normalize_text_intent(
    text: &str,
    maximum: usize,
) -> Result<String, PromptValueError> {
    let text = normalize_newlines(text);
    validate_prompt_text(&text, maximum, false)?;
    Ok(text)
}

#[derive(Clone, Eq, PartialEq)]
pub enum MessageContent {
    Text(MessageText),
}

#[derive(Clone, Eq, PartialEq)]
pub struct MessageText(Arc<str>);

impl MessageText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl MessageContent {
    #[allow(dead_code, reason = "consumed by PromptSet composition in M7")]
    fn text(text: impl AsRef<str>) -> Result<Self, PromptValueError> {
        let text = normalize_newlines(text.as_ref());
        let maximum = ProtocolLimits::v1_0().prompt.max_message_part_bytes as usize;
        validate_prompt_text(&text, maximum, false)?;
        Ok(Self::Text(MessageText(text.into())))
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct_text(text: impl AsRef<str>) -> Result<Self, PromptValueError> {
        Self::text(text)
    }

    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text.as_str(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct MessageRecord {
    content: Arc<[MessageContent]>,
}

impl MessageRecord {
    #[allow(dead_code, reason = "consumed by PromptSet composition and replay")]
    fn new(content: Vec<MessageContent>) -> Result<Self, PromptValueError> {
        let limits = ProtocolLimits::v1_0().prompt;
        if content.is_empty() || content.len() > limits.max_user_message_parts as usize {
            return Err(PromptValueError::InvalidPartCount);
        }
        let mut aggregate = 0_usize;
        for part in &content {
            let text = part.as_text();
            validate_prompt_text(text, limits.max_message_part_bytes as usize, false)?;
            aggregate = aggregate
                .checked_add(text.len())
                .ok_or(PromptValueError::TextTooLong)?;
            if aggregate > limits.max_user_message_bytes as usize {
                return Err(PromptValueError::TextTooLong);
            }
        }
        Ok(Self {
            content: content.into(),
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(content: Vec<MessageContent>) -> Result<Self, PromptValueError> {
        Self::new(content)
    }

    pub fn content(&self) -> &[MessageContent] {
        &self.content
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PromptContributionOrigin {
    Skill {
        skill_id: SkillId,
    },
    Workspace {
        root_key: WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
    },
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PromptContributionStamp {
    content_part_index: u32,
    origin: PromptContributionOrigin,
}

impl PromptContributionStamp {
    #[allow(dead_code, reason = "consumed by PromptSet composition and replay")]
    fn new(
        content_part_index: u32,
        origin: PromptContributionOrigin,
    ) -> Result<Self, PromptValueError> {
        if let PromptContributionOrigin::Workspace {
            relative_location, ..
        } = &origin
        {
            validate_prompt_text(
                relative_location.as_str(),
                ProtocolLimits::v1_0().workspace.max_relative_path_bytes as usize,
                true,
            )?;
        }
        Ok(Self {
            content_part_index,
            origin,
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        content_part_index: u32,
        origin: PromptContributionOrigin,
    ) -> Result<Self, PromptValueError> {
        Self::new(content_part_index, origin)
    }

    pub const fn content_part_index(&self) -> u32 {
        self.content_part_index
    }

    pub const fn origin(&self) -> &PromptContributionOrigin {
        &self.origin
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CanonicalUserMessage {
    message: MessageRecord,
    contribution_stamps: Arc<[PromptContributionStamp]>,
}

impl CanonicalUserMessage {
    #[allow(dead_code, reason = "consumed by PromptSet composition and replay")]
    fn new(
        message: MessageRecord,
        contribution_stamps: Vec<PromptContributionStamp>,
    ) -> Result<Self, PromptValueError> {
        validate_contribution_stamps(&message, &contribution_stamps, true)?;
        Ok(Self {
            message,
            contribution_stamps: contribution_stamps.into(),
        })
    }

    #[allow(
        dead_code,
        reason = "consumed by tolerant Conversation replay in M3/M5"
    )]
    pub(crate) fn reconstruct(
        message: MessageRecord,
        contribution_stamps: Vec<PromptContributionStamp>,
    ) -> Result<Self, PromptValueError> {
        validate_contribution_stamps(&message, &contribution_stamps, false)?;
        Ok(Self {
            message,
            contribution_stamps: contribution_stamps.into(),
        })
    }

    pub const fn message(&self) -> &MessageRecord {
        &self.message
    }

    pub fn contribution_stamps(&self) -> &[PromptContributionStamp] {
        &self.contribution_stamps
    }

    pub(crate) fn validate_for_wire(&self) -> Result<(), PromptValueError> {
        validate_contribution_stamps(&self.message, &self.contribution_stamps, true)
    }
}

fn validate_contribution_stamps(
    message: &MessageRecord,
    contribution_stamps: &[PromptContributionStamp],
    require_complete_provenance: bool,
) -> Result<(), PromptValueError> {
    if contribution_stamps.len() > message.content().len() {
        return Err(PromptValueError::InvalidContributionStamp);
    }
    let mut indices = BTreeSet::new();
    let mut origins = BTreeSet::new();
    let mut previous_index = None;
    for stamp in contribution_stamps {
        let index = stamp.content_part_index() as usize;
        if index >= message.content().len()
            || previous_index.is_some_and(|previous| index <= previous)
            || !indices.insert(index)
            || !origins.insert(stamp.origin())
        {
            return Err(PromptValueError::InvalidContributionStamp);
        }
        previous_index = Some(index);
    }
    if require_complete_provenance {
        let unstamped = (0..message.content().len())
            .filter(|index| !indices.contains(index))
            .collect::<Vec<_>>();
        if unstamped.len() > 1 || unstamped.first().is_some_and(|index| *index != 0) {
            return Err(PromptValueError::InvalidContributionStamp);
        }
    }
    Ok(())
}

impl fmt::Debug for CanonicalUserMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalUserMessage")
            .field("parts", &self.message.content().len())
            .field("contribution_stamps", &self.contribution_stamps.len())
            .finish()
    }
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ModelMessageError {
    reason: ModelMessageErrorReason,
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelMessageErrorReason {
    EmptyText,
    UnsafeText,
    TextTooLong,
    EmptyAssistantContent,
    DuplicateToolCallId,
}

impl fmt::Debug for ModelMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelMessageError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for ModelMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid model message")
    }
}

impl std::error::Error for ModelMessageError {}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone)]
pub(crate) struct ModelMessage {
    kind: ModelMessageKind,
}

#[allow(
    dead_code,
    reason = "owned exclusively by Prompt transcript construction"
)]
#[derive(Clone)]
enum ModelMessageKind {
    User {
        message: CanonicalUserMessage,
    },
    Assistant {
        content: Arc<[ModelAssistantContent]>,
    },
    Tool {
        tool_call_id: ToolCallId,
        content: ToolResultContent,
    },
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone)]
pub(crate) struct ModelAssistantContent {
    kind: ModelAssistantContentKind,
}

#[allow(
    dead_code,
    reason = "owned exclusively by Prompt transcript construction"
)]
#[derive(Clone)]
enum ModelAssistantContentKind {
    Reasoning(ReasoningContent),
    Text(Arc<str>),
    ToolCall {
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}

#[allow(dead_code, reason = "consumed by authorized M4 transcript readers")]
#[derive(Clone, Copy)]
pub(crate) enum ModelMessageRef<'a> {
    User {
        content: &'a [MessageContent],
    },
    Assistant {
        content: &'a [ModelAssistantContent],
    },
    Tool {
        tool_call_id: &'a ToolCallId,
        content: &'a ToolResultContent,
    },
}

#[allow(dead_code, reason = "consumed by authorized M4 transcript readers")]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ModelAssistantContentRef<'a> {
    Reasoning(&'a ReasoningContent),
    Text(&'a str),
    ToolCall {
        tool_call_id: &'a ToolCallId,
        name: &'a ToolName,
        arguments: &'a BoundedJsonObject,
    },
}

impl PartialEq for ModelMessageRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::User { content: left }, Self::User { content: right }) => left == right,
            (Self::Assistant { content: left }, Self::Assistant { content: right }) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right.iter())
                        .all(|(left, right)| left.as_ref() == right.as_ref())
            }
            (
                Self::Tool {
                    tool_call_id: left_id,
                    content: left_content,
                },
                Self::Tool {
                    tool_call_id: right_id,
                    content: right_content,
                },
            ) => left_id == right_id && left_content == right_content,
            _ => false,
        }
    }
}

impl Eq for ModelMessageRef<'_> {}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
impl ModelMessage {
    pub(crate) fn canonical_user(message: CanonicalUserMessage) -> Self {
        Self {
            kind: ModelMessageKind::User { message },
        }
    }

    pub(crate) fn unstamped_user_text(text: Arc<str>) -> Result<Self, ModelMessageError> {
        let text = normalize_newlines(&text);
        let maximum = ProtocolLimits::v1_0().prompt.max_message_part_bytes as usize;
        validate_model_message_text(&text, maximum)?;
        Ok(Self::canonical_user(CanonicalUserMessage {
            message: MessageRecord {
                content: Arc::from([MessageContent::Text(MessageText(text.into()))]),
            },
            contribution_stamps: Arc::from([]),
        }))
    }

    pub(crate) fn rolling_summary(summary: Arc<str>) -> Result<Self, ModelMessageError> {
        validate_model_message_text(&summary, 65_536)?;
        Ok(Self::canonical_user(CanonicalUserMessage {
            message: MessageRecord {
                content: Arc::from([MessageContent::Text(MessageText(summary))]),
            },
            contribution_stamps: Arc::from([]),
        }))
    }

    pub(crate) fn assistant(
        content: Arc<[ModelAssistantContent]>,
    ) -> Result<Self, ModelMessageError> {
        if content.is_empty() {
            return Err(ModelMessageError {
                reason: ModelMessageErrorReason::EmptyAssistantContent,
            });
        }
        let mut tool_call_ids = BTreeSet::new();
        for block in &*content {
            if let ModelAssistantContentKind::ToolCall { tool_call_id, .. } = &block.kind {
                if !tool_call_ids.insert(tool_call_id) {
                    return Err(ModelMessageError {
                        reason: ModelMessageErrorReason::DuplicateToolCallId,
                    });
                }
            }
        }
        Ok(Self {
            kind: ModelMessageKind::Assistant { content },
        })
    }

    pub(crate) fn tool_result(tool_call_id: ToolCallId, content: ToolResultContent) -> Self {
        Self {
            kind: ModelMessageKind::Tool {
                tool_call_id,
                content,
            },
        }
    }

    pub(crate) fn as_ref(&self) -> ModelMessageRef<'_> {
        match &self.kind {
            ModelMessageKind::User { message } => ModelMessageRef::User {
                content: message.message().content(),
            },
            ModelMessageKind::Assistant { content } => ModelMessageRef::Assistant { content },
            ModelMessageKind::Tool {
                tool_call_id,
                content,
            } => ModelMessageRef::Tool {
                tool_call_id,
                content,
            },
        }
    }

    pub(crate) fn compaction_estimated_tokens(&self, estimator: TokenEstimator) -> Option<u64> {
        let mut bytes = 1_usize;
        add_model_message(&mut bytes, self)?;
        estimator.checked_estimate_utf8_bytes(u64::try_from(bytes).ok()?)
    }
}

impl fmt::Debug for ModelMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ModelMessageKind::User { message } => formatter
                .debug_struct("ModelMessage")
                .field("role", &"user")
                .field("content_parts", &message.message().content().len())
                .finish(),
            ModelMessageKind::Assistant { content } => formatter
                .debug_struct("ModelMessage")
                .field("role", &"assistant")
                .field("content_blocks", &content.len())
                .finish(),
            ModelMessageKind::Tool { .. } => formatter
                .debug_struct("ModelMessage")
                .field("role", &"tool")
                .field("content", &"redacted")
                .finish(),
        }
    }
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
impl ModelAssistantContent {
    pub(crate) fn reasoning(content: ReasoningContent) -> Self {
        Self {
            kind: ModelAssistantContentKind::Reasoning(content),
        }
    }

    pub(crate) fn text(text: Arc<str>) -> Result<Self, ModelMessageError> {
        validate_model_message_text(&text, 65_536)?;
        Ok(Self {
            kind: ModelAssistantContentKind::Text(text),
        })
    }

    pub(crate) fn tool_call(
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    ) -> Self {
        Self {
            kind: ModelAssistantContentKind::ToolCall {
                tool_call_id,
                name,
                arguments,
            },
        }
    }

    pub(crate) fn as_ref(&self) -> ModelAssistantContentRef<'_> {
        match &self.kind {
            ModelAssistantContentKind::Reasoning(content) => {
                ModelAssistantContentRef::Reasoning(content)
            }
            ModelAssistantContentKind::Text(text) => ModelAssistantContentRef::Text(text),
            ModelAssistantContentKind::ToolCall {
                tool_call_id,
                name,
                arguments,
            } => ModelAssistantContentRef::ToolCall {
                tool_call_id,
                name,
                arguments,
            },
        }
    }
}

impl fmt::Debug for ModelAssistantContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ModelAssistantContentKind::Reasoning(_) => formatter
                .debug_tuple("ModelAssistantContent::Reasoning")
                .field(&"redacted")
                .finish(),
            ModelAssistantContentKind::Text(_) => formatter
                .debug_tuple("ModelAssistantContent::Text")
                .field(&"redacted")
                .finish(),
            ModelAssistantContentKind::ToolCall { .. } => formatter
                .debug_tuple("ModelAssistantContent::ToolCall")
                .field(&"redacted")
                .finish(),
        }
    }
}

impl fmt::Debug for ModelMessageRef<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User { content } => formatter
                .debug_struct("ModelMessageRef::User")
                .field("content_parts", &content.len())
                .finish(),
            Self::Assistant { content } => formatter
                .debug_struct("ModelMessageRef::Assistant")
                .field("content_blocks", &content.len())
                .finish(),
            Self::Tool { .. } => formatter
                .debug_struct("ModelMessageRef::Tool")
                .field("content", &"redacted")
                .finish(),
        }
    }
}

impl fmt::Debug for ModelAssistantContentRef<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reasoning(_) => formatter
                .debug_tuple("ModelAssistantContentRef::Reasoning")
                .field(&"redacted")
                .finish(),
            Self::Text(_) => formatter
                .debug_tuple("ModelAssistantContentRef::Text")
                .field(&"redacted")
                .finish(),
            Self::ToolCall { .. } => formatter
                .debug_tuple("ModelAssistantContentRef::ToolCall")
                .field(&"redacted")
                .finish(),
        }
    }
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
fn validate_model_message_text(text: &str, maximum: usize) -> Result<(), ModelMessageError> {
    validate_safe_text(text, maximum, false).map_err(|error| ModelMessageError {
        reason: match error {
            LexicalError::Empty => ModelMessageErrorReason::EmptyText,
            LexicalError::TooLong => ModelMessageErrorReason::TextTooLong,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                ModelMessageErrorReason::UnsafeText
            }
        },
    })
}

fn validate_prompt_text(
    text: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), PromptValueError> {
    validate_safe_text(text, maximum, allow_empty).map_err(|error| match error {
        crate::wire::lexical::LexicalError::Empty => PromptValueError::EmptyText,
        crate::wire::lexical::LexicalError::TooLong => PromptValueError::TextTooLong,
        crate::wire::lexical::LexicalError::InvalidGrammar
        | crate::wire::lexical::LexicalError::UnsafeText => PromptValueError::UnsafeText,
    })
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::agent_session_lifecycle::AgentRevisionRef;
    use crate::conversation_storage::StoredUserMessage;
    use crate::live_conversation::LiveSessionState;
    use crate::model_gateway::{
        ModelCallPurpose, ProviderItemId, StructuredOutputContract, TurnModelSnapshot,
    };
    use crate::skills::SkillView;
    use crate::tools::{ToolCallId, ToolName, ToolResultContent, ToolSet};
    use crate::turn_item_interaction::UserMessageSource;
    use crate::wire::lexical::canonical_json_string_len;
    use crate::wire::{
        AgentRevision, BoundedJsonObject, BoundedJsonSchema, ItemId, SessionDefinitionRevision,
        SessionId, Timestamp, TurnId,
    };
    use crate::workspace::prompt_candidate_for_test;

    struct MutablePromptSource {
        result: Mutex<Result<Vec<PromptSourceDefinition>, PromptSourceError>>,
        calls: AtomicUsize,
    }

    impl MutablePromptSource {
        fn new(definitions: Vec<PromptSourceDefinition>) -> Self {
            Self {
                result: Mutex::new(Ok(definitions)),
                calls: AtomicUsize::new(0),
            }
        }

        fn replace(&self, definitions: Vec<PromptSourceDefinition>) {
            *self.result.lock().unwrap() = Ok(definitions);
        }

        fn fail(&self) {
            *self.result.lock().unwrap() = Err(PromptSourceError::Unavailable);
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl PromptSourceAdapter for MutablePromptSource {
        fn discover(&self) -> PromptSourceFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }
    }

    struct MutableWorkspacePromptSource {
        result: Mutex<Result<Vec<WorkspacePromptSource>, PromptSourceError>>,
        calls: AtomicUsize,
    }

    impl MutableWorkspacePromptSource {
        fn new(sources: Vec<WorkspacePromptSource>) -> Self {
            Self {
                result: Mutex::new(Ok(sources)),
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl WorkspacePromptSourceAdapter for MutableWorkspacePromptSource {
        fn capture<'a>(
            &'a self,
            _context: &'a WorkspacePromptCaptureContext,
        ) -> WorkspacePromptSourceFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }
    }

    fn session_id() -> SessionId {
        "ses_11111111111111111111111111111111".parse().unwrap()
    }

    fn other_session_id() -> SessionId {
        "ses_22222222222222222222222222222222".parse().unwrap()
    }

    fn agent_revision_ref() -> AgentRevisionRef {
        AgentRevisionRef::new(
            "agt_11111111111111111111111111111111".parse().unwrap(),
            AgentRevision::new(NonZeroU64::new(7).unwrap()),
        )
    }

    fn session_revision() -> SessionDefinitionRevision {
        SessionDefinitionRevision::new(NonZeroU64::new(11).unwrap())
    }

    fn source_definition(
        id: &str,
        key: &str,
        role: PromptRole,
        provenance: PromptSourceProvenance,
        content: &str,
    ) -> PromptSourceDefinition {
        PromptSourceDefinition::new(
            id.parse().unwrap(),
            key,
            format!("name-{id}"),
            Some(format!("description-{id}").into_boxed_str()),
            role,
            Arc::from(content),
            provenance,
        )
    }

    fn runtime_definition(
        id: &str,
        key: &str,
        role: PromptRole,
        content: &str,
    ) -> PromptSourceDefinition {
        source_definition(
            id,
            key,
            role,
            PromptSourceProvenance::Runtime(format!("source-{id}").into_boxed_str()),
            content,
        )
    }

    fn user_definition(
        id: &str,
        key: &str,
        role: PromptRole,
        content: &str,
    ) -> PromptSourceDefinition {
        source_definition(
            id,
            key,
            role,
            PromptSourceProvenance::User(format!("source-{id}").into_boxed_str()),
            content,
        )
    }

    fn empty_workspace(session_id: SessionId) -> WorkspacePromptContext {
        prompt_candidate_for_test(session_id, vec!["root".parse().unwrap()])
            .finish(Arc::from([]), Arc::from([]))
            .unwrap()
            .prompt_context()
    }

    fn turn_context(
        resources: Arc<PromptResourceView>,
        agent_prompts: Vec<PromptId>,
        session_prompts: Vec<PromptId>,
        workspace: WorkspacePromptContext,
        tool_set: &Arc<ToolSet>,
        skill_view: &Arc<SkillView>,
    ) -> PromptTurnContext {
        turn_context_with_model(
            resources,
            agent_prompts,
            session_prompts,
            workspace,
            tool_set,
            skill_view,
            TurnModelSnapshot::test_fixture(None),
        )
    }

    fn turn_context_with_model(
        resources: Arc<PromptResourceView>,
        agent_prompts: Vec<PromptId>,
        session_prompts: Vec<PromptId>,
        workspace: WorkspacePromptContext,
        tool_set: &Arc<ToolSet>,
        skill_view: &Arc<SkillView>,
        model: Arc<TurnModelSnapshot>,
    ) -> PromptTurnContext {
        PromptTurnContext::new(
            agent_revision_ref(),
            session_id(),
            session_revision(),
            resources,
            AgentPromptSelection::new(agent_prompts).unwrap(),
            SessionPromptSelection::new(session_prompts).unwrap(),
            workspace,
            tool_set.prompt_view(),
            skill_view.prompt_view(),
            model,
        )
    }

    fn profile_texts(sections: &[PromptSection]) -> Vec<&str> {
        sections.iter().map(PromptSection::text).collect()
    }

    fn assert_prompt_error<T>(result: Result<T, PromptError>, expected: PromptErrorKind) {
        match result {
            Err(error) => assert_eq!(error.kind(), expected),
            Ok(_) => panic!("prompt operation unexpectedly succeeded"),
        }
    }

    fn assert_model_message_error<T>(
        result: Result<T, ModelMessageError>,
        expected: ModelMessageErrorReason,
    ) {
        match result {
            Err(error) => assert_eq!(error.reason, expected),
            Ok(_) => panic!("model message construction unexpectedly succeeded"),
        }
    }

    fn reasoning_content() -> ReasoningContent {
        ReasoningContent::reconstruct(
            Some("reasoning artifact".to_owned()),
            Some("reasoning summary".to_owned()),
            Some("encrypted artifact".to_owned()),
            Some("reasoning signature".to_owned()),
            Some(ProviderItemId::from_str("provider-item").unwrap()),
        )
        .unwrap()
    }

    fn tool_call_id(value: &str) -> ToolCallId {
        ToolCallId::from_str(value).unwrap()
    }

    fn tool_name(value: &str) -> ToolName {
        ToolName::from_str(value).unwrap()
    }

    fn tool_arguments() -> BoundedJsonObject {
        BoundedJsonObject::from_slice(br#"{"query":"argument secret"}"#).unwrap()
    }

    fn tool_result() -> ToolResultContent {
        ToolResultContent::from_text_parts(vec!["tool result secret".to_owned()]).unwrap()
    }

    fn canonical_user_with_stamp() -> CanonicalUserMessage {
        let message =
            MessageRecord::new(vec![MessageContent::text("user secret").unwrap()]).unwrap();
        let stamp = PromptContributionStamp::new(
            0,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("review").unwrap(),
            },
        )
        .unwrap();
        CanonicalUserMessage::new(message, vec![stamp]).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_candidates_pin_materialized_content_without_a_current_pointer() {
        let source = Arc::new(MutablePromptSource::new(vec![runtime_definition(
            "agent-main",
            "agent-main",
            PromptRole::System,
            "old\r\nagent content",
        )]));
        let shared_source: Arc<dyn PromptSourceAdapter> = source.clone();
        let service = PromptService::new(
            Arc::from("runtime policy secret\r\nline"),
            Some(Arc::from("base policy")),
            vec![shared_source],
            Vec::new(),
        )
        .unwrap();

        let old_resources = service.initialize().await.unwrap();
        let tool_set = ToolSet::empty();
        let skill_view = SkillView::empty();
        let old_set = service
            .for_turn(turn_context(
                Arc::clone(&old_resources),
                vec!["agent-main".parse().unwrap()],
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            ))
            .unwrap();
        assert_eq!(source.call_count(), 1);
        assert_eq!(old_resources.definition_count(), 1);
        assert_eq!(
            profile_texts(old_set.profile().system()),
            [
                "runtime policy secret\nline",
                "base policy",
                "old\nagent content",
            ]
        );
        assert_eq!(old_set.agent(), agent_revision_ref());
        assert_eq!(old_set.session_id(), session_id());
        assert_eq!(old_set.session_revision(), session_revision());

        source.replace(vec![runtime_definition(
            "agent-main",
            "agent-main",
            PromptRole::System,
            "new agent content",
        )]);
        let new_resources = service.build_reload_candidate().await.unwrap();
        let new_set = service
            .for_turn(turn_context(
                new_resources,
                vec!["agent-main".parse().unwrap()],
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            ))
            .unwrap();

        assert_eq!(source.call_count(), 2);
        assert_eq!(
            profile_texts(old_set.profile().system()),
            [
                "runtime policy secret\nline",
                "base policy",
                "old\nagent content",
            ]
        );
        assert_eq!(
            profile_texts(new_set.profile().system()),
            [
                "runtime policy secret\nline",
                "base policy",
                "new agent content",
            ]
        );
        assert!(tool_set.owns_prompt_view(&old_set.tools));
        assert!(skill_view.owns_prompt_view(&old_set.skills));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_profile_uses_fixed_layers_and_stable_source_order() {
        let shared = Arc::new(MutablePromptSource::new(vec![
            user_definition("session-z", "session-z", PromptRole::User, "session z"),
            runtime_definition("agent-z", "agent-z", PromptRole::System, "agent z"),
            user_definition("session-a", "session-a", PromptRole::User, "session a"),
            runtime_definition("agent-a", "agent-a", PromptRole::System, "agent a"),
        ]));
        let workspace = Arc::new(MutableWorkspacePromptSource::new(vec![
            WorkspacePromptSource::new(
                "root-a".parse().unwrap(),
                "z.md".parse().unwrap(),
                Arc::from("workspace z"),
            ),
            WorkspacePromptSource::new(
                "root-b".parse().unwrap(),
                "same.md".parse().unwrap(),
                Arc::from("workspace same\r\nline"),
            ),
            WorkspacePromptSource::new(
                "root-a".parse().unwrap(),
                "a.md".parse().unwrap(),
                Arc::from("workspace a"),
            ),
        ]));
        let shared_adapter: Arc<dyn PromptSourceAdapter> = shared.clone();
        let workspace_adapter: Arc<dyn WorkspacePromptSourceAdapter> = workspace.clone();
        let service = PromptService::new(
            Arc::from("required"),
            Some(Arc::from("base")),
            vec![shared_adapter],
            vec![workspace_adapter],
        )
        .unwrap();
        let resources = service.build_candidate().await.unwrap();

        let candidate = prompt_candidate_for_test(
            session_id(),
            vec!["root-b".parse().unwrap(), "root-a".parse().unwrap()],
        );
        let captured = service
            .capture_workspace_sources(candidate.prompt_capture_context())
            .await
            .unwrap();
        assert_eq!(workspace.call_count(), 1);
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].relative_location().as_str(), "a.md");
        assert_eq!(captured[1].relative_location().as_str(), "same.md");
        assert_eq!(captured[1].content(), "workspace same\nline");
        assert_eq!(captured[2].relative_location().as_str(), "z.md");
        let workspace_context = candidate
            .finish(captured, Arc::from([]))
            .unwrap()
            .prompt_context();

        let tool_set = ToolSet::empty();
        let skill_view = SkillView::empty();
        let set = service
            .for_turn(turn_context(
                resources,
                vec!["agent-z".parse().unwrap(), "agent-a".parse().unwrap()],
                vec!["session-z".parse().unwrap(), "session-a".parse().unwrap()],
                workspace_context,
                &tool_set,
                &skill_view,
            ))
            .unwrap();

        assert_eq!(shared.call_count(), 1);
        assert_eq!(workspace.call_count(), 1);
        assert_eq!(
            set.profile()
                .system()
                .iter()
                .map(PromptSection::kind)
                .collect::<Vec<_>>(),
            [
                PromptSectionKind::RuntimeRequired,
                PromptSectionKind::RuntimeBase,
                PromptSectionKind::Agent,
                PromptSectionKind::Agent,
            ]
        );
        assert_eq!(
            profile_texts(set.profile().system()),
            ["required", "base", "agent a", "agent z"]
        );
        assert_eq!(
            set.profile()
                .user_context()
                .iter()
                .map(PromptSection::kind)
                .collect::<Vec<_>>(),
            [
                PromptSectionKind::Session,
                PromptSectionKind::Session,
                PromptSectionKind::Workspace,
                PromptSectionKind::Workspace,
                PromptSectionKind::Workspace,
            ]
        );
        assert_eq!(
            profile_texts(set.profile().user_context()),
            [
                "session a",
                "session z",
                "workspace a",
                "workspace same\nline",
                "workspace z",
            ]
        );
        assert!(
            set.profile()
                .system()
                .iter()
                .all(|section| section.role() == PromptRole::System)
        );
        assert!(
            set.profile()
                .user_context()
                .iter()
                .all(|section| section.role() == PromptRole::User)
        );
        assert!(tool_set.owns_prompt_view(&set.tools));
        assert!(skill_view.owns_prompt_view(&set.skills));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_candidate_and_selection_fail_closed_with_typed_redacted_errors() {
        assert_prompt_error(
            PromptService::new(Arc::from(""), None, Vec::new(), Vec::new()),
            PromptErrorKind::RequiredPromptMissing,
        );

        let duplicate = Arc::new(MutablePromptSource::new(vec![
            runtime_definition("same", "first", PromptRole::System, "first"),
            runtime_definition("same", "second", PromptRole::System, "second"),
        ]));
        let duplicate_adapter: Arc<dyn PromptSourceAdapter> = duplicate;
        let service = PromptService::new(
            Arc::from("required"),
            None,
            vec![duplicate_adapter],
            Vec::new(),
        )
        .unwrap();
        assert_prompt_error(
            service.build_candidate().await,
            PromptErrorKind::DuplicateKey,
        );

        let unsafe_source = Arc::new(MutablePromptSource::new(vec![runtime_definition(
            "unsafe",
            "unsafe",
            PromptRole::System,
            "secret\u{001b}",
        )]));
        let unsafe_adapter: Arc<dyn PromptSourceAdapter> = unsafe_source;
        let service = PromptService::new(
            Arc::from("required"),
            None,
            vec![unsafe_adapter],
            Vec::new(),
        )
        .unwrap();
        let error = service.build_candidate().await.unwrap_err();
        assert_eq!(error.kind(), PromptErrorKind::ContentLoad);
        assert_eq!(error.to_string(), "prompt operation failed");
        assert!(!format!("{error:?}").contains("secret"));

        let failed_source = Arc::new(MutablePromptSource::new(Vec::new()));
        failed_source.fail();
        let failed_adapter: Arc<dyn PromptSourceAdapter> = failed_source;
        let service = PromptService::new(
            Arc::from("required"),
            None,
            vec![failed_adapter],
            Vec::new(),
        )
        .unwrap();
        assert_prompt_error(
            service.build_candidate().await,
            PromptErrorKind::SourceDiscovery,
        );

        let source = Arc::new(MutablePromptSource::new(vec![
            runtime_definition("agent-a", "duplicate", PromptRole::System, "agent a"),
            runtime_definition("agent-b", "duplicate", PromptRole::System, "agent b"),
            user_definition(
                "untrusted-system",
                "untrusted-system",
                PromptRole::System,
                "untrusted",
            ),
            user_definition("session-user", "session-user", PromptRole::User, "session"),
        ]));
        let adapter: Arc<dyn PromptSourceAdapter> = source;
        let service =
            PromptService::new(Arc::from("required"), None, vec![adapter], Vec::new()).unwrap();
        let resources = service.build_candidate().await.unwrap();
        let tool_set = ToolSet::empty();
        let skill_view = SkillView::empty();

        assert_prompt_error(
            service.for_turn(turn_context(
                Arc::clone(&resources),
                vec!["missing".parse().unwrap()],
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            )),
            PromptErrorKind::PromptUnavailable,
        );
        assert_prompt_error(
            service.for_turn(turn_context(
                Arc::clone(&resources),
                vec!["session-user".parse().unwrap()],
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            )),
            PromptErrorKind::InvalidRole,
        );
        assert_prompt_error(
            service.for_turn(turn_context(
                Arc::clone(&resources),
                vec!["untrusted-system".parse().unwrap()],
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            )),
            PromptErrorKind::InvalidRole,
        );
        assert_prompt_error(
            service.for_turn(turn_context(
                Arc::clone(&resources),
                vec!["agent-a".parse().unwrap(), "agent-b".parse().unwrap()],
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            )),
            PromptErrorKind::DuplicateKey,
        );
        assert_prompt_error(
            service.for_turn(turn_context(
                Arc::clone(&resources),
                Vec::new(),
                vec!["agent-a".parse().unwrap()],
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            )),
            PromptErrorKind::InvalidRole,
        );

        let other_service =
            PromptService::new(Arc::from("other required"), None, Vec::new(), Vec::new()).unwrap();
        let other_resources = other_service.build_candidate().await.unwrap();
        assert_prompt_error(
            service.for_turn(turn_context(
                other_resources,
                Vec::new(),
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            )),
            PromptErrorKind::PromptUnavailable,
        );

        let mismatched_workspace = empty_workspace(other_session_id());
        assert_prompt_error(
            service.for_turn(turn_context(
                resources,
                Vec::new(),
                Vec::new(),
                mismatched_workspace,
                &tool_set,
                &skill_view,
            )),
            PromptErrorKind::InvalidContribution,
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn workspace_source_capture_is_candidate_only_bounded_and_fail_closed() {
        let candidate = prompt_candidate_for_test(session_id(), vec!["root".parse().unwrap()]);
        let service =
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap();
        assert_prompt_error(
            service
                .capture_workspace_sources(candidate.prompt_capture_context())
                .await,
            PromptErrorKind::SourceDiscovery,
        );

        let duplicate = Arc::new(MutableWorkspacePromptSource::new(vec![
            WorkspacePromptSource::new(
                "root".parse().unwrap(),
                "instructions.md".parse().unwrap(),
                Arc::from("first"),
            ),
            WorkspacePromptSource::new(
                "root".parse().unwrap(),
                "instructions.md".parse().unwrap(),
                Arc::from("second"),
            ),
        ]));
        let duplicate_adapter: Arc<dyn WorkspacePromptSourceAdapter> = duplicate;
        let service = PromptService::new(
            Arc::from("required"),
            None,
            Vec::new(),
            vec![duplicate_adapter],
        )
        .unwrap();
        assert_prompt_error(
            service
                .capture_workspace_sources(candidate.prompt_capture_context())
                .await,
            PromptErrorKind::DuplicateKey,
        );

        for source in [
            WorkspacePromptSource::new(
                "other".parse().unwrap(),
                "instructions.md".parse().unwrap(),
                Arc::from("unauthorized"),
            ),
            WorkspacePromptSource::new(
                "root".parse().unwrap(),
                WorkspaceRelativePath::default(),
                Arc::from("root location"),
            ),
            WorkspacePromptSource::new(
                "root".parse().unwrap(),
                "unsafe.md".parse().unwrap(),
                Arc::from("unsafe\u{001b}"),
            ),
        ] {
            let adapter = Arc::new(MutableWorkspacePromptSource::new(vec![source]));
            let adapter: Arc<dyn WorkspacePromptSourceAdapter> = adapter;
            let service =
                PromptService::new(Arc::from("required"), None, Vec::new(), vec![adapter]).unwrap();
            assert_prompt_error(
                service
                    .capture_workspace_sources(candidate.prompt_capture_context())
                    .await,
                PromptErrorKind::ContentLoad,
            );
        }

        let empty_candidate =
            prompt_candidate_for_test(session_id(), vec!["root".parse().unwrap()]);
        let empty_snapshot = empty_candidate
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        assert!(empty_snapshot.prompt_context().sources().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_set_composes_one_atomic_text_message_and_rejects_deferred_skills() {
        let service =
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap();
        let resources = service.build_candidate().await.unwrap();
        let tool_set = ToolSet::empty();
        let skill_view = SkillView::empty();
        let set = service
            .for_turn(turn_context(
                resources,
                Vec::new(),
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
            ))
            .unwrap();

        let message = set
            .compose_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("first\r\nsecond").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(message.message().content().len(), 1);
        assert_eq!(message.message().content()[0].as_text(), "first\nsecond");
        assert!(message.contribution_stamps().is_empty());

        assert_prompt_error(
            set.compose_user_message(
                PromptIntent::new(PromptBodyIntent::Empty, Vec::new()).unwrap(),
            ),
            PromptErrorKind::InvalidIntent,
        );
        assert_prompt_error(
            set.compose_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("body remains unapplied").unwrap()),
                    vec![SkillIntent::new("deferred-skill".parse().unwrap())],
                )
                .unwrap(),
            ),
            PromptErrorKind::PromptUnavailable,
        );

        let debug = format!("{service:?} {set:?} {:?}", set.profile().system()[0]);
        for secret in [
            "runtime policy secret",
            "first",
            "second",
            "body remains unapplied",
            "deferred-skill",
        ] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn model_message_user_constructors_project_only_content() {
        let canonical = ModelMessage::canonical_user(canonical_user_with_stamp());
        let ModelMessageRef::User { content } = canonical.as_ref() else {
            panic!("canonical user did not retain the user role");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "user secret");

        let unstamped = ModelMessage::unstamped_user_text(Arc::from("a\r\nb\rc")).unwrap();
        let ModelMessageRef::User { content } = unstamped.as_ref() else {
            panic!("unstamped user text did not construct a user message");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "a\nb\nc");
        let ModelMessageKind::User { message } = &unstamped.kind else {
            panic!("unstamped user text did not retain the user role");
        };
        assert!(message.contribution_stamps().is_empty());

        assert_model_message_error(
            ModelMessage::unstamped_user_text(Arc::from("")),
            ModelMessageErrorReason::EmptyText,
        );
        assert_model_message_error(
            ModelMessage::unstamped_user_text(Arc::from("x".repeat(131_073))),
            ModelMessageErrorReason::TextTooLong,
        );
        assert_model_message_error(
            ModelMessage::unstamped_user_text(Arc::from("unsafe\u{001b}")),
            ModelMessageErrorReason::UnsafeText,
        );
    }

    #[test]
    fn rolling_summary_is_verbatim_one_unstamped_user_text_and_rejects_invalid_text() {
        let summary = ModelMessage::rolling_summary(Arc::from("first line\nsecond line")).unwrap();
        let ModelMessageRef::User { content } = summary.as_ref() else {
            panic!("rolling summary did not construct a user message");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "first line\nsecond line");
        let ModelMessageKind::User { message } = &summary.kind else {
            panic!("rolling summary did not retain the user role");
        };
        assert!(message.contribution_stamps().is_empty());

        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("")),
            ModelMessageErrorReason::EmptyText,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("x".repeat(65_537))),
            ModelMessageErrorReason::TextTooLong,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("first\rsecond")),
            ModelMessageErrorReason::UnsafeText,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("first\r\nsecond")),
            ModelMessageErrorReason::UnsafeText,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("unsafe\u{001b}")),
            ModelMessageErrorReason::UnsafeText,
        );
    }

    #[test]
    fn assistant_content_and_message_preserve_source_order() {
        let reasoning = reasoning_content();
        let text = ModelAssistantContent::text(Arc::from("assistant\ntext")).unwrap();
        let call_id = tool_call_id("call-1");
        let name = tool_name("read_file");
        let arguments = tool_arguments();
        let call =
            ModelAssistantContent::tool_call(call_id.clone(), name.clone(), arguments.clone());
        let content: Arc<[ModelAssistantContent]> = Arc::from([
            ModelAssistantContent::reasoning(reasoning.clone()),
            text.clone(),
            call.clone(),
        ]);
        let assistant = ModelMessage::assistant(content).unwrap();

        let ModelMessageRef::Assistant { content } = assistant.as_ref() else {
            panic!("assistant content did not construct an assistant message");
        };
        assert_eq!(content.len(), 3);
        assert!(
            matches!(content[0].as_ref(), ModelAssistantContentRef::Reasoning(actual) if actual == &reasoning)
        );
        assert!(matches!(
            content[1].as_ref(),
            ModelAssistantContentRef::Text("assistant\ntext")
        ));
        assert!(matches!(
            content[2].as_ref(),
            ModelAssistantContentRef::ToolCall {
                tool_call_id,
                name: actual_name,
                arguments: actual_arguments,
            } if tool_call_id == &call_id
                && actual_name == &name
                && actual_arguments == &arguments
        ));

        assert!(matches!(
            text.as_ref(),
            ModelAssistantContentRef::Text("assistant\ntext")
        ));
        assert!(matches!(
            call.as_ref(),
            ModelAssistantContentRef::ToolCall { .. }
        ));
    }

    #[test]
    fn assistant_content_text_uses_external_text_rules() {
        let text = ModelAssistantContent::text(Arc::from("line one\nline two")).unwrap();
        assert!(matches!(
            text.as_ref(),
            ModelAssistantContentRef::Text("line one\nline two")
        ));
        assert_model_message_error(
            ModelAssistantContent::text(Arc::from("")),
            ModelMessageErrorReason::EmptyText,
        );
        assert_model_message_error(
            ModelAssistantContent::text(Arc::from("x".repeat(65_537))),
            ModelMessageErrorReason::TextTooLong,
        );
        assert_model_message_error(
            ModelAssistantContent::text(Arc::from("line one\r\nline two")),
            ModelMessageErrorReason::UnsafeText,
        );
    }

    #[test]
    fn assistant_rejects_empty_and_duplicate_tool_calls() {
        assert_model_message_error(
            ModelMessage::assistant(Arc::from([])),
            ModelMessageErrorReason::EmptyAssistantContent,
        );

        let duplicate = tool_call_id("call-duplicate");
        let content: Arc<[ModelAssistantContent]> = Arc::from([
            ModelAssistantContent::tool_call(
                duplicate.clone(),
                tool_name("read_file"),
                tool_arguments(),
            ),
            ModelAssistantContent::text(Arc::from("between calls")).unwrap(),
            ModelAssistantContent::tool_call(duplicate, tool_name("write_file"), tool_arguments()),
        ]);
        assert_model_message_error(
            ModelMessage::assistant(content),
            ModelMessageErrorReason::DuplicateToolCallId,
        );
    }

    #[test]
    fn tool_result_projection_exposes_only_tools_owned_values() {
        let call_id = tool_call_id("call-tool-result");
        let result = tool_result();
        let message = ModelMessage::tool_result(call_id.clone(), result.clone());
        let ModelMessageRef::Tool {
            tool_call_id,
            content,
        } = message.as_ref()
        else {
            panic!("tool result did not construct a tool message");
        };
        assert_eq!(tool_call_id, &call_id);
        assert_eq!(content, &result);
    }

    #[test]
    fn model_messages_and_content_clone_preserve_read_projections() {
        let content = ModelAssistantContent::text(Arc::from("assistant text")).unwrap();
        let content_clone = content.clone();
        assert_eq!(content.as_ref(), content_clone.as_ref());
        assert!(matches!(
            content_clone.as_ref(),
            ModelAssistantContentRef::Text("assistant text")
        ));

        let messages = [
            ModelMessage::unstamped_user_text(Arc::from("user text")).unwrap(),
            ModelMessage::assistant(Arc::from([content])).unwrap(),
            ModelMessage::tool_result(tool_call_id("call-clone"), tool_result()),
        ];
        for message in messages {
            let clone = message.clone();
            assert_eq!(message.as_ref(), clone.as_ref());
            match (message.as_ref(), clone.as_ref()) {
                (
                    ModelMessageRef::User {
                        content: original_content,
                    },
                    ModelMessageRef::User {
                        content: cloned_content,
                    },
                ) => {
                    assert_eq!(original_content.len(), cloned_content.len());
                    for (original, cloned) in original_content.iter().zip(cloned_content) {
                        assert_eq!(original.as_text(), cloned.as_text());
                    }
                }
                (
                    ModelMessageRef::Assistant {
                        content: original_content,
                    },
                    ModelMessageRef::Assistant {
                        content: cloned_content,
                    },
                ) => {
                    assert_eq!(original_content.len(), cloned_content.len());
                    for (original, cloned) in original_content.iter().zip(cloned_content) {
                        assert_eq!(original.as_ref(), cloned.as_ref());
                    }
                }
                (
                    ModelMessageRef::Tool {
                        tool_call_id: original_id,
                        content: original_content,
                    },
                    ModelMessageRef::Tool {
                        tool_call_id: cloned_id,
                        content: cloned_content,
                    },
                ) => {
                    assert_eq!(original_id, cloned_id);
                    assert_eq!(original_content, cloned_content);
                }
                _ => panic!("cloning changed the message role"),
            }
        }
    }

    #[test]
    fn model_transcript_debug_is_redacted() {
        let user = ModelMessage::canonical_user(canonical_user_with_stamp());
        let reasoning = ModelAssistantContent::reasoning(reasoning_content());
        let text = ModelAssistantContent::text(Arc::from("assistant secret")).unwrap();
        let call = ModelAssistantContent::tool_call(
            tool_call_id("call-secret"),
            tool_name("secret_tool"),
            tool_arguments(),
        );
        let assistant =
            ModelMessage::assistant(Arc::from([reasoning.clone(), text.clone(), call.clone()]))
                .unwrap();
        let tool = ModelMessage::tool_result(tool_call_id("tool-call-secret"), tool_result());
        let error = ModelAssistantContent::text(Arc::from("error secret\r")).unwrap_err();

        let debug = [
            format!("{user:?}"),
            format!("{:?}", user.as_ref()),
            format!("{assistant:?}"),
            format!("{:?}", assistant.as_ref()),
            format!("{tool:?}"),
            format!("{:?}", tool.as_ref()),
            format!("{reasoning:?}"),
            format!("{:?}", reasoning.as_ref()),
            format!("{text:?}"),
            format!("{:?}", text.as_ref()),
            format!("{call:?}"),
            format!("{:?}", call.as_ref()),
            format!("{error:?}"),
        ];
        for value in debug {
            for secret in [
                "user secret",
                "review",
                "reasoning artifact",
                "reasoning summary",
                "encrypted artifact",
                "reasoning signature",
                "provider-item",
                "assistant secret",
                "argument secret",
                "call-secret",
                "secret_tool",
                "tool-call-secret",
                "tool result secret",
                "error secret",
            ] {
                assert!(!value.contains(secret));
            }
        }
    }

    #[test]
    fn canonical_message_enforces_parts_aggregate_and_stamp_indices() {
        let body = MessageContent::text("body").unwrap();
        let contribution = MessageContent::text("contribution").unwrap();
        let message = MessageRecord::new(vec![body, contribution]).unwrap();
        let stamp = PromptContributionStamp::new(
            1,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("review").unwrap(),
            },
        )
        .unwrap();
        let canonical = CanonicalUserMessage::new(message, vec![stamp]).unwrap();
        assert_eq!(canonical.message().content().len(), 2);
        assert_eq!(canonical.contribution_stamps()[0].content_part_index(), 1);

        let duplicate = canonical.contribution_stamps()[0].clone();
        assert_eq!(
            CanonicalUserMessage::new(
                canonical.message.clone(),
                vec![duplicate.clone(), duplicate]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );

        let out_of_range = PromptContributionStamp::new(
            2,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("other").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            CanonicalUserMessage::new(canonical.message.clone(), vec![out_of_range]),
            Err(PromptValueError::InvalidContributionStamp)
        );

        let same_origin =
            PromptContributionStamp::new(0, canonical.contribution_stamps()[0].origin().clone())
                .unwrap();
        assert_eq!(
            CanonicalUserMessage::new(
                canonical.message.clone(),
                vec![same_origin, canonical.contribution_stamps()[0].clone()]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );
    }

    #[test]
    fn replay_reconstruction_preserves_text_after_stamp_degradation() {
        let message = MessageRecord::reconstruct(vec![
            MessageContent::reconstruct_text("body").unwrap(),
            MessageContent::reconstruct_text("unstamped contribution").unwrap(),
            MessageContent::reconstruct_text("valid contribution").unwrap(),
        ])
        .unwrap();
        let surviving_stamp = PromptContributionStamp::reconstruct(
            2,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("review").unwrap(),
            },
        )
        .unwrap();

        assert_eq!(
            CanonicalUserMessage::new(message.clone(), vec![surviving_stamp.clone()]),
            Err(PromptValueError::InvalidContributionStamp)
        );
        let out_of_range = PromptContributionStamp::reconstruct(
            3,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("other").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            CanonicalUserMessage::reconstruct(message.clone(), vec![out_of_range]),
            Err(PromptValueError::InvalidContributionStamp)
        );
        let earlier_stamp = PromptContributionStamp::reconstruct(
            1,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("other").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            CanonicalUserMessage::reconstruct(
                message.clone(),
                vec![surviving_stamp.clone(), earlier_stamp]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );
        let duplicate_origin =
            PromptContributionStamp::reconstruct(1, surviving_stamp.origin().clone()).unwrap();
        assert_eq!(
            CanonicalUserMessage::reconstruct(
                message.clone(),
                vec![duplicate_origin, surviving_stamp.clone()]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );

        let reconstructed =
            CanonicalUserMessage::reconstruct(message, vec![surviving_stamp]).unwrap();
        assert_eq!(reconstructed.message().content().len(), 3);
        assert_eq!(
            reconstructed.message().content()[1].as_text(),
            "unstamped contribution"
        );
        assert_eq!(reconstructed.contribution_stamps().len(), 1);
    }

    #[test]
    fn message_record_enforces_part_and_aggregate_limits() {
        let limits = ProtocolLimits::v1_0().prompt;
        assert!(MessageRecord::new(Vec::new()).is_err());
        assert!(
            MessageContent::text("x".repeat(limits.max_message_part_bytes as usize + 1)).is_err()
        );
        assert!(
            MessageRecord::new(
                (0..5)
                    .map(|_| MessageContent::text("x".repeat(120_000)).unwrap())
                    .collect()
            )
            .is_err()
        );

        let normalized = MessageContent::text("a\r\nb\rc").unwrap();
        assert_eq!(normalized.as_text(), "a\nb\nc");

        let boundary_parts = (0..limits.max_user_message_parts)
            .map(|_| MessageContent::text("x").unwrap())
            .collect::<Vec<_>>();
        assert!(MessageRecord::new(boundary_parts.clone()).is_ok());
        let mut oversized_parts = boundary_parts;
        oversized_parts.push(MessageContent::text("x").unwrap());
        assert!(MessageRecord::new(oversized_parts).is_err());

        let aggregate_boundary = (0..4)
            .map(|_| MessageContent::text("x".repeat(131_072)).unwrap())
            .collect::<Vec<_>>();
        assert!(MessageRecord::new(aggregate_boundary.clone()).is_ok());
        let mut aggregate_oversized = aggregate_boundary;
        aggregate_oversized.push(MessageContent::text("x").unwrap());
        assert!(MessageRecord::new(aggregate_oversized).is_err());
    }

    #[test]
    fn workspace_relative_path_rejects_unsafe_location_before_prompt_stamping() {
        assert!(WorkspaceRelativePath::from_str("src/\u{001b}[31m").is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_run_assembly_orders_static_context_before_sanitized_conversation() {
        let source = Arc::new(MutablePromptSource::new(vec![user_definition(
            "session-context",
            "session-context",
            PromptRole::User,
            "static session context",
        )]));
        let source_adapter: Arc<dyn PromptSourceAdapter> = source;
        let service = PromptService::new(
            Arc::from("required system policy"),
            Some(Arc::from("base system policy")),
            vec![source_adapter],
            Vec::new(),
        )
        .unwrap();
        let resources = service.initialize().await.unwrap();
        let tool_set = ToolSet::empty();
        let skill_view = SkillView::empty();
        let model = TurnModelSnapshot::test_fixture(None);
        let model_ref = model.turn_model_ref();
        let set = service
            .for_turn(turn_context_with_model(
                resources,
                Vec::new(),
                vec!["session-context".parse().unwrap()],
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
                model,
            ))
            .unwrap();

        let user = set
            .compose_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("live user input").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut live = LiveSessionState::new(session_id(), []);
        live.apply_user_message(
            StoredUserMessage::reconstruct(
                "itm_00000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                UserMessageSource::Input,
                user,
            ),
            "trn_00000000000000000000000000000001"
                .parse::<TurnId>()
                .unwrap(),
            "2026-08-08T00:00:00.000Z".parse::<Timestamp>().unwrap(),
        )
        .unwrap();
        let views = live.capture_conversation_views().unwrap();

        let output_contract = OutputContract::NoToolCalls;
        let assembled = set
            .assemble(PromptAssemblyInput::agent_run(
                views.conversation(),
                Some(&output_contract),
            ))
            .unwrap();

        assert_eq!(
            assembled
                .system()
                .iter()
                .map(PromptSection::text)
                .collect::<Vec<_>>(),
            ["required system policy", "base system policy"]
        );
        assert_eq!(assembled.messages().len(), 2);
        for (message, expected) in assembled
            .messages()
            .iter()
            .zip(["static session context", "live user input"])
        {
            match message.as_ref() {
                ModelMessageRef::User { content } => {
                    assert_eq!(content.len(), 1);
                    assert_eq!(content[0].as_text(), expected);
                }
                _ => panic!("assembly changed the expected user-message order"),
            }
        }
        let proof = assembled.assembly_proof();
        assert_eq!(proof.purpose(), ModelCallPurpose::AgentRun);
        assert!(proof.turn_model().is_exact(&model_ref));
        assert_eq!(proof.source_revision(), views.conversation().revision());
        assert_eq!(proof.output_contract(), Some(&output_contract));

        let agent_basis = set.agent_run_compaction_assembly_basis().unwrap();
        let stable_tokens = views.compaction_source().units()[0]
            .messages()
            .iter()
            .map(|message| {
                message
                    .compaction_estimated_tokens(agent_basis.estimator())
                    .unwrap()
            })
            .sum::<u64>();
        let exact_agent_bytes = canonical_model_context_bytes(
            &set.profile.system,
            assembled.messages(),
            set.tools.specs(),
            None,
        )
        .unwrap();
        let exact_agent_tokens = agent_basis
            .estimator()
            .checked_estimate_utf8_bytes(u64::try_from(exact_agent_bytes).unwrap())
            .unwrap();
        assert!(agent_basis.fixed_input_tokens() + stable_tokens >= exact_agent_tokens);

        let summary_basis = set.compaction_summary_assembly_basis().unwrap();
        assert_eq!(summary_basis.system_sections().len(), 1);
        assert_eq!(
            summary_basis.system_sections()[0].text(),
            "required system policy"
        );
        assert_eq!(
            summary_basis.output_contract(),
            &OutputContract::NoToolCalls
        );
        assert_eq!(summary_basis.estimator(), agent_basis.estimator());
    }

    #[test]
    fn structured_contract_counts_type_nullable_name_and_canonical_schema_bytes() {
        let model =
            TurnModelSnapshot::test_fixture_with_structured(None, NonZeroU32::new(65_536).unwrap());
        let described_schema: BoundedJsonSchema =
            r#"{"type":"object","description":"SECRET description"}"#
                .parse()
                .unwrap();
        let bare_schema: BoundedJsonSchema = r#"{"type":"object"}"#.parse().unwrap();
        let named = OutputContract::Structured(
            StructuredOutputContract::new(&model, Some("weather"), described_schema.clone())
                .unwrap(),
        );
        let unnamed = OutputContract::Structured(
            StructuredOutputContract::new(&model, None, described_schema.clone()).unwrap(),
        );
        let sparse = OutputContract::Structured(
            StructuredOutputContract::new(&model, None, bare_schema.clone()).unwrap(),
        );

        let none = canonical_model_context_bytes(&[], &[], &[], None).unwrap();
        let no_tool_calls =
            canonical_model_context_bytes(&[], &[], &[], Some(&OutputContract::NoToolCalls))
                .unwrap();
        assert_eq!(
            none,
            r#"{"system":[],"messages":[],"tools":[],"outputContract":null}"#.len()
        );
        assert_eq!(
            no_tool_calls,
            r#"{"system":[],"messages":[],"tools":[],"outputContract":"no_tool_calls"}"#.len()
        );

        let named_bytes = canonical_model_context_bytes(&[], &[], &[], Some(&named)).unwrap();
        let unnamed_bytes = canonical_model_context_bytes(&[], &[], &[], Some(&unnamed)).unwrap();
        let sparse_bytes = canonical_model_context_bytes(&[], &[], &[], Some(&sparse)).unwrap();

        // The nullable name is counted exactly: an escaped JSON string when present, "null" when
        // absent, never folded into a fixed string.
        assert_eq!(
            named_bytes,
            unnamed_bytes + canonical_json_string_len("weather").unwrap() - "null".len()
        );
        // The canonical schema bytes are counted raw and exactly.
        assert_eq!(
            unnamed_bytes,
            sparse_bytes + described_schema.canonical_bytes().len()
                - bare_schema.canonical_bytes().len()
        );
        // Exact envelope accounting for the whole structured fact.
        let envelope = r#"{"system":[],"messages":[],"tools":[],"outputContract":"#;
        assert_eq!(
            named_bytes,
            envelope.len()
                + r#"{"type":"structured","name":"#.len()
                + canonical_json_string_len("weather").unwrap()
                + r#","schema":"#.len()
                + described_schema.canonical_bytes().len()
                + 1
                + 1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_run_assembly_proves_the_exact_structured_contract() {
        let model =
            TurnModelSnapshot::test_fixture_with_structured(None, NonZeroU32::new(65_536).unwrap());
        let contract = StructuredOutputContract::new(
            &model,
            Some("weather"),
            r#"{"type":"object","description":"SECRET description","properties":{"summary":{"type":"string"}}}"#
                .parse()
                .unwrap(),
        )
        .unwrap();
        let service = PromptService::new(
            Arc::from("required system policy"),
            None,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let resources = service.initialize().await.unwrap();
        let tool_set = ToolSet::empty();
        let skill_view = SkillView::empty();
        let set = service
            .for_turn(turn_context_with_model(
                resources,
                Vec::new(),
                Vec::new(),
                empty_workspace(session_id()),
                &tool_set,
                &skill_view,
                Arc::clone(&model),
            ))
            .unwrap();

        let user = set
            .compose_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("live user input").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut live = LiveSessionState::new(session_id(), []);
        live.apply_user_message(
            StoredUserMessage::reconstruct(
                "itm_00000000000000000000000000000001"
                    .parse::<ItemId>()
                    .unwrap(),
                UserMessageSource::Input,
                user,
            ),
            "trn_00000000000000000000000000000001"
                .parse::<TurnId>()
                .unwrap(),
            "2026-08-08T00:00:00.000Z".parse::<Timestamp>().unwrap(),
        )
        .unwrap();
        let views = live.capture_conversation_views().unwrap();

        let output_contract = OutputContract::Structured(contract.clone());
        let assembled = set
            .assemble(PromptAssemblyInput::agent_run(
                views.conversation(),
                Some(&output_contract),
            ))
            .unwrap();

        assert_eq!(
            assembled.output_contract(),
            Some(&OutputContract::Structured(contract.clone()))
        );
        assert_eq!(
            assembled.assembly_proof().output_contract(),
            Some(&OutputContract::Structured(contract.clone()))
        );
        assert!(
            assembled
                .assembly_proof()
                .turn_model()
                .is_exact(&model.turn_model_ref())
        );
        let estimated = canonical_model_context_bytes(
            assembled.system(),
            assembled.messages(),
            set.tools.specs(),
            assembled.output_contract(),
        )
        .unwrap();
        assert!(estimated > 0);

        for debug in [format!("{assembled:?}"), format!("{contract:?}")] {
            assert!(!debug.contains("SECRET description"));
            assert!(!debug.contains("weather"));
        }
    }
}
