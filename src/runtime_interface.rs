use std::collections::BTreeSet;
use std::fmt;

use thiserror::Error;

use crate::agent_session_lifecycle::SessionModelConfig;
use crate::prompt::{PromptIntent, SessionPromptSelection};
use crate::skills::SkillId;
use crate::wire::lexical::{normalize_newlines, validate_safe_text};
use crate::wire::{AgentId, CommandId, ItemId, ProtocolLimits, RequestId, SessionId, TurnId};
use crate::workspace::WorkspaceDefinitionInput;

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

    pub fn all_v1() -> Self {
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
        | CommandErrorCode::SessionNotReady
        | CommandErrorCode::ReloadValidationFailed
        | CommandErrorCode::Unauthorized
        | CommandErrorCode::DurableStateCorrupt
        | CommandErrorCode::DurableStateTooLarge => retry == RetryAdvice::UserActionRequired,
        CommandErrorCode::Unavailable => retry == RetryAdvice::DoNotRetry,
        CommandErrorCode::IngressLaneFull { .. } | CommandErrorCode::RuntimeClosing => false,
    };
    if valid {
        Ok(())
    } else {
        Err(CommandValueError::InvalidErrorContract)
    }
}

pub(crate) const fn command_error_code_allows_retry_with_backoff(code: CommandErrorCode) -> bool {
    matches!(
        code,
        CommandErrorCode::IngressLaneFull { .. }
            | CommandErrorCode::SessionNotReady
            | CommandErrorCode::Unavailable
            | CommandErrorCode::RuntimeClosing
    )
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
    UserActionRequired,
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
