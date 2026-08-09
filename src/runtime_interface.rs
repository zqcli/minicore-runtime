use std::collections::BTreeSet;
use std::fmt;

use thiserror::Error;

use crate::agent_session_lifecycle::{AgentRevisionRef, SessionModelConfig};
use crate::prompt::{PromptIntent, SessionPromptSelection};
use crate::skills::SkillId;
use crate::wire::lexical::{normalize_newlines, validate_safe_text, validate_stable_symbolic_key};
use crate::wire::{
    AgentId, CommandId, Duration, ItemId, Money, ProtocolLimits, RequestId,
    SessionDefinitionRevision, SessionId, SessionMetadataRevision, Timestamp, TurnId,
};
use crate::workspace::{WorkspaceDefinitionInput, WorkspaceDefinitionSummaryView};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum RuntimeCapability {
    StateEvents,
    ProgressEvents,
    RuntimeSnapshot,
    SessionSnapshot,
    PagedQueries,
    CommandCatalog,
    InteractionResolution,
    SessionFork,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCapabilities {
    values: Vec<RuntimeCapability>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RuntimeCapabilitiesError {
    #[error("runtime capability set contains a duplicate value")]
    DuplicateCapability,
    #[error("runtime capability is not declared by protocol v1.0")]
    UnsupportedCapability,
}

impl RuntimeCapabilities {
    pub fn empty() -> Self {
        Self { values: Vec::new() }
    }

    #[cfg(test)]
    pub(crate) fn all_v1() -> Self {
        Self {
            values: v1_runtime_capabilities().to_vec(),
        }
    }

    pub fn for_v1(values: Vec<RuntimeCapability>) -> Result<Self, RuntimeCapabilitiesError> {
        let selected = values.iter().copied().collect::<BTreeSet<_>>();
        if selected.len() != values.len() {
            return Err(RuntimeCapabilitiesError::DuplicateCapability);
        }
        if selected
            .iter()
            .any(|capability| !v1_runtime_capabilities().contains(capability))
        {
            return Err(RuntimeCapabilitiesError::UnsupportedCapability);
        }
        Ok(Self {
            values: v1_runtime_capabilities()
                .iter()
                .copied()
                .filter(|capability| selected.contains(capability))
                .collect(),
        })
    }

    pub fn values(&self) -> &[RuntimeCapability] {
        &self.values
    }
}

const fn v1_runtime_capabilities() -> &'static [RuntimeCapability; 8] {
    &[
        RuntimeCapability::StateEvents,
        RuntimeCapability::ProgressEvents,
        RuntimeCapability::RuntimeSnapshot,
        RuntimeCapability::SessionSnapshot,
        RuntimeCapability::PagedQueries,
        RuntimeCapability::CommandCatalog,
        RuntimeCapability::InteractionResolution,
        RuntimeCapability::SessionFork,
    ]
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeLifecycleCommand {
    ReloadSharedResources,
}

pub type PromptIntentInput = PromptIntent;

#[derive(Clone, Eq, PartialEq)]
pub struct NewSessionDefinition {
    workspace: WorkspaceDefinitionInput,
    model: SessionModelConfig,
    prompts: SessionPromptSelection,
}

impl NewSessionDefinition {
    pub const fn new(
        workspace: WorkspaceDefinitionInput,
        model: SessionModelConfig,
        prompts: SessionPromptSelection,
    ) -> Self {
        Self {
            workspace,
            model,
            prompts,
        }
    }

    pub const fn workspace(&self) -> &WorkspaceDefinitionInput {
        &self.workspace
    }

    pub const fn model(&self) -> &SessionModelConfig {
        &self.model
    }

    pub const fn prompts(&self) -> &SessionPromptSelection {
        &self.prompts
    }
}

impl fmt::Debug for NewSessionDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewSessionDefinition")
            .field("workspace", &self.workspace)
            .field("model", &self.model)
            .field("prompts", &self.prompts)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NewSessionMetadataError {
    #[error("session name must be non-empty")]
    EmptyName,
    #[error("session metadata exceeds its selected text limit")]
    TextTooLong,
    #[error("session metadata contains an unsafe control character")]
    UnsafeText,
}

#[derive(Clone, Eq, PartialEq)]
pub struct NewSessionMetadata {
    name: Option<Box<str>>,
    description: Option<Box<str>>,
}

impl NewSessionMetadata {
    pub fn new<N, D>(
        name: Option<N>,
        description: Option<D>,
    ) -> Result<Self, NewSessionMetadataError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        Self::new_with_limits(name, description, ProtocolLimits::v1_0())
    }

    pub(crate) fn new_with_limits<N, D>(
        name: Option<N>,
        description: Option<D>,
        limits: ProtocolLimits,
    ) -> Result<Self, NewSessionMetadataError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        let name =
            normalize_metadata_text(name, usize::from(limits.text.max_display_name_bytes), true)?;
        let description = normalize_metadata_text(
            description,
            usize::try_from(limits.text.max_description_bytes).unwrap_or(usize::MAX),
            false,
        )?;
        Ok(Self { name, description })
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

impl fmt::Debug for NewSessionMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewSessionMetadata")
            .field("name_present", &self.name.is_some())
            .field("description_present", &self.description.is_some())
            .finish()
    }
}

fn normalize_metadata_text<T>(
    value: Option<T>,
    maximum: usize,
    require_non_empty: bool,
) -> Result<Option<Box<str>>, NewSessionMetadataError>
where
    T: AsRef<str>,
{
    let Some(value) = value else {
        return Ok(None);
    };
    let normalized = normalize_newlines(value.as_ref());
    if require_non_empty && normalized.is_empty() {
        return Err(NewSessionMetadataError::EmptyName);
    }
    if normalized.len() > maximum {
        return Err(NewSessionMetadataError::TextTooLong);
    }
    validate_safe_text(&normalized, maximum, !require_non_empty)
        .map_err(|_| NewSessionMetadataError::UnsafeText)?;
    Ok(Some(normalized.into()))
}

#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeCommand {
    Runtime(RuntimeLifecycleCommand),
    Session(SessionCommand),
    Turn(TurnCommand),
}

impl fmt::Debug for RuntimeCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(command) => formatter.debug_tuple("Runtime").field(command).finish(),
            Self::Session(command) => formatter.debug_tuple("Session").field(command).finish(),
            Self::Turn(command) => formatter.debug_tuple("Turn").field(command).finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionCommand {
    Create {
        agent_id: AgentId,
        definition: Box<NewSessionDefinition>,
        metadata: NewSessionMetadata,
    },
    Load {
        session_id: SessionId,
    },
    Unload {
        session_id: SessionId,
    },
}

#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum TurnCommand {
    Submit {
        session_id: SessionId,
        intent: PromptIntentInput,
    },
    Steer {
        session_id: SessionId,
        expected_turn_id: TurnId,
        intent: PromptIntentInput,
    },
    FollowUp {
        session_id: SessionId,
        intent: PromptIntentInput,
    },
    CancelQueuedMessage {
        session_id: SessionId,
        target_command_id: CommandId,
    },
    Cancel {
        session_id: SessionId,
        target: PublicCancelTarget,
    },
}

impl fmt::Debug for TurnCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Submit { session_id, .. } => formatter
                .debug_struct("Submit")
                .field("session_id", session_id)
                .field("intent", &"<redacted>")
                .finish(),
            Self::Steer {
                session_id,
                expected_turn_id,
                ..
            } => formatter
                .debug_struct("Steer")
                .field("session_id", session_id)
                .field("expected_turn_id", expected_turn_id)
                .field("intent", &"<redacted>")
                .finish(),
            Self::FollowUp { session_id, .. } => formatter
                .debug_struct("FollowUp")
                .field("session_id", session_id)
                .field("intent", &"<redacted>")
                .finish(),
            Self::CancelQueuedMessage {
                session_id,
                target_command_id,
            } => formatter
                .debug_struct("CancelQueuedMessage")
                .field("session_id", session_id)
                .field("target_command_id", target_command_id)
                .finish(),
            Self::Cancel { session_id, target } => formatter
                .debug_struct("Cancel")
                .field("session_id", session_id)
                .field("target", target)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PublicCancelTarget {
    Submit(CommandId),
    Turn(TurnId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    command_id: CommandId,
    command: RuntimeCommand,
}

impl CommandRequest {
    pub const fn new(command_id: CommandId, command: RuntimeCommand) -> Self {
        Self {
            command_id,
            command,
        }
    }

    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub const fn command(&self) -> &RuntimeCommand {
        &self.command
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CommandResponse {
    command_id: CommandId,
    completion: CommandCompletion,
}

impl CommandResponse {
    pub fn new(
        command_id: CommandId,
        completion: CommandCompletion,
    ) -> Result<Self, CommandValueError> {
        if let CommandCompletion::Completed { outcome, output } = &completion {
            let expects_output = matches!(outcome, CommandOutcome::CommandOutput);
            if expects_output != output.is_some() {
                return Err(CommandValueError::InvalidCompletion);
            }
        }
        Ok(Self {
            command_id,
            completion,
        })
    }

    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub const fn completion(&self) -> &CommandCompletion {
        &self.completion
    }
}

impl fmt::Debug for CommandResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandResponse")
            .field("command_id", &self.command_id)
            .field("completion", &self.completion)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum CommandCompletion {
    Completed {
        outcome: CommandOutcome,
        output: Option<CommandOutput>,
    },
    Rejected(CommandError),
}

impl fmt::Debug for CommandCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed { outcome, output } => formatter
                .debug_struct("Completed")
                .field("outcome", outcome)
                .field("output", &output.as_ref().map(|_| "<redacted>"))
                .finish(),
            Self::Rejected(error) => formatter.debug_tuple("Rejected").field(error).finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CommandOutcome {
    TurnStarted { turn_id: TurnId },
    SteerQueued { turn_id: TurnId },
    FollowUpQueued,
    QueuedMessageCancelled,
    CommandOutput,
}

#[derive(Clone, Eq, PartialEq)]
pub struct CommandOutput(Box<str>);

impl CommandOutput {
    pub fn new(text: impl AsRef<str>) -> Result<Self, CommandValueError> {
        Self::new_with_maximum(
            text,
            ProtocolLimits::v1_0().text.max_command_output_bytes as usize,
        )
    }

    pub(crate) fn new_with_maximum(
        text: impl AsRef<str>,
        maximum: usize,
    ) -> Result<Self, CommandValueError> {
        let text = text.as_ref();
        validate_command_output(text, maximum)?;
        Ok(Self(text.into()))
    }

    pub fn text(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CommandError {
    code: CommandErrorCode,
    message: Box<str>,
    retry: RetryAdvice,
    subject: Option<PublicSubject>,
}

impl CommandError {
    pub fn new(
        code: CommandErrorCode,
        message: impl AsRef<str>,
        retry: RetryAdvice,
        subject: Option<PublicSubject>,
    ) -> Result<Self, CommandValueError> {
        Self::new_with_maximum_message(
            code,
            message,
            retry,
            subject,
            ProtocolLimits::v1_0().text.max_diagnostic_message_bytes as usize,
        )
    }

    pub(crate) fn new_with_maximum_message(
        code: CommandErrorCode,
        message: impl AsRef<str>,
        retry: RetryAdvice,
        subject: Option<PublicSubject>,
        maximum: usize,
    ) -> Result<Self, CommandValueError> {
        let message = message.as_ref();
        validate_command_error_message(message, maximum)?;
        validate_command_error_contract(code, retry)?;
        Ok(Self {
            code,
            message: message.into(),
            retry,
            subject,
        })
    }

    pub const fn code(&self) -> CommandErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn retry(&self) -> RetryAdvice {
        self.retry
    }

    pub const fn subject(&self) -> Option<&PublicSubject> {
        self.subject.as_ref()
    }
}

pub(crate) fn validate_command_error_contract(
    code: CommandErrorCode,
    retry: RetryAdvice,
) -> Result<(), CommandValueError> {
    let valid = match code {
        CommandErrorCode::InvalidArgument
        | CommandErrorCode::CommandConflict
        | CommandErrorCode::AgentDeleted
        | CommandErrorCode::SessionDeleted
        | CommandErrorCode::InteractionFamilyMismatch => retry == RetryAdvice::DoNotRetry,
        CommandErrorCode::NotFound
        | CommandErrorCode::StaleRevision
        | CommandErrorCode::SessionBusy
        | CommandErrorCode::QueuedMessageNotQueued
        | CommandErrorCode::SubmitNotCancellable
        | CommandErrorCode::ExpectedTurnMismatch
        | CommandErrorCode::TurnNotRunning
        | CommandErrorCode::TurnCancelling
        | CommandErrorCode::TurnTerminal
        | CommandErrorCode::InteractionNotFound
        | CommandErrorCode::InteractionAlreadyResolved
        | CommandErrorCode::InvalidForkAnchor => retry == RetryAdvice::RefreshAndRetry,
        CommandErrorCode::AgentDisabled
        | CommandErrorCode::SessionArchived
        | CommandErrorCode::SessionNotLoaded
        | CommandErrorCode::ReloadValidationFailed
        | CommandErrorCode::Unauthorized
        | CommandErrorCode::DurableStateCorrupt
        | CommandErrorCode::DurableStateTooLarge => retry == RetryAdvice::UserActionRequired,
        CommandErrorCode::SessionNotReady => {
            retry == RetryAdvice::UserActionRequired || retry.is_backoff()
        }
        CommandErrorCode::Unavailable => retry == RetryAdvice::DoNotRetry || retry.is_backoff(),
        CommandErrorCode::IngressLaneFull { .. } | CommandErrorCode::RuntimeClosing => {
            retry.is_backoff()
        }
    };
    if valid {
        Ok(())
    } else {
        Err(CommandValueError::InvalidErrorContract)
    }
}

pub(crate) fn validate_command_output(text: &str, maximum: usize) -> Result<(), CommandValueError> {
    validate_safe_text(text, maximum, false).map_err(|_| CommandValueError::InvalidOutput)
}

pub(crate) fn validate_command_error_message(
    message: &str,
    maximum: usize,
) -> Result<(), CommandValueError> {
    validate_safe_text(message, maximum, false).map_err(|_| CommandValueError::InvalidErrorMessage)
}

impl fmt::Debug for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandError")
            .field("code", &self.code)
            .field("message", &"<redacted>")
            .field("retry", &self.retry)
            .field("subject", &self.subject)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum CommandErrorCode {
    InvalidArgument,
    NotFound,
    CommandConflict,
    StaleRevision,
    AgentDisabled,
    AgentDeleted,
    SessionArchived,
    SessionDeleted,
    SessionNotLoaded,
    SessionNotReady,
    SessionBusy,
    ReloadValidationFailed,
    IngressLaneFull { lane: PublicIngressLane },
    QueuedMessageNotQueued,
    SubmitNotCancellable,
    ExpectedTurnMismatch,
    TurnNotRunning,
    TurnCancelling,
    TurnTerminal,
    InteractionNotFound,
    InteractionAlreadyResolved,
    InteractionFamilyMismatch,
    InvalidForkAnchor,
    Unauthorized,
    Unavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeClosing,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PublicIngressLane {
    TurnAdmission,
    Steer,
    FollowUp,
    InteractionControl,
    ToolControl,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum RetryAdvice {
    DoNotRetry,
    RefreshAndRetry,
    RetryWithBackoff { retry_after: Option<Duration> },
    UserActionRequired,
}

impl RetryAdvice {
    const fn is_backoff(self) -> bool {
        matches!(self, Self::RetryWithBackoff { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PublicSubject {
    Runtime,
    Command(CommandId),
    Agent(AgentId),
    Session(SessionId),
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Item {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
    },
    Interaction {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
        request_id: RequestId,
    },
    Skill(SkillId),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CommandValueError {
    #[error("command completion outcome and output do not match")]
    InvalidCompletion,
    #[error("command error code and retry advice do not form a valid machine contract")]
    InvalidErrorContract,
    #[error("command output is empty, unsafe, or exceeds its limit")]
    InvalidOutput,
    #[error("command error message is empty, unsafe, or exceeds its limit")]
    InvalidErrorMessage,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum RuntimeReadQuery {
    GetCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeQuery {
    Runtime(RuntimeReadQuery),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResponse {
    data: QueryResult,
}

impl QueryResponse {
    pub const fn new(data: QueryResult) -> Self {
        Self { data }
    }

    pub const fn data(&self) -> &QueryResult {
        &self.data
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum QueryResult {
    Runtime(RuntimeQueryResult),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeQueryResult {
    Capabilities(RuntimeCapabilities),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SnapshotRequest {
    Runtime,
    Session { session_id: SessionId },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ObservationValueError {
    #[error("observation text is empty")]
    EmptyText,
    #[error("observation text exceeds its selected limit")]
    TextTooLong,
    #[error("observation text contains an unsafe control character")]
    UnsafeText,
    #[error("observation collection exceeds its selected limit")]
    TooManyValues,
    #[error("observation collection contains a duplicate value")]
    DuplicateValue,
    #[error("session snapshot identity is inconsistent")]
    SessionIdentityMismatch,
    #[error("usage currencies must be unique and canonically ordered")]
    InvalidCurrencyOrder,
    #[error("degraded recording state requires a public diagnostic")]
    DegradedWithoutDiagnostic,
    #[error("loaded session readiness and execution are inconsistent")]
    InconsistentLoadedSessionState,
    #[error("state event route, kind, snapshot, and detail are inconsistent")]
    InconsistentStateEvent,
    #[error("observation value belongs to a later protocol slice")]
    NonMinimalObservation,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionMetadataView {
    revision: SessionMetadataRevision,
    name: Option<Box<str>>,
    description: Option<Box<str>>,
    updated_at: Timestamp,
}

impl SessionMetadataView {
    pub fn new<N, D>(
        revision: SessionMetadataRevision,
        name: Option<N>,
        description: Option<D>,
        updated_at: Timestamp,
    ) -> Result<Self, ObservationValueError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        Self::new_with_limits(
            revision,
            name,
            description,
            updated_at,
            ProtocolLimits::v1_0(),
        )
    }

    pub(crate) fn new_with_limits<N, D>(
        revision: SessionMetadataRevision,
        name: Option<N>,
        description: Option<D>,
        updated_at: Timestamp,
        limits: ProtocolLimits,
    ) -> Result<Self, ObservationValueError>
    where
        N: AsRef<str>,
        D: AsRef<str>,
    {
        let name = normalize_observation_text(
            name,
            usize::from(limits.text.max_display_name_bytes),
            true,
        )?;
        let description = normalize_observation_text(
            description,
            usize::try_from(limits.text.max_description_bytes).unwrap_or(usize::MAX),
            false,
        )?;
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

impl fmt::Debug for SessionMetadataView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionMetadataView")
            .field("revision", &self.revision)
            .field("name_present", &self.name.is_some())
            .field("description_present", &self.description.is_some())
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

fn normalize_observation_text<T>(
    value: Option<T>,
    maximum: usize,
    require_non_empty: bool,
) -> Result<Option<Box<str>>, ObservationValueError>
where
    T: AsRef<str>,
{
    let Some(value) = value else {
        return Ok(None);
    };
    let normalized = normalize_newlines(value.as_ref());
    if require_non_empty && normalized.is_empty() {
        return Err(ObservationValueError::EmptyText);
    }
    if normalized.len() > maximum {
        return Err(ObservationValueError::TextTooLong);
    }
    validate_safe_text(&normalized, maximum, !require_non_empty)
        .map_err(|_| ObservationValueError::UnsafeText)?;
    Ok(Some(normalized.into()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDefinitionSummary {
    session_id: SessionId,
    revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    workspace: WorkspaceDefinitionSummaryView,
    model: SessionModelConfig,
    prompts: SessionPromptSelection,
    created_at: Timestamp,
}

impl SessionDefinitionSummary {
    pub const fn new(
        session_id: SessionId,
        revision: SessionDefinitionRevision,
        agent: AgentRevisionRef,
        workspace: WorkspaceDefinitionSummaryView,
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

    pub const fn workspace(&self) -> &WorkspaceDefinitionSummaryView {
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

macro_rules! diagnostic_view {
    ($name:ident) => {
        #[derive(Clone, Eq, PartialEq)]
        pub struct $name {
            code: Box<str>,
            message: Box<str>,
        }

        impl $name {
            pub(crate) fn new_with_limits(
                code: impl AsRef<str>,
                message: impl AsRef<str>,
                limits: ProtocolLimits,
            ) -> Result<Self, ObservationValueError> {
                let code = code.as_ref();
                let message = message.as_ref();
                let code_maximum = usize::from(limits.text.max_diagnostic_code_bytes);
                let message_maximum = usize::from(limits.text.max_diagnostic_message_bytes);
                if message.is_empty() {
                    return Err(ObservationValueError::EmptyText);
                }
                validate_stable_symbolic_key(code, code_maximum, false)
                    .map_err(|_| ObservationValueError::UnsafeText)?;
                if message.len() > message_maximum {
                    return Err(ObservationValueError::TextTooLong);
                }
                validate_safe_text(message, message_maximum, false)
                    .map_err(|_| ObservationValueError::UnsafeText)?;
                Ok(Self {
                    code: code.into(),
                    message: message.into(),
                })
            }

            pub fn code(&self) -> &str {
                &self.code
            }

            pub fn message(&self) -> &str {
                &self.message
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("code", &self.code)
                    .field("message", &"<redacted>")
                    .finish()
            }
        }
    };
}

diagnostic_view!(RuntimeDiagnosticView);
diagnostic_view!(SessionDiagnosticView);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionUsageView {
    model_calls: u64,
    compaction_calls: u64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reported_costs: Vec<Money>,
}

impl SessionUsageView {
    #[allow(
        clippy::too_many_arguments,
        reason = "fields are the exact public usage contract"
    )]
    pub(crate) fn new(
        model_calls: u64,
        compaction_calls: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        reported_costs: Vec<Money>,
    ) -> Result<Self, ObservationValueError> {
        if model_calls > 1_000_000
            || compaction_calls > 1_000_000
            || reported_costs.len() > 8
            || reported_costs
                .windows(2)
                .any(|pair| pair[0].currency() >= pair[1].currency())
        {
            return Err(ObservationValueError::InvalidCurrencyOrder);
        }
        Ok(Self {
            model_calls,
            compaction_calls,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reported_costs,
        })
    }

    pub const fn model_calls(&self) -> u64 {
        self.model_calls
    }

    pub const fn compaction_calls(&self) -> u64 {
        self.compaction_calls
    }

    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    pub const fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }

    pub const fn cache_read_tokens(&self) -> Option<u64> {
        self.cache_read_tokens
    }

    pub const fn cache_write_tokens(&self) -> Option<u64> {
        self.cache_write_tokens
    }

    pub fn reported_costs(&self) -> &[Money] {
        &self.reported_costs
    }

    pub fn is_zero(&self) -> bool {
        self.model_calls == 0
            && self.compaction_calls == 0
            && self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.reasoning_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.reported_costs.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeStatusView {
    Running,
    Closing,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeView {
    status: RuntimeStatusView,
}

impl RuntimeView {
    pub const fn new(status: RuntimeStatusView) -> Self {
        Self { status }
    }

    pub const fn status(self) -> RuntimeStatusView {
        self.status
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionLifecycleView {
    Open,
    Archived,
    Deleted,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionLoadStateView {
    Loaded,
    Unloading,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionUnavailableView {
    AgentUnavailable,
    WorkspaceUnavailable,
    ModelUnavailable,
    PromptUnavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeDependencyUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionReadinessView {
    Preparing,
    Ready,
    Unavailable(SessionUnavailableView),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionExecutionView {
    Idle,
    Starting,
    Running,
    Finishing,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionRecordingState {
    Healthy,
    Degraded,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionRecordingView {
    state: SessionRecordingState,
}

impl SessionRecordingView {
    pub const fn new(state: SessionRecordingState) -> Self {
        Self { state }
    }

    pub const fn state(self) -> SessionRecordingState {
        self.state
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LoadedSessionSummary {
    session_id: SessionId,
    readiness: SessionReadinessView,
    execution: SessionExecutionView,
    recording: SessionRecordingView,
}

impl LoadedSessionSummary {
    pub fn new(
        session_id: SessionId,
        readiness: SessionReadinessView,
        execution: SessionExecutionView,
        recording: SessionRecordingView,
    ) -> Result<Self, ObservationValueError> {
        if readiness != SessionReadinessView::Ready && execution != SessionExecutionView::Idle {
            return Err(ObservationValueError::InconsistentLoadedSessionState);
        }
        Ok(Self {
            session_id,
            readiness,
            execution,
            recording,
        })
    }

    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    pub const fn readiness(self) -> SessionReadinessView {
        self.readiness
    }

    pub const fn execution(self) -> SessionExecutionView {
        self.execution
    }

    pub const fn recording(self) -> SessionRecordingView {
        self.recording
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSnapshot {
    runtime: RuntimeView,
    loaded_sessions: Vec<LoadedSessionSummary>,
    diagnostics: Vec<RuntimeDiagnosticView>,
}

impl RuntimeSnapshot {
    pub fn new(
        runtime: RuntimeView,
        loaded_sessions: Vec<LoadedSessionSummary>,
        diagnostics: Vec<RuntimeDiagnosticView>,
    ) -> Result<Self, ObservationValueError> {
        Self::new_with_limits(
            runtime,
            loaded_sessions,
            diagnostics,
            ProtocolLimits::v1_0(),
        )
    }

    pub(crate) fn new_with_limits(
        runtime: RuntimeView,
        loaded_sessions: Vec<LoadedSessionSummary>,
        diagnostics: Vec<RuntimeDiagnosticView>,
        limits: ProtocolLimits,
    ) -> Result<Self, ObservationValueError> {
        if !diagnostics.is_empty() {
            return Err(ObservationValueError::NonMinimalObservation);
        }
        if loaded_sessions.len()
            > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX)
            || diagnostics.len() > usize::from(limits.observation.max_snapshot_diagnostics)
        {
            return Err(ObservationValueError::TooManyValues);
        }
        let unique = loaded_sessions
            .iter()
            .map(|session| session.session_id)
            .collect::<BTreeSet<_>>();
        if unique.len() != loaded_sessions.len() {
            return Err(ObservationValueError::DuplicateValue);
        }
        Ok(Self {
            runtime,
            loaded_sessions,
            diagnostics,
        })
    }

    pub const fn runtime(&self) -> RuntimeView {
        self.runtime
    }

    pub fn loaded_sessions(&self) -> &[LoadedSessionSummary] {
        &self.loaded_sessions
    }

    pub fn diagnostics(&self) -> &[RuntimeDiagnosticView] {
        &self.diagnostics
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SessionSnapshotState {
    LoadedReadyIdle,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionSnapshot {
    session_id: SessionId,
    metadata: SessionMetadataView,
    definition: SessionDefinitionSummary,
    recording: SessionRecordingView,
    usage: Option<SessionUsageView>,
    diagnostics: Vec<SessionDiagnosticView>,
    state: SessionSnapshotState,
}

impl SessionSnapshot {
    pub fn new_loaded_ready_idle(
        session_id: SessionId,
        metadata: SessionMetadataView,
        definition: SessionDefinitionSummary,
        recording: SessionRecordingView,
        usage: Option<SessionUsageView>,
        diagnostics: Vec<SessionDiagnosticView>,
    ) -> Result<Self, ObservationValueError> {
        Self::new_loaded_ready_idle_with_limits(
            session_id,
            metadata,
            definition,
            recording,
            usage,
            diagnostics,
            ProtocolLimits::v1_0(),
        )
    }

    pub(crate) fn new_loaded_ready_idle_with_limits(
        session_id: SessionId,
        metadata: SessionMetadataView,
        definition: SessionDefinitionSummary,
        recording: SessionRecordingView,
        usage: Option<SessionUsageView>,
        diagnostics: Vec<SessionDiagnosticView>,
        _limits: ProtocolLimits,
    ) -> Result<Self, ObservationValueError> {
        if session_id != definition.session_id() {
            return Err(ObservationValueError::SessionIdentityMismatch);
        }
        if recording.state() != SessionRecordingState::Healthy
            || !diagnostics.is_empty()
            || usage.as_ref().is_some_and(|usage| !usage.is_zero())
        {
            return Err(ObservationValueError::NonMinimalObservation);
        }
        Ok(Self {
            session_id,
            metadata,
            definition,
            recording,
            usage,
            diagnostics,
            state: SessionSnapshotState::LoadedReadyIdle,
        })
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn lifecycle(&self) -> SessionLifecycleView {
        SessionLifecycleView::Open
    }

    pub const fn metadata(&self) -> &SessionMetadataView {
        &self.metadata
    }

    pub const fn definition(&self) -> &SessionDefinitionSummary {
        &self.definition
    }

    pub const fn load_state(&self) -> SessionLoadStateView {
        SessionLoadStateView::Loaded
    }

    pub const fn readiness(&self) -> SessionReadinessView {
        SessionReadinessView::Ready
    }

    pub const fn execution(&self) -> SessionExecutionView {
        match self.state {
            SessionSnapshotState::LoadedReadyIdle => SessionExecutionView::Idle,
        }
    }

    pub const fn recording(&self) -> SessionRecordingView {
        self.recording
    }

    pub const fn usage(&self) -> Option<&SessionUsageView> {
        self.usage.as_ref()
    }

    pub fn diagnostics(&self) -> &[SessionDiagnosticView] {
        &self.diagnostics
    }
}

impl fmt::Debug for SessionSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionSnapshot")
            .field("session_id", &self.session_id)
            .field("metadata", &self.metadata)
            .field("definition", &self.definition)
            .field("recording", &self.recording)
            .field("usage", &self.usage)
            .field("diagnostics", &self.diagnostics)
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotResponse {
    Runtime(RuntimeSnapshot),
    Session(Box<SessionSnapshot>),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EventRoute {
    Runtime,
    Agent {
        agent_id: AgentId,
    },
    Session {
        session_id: SessionId,
    },
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Item {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
    },
    Interaction {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
        request_id: RequestId,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeStateEventKind {
    CommandCatalogInvalidated,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionStateEventKind {
    TurnCompleted,
    TurnFailed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TurnFailureView {
    Prompt,
    Model,
    Tool,
    ContextOverflow,
    DependencyUnavailable,
    InvariantFailure,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TurnTerminalView {
    Completed {
        completed_at: Timestamp,
    },
    Failed {
        completed_at: Timestamp,
        reason: TurnFailureView,
    },
}

impl TurnTerminalView {
    pub const fn completed_at(self) -> Timestamp {
        match self {
            Self::Completed { completed_at } | Self::Failed { completed_at, .. } => completed_at,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionEventDetail {
    TurnTerminal {
        turn_id: TurnId,
        terminal: TurnTerminalView,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateEventMsg {
    Runtime {
        kind: RuntimeStateEventKind,
        snapshot: RuntimeSnapshot,
    },
    Session {
        kind: SessionStateEventKind,
        snapshot: Box<SessionSnapshot>,
        detail: Option<SessionEventDetail>,
    },
}

impl StateEventMsg {
    pub const fn runtime_kind(&self) -> Option<RuntimeStateEventKind> {
        match self {
            Self::Runtime { kind, .. } => Some(*kind),
            Self::Session { .. } => None,
        }
    }

    pub const fn runtime_snapshot(&self) -> Option<&RuntimeSnapshot> {
        match self {
            Self::Runtime { snapshot, .. } => Some(snapshot),
            Self::Session { .. } => None,
        }
    }

    pub const fn session_kind(&self) -> Option<SessionStateEventKind> {
        match self {
            Self::Runtime { .. } => None,
            Self::Session { kind, .. } => Some(*kind),
        }
    }

    pub const fn session_snapshot(&self) -> Option<&SessionSnapshot> {
        match self {
            Self::Runtime { .. } => None,
            Self::Session { snapshot, .. } => Some(snapshot),
        }
    }

    pub const fn session_detail(&self) -> Option<SessionEventDetail> {
        match self {
            Self::Runtime { .. } => None,
            Self::Session { detail, .. } => *detail,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateEvent {
    timestamp: Timestamp,
    command_id: Option<CommandId>,
    route: EventRoute,
    msg: StateEventMsg,
}

impl StateEvent {
    pub fn runtime_command_catalog_invalidated(
        timestamp: Timestamp,
        command_id: Option<CommandId>,
        snapshot: RuntimeSnapshot,
    ) -> Self {
        Self {
            timestamp,
            command_id,
            route: EventRoute::Runtime,
            msg: StateEventMsg::Runtime {
                kind: RuntimeStateEventKind::CommandCatalogInvalidated,
                snapshot,
            },
        }
    }

    pub fn turn_completed(
        timestamp: Timestamp,
        command_id: Option<CommandId>,
        snapshot: SessionSnapshot,
        turn_id: TurnId,
        completed_at: Timestamp,
    ) -> Self {
        let session_id = snapshot.session_id();
        Self {
            timestamp,
            command_id,
            route: EventRoute::Turn {
                session_id,
                turn_id,
            },
            msg: StateEventMsg::Session {
                kind: SessionStateEventKind::TurnCompleted,
                snapshot: Box::new(snapshot),
                detail: Some(SessionEventDetail::TurnTerminal {
                    turn_id,
                    terminal: TurnTerminalView::Completed { completed_at },
                }),
            },
        }
    }

    pub fn turn_failed(
        timestamp: Timestamp,
        command_id: Option<CommandId>,
        snapshot: SessionSnapshot,
        turn_id: TurnId,
        completed_at: Timestamp,
        reason: TurnFailureView,
    ) -> Self {
        let session_id = snapshot.session_id();
        Self {
            timestamp,
            command_id,
            route: EventRoute::Turn {
                session_id,
                turn_id,
            },
            msg: StateEventMsg::Session {
                kind: SessionStateEventKind::TurnFailed,
                snapshot: Box::new(snapshot),
                detail: Some(SessionEventDetail::TurnTerminal {
                    turn_id,
                    terminal: TurnTerminalView::Failed {
                        completed_at,
                        reason,
                    },
                }),
            },
        }
    }

    pub(crate) fn from_wire(
        timestamp: Timestamp,
        command_id: Option<CommandId>,
        route: EventRoute,
        msg: StateEventMsg,
    ) -> Result<Self, ObservationValueError> {
        let valid = match (&route, &msg) {
            (
                EventRoute::Runtime,
                StateEventMsg::Runtime {
                    kind: RuntimeStateEventKind::CommandCatalogInvalidated,
                    ..
                },
            ) => true,
            (
                EventRoute::Turn {
                    session_id,
                    turn_id,
                },
                StateEventMsg::Session {
                    kind,
                    snapshot,
                    detail:
                        Some(SessionEventDetail::TurnTerminal {
                            turn_id: detail_turn_id,
                            terminal,
                        }),
                },
            ) => {
                let kind_matches = matches!(
                    (kind, terminal),
                    (
                        SessionStateEventKind::TurnCompleted,
                        TurnTerminalView::Completed { .. }
                    ) | (
                        SessionStateEventKind::TurnFailed,
                        TurnTerminalView::Failed { .. }
                    )
                );
                *session_id == snapshot.session_id() && *turn_id == *detail_turn_id && kind_matches
            }
            _ => false,
        };
        if !valid {
            return Err(ObservationValueError::InconsistentStateEvent);
        }
        Ok(Self {
            timestamp,
            command_id,
            route,
            msg,
        })
    }

    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub const fn command_id(&self) -> Option<CommandId> {
        self.command_id
    }

    pub const fn route(&self) -> EventRoute {
        self.route
    }

    pub const fn msg(&self) -> &StateEventMsg {
        &self.msg
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventFrame {
    Snapshot(SnapshotResponse),
    State(StateEvent),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionScope {
    Runtime,
    Session { session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SubscriptionRequest {
    scope: SubscriptionScope,
    include_progress: bool,
}

impl SubscriptionRequest {
    pub const fn new(scope: SubscriptionScope, include_progress: bool) -> Self {
        Self {
            scope,
            include_progress,
        }
    }

    pub const fn scope(&self) -> SubscriptionScope {
        self.scope
    }

    pub const fn include_progress(&self) -> bool {
        self.include_progress
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeRequest {
    Dispatch(CommandRequest),
    Query(RuntimeQuery),
    Snapshot(SnapshotRequest),
    Subscribe(SubscriptionRequest),
}

#[derive(Clone, Copy, Debug, Eq, Error, Hash, PartialEq)]
pub enum RuntimeDispatchError {
    #[error("runtime dispatch envelope is invalid")]
    InvalidEnvelope,
    #[error("runtime dispatch request exceeds the selected limit")]
    RequestTooLarge,
    #[error("runtime is closed")]
    RuntimeClosed,
    #[error("runtime dispatch owner is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum QueryErrorCode {
    InvalidArgument,
    NotFound,
    SessionNotLoaded,
    StaleCursor,
    ResultTooLarge,
    Unavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeClosing,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SnapshotErrorCode {
    NotFound,
    SessionNotLoaded,
    Unavailable,
    RuntimeClosing,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionErrorCode {
    UnsupportedScope,
    NotFound,
    SessionNotLoaded,
    PublisherUnavailable,
    RuntimeClosing,
}

macro_rules! public_route_error {
    ($name:ident, $code:ident) => {
        #[derive(Clone, Eq, PartialEq)]
        pub struct $name {
            code: $code,
            message: Box<str>,
            retry: RetryAdvice,
            subject: Option<PublicSubject>,
        }

        impl $name {
            pub(crate) fn new(
                code: $code,
                message: &'static str,
                retry: RetryAdvice,
                subject: Option<PublicSubject>,
            ) -> Self {
                debug_assert!(
                    validate_safe_text(
                        message,
                        ProtocolLimits::v1_0().text.max_diagnostic_message_bytes as usize,
                        false,
                    )
                    .is_ok()
                );
                Self {
                    code,
                    message: message.into(),
                    retry,
                    subject,
                }
            }

            pub const fn code(&self) -> $code {
                self.code
            }

            pub fn message(&self) -> &str {
                &self.message
            }

            pub const fn retry(&self) -> RetryAdvice {
                self.retry
            }

            pub const fn subject(&self) -> Option<&PublicSubject> {
                self.subject.as_ref()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("code", &self.code)
                    .field("message", &"<redacted>")
                    .field("retry", &self.retry)
                    .field("subject", &self.subject)
                    .finish()
            }
        }
    };
}

public_route_error!(QueryError, QueryErrorCode);
public_route_error!(SnapshotError, SnapshotErrorCode);
public_route_error!(SubscriptionError, SubscriptionErrorCode);
