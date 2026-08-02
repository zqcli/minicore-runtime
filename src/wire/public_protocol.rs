use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::agent_session_lifecycle::SessionModelConfig;
use crate::model_gateway::{ModelId, ModelSelection, ProviderId, ReasoningPreference};
use crate::prompt::{
    PromptBodyIntent, PromptId, PromptIntent, PromptValueError, SessionPromptSelection,
    SkillIntent, TextIntent, normalize_text_intent, validate_skill_intent_count,
};
use crate::runtime_interface::{
    CommandCompletion, CommandError, CommandErrorCode, CommandOutcome, CommandOutput,
    CommandRequest, CommandResponse, NewSessionDefinition, NewSessionMetadata, PublicCancelTarget,
    PublicIngressLane, PublicSubject, QueryResponse, QueryResult, RetryAdvice, RuntimeCapabilities,
    RuntimeCommand, RuntimeDispatchError, RuntimeLifecycleCommand, RuntimeQuery,
    RuntimeQueryResult, RuntimeReadQuery, RuntimeRequest, SessionCommand, SnapshotRequest,
    SubscriptionRequest, SubscriptionScope, TurnCommand,
    command_error_code_allows_retry_with_backoff, validate_command_error_contract,
    validate_command_error_message, validate_command_output,
};
use crate::skills::SkillId;
use crate::workspace::{
    RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspaceRootInput,
    WorkspaceRootKey, WorkspaceSourcePolicy,
};

use super::bounded_json::JsonNode;
use super::limits::{CapabilityToken, ProtocolLimits, runtime_capability_from_token};
use super::scalar::{AgentId, CommandId, ItemId, RequestId, SessionId, TurnId};
use super::typed_json::{
    PublicDecodeCode, PublicDecodeError, PublicDecodeStage, PublicJsonKind, TypedJsonError,
    WireV1Codec,
};
use super::{CanonicalFileUri, WorkspaceRelativePath};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeRequestKind {
    Dispatch,
    Query,
    Snapshot,
    Subscribe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selected-V1 codec for the public targets activated by the current behavior slices.
pub struct IncrementalRuntimeProtocolV1 {
    codec: WireV1Codec,
}

impl IncrementalRuntimeProtocolV1 {
    pub fn new(codec: WireV1Codec) -> Self {
        Self { codec }
    }

    pub fn v1_0() -> Self {
        Self::new(WireV1Codec::v1_0())
    }

    pub fn decode_request(
        &self,
        kind: RuntimeRequestKind,
        input: &[u8],
    ) -> Result<RuntimeRequest, TypedJsonError> {
        match kind {
            RuntimeRequestKind::Dispatch => {
                decode_command_request(&self.codec, input).map(RuntimeRequest::Dispatch)
            }
            RuntimeRequestKind::Query => {
                decode_runtime_query(&self.codec, input).map(RuntimeRequest::Query)
            }
            RuntimeRequestKind::Snapshot => {
                decode_snapshot_request(&self.codec, input).map(RuntimeRequest::Snapshot)
            }
            RuntimeRequestKind::Subscribe => {
                decode_subscription_request(&self.codec, input).map(RuntimeRequest::Subscribe)
            }
        }
    }

    pub fn encode_request(&self, request: &RuntimeRequest) -> Result<Vec<u8>, TypedJsonError> {
        match request {
            RuntimeRequest::Dispatch(request) => encode_command_request(&self.codec, request),
            RuntimeRequest::Query(query) => encode_runtime_query(&self.codec, query),
            RuntimeRequest::Snapshot(request) => encode_snapshot_request(&self.codec, *request),
            RuntimeRequest::Subscribe(request) => {
                encode_subscription_request(&self.codec, *request)
            }
        }
    }

    pub fn decode_query_response(&self, input: &[u8]) -> Result<QueryResponse, TypedJsonError> {
        decode_query_response(&self.codec, input)
    }

    pub fn decode_command_response(&self, input: &[u8]) -> Result<CommandResponse, TypedJsonError> {
        decode_command_response(&self.codec, input)
    }

    pub fn encode_command_response(
        &self,
        response: &CommandResponse,
    ) -> Result<Vec<u8>, TypedJsonError> {
        encode_command_response(&self.codec, response)
    }

    pub fn encode_query_response(
        &self,
        response: &QueryResponse,
    ) -> Result<Vec<u8>, TypedJsonError> {
        encode_query_response(&self.codec, response)
    }

    pub fn decode_runtime_dispatch_error(
        &self,
        input: &[u8],
    ) -> Result<RuntimeDispatchError, TypedJsonError> {
        decode_runtime_dispatch_error(&self.codec, input)
    }

    pub fn encode_runtime_dispatch_error(
        &self,
        error: RuntimeDispatchError,
    ) -> Result<Vec<u8>, TypedJsonError> {
        encode_runtime_dispatch_error(&self.codec, error)
    }

    pub const fn codec(&self) -> &WireV1Codec {
        &self.codec
    }
}

fn decode_runtime_dispatch_error(
    codec: &WireV1Codec,
    input: &[u8],
) -> Result<RuntimeDispatchError, TypedJsonError> {
    let node: String = codec.decode_with_shape(PublicJsonKind::Response, input, |node| {
        let value = node.as_str().ok_or_else(typed_wrong_json_type)?;
        parse_runtime_dispatch_error(value).ok_or_else(unknown_output_variant)?;
        Ok(())
    })?;
    parse_runtime_dispatch_error(&node).ok_or_else(unknown_output_variant)
}

fn encode_runtime_dispatch_error(
    codec: &WireV1Codec,
    error: RuntimeDispatchError,
) -> Result<Vec<u8>, TypedJsonError> {
    codec.encode(
        PublicJsonKind::Response,
        &runtime_dispatch_error_name(error),
    )
}

fn decode_command_request(
    codec: &WireV1Codec,
    input: &[u8],
) -> Result<CommandRequest, TypedJsonError> {
    let decoded: CommandRequestInput =
        codec.decode_with_shape(PublicJsonKind::Request, input, |node| {
            validate_command_request_shape(node, codec.limits())
        })?;
    Ok(CommandRequest::new(
        decoded.command_id,
        decoded.command.into_semantic(codec.limits())?,
    ))
}

fn encode_command_request(
    codec: &WireV1Codec,
    request: &CommandRequest,
) -> Result<Vec<u8>, TypedJsonError> {
    validate_command_semantic_limits(request.command(), codec.limits())?;
    codec.encode(
        PublicJsonKind::Request,
        &CommandRequestOutput {
            command_id: request.command_id(),
            command: RuntimeCommandOutput::from_semantic(request.command()),
        },
    )
}

fn decode_command_response(
    codec: &WireV1Codec,
    input: &[u8],
) -> Result<CommandResponse, TypedJsonError> {
    let decoded: CommandResponseInput =
        codec.decode_with_shape(PublicJsonKind::Response, input, |node| {
            validate_command_response_shape(node, codec.limits())
        })?;
    decoded.into_semantic(codec.limits())
}

fn encode_command_response(
    codec: &WireV1Codec,
    response: &CommandResponse,
) -> Result<Vec<u8>, TypedJsonError> {
    validate_command_response_semantic_limits(response, codec.limits())?;
    codec.encode(
        PublicJsonKind::Response,
        &CommandResponseOutput::from_semantic(response),
    )
}

fn decode_runtime_query(codec: &WireV1Codec, input: &[u8]) -> Result<RuntimeQuery, TypedJsonError> {
    let decoded: RuntimeQueryInput =
        codec.decode_with_shape(PublicJsonKind::Request, input, validate_runtime_query_shape)?;
    Ok(decoded.into_semantic())
}

fn encode_runtime_query(
    codec: &WireV1Codec,
    query: &RuntimeQuery,
) -> Result<Vec<u8>, TypedJsonError> {
    codec.encode(
        PublicJsonKind::Request,
        &RuntimeQueryOutput::from_semantic(query),
    )
}

fn decode_snapshot_request(
    codec: &WireV1Codec,
    input: &[u8],
) -> Result<SnapshotRequest, TypedJsonError> {
    let decoded: SnapshotRequestInput = codec.decode_with_shape(
        PublicJsonKind::Request,
        input,
        validate_snapshot_request_shape,
    )?;
    Ok(decoded.into_semantic())
}

fn encode_snapshot_request(
    codec: &WireV1Codec,
    request: SnapshotRequest,
) -> Result<Vec<u8>, TypedJsonError> {
    codec.encode(
        PublicJsonKind::Request,
        &SnapshotRequestOutput::from_semantic(request),
    )
}

fn decode_subscription_request(
    codec: &WireV1Codec,
    input: &[u8],
) -> Result<SubscriptionRequest, TypedJsonError> {
    let decoded: SubscriptionRequestInput = codec.decode_with_shape(
        PublicJsonKind::Request,
        input,
        validate_subscription_request_shape,
    )?;
    Ok(SubscriptionRequest::new(
        decoded.scope.into_semantic(),
        decoded.include_progress,
    ))
}

fn encode_subscription_request(
    codec: &WireV1Codec,
    request: SubscriptionRequest,
) -> Result<Vec<u8>, TypedJsonError> {
    codec.encode(
        PublicJsonKind::Request,
        &SubscriptionRequestOutput {
            scope: SubscriptionScopeOutput::from_semantic(request.scope()),
            include_progress: request.include_progress(),
        },
    )
}

fn decode_query_response(
    codec: &WireV1Codec,
    input: &[u8],
) -> Result<QueryResponse, TypedJsonError> {
    let decoded: QueryResponseInput = codec.decode_with_shape(
        PublicJsonKind::Response,
        input,
        validate_query_response_shape,
    )?;
    Ok(QueryResponse::new(decoded.data.into_semantic()?))
}

fn encode_query_response(
    codec: &WireV1Codec,
    response: &QueryResponse,
) -> Result<Vec<u8>, TypedJsonError> {
    codec.encode(
        PublicJsonKind::Response,
        &QueryResponseOutput {
            data: QueryResultOutput::from_semantic(response.data()),
        },
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandRequestInput {
    command_id: CommandId,
    command: RuntimeCommandInput,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeCommandInput {
    Runtime(RuntimeLifecycleCommandInput),
    Session(SessionCommandInput),
    Turn(TurnCommandInput),
}

impl RuntimeCommandInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<RuntimeCommand, TypedJsonError> {
        Ok(match self {
            Self::Runtime(RuntimeLifecycleCommandInput::ReloadSharedResources) => {
                RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources)
            }
            Self::Session(SessionCommandInput::Create(value)) => {
                RuntimeCommand::Session(value.into_semantic(limits)?)
            }
            Self::Session(SessionCommandInput::Load(value)) => {
                RuntimeCommand::Session(SessionCommand::Load {
                    session_id: value.session_id,
                })
            }
            Self::Session(SessionCommandInput::Unload(value)) => {
                RuntimeCommand::Session(SessionCommand::Unload {
                    session_id: value.session_id,
                })
            }
            Self::Turn(TurnCommandInput::Submit(value)) => {
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id: value.session_id,
                    intent: value.intent.into_semantic(limits)?,
                })
            }
            Self::Turn(TurnCommandInput::Cancel(value)) => {
                RuntimeCommand::Turn(TurnCommand::Cancel {
                    session_id: value.session_id,
                    target: value.target.into_semantic(),
                })
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeLifecycleCommandInput {
    ReloadSharedResources,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionCommandInput {
    Create(Box<CreateSessionCommandInput>),
    Load(SessionIdInput),
    Unload(SessionIdInput),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateSessionCommandInput {
    agent_id: AgentId,
    definition: NewSessionDefinitionInput,
    metadata: NewSessionMetadataInput,
}

impl CreateSessionCommandInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<SessionCommand, TypedJsonError> {
        Ok(SessionCommand::Create {
            agent_id: self.agent_id,
            definition: Box::new(self.definition.into_semantic(limits)?),
            metadata: self.metadata.into_semantic(limits)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSessionDefinitionInput {
    workspace: WorkspaceDefinitionWireInput,
    model: SessionModelConfigInput,
    prompts: SessionPromptSelectionInput,
}

impl NewSessionDefinitionInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<NewSessionDefinition, TypedJsonError> {
        Ok(NewSessionDefinition::new(
            self.workspace.into_semantic(limits)?,
            self.model.into_semantic()?,
            self.prompts.into_semantic(limits)?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceDefinitionWireInput {
    primary_root: WorkspaceRootWireInput,
    additional_roots: Vec<WorkspaceRootWireInput>,
    cwd: WorkspaceCwdWireInput,
}

impl WorkspaceDefinitionWireInput {
    fn into_semantic(
        self,
        limits: ProtocolLimits,
    ) -> Result<WorkspaceDefinitionInput, TypedJsonError> {
        WorkspaceDefinitionInput::new_with_limits(
            self.primary_root.into_semantic()?,
            self.additional_roots
                .into_iter()
                .map(WorkspaceRootWireInput::into_semantic)
                .collect::<Result<Vec<_>, _>>()?,
            self.cwd.into_semantic()?,
            limits,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceRootWireInput {
    key: String,
    path: CanonicalFileUri,
    requested_access: RequestedFilesystemAccessInput,
    sources: WorkspaceSourcePolicyInput,
}

impl WorkspaceRootWireInput {
    fn into_semantic(self) -> Result<WorkspaceRootInput, TypedJsonError> {
        Ok(WorkspaceRootInput::new(
            self.key
                .parse::<WorkspaceRootKey>()
                .map_err(|_| invalid_scalar())?,
            self.path,
            self.requested_access.into_semantic(),
            self.sources.into_semantic(),
        ))
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RequestedFilesystemAccessInput {
    ReadOnly,
    ReadWrite,
}

impl RequestedFilesystemAccessInput {
    const fn into_semantic(self) -> RequestedFilesystemAccess {
        match self {
            Self::ReadOnly => RequestedFilesystemAccess::ReadOnly,
            Self::ReadWrite => RequestedFilesystemAccess::ReadWrite,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceSourcePolicyInput {
    prompt: bool,
    skill: bool,
}

impl WorkspaceSourcePolicyInput {
    const fn into_semantic(self) -> WorkspaceSourcePolicy {
        WorkspaceSourcePolicy::new(self.prompt, self.skill)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceCwdWireInput {
    root: String,
    relative_path: WorkspaceRelativePath,
}

impl WorkspaceCwdWireInput {
    fn into_semantic(self) -> Result<WorkspaceCwdSpec, TypedJsonError> {
        Ok(WorkspaceCwdSpec::new(
            self.root
                .parse::<WorkspaceRootKey>()
                .map_err(|_| invalid_scalar())?,
            self.relative_path,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionModelConfigInput {
    selection: ModelSelectionInput,
    reasoning: ReasoningPreferenceInput,
    max_output_tokens: Option<NonZeroU32>,
}

impl SessionModelConfigInput {
    fn into_semantic(self) -> Result<SessionModelConfig, TypedJsonError> {
        Ok(SessionModelConfig::new(
            self.selection.into_semantic()?,
            self.reasoning.into_semantic(),
            self.max_output_tokens,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelSelectionInput {
    provider_id: String,
    model_id: String,
}

impl ModelSelectionInput {
    fn into_semantic(self) -> Result<ModelSelection, TypedJsonError> {
        Ok(ModelSelection::new(
            self.provider_id
                .parse::<ProviderId>()
                .map_err(|_| invalid_scalar())?,
            self.model_id
                .parse::<ModelId>()
                .map_err(|_| invalid_scalar())?,
        ))
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReasoningPreferenceInput {
    Auto,
    Disabled,
    Low,
    Medium,
    High,
}

impl ReasoningPreferenceInput {
    const fn into_semantic(self) -> ReasoningPreference {
        match self {
            Self::Auto => ReasoningPreference::Auto,
            Self::Disabled => ReasoningPreference::Disabled,
            Self::Low => ReasoningPreference::Low,
            Self::Medium => ReasoningPreference::Medium,
            Self::High => ReasoningPreference::High,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionPromptSelectionInput {
    enabled: Vec<String>,
}

impl SessionPromptSelectionInput {
    fn into_semantic(
        self,
        limits: ProtocolLimits,
    ) -> Result<SessionPromptSelection, TypedJsonError> {
        let enabled = self
            .enabled
            .into_iter()
            .map(|value| value.parse::<PromptId>().map_err(|_| invalid_scalar()))
            .collect::<Result<Vec<_>, _>>()?;
        SessionPromptSelection::new_with_maximum(
            enabled,
            usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX),
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSessionMetadataInput {
    name: Option<String>,
    description: Option<String>,
}

impl NewSessionMetadataInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<NewSessionMetadata, TypedJsonError> {
        NewSessionMetadata::new_with_limits(self.name, self.description, limits)
            .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum TurnCommandInput {
    Submit(SubmitCommandInput),
    Cancel(CancelCommandInput),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmitCommandInput {
    session_id: SessionId,
    intent: PromptIntentWireInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PromptIntentWireInput {
    body: PromptBodyIntentInput,
    skills: Vec<SkillIntentInput>,
}

impl PromptIntentWireInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<PromptIntent, TypedJsonError> {
        let body = match self.body {
            PromptBodyIntentInput::Empty => PromptBodyIntent::Empty,
            PromptBodyIntentInput::Text(value) => PromptBodyIntent::Text(
                TextIntent::new_with_maximum(
                    value.text,
                    limits.text.max_text_intent_bytes as usize,
                )
                .map_err(map_prompt_value_error)?,
            ),
        };
        let skills = self
            .skills
            .into_iter()
            .map(|value| {
                value
                    .skill_id
                    .parse::<SkillId>()
                    .map(SkillIntent::new)
                    .map_err(|_| invalid_scalar())
            })
            .collect::<Result<Vec<_>, _>>()?;
        PromptIntent::new_with_maximum_skills(
            body,
            skills,
            limits.prompt.max_skills_per_intent as usize,
        )
        .map_err(map_prompt_value_error)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum PromptBodyIntentInput {
    Empty,
    Text(TextIntentInput),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextIntentInput {
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillIntentInput {
    skill_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelCommandInput {
    session_id: SessionId,
    target: PublicCancelTargetInput,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum PublicCancelTargetInput {
    Submit(CommandId),
    Turn(TurnId),
}

impl PublicCancelTargetInput {
    const fn into_semantic(self) -> PublicCancelTarget {
        match self {
            Self::Submit(command_id) => PublicCancelTarget::Submit(command_id),
            Self::Turn(turn_id) => PublicCancelTarget::Turn(turn_id),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandRequestOutput<'a> {
    command_id: CommandId,
    command: RuntimeCommandOutput<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeCommandOutput<'a> {
    Runtime(RuntimeLifecycleCommandOutput),
    Session(SessionCommandOutput<'a>),
    Turn(TurnCommandOutput<'a>),
}

impl<'a> RuntimeCommandOutput<'a> {
    fn from_semantic(value: &'a RuntimeCommand) -> Self {
        match value {
            RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources) => {
                Self::Runtime(RuntimeLifecycleCommandOutput::ReloadSharedResources)
            }
            RuntimeCommand::Session(SessionCommand::Create {
                agent_id,
                definition,
                metadata,
            }) => Self::Session(SessionCommandOutput::Create(CreateSessionCommandOutput {
                agent_id: *agent_id,
                definition: NewSessionDefinitionOutput::from_semantic(definition),
                metadata: NewSessionMetadataOutput::from_semantic(metadata),
            })),
            RuntimeCommand::Session(SessionCommand::Load { session_id }) => {
                Self::Session(SessionCommandOutput::Load(SessionIdOutput {
                    session_id: *session_id,
                }))
            }
            RuntimeCommand::Session(SessionCommand::Unload { session_id }) => {
                Self::Session(SessionCommandOutput::Unload(SessionIdOutput {
                    session_id: *session_id,
                }))
            }
            RuntimeCommand::Turn(TurnCommand::Submit { session_id, intent }) => {
                Self::Turn(TurnCommandOutput::Submit(SubmitCommandOutput {
                    session_id: *session_id,
                    intent: PromptIntentWireOutput::from_semantic(intent),
                }))
            }
            RuntimeCommand::Turn(TurnCommand::Cancel { session_id, target }) => {
                Self::Turn(TurnCommandOutput::Cancel(CancelCommandOutput {
                    session_id: *session_id,
                    target: PublicCancelTargetOutput::from_semantic(*target),
                }))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeLifecycleCommandOutput {
    ReloadSharedResources,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionCommandOutput<'a> {
    Create(CreateSessionCommandOutput<'a>),
    Load(SessionIdOutput),
    Unload(SessionIdOutput),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateSessionCommandOutput<'a> {
    agent_id: AgentId,
    definition: NewSessionDefinitionOutput<'a>,
    metadata: NewSessionMetadataOutput<'a>,
}

#[derive(Serialize)]
struct NewSessionDefinitionOutput<'a> {
    workspace: WorkspaceDefinitionOutput<'a>,
    model: SessionModelConfigOutput<'a>,
    prompts: SessionPromptSelectionOutput<'a>,
}

impl<'a> NewSessionDefinitionOutput<'a> {
    fn from_semantic(value: &'a NewSessionDefinition) -> Self {
        Self {
            workspace: WorkspaceDefinitionOutput::from_semantic(value.workspace()),
            model: SessionModelConfigOutput::from_semantic(value.model()),
            prompts: SessionPromptSelectionOutput::from_semantic(value.prompts()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDefinitionOutput<'a> {
    primary_root: WorkspaceRootOutput<'a>,
    additional_roots: Vec<WorkspaceRootOutput<'a>>,
    cwd: WorkspaceCwdOutput<'a>,
}

impl<'a> WorkspaceDefinitionOutput<'a> {
    fn from_semantic(value: &'a WorkspaceDefinitionInput) -> Self {
        Self {
            primary_root: WorkspaceRootOutput::from_semantic(value.primary_root()),
            additional_roots: value
                .additional_roots()
                .iter()
                .map(WorkspaceRootOutput::from_semantic)
                .collect(),
            cwd: WorkspaceCwdOutput::from_semantic(value.cwd()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRootOutput<'a> {
    key: &'a str,
    path: &'a CanonicalFileUri,
    requested_access: RequestedFilesystemAccessOutput,
    sources: WorkspaceSourcePolicyOutput,
}

impl<'a> WorkspaceRootOutput<'a> {
    fn from_semantic(value: &'a WorkspaceRootInput) -> Self {
        Self {
            key: value.key().as_str(),
            path: value.path(),
            requested_access: RequestedFilesystemAccessOutput::from_semantic(
                value.requested_access(),
            ),
            sources: WorkspaceSourcePolicyOutput::from_semantic(value.sources()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RequestedFilesystemAccessOutput {
    ReadOnly,
    ReadWrite,
}

impl RequestedFilesystemAccessOutput {
    const fn from_semantic(value: RequestedFilesystemAccess) -> Self {
        match value {
            RequestedFilesystemAccess::ReadOnly => Self::ReadOnly,
            RequestedFilesystemAccess::ReadWrite => Self::ReadWrite,
        }
    }
}

#[derive(Serialize)]
struct WorkspaceSourcePolicyOutput {
    prompt: bool,
    skill: bool,
}

impl WorkspaceSourcePolicyOutput {
    const fn from_semantic(value: WorkspaceSourcePolicy) -> Self {
        Self {
            prompt: value.prompt(),
            skill: value.skill(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceCwdOutput<'a> {
    root: &'a str,
    relative_path: &'a WorkspaceRelativePath,
}

impl<'a> WorkspaceCwdOutput<'a> {
    fn from_semantic(value: &'a WorkspaceCwdSpec) -> Self {
        Self {
            root: value.root().as_str(),
            relative_path: value.relative_path(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionModelConfigOutput<'a> {
    selection: ModelSelectionOutput<'a>,
    reasoning: ReasoningPreferenceOutput,
    max_output_tokens: Option<NonZeroU32>,
}

impl<'a> SessionModelConfigOutput<'a> {
    fn from_semantic(value: &'a SessionModelConfig) -> Self {
        Self {
            selection: ModelSelectionOutput::from_semantic(value.selection()),
            reasoning: ReasoningPreferenceOutput::from_semantic(value.reasoning()),
            max_output_tokens: value.max_output_tokens(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelSelectionOutput<'a> {
    provider_id: &'a str,
    model_id: &'a str,
}

impl<'a> ModelSelectionOutput<'a> {
    fn from_semantic(value: &'a ModelSelection) -> Self {
        Self {
            provider_id: value.provider_id().as_str(),
            model_id: value.model_id().as_str(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ReasoningPreferenceOutput {
    Auto,
    Disabled,
    Low,
    Medium,
    High,
}

impl ReasoningPreferenceOutput {
    const fn from_semantic(value: ReasoningPreference) -> Self {
        match value {
            ReasoningPreference::Auto => Self::Auto,
            ReasoningPreference::Disabled => Self::Disabled,
            ReasoningPreference::Low => Self::Low,
            ReasoningPreference::Medium => Self::Medium,
            ReasoningPreference::High => Self::High,
        }
    }
}

#[derive(Serialize)]
struct SessionPromptSelectionOutput<'a> {
    enabled: Vec<&'a str>,
}

impl<'a> SessionPromptSelectionOutput<'a> {
    fn from_semantic(value: &'a SessionPromptSelection) -> Self {
        Self {
            enabled: value.enabled().iter().map(PromptId::as_str).collect(),
        }
    }
}

#[derive(Serialize)]
struct NewSessionMetadataOutput<'a> {
    name: Option<&'a str>,
    description: Option<&'a str>,
}

impl<'a> NewSessionMetadataOutput<'a> {
    fn from_semantic(value: &'a NewSessionMetadata) -> Self {
        Self {
            name: value.name(),
            description: value.description(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum TurnCommandOutput<'a> {
    Submit(SubmitCommandOutput<'a>),
    Cancel(CancelCommandOutput),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitCommandOutput<'a> {
    session_id: SessionId,
    intent: PromptIntentWireOutput<'a>,
}

#[derive(Serialize)]
struct PromptIntentWireOutput<'a> {
    body: PromptBodyIntentOutput<'a>,
    skills: Vec<SkillIntentOutput<'a>>,
}

impl<'a> PromptIntentWireOutput<'a> {
    fn from_semantic(value: &'a PromptIntent) -> Self {
        let body = match value.body() {
            PromptBodyIntent::Empty => PromptBodyIntentOutput::Empty,
            PromptBodyIntent::Text(text) => {
                PromptBodyIntentOutput::Text(TextIntentOutput { text: text.text() })
            }
        };
        let skills = value
            .skills()
            .iter()
            .map(|skill| SkillIntentOutput {
                skill_id: skill.skill_id().as_str(),
            })
            .collect();
        Self { body, skills }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum PromptBodyIntentOutput<'a> {
    Empty,
    Text(TextIntentOutput<'a>),
}

#[derive(Serialize)]
struct TextIntentOutput<'a> {
    text: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillIntentOutput<'a> {
    skill_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelCommandOutput {
    session_id: SessionId,
    target: PublicCancelTargetOutput,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum PublicCancelTargetOutput {
    Submit(CommandId),
    Turn(TurnId),
}

impl PublicCancelTargetOutput {
    const fn from_semantic(value: PublicCancelTarget) -> Self {
        match value {
            PublicCancelTarget::Submit(command_id) => Self::Submit(command_id),
            PublicCancelTarget::Turn(turn_id) => Self::Turn(turn_id),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandResponseInput {
    command_id: CommandId,
    completion: CommandCompletionInput,
}

impl CommandResponseInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<CommandResponse, TypedJsonError> {
        let completion = match self.completion {
            CommandCompletionInput::Completed(value) => CommandCompletion::Completed {
                outcome: value.outcome.into_semantic(),
                output: value
                    .output
                    .map(|output| {
                        CommandOutput::new_with_maximum(
                            output.text,
                            limits.text.max_command_output_bytes as usize,
                        )
                        .map_err(|_| invalid_scalar())
                    })
                    .transpose()?,
            },
            CommandCompletionInput::Rejected(value) => {
                CommandCompletion::Rejected(value.into_semantic(limits)?)
            }
        };
        CommandResponse::new(self.command_id, completion).map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum CommandCompletionInput {
    Completed(CompletedCommandInput),
    Rejected(CommandErrorInput),
}

#[derive(Deserialize)]
struct CompletedCommandInput {
    outcome: CommandOutcomeInput,
    output: Option<CommandOutputInput>,
}

#[derive(Deserialize)]
struct CommandOutputInput {
    text: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum CommandOutcomeInput {
    TurnStarted(TurnIdInput),
    CommandOutput,
}

impl CommandOutcomeInput {
    const fn into_semantic(self) -> CommandOutcome {
        match self {
            Self::TurnStarted(value) => CommandOutcome::TurnStarted {
                turn_id: value.turn_id,
            },
            Self::CommandOutput => CommandOutcome::CommandOutput,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnIdInput {
    turn_id: TurnId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandErrorInput {
    code: CommandErrorCodeInput,
    message: String,
    retry: RetryAdviceInput,
    subject: Option<PublicSubjectInput>,
}

impl CommandErrorInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<CommandError, TypedJsonError> {
        CommandError::new_with_maximum_message(
            self.code.into_semantic()?,
            self.message,
            self.retry.into_semantic()?,
            self.subject
                .map(PublicSubjectInput::into_semantic)
                .transpose()?,
            limits.text.max_diagnostic_message_bytes as usize,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
struct CommandErrorCodeInput {
    #[serde(rename = "type")]
    kind: String,
    data: Option<IngressLaneFullInput>,
}

impl CommandErrorCodeInput {
    fn into_semantic(self) -> Result<CommandErrorCode, TypedJsonError> {
        let code = match self.kind.as_str() {
            "invalid_argument" => CommandErrorCode::InvalidArgument,
            "not_found" => CommandErrorCode::NotFound,
            "command_conflict" => CommandErrorCode::CommandConflict,
            "stale_revision" => CommandErrorCode::StaleRevision,
            "agent_disabled" => CommandErrorCode::AgentDisabled,
            "agent_deleted" => CommandErrorCode::AgentDeleted,
            "session_archived" => CommandErrorCode::SessionArchived,
            "session_deleted" => CommandErrorCode::SessionDeleted,
            "session_not_loaded" => CommandErrorCode::SessionNotLoaded,
            "session_not_ready" => CommandErrorCode::SessionNotReady,
            "session_busy" => CommandErrorCode::SessionBusy,
            "reload_validation_failed" => CommandErrorCode::ReloadValidationFailed,
            "ingress_lane_full" => CommandErrorCode::IngressLaneFull {
                lane: self
                    .data
                    .ok_or_else(missing_required_field)?
                    .lane
                    .into_semantic(),
            },
            "queued_message_not_queued" => CommandErrorCode::QueuedMessageNotQueued,
            "submit_not_cancellable" => CommandErrorCode::SubmitNotCancellable,
            "expected_turn_mismatch" => CommandErrorCode::ExpectedTurnMismatch,
            "turn_not_running" => CommandErrorCode::TurnNotRunning,
            "turn_cancelling" => CommandErrorCode::TurnCancelling,
            "turn_terminal" => CommandErrorCode::TurnTerminal,
            "interaction_not_found" => CommandErrorCode::InteractionNotFound,
            "interaction_already_resolved" => CommandErrorCode::InteractionAlreadyResolved,
            "interaction_family_mismatch" => CommandErrorCode::InteractionFamilyMismatch,
            "invalid_fork_anchor" => CommandErrorCode::InvalidForkAnchor,
            "unauthorized" => CommandErrorCode::Unauthorized,
            "unavailable" => CommandErrorCode::Unavailable,
            "durable_state_corrupt" => CommandErrorCode::DurableStateCorrupt,
            "durable_state_too_large" => CommandErrorCode::DurableStateTooLarge,
            "runtime_closing" => CommandErrorCode::RuntimeClosing,
            _ => return Err(unknown_output_variant()),
        };
        Ok(code)
    }
}

#[derive(Deserialize)]
struct IngressLaneFullInput {
    lane: PublicIngressLaneInput,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PublicIngressLaneInput {
    TurnAdmission,
    Steer,
    FollowUp,
    InteractionControl,
    ToolControl,
}

impl PublicIngressLaneInput {
    const fn into_semantic(self) -> PublicIngressLane {
        match self {
            Self::TurnAdmission => PublicIngressLane::TurnAdmission,
            Self::Steer => PublicIngressLane::Steer,
            Self::FollowUp => PublicIngressLane::FollowUp,
            Self::InteractionControl => PublicIngressLane::InteractionControl,
            Self::ToolControl => PublicIngressLane::ToolControl,
        }
    }
}

#[derive(Deserialize)]
struct RetryAdviceInput {
    #[serde(rename = "type")]
    kind: String,
}

impl RetryAdviceInput {
    fn into_semantic(self) -> Result<RetryAdvice, TypedJsonError> {
        Ok(match self.kind.as_str() {
            "do_not_retry" => RetryAdvice::DoNotRetry,
            "refresh_and_retry" => RetryAdvice::RefreshAndRetry,
            "user_action_required" => RetryAdvice::UserActionRequired,
            "retry_with_backoff" => return Err(TypedJsonError::PendingPublicTarget),
            _ => return Err(unknown_output_variant()),
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum PublicSubjectInput {
    Runtime,
    Command(CommandId),
    Agent(AgentId),
    Session(SessionId),
    Turn(TurnSubjectInput),
    Item(ItemSubjectInput),
    Interaction(InteractionSubjectInput),
    Skill(String),
}

impl PublicSubjectInput {
    fn into_semantic(self) -> Result<PublicSubject, TypedJsonError> {
        Ok(match self {
            Self::Runtime => PublicSubject::Runtime,
            Self::Command(command_id) => PublicSubject::Command(command_id),
            Self::Agent(agent_id) => PublicSubject::Agent(agent_id),
            Self::Session(session_id) => PublicSubject::Session(session_id),
            Self::Turn(value) => PublicSubject::Turn {
                session_id: value.session_id,
                turn_id: value.turn_id,
            },
            Self::Item(value) => PublicSubject::Item {
                session_id: value.session_id,
                turn_id: value.turn_id,
                item_id: value.item_id,
            },
            Self::Interaction(value) => PublicSubject::Interaction {
                session_id: value.session_id,
                turn_id: value.turn_id,
                item_id: value.item_id,
                request_id: value.request_id,
            },
            Self::Skill(value) => {
                PublicSubject::Skill(value.parse::<SkillId>().map_err(|_| invalid_scalar())?)
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnSubjectInput {
    session_id: SessionId,
    turn_id: TurnId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemSubjectInput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InteractionSubjectInput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
    request_id: RequestId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandResponseOutput<'a> {
    command_id: CommandId,
    completion: CommandCompletionOutput<'a>,
}

impl<'a> CommandResponseOutput<'a> {
    fn from_semantic(value: &'a CommandResponse) -> Self {
        Self {
            command_id: value.command_id(),
            completion: CommandCompletionOutput::from_semantic(value.completion()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum CommandCompletionOutput<'a> {
    Completed(CompletedCommandOutput<'a>),
    Rejected(CommandErrorOutput<'a>),
}

impl<'a> CommandCompletionOutput<'a> {
    fn from_semantic(value: &'a CommandCompletion) -> Self {
        match value {
            CommandCompletion::Completed { outcome, output } => {
                Self::Completed(CompletedCommandOutput {
                    outcome: CommandOutcomeOutput::from_semantic(outcome),
                    output: output.as_ref().map(|output| CommandOutputOutput {
                        text: output.text(),
                    }),
                })
            }
            CommandCompletion::Rejected(error) => {
                Self::Rejected(CommandErrorOutput::from_semantic(error))
            }
        }
    }
}

#[derive(Serialize)]
struct CompletedCommandOutput<'a> {
    outcome: CommandOutcomeOutput,
    output: Option<CommandOutputOutput<'a>>,
}

#[derive(Serialize)]
struct CommandOutputOutput<'a> {
    text: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum CommandOutcomeOutput {
    TurnStarted(TurnIdOutput),
    CommandOutput,
}

impl CommandOutcomeOutput {
    fn from_semantic(value: &CommandOutcome) -> Self {
        match value {
            CommandOutcome::TurnStarted { turn_id } => {
                Self::TurnStarted(TurnIdOutput { turn_id: *turn_id })
            }
            CommandOutcome::CommandOutput => Self::CommandOutput,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnIdOutput {
    turn_id: TurnId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandErrorOutput<'a> {
    code: CommandErrorCodeOutput,
    message: &'a str,
    retry: RetryAdviceOutput,
    subject: Option<PublicSubjectOutput<'a>>,
}

impl<'a> CommandErrorOutput<'a> {
    fn from_semantic(value: &'a CommandError) -> Self {
        Self {
            code: CommandErrorCodeOutput::from_semantic(value.code()),
            message: value.message(),
            retry: RetryAdviceOutput::from_semantic(value.retry()),
            subject: value.subject().map(PublicSubjectOutput::from_semantic),
        }
    }
}

#[derive(Serialize)]
struct CommandErrorCodeOutput {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<IngressLaneFullOutput>,
}

impl CommandErrorCodeOutput {
    fn from_semantic(value: CommandErrorCode) -> Self {
        let (kind, data) = match value {
            CommandErrorCode::InvalidArgument => ("invalid_argument", None),
            CommandErrorCode::NotFound => ("not_found", None),
            CommandErrorCode::CommandConflict => ("command_conflict", None),
            CommandErrorCode::StaleRevision => ("stale_revision", None),
            CommandErrorCode::AgentDisabled => ("agent_disabled", None),
            CommandErrorCode::AgentDeleted => ("agent_deleted", None),
            CommandErrorCode::SessionArchived => ("session_archived", None),
            CommandErrorCode::SessionDeleted => ("session_deleted", None),
            CommandErrorCode::SessionNotLoaded => ("session_not_loaded", None),
            CommandErrorCode::SessionNotReady => ("session_not_ready", None),
            CommandErrorCode::SessionBusy => ("session_busy", None),
            CommandErrorCode::ReloadValidationFailed => ("reload_validation_failed", None),
            CommandErrorCode::IngressLaneFull { lane } => (
                "ingress_lane_full",
                Some(IngressLaneFullOutput {
                    lane: PublicIngressLaneOutput::from_semantic(lane),
                }),
            ),
            CommandErrorCode::QueuedMessageNotQueued => ("queued_message_not_queued", None),
            CommandErrorCode::SubmitNotCancellable => ("submit_not_cancellable", None),
            CommandErrorCode::ExpectedTurnMismatch => ("expected_turn_mismatch", None),
            CommandErrorCode::TurnNotRunning => ("turn_not_running", None),
            CommandErrorCode::TurnCancelling => ("turn_cancelling", None),
            CommandErrorCode::TurnTerminal => ("turn_terminal", None),
            CommandErrorCode::InteractionNotFound => ("interaction_not_found", None),
            CommandErrorCode::InteractionAlreadyResolved => ("interaction_already_resolved", None),
            CommandErrorCode::InteractionFamilyMismatch => ("interaction_family_mismatch", None),
            CommandErrorCode::InvalidForkAnchor => ("invalid_fork_anchor", None),
            CommandErrorCode::Unauthorized => ("unauthorized", None),
            CommandErrorCode::Unavailable => ("unavailable", None),
            CommandErrorCode::DurableStateCorrupt => ("durable_state_corrupt", None),
            CommandErrorCode::DurableStateTooLarge => ("durable_state_too_large", None),
            CommandErrorCode::RuntimeClosing => ("runtime_closing", None),
        };
        Self { kind, data }
    }
}

#[derive(Serialize)]
struct IngressLaneFullOutput {
    lane: PublicIngressLaneOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum PublicIngressLaneOutput {
    TurnAdmission,
    Steer,
    FollowUp,
    InteractionControl,
    ToolControl,
}

impl PublicIngressLaneOutput {
    const fn from_semantic(value: PublicIngressLane) -> Self {
        match value {
            PublicIngressLane::TurnAdmission => Self::TurnAdmission,
            PublicIngressLane::Steer => Self::Steer,
            PublicIngressLane::FollowUp => Self::FollowUp,
            PublicIngressLane::InteractionControl => Self::InteractionControl,
            PublicIngressLane::ToolControl => Self::ToolControl,
        }
    }
}

#[derive(Serialize)]
struct RetryAdviceOutput {
    #[serde(rename = "type")]
    kind: &'static str,
}

impl RetryAdviceOutput {
    const fn from_semantic(value: RetryAdvice) -> Self {
        let kind = match value {
            RetryAdvice::DoNotRetry => "do_not_retry",
            RetryAdvice::RefreshAndRetry => "refresh_and_retry",
            RetryAdvice::UserActionRequired => "user_action_required",
        };
        Self { kind }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum PublicSubjectOutput<'a> {
    Runtime,
    Command(CommandId),
    Agent(AgentId),
    Session(SessionId),
    Turn(TurnSubjectOutput),
    Item(ItemSubjectOutput),
    Interaction(InteractionSubjectOutput),
    Skill(&'a str),
}

impl<'a> PublicSubjectOutput<'a> {
    fn from_semantic(value: &'a PublicSubject) -> Self {
        match value {
            PublicSubject::Runtime => Self::Runtime,
            PublicSubject::Command(command_id) => Self::Command(*command_id),
            PublicSubject::Agent(agent_id) => Self::Agent(*agent_id),
            PublicSubject::Session(session_id) => Self::Session(*session_id),
            PublicSubject::Turn {
                session_id,
                turn_id,
            } => Self::Turn(TurnSubjectOutput {
                session_id: *session_id,
                turn_id: *turn_id,
            }),
            PublicSubject::Item {
                session_id,
                turn_id,
                item_id,
            } => Self::Item(ItemSubjectOutput {
                session_id: *session_id,
                turn_id: *turn_id,
                item_id: *item_id,
            }),
            PublicSubject::Interaction {
                session_id,
                turn_id,
                item_id,
                request_id,
            } => Self::Interaction(InteractionSubjectOutput {
                session_id: *session_id,
                turn_id: *turn_id,
                item_id: *item_id,
                request_id: *request_id,
            }),
            PublicSubject::Skill(skill_id) => Self::Skill(skill_id.as_str()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnSubjectOutput {
    session_id: SessionId,
    turn_id: TurnId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemSubjectOutput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InteractionSubjectOutput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
    request_id: RequestId,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeQueryInput {
    Runtime(RuntimeReadQueryInput),
}

impl RuntimeQueryInput {
    fn into_semantic(self) -> RuntimeQuery {
        match self {
            Self::Runtime(RuntimeReadQueryInput::GetCapabilities) => {
                RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeReadQueryInput {
    GetCapabilities,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeQueryOutput {
    Runtime(RuntimeReadQueryOutput),
}

impl RuntimeQueryOutput {
    fn from_semantic(value: &RuntimeQuery) -> Self {
        match value {
            RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities) => {
                Self::Runtime(RuntimeReadQueryOutput::GetCapabilities)
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeReadQueryOutput {
    GetCapabilities,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SnapshotRequestInput {
    Runtime,
    Session(SessionIdInput),
}

impl SnapshotRequestInput {
    fn into_semantic(self) -> SnapshotRequest {
        match self {
            Self::Runtime => SnapshotRequest::Runtime,
            Self::Session(value) => SnapshotRequest::Session {
                session_id: value.session_id,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionIdInput {
    session_id: SessionId,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SnapshotRequestOutput {
    Runtime,
    Session(SessionIdOutput),
}

impl SnapshotRequestOutput {
    fn from_semantic(value: SnapshotRequest) -> Self {
        match value {
            SnapshotRequest::Runtime => Self::Runtime,
            SnapshotRequest::Session { session_id } => {
                Self::Session(SessionIdOutput { session_id })
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdOutput {
    session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubscriptionRequestInput {
    scope: SubscriptionScopeInput,
    include_progress: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SubscriptionScopeInput {
    Runtime,
    Session(SessionIdInput),
}

impl SubscriptionScopeInput {
    fn into_semantic(self) -> SubscriptionScope {
        match self {
            Self::Runtime => SubscriptionScope::Runtime,
            Self::Session(value) => SubscriptionScope::Session {
                session_id: value.session_id,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionRequestOutput {
    scope: SubscriptionScopeOutput,
    include_progress: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SubscriptionScopeOutput {
    Runtime,
    Session(SessionIdOutput),
}

impl SubscriptionScopeOutput {
    fn from_semantic(value: SubscriptionScope) -> Self {
        match value {
            SubscriptionScope::Runtime => Self::Runtime,
            SubscriptionScope::Session { session_id } => {
                Self::Session(SessionIdOutput { session_id })
            }
        }
    }
}

#[derive(Deserialize)]
struct QueryResponseInput {
    data: QueryResultInput,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum QueryResultInput {
    Runtime(RuntimeQueryResultInput),
}

impl QueryResultInput {
    fn into_semantic(self) -> Result<QueryResult, TypedJsonError> {
        match self {
            Self::Runtime(RuntimeQueryResultInput::Capabilities(values)) => {
                let values = values
                    .values
                    .into_iter()
                    .map(|value| CapabilityToken::from_str(&value))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| invalid_scalar())?;
                let capabilities = RuntimeCapabilities::for_v1(
                    values
                        .iter()
                        .filter_map(runtime_capability_from_token)
                        .collect(),
                )
                .map_err(|_| duplicate_value())?;
                Ok(QueryResult::Runtime(RuntimeQueryResult::Capabilities(
                    capabilities,
                )))
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeQueryResultInput {
    Capabilities(CapabilityValuesInput),
}

#[derive(Deserialize)]
struct CapabilityValuesInput {
    values: Vec<String>,
}

#[derive(Serialize)]
struct QueryResponseOutput<'a> {
    data: QueryResultOutput<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum QueryResultOutput<'a> {
    Runtime(RuntimeQueryResultOutput<'a>),
}

impl<'a> QueryResultOutput<'a> {
    fn from_semantic(value: &'a QueryResult) -> Self {
        match value {
            QueryResult::Runtime(RuntimeQueryResult::Capabilities(capabilities)) => {
                Self::Runtime(RuntimeQueryResultOutput::Capabilities(capabilities))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeQueryResultOutput<'a> {
    Capabilities(&'a RuntimeCapabilities),
}

fn validate_command_semantic_limits(
    command: &RuntimeCommand,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match command {
        RuntimeCommand::Turn(TurnCommand::Submit { intent, .. }) => {
            validate_skill_intent_count(
                intent.skills().len(),
                limits.prompt.max_skills_per_intent as usize,
            )
            .map_err(map_prompt_value_error)?;
            if let PromptBodyIntent::Text(text) = intent.body() {
                normalize_text_intent(text.text(), limits.text.max_text_intent_bytes as usize)
                    .map_err(map_prompt_value_error)?;
            }
        }
        RuntimeCommand::Session(SessionCommand::Create {
            definition,
            metadata,
            ..
        }) => {
            let workspace = definition.workspace();
            let root_count = workspace.additional_roots().len().saturating_add(1);
            if root_count > usize::from(limits.workspace.max_workspace_roots) {
                return Err(invalid_scalar());
            }
            for root in
                std::iter::once(workspace.primary_root()).chain(workspace.additional_roots())
            {
                if root.path().as_str().len()
                    > usize::try_from(limits.workspace.max_absolute_path_uri_bytes)
                        .unwrap_or(usize::MAX)
                {
                    return Err(invalid_scalar());
                }
            }
            let relative = workspace.cwd().relative_path().as_str();
            let relative_segments = if relative.is_empty() {
                0
            } else {
                relative.split('/').count()
            };
            if relative.len()
                > usize::try_from(limits.workspace.max_relative_path_bytes).unwrap_or(usize::MAX)
                || relative_segments > usize::from(limits.workspace.max_relative_path_segments)
                || definition.prompts().enabled().len()
                    > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX)
            {
                return Err(invalid_scalar());
            }
            NewSessionMetadata::new_with_limits(metadata.name(), metadata.description(), limits)
                .map_err(|_| invalid_scalar())?;
        }
        _ => {}
    }
    Ok(())
}

fn validate_command_response_semantic_limits(
    response: &CommandResponse,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match response.completion() {
        CommandCompletion::Completed {
            output: Some(output),
            ..
        } => validate_command_output(output.text(), limits.text.max_command_output_bytes as usize)
            .map_err(|_| invalid_scalar()),
        CommandCompletion::Rejected(error) => validate_command_error_message(
            error.message(),
            limits.text.max_diagnostic_message_bytes as usize,
        )
        .map_err(|_| invalid_scalar()),
        CommandCompletion::Completed { output: None, .. } => Ok(()),
    }
}

fn validate_command_response_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    validate_id::<CommandId>(required(object, "commandId")?)?;
    validate_adjacent_output(required(object, "completion")?, |kind, data| match kind {
        "completed" => validate_completed_command(data.ok_or_else(missing_required_field)?, limits),
        "rejected" => validate_command_error(data.ok_or_else(missing_required_field)?, limits),
        _ => Err(unknown_output_variant()),
    })
}

fn validate_completed_command(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let outcome = required(object, "outcome")?;
    let pending_outcome = match validate_command_outcome(outcome) {
        Err(TypedJsonError::PendingPublicTarget) => true,
        Err(error) => return Err(error),
        Ok(()) => false,
    };
    let expects_output = outcome
        .as_object()
        .and_then(|value| value.get("type"))
        .and_then(JsonNode::as_str)
        == Some("command_output");
    let mut has_output = false;
    if let Some(output) = object.get("output") {
        match output {
            JsonNode::Null => {}
            JsonNode::Object(output) => {
                has_output = true;
                let text = required(output, "text")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?;
                validate_command_output(text, limits.text.max_command_output_bytes as usize)
                    .map_err(|_| invalid_scalar())?;
            }
            _ => return Err(selected_wrong_json_type()),
        }
    }
    if expects_output != has_output {
        return Err(invalid_scalar());
    }
    if pending_outcome {
        Err(TypedJsonError::PendingPublicTarget)
    } else {
        Ok(())
    }
}

fn validate_command_outcome(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "turn_started" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_id::<TurnId>(required(object, "turnId")?)
        }
        "command_output" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "agent_created"
        | "agent_definition_updated"
        | "agent_metadata_updated"
        | "agent_status_changed"
        | "session_created"
        | "session_definition_updated"
        | "session_metadata_updated"
        | "session_forked"
        | "steer_queued"
        | "cancel_accepted" => pending_output_object(data),
        "agent_deleted"
        | "session_loaded"
        | "session_unloaded"
        | "session_archived"
        | "session_unarchived"
        | "session_deleted"
        | "runtime_reloaded"
        | "workspace_reloaded"
        | "submit_cancelled"
        | "follow_up_queued"
        | "queued_message_cancelled"
        | "interaction_resolved"
        | "no_change" => pending_output_unit(data),
        _ => Err(unknown_output_variant()),
    })
}

fn validate_command_error(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let code = validate_command_error_code(required(object, "code")?)?;
    let message = required(object, "message")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    validate_command_error_message(message, limits.text.max_diagnostic_message_bytes as usize)
        .map_err(|_| invalid_scalar())?;
    let pending_retry = match validate_retry_advice(required(object, "retry")?)? {
        ValidatedRetryAdvice::Current(retry) => {
            validate_command_error_contract(code, retry).map_err(|_| invalid_scalar())?;
            false
        }
        ValidatedRetryAdvice::RetryWithBackoff => {
            if !command_error_code_allows_retry_with_backoff(code) {
                return Err(invalid_scalar());
            }
            true
        }
    };
    if let Some(subject) = object.get("subject") {
        if !matches!(subject, JsonNode::Null) {
            validate_public_subject(subject)?;
        }
    }
    if pending_retry {
        Err(TypedJsonError::PendingPublicTarget)
    } else {
        Ok(())
    }
}

fn validate_command_error_code(node: &JsonNode) -> Result<CommandErrorCode, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    let data = object.get("data");
    let code = match kind {
        "ingress_lane_full" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            let lane = parse_ingress_lane(
                required(object, "lane")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?,
            )?;
            CommandErrorCode::IngressLaneFull { lane }
        }
        "invalid_argument" => CommandErrorCode::InvalidArgument,
        "not_found" => CommandErrorCode::NotFound,
        "command_conflict" => CommandErrorCode::CommandConflict,
        "stale_revision" => CommandErrorCode::StaleRevision,
        "agent_disabled" => CommandErrorCode::AgentDisabled,
        "agent_deleted" => CommandErrorCode::AgentDeleted,
        "session_archived" => CommandErrorCode::SessionArchived,
        "session_deleted" => CommandErrorCode::SessionDeleted,
        "session_not_loaded" => CommandErrorCode::SessionNotLoaded,
        "session_not_ready" => CommandErrorCode::SessionNotReady,
        "session_busy" => CommandErrorCode::SessionBusy,
        "reload_validation_failed" => CommandErrorCode::ReloadValidationFailed,
        "queued_message_not_queued" => CommandErrorCode::QueuedMessageNotQueued,
        "submit_not_cancellable" => CommandErrorCode::SubmitNotCancellable,
        "expected_turn_mismatch" => CommandErrorCode::ExpectedTurnMismatch,
        "turn_not_running" => CommandErrorCode::TurnNotRunning,
        "turn_cancelling" => CommandErrorCode::TurnCancelling,
        "turn_terminal" => CommandErrorCode::TurnTerminal,
        "interaction_not_found" => CommandErrorCode::InteractionNotFound,
        "interaction_already_resolved" => CommandErrorCode::InteractionAlreadyResolved,
        "interaction_family_mismatch" => CommandErrorCode::InteractionFamilyMismatch,
        "invalid_fork_anchor" => CommandErrorCode::InvalidForkAnchor,
        "unauthorized" => CommandErrorCode::Unauthorized,
        "unavailable" => CommandErrorCode::Unavailable,
        "durable_state_corrupt" => CommandErrorCode::DurableStateCorrupt,
        "durable_state_too_large" => CommandErrorCode::DurableStateTooLarge,
        "runtime_closing" => CommandErrorCode::RuntimeClosing,
        _ => return Err(unknown_output_variant()),
    };
    if !matches!(code, CommandErrorCode::IngressLaneFull { .. }) && data.is_some() {
        return Err(selected_wrong_json_type());
    }
    Ok(code)
}

enum ValidatedRetryAdvice {
    Current(RetryAdvice),
    RetryWithBackoff,
}

fn validate_retry_advice(node: &JsonNode) -> Result<ValidatedRetryAdvice, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    let data = object.get("data");
    let retry = match kind {
        "do_not_retry" => ValidatedRetryAdvice::Current(RetryAdvice::DoNotRetry),
        "refresh_and_retry" => ValidatedRetryAdvice::Current(RetryAdvice::RefreshAndRetry),
        "user_action_required" => ValidatedRetryAdvice::Current(RetryAdvice::UserActionRequired),
        "retry_with_backoff" => {
            data.ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            return Ok(ValidatedRetryAdvice::RetryWithBackoff);
        }
        _ => return Err(unknown_output_variant()),
    };
    if data.is_some() {
        return Err(selected_wrong_json_type());
    }
    Ok(retry)
}

fn parse_ingress_lane(value: &str) -> Result<PublicIngressLane, TypedJsonError> {
    Ok(match value {
        "turn_admission" => PublicIngressLane::TurnAdmission,
        "steer" => PublicIngressLane::Steer,
        "follow_up" => PublicIngressLane::FollowUp,
        "interaction_control" => PublicIngressLane::InteractionControl,
        "tool_control" => PublicIngressLane::ToolControl,
        _ => return Err(unknown_output_variant()),
    })
}

fn validate_public_subject(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "runtime" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "command" => validate_id::<CommandId>(data.ok_or_else(missing_required_field)?),
        "agent" => validate_id::<AgentId>(data.ok_or_else(missing_required_field)?),
        "session" => validate_id::<SessionId>(data.ok_or_else(missing_required_field)?),
        "turn" => validate_subject_ids(
            data.ok_or_else(missing_required_field)?,
            &["sessionId", "turnId"],
        ),
        "item" => validate_subject_ids(
            data.ok_or_else(missing_required_field)?,
            &["sessionId", "turnId", "itemId"],
        ),
        "interaction" => validate_subject_ids(
            data.ok_or_else(missing_required_field)?,
            &["sessionId", "turnId", "itemId", "requestId"],
        ),
        "skill" => {
            let value = data
                .ok_or_else(missing_required_field)?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?;
            value.parse::<SkillId>().map_err(|_| invalid_scalar())?;
            Ok(())
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_subject_ids(node: &JsonNode, fields: &[&str]) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    for field in fields {
        let value = required(object, field)?;
        match *field {
            "sessionId" => validate_id::<SessionId>(value)?,
            "turnId" => validate_id::<TurnId>(value)?,
            "itemId" => validate_id::<ItemId>(value)?,
            "requestId" => validate_id::<RequestId>(value)?,
            _ => return Err(TypedJsonError::EncodingInvariant),
        }
    }
    Ok(())
}

fn pending_output_object(data: Option<&JsonNode>) -> Result<(), TypedJsonError> {
    data.ok_or_else(missing_required_field)?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    Err(TypedJsonError::PendingPublicTarget)
}

fn pending_output_unit(data: Option<&JsonNode>) -> Result<(), TypedJsonError> {
    if data.is_some() {
        return Err(selected_wrong_json_type());
    }
    Err(TypedJsonError::PendingPublicTarget)
}

fn validate_command_request_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["commandId", "command"])?;
    validate_id::<CommandId>(required(object, "commandId")?)?;
    let command = required(object, "command")?;
    validate_adjacent_input(command, |kind, data| match kind {
        "runtime" => {
            let value = data
                .ok_or_else(missing_required_field)?
                .as_str()
                .ok_or_else(selected_wrong_json_type)?;
            if value != "reload_shared_resources" {
                return Err(unknown_input_variant());
            }
            Ok(())
        }
        "session" => validate_session_command(data.ok_or_else(missing_required_field)?, limits),
        "turn" => validate_turn_command(data.ok_or_else(missing_required_field)?, limits),
        "agent" => validate_pending_command_family(
            data,
            &[
                "create",
                "update_definition",
                "update_metadata",
                "set_status",
                "delete",
            ],
        ),
        "interaction" => validate_pending_command_family(data, &["resolve"]),
        "command_surface" => {
            validate_pending_command_family(data, &["execute_text", "execute_catalog"])
        }
        _ => Err(unknown_input_variant()),
    })
}

fn validate_session_command(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "load" | "unload" => validate_session_id_object(data.ok_or_else(missing_required_field)?),
        "create" => {
            validate_create_session_command(data.ok_or_else(missing_required_field)?, limits)
        }
        "update_definition"
        | "upgrade_agent_revision"
        | "update_metadata"
        | "reload_workspace"
        | "archive"
        | "unarchive"
        | "delete"
        | "fork" => pending_object(data),
        _ => Err(unknown_input_variant()),
    })
}

fn validate_create_session_command(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["agentId", "definition", "metadata"],
    )?;
    validate_id::<AgentId>(required(object, "agentId")?)?;
    validate_new_session_definition(required(object, "definition")?, limits)?;
    validate_new_session_metadata(required(object, "metadata")?, limits)
}

fn validate_new_session_definition(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["workspace", "model", "prompts"],
    )?;
    validate_workspace_definition(required(object, "workspace")?, limits)?;
    validate_session_model_config(required(object, "model")?)?;
    validate_session_prompt_selection(required(object, "prompts")?, limits)
}

fn validate_workspace_definition(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["primaryRoot", "additionalRoots", "cwd"],
    )?;
    let additional = required(object, "additionalRoots")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if additional.len().saturating_add(1) > usize::from(limits.workspace.max_workspace_roots) {
        return Err(invalid_scalar());
    }

    let mut keys = BTreeSet::new();
    let mut uris = BTreeSet::new();
    let (primary_key, primary_uri) =
        validate_workspace_root(required(object, "primaryRoot")?, limits)?;
    keys.insert(primary_key);
    uris.insert(primary_uri);
    for root in additional {
        let (key, uri) = validate_workspace_root(root, limits)?;
        if !keys.insert(key) || !uris.insert(uri) {
            return Err(duplicate_value());
        }
    }
    let cwd_root = validate_workspace_cwd(required(object, "cwd")?, limits)?;
    if !keys.contains(cwd_root) {
        return Err(invalid_scalar());
    }
    Ok(())
}

fn validate_workspace_root(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(&str, &str), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["key", "path", "requestedAccess", "sources"],
    )?;
    let key = required(object, "key")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    key.parse::<WorkspaceRootKey>()
        .map_err(|_| invalid_scalar())?;
    let path = required(object, "path")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    if path.len()
        > usize::try_from(limits.workspace.max_absolute_path_uri_bytes).unwrap_or(usize::MAX)
    {
        return Err(invalid_scalar());
    }
    path.parse::<CanonicalFileUri>()
        .map_err(|_| invalid_scalar())?;
    match required(object, "requestedAccess")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "read_only" | "read_write" => {}
        _ => return Err(unknown_input_variant()),
    }
    let sources = input_object(required(object, "sources")?)?;
    reject_unknown_fields(sources.keys().map(AsRef::as_ref), &["prompt", "skill"])?;
    for field in ["prompt", "skill"] {
        if !matches!(required(sources, field)?, JsonNode::Bool(_)) {
            return Err(typed_wrong_json_type());
        }
    }
    Ok((key, path))
}

fn validate_workspace_cwd(node: &JsonNode, limits: ProtocolLimits) -> Result<&str, TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["root", "relativePath"])?;
    let root = required(object, "root")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    root.parse::<WorkspaceRootKey>()
        .map_err(|_| invalid_scalar())?;
    let relative = required(object, "relativePath")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    if relative.len()
        > usize::try_from(limits.workspace.max_relative_path_bytes).unwrap_or(usize::MAX)
        || (!relative.is_empty()
            && relative.split('/').count()
                > usize::from(limits.workspace.max_relative_path_segments))
    {
        return Err(invalid_scalar());
    }
    relative
        .parse::<WorkspaceRelativePath>()
        .map_err(|_| invalid_scalar())?;
    Ok(root)
}

fn validate_session_model_config(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["selection", "reasoning", "maxOutputTokens"],
    )?;
    let selection = input_object(required(object, "selection")?)?;
    reject_unknown_fields(
        selection.keys().map(AsRef::as_ref),
        &["providerId", "modelId"],
    )?;
    required(selection, "providerId")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<ProviderId>()
        .map_err(|_| invalid_scalar())?;
    required(selection, "modelId")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<ModelId>()
        .map_err(|_| invalid_scalar())?;
    match required(object, "reasoning")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "auto" | "disabled" | "low" | "medium" | "high" => {}
        _ => return Err(unknown_input_variant()),
    }
    match object.get("maxOutputTokens") {
        None | Some(JsonNode::Null) => Ok(()),
        Some(node) => validate_nonzero_u32(node),
    }
}

fn validate_nonzero_u32(node: &JsonNode) -> Result<(), TypedJsonError> {
    let literal = node
        .as_number()
        .map(|number| number.raw())
        .ok_or_else(typed_wrong_json_type)?;
    if !literal.bytes().all(|byte| byte.is_ascii_digit())
        || literal
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .is_none()
    {
        return Err(invalid_scalar());
    }
    Ok(())
}

fn validate_session_prompt_selection(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["enabled"])?;
    let enabled = required(object, "enabled")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if enabled.len() > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX) {
        return Err(invalid_scalar());
    }
    let mut unique = BTreeSet::new();
    for prompt in enabled {
        let prompt = prompt.as_str().ok_or_else(typed_wrong_json_type)?;
        prompt.parse::<PromptId>().map_err(|_| invalid_scalar())?;
        if !unique.insert(prompt) {
            return Err(duplicate_value());
        }
    }
    Ok(())
}

fn validate_new_session_metadata(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["name", "description"])?;
    let name = object
        .get("name")
        .map(nullable_string)
        .transpose()?
        .flatten();
    let description = object
        .get("description")
        .map(nullable_string)
        .transpose()?
        .flatten();
    NewSessionMetadata::new_with_limits(name, description, limits).map_err(|_| invalid_scalar())?;
    Ok(())
}

fn nullable_string(node: &JsonNode) -> Result<Option<&str>, TypedJsonError> {
    match node {
        JsonNode::Null => Ok(None),
        _ => node.as_str().map(Some).ok_or_else(typed_wrong_json_type),
    }
}

fn validate_turn_command(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "submit" => validate_submit_command(data.ok_or_else(missing_required_field)?, limits),
        "cancel" => validate_cancel_command(data.ok_or_else(missing_required_field)?),
        "steer" | "follow_up" | "cancel_queued_message" => pending_object(data),
        _ => Err(unknown_input_variant()),
    })
}

fn validate_submit_command(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["sessionId", "intent"])?;
    validate_id::<SessionId>(required(object, "sessionId")?)?;
    validate_prompt_intent(required(object, "intent")?, limits)
}

fn validate_prompt_intent(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["body", "skills"])?;
    validate_adjacent_input(required(object, "body")?, |kind, data| match kind {
        "empty" if data.is_none() => Ok(()),
        "empty" => Err(selected_wrong_json_type()),
        "text" => {
            let object = input_object(data.ok_or_else(missing_required_field)?)?;
            reject_unknown_fields(object.keys().map(AsRef::as_ref), &["text"])?;
            let text = required(object, "text")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?;
            normalize_text_intent(text, limits.text.max_text_intent_bytes as usize)
                .map_err(map_prompt_value_error)?;
            Ok(())
        }
        _ => Err(unknown_input_variant()),
    })?;
    let skills = required(object, "skills")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    validate_skill_intent_count(skills.len(), limits.prompt.max_skills_per_intent as usize)
        .map_err(map_prompt_value_error)?;
    let mut unique = BTreeSet::new();
    for skill in skills {
        let object = input_object(skill)?;
        reject_unknown_fields(object.keys().map(AsRef::as_ref), &["skillId"])?;
        let skill_id = required(object, "skillId")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?;
        skill_id.parse::<SkillId>().map_err(|_| invalid_scalar())?;
        if !unique.insert(skill_id) {
            return Err(duplicate_value());
        }
    }
    Ok(())
}

fn validate_cancel_command(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["sessionId", "target"])?;
    validate_id::<SessionId>(required(object, "sessionId")?)?;
    validate_adjacent_input(required(object, "target")?, |kind, data| match kind {
        "submit" => validate_id::<CommandId>(data.ok_or_else(missing_required_field)?),
        "turn" => validate_id::<TurnId>(data.ok_or_else(missing_required_field)?),
        _ => Err(unknown_input_variant()),
    })
}

fn validate_pending_command_family(
    data: Option<&JsonNode>,
    variants: &[&str],
) -> Result<(), TypedJsonError> {
    validate_adjacent_input(data.ok_or_else(missing_required_field)?, |kind, data| {
        if variants.contains(&kind) {
            pending_object(data)
        } else {
            Err(unknown_input_variant())
        }
    })
}

fn pending_object(data: Option<&JsonNode>) -> Result<(), TypedJsonError> {
    data.ok_or_else(missing_required_field)?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    Err(TypedJsonError::PendingPublicTarget)
}

fn validate_runtime_query_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "runtime" => {
            let value = data
                .ok_or_else(missing_required_field)?
                .as_str()
                .ok_or_else(selected_wrong_json_type)?;
            match value {
                "get_capabilities" => Ok(()),
                "get_runtime_info" | "list_loaded_sessions" => {
                    Err(TypedJsonError::PendingPublicTarget)
                }
                _ => Err(unknown_input_variant()),
            }
        }
        "agent" | "session" | "command_surface" | "model" | "prompt" | "skill" | "tool"
        | "usage" | "diagnostics" => Err(TypedJsonError::PendingPublicTarget),
        _ => Err(unknown_input_variant()),
    })
}

fn validate_snapshot_request_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "runtime" if data.is_none() => Ok(()),
        "runtime" => Err(selected_wrong_json_type()),
        "session" => validate_session_id_object(data.ok_or_else(missing_required_field)?),
        _ => Err(unknown_input_variant()),
    })
}

fn validate_subscription_request_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["scope", "includeProgress"],
    )?;
    validate_adjacent_input(required(object, "scope")?, |kind, data| match kind {
        "runtime" if data.is_none() => Ok(()),
        "runtime" => Err(selected_wrong_json_type()),
        "session" => validate_session_id_object(data.ok_or_else(missing_required_field)?),
        _ => Err(unknown_input_variant()),
    })?;
    if !matches!(required(object, "includeProgress")?, JsonNode::Bool(_)) {
        return Err(typed_wrong_json_type());
    }
    Ok(())
}

fn validate_query_response_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let data = required(object, "data")?;
    validate_adjacent_output(data, |kind, data| match kind {
        "runtime" => {
            validate_adjacent_output(data.ok_or_else(missing_required_field)?, |kind, data| {
                match kind {
                    "capabilities" => {
                        let object = data
                            .ok_or_else(missing_required_field)?
                            .as_object()
                            .ok_or_else(selected_wrong_json_type)?;
                        let values = required(object, "values")?
                            .as_array()
                            .ok_or_else(selected_wrong_json_type)?;
                        if values.iter().any(|value| value.as_str().is_none()) {
                            return Err(typed_wrong_json_type());
                        }
                        Ok(())
                    }
                    "info" | "loaded_sessions" => Err(TypedJsonError::PendingPublicTarget),
                    _ => Err(unknown_output_variant()),
                }
            })
        }
        "agent" | "session" | "command_surface" | "model" | "prompt" | "skill" | "tool"
        | "usage" | "diagnostics" => Err(TypedJsonError::PendingPublicTarget),
        _ => Err(unknown_output_variant()),
    })
}

fn validate_session_id_object(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["sessionId"])?;
    validate_id::<SessionId>(required(object, "sessionId")?)
}

fn validate_adjacent_input(
    node: &JsonNode,
    validate: impl FnOnce(&str, Option<&JsonNode>) -> Result<(), TypedJsonError>,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["type", "data"])?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    validate(kind, object.get("data"))
}

fn validate_adjacent_output(
    node: &JsonNode,
    validate: impl FnOnce(&str, Option<&JsonNode>) -> Result<(), TypedJsonError>,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    validate(kind, object.get("data"))
}

fn input_object(
    node: &JsonNode,
) -> Result<&std::collections::BTreeMap<Box<str>, JsonNode>, TypedJsonError> {
    node.as_object().ok_or_else(selected_wrong_json_type)
}

fn required<'a>(
    object: &'a std::collections::BTreeMap<Box<str>, JsonNode>,
    field: &str,
) -> Result<&'a JsonNode, TypedJsonError> {
    object.get(field).ok_or_else(missing_required_field)
}

fn reject_unknown_fields<'a>(
    fields: impl IntoIterator<Item = &'a str>,
    allowed: &[&str],
) -> Result<(), TypedJsonError> {
    if fields.into_iter().any(|field| !allowed.contains(&field)) {
        return Err(public_fault(
            PublicDecodeStage::SelectedSchema,
            PublicDecodeCode::UnknownInputField,
        ));
    }
    Ok(())
}

fn validate_id<T: FromStr>(node: &JsonNode) -> Result<(), TypedJsonError> {
    let value = node.as_str().ok_or_else(typed_wrong_json_type)?;
    T::from_str(value).map_err(|_| {
        public_fault(
            PublicDecodeStage::TypedScalar,
            PublicDecodeCode::NoncanonicalId,
        )
    })?;
    Ok(())
}

fn selected_wrong_json_type() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::WrongJsonType,
    )
}

fn typed_wrong_json_type() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::TypedScalar,
        PublicDecodeCode::WrongJsonType,
    )
}

fn missing_required_field() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::MissingRequiredField,
    )
}

fn invalid_scalar() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::TypedScalar,
        PublicDecodeCode::InvalidScalar,
    )
}

fn duplicate_value() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::DuplicateValue,
    )
}

fn map_prompt_value_error(error: PromptValueError) -> TypedJsonError {
    match error {
        PromptValueError::DuplicateSkill => duplicate_value(),
        PromptValueError::EmptyText
        | PromptValueError::TextTooLong
        | PromptValueError::UnsafeText
        | PromptValueError::TooManySkills => invalid_scalar(),
        PromptValueError::InvalidPartCount | PromptValueError::InvalidContributionStamp => {
            TypedJsonError::EncodingInvariant
        }
    }
}

fn unknown_input_variant() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::UnknownInputVariant,
    )
}

fn unknown_output_variant() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::SelectedSchema,
        PublicDecodeCode::UnknownOutputVariant,
    )
}

fn public_fault(stage: PublicDecodeStage, code: PublicDecodeCode) -> TypedJsonError {
    PublicDecodeError::new(stage, code).into()
}

fn parse_runtime_dispatch_error(value: &str) -> Option<RuntimeDispatchError> {
    Some(match value {
        "invalid_envelope" => RuntimeDispatchError::InvalidEnvelope,
        "request_too_large" => RuntimeDispatchError::RequestTooLarge,
        "runtime_closed" => RuntimeDispatchError::RuntimeClosed,
        "internal_dispatch_unavailable" => RuntimeDispatchError::InternalDispatchUnavailable,
        _ => return None,
    })
}

const fn runtime_dispatch_error_name(value: RuntimeDispatchError) -> &'static str {
    match value {
        RuntimeDispatchError::InvalidEnvelope => "invalid_envelope",
        RuntimeDispatchError::RequestTooLarge => "request_too_large",
        RuntimeDispatchError::RuntimeClosed => "runtime_closed",
        RuntimeDispatchError::InternalDispatchUnavailable => "internal_dispatch_unavailable",
    }
}
