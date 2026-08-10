use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::agent_session_lifecycle::{
    AgentRevisionRef, AgentStatus, AgentUsableStatus, ForkAnchor, ForkSourceKind,
    NewAgentDefinition, NewAgentMetadata, SessionModelConfig,
};
use crate::model_gateway::{ModelId, ModelSelection, ProviderId, ReasoningPreference};
use crate::prompt::{
    AgentPromptSelection, PromptBodyIntent, PromptId, PromptIntent, PromptValueError,
    SessionPromptSelection, SkillIntent, TextIntent, normalize_text_intent,
    validate_skill_intent_count,
};
use crate::runtime_interface::{
    AgentCommand, AgentMetadataView, AgentQuery, AgentQueryResult, AgentSummary, CommandCompletion,
    CommandError, CommandErrorCode, CommandOutcome, CommandOutput, CommandRequest, CommandResponse,
    CurrentTurnView, EventFrame, EventRoute, InteractionCommand, InteractionView, ItemContentView,
    ItemProgressContentKind, ItemStatusView, ItemView, ModelCallPurpose, NewSessionDefinition,
    NewSessionMetadata, Page, PageRequest, ProgressEvent, ProgressEventKind, ProgressUpdate,
    PublicCancelTarget, PublicIngressLane, PublicSubject, QueryError, QueryErrorCode,
    QueryResponse, QueryResult, QueuedFollowUpView, QueuedSteerView, RetryAdvice,
    RuntimeCapabilities, RuntimeCommand, RuntimeDiagnosticView, RuntimeDispatchError,
    RuntimeEventDetail, RuntimeLifecycleCommand, RuntimeQuery, RuntimeQueryResult,
    RuntimeReadQuery, RuntimeRequest, RuntimeSnapshot, RuntimeStateEventKind, RuntimeStatusView,
    RuntimeView, SessionCommand, SessionDefinitionSummary, SessionDiagnosticView,
    SessionEventDetail, SessionExecutionView, SessionForkProvenanceView, SessionLifecycleView,
    SessionMetadataView, SessionQuery, SessionQueryResult, SessionQueueView, SessionReadinessView,
    SessionRecordingState, SessionRecordingView, SessionSnapshot, SessionStateEventKind,
    SessionSummary, SessionUsageView, SnapshotRequest, SnapshotResponse, StateEvent, StateEventMsg,
    SubmitAdmissionStateView, SubmitAdmissionView, SubscriptionClosed, SubscriptionRequest,
    SubscriptionScope, TurnCommand, TurnExecutionPhaseView, TurnFailureView, TurnInterruptionView,
    TurnStatusView, TurnTerminalView, validate_command_error_contract,
    validate_command_error_message, validate_command_output,
};
use crate::skills::SkillId;
use crate::tools::{
    ToolApprovalDecisionInput, ToolApprovalOptionKindView, ToolApprovalOptionView,
    ToolApprovalRequestView, ToolCallId, ToolRequirementSummaryView, UserQuestionAnswer,
    UserQuestionAnswerValue, UserQuestionChoice, UserQuestionField, UserQuestionFieldAnswer,
    UserQuestionInput, UserQuestionRequest,
};
use crate::turn_item_interaction::{
    InteractionRequestView, InteractionResolutionInput, UserMessageSource,
};
use crate::workspace::{
    RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput,
    WorkspaceDefinitionSummaryView, WorkspaceRootInput, WorkspaceRootKey, WorkspaceRootSummaryView,
    WorkspaceSourcePolicy,
};

use super::bounded_json::JsonNode;
use super::limits::{CapabilityToken, ProtocolLimits, runtime_capability_from_token};
use super::scalar::{
    AgentId, AgentMetadataRevision, AgentRevision, CommandId, InteractionResolutionKey, ItemId,
    PageCursor, RequestId, SessionDefinitionRevision, SessionId, SessionMetadataRevision, TurnId,
};
use super::typed_json::{
    PublicDecodeCode, PublicDecodeError, PublicDecodeStage, PublicJsonKind, TypedJsonError,
    WireV1Codec,
};
use super::{CanonicalFileUri, Duration, Money, Timestamp, WorkspaceRelativePath};

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

    pub fn decode_event_frame(&self, input: &[u8]) -> Result<EventFrame, TypedJsonError> {
        decode_event_frame(&self.codec, input)
    }

    pub fn encode_event_frame(&self, frame: &EventFrame) -> Result<Vec<u8>, TypedJsonError> {
        encode_event_frame(&self.codec, frame)
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

    pub fn decode_query_error(&self, input: &[u8]) -> Result<QueryError, TypedJsonError> {
        decode_query_error(&self.codec, input)
    }

    pub fn encode_query_error(&self, error: &QueryError) -> Result<Vec<u8>, TypedJsonError> {
        encode_query_error(&self.codec, error)
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

fn decode_query_error(codec: &WireV1Codec, input: &[u8]) -> Result<QueryError, TypedJsonError> {
    let decoded: QueryErrorInput =
        codec.decode_with_shape(PublicJsonKind::Response, input, |node| {
            validate_query_error_shape(node, codec.limits())
        })?;
    decoded.into_semantic(codec.limits())
}

fn encode_query_error(codec: &WireV1Codec, error: &QueryError) -> Result<Vec<u8>, TypedJsonError> {
    validate_command_error_message(
        error.message(),
        codec.limits().text.max_diagnostic_message_bytes as usize,
    )
    .map_err(|_| invalid_scalar())?;
    codec.encode(
        PublicJsonKind::Response,
        &QueryErrorOutput::from_semantic(error),
    )
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
        codec.decode_with_shape(PublicJsonKind::Request, input, |node| {
            validate_runtime_query_shape(node, codec.limits())
        })?;
    Ok(decoded.into_semantic())
}

fn encode_runtime_query(
    codec: &WireV1Codec,
    query: &RuntimeQuery,
) -> Result<Vec<u8>, TypedJsonError> {
    validate_runtime_query_semantic_limits(query, codec.limits())?;
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
    let decoded: QueryResponseInput =
        codec.decode_with_shape(PublicJsonKind::Response, input, |node| {
            validate_query_response_shape(node, codec.limits())
        })?;
    Ok(QueryResponse::new(
        decoded.data.into_semantic(codec.limits())?,
    ))
}

fn encode_query_response(
    codec: &WireV1Codec,
    response: &QueryResponse,
) -> Result<Vec<u8>, TypedJsonError> {
    validate_query_response_semantic_limits(response, codec.limits())?;
    codec.encode(
        PublicJsonKind::Response,
        &QueryResponseOutput {
            data: QueryResultOutput::from_semantic(response.data()),
        },
    )
}

fn decode_event_frame(codec: &WireV1Codec, input: &[u8]) -> Result<EventFrame, TypedJsonError> {
    let decoded: EventFrameInput = codec.decode_event_frame_with_shape(input, |node| {
        validate_event_frame_shape(node, codec.limits())
    })?;
    decoded.into_semantic(codec.limits())
}

fn encode_event_frame(codec: &WireV1Codec, frame: &EventFrame) -> Result<Vec<u8>, TypedJsonError> {
    validate_event_frame_semantic_limits(frame, codec.limits())?;
    let kind = match frame {
        EventFrame::Snapshot(SnapshotResponse::Runtime(_)) => PublicJsonKind::RuntimeSnapshot,
        EventFrame::Snapshot(SnapshotResponse::Session(_)) => PublicJsonKind::SessionSnapshot,
        EventFrame::State(_) => PublicJsonKind::StateEvent,
        EventFrame::Progress(_) => PublicJsonKind::ProgressEvent,
        EventFrame::Closed(_) => PublicJsonKind::Response,
    };
    codec.encode(kind, &EventFrameOutput::from_semantic(frame))
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
    Agent(AgentCommandInput),
    Session(SessionCommandInput),
    Turn(TurnCommandInput),
    Interaction(InteractionCommandInput),
}

impl RuntimeCommandInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<RuntimeCommand, TypedJsonError> {
        Ok(match self {
            Self::Runtime(RuntimeLifecycleCommandInput::ReloadSharedResources) => {
                RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources)
            }
            Self::Agent(command) => RuntimeCommand::Agent(command.into_semantic(limits)?),
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
            Self::Session(SessionCommandInput::Archive(value)) => {
                RuntimeCommand::Session(SessionCommand::Archive {
                    session_id: value.session_id,
                })
            }
            Self::Session(SessionCommandInput::Unarchive(value)) => {
                RuntimeCommand::Session(SessionCommand::Unarchive {
                    session_id: value.session_id,
                })
            }
            Self::Session(SessionCommandInput::Delete(value)) => {
                RuntimeCommand::Session(SessionCommand::Delete {
                    session_id: value.session_id,
                })
            }
            Self::Session(SessionCommandInput::Fork(value)) => {
                RuntimeCommand::Session(SessionCommand::Fork {
                    source_session_id: value.source_session_id,
                    anchor: value.anchor.into_semantic(),
                })
            }
            Self::Turn(TurnCommandInput::Submit(value)) => {
                RuntimeCommand::Turn(TurnCommand::Submit {
                    session_id: value.session_id,
                    intent: value.intent.into_semantic(limits)?,
                })
            }
            Self::Turn(TurnCommandInput::Steer(value)) => {
                RuntimeCommand::Turn(TurnCommand::Steer {
                    session_id: value.session_id,
                    expected_turn_id: value.expected_turn_id,
                    intent: value.intent.into_semantic(limits)?,
                })
            }
            Self::Turn(TurnCommandInput::FollowUp(value)) => {
                RuntimeCommand::Turn(TurnCommand::FollowUp {
                    session_id: value.session_id,
                    intent: value.intent.into_semantic(limits)?,
                })
            }
            Self::Turn(TurnCommandInput::CancelQueuedMessage(value)) => {
                RuntimeCommand::Turn(TurnCommand::CancelQueuedMessage {
                    session_id: value.session_id,
                    target_command_id: value.target_command_id,
                })
            }
            Self::Turn(TurnCommandInput::Cancel(value)) => {
                RuntimeCommand::Turn(TurnCommand::Cancel {
                    session_id: value.session_id,
                    target: value.target.into_semantic(),
                })
            }
            Self::Interaction(InteractionCommandInput::Resolve(value)) => {
                RuntimeCommand::Interaction(InteractionCommand::Resolve {
                    session_id: value.session_id,
                    expected_turn_id: value.expected_turn_id,
                    item_id: value.item_id,
                    request_id: value.request_id,
                    resolution: value.resolution.into_semantic()?,
                    resolution_key: value.resolution_key,
                })
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum AgentCommandInput {
    Create(CreateAgentCommandInput),
    SetStatus(SetAgentStatusCommandInput),
    Delete(DeleteAgentCommandInput),
}

impl AgentCommandInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<AgentCommand, TypedJsonError> {
        Ok(match self {
            Self::Create(value) => AgentCommand::Create {
                definition: value.definition.into_semantic(limits)?,
                metadata: value.metadata.into_semantic(limits)?,
            },
            Self::SetStatus(value) => AgentCommand::SetStatus {
                agent_id: value.agent_id,
                expected_status: value.expected_status.into_semantic(),
                status: value.status.into_semantic(),
            },
            Self::Delete(value) => AgentCommand::Delete {
                agent_id: value.agent_id,
                expected_status: value.expected_status.into_semantic(),
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateAgentCommandInput {
    definition: NewAgentDefinitionInput,
    metadata: NewAgentMetadataInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAgentDefinitionInput {
    prompts: AgentPromptSelectionInput,
}

impl NewAgentDefinitionInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<NewAgentDefinition, TypedJsonError> {
        Ok(NewAgentDefinition::new(self.prompts.into_semantic(limits)?))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentPromptSelectionInput {
    enabled: Vec<String>,
}

impl AgentPromptSelectionInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<AgentPromptSelection, TypedJsonError> {
        let enabled = self
            .enabled
            .into_iter()
            .map(|value| value.parse::<PromptId>().map_err(|_| invalid_scalar()))
            .collect::<Result<Vec<_>, _>>()?;
        AgentPromptSelection::new_with_maximum(
            enabled,
            usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX),
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAgentMetadataInput {
    name: String,
    description: Option<String>,
}

impl NewAgentMetadataInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<NewAgentMetadata, TypedJsonError> {
        NewAgentMetadata::new_with_limits(self.name, self.description, limits)
            .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetAgentStatusCommandInput {
    agent_id: AgentId,
    expected_status: AgentStatusInput,
    status: AgentUsableStatusInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteAgentCommandInput {
    agent_id: AgentId,
    expected_status: AgentStatusInput,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentUsableStatusInput {
    Enabled,
    Disabled,
}

impl AgentUsableStatusInput {
    const fn into_semantic(self) -> AgentUsableStatus {
        match self {
            Self::Enabled => AgentUsableStatus::Enabled,
            Self::Disabled => AgentUsableStatus::Disabled,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum InteractionCommandInput {
    Resolve(ResolveInteractionCommandInput),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolveInteractionCommandInput {
    session_id: SessionId,
    expected_turn_id: TurnId,
    item_id: ItemId,
    request_id: RequestId,
    resolution: InteractionResolutionCommandInput,
    resolution_key: InteractionResolutionKey,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum InteractionResolutionCommandInput {
    ToolApproval(ToolApprovalDecisionCommandInput),
    UserAnswer(UserQuestionAnswerCommandInput),
    Cancelled,
}

impl InteractionResolutionCommandInput {
    fn into_semantic(self) -> Result<InteractionResolutionInput, TypedJsonError> {
        Ok(match self {
            Self::ToolApproval(value) => {
                InteractionResolutionInput::ToolApproval(value.into_semantic())
            }
            Self::UserAnswer(value) => {
                InteractionResolutionInput::UserAnswer(value.into_semantic()?)
            }
            Self::Cancelled => InteractionResolutionInput::Cancelled,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ToolApprovalDecisionCommandInput {
    Allow(OptionIndexInput),
    Deny,
}

impl ToolApprovalDecisionCommandInput {
    const fn into_semantic(self) -> ToolApprovalDecisionInput {
        match self {
            Self::Allow(value) => ToolApprovalDecisionInput::Allow {
                option_index: value.option_index,
            },
            Self::Deny => ToolApprovalDecisionInput::Deny,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OptionIndexInput {
    option_index: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserQuestionAnswerCommandInput {
    answers: Vec<UserQuestionFieldAnswerCommandInput>,
}

impl UserQuestionAnswerCommandInput {
    fn into_semantic(self) -> Result<UserQuestionAnswer, TypedJsonError> {
        UserQuestionAnswer::new(
            self.answers
                .into_iter()
                .map(UserQuestionFieldAnswerCommandInput::into_semantic)
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UserQuestionFieldAnswerCommandInput {
    question_index: u32,
    value: UserQuestionAnswerValueCommandInput,
}

impl UserQuestionFieldAnswerCommandInput {
    fn into_semantic(self) -> Result<UserQuestionFieldAnswer, TypedJsonError> {
        match self.value {
            UserQuestionAnswerValueCommandInput::Text(text) => {
                UserQuestionFieldAnswer::text(self.question_index, text.text)
                    .map_err(|_| invalid_scalar())
            }
            UserQuestionAnswerValueCommandInput::Choice(value) => Ok(
                UserQuestionFieldAnswer::choice(self.question_index, value.option_index),
            ),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum UserQuestionAnswerValueCommandInput {
    Text(TextValueInput),
    Choice(OptionIndexInput),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextValueInput {
    text: String,
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
    Archive(SessionIdInput),
    Unarchive(SessionIdInput),
    Delete(SessionIdInput),
    Fork(ForkSessionCommandInput),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForkSessionCommandInput {
    source_session_id: SessionId,
    anchor: ForkAnchorInput,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ForkAnchorInput {
    Genesis,
    BeforeUserMessage(ForkItemAnchorInput),
    AfterUserMessage(ForkItemAnchorInput),
    BeforeFinalAgentMessage(ForkItemAnchorInput),
    AfterFinalAgentMessage(ForkItemAnchorInput),
}

impl ForkAnchorInput {
    const fn into_semantic(self) -> ForkAnchor {
        match self {
            Self::Genesis => ForkAnchor::Genesis,
            Self::BeforeUserMessage(value) => ForkAnchor::BeforeUserMessage {
                item_id: value.item_id,
            },
            Self::AfterUserMessage(value) => ForkAnchor::AfterUserMessage {
                item_id: value.item_id,
            },
            Self::BeforeFinalAgentMessage(value) => ForkAnchor::BeforeFinalAgentMessage {
                item_id: value.item_id,
            },
            Self::AfterFinalAgentMessage(value) => ForkAnchor::AfterFinalAgentMessage {
                item_id: value.item_id,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForkItemAnchorInput {
    item_id: crate::wire::ItemId,
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
enum EventFrameInput {
    Snapshot(SnapshotResponseInput),
    State(StateEventInput),
    Progress(ProgressEventInput),
    Closed(SubscriptionClosedInput),
}

struct UnsupportedObservationInput;

impl<'de> Deserialize<'de> for UnsupportedObservationInput {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom(
            "observation value belongs to a pending protocol slice",
        ))
    }
}

impl EventFrameInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<EventFrame, TypedJsonError> {
        Ok(match self {
            Self::Snapshot(snapshot) => EventFrame::Snapshot(snapshot.into_semantic(limits)?),
            Self::State(event) => EventFrame::State(event.into_semantic(limits)?),
            Self::Progress(event) => EventFrame::Progress(event.into_semantic()?),
            Self::Closed(reason) => EventFrame::Closed(reason.into_semantic()),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEventInput {
    timestamp: Timestamp,
    route: EventRouteInput,
    kind: ProgressEventKindInput,
    update: ProgressUpdateInput,
}

impl ProgressEventInput {
    fn into_semantic(self) -> Result<ProgressEvent, TypedJsonError> {
        ProgressEvent::new(
            self.timestamp,
            self.route.into_semantic()?,
            self.kind.into_semantic(),
            self.update.into_semantic()?,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProgressEventKindInput {
    Model,
    Tool,
    Compaction,
    Retry,
}

impl ProgressEventKindInput {
    const fn into_semantic(self) -> ProgressEventKind {
        match self {
            Self::Model => ProgressEventKind::Model,
            Self::Tool => ProgressEventKind::Tool,
            Self::Compaction => ProgressEventKind::Compaction,
            Self::Retry => ProgressEventKind::Retry,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ProgressUpdateInput {
    ItemStarted(ItemStartedProgressInput),
    ItemDelta(ItemDeltaProgressInput),
    ToolOutputDelta(ToolOutputDeltaProgressInput),
    ModelRetryScheduled(ModelRetryScheduledInput),
    OperationStatus(OperationStatusInput),
}

impl ProgressUpdateInput {
    fn into_semantic(self) -> Result<ProgressUpdate, TypedJsonError> {
        Ok(match self {
            Self::ItemStarted(value) => ProgressUpdate::item_started(
                value.item_id,
                value.content_index,
                value.content_kind.into_semantic(),
            ),
            Self::ItemDelta(value) => ProgressUpdate::item_delta(
                value.item_id,
                value.content_index,
                value.content_kind.into_semantic(),
                value.delta,
            )
            .map_err(|_| invalid_scalar())?,
            Self::ToolOutputDelta(value) => ProgressUpdate::tool_output_delta(
                value.item_id,
                value
                    .tool_call_id
                    .parse::<ToolCallId>()
                    .map_err(|_| invalid_scalar())?,
                value.delta,
            )
            .map_err(|_| invalid_scalar())?,
            Self::ModelRetryScheduled(value) => ProgressUpdate::model_retry_scheduled(
                value.purpose.into_semantic(),
                value.retry_count,
                value.ready_at,
            ),
            Self::OperationStatus(value) => {
                ProgressUpdate::operation_status(value.message).map_err(|_| invalid_scalar())?
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemStartedProgressInput {
    item_id: ItemId,
    content_index: u32,
    content_kind: ItemProgressContentKindInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemDeltaProgressInput {
    item_id: ItemId,
    content_index: u32,
    content_kind: ItemProgressContentKindInput,
    delta: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemProgressContentKindInput {
    AssistantText,
    Reasoning,
}

impl ItemProgressContentKindInput {
    const fn into_semantic(self) -> ItemProgressContentKind {
        match self {
            Self::AssistantText => ItemProgressContentKind::AssistantText,
            Self::Reasoning => ItemProgressContentKind::Reasoning,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolOutputDeltaProgressInput {
    item_id: ItemId,
    tool_call_id: String,
    delta: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelRetryScheduledInput {
    purpose: ModelCallPurposeInput,
    retry_count: u8,
    ready_at: Timestamp,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModelCallPurposeInput {
    AgentRun,
    CompactionSummary,
}

impl ModelCallPurposeInput {
    const fn into_semantic(self) -> ModelCallPurpose {
        match self {
            Self::AgentRun => ModelCallPurpose::AgentRun,
            Self::CompactionSummary => ModelCallPurpose::CompactionSummary,
        }
    }
}

#[derive(Deserialize)]
struct OperationStatusInput {
    message: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubscriptionClosedInput {
    Backpressure,
    RuntimeClosing,
    PublisherRestarted,
}

impl SubscriptionClosedInput {
    const fn into_semantic(self) -> SubscriptionClosed {
        match self {
            Self::Backpressure => SubscriptionClosed::Backpressure,
            Self::RuntimeClosing => SubscriptionClosed::RuntimeClosing,
            Self::PublisherRestarted => SubscriptionClosed::PublisherRestarted,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SnapshotResponseInput {
    Runtime(RuntimeSnapshotInput),
    Session(Box<SessionSnapshotInput>),
}

impl SnapshotResponseInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<SnapshotResponse, TypedJsonError> {
        Ok(match self {
            Self::Runtime(snapshot) => SnapshotResponse::Runtime(snapshot.into_semantic(limits)?),
            Self::Session(snapshot) => SessionSnapshot::from_input(*snapshot, limits)
                .map(Box::new)
                .map(SnapshotResponse::Session)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSnapshotInput {
    runtime: RuntimeViewInput,
    loaded_sessions: Vec<LoadedSessionSummaryInput>,
    diagnostics: Vec<PublicDiagnosticInput>,
}

impl RuntimeSnapshotInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<RuntimeSnapshot, TypedJsonError> {
        let loaded_sessions = self
            .loaded_sessions
            .into_iter()
            .map(LoadedSessionSummaryInput::into_semantic)
            .collect::<Result<Vec<_>, _>>()?;
        let diagnostics = self
            .diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.into_runtime(limits))
            .collect::<Result<Vec<_>, _>>()?;
        RuntimeSnapshot::new_with_limits(
            RuntimeView::new(self.runtime.status.into_semantic()),
            loaded_sessions,
            diagnostics,
            limits,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
struct RuntimeViewInput {
    status: RuntimeStatusInput,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeStatusInput {
    Running,
    Closing,
}

impl RuntimeStatusInput {
    const fn into_semantic(self) -> RuntimeStatusView {
        match self {
            Self::Running => RuntimeStatusView::Running,
            Self::Closing => RuntimeStatusView::Closing,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadedSessionSummaryInput {
    session_id: SessionId,
    readiness: SessionReadinessInput,
    execution: SessionExecutionInput,
    recording: SessionRecordingInput,
}

impl LoadedSessionSummaryInput {
    fn into_semantic(
        self,
    ) -> Result<crate::runtime_interface::LoadedSessionSummary, TypedJsonError> {
        crate::runtime_interface::LoadedSessionSummary::new(
            self.session_id,
            self.readiness.into_semantic()?,
            self.execution.into_semantic(),
            self.recording.into_semantic(),
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionReadinessInput {
    Preparing,
    Ready,
    Unavailable(SessionUnavailableInput),
}

impl SessionReadinessInput {
    fn into_semantic(self) -> Result<SessionReadinessView, TypedJsonError> {
        Ok(match self {
            Self::Preparing => SessionReadinessView::Preparing,
            Self::Ready => SessionReadinessView::Ready,
            Self::Unavailable(reason) => SessionReadinessView::Unavailable(reason.into_semantic()),
        })
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionUnavailableInput {
    AgentUnavailable,
    WorkspaceUnavailable,
    ModelUnavailable,
    PromptUnavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeDependencyUnavailable,
}

impl SessionUnavailableInput {
    const fn into_semantic(self) -> crate::runtime_interface::SessionUnavailableView {
        use crate::runtime_interface::SessionUnavailableView;
        match self {
            Self::AgentUnavailable => SessionUnavailableView::AgentUnavailable,
            Self::WorkspaceUnavailable => SessionUnavailableView::WorkspaceUnavailable,
            Self::ModelUnavailable => SessionUnavailableView::ModelUnavailable,
            Self::PromptUnavailable => SessionUnavailableView::PromptUnavailable,
            Self::DurableStateCorrupt => SessionUnavailableView::DurableStateCorrupt,
            Self::DurableStateTooLarge => SessionUnavailableView::DurableStateTooLarge,
            Self::RuntimeDependencyUnavailable => {
                SessionUnavailableView::RuntimeDependencyUnavailable
            }
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionExecutionInput {
    Idle,
    Starting,
    Running,
    Finishing,
}

impl SessionExecutionInput {
    const fn into_semantic(self) -> SessionExecutionView {
        match self {
            Self::Idle => SessionExecutionView::Idle,
            Self::Starting => SessionExecutionView::Starting,
            Self::Running => SessionExecutionView::Running,
            Self::Finishing => SessionExecutionView::Finishing,
        }
    }
}

#[derive(Deserialize)]
struct SessionRecordingInput {
    state: SessionRecordingStateInput,
}

impl SessionRecordingInput {
    const fn into_semantic(self) -> SessionRecordingView {
        SessionRecordingView::new(self.state.into_semantic())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionRecordingStateInput {
    Healthy,
    Degraded,
}

impl SessionRecordingStateInput {
    const fn into_semantic(self) -> SessionRecordingState {
        match self {
            Self::Healthy => SessionRecordingState::Healthy,
            Self::Degraded => SessionRecordingState::Degraded,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionSnapshotInput {
    session_id: SessionId,
    lifecycle: SessionLifecycleInput,
    metadata: SessionMetadataInput,
    definition: SessionDefinitionSummaryInput,
    load_state: SessionLoadStateInput,
    readiness: SessionReadinessInput,
    execution: SessionExecutionInput,
    current_turn: Option<CurrentTurnInput>,
    active_items: Vec<ItemInput>,
    pending_interactions: Vec<InteractionInput>,
    queues: SessionQueueInput,
    recording: SessionRecordingInput,
    usage: Option<SessionUsageInput>,
    diagnostics: Vec<PublicDiagnosticInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentTurnInput {
    turn_id: TurnId,
    status: TurnStatusInput,
    phase: Option<TurnExecutionPhaseInput>,
    started_at: Timestamp,
}

impl CurrentTurnInput {
    fn into_semantic(self) -> Result<CurrentTurnView, TypedJsonError> {
        Ok(CurrentTurnView::new(
            self.turn_id,
            self.status.into_semantic()?,
            self.phase.map(TurnExecutionPhaseInput::into_semantic),
            self.started_at,
        ))
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum TurnStatusInput {
    Running,
    Completed(CompletedTurnStatusInput),
    Interrupted(InterruptedTurnStatusInput),
    Failed(FailedTurnStatusInput),
}

impl TurnStatusInput {
    fn into_semantic(self) -> Result<TurnStatusView, TypedJsonError> {
        Ok(match self {
            Self::Running => TurnStatusView::Running,
            Self::Completed(value) => TurnStatusView::Completed {
                completed_at: value.completed_at,
            },
            Self::Interrupted(value) => TurnStatusView::Interrupted {
                completed_at: value.completed_at,
                reason: value.reason.into_semantic(),
            },
            Self::Failed(value) => TurnStatusView::Failed {
                completed_at: value.completed_at,
                reason: value.reason.into_semantic(),
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompletedTurnStatusInput {
    completed_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InterruptedTurnStatusInput {
    completed_at: Timestamp,
    reason: TurnInterruptionInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FailedTurnStatusInput {
    completed_at: Timestamp,
    reason: TurnFailureInput,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TurnExecutionPhaseInput {
    Sampling,
    RetryBackoff,
    Compacting,
    WaitingApproval,
    WaitingForUserInput,
    ExecutingTools,
}

impl TurnExecutionPhaseInput {
    const fn into_semantic(self) -> TurnExecutionPhaseView {
        match self {
            Self::Sampling => TurnExecutionPhaseView::Sampling,
            Self::RetryBackoff => TurnExecutionPhaseView::RetryBackoff,
            Self::Compacting => TurnExecutionPhaseView::Compacting,
            Self::WaitingApproval => TurnExecutionPhaseView::WaitingApproval,
            Self::WaitingForUserInput => TurnExecutionPhaseView::WaitingForUserInput,
            Self::ExecutingTools => TurnExecutionPhaseView::ExecutingTools,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemInput {
    item_id: ItemId,
    turn_id: TurnId,
    status: ItemStatusInput,
    content: ItemContentInput,
    created_at: Timestamp,
    completed_at: Option<Timestamp>,
}

impl ItemInput {
    fn into_semantic(self) -> Result<ItemView, TypedJsonError> {
        ItemView::new(
            self.item_id,
            self.turn_id,
            self.status.into_semantic(),
            self.content.into_semantic()?,
            self.created_at,
            self.completed_at,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemStatusInput {
    Started,
    Completed,
}

impl ItemStatusInput {
    const fn into_semantic(self) -> ItemStatusView {
        match self {
            Self::Started => ItemStatusView::Started,
            Self::Completed => ItemStatusView::Completed,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ItemContentInput {
    UserMessage(UserMessageItemInput),
    AgentMessage(TextItemInput),
    Reasoning(TextItemInput),
    ToolInvocation(ToolInvocationItemInput),
}

impl ItemContentInput {
    fn into_semantic(self) -> Result<ItemContentView, TypedJsonError> {
        match self {
            Self::UserMessage(value) => ItemContentView::user_message(
                value.source.into_semantic(),
                value.body,
                value.contributions.into_iter().map(Into::into).collect(),
            ),
            Self::AgentMessage(value) => ItemContentView::agent_message(value.body),
            Self::Reasoning(value) => ItemContentView::reasoning(value.body),
            Self::ToolInvocation(value) => ItemContentView::tool_invocation(
                value.tool_call_id,
                value.tool_name,
                value.arguments_summary,
                value.result.map(|_| "present"),
            ),
        }
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserMessageItemInput {
    source: UserMessageSourceInput,
    body: String,
    contributions: Vec<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UserMessageSourceInput {
    Input,
    Steer,
}

impl UserMessageSourceInput {
    const fn into_semantic(self) -> UserMessageSource {
        match self {
            Self::Input => UserMessageSource::Input,
            Self::Steer => UserMessageSource::Steer,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextItemInput {
    body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolInvocationItemInput {
    tool_call_id: String,
    tool_name: String,
    arguments_summary: String,
    result: Option<UnsupportedObservationInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InteractionInput {
    request_id: RequestId,
    turn_id: TurnId,
    item_id: ItemId,
    request: InteractionRequestInput,
}

impl InteractionInput {
    fn into_semantic(self) -> Result<InteractionView, TypedJsonError> {
        Ok(InteractionView::new(
            self.request_id,
            self.turn_id,
            self.item_id,
            self.request.into_semantic()?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum InteractionRequestInput {
    ToolApproval(ToolApprovalRequestInput),
    UserQuestion(UserQuestionRequestInput),
}

impl InteractionRequestInput {
    fn into_semantic(self) -> Result<InteractionRequestView, TypedJsonError> {
        match self {
            Self::ToolApproval(value) => {
                Ok(InteractionRequestView::ToolApproval(value.into_semantic()?))
            }
            Self::UserQuestion(value) => {
                Ok(InteractionRequestView::UserQuestion(value.into_semantic()?))
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolApprovalRequestInput {
    tool_name: String,
    arguments_summary: String,
    reason: String,
    requirements: ToolRequirementsInput,
    options: Vec<ToolApprovalOptionInput>,
}

impl ToolApprovalRequestInput {
    fn into_semantic(self) -> Result<crate::tools::ToolApprovalRequestView, TypedJsonError> {
        let options = self
            .options
            .into_iter()
            .map(ToolApprovalOptionInput::into_semantic)
            .collect::<Result<Vec<_>, _>>()?;
        ToolApprovalRequestView::reconstruct(
            self.tool_name.parse().map_err(|_| invalid_scalar())?,
            self.arguments_summary,
            self.reason,
            self.requirements.into_semantic()?,
            options,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolRequirementsInput {
    filesystem: Option<String>,
    network: Option<String>,
    process: Option<String>,
}

impl ToolRequirementsInput {
    fn into_semantic(self) -> Result<ToolRequirementSummaryView, TypedJsonError> {
        ToolRequirementSummaryView::reconstruct(self.filesystem, self.network, self.process)
            .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolApprovalOptionInput {
    option_index: u32,
    kind: ToolApprovalOptionKindInput,
    label: String,
    effective_requirements: ToolRequirementsInput,
}

impl ToolApprovalOptionInput {
    fn into_semantic(self) -> Result<ToolApprovalOptionView, TypedJsonError> {
        ToolApprovalOptionView::reconstruct(
            self.option_index,
            self.kind.into_semantic(),
            self.label,
            self.effective_requirements.into_semantic()?,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolApprovalOptionKindInput {
    AsRequested,
    Restricted,
}

impl ToolApprovalOptionKindInput {
    const fn into_semantic(self) -> ToolApprovalOptionKindView {
        match self {
            Self::AsRequested => ToolApprovalOptionKindView::AsRequested,
            Self::Restricted => ToolApprovalOptionKindView::Restricted,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserQuestionRequestInput {
    title: Option<String>,
    questions: Vec<UserQuestionFieldInput>,
}

impl UserQuestionRequestInput {
    fn into_semantic(self) -> Result<crate::tools::UserQuestionRequest, TypedJsonError> {
        let questions = self
            .questions
            .into_iter()
            .map(UserQuestionFieldInput::into_semantic)
            .collect::<Result<Vec<_>, _>>()?;
        UserQuestionRequest::reconstruct(self.title, questions).map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserQuestionFieldInput {
    question_index: u32,
    prompt: String,
    required: bool,
    input: UserQuestionInputInput,
}

impl UserQuestionFieldInput {
    fn into_semantic(self) -> Result<UserQuestionField, TypedJsonError> {
        UserQuestionField::reconstruct(
            self.question_index,
            self.prompt,
            self.required,
            self.input.into_semantic()?,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum UserQuestionInputInput {
    Text(TextQuestionInput),
    SingleChoice(SingleChoiceQuestionInput),
}

impl UserQuestionInputInput {
    fn into_semantic(self) -> Result<UserQuestionInput, TypedJsonError> {
        Ok(match self {
            Self::Text(value) => UserQuestionInput::Text {
                multiline: value.multiline,
            },
            Self::SingleChoice(value) => UserQuestionInput::SingleChoice {
                options: value
                    .options
                    .into_iter()
                    .map(|option| {
                        UserQuestionChoice::reconstruct(option.option_index, option.label)
                            .map_err(|_| invalid_scalar())
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextQuestionInput {
    multiline: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SingleChoiceQuestionInput {
    options: Vec<UserQuestionChoiceInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserQuestionChoiceInput {
    option_index: u32,
    label: String,
}

impl SessionSnapshot {
    fn from_input(
        value: SessionSnapshotInput,
        limits: ProtocolLimits,
    ) -> Result<Self, TypedJsonError> {
        if !matches!(value.lifecycle, SessionLifecycleInput::Open) {
            return Err(invalid_scalar());
        }
        if !matches!(value.load_state, SessionLoadStateInput::Loaded)
            || !matches!(value.readiness, SessionReadinessInput::Ready)
        {
            return Err(TypedJsonError::PendingPublicTarget);
        }
        let current_turn = value
            .current_turn
            .map(CurrentTurnInput::into_semantic)
            .transpose()?;
        let active_items = value
            .active_items
            .into_iter()
            .map(ItemInput::into_semantic)
            .collect::<Result<Vec<_>, _>>()?;
        let pending_interactions = value
            .pending_interactions
            .into_iter()
            .map(InteractionInput::into_semantic)
            .collect::<Result<Vec<_>, _>>()?;
        let execution = value.execution.into_semantic();
        let queues = value.queues.into_semantic(limits)?;
        let diagnostics = value
            .diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.into_session(limits))
            .collect::<Result<Vec<_>, _>>()?;
        SessionSnapshot::new_loaded_ready_with_observation(
            value.session_id,
            value.metadata.into_semantic(limits)?,
            value.definition.into_semantic(limits)?,
            execution,
            current_turn,
            active_items,
            pending_interactions,
            queues,
            value.recording.into_semantic(),
            value
                .usage
                .map(SessionUsageInput::into_semantic)
                .transpose()?,
            diagnostics,
            limits,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionLifecycleInput {
    Open,
    Archived,
    Deleted,
}

impl SessionLifecycleInput {
    const fn into_semantic(self) -> SessionLifecycleView {
        match self {
            Self::Open => SessionLifecycleView::Open,
            Self::Archived => SessionLifecycleView::Archived,
            Self::Deleted => SessionLifecycleView::Deleted,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionLoadStateInput {
    Loaded,
    Unloading,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionQueueInput {
    submit_admissions: Vec<SubmitAdmissionInput>,
    steers: Vec<QueuedSteerInput>,
    follow_ups: Vec<QueuedFollowUpInput>,
    accepting_input: bool,
}

impl SessionQueueInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<SessionQueueView, TypedJsonError> {
        let submit_admissions = self
            .submit_admissions
            .into_iter()
            .map(SubmitAdmissionInput::into_semantic)
            .collect();
        let steers = self
            .steers
            .into_iter()
            .map(QueuedSteerInput::into_semantic)
            .collect();
        let follow_ups = self
            .follow_ups
            .into_iter()
            .map(QueuedFollowUpInput::into_semantic)
            .collect();
        SessionQueueView::new_with_limits(
            submit_admissions,
            steers,
            follow_ups,
            self.accepting_input,
            limits,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmitAdmissionInput {
    command_id: CommandId,
    state: SubmitAdmissionStateInput,
}

impl SubmitAdmissionInput {
    const fn into_semantic(self) -> SubmitAdmissionView {
        SubmitAdmissionView::new(self.command_id, self.state.into_semantic())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubmitAdmissionStateInput {
    Queued,
    Starting,
}

impl SubmitAdmissionStateInput {
    const fn into_semantic(self) -> SubmitAdmissionStateView {
        match self {
            Self::Queued => SubmitAdmissionStateView::Queued,
            Self::Starting => SubmitAdmissionStateView::Starting,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueuedSteerInput {
    command_id: CommandId,
    expected_turn_id: TurnId,
}

impl QueuedSteerInput {
    const fn into_semantic(self) -> QueuedSteerView {
        QueuedSteerView::new(self.command_id, self.expected_turn_id)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueuedFollowUpInput {
    command_id: CommandId,
}

impl QueuedFollowUpInput {
    const fn into_semantic(self) -> QueuedFollowUpView {
        QueuedFollowUpView::new(self.command_id)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMetadataInput {
    revision: SessionMetadataRevision,
    name: Option<String>,
    description: Option<String>,
    updated_at: Timestamp,
}

impl SessionMetadataInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<SessionMetadataView, TypedJsonError> {
        SessionMetadataView::new_with_limits(
            self.revision,
            self.name,
            self.description,
            self.updated_at,
            limits,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionDefinitionSummaryInput {
    session_id: SessionId,
    revision: SessionDefinitionRevision,
    agent: AgentRevisionRefInput,
    workspace: WorkspaceDefinitionSummaryInput,
    model: SessionModelSummaryInput,
    prompt_ids: Vec<String>,
    created_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionModelSummaryInput {
    selection: ModelSelectionOutputInput,
    reasoning: ReasoningPreferenceOutputInput,
    max_output_tokens: Option<NonZeroU32>,
}

impl SessionModelSummaryInput {
    fn into_semantic(self) -> Result<SessionModelConfig, TypedJsonError> {
        Ok(SessionModelConfig::new(
            self.selection.into_semantic()?,
            self.reasoning.into_semantic(),
            self.max_output_tokens,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelSelectionOutputInput {
    provider_id: String,
    model_id: String,
}

impl ModelSelectionOutputInput {
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
enum ReasoningPreferenceOutputInput {
    Auto,
    Disabled,
    Low,
    Medium,
    High,
}

impl ReasoningPreferenceOutputInput {
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

impl SessionDefinitionSummaryInput {
    fn into_semantic(
        self,
        limits: ProtocolLimits,
    ) -> Result<SessionDefinitionSummary, TypedJsonError> {
        let prompts = self
            .prompt_ids
            .into_iter()
            .map(|prompt| prompt.parse::<PromptId>().map_err(|_| invalid_scalar()))
            .collect::<Result<Vec<_>, _>>()?;
        let prompts = SessionPromptSelection::new_with_maximum(
            prompts,
            usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX),
        )
        .map_err(|_| invalid_scalar())?;
        Ok(SessionDefinitionSummary::new(
            self.session_id,
            self.revision,
            self.agent.into_semantic(),
            self.workspace.into_semantic(limits)?,
            self.model.into_semantic()?,
            prompts,
            self.created_at,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRevisionRefInput {
    agent_id: AgentId,
    revision: AgentRevision,
}

impl AgentRevisionRefInput {
    const fn into_semantic(self) -> AgentRevisionRef {
        AgentRevisionRef::new(self.agent_id, self.revision)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDefinitionSummaryInput {
    roots: Vec<WorkspaceRootSummaryInput>,
    cwd_root: String,
    cwd_relative: WorkspaceRelativePath,
}

impl WorkspaceDefinitionSummaryInput {
    fn into_semantic(
        self,
        limits: ProtocolLimits,
    ) -> Result<WorkspaceDefinitionSummaryView, TypedJsonError> {
        let mut roots = self
            .roots
            .into_iter()
            .map(WorkspaceRootSummaryInput::into_semantic)
            .collect::<Result<Vec<_>, _>>()?;
        if roots.is_empty() {
            return Err(invalid_scalar());
        }
        let primary_root = roots.remove(0);
        let cwd = WorkspaceCwdSpec::new(
            self.cwd_root
                .parse::<WorkspaceRootKey>()
                .map_err(|_| invalid_scalar())?,
            self.cwd_relative,
        );
        WorkspaceDefinitionSummaryView::new_with_limits(primary_root, roots, cwd, limits)
            .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRootSummaryInput {
    key: String,
    requested_access: RequestedFilesystemAccessInput,
    prompt_source: bool,
    skill_source: bool,
}

impl WorkspaceRootSummaryInput {
    fn into_semantic(self) -> Result<WorkspaceRootSummaryView, TypedJsonError> {
        Ok(WorkspaceRootSummaryView::new(
            self.key
                .parse::<WorkspaceRootKey>()
                .map_err(|_| invalid_scalar())?,
            self.requested_access.into_semantic(),
            WorkspaceSourcePolicy::new(self.prompt_source, self.skill_source),
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionUsageInput {
    model_calls: super::CanonicalU64,
    compaction_calls: super::CanonicalU64,
    input_tokens: Option<super::CanonicalU64>,
    output_tokens: Option<super::CanonicalU64>,
    reasoning_tokens: Option<super::CanonicalU64>,
    cache_read_tokens: Option<super::CanonicalU64>,
    cache_write_tokens: Option<super::CanonicalU64>,
    reported_costs: Vec<Money>,
}

impl SessionUsageInput {
    fn into_semantic(self) -> Result<SessionUsageView, TypedJsonError> {
        SessionUsageView::new(
            self.model_calls.get(),
            self.compaction_calls.get(),
            self.input_tokens.map(super::CanonicalU64::get),
            self.output_tokens.map(super::CanonicalU64::get),
            self.reasoning_tokens.map(super::CanonicalU64::get),
            self.cache_read_tokens.map(super::CanonicalU64::get),
            self.cache_write_tokens.map(super::CanonicalU64::get),
            self.reported_costs,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
struct PublicDiagnosticInput {
    code: String,
    message: String,
}

impl PublicDiagnosticInput {
    fn into_runtime(self, limits: ProtocolLimits) -> Result<RuntimeDiagnosticView, TypedJsonError> {
        RuntimeDiagnosticView::new_with_limits(self.code, self.message, limits)
            .map_err(|_| invalid_scalar())
    }

    fn into_session(self, limits: ProtocolLimits) -> Result<SessionDiagnosticView, TypedJsonError> {
        SessionDiagnosticView::new_with_limits(self.code, self.message, limits)
            .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateEventInput {
    timestamp: Timestamp,
    command_id: Option<CommandId>,
    route: EventRouteInput,
    msg: StateEventMsgInput,
}

impl StateEventInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<StateEvent, TypedJsonError> {
        StateEvent::from_wire(
            self.timestamp,
            self.command_id,
            self.route.into_semantic()?,
            self.msg.into_semantic(limits)?,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum EventRouteInput {
    Runtime,
    Agent(AgentRouteInput),
    Session(SessionIdInput),
    Turn(TurnRouteInput),
    Item(ItemRouteInput),
    Interaction(InteractionRouteInput),
}

impl EventRouteInput {
    fn into_semantic(self) -> Result<EventRoute, TypedJsonError> {
        Ok(match self {
            Self::Runtime => EventRoute::Runtime,
            Self::Agent(value) => EventRoute::Agent {
                agent_id: value.agent_id,
            },
            Self::Session(value) => EventRoute::Session {
                session_id: value.session_id,
            },
            Self::Turn(value) => EventRoute::Turn {
                session_id: value.session_id,
                turn_id: value.turn_id,
            },
            Self::Item(value) => EventRoute::Item {
                session_id: value.session_id,
                turn_id: value.turn_id,
                item_id: value.item_id,
            },
            Self::Interaction(value) => EventRoute::Interaction {
                session_id: value.session_id,
                turn_id: value.turn_id,
                item_id: value.item_id,
                request_id: value.request_id,
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRouteInput {
    agent_id: AgentId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnRouteInput {
    session_id: SessionId,
    turn_id: TurnId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemRouteInput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InteractionRouteInput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
    request_id: RequestId,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum StateEventMsgInput {
    Runtime(RuntimeStateEventInput),
    Session(Box<SessionStateEventInput>),
}

impl StateEventMsgInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<StateEventMsg, TypedJsonError> {
        Ok(match self {
            Self::Runtime(value) => StateEventMsg::Runtime {
                kind: value.kind.into_semantic()?,
                snapshot: value.snapshot.into_semantic(limits)?,
                detail: value
                    .detail
                    .map(|detail| detail.into_semantic(limits))
                    .transpose()?,
            },
            Self::Session(value) => {
                let SessionStateEventInput {
                    kind,
                    snapshot,
                    detail,
                } = *value;
                StateEventMsg::Session {
                    kind: kind.into_semantic()?,
                    snapshot: Box::new(SessionSnapshot::from_input(snapshot, limits)?),
                    detail: detail
                        .map(SessionEventDetailInput::into_semantic)
                        .transpose()?,
                }
            }
        })
    }
}

#[derive(Deserialize)]
struct RuntimeStateEventInput {
    kind: RuntimeStateEventKindInput,
    snapshot: RuntimeSnapshotInput,
    detail: Option<RuntimeEventDetailInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeStateEventKindInput {
    AgentCreated,
    AgentStatusChanged,
    SessionCreated,
    SessionLoaded,
    SessionUnloaded,
    SessionArchived,
    SessionUnarchived,
    SessionDeleted,
    SessionForked,
    CommandCatalogInvalidated,
}

impl RuntimeStateEventKindInput {
    fn into_semantic(self) -> Result<RuntimeStateEventKind, TypedJsonError> {
        Ok(match self {
            Self::AgentCreated => RuntimeStateEventKind::AgentCreated,
            Self::AgentStatusChanged => RuntimeStateEventKind::AgentStatusChanged,
            Self::SessionCreated => RuntimeStateEventKind::SessionCreated,
            Self::SessionLoaded => RuntimeStateEventKind::SessionLoaded,
            Self::SessionUnloaded => RuntimeStateEventKind::SessionUnloaded,
            Self::SessionArchived => RuntimeStateEventKind::SessionArchived,
            Self::SessionUnarchived => RuntimeStateEventKind::SessionUnarchived,
            Self::SessionDeleted => RuntimeStateEventKind::SessionDeleted,
            Self::SessionForked => RuntimeStateEventKind::SessionForked,
            Self::CommandCatalogInvalidated => RuntimeStateEventKind::CommandCatalogInvalidated,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeEventDetailInput {
    AgentChanged(AgentChangedDetailInput),
    SessionChanged(Box<SessionChangedDetailInput>),
}

impl RuntimeEventDetailInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<RuntimeEventDetail, TypedJsonError> {
        match self {
            Self::AgentChanged(detail) => Ok(RuntimeEventDetail::AgentChanged {
                agent: detail.agent.into_semantic(limits)?,
            }),
            Self::SessionChanged(detail) => Ok(RuntimeEventDetail::SessionChanged {
                session: Box::new(detail.session.into_semantic(limits)?),
            }),
        }
    }
}

#[derive(Deserialize)]
struct AgentChangedDetailInput {
    agent: AgentSummaryInput,
}

#[derive(Deserialize)]
struct SessionChangedDetailInput {
    session: SessionSummaryInput,
}

#[derive(Deserialize)]
struct SessionStateEventInput {
    kind: SessionStateEventKindInput,
    snapshot: SessionSnapshotInput,
    detail: Option<SessionEventDetailInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(
    clippy::enum_variant_names,
    reason = "the wire discriminator names are fixed by the public protocol"
)]
enum SessionStateEventKindInput {
    SessionExecutionChanged,
    TurnCompleted,
    TurnInterrupted,
    TurnFailed,
}

impl SessionStateEventKindInput {
    fn into_semantic(self) -> Result<SessionStateEventKind, TypedJsonError> {
        Ok(match self {
            Self::SessionExecutionChanged => SessionStateEventKind::SessionExecutionChanged,
            Self::TurnCompleted => SessionStateEventKind::TurnCompleted,
            Self::TurnInterrupted => SessionStateEventKind::TurnInterrupted,
            Self::TurnFailed => SessionStateEventKind::TurnFailed,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionEventDetailInput {
    TurnTerminal(TurnTerminalDetailInput),
}

impl SessionEventDetailInput {
    fn into_semantic(self) -> Result<SessionEventDetail, TypedJsonError> {
        Ok(match self {
            Self::TurnTerminal(value) => SessionEventDetail::TurnTerminal {
                turn_id: value.turn_id,
                terminal: value.terminal.into_semantic(),
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnTerminalDetailInput {
    turn_id: TurnId,
    terminal: TurnTerminalInput,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum TurnTerminalInput {
    Completed(CompletedTerminalInput),
    Interrupted(InterruptedTerminalInput),
    Failed(FailedTerminalInput),
}

impl TurnTerminalInput {
    const fn into_semantic(self) -> TurnTerminalView {
        match self {
            Self::Completed(value) => TurnTerminalView::Completed {
                completed_at: value.completed_at,
            },
            Self::Interrupted(value) => TurnTerminalView::Interrupted {
                completed_at: value.completed_at,
                reason: value.reason.into_semantic(),
            },
            Self::Failed(value) => TurnTerminalView::Failed {
                completed_at: value.completed_at,
                reason: value.reason.into_semantic(),
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompletedTerminalInput {
    completed_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InterruptedTerminalInput {
    completed_at: Timestamp,
    reason: TurnInterruptionInput,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TurnInterruptionInput {
    UserCancelled,
    SecurityRevoked,
    PrepareForUnload,
    RuntimeShutdown,
    RuntimeFailure,
}

impl TurnInterruptionInput {
    const fn into_semantic(self) -> TurnInterruptionView {
        match self {
            Self::UserCancelled => TurnInterruptionView::UserCancelled,
            Self::SecurityRevoked => TurnInterruptionView::SecurityRevoked,
            Self::PrepareForUnload => TurnInterruptionView::PrepareForUnload,
            Self::RuntimeShutdown => TurnInterruptionView::RuntimeShutdown,
            Self::RuntimeFailure => TurnInterruptionView::RuntimeFailure,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FailedTerminalInput {
    completed_at: Timestamp,
    reason: TurnFailureInput,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TurnFailureInput {
    Prompt,
    Model,
    Tool,
    ContextOverflow,
    DependencyUnavailable,
    InvariantFailure,
}

impl TurnFailureInput {
    const fn into_semantic(self) -> TurnFailureView {
        match self {
            Self::Prompt => TurnFailureView::Prompt,
            Self::Model => TurnFailureView::Model,
            Self::Tool => TurnFailureView::Tool,
            Self::ContextOverflow => TurnFailureView::ContextOverflow,
            Self::DependencyUnavailable => TurnFailureView::DependencyUnavailable,
            Self::InvariantFailure => TurnFailureView::InvariantFailure,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum EventFrameOutput<'a> {
    Snapshot(SnapshotResponseOutput<'a>),
    State(StateEventOutput<'a>),
    Progress(ProgressEventOutput<'a>),
    Closed(SubscriptionClosedOutput),
}

impl<'a> EventFrameOutput<'a> {
    fn from_semantic(value: &'a EventFrame) -> Self {
        match value {
            EventFrame::Snapshot(snapshot) => {
                Self::Snapshot(SnapshotResponseOutput::from_semantic(snapshot))
            }
            EventFrame::State(event) => Self::State(StateEventOutput::from_semantic(event)),
            EventFrame::Progress(event) => {
                Self::Progress(ProgressEventOutput::from_semantic(event))
            }
            EventFrame::Closed(reason) => {
                Self::Closed(SubscriptionClosedOutput::from_semantic(*reason))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEventOutput<'a> {
    timestamp: Timestamp,
    route: EventRouteOutput,
    kind: ProgressEventKindOutput,
    update: ProgressUpdateOutput<'a>,
}

impl<'a> ProgressEventOutput<'a> {
    fn from_semantic(value: &'a ProgressEvent) -> Self {
        Self {
            timestamp: value.timestamp(),
            route: EventRouteOutput::from_semantic(value.route()),
            kind: ProgressEventKindOutput::from_semantic(value.kind()),
            update: ProgressUpdateOutput::from_semantic(value.update()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ProgressEventKindOutput {
    Model,
    Tool,
    Compaction,
    Retry,
}

impl ProgressEventKindOutput {
    const fn from_semantic(value: ProgressEventKind) -> Self {
        match value {
            ProgressEventKind::Model => Self::Model,
            ProgressEventKind::Tool => Self::Tool,
            ProgressEventKind::Compaction => Self::Compaction,
            ProgressEventKind::Retry => Self::Retry,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ProgressUpdateOutput<'a> {
    ItemStarted(ItemStartedProgressOutput),
    ItemDelta(ItemDeltaProgressOutput<'a>),
    ToolOutputDelta(ToolOutputDeltaProgressOutput<'a>),
    ModelRetryScheduled(ModelRetryScheduledOutput),
    OperationStatus(OperationStatusOutput<'a>),
}

impl<'a> ProgressUpdateOutput<'a> {
    fn from_semantic(value: &'a ProgressUpdate) -> Self {
        match value {
            ProgressUpdate::ItemStarted {
                item_id,
                content_index,
                content_kind,
            } => Self::ItemStarted(ItemStartedProgressOutput {
                item_id: *item_id,
                content_index: *content_index,
                content_kind: ItemProgressContentKindOutput::from_semantic(*content_kind),
            }),
            ProgressUpdate::ItemDelta {
                item_id,
                content_index,
                content_kind,
                delta,
            } => Self::ItemDelta(ItemDeltaProgressOutput {
                item_id: *item_id,
                content_index: *content_index,
                content_kind: ItemProgressContentKindOutput::from_semantic(*content_kind),
                delta,
            }),
            ProgressUpdate::ToolOutputDelta {
                item_id,
                tool_call_id,
                delta,
            } => Self::ToolOutputDelta(ToolOutputDeltaProgressOutput {
                item_id: *item_id,
                tool_call_id: tool_call_id.as_str(),
                delta,
            }),
            ProgressUpdate::ModelRetryScheduled {
                purpose,
                retry_count,
                ready_at,
            } => Self::ModelRetryScheduled(ModelRetryScheduledOutput {
                purpose: ModelCallPurposeOutput::from_semantic(*purpose),
                retry_count: *retry_count,
                ready_at: *ready_at,
            }),
            ProgressUpdate::OperationStatus { message } => {
                Self::OperationStatus(OperationStatusOutput { message })
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemStartedProgressOutput {
    item_id: ItemId,
    content_index: u32,
    content_kind: ItemProgressContentKindOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemDeltaProgressOutput<'a> {
    item_id: ItemId,
    content_index: u32,
    content_kind: ItemProgressContentKindOutput,
    delta: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ItemProgressContentKindOutput {
    AssistantText,
    Reasoning,
}

impl ItemProgressContentKindOutput {
    const fn from_semantic(value: ItemProgressContentKind) -> Self {
        match value {
            ItemProgressContentKind::AssistantText => Self::AssistantText,
            ItemProgressContentKind::Reasoning => Self::Reasoning,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolOutputDeltaProgressOutput<'a> {
    item_id: ItemId,
    tool_call_id: &'a str,
    delta: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelRetryScheduledOutput {
    purpose: ModelCallPurposeOutput,
    retry_count: u8,
    ready_at: Timestamp,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ModelCallPurposeOutput {
    AgentRun,
    CompactionSummary,
}

impl ModelCallPurposeOutput {
    const fn from_semantic(value: ModelCallPurpose) -> Self {
        match value {
            ModelCallPurpose::AgentRun => Self::AgentRun,
            ModelCallPurpose::CompactionSummary => Self::CompactionSummary,
        }
    }
}

#[derive(Serialize)]
struct OperationStatusOutput<'a> {
    message: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SubscriptionClosedOutput {
    Backpressure,
    RuntimeClosing,
    PublisherRestarted,
}

impl SubscriptionClosedOutput {
    const fn from_semantic(value: SubscriptionClosed) -> Self {
        match value {
            SubscriptionClosed::Backpressure => Self::Backpressure,
            SubscriptionClosed::RuntimeClosing => Self::RuntimeClosing,
            SubscriptionClosed::PublisherRestarted => Self::PublisherRestarted,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SnapshotResponseOutput<'a> {
    Runtime(RuntimeSnapshotOutput<'a>),
    Session(Box<SessionSnapshotOutput<'a>>),
}

impl<'a> SnapshotResponseOutput<'a> {
    fn from_semantic(value: &'a SnapshotResponse) -> Self {
        match value {
            SnapshotResponse::Runtime(snapshot) => {
                Self::Runtime(RuntimeSnapshotOutput::from_semantic(snapshot))
            }
            SnapshotResponse::Session(snapshot) => {
                Self::Session(Box::new(SessionSnapshotOutput::from_semantic(snapshot)))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSnapshotOutput<'a> {
    runtime: RuntimeViewOutput,
    loaded_sessions: Vec<LoadedSessionSummaryOutput>,
    diagnostics: Vec<PublicDiagnosticOutput<'a>>,
}

impl<'a> RuntimeSnapshotOutput<'a> {
    fn from_semantic(value: &'a RuntimeSnapshot) -> Self {
        Self {
            runtime: RuntimeViewOutput {
                status: RuntimeStatusOutput::from_semantic(value.runtime().status()),
            },
            loaded_sessions: value
                .loaded_sessions()
                .iter()
                .copied()
                .map(LoadedSessionSummaryOutput::from_semantic)
                .collect(),
            diagnostics: value
                .diagnostics()
                .iter()
                .map(PublicDiagnosticOutput::from_runtime)
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct RuntimeViewOutput {
    status: RuntimeStatusOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeStatusOutput {
    Running,
    Closing,
}

impl RuntimeStatusOutput {
    const fn from_semantic(value: RuntimeStatusView) -> Self {
        match value {
            RuntimeStatusView::Running => Self::Running,
            RuntimeStatusView::Closing => Self::Closing,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadedSessionSummaryOutput {
    session_id: SessionId,
    readiness: SessionReadinessOutput,
    execution: SessionExecutionOutput,
    recording: SessionRecordingOutput,
}

impl LoadedSessionSummaryOutput {
    fn from_semantic(value: crate::runtime_interface::LoadedSessionSummary) -> Self {
        Self {
            session_id: value.session_id(),
            readiness: SessionReadinessOutput::from_semantic(value.readiness()),
            execution: SessionExecutionOutput::from_semantic(value.execution()),
            recording: SessionRecordingOutput::from_semantic(value.recording()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionReadinessOutput {
    Preparing,
    Ready,
    Unavailable(SessionUnavailableOutput),
}

impl SessionReadinessOutput {
    fn from_semantic(value: SessionReadinessView) -> Self {
        match value {
            SessionReadinessView::Preparing => Self::Preparing,
            SessionReadinessView::Ready => Self::Ready,
            SessionReadinessView::Unavailable(reason) => {
                Self::Unavailable(SessionUnavailableOutput::from_semantic(reason))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionUnavailableOutput {
    AgentUnavailable,
    WorkspaceUnavailable,
    ModelUnavailable,
    PromptUnavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeDependencyUnavailable,
}

impl SessionUnavailableOutput {
    const fn from_semantic(value: crate::runtime_interface::SessionUnavailableView) -> Self {
        use crate::runtime_interface::SessionUnavailableView;
        match value {
            SessionUnavailableView::AgentUnavailable => Self::AgentUnavailable,
            SessionUnavailableView::WorkspaceUnavailable => Self::WorkspaceUnavailable,
            SessionUnavailableView::ModelUnavailable => Self::ModelUnavailable,
            SessionUnavailableView::PromptUnavailable => Self::PromptUnavailable,
            SessionUnavailableView::DurableStateCorrupt => Self::DurableStateCorrupt,
            SessionUnavailableView::DurableStateTooLarge => Self::DurableStateTooLarge,
            SessionUnavailableView::RuntimeDependencyUnavailable => {
                Self::RuntimeDependencyUnavailable
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionExecutionOutput {
    Idle,
    Starting,
    Running,
    Finishing,
}

impl SessionExecutionOutput {
    const fn from_semantic(value: SessionExecutionView) -> Self {
        match value {
            SessionExecutionView::Idle => Self::Idle,
            SessionExecutionView::Starting => Self::Starting,
            SessionExecutionView::Running => Self::Running,
            SessionExecutionView::Finishing => Self::Finishing,
        }
    }
}

#[derive(Serialize)]
struct SessionRecordingOutput {
    state: SessionRecordingStateOutput,
}

impl SessionRecordingOutput {
    const fn from_semantic(value: SessionRecordingView) -> Self {
        Self {
            state: SessionRecordingStateOutput::from_semantic(value.state()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionRecordingStateOutput {
    Healthy,
    Degraded,
}

impl SessionRecordingStateOutput {
    const fn from_semantic(value: SessionRecordingState) -> Self {
        match value {
            SessionRecordingState::Healthy => Self::Healthy,
            SessionRecordingState::Degraded => Self::Degraded,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionSnapshotOutput<'a> {
    session_id: SessionId,
    lifecycle: SessionLifecycleOutput,
    metadata: SessionMetadataOutput<'a>,
    definition: SessionDefinitionSummaryOutput<'a>,
    load_state: SessionLoadStateOutput,
    readiness: SessionReadinessOutput,
    execution: SessionExecutionOutput,
    current_turn: Option<CurrentTurnOutput>,
    active_items: Vec<ItemOutput<'a>>,
    pending_interactions: Vec<InteractionOutput<'a>>,
    queues: SessionQueueOutput,
    recording: SessionRecordingOutput,
    usage: Option<SessionUsageOutput<'a>>,
    diagnostics: Vec<PublicDiagnosticOutput<'a>>,
}

impl<'a> SessionSnapshotOutput<'a> {
    fn from_semantic(value: &'a SessionSnapshot) -> Self {
        Self {
            session_id: value.session_id(),
            lifecycle: SessionLifecycleOutput::Open,
            metadata: SessionMetadataOutput::from_semantic(value.metadata()),
            definition: SessionDefinitionSummaryOutput::from_semantic(value.definition()),
            load_state: SessionLoadStateOutput::Loaded,
            readiness: SessionReadinessOutput::Ready,
            execution: SessionExecutionOutput::from_semantic(value.execution()),
            current_turn: value.current_turn().map(CurrentTurnOutput::from_semantic),
            active_items: value
                .active_items()
                .iter()
                .map(ItemOutput::from_semantic)
                .collect(),
            pending_interactions: value
                .pending_interactions()
                .iter()
                .map(InteractionOutput::from_semantic)
                .collect(),
            queues: SessionQueueOutput::from_semantic(value.queues()),
            recording: SessionRecordingOutput::from_semantic(value.recording()),
            usage: value.usage().map(SessionUsageOutput::from_semantic),
            diagnostics: value
                .diagnostics()
                .iter()
                .map(PublicDiagnosticOutput::from_session)
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentTurnOutput {
    turn_id: TurnId,
    status: TurnStatusOutput,
    phase: Option<TurnExecutionPhaseOutput>,
    started_at: Timestamp,
}

impl CurrentTurnOutput {
    fn from_semantic(value: CurrentTurnView) -> Self {
        Self {
            turn_id: value.turn_id(),
            status: TurnStatusOutput::from_semantic(value.status()),
            phase: value.phase().map(TurnExecutionPhaseOutput::from_semantic),
            started_at: value.started_at(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum TurnStatusOutput {
    Running,
    Completed(CompletedTurnStatusOutput),
    Interrupted(InterruptedTurnStatusOutput),
    Failed(FailedTurnStatusOutput),
}

impl TurnStatusOutput {
    fn from_semantic(value: TurnStatusView) -> Self {
        match value {
            TurnStatusView::Running => Self::Running,
            TurnStatusView::Completed { completed_at } => {
                Self::Completed(CompletedTurnStatusOutput { completed_at })
            }
            TurnStatusView::Interrupted {
                completed_at,
                reason,
            } => Self::Interrupted(InterruptedTurnStatusOutput {
                completed_at,
                reason: TurnInterruptionOutput::from_semantic(reason),
            }),
            TurnStatusView::Failed {
                completed_at,
                reason,
            } => Self::Failed(FailedTurnStatusOutput {
                completed_at,
                reason: TurnFailureOutput::from_semantic(reason),
            }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletedTurnStatusOutput {
    completed_at: Timestamp,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InterruptedTurnStatusOutput {
    completed_at: Timestamp,
    reason: TurnInterruptionOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FailedTurnStatusOutput {
    completed_at: Timestamp,
    reason: TurnFailureOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum TurnExecutionPhaseOutput {
    Sampling,
    RetryBackoff,
    Compacting,
    WaitingApproval,
    WaitingForUserInput,
    ExecutingTools,
}

impl TurnExecutionPhaseOutput {
    const fn from_semantic(value: TurnExecutionPhaseView) -> Self {
        match value {
            TurnExecutionPhaseView::Sampling => Self::Sampling,
            TurnExecutionPhaseView::RetryBackoff => Self::RetryBackoff,
            TurnExecutionPhaseView::Compacting => Self::Compacting,
            TurnExecutionPhaseView::WaitingApproval => Self::WaitingApproval,
            TurnExecutionPhaseView::WaitingForUserInput => Self::WaitingForUserInput,
            TurnExecutionPhaseView::ExecutingTools => Self::ExecutingTools,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemOutput<'a> {
    item_id: ItemId,
    turn_id: TurnId,
    status: ItemStatusOutput,
    content: ItemContentOutput<'a>,
    created_at: Timestamp,
    completed_at: Option<Timestamp>,
}

impl<'a> ItemOutput<'a> {
    fn from_semantic(value: &'a ItemView) -> Self {
        Self {
            item_id: value.item_id(),
            turn_id: value.turn_id(),
            status: ItemStatusOutput::from_semantic(value.status()),
            content: ItemContentOutput::from_semantic(value.content()),
            created_at: value.created_at(),
            completed_at: value.completed_at(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ItemStatusOutput {
    Started,
    Completed,
}

impl ItemStatusOutput {
    const fn from_semantic(value: ItemStatusView) -> Self {
        match value {
            ItemStatusView::Started => Self::Started,
            ItemStatusView::Completed => Self::Completed,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ItemContentOutput<'a> {
    UserMessage(UserMessageItemOutput<'a>),
    AgentMessage(TextItemOutput<'a>),
    Reasoning(TextItemOutput<'a>),
    ToolInvocation(ToolInvocationItemOutput<'a>),
}

impl<'a> ItemContentOutput<'a> {
    fn from_semantic(value: &'a ItemContentView) -> Self {
        match value {
            ItemContentView::UserMessage {
                source,
                body,
                contributions,
            } => Self::UserMessage(UserMessageItemOutput {
                source: UserMessageSourceOutput::from_semantic(*source),
                body,
                contributions: contributions.iter().map(|value| value.as_ref()).collect(),
            }),
            ItemContentView::AgentMessage { body } => Self::AgentMessage(TextItemOutput { body }),
            ItemContentView::Reasoning { body } => Self::Reasoning(TextItemOutput { body }),
            ItemContentView::ToolInvocation {
                tool_call_id,
                tool_name,
                arguments_summary,
                result,
            } => Self::ToolInvocation(ToolInvocationItemOutput {
                tool_call_id,
                tool_name,
                arguments_summary,
                result: result.as_deref(),
            }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserMessageItemOutput<'a> {
    source: UserMessageSourceOutput,
    body: &'a str,
    contributions: Vec<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TextItemOutput<'a> {
    body: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolInvocationItemOutput<'a> {
    tool_call_id: &'a str,
    tool_name: &'a str,
    arguments_summary: &'a str,
    result: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum UserMessageSourceOutput {
    Input,
    Steer,
}

impl UserMessageSourceOutput {
    const fn from_semantic(value: UserMessageSource) -> Self {
        match value {
            UserMessageSource::Input => Self::Input,
            UserMessageSource::Steer => Self::Steer,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InteractionOutput<'a> {
    request_id: RequestId,
    turn_id: TurnId,
    item_id: ItemId,
    request: InteractionRequestOutput<'a>,
}

impl<'a> InteractionOutput<'a> {
    fn from_semantic(value: &'a InteractionView) -> Self {
        Self {
            request_id: value.request_id(),
            turn_id: value.turn_id(),
            item_id: value.item_id(),
            request: InteractionRequestOutput::from_semantic(value.request()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum InteractionRequestOutput<'a> {
    ToolApproval(ToolApprovalRequestOutput<'a>),
    UserQuestion(UserQuestionRequestOutput<'a>),
}

impl<'a> InteractionRequestOutput<'a> {
    fn from_semantic(value: &'a InteractionRequestView) -> Self {
        match value {
            InteractionRequestView::ToolApproval(request) => {
                Self::ToolApproval(ToolApprovalRequestOutput::from_semantic(request))
            }
            InteractionRequestView::UserQuestion(request) => {
                Self::UserQuestion(UserQuestionRequestOutput::from_semantic(request))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolApprovalRequestOutput<'a> {
    tool_name: &'a str,
    arguments_summary: &'a str,
    reason: &'a str,
    requirements: ToolRequirementsOutput<'a>,
    options: Vec<ToolApprovalOptionOutput<'a>>,
}

impl<'a> ToolApprovalRequestOutput<'a> {
    fn from_semantic(value: &'a crate::tools::ToolApprovalRequestView) -> Self {
        Self {
            tool_name: value.tool_name().as_str(),
            arguments_summary: value.arguments_summary(),
            reason: value.reason(),
            requirements: ToolRequirementsOutput::from_semantic(value.requirements()),
            options: value
                .options()
                .iter()
                .map(ToolApprovalOptionOutput::from_semantic)
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolRequirementsOutput<'a> {
    filesystem: Option<&'a str>,
    network: Option<&'a str>,
    process: Option<&'a str>,
}

impl<'a> ToolRequirementsOutput<'a> {
    fn from_semantic(value: &'a ToolRequirementSummaryView) -> Self {
        Self {
            filesystem: value.filesystem(),
            network: value.network(),
            process: value.process(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolApprovalOptionOutput<'a> {
    option_index: u32,
    kind: ToolApprovalOptionKindOutput,
    label: &'a str,
    effective_requirements: ToolRequirementsOutput<'a>,
}

impl<'a> ToolApprovalOptionOutput<'a> {
    fn from_semantic(value: &'a ToolApprovalOptionView) -> Self {
        Self {
            option_index: value.option_index(),
            kind: ToolApprovalOptionKindOutput::from_semantic(value.kind()),
            label: value.label(),
            effective_requirements: ToolRequirementsOutput::from_semantic(
                value.effective_requirements(),
            ),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolApprovalOptionKindOutput {
    AsRequested,
    Restricted,
}

impl ToolApprovalOptionKindOutput {
    const fn from_semantic(value: ToolApprovalOptionKindView) -> Self {
        match value {
            ToolApprovalOptionKindView::AsRequested => Self::AsRequested,
            ToolApprovalOptionKindView::Restricted => Self::Restricted,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserQuestionRequestOutput<'a> {
    title: Option<&'a str>,
    questions: Vec<UserQuestionFieldOutput<'a>>,
}

impl<'a> UserQuestionRequestOutput<'a> {
    fn from_semantic(value: &'a crate::tools::UserQuestionRequest) -> Self {
        Self {
            title: value.title(),
            questions: value
                .questions()
                .iter()
                .map(UserQuestionFieldOutput::from_semantic)
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserQuestionFieldOutput<'a> {
    question_index: u32,
    prompt: &'a str,
    required: bool,
    input: UserQuestionInputOutput<'a>,
}

impl<'a> UserQuestionFieldOutput<'a> {
    fn from_semantic(value: &'a UserQuestionField) -> Self {
        Self {
            question_index: value.question_index(),
            prompt: value.prompt(),
            required: value.required(),
            input: UserQuestionInputOutput::from_semantic(value.input()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum UserQuestionInputOutput<'a> {
    Text(TextQuestionOutput),
    SingleChoice(SingleChoiceQuestionOutput<'a>),
}

impl<'a> UserQuestionInputOutput<'a> {
    fn from_semantic(value: &'a UserQuestionInput) -> Self {
        match value {
            UserQuestionInput::Text { multiline } => Self::Text(TextQuestionOutput {
                multiline: *multiline,
            }),
            UserQuestionInput::SingleChoice { options } => {
                Self::SingleChoice(SingleChoiceQuestionOutput {
                    options: options
                        .iter()
                        .map(|option| UserQuestionChoiceOutput {
                            option_index: option.option_index(),
                            label: option.label(),
                        })
                        .collect(),
                })
            }
        }
    }
}

#[derive(Serialize)]
struct TextQuestionOutput {
    multiline: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SingleChoiceQuestionOutput<'a> {
    options: Vec<UserQuestionChoiceOutput<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserQuestionChoiceOutput<'a> {
    option_index: u32,
    label: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionLifecycleOutput {
    Open,
    Archived,
    Deleted,
}

impl SessionLifecycleOutput {
    const fn from_semantic(value: SessionLifecycleView) -> Self {
        match value {
            SessionLifecycleView::Open => Self::Open,
            SessionLifecycleView::Archived => Self::Archived,
            SessionLifecycleView::Deleted => Self::Deleted,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionLoadStateOutput {
    Loaded,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionMetadataOutput<'a> {
    revision: SessionMetadataRevision,
    name: Option<&'a str>,
    description: Option<&'a str>,
    updated_at: Timestamp,
}

impl<'a> SessionMetadataOutput<'a> {
    fn from_semantic(value: &'a SessionMetadataView) -> Self {
        Self {
            revision: value.revision(),
            name: value.name(),
            description: value.description(),
            updated_at: value.updated_at(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDefinitionSummaryOutput<'a> {
    session_id: SessionId,
    revision: SessionDefinitionRevision,
    agent: AgentRevisionRefOutput,
    workspace: WorkspaceDefinitionSummaryOutput<'a>,
    model: SessionModelConfigOutput<'a>,
    prompt_ids: Vec<&'a str>,
    created_at: Timestamp,
}

impl<'a> SessionDefinitionSummaryOutput<'a> {
    fn from_semantic(value: &'a SessionDefinitionSummary) -> Self {
        Self {
            session_id: value.session_id(),
            revision: value.revision(),
            agent: AgentRevisionRefOutput::from_semantic(value.agent()),
            workspace: WorkspaceDefinitionSummaryOutput::from_semantic(value.workspace()),
            model: SessionModelConfigOutput::from_semantic(value.model()),
            prompt_ids: value
                .prompts()
                .enabled()
                .iter()
                .map(PromptId::as_str)
                .collect(),
            created_at: value.created_at(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentRevisionRefOutput {
    agent_id: AgentId,
    revision: AgentRevision,
}

impl AgentRevisionRefOutput {
    const fn from_semantic(value: AgentRevisionRef) -> Self {
        Self {
            agent_id: value.agent_id(),
            revision: value.revision(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDefinitionSummaryOutput<'a> {
    roots: Vec<WorkspaceRootSummaryOutput<'a>>,
    cwd_root: &'a str,
    cwd_relative: &'a WorkspaceRelativePath,
}

impl<'a> WorkspaceDefinitionSummaryOutput<'a> {
    fn from_semantic(value: &'a WorkspaceDefinitionSummaryView) -> Self {
        Self {
            roots: value
                .roots()
                .iter()
                .map(WorkspaceRootSummaryOutput::from_semantic)
                .collect(),
            cwd_root: value.cwd().root().as_str(),
            cwd_relative: value.cwd().relative_path(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRootSummaryOutput<'a> {
    key: &'a str,
    requested_access: RequestedFilesystemAccessOutput,
    prompt_source: bool,
    skill_source: bool,
}

impl<'a> WorkspaceRootSummaryOutput<'a> {
    fn from_semantic(value: &'a WorkspaceRootSummaryView) -> Self {
        let sources = value.sources();
        Self {
            key: value.key().as_str(),
            requested_access: RequestedFilesystemAccessOutput::from_semantic(
                value.requested_access(),
            ),
            prompt_source: sources.prompt(),
            skill_source: sources.skill(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionQueueOutput {
    submit_admissions: Vec<SubmitAdmissionOutput>,
    steers: Vec<QueuedSteerOutput>,
    follow_ups: Vec<QueuedFollowUpOutput>,
    accepting_input: bool,
}

impl SessionQueueOutput {
    fn from_semantic(value: &SessionQueueView) -> Self {
        Self {
            submit_admissions: value
                .submit_admissions()
                .iter()
                .copied()
                .map(SubmitAdmissionOutput::from_semantic)
                .collect(),
            steers: value
                .steers()
                .iter()
                .copied()
                .map(QueuedSteerOutput::from_semantic)
                .collect(),
            follow_ups: value
                .follow_ups()
                .iter()
                .copied()
                .map(QueuedFollowUpOutput::from_semantic)
                .collect(),
            accepting_input: value.accepting_input(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitAdmissionOutput {
    command_id: CommandId,
    state: SubmitAdmissionStateOutput,
}

impl SubmitAdmissionOutput {
    const fn from_semantic(value: SubmitAdmissionView) -> Self {
        Self {
            command_id: value.command_id(),
            state: SubmitAdmissionStateOutput::from_semantic(value.state()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SubmitAdmissionStateOutput {
    Queued,
    Starting,
}

impl SubmitAdmissionStateOutput {
    const fn from_semantic(value: SubmitAdmissionStateView) -> Self {
        match value {
            SubmitAdmissionStateView::Queued => Self::Queued,
            SubmitAdmissionStateView::Starting => Self::Starting,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QueuedSteerOutput {
    command_id: CommandId,
    expected_turn_id: TurnId,
}

impl QueuedSteerOutput {
    const fn from_semantic(value: QueuedSteerView) -> Self {
        Self {
            command_id: value.command_id(),
            expected_turn_id: value.expected_turn_id(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QueuedFollowUpOutput {
    command_id: CommandId,
}

impl QueuedFollowUpOutput {
    const fn from_semantic(value: QueuedFollowUpView) -> Self {
        Self {
            command_id: value.command_id(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionUsageOutput<'a> {
    model_calls: super::CanonicalU64,
    compaction_calls: super::CanonicalU64,
    input_tokens: Option<super::CanonicalU64>,
    output_tokens: Option<super::CanonicalU64>,
    reasoning_tokens: Option<super::CanonicalU64>,
    cache_read_tokens: Option<super::CanonicalU64>,
    cache_write_tokens: Option<super::CanonicalU64>,
    reported_costs: &'a [Money],
}

impl<'a> SessionUsageOutput<'a> {
    fn from_semantic(value: &'a SessionUsageView) -> Self {
        Self {
            model_calls: super::CanonicalU64::new(value.model_calls()),
            compaction_calls: super::CanonicalU64::new(value.compaction_calls()),
            input_tokens: value.input_tokens().map(super::CanonicalU64::new),
            output_tokens: value.output_tokens().map(super::CanonicalU64::new),
            reasoning_tokens: value.reasoning_tokens().map(super::CanonicalU64::new),
            cache_read_tokens: value.cache_read_tokens().map(super::CanonicalU64::new),
            cache_write_tokens: value.cache_write_tokens().map(super::CanonicalU64::new),
            reported_costs: value.reported_costs(),
        }
    }
}

#[derive(Serialize)]
struct PublicDiagnosticOutput<'a> {
    code: &'a str,
    message: &'a str,
}

impl<'a> PublicDiagnosticOutput<'a> {
    fn from_runtime(value: &'a RuntimeDiagnosticView) -> Self {
        Self {
            code: value.code(),
            message: value.message(),
        }
    }

    fn from_session(value: &'a SessionDiagnosticView) -> Self {
        Self {
            code: value.code(),
            message: value.message(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateEventOutput<'a> {
    timestamp: Timestamp,
    command_id: Option<CommandId>,
    route: EventRouteOutput,
    msg: StateEventMsgOutput<'a>,
}

impl<'a> StateEventOutput<'a> {
    fn from_semantic(value: &'a StateEvent) -> Self {
        Self {
            timestamp: value.timestamp(),
            command_id: value.command_id(),
            route: EventRouteOutput::from_semantic(value.route()),
            msg: StateEventMsgOutput::from_semantic(value.msg()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum EventRouteOutput {
    Runtime,
    Agent(AgentRouteOutput),
    Session(SessionIdOutput),
    Turn(TurnRouteOutput),
    Item(ItemRouteOutput),
    Interaction(InteractionRouteOutput),
}

impl EventRouteOutput {
    const fn from_semantic(value: EventRoute) -> Self {
        match value {
            EventRoute::Runtime => Self::Runtime,
            EventRoute::Agent { agent_id } => Self::Agent(AgentRouteOutput { agent_id }),
            EventRoute::Session { session_id } => Self::Session(SessionIdOutput { session_id }),
            EventRoute::Turn {
                session_id,
                turn_id,
            } => Self::Turn(TurnRouteOutput {
                session_id,
                turn_id,
            }),
            EventRoute::Item {
                session_id,
                turn_id,
                item_id,
            } => Self::Item(ItemRouteOutput {
                session_id,
                turn_id,
                item_id,
            }),
            EventRoute::Interaction {
                session_id,
                turn_id,
                item_id,
                request_id,
            } => Self::Interaction(InteractionRouteOutput {
                session_id,
                turn_id,
                item_id,
                request_id,
            }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentRouteOutput {
    agent_id: AgentId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnRouteOutput {
    session_id: SessionId,
    turn_id: TurnId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemRouteOutput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InteractionRouteOutput {
    session_id: SessionId,
    turn_id: TurnId,
    item_id: ItemId,
    request_id: RequestId,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum StateEventMsgOutput<'a> {
    Runtime(RuntimeStateEventOutput<'a>),
    Session(Box<SessionStateEventOutput<'a>>),
}

impl<'a> StateEventMsgOutput<'a> {
    fn from_semantic(value: &'a StateEventMsg) -> Self {
        match value {
            StateEventMsg::Runtime {
                kind,
                snapshot,
                detail,
            } => Self::Runtime(RuntimeStateEventOutput {
                kind: RuntimeStateEventKindOutput::from_semantic(*kind),
                snapshot: RuntimeSnapshotOutput::from_semantic(snapshot),
                detail: detail.as_ref().map(RuntimeEventDetailOutput::from_semantic),
            }),
            StateEventMsg::Session {
                kind,
                snapshot,
                detail,
            } => Self::Session(Box::new(SessionStateEventOutput {
                kind: SessionStateEventKindOutput::from_semantic(*kind),
                snapshot: SessionSnapshotOutput::from_semantic(snapshot),
                detail: detail.map(SessionEventDetailOutput::from_semantic),
            })),
        }
    }
}

#[derive(Serialize)]
struct RuntimeStateEventOutput<'a> {
    kind: RuntimeStateEventKindOutput,
    snapshot: RuntimeSnapshotOutput<'a>,
    detail: Option<RuntimeEventDetailOutput<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeStateEventKindOutput {
    AgentCreated,
    AgentStatusChanged,
    SessionCreated,
    SessionLoaded,
    SessionUnloaded,
    SessionArchived,
    SessionUnarchived,
    SessionDeleted,
    SessionForked,
    CommandCatalogInvalidated,
}

impl RuntimeStateEventKindOutput {
    const fn from_semantic(value: RuntimeStateEventKind) -> Self {
        match value {
            RuntimeStateEventKind::AgentCreated => Self::AgentCreated,
            RuntimeStateEventKind::AgentStatusChanged => Self::AgentStatusChanged,
            RuntimeStateEventKind::SessionCreated => Self::SessionCreated,
            RuntimeStateEventKind::SessionLoaded => Self::SessionLoaded,
            RuntimeStateEventKind::SessionUnloaded => Self::SessionUnloaded,
            RuntimeStateEventKind::SessionArchived => Self::SessionArchived,
            RuntimeStateEventKind::SessionUnarchived => Self::SessionUnarchived,
            RuntimeStateEventKind::SessionDeleted => Self::SessionDeleted,
            RuntimeStateEventKind::SessionForked => Self::SessionForked,
            RuntimeStateEventKind::CommandCatalogInvalidated => Self::CommandCatalogInvalidated,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeEventDetailOutput<'a> {
    AgentChanged(AgentChangedDetailOutput<'a>),
    SessionChanged(Box<SessionChangedDetailOutput<'a>>),
}

impl<'a> RuntimeEventDetailOutput<'a> {
    fn from_semantic(value: &'a RuntimeEventDetail) -> Self {
        match value {
            RuntimeEventDetail::AgentChanged { agent } => {
                Self::AgentChanged(AgentChangedDetailOutput {
                    agent: AgentSummaryOutput::from_semantic(agent),
                })
            }
            RuntimeEventDetail::SessionChanged { session } => {
                Self::SessionChanged(Box::new(SessionChangedDetailOutput {
                    session: SessionSummaryOutput::from_semantic(session),
                }))
            }
        }
    }
}

#[derive(Serialize)]
struct AgentChangedDetailOutput<'a> {
    agent: AgentSummaryOutput<'a>,
}

#[derive(Serialize)]
struct SessionChangedDetailOutput<'a> {
    session: SessionSummaryOutput<'a>,
}

#[derive(Serialize)]
struct SessionStateEventOutput<'a> {
    kind: SessionStateEventKindOutput,
    snapshot: SessionSnapshotOutput<'a>,
    detail: Option<SessionEventDetailOutput>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(
    clippy::enum_variant_names,
    reason = "the wire discriminator names are fixed by the public protocol"
)]
enum SessionStateEventKindOutput {
    SessionExecutionChanged,
    TurnCompleted,
    TurnInterrupted,
    TurnFailed,
}

impl SessionStateEventKindOutput {
    const fn from_semantic(value: SessionStateEventKind) -> Self {
        match value {
            SessionStateEventKind::SessionExecutionChanged => Self::SessionExecutionChanged,
            SessionStateEventKind::TurnCompleted => Self::TurnCompleted,
            SessionStateEventKind::TurnInterrupted => Self::TurnInterrupted,
            SessionStateEventKind::TurnFailed => Self::TurnFailed,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionEventDetailOutput {
    TurnTerminal(TurnTerminalDetailOutput),
}

impl SessionEventDetailOutput {
    const fn from_semantic(value: SessionEventDetail) -> Self {
        match value {
            SessionEventDetail::TurnTerminal { turn_id, terminal } => {
                Self::TurnTerminal(TurnTerminalDetailOutput {
                    turn_id,
                    terminal: TurnTerminalOutput::from_semantic(terminal),
                })
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnTerminalDetailOutput {
    turn_id: TurnId,
    terminal: TurnTerminalOutput,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum TurnTerminalOutput {
    Completed(CompletedTerminalOutput),
    Interrupted(InterruptedTerminalOutput),
    Failed(FailedTerminalOutput),
}

impl TurnTerminalOutput {
    const fn from_semantic(value: TurnTerminalView) -> Self {
        match value {
            TurnTerminalView::Completed { completed_at } => {
                Self::Completed(CompletedTerminalOutput { completed_at })
            }
            TurnTerminalView::Interrupted {
                completed_at,
                reason,
            } => Self::Interrupted(InterruptedTerminalOutput {
                completed_at,
                reason: TurnInterruptionOutput::from_semantic(reason),
            }),
            TurnTerminalView::Failed {
                completed_at,
                reason,
            } => Self::Failed(FailedTerminalOutput {
                completed_at,
                reason: TurnFailureOutput::from_semantic(reason),
            }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletedTerminalOutput {
    completed_at: Timestamp,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InterruptedTerminalOutput {
    completed_at: Timestamp,
    reason: TurnInterruptionOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum TurnInterruptionOutput {
    UserCancelled,
    SecurityRevoked,
    PrepareForUnload,
    RuntimeShutdown,
    RuntimeFailure,
}

impl TurnInterruptionOutput {
    const fn from_semantic(value: TurnInterruptionView) -> Self {
        match value {
            TurnInterruptionView::UserCancelled => Self::UserCancelled,
            TurnInterruptionView::SecurityRevoked => Self::SecurityRevoked,
            TurnInterruptionView::PrepareForUnload => Self::PrepareForUnload,
            TurnInterruptionView::RuntimeShutdown => Self::RuntimeShutdown,
            TurnInterruptionView::RuntimeFailure => Self::RuntimeFailure,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FailedTerminalOutput {
    completed_at: Timestamp,
    reason: TurnFailureOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum TurnFailureOutput {
    Prompt,
    Model,
    Tool,
    ContextOverflow,
    DependencyUnavailable,
    InvariantFailure,
}

impl TurnFailureOutput {
    const fn from_semantic(value: TurnFailureView) -> Self {
        match value {
            TurnFailureView::Prompt => Self::Prompt,
            TurnFailureView::Model => Self::Model,
            TurnFailureView::Tool => Self::Tool,
            TurnFailureView::ContextOverflow => Self::ContextOverflow,
            TurnFailureView::DependencyUnavailable => Self::DependencyUnavailable,
            TurnFailureView::InvariantFailure => Self::InvariantFailure,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum TurnCommandInput {
    Submit(SubmitCommandInput),
    Steer(SteerCommandInput),
    FollowUp(FollowUpCommandInput),
    CancelQueuedMessage(CancelQueuedMessageCommandInput),
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
struct SteerCommandInput {
    session_id: SessionId,
    expected_turn_id: TurnId,
    intent: PromptIntentWireInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FollowUpCommandInput {
    session_id: SessionId,
    intent: PromptIntentWireInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelQueuedMessageCommandInput {
    session_id: SessionId,
    target_command_id: CommandId,
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
    Agent(AgentCommandOutput<'a>),
    Session(SessionCommandOutput<'a>),
    Turn(TurnCommandOutput<'a>),
    Interaction(InteractionCommandOutput<'a>),
}

impl<'a> RuntimeCommandOutput<'a> {
    fn from_semantic(value: &'a RuntimeCommand) -> Self {
        match value {
            RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources) => {
                Self::Runtime(RuntimeLifecycleCommandOutput::ReloadSharedResources)
            }
            RuntimeCommand::Agent(AgentCommand::Create {
                definition,
                metadata,
            }) => Self::Agent(AgentCommandOutput::Create(CreateAgentCommandOutput {
                definition: NewAgentDefinitionOutput::from_semantic(definition),
                metadata: NewAgentMetadataOutput::from_semantic(metadata),
            })),
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status,
                status,
            }) => Self::Agent(AgentCommandOutput::SetStatus(SetAgentStatusCommandOutput {
                agent_id: *agent_id,
                expected_status: AgentStatusOutput::from_semantic(*expected_status),
                status: AgentUsableStatusOutput::from_semantic(*status),
            })),
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id,
                expected_status,
            }) => Self::Agent(AgentCommandOutput::Delete(DeleteAgentCommandOutput {
                agent_id: *agent_id,
                expected_status: AgentStatusOutput::from_semantic(*expected_status),
            })),
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
            RuntimeCommand::Session(SessionCommand::Archive { session_id }) => {
                Self::Session(SessionCommandOutput::Archive(SessionIdOutput {
                    session_id: *session_id,
                }))
            }
            RuntimeCommand::Session(SessionCommand::Unarchive { session_id }) => {
                Self::Session(SessionCommandOutput::Unarchive(SessionIdOutput {
                    session_id: *session_id,
                }))
            }
            RuntimeCommand::Session(SessionCommand::Delete { session_id }) => {
                Self::Session(SessionCommandOutput::Delete(SessionIdOutput {
                    session_id: *session_id,
                }))
            }
            RuntimeCommand::Session(SessionCommand::Fork {
                source_session_id,
                anchor,
            }) => Self::Session(SessionCommandOutput::Fork(ForkSessionCommandOutput {
                source_session_id: *source_session_id,
                anchor: ForkAnchorOutput::from_semantic(anchor),
            })),
            RuntimeCommand::Turn(TurnCommand::Submit { session_id, intent }) => {
                Self::Turn(TurnCommandOutput::Submit(SubmitCommandOutput {
                    session_id: *session_id,
                    intent: PromptIntentWireOutput::from_semantic(intent),
                }))
            }
            RuntimeCommand::Turn(TurnCommand::Steer {
                session_id,
                expected_turn_id,
                intent,
            }) => Self::Turn(TurnCommandOutput::Steer(SteerCommandOutput {
                session_id: *session_id,
                expected_turn_id: *expected_turn_id,
                intent: PromptIntentWireOutput::from_semantic(intent),
            })),
            RuntimeCommand::Turn(TurnCommand::FollowUp { session_id, intent }) => {
                Self::Turn(TurnCommandOutput::FollowUp(FollowUpCommandOutput {
                    session_id: *session_id,
                    intent: PromptIntentWireOutput::from_semantic(intent),
                }))
            }
            RuntimeCommand::Turn(TurnCommand::CancelQueuedMessage {
                session_id,
                target_command_id,
            }) => Self::Turn(TurnCommandOutput::CancelQueuedMessage(
                CancelQueuedMessageCommandOutput {
                    session_id: *session_id,
                    target_command_id: *target_command_id,
                },
            )),
            RuntimeCommand::Turn(TurnCommand::Cancel { session_id, target }) => {
                Self::Turn(TurnCommandOutput::Cancel(CancelCommandOutput {
                    session_id: *session_id,
                    target: PublicCancelTargetOutput::from_semantic(*target),
                }))
            }
            RuntimeCommand::Interaction(InteractionCommand::Resolve {
                session_id,
                expected_turn_id,
                item_id,
                request_id,
                resolution,
                resolution_key,
            }) => Self::Interaction(InteractionCommandOutput::Resolve(
                ResolveInteractionCommandOutput {
                    session_id: *session_id,
                    expected_turn_id: *expected_turn_id,
                    item_id: *item_id,
                    request_id: *request_id,
                    resolution: InteractionResolutionCommandOutput::from_semantic(resolution),
                    resolution_key: resolution_key.clone(),
                },
            )),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum AgentCommandOutput<'a> {
    Create(CreateAgentCommandOutput<'a>),
    SetStatus(SetAgentStatusCommandOutput),
    Delete(DeleteAgentCommandOutput),
}

#[derive(Serialize)]
struct CreateAgentCommandOutput<'a> {
    definition: NewAgentDefinitionOutput<'a>,
    metadata: NewAgentMetadataOutput<'a>,
}

#[derive(Serialize)]
struct NewAgentDefinitionOutput<'a> {
    prompts: AgentPromptSelectionOutput<'a>,
}

impl<'a> NewAgentDefinitionOutput<'a> {
    fn from_semantic(value: &'a NewAgentDefinition) -> Self {
        Self {
            prompts: AgentPromptSelectionOutput::from_semantic(value.prompts()),
        }
    }
}

#[derive(Serialize)]
struct AgentPromptSelectionOutput<'a> {
    enabled: Vec<&'a str>,
}

impl<'a> AgentPromptSelectionOutput<'a> {
    fn from_semantic(value: &'a AgentPromptSelection) -> Self {
        Self {
            enabled: value.enabled().iter().map(PromptId::as_str).collect(),
        }
    }
}

#[derive(Serialize)]
struct NewAgentMetadataOutput<'a> {
    name: &'a str,
    description: Option<&'a str>,
}

impl<'a> NewAgentMetadataOutput<'a> {
    fn from_semantic(value: &'a NewAgentMetadata) -> Self {
        Self {
            name: value.name(),
            description: value.description(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetAgentStatusCommandOutput {
    agent_id: AgentId,
    expected_status: AgentStatusOutput,
    status: AgentUsableStatusOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteAgentCommandOutput {
    agent_id: AgentId,
    expected_status: AgentStatusOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentUsableStatusOutput {
    Enabled,
    Disabled,
}

impl AgentUsableStatusOutput {
    const fn from_semantic(value: AgentUsableStatus) -> Self {
        match value {
            AgentUsableStatus::Enabled => Self::Enabled,
            AgentUsableStatus::Disabled => Self::Disabled,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum InteractionCommandOutput<'a> {
    Resolve(ResolveInteractionCommandOutput<'a>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveInteractionCommandOutput<'a> {
    session_id: SessionId,
    expected_turn_id: TurnId,
    item_id: ItemId,
    request_id: RequestId,
    resolution: InteractionResolutionCommandOutput<'a>,
    resolution_key: InteractionResolutionKey,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum InteractionResolutionCommandOutput<'a> {
    ToolApproval(ToolApprovalDecisionCommandOutput),
    UserAnswer(UserQuestionAnswerCommandOutput<'a>),
    Cancelled,
}

impl<'a> InteractionResolutionCommandOutput<'a> {
    fn from_semantic(value: &'a InteractionResolutionInput) -> Self {
        match value {
            InteractionResolutionInput::ToolApproval(value) => {
                Self::ToolApproval(ToolApprovalDecisionCommandOutput::from_semantic(*value))
            }
            InteractionResolutionInput::UserAnswer(value) => {
                Self::UserAnswer(UserQuestionAnswerCommandOutput {
                    answers: value
                        .answers()
                        .iter()
                        .map(UserQuestionFieldAnswerCommandOutput::from_semantic)
                        .collect(),
                })
            }
            InteractionResolutionInput::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ToolApprovalDecisionCommandOutput {
    Allow(OptionIndexOutput),
    Deny,
}

impl ToolApprovalDecisionCommandOutput {
    const fn from_semantic(value: ToolApprovalDecisionInput) -> Self {
        match value {
            ToolApprovalDecisionInput::Allow { option_index } => {
                Self::Allow(OptionIndexOutput { option_index })
            }
            ToolApprovalDecisionInput::Deny => Self::Deny,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OptionIndexOutput {
    option_index: u32,
}

#[derive(Serialize)]
struct UserQuestionAnswerCommandOutput<'a> {
    answers: Vec<UserQuestionFieldAnswerCommandOutput<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserQuestionFieldAnswerCommandOutput<'a> {
    question_index: u32,
    value: UserQuestionAnswerValueCommandOutput<'a>,
}

impl<'a> UserQuestionFieldAnswerCommandOutput<'a> {
    fn from_semantic(value: &'a UserQuestionFieldAnswer) -> Self {
        Self {
            question_index: value.question_index(),
            value: UserQuestionAnswerValueCommandOutput::from_semantic(value.value()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum UserQuestionAnswerValueCommandOutput<'a> {
    Text(TextValueOutput<'a>),
    Choice(OptionIndexOutput),
}

impl<'a> UserQuestionAnswerValueCommandOutput<'a> {
    fn from_semantic(value: &'a UserQuestionAnswerValue) -> Self {
        match value {
            UserQuestionAnswerValue::Text(text) => Self::Text(TextValueOutput { text }),
            UserQuestionAnswerValue::Choice { option_index } => Self::Choice(OptionIndexOutput {
                option_index: *option_index,
            }),
        }
    }
}

#[derive(Serialize)]
struct TextValueOutput<'a> {
    text: &'a str,
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
    Archive(SessionIdOutput),
    Unarchive(SessionIdOutput),
    Delete(SessionIdOutput),
    Fork(ForkSessionCommandOutput),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ForkSessionCommandOutput {
    source_session_id: SessionId,
    anchor: ForkAnchorOutput,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ForkAnchorOutput {
    Genesis,
    BeforeUserMessage(ForkItemAnchorOutput),
    AfterUserMessage(ForkItemAnchorOutput),
    BeforeFinalAgentMessage(ForkItemAnchorOutput),
    AfterFinalAgentMessage(ForkItemAnchorOutput),
}

impl ForkAnchorOutput {
    const fn from_semantic(value: &ForkAnchor) -> Self {
        match value {
            ForkAnchor::Genesis => Self::Genesis,
            ForkAnchor::BeforeUserMessage { item_id } => {
                Self::BeforeUserMessage(ForkItemAnchorOutput { item_id: *item_id })
            }
            ForkAnchor::AfterUserMessage { item_id } => {
                Self::AfterUserMessage(ForkItemAnchorOutput { item_id: *item_id })
            }
            ForkAnchor::BeforeFinalAgentMessage { item_id } => {
                Self::BeforeFinalAgentMessage(ForkItemAnchorOutput { item_id: *item_id })
            }
            ForkAnchor::AfterFinalAgentMessage { item_id } => {
                Self::AfterFinalAgentMessage(ForkItemAnchorOutput { item_id: *item_id })
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ForkItemAnchorOutput {
    item_id: crate::wire::ItemId,
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
    Steer(SteerCommandOutput<'a>),
    FollowUp(FollowUpCommandOutput<'a>),
    CancelQueuedMessage(CancelQueuedMessageCommandOutput),
    Cancel(CancelCommandOutput),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitCommandOutput<'a> {
    session_id: SessionId,
    intent: PromptIntentWireOutput<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SteerCommandOutput<'a> {
    session_id: SessionId,
    expected_turn_id: TurnId,
    intent: PromptIntentWireOutput<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FollowUpCommandOutput<'a> {
    session_id: SessionId,
    intent: PromptIntentWireOutput<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelQueuedMessageCommandOutput {
    session_id: SessionId,
    target_command_id: CommandId,
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
    AgentCreated(AgentCreatedInput),
    AgentStatusChanged(AgentStatusChangedInput),
    AgentDeleted,
    SessionDefinitionUpdated(SessionDefinitionUpdatedInput),
    SessionForked(SessionForkedInput),
    SessionArchived,
    SessionUnarchived,
    SessionDeleted,
    InteractionResolved,
    TurnStarted(TurnIdInput),
    SubmitCancelled,
    SteerQueued(TurnIdInput),
    FollowUpQueued,
    QueuedMessageCancelled,
    CancelAccepted(CancelAcceptedInput),
    CommandOutput,
    NoChange,
}

impl CommandOutcomeInput {
    const fn into_semantic(self) -> CommandOutcome {
        match self {
            Self::AgentCreated(value) => CommandOutcome::AgentCreated {
                agent_id: value.agent_id,
                definition_revision: value.definition_revision,
                metadata_revision: value.metadata_revision,
            },
            Self::AgentStatusChanged(value) => CommandOutcome::AgentStatusChanged {
                status: value.status.into_semantic(),
            },
            Self::AgentDeleted => CommandOutcome::AgentDeleted,
            Self::SessionDefinitionUpdated(value) => CommandOutcome::SessionDefinitionUpdated {
                definition_revision: value.definition_revision,
            },
            Self::SessionForked(value) => CommandOutcome::SessionForked {
                session_id: value.session_id,
                source: value.source.into_semantic(),
            },
            Self::SessionArchived => CommandOutcome::SessionArchived,
            Self::SessionUnarchived => CommandOutcome::SessionUnarchived,
            Self::SessionDeleted => CommandOutcome::SessionDeleted,
            Self::InteractionResolved => CommandOutcome::InteractionResolved,
            Self::TurnStarted(value) => CommandOutcome::TurnStarted {
                turn_id: value.turn_id,
            },
            Self::SubmitCancelled => CommandOutcome::SubmitCancelled,
            Self::SteerQueued(value) => CommandOutcome::SteerQueued {
                turn_id: value.turn_id,
            },
            Self::FollowUpQueued => CommandOutcome::FollowUpQueued,
            Self::QueuedMessageCancelled => CommandOutcome::QueuedMessageCancelled,
            Self::CancelAccepted(value) => CommandOutcome::CancelAccepted {
                target: value.target.into_semantic(),
                cancel_epoch: value.cancel_epoch.get(),
            },
            Self::CommandOutput => CommandOutcome::CommandOutput,
            Self::NoChange => CommandOutcome::NoChange,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentCreatedInput {
    agent_id: AgentId,
    definition_revision: AgentRevision,
    metadata_revision: AgentMetadataRevision,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStatusChangedInput {
    status: AgentStatusInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionDefinitionUpdatedInput {
    definition_revision: SessionDefinitionRevision,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionForkedInput {
    session_id: SessionId,
    source: ForkSourceKindInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ForkSourceKindInput {
    LiveSnapshot,
    RecordedHistory,
}

impl ForkSourceKindInput {
    const fn into_semantic(self) -> ForkSourceKind {
        match self {
            Self::LiveSnapshot => ForkSourceKind::LiveSnapshot,
            Self::RecordedHistory => ForkSourceKind::RecordedHistory,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnIdInput {
    turn_id: TurnId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelAcceptedInput {
    target: PublicCancelTargetInput,
    cancel_epoch: super::CanonicalU64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandErrorInput {
    code: CommandErrorCodeInput,
    message: String,
    retry: RetryAdviceInput,
    subject: Option<PublicSubjectInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryErrorInput {
    code: QueryErrorCodeInput,
    message: String,
    retry: RetryAdviceInput,
    subject: Option<PublicSubjectInput>,
}

impl QueryErrorInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<QueryError, TypedJsonError> {
        validate_command_error_message(
            &self.message,
            limits.text.max_diagnostic_message_bytes as usize,
        )
        .map_err(|_| invalid_scalar())?;
        Ok(QueryError::new_boxed(
            self.code.into_semantic(),
            self.message.into_boxed_str(),
            self.retry.into_semantic()?,
            self.subject
                .map(PublicSubjectInput::into_semantic)
                .transpose()?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueryErrorCodeInput {
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

impl QueryErrorCodeInput {
    const fn into_semantic(self) -> QueryErrorCode {
        match self {
            Self::InvalidArgument => QueryErrorCode::InvalidArgument,
            Self::NotFound => QueryErrorCode::NotFound,
            Self::SessionNotLoaded => QueryErrorCode::SessionNotLoaded,
            Self::StaleCursor => QueryErrorCode::StaleCursor,
            Self::ResultTooLarge => QueryErrorCode::ResultTooLarge,
            Self::Unavailable => QueryErrorCode::Unavailable,
            Self::DurableStateCorrupt => QueryErrorCode::DurableStateCorrupt,
            Self::DurableStateTooLarge => QueryErrorCode::DurableStateTooLarge,
            Self::RuntimeClosing => QueryErrorCode::RuntimeClosing,
        }
    }
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
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RetryAdviceInput {
    DoNotRetry,
    RefreshAndRetry,
    RetryWithBackoff(RetryWithBackoffInput),
    UserActionRequired,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetryWithBackoffInput {
    retry_after: Option<Duration>,
}

impl RetryAdviceInput {
    fn into_semantic(self) -> Result<RetryAdvice, TypedJsonError> {
        Ok(match self {
            Self::DoNotRetry => RetryAdvice::DoNotRetry,
            Self::RefreshAndRetry => RetryAdvice::RefreshAndRetry,
            Self::RetryWithBackoff(value) => RetryAdvice::RetryWithBackoff {
                retry_after: value.retry_after,
            },
            Self::UserActionRequired => RetryAdvice::UserActionRequired,
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
    AgentCreated(AgentCreatedOutput),
    AgentStatusChanged(AgentStatusChangedOutput),
    AgentDeleted,
    SessionDefinitionUpdated(SessionDefinitionUpdatedOutput),
    SessionForked(SessionForkedOutput),
    SessionArchived,
    SessionUnarchived,
    SessionDeleted,
    InteractionResolved,
    TurnStarted(TurnIdOutput),
    SubmitCancelled,
    SteerQueued(TurnIdOutput),
    FollowUpQueued,
    QueuedMessageCancelled,
    CancelAccepted(CancelAcceptedOutput),
    CommandOutput,
    NoChange,
}

impl CommandOutcomeOutput {
    fn from_semantic(value: &CommandOutcome) -> Self {
        match value {
            CommandOutcome::AgentCreated {
                agent_id,
                definition_revision,
                metadata_revision,
            } => Self::AgentCreated(AgentCreatedOutput {
                agent_id: *agent_id,
                definition_revision: *definition_revision,
                metadata_revision: *metadata_revision,
            }),
            CommandOutcome::AgentStatusChanged { status } => {
                Self::AgentStatusChanged(AgentStatusChangedOutput {
                    status: AgentStatusOutput::from_semantic(*status),
                })
            }
            CommandOutcome::AgentDeleted => Self::AgentDeleted,
            CommandOutcome::SessionDefinitionUpdated {
                definition_revision,
            } => Self::SessionDefinitionUpdated(SessionDefinitionUpdatedOutput {
                definition_revision: *definition_revision,
            }),
            CommandOutcome::SessionForked { session_id, source } => {
                Self::SessionForked(SessionForkedOutput {
                    session_id: *session_id,
                    source: ForkSourceKindOutput::from_semantic(*source),
                })
            }
            CommandOutcome::SessionArchived => Self::SessionArchived,
            CommandOutcome::SessionUnarchived => Self::SessionUnarchived,
            CommandOutcome::SessionDeleted => Self::SessionDeleted,
            CommandOutcome::InteractionResolved => Self::InteractionResolved,
            CommandOutcome::TurnStarted { turn_id } => {
                Self::TurnStarted(TurnIdOutput { turn_id: *turn_id })
            }
            CommandOutcome::SubmitCancelled => Self::SubmitCancelled,
            CommandOutcome::SteerQueued { turn_id } => {
                Self::SteerQueued(TurnIdOutput { turn_id: *turn_id })
            }
            CommandOutcome::FollowUpQueued => Self::FollowUpQueued,
            CommandOutcome::QueuedMessageCancelled => Self::QueuedMessageCancelled,
            CommandOutcome::CancelAccepted {
                target,
                cancel_epoch,
            } => Self::CancelAccepted(CancelAcceptedOutput {
                target: PublicCancelTargetOutput::from_semantic(*target),
                cancel_epoch: super::CanonicalU64::new(*cancel_epoch),
            }),
            CommandOutcome::CommandOutput => Self::CommandOutput,
            CommandOutcome::NoChange => Self::NoChange,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentCreatedOutput {
    agent_id: AgentId,
    definition_revision: AgentRevision,
    metadata_revision: AgentMetadataRevision,
}

#[derive(Serialize)]
struct AgentStatusChangedOutput {
    status: AgentStatusOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDefinitionUpdatedOutput {
    definition_revision: SessionDefinitionRevision,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionForkedOutput {
    session_id: SessionId,
    source: ForkSourceKindOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ForkSourceKindOutput {
    LiveSnapshot,
    RecordedHistory,
}

impl ForkSourceKindOutput {
    const fn from_semantic(value: ForkSourceKind) -> Self {
        match value {
            ForkSourceKind::LiveSnapshot => Self::LiveSnapshot,
            ForkSourceKind::RecordedHistory => Self::RecordedHistory,
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
struct CancelAcceptedOutput {
    target: PublicCancelTargetOutput,
    cancel_epoch: super::CanonicalU64,
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
#[serde(rename_all = "camelCase")]
struct QueryErrorOutput<'a> {
    code: QueryErrorCodeOutput,
    message: &'a str,
    retry: RetryAdviceOutput,
    subject: Option<PublicSubjectOutput<'a>>,
}

impl<'a> QueryErrorOutput<'a> {
    fn from_semantic(value: &'a QueryError) -> Self {
        Self {
            code: QueryErrorCodeOutput::from_semantic(value.code()),
            message: value.message(),
            retry: RetryAdviceOutput::from_semantic(value.retry()),
            subject: value.subject().map(PublicSubjectOutput::from_semantic),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum QueryErrorCodeOutput {
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

impl QueryErrorCodeOutput {
    const fn from_semantic(value: QueryErrorCode) -> Self {
        match value {
            QueryErrorCode::InvalidArgument => Self::InvalidArgument,
            QueryErrorCode::NotFound => Self::NotFound,
            QueryErrorCode::SessionNotLoaded => Self::SessionNotLoaded,
            QueryErrorCode::StaleCursor => Self::StaleCursor,
            QueryErrorCode::ResultTooLarge => Self::ResultTooLarge,
            QueryErrorCode::Unavailable => Self::Unavailable,
            QueryErrorCode::DurableStateCorrupt => Self::DurableStateCorrupt,
            QueryErrorCode::DurableStateTooLarge => Self::DurableStateTooLarge,
            QueryErrorCode::RuntimeClosing => Self::RuntimeClosing,
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
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RetryAdviceOutput {
    DoNotRetry,
    RefreshAndRetry,
    RetryWithBackoff(RetryWithBackoffOutput),
    UserActionRequired,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RetryWithBackoffOutput {
    retry_after: Option<Duration>,
}

impl RetryAdviceOutput {
    const fn from_semantic(value: RetryAdvice) -> Self {
        match value {
            RetryAdvice::DoNotRetry => Self::DoNotRetry,
            RetryAdvice::RefreshAndRetry => Self::RefreshAndRetry,
            RetryAdvice::RetryWithBackoff { retry_after } => {
                Self::RetryWithBackoff(RetryWithBackoffOutput { retry_after })
            }
            RetryAdvice::UserActionRequired => Self::UserActionRequired,
        }
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
    Agent(AgentQueryInput),
    Session(SessionQueryInput),
}

impl RuntimeQueryInput {
    fn into_semantic(self) -> RuntimeQuery {
        match self {
            Self::Runtime(RuntimeReadQueryInput::GetCapabilities) => {
                RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities)
            }
            Self::Agent(AgentQueryInput::ListAgents(value)) => {
                RuntimeQuery::Agent(AgentQuery::ListAgents {
                    page: value.page.into_semantic(),
                    include_deleted: value.include_deleted,
                })
            }
            Self::Session(SessionQueryInput::ListSessions(value)) => {
                RuntimeQuery::Session(SessionQuery::ListSessions {
                    page: value.page.into_semantic(),
                    include_archived: value.include_archived,
                })
            }
            Self::Session(SessionQueryInput::GetSessionForkProvenance(value)) => {
                RuntimeQuery::Session(SessionQuery::GetSessionForkProvenance {
                    session_id: value.session_id,
                })
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeReadQueryInput {
    GetCapabilities,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum AgentQueryInput {
    ListAgents(ListAgentsQueryInput),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListAgentsQueryInput {
    page: PageRequestInput,
    include_deleted: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionQueryInput {
    ListSessions(ListSessionsQueryInput),
    GetSessionForkProvenance(SessionIdInput),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListSessionsQueryInput {
    page: PageRequestInput,
    include_archived: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageRequestInput {
    cursor: Option<PageCursor>,
    limit: NonZeroU32,
}

impl PageRequestInput {
    const fn into_semantic(self) -> PageRequest {
        PageRequest::new(self.cursor, self.limit)
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeQueryOutput {
    Runtime(RuntimeReadQueryOutput),
    Agent(AgentQueryOutput),
    Session(SessionQueryOutput),
}

impl RuntimeQueryOutput {
    fn from_semantic(value: &RuntimeQuery) -> Self {
        match value {
            RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities) => {
                Self::Runtime(RuntimeReadQueryOutput::GetCapabilities)
            }
            RuntimeQuery::Agent(AgentQuery::ListAgents {
                page,
                include_deleted,
            }) => Self::Agent(AgentQueryOutput::ListAgents(ListAgentsQueryOutput {
                page: PageRequestOutput::from_semantic(*page),
                include_deleted: *include_deleted,
            })),
            RuntimeQuery::Session(SessionQuery::ListSessions {
                page,
                include_archived,
            }) => Self::Session(SessionQueryOutput::ListSessions(ListSessionsQueryOutput {
                page: PageRequestOutput::from_semantic(*page),
                include_archived: *include_archived,
            })),
            RuntimeQuery::Session(SessionQuery::GetSessionForkProvenance { session_id }) => {
                Self::Session(SessionQueryOutput::GetSessionForkProvenance(
                    SessionIdOutput {
                        session_id: *session_id,
                    },
                ))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeReadQueryOutput {
    GetCapabilities,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum AgentQueryOutput {
    ListAgents(ListAgentsQueryOutput),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListAgentsQueryOutput {
    page: PageRequestOutput,
    include_deleted: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionQueryOutput {
    ListSessions(ListSessionsQueryOutput),
    GetSessionForkProvenance(SessionIdOutput),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListSessionsQueryOutput {
    page: PageRequestOutput,
    include_archived: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PageRequestOutput {
    cursor: Option<PageCursor>,
    limit: NonZeroU32,
}

impl PageRequestOutput {
    const fn from_semantic(value: PageRequest) -> Self {
        Self {
            cursor: value.cursor(),
            limit: value.limit(),
        }
    }
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
    Agent(AgentQueryResultInput),
    Session(SessionQueryResultInput),
}

impl QueryResultInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<QueryResult, TypedJsonError> {
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
            Self::Agent(AgentQueryResultInput::Agents(page)) => {
                let items = page
                    .items
                    .into_iter()
                    .map(|item| item.into_semantic(limits))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(QueryResult::Agent(AgentQueryResult::Agents(Page::new(
                    items,
                    page.next_cursor,
                ))))
            }
            Self::Session(SessionQueryResultInput::Sessions(page)) => {
                let items = page
                    .items
                    .into_iter()
                    .map(|item| item.into_semantic(limits))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(QueryResult::Session(SessionQueryResult::Sessions(
                    Page::new(items, page.next_cursor),
                )))
            }
            Self::Session(SessionQueryResultInput::ForkProvenance(provenance)) => {
                let provenance = provenance.map(SessionForkProvenanceInput::into_semantic);
                Ok(QueryResult::Session(SessionQueryResult::ForkProvenance(
                    provenance,
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

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum AgentQueryResultInput {
    Agents(AgentPageInput),
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionQueryResultInput {
    Sessions(SessionPageInput),
    ForkProvenance(Option<SessionForkProvenanceInput>),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionForkProvenanceInput {
    source_session_id: SessionId,
    source: ForkSourceKindInput,
    anchor: ForkAnchorInput,
}

impl SessionForkProvenanceInput {
    fn into_semantic(self) -> SessionForkProvenanceView {
        SessionForkProvenanceView::new(
            self.source_session_id,
            self.source.into_semantic(),
            self.anchor.into_semantic(),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionPageInput {
    items: Vec<SessionSummaryInput>,
    next_cursor: Option<PageCursor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentPageInput {
    items: Vec<AgentSummaryInput>,
    next_cursor: Option<PageCursor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSummaryInput {
    agent_id: AgentId,
    definition_revision: AgentRevision,
    metadata: AgentMetadataInput,
    status: AgentStatusInput,
    created_at: Timestamp,
}

impl AgentSummaryInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<AgentSummary, TypedJsonError> {
        Ok(AgentSummary::new(
            self.agent_id,
            self.definition_revision,
            self.metadata.into_semantic(limits)?,
            self.status.into_semantic(),
            self.created_at,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentMetadataInput {
    revision: AgentMetadataRevision,
    name: String,
    description: Option<String>,
    updated_at: Timestamp,
}

impl AgentMetadataInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<AgentMetadataView, TypedJsonError> {
        AgentMetadataView::new_with_limits(
            self.revision,
            self.name,
            self.description,
            self.updated_at,
            limits,
        )
        .map_err(|_| invalid_scalar())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentStatusInput {
    Enabled,
    Disabled,
    Deleted,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionSummaryInput {
    session_id: SessionId,
    definition_revision: SessionDefinitionRevision,
    metadata: SessionMetadataInput,
    lifecycle: SessionLifecycleInput,
    forked: bool,
    created_at: Timestamp,
}

impl SessionSummaryInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<SessionSummary, TypedJsonError> {
        Ok(SessionSummary::new(
            self.session_id,
            self.definition_revision,
            self.metadata.into_semantic(limits)?,
            self.lifecycle.into_semantic(),
            self.forked,
            self.created_at,
        ))
    }
}

impl AgentStatusInput {
    const fn into_semantic(self) -> AgentStatus {
        match self {
            Self::Enabled => AgentStatus::Enabled,
            Self::Disabled => AgentStatus::Disabled,
            Self::Deleted => AgentStatus::Deleted,
        }
    }
}

#[derive(Serialize)]
struct QueryResponseOutput<'a> {
    data: QueryResultOutput<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum QueryResultOutput<'a> {
    Runtime(RuntimeQueryResultOutput<'a>),
    Agent(AgentQueryResultOutput<'a>),
    Session(SessionQueryResultOutput<'a>),
}

impl<'a> QueryResultOutput<'a> {
    fn from_semantic(value: &'a QueryResult) -> Self {
        match value {
            QueryResult::Runtime(RuntimeQueryResult::Capabilities(capabilities)) => {
                Self::Runtime(RuntimeQueryResultOutput::Capabilities(capabilities))
            }
            QueryResult::Agent(AgentQueryResult::Agents(page)) => Self::Agent(
                AgentQueryResultOutput::Agents(AgentPageOutput::from_semantic(page)),
            ),
            QueryResult::Session(SessionQueryResult::Sessions(page)) => Self::Session(
                SessionQueryResultOutput::Sessions(SessionPageOutput::from_semantic(page)),
            ),
            QueryResult::Session(SessionQueryResult::ForkProvenance(provenance)) => {
                Self::Session(SessionQueryResultOutput::ForkProvenance(
                    provenance
                        .as_ref()
                        .map(SessionForkProvenanceOutput::from_semantic),
                ))
            }
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeQueryResultOutput<'a> {
    Capabilities(&'a RuntimeCapabilities),
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum AgentQueryResultOutput<'a> {
    Agents(AgentPageOutput<'a>),
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum SessionQueryResultOutput<'a> {
    Sessions(SessionPageOutput<'a>),
    ForkProvenance(Option<SessionForkProvenanceOutput>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionForkProvenanceOutput {
    source_session_id: SessionId,
    source: ForkSourceKindOutput,
    anchor: ForkAnchorOutput,
}

impl SessionForkProvenanceOutput {
    fn from_semantic(value: &SessionForkProvenanceView) -> Self {
        Self {
            source_session_id: value.source_session_id(),
            source: ForkSourceKindOutput::from_semantic(value.source()),
            anchor: ForkAnchorOutput::from_semantic(value.anchor()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentPageOutput<'a> {
    items: Vec<AgentSummaryOutput<'a>>,
    next_cursor: Option<PageCursor>,
}

impl<'a> AgentPageOutput<'a> {
    fn from_semantic(value: &'a Page<AgentSummary>) -> Self {
        Self {
            items: value
                .items()
                .iter()
                .map(AgentSummaryOutput::from_semantic)
                .collect(),
            next_cursor: value.next_cursor(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSummaryOutput<'a> {
    agent_id: AgentId,
    definition_revision: AgentRevision,
    metadata: AgentMetadataOutput<'a>,
    status: AgentStatusOutput,
    created_at: Timestamp,
}

impl<'a> AgentSummaryOutput<'a> {
    fn from_semantic(value: &'a AgentSummary) -> Self {
        Self {
            agent_id: value.agent_id(),
            definition_revision: value.definition_revision(),
            metadata: AgentMetadataOutput::from_semantic(value.metadata()),
            status: AgentStatusOutput::from_semantic(value.status()),
            created_at: value.created_at(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentMetadataOutput<'a> {
    revision: AgentMetadataRevision,
    name: &'a str,
    description: Option<&'a str>,
    updated_at: Timestamp,
}

impl<'a> AgentMetadataOutput<'a> {
    fn from_semantic(value: &'a AgentMetadataView) -> Self {
        Self {
            revision: value.revision(),
            name: value.name(),
            description: value.description(),
            updated_at: value.updated_at(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentStatusOutput {
    Enabled,
    Disabled,
    Deleted,
}

impl AgentStatusOutput {
    const fn from_semantic(value: AgentStatus) -> Self {
        match value {
            AgentStatus::Enabled => Self::Enabled,
            AgentStatus::Disabled => Self::Disabled,
            AgentStatus::Deleted => Self::Deleted,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionPageOutput<'a> {
    items: Vec<SessionSummaryOutput<'a>>,
    next_cursor: Option<PageCursor>,
}

impl<'a> SessionPageOutput<'a> {
    fn from_semantic(value: &'a Page<SessionSummary>) -> Self {
        Self {
            items: value
                .items()
                .iter()
                .map(SessionSummaryOutput::from_semantic)
                .collect(),
            next_cursor: value.next_cursor(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionSummaryOutput<'a> {
    session_id: SessionId,
    definition_revision: SessionDefinitionRevision,
    metadata: SessionMetadataOutput<'a>,
    lifecycle: SessionLifecycleOutput,
    forked: bool,
    created_at: Timestamp,
}

impl<'a> SessionSummaryOutput<'a> {
    fn from_semantic(value: &'a SessionSummary) -> Self {
        Self {
            session_id: value.session_id(),
            definition_revision: value.definition_revision(),
            metadata: SessionMetadataOutput::from_semantic(value.metadata()),
            lifecycle: SessionLifecycleOutput::from_semantic(value.lifecycle()),
            forked: value.forked(),
            created_at: value.created_at(),
        }
    }
}

fn validate_query_response_semantic_limits(
    response: &QueryResponse,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match response.data() {
        QueryResult::Runtime(RuntimeQueryResult::Capabilities(_))
        | QueryResult::Session(SessionQueryResult::ForkProvenance(_)) => Ok(()),
        QueryResult::Agent(AgentQueryResult::Agents(page)) => {
            if page.items().len() > usize::from(limits.paging.max_page_size) {
                return Err(invalid_scalar());
            }
            validate_semantic_page_cursor(page.next_cursor(), limits)?;
            for agent in page.items() {
                let metadata = agent.metadata();
                AgentMetadataView::new_with_limits(
                    metadata.revision(),
                    metadata.name(),
                    metadata.description(),
                    metadata.updated_at(),
                    limits,
                )
                .map_err(|_| invalid_scalar())?;
            }
            Ok(())
        }
        QueryResult::Session(SessionQueryResult::Sessions(page)) => {
            if page.items().len() > usize::from(limits.paging.max_page_size) {
                return Err(invalid_scalar());
            }
            validate_semantic_page_cursor(page.next_cursor(), limits)?;
            for session in page.items() {
                validate_session_summary_semantic_limits(session, limits)?;
            }
            Ok(())
        }
    }
}

fn validate_session_summary_semantic_limits(
    session: &SessionSummary,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let metadata = session.metadata();
    SessionMetadataView::new_with_limits(
        metadata.revision(),
        metadata.name(),
        metadata.description(),
        metadata.updated_at(),
        limits,
    )
    .map(|_| ())
    .map_err(|_| invalid_scalar())
}

fn validate_agent_summary_semantic_limits(
    agent: &AgentSummary,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let metadata = agent.metadata();
    AgentMetadataView::new_with_limits(
        metadata.revision(),
        metadata.name(),
        metadata.description(),
        metadata.updated_at(),
        limits,
    )
    .map(|_| ())
    .map_err(|_| invalid_scalar())
}

fn validate_semantic_page_cursor(
    cursor: Option<PageCursor>,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    if cursor.is_some_and(|cursor| {
        cursor.to_string().len() > usize::from(limits.paging.max_page_cursor_bytes)
    }) {
        Err(invalid_scalar())
    } else {
        Ok(())
    }
}

fn validate_runtime_query_semantic_limits(
    query: &RuntimeQuery,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let page = match query {
        RuntimeQuery::Agent(AgentQuery::ListAgents { page, .. })
        | RuntimeQuery::Session(SessionQuery::ListSessions { page, .. }) => Some(*page),
        RuntimeQuery::Runtime(RuntimeReadQuery::GetCapabilities)
        | RuntimeQuery::Session(SessionQuery::GetSessionForkProvenance { .. }) => None,
    };
    if page.and_then(PageRequest::cursor).is_some_and(|cursor| {
        cursor.to_string().len() > usize::from(limits.paging.max_page_cursor_bytes)
    }) {
        return Err(invalid_scalar());
    }
    Ok(())
}

fn validate_command_semantic_limits(
    command: &RuntimeCommand,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match command {
        RuntimeCommand::Agent(AgentCommand::Create {
            definition,
            metadata,
        }) => {
            if definition.prompts().enabled().len()
                > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX)
            {
                return Err(invalid_scalar());
            }
            NewAgentMetadata::new_with_limits(metadata.name(), metadata.description(), limits)
                .map_err(|_| invalid_scalar())?;
        }
        RuntimeCommand::Turn(TurnCommand::Submit { intent, .. })
        | RuntimeCommand::Turn(TurnCommand::Steer { intent, .. })
        | RuntimeCommand::Turn(TurnCommand::FollowUp { intent, .. }) => {
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
        "cancel_accepted" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_public_cancel_target(required(object, "target")?)?;
            validate_canonical_u64(required(object, "cancelEpoch")?)?;
            Ok(())
        }
        "agent_created" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_id::<AgentId>(required(object, "agentId")?)?;
            validate_revision::<AgentRevision>(required(object, "definitionRevision")?)?;
            validate_revision::<AgentMetadataRevision>(required(object, "metadataRevision")?)
        }
        "agent_status_changed" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_agent_status_output(required(object, "status")?)
        }
        "agent_definition_updated"
        | "agent_metadata_updated"
        | "session_created"
        | "session_metadata_updated" => pending_output_object(data),
        "session_definition_updated" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_revision::<SessionDefinitionRevision>(required(object, "definitionRevision")?)
        }
        "session_forked" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_id::<SessionId>(required(object, "sessionId")?)?;
            match required(object, "source")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
            {
                "live_snapshot" | "recorded_history" => Ok(()),
                _ => Err(unknown_output_variant()),
            }
        }
        "steer_queued" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_id::<TurnId>(required(object, "turnId")?)
        }
        "session_archived" | "session_unarchived" | "session_deleted" | "no_change" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "agent_deleted" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "session_loaded" | "session_unloaded" | "runtime_reloaded" | "workspace_reloaded" => {
            pending_output_unit(data)
        }
        "interaction_resolved" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "submit_cancelled" | "follow_up_queued" | "queued_message_cancelled" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_public_cancel_target(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    let data = required(object, "data")?;
    match kind {
        "submit" => validate_id::<CommandId>(data),
        "turn" => validate_id::<TurnId>(data),
        _ => Err(unknown_output_variant()),
    }
}

fn validate_command_error(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let code = validate_command_error_code(required(object, "code")?)?;
    let message = required(object, "message")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    validate_command_error_message(message, limits.text.max_diagnostic_message_bytes as usize)
        .map_err(|_| invalid_scalar())?;
    let retry = validate_retry_advice(required(object, "retry")?)?;
    validate_command_error_contract(code, retry).map_err(|_| invalid_scalar())?;
    if let Some(subject) = object.get("subject") {
        if !matches!(subject, JsonNode::Null) {
            validate_public_subject(subject)?;
        }
    }
    Ok(())
}

fn validate_query_error_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    match required(object, "code")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "invalid_argument"
        | "not_found"
        | "session_not_loaded"
        | "stale_cursor"
        | "result_too_large"
        | "unavailable"
        | "durable_state_corrupt"
        | "durable_state_too_large"
        | "runtime_closing" => {}
        _ => return Err(unknown_output_variant()),
    }
    let message = required(object, "message")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    validate_command_error_message(message, limits.text.max_diagnostic_message_bytes as usize)
        .map_err(|_| invalid_scalar())?;
    validate_retry_advice(required(object, "retry")?)?;
    if let Some(subject) = object.get("subject") {
        if !matches!(subject, JsonNode::Null) {
            validate_public_subject(subject)?;
        }
    }
    Ok(())
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

fn validate_retry_advice(node: &JsonNode) -> Result<RetryAdvice, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    let data = object.get("data");
    let retry = match kind {
        "do_not_retry" => RetryAdvice::DoNotRetry,
        "refresh_and_retry" => RetryAdvice::RefreshAndRetry,
        "user_action_required" => RetryAdvice::UserActionRequired,
        "retry_with_backoff" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            let retry_after = match required(object, "retryAfter")? {
                JsonNode::Null => None,
                value => {
                    let raw = value.as_number().ok_or_else(typed_wrong_json_type)?.raw();
                    let milliseconds = raw.parse::<u64>().map_err(|_| invalid_scalar())?;
                    let milliseconds =
                        u32::try_from(milliseconds).map_err(|_| duration_out_of_range())?;
                    Some(Duration::new(milliseconds).map_err(|_| duration_out_of_range())?)
                }
            };
            return Ok(RetryAdvice::RetryWithBackoff { retry_after });
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

fn validate_event_frame_semantic_limits(
    frame: &EventFrame,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match frame {
        EventFrame::Snapshot(snapshot) => validate_snapshot_semantic_limits(snapshot, limits),
        EventFrame::State(event) => {
            if !event.has_valid_contract() {
                return Err(invalid_scalar());
            }
            match event.msg() {
                StateEventMsg::Runtime {
                    snapshot, detail, ..
                } => {
                    validate_runtime_snapshot_semantic_limits(snapshot, limits)?;
                    match detail {
                        Some(RuntimeEventDetail::AgentChanged { agent }) => {
                            validate_agent_summary_semantic_limits(agent, limits)?;
                        }
                        Some(RuntimeEventDetail::SessionChanged { session }) => {
                            validate_session_summary_semantic_limits(session, limits)?;
                        }
                        None => {}
                    }
                    Ok(())
                }
                StateEventMsg::Session { snapshot, .. } => {
                    validate_session_snapshot_semantic_limits(snapshot, limits)
                }
            }
        }
        EventFrame::Progress(event) => {
            if event.has_valid_contract() {
                Ok(())
            } else {
                Err(invalid_scalar())
            }
        }
        EventFrame::Closed(_) => Ok(()),
    }
}

fn validate_event_frame_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "snapshot" => {
            validate_snapshot_response_shape(data.ok_or_else(missing_required_field)?, limits)
        }
        "state" => validate_state_event_shape(data.ok_or_else(missing_required_field)?, limits),
        "progress" => {
            validate_progress_event_shape(data.ok_or_else(missing_required_field)?)?;
            Ok(())
        }
        "closed" => {
            let reason = data
                .ok_or_else(missing_required_field)?
                .as_str()
                .ok_or_else(selected_wrong_json_type)?;
            if !matches!(
                reason,
                "backpressure" | "runtime_closing" | "publisher_restarted"
            ) {
                return Err(unknown_output_variant());
            }
            Ok(())
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_snapshot_response_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "runtime" => {
            validate_runtime_snapshot_shape(data.ok_or_else(missing_required_field)?, limits)
        }
        "session" => {
            validate_session_snapshot_shape(data.ok_or_else(missing_required_field)?, limits)
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_runtime_snapshot_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let runtime = required(object, "runtime")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    match required(runtime, "status")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "running" | "closing" => {}
        _ => return Err(unknown_output_variant()),
    }
    let loaded = required(object, "loadedSessions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if loaded.len() > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX) {
        return Err(invalid_scalar());
    }
    let diagnostics_pending =
        validate_snapshot_diagnostics_shape(required(object, "diagnostics")?)?;
    if diagnostics_pending {
        return Err(TypedJsonError::PendingPublicTarget);
    }
    let mut session_ids = BTreeSet::new();
    for session in loaded {
        let session = session.as_object().ok_or_else(selected_wrong_json_type)?;
        let session_id = required(session, "sessionId")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?;
        session_id
            .parse::<SessionId>()
            .map_err(|_| noncanonical_id())?;
        if !session_ids.insert(session_id) {
            return Err(duplicate_value());
        }
        let ready = validate_readiness(required(session, "readiness")?)?;
        let execution = validate_execution(required(session, "execution")?)?;
        if !ready && execution != "idle" {
            return Err(invalid_scalar());
        }
        validate_recording(required(session, "recording")?)?;
    }
    Ok(())
}

fn validate_session_snapshot_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let session_id = parse_id(required(object, "sessionId")?)?;

    let lifecycle = required(object, "lifecycle")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    match lifecycle {
        "open" => {}
        "archived" | "deleted" => return Err(invalid_scalar()),
        _ => return Err(unknown_output_variant()),
    }

    required(object, "metadata")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    required(object, "definition")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;

    let load_state = required(object, "loadState")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    match load_state {
        "loaded" | "unloading" => {}
        _ => return Err(unknown_output_variant()),
    }
    validate_readiness(required(object, "readiness")?)?;
    let _execution = validate_execution(required(object, "execution")?)?;

    if let Some(current_turn) = object.get("currentTurn") {
        if !matches!(current_turn, JsonNode::Null) {
            current_turn
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
        }
    }
    let active_items = required(object, "activeItems")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if active_items.len() > usize::from(limits.observation.max_active_items) {
        return Err(invalid_scalar());
    }
    let pending_interactions = required(object, "pendingInteractions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if pending_interactions.len() > usize::from(limits.observation.max_pending_interactions) {
        return Err(invalid_scalar());
    }

    validate_session_queues_shape(required(object, "queues")?)?;
    validate_recording(required(object, "recording")?)?;
    required(object, "diagnostics")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if let Some(usage) = object.get("usage") {
        if !matches!(usage, JsonNode::Null) {
            validate_session_usage_shape(usage)?;
        }
    }

    // The loaded-ready-idle candidate remains owned by the semantic model.
    validate_session_metadata(required(object, "metadata")?, limits)?;
    let definition_session_id =
        validate_session_definition_summary(required(object, "definition")?, limits)?;
    if definition_session_id != session_id {
        return Err(invalid_scalar());
    }
    Ok(())
}

fn validate_session_metadata(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let revision = required(object, "revision")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<SessionMetadataRevision>()
        .map_err(|_| invalid_scalar())?;
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
    let updated_at = required(object, "updatedAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    SessionMetadataView::new_with_limits(revision, name, description, updated_at, limits)
        .map_err(|_| invalid_scalar())?;
    Ok(())
}

fn validate_session_definition_summary(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<SessionId, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let session_id = parse_id(required(object, "sessionId")?)?;
    validate_revision::<SessionDefinitionRevision>(required(object, "revision")?)?;
    let agent = required(object, "agent")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    validate_id::<AgentId>(required(agent, "agentId")?)?;
    validate_revision::<AgentRevision>(required(agent, "revision")?)?;
    validate_workspace_summary(required(object, "workspace")?, limits)?;
    validate_session_model_config_shape(required(object, "model")?)?;
    let prompt_ids = required(object, "promptIds")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if prompt_ids.len() > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX) {
        return Err(invalid_scalar());
    }
    let mut unique = BTreeSet::new();
    for prompt in prompt_ids {
        let prompt = prompt.as_str().ok_or_else(typed_wrong_json_type)?;
        prompt.parse::<PromptId>().map_err(|_| invalid_scalar())?;
        if !unique.insert(prompt) {
            return Err(duplicate_value());
        }
    }
    required(object, "createdAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    Ok(session_id)
}

fn validate_workspace_summary(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let roots = required(object, "roots")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if roots.is_empty() || roots.len() > usize::from(limits.workspace.max_workspace_roots) {
        return Err(invalid_scalar());
    }
    let mut keys = BTreeSet::new();
    for root in roots {
        let root = root.as_object().ok_or_else(selected_wrong_json_type)?;
        let key = required(root, "key")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?;
        key.parse::<WorkspaceRootKey>()
            .map_err(|_| invalid_scalar())?;
        if !keys.insert(key) {
            return Err(duplicate_value());
        }
        match required(root, "requestedAccess")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?
        {
            "read_only" | "read_write" => {}
            _ => return Err(unknown_output_variant()),
        }
        for field in ["promptSource", "skillSource"] {
            if !matches!(required(root, field)?, JsonNode::Bool(_)) {
                return Err(typed_wrong_json_type());
            }
        }
    }
    let cwd_root = required(object, "cwdRoot")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    cwd_root
        .parse::<WorkspaceRootKey>()
        .map_err(|_| invalid_scalar())?;
    if !keys.contains(cwd_root) {
        return Err(invalid_scalar());
    }
    let relative = required(object, "cwdRelative")?
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
    Ok(())
}

fn validate_session_model_config_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let selection = required(object, "selection")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
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
        _ => return Err(unknown_output_variant()),
    }
    match object.get("maxOutputTokens") {
        None | Some(JsonNode::Null) => Ok(()),
        Some(node) => validate_nonzero_u32(node),
    }
}

fn validate_readiness(node: &JsonNode) -> Result<bool, TypedJsonError> {
    let mut is_ready = false;
    validate_adjacent_output(node, |kind, data| {
        match kind {
            "preparing" | "ready" => {
                if data.is_some() {
                    return Err(selected_wrong_json_type());
                }
            }
            "unavailable" => {
                let reason = data
                    .ok_or_else(missing_required_field)?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?;
                if !matches!(
                    reason,
                    "agent_unavailable"
                        | "workspace_unavailable"
                        | "model_unavailable"
                        | "prompt_unavailable"
                        | "durable_state_corrupt"
                        | "durable_state_too_large"
                        | "runtime_dependency_unavailable"
                ) {
                    return Err(unknown_output_variant());
                }
            }
            _ => return Err(unknown_output_variant()),
        }
        is_ready = kind == "ready";
        Ok(())
    })?;
    Ok(is_ready)
}

fn validate_execution(node: &JsonNode) -> Result<&str, TypedJsonError> {
    let value = node.as_str().ok_or_else(typed_wrong_json_type)?;
    if matches!(value, "idle" | "starting" | "running" | "finishing") {
        Ok(value)
    } else {
        Err(unknown_output_variant())
    }
}

fn validate_recording(node: &JsonNode) -> Result<bool, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    match required(object, "state")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "healthy" => Ok(true),
        "degraded" => Ok(false),
        _ => Err(unknown_output_variant()),
    }
}

fn validate_session_queues_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["submitAdmissions", "steers", "followUps", "acceptingInput"],
    )?;
    let submit = required(object, "submitAdmissions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    let steers = required(object, "steers")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    let follow_ups = required(object, "followUps")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    let accepting = match required(object, "acceptingInput")? {
        JsonNode::Bool(value) => *value,
        _ => return Err(typed_wrong_json_type()),
    };
    let _ = accepting;
    for entry in submit {
        let entry = entry.as_object().ok_or_else(selected_wrong_json_type)?;
        reject_unknown_fields(entry.keys().map(AsRef::as_ref), &["commandId", "state"])?;
        validate_id::<CommandId>(required(entry, "commandId")?)?;
        match required(entry, "state")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?
        {
            "queued" | "starting" => {}
            _ => return Err(unknown_output_variant()),
        }
    }
    for entry in steers {
        let entry = entry.as_object().ok_or_else(selected_wrong_json_type)?;
        reject_unknown_fields(
            entry.keys().map(AsRef::as_ref),
            &["commandId", "expectedTurnId"],
        )?;
        validate_id::<CommandId>(required(entry, "commandId")?)?;
        validate_id::<TurnId>(required(entry, "expectedTurnId")?)?;
    }
    for entry in follow_ups {
        let entry = entry.as_object().ok_or_else(selected_wrong_json_type)?;
        reject_unknown_fields(entry.keys().map(AsRef::as_ref), &["commandId"])?;
        validate_id::<CommandId>(required(entry, "commandId")?)?;
    }
    Ok(())
}

fn validate_session_usage_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    validate_canonical_u64(required(object, "modelCalls")?)?;
    validate_canonical_u64(required(object, "compactionCalls")?)?;

    for field in [
        "inputTokens",
        "outputTokens",
        "reasoningTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
    ] {
        if let Some(value) = object.get(field) {
            if !matches!(value, JsonNode::Null) {
                validate_canonical_u64(value)?;
            }
        }
    }
    required(object, "reportedCosts")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    Ok(())
}

fn validate_canonical_u64(node: &JsonNode) -> Result<u64, TypedJsonError> {
    let value = node
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<super::CanonicalU64>()
        .map_err(|_| invalid_scalar())?;
    Ok(value.get())
}

fn validate_snapshot_diagnostics_shape(node: &JsonNode) -> Result<bool, TypedJsonError> {
    let diagnostics = node.as_array().ok_or_else(selected_wrong_json_type)?;
    Ok(!diagnostics.is_empty())
}

fn validate_state_event_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    required(object, "timestamp")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    if let Some(command_id) = object.get("commandId") {
        if !matches!(command_id, JsonNode::Null) {
            validate_id::<CommandId>(command_id)?;
        }
    }
    let route = validate_event_route(required(object, "route")?)?;
    let pending = validate_adjacent_output(required(object, "msg")?, |kind, data| match kind {
        "runtime" => {
            validate_runtime_state_msg(data.ok_or_else(missing_required_field)?, limits, route)
        }
        "session" => {
            validate_session_state_msg(data.ok_or_else(missing_required_field)?, limits, route)
        }
        _ => Err(unknown_output_variant()),
    })?;
    if pending {
        Err(TypedJsonError::PendingPublicTarget)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventRouteFamily {
    Runtime,
    Agent,
    Session,
    Turn,
    Item,
    Interaction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventRouteFacts {
    family: EventRouteFamily,
    agent_id: Option<AgentId>,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
}

fn validate_event_route(node: &JsonNode) -> Result<EventRouteFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    let data = object.get("data");
    let mut facts = EventRouteFacts {
        family: EventRouteFamily::Runtime,
        agent_id: None,
        session_id: None,
        turn_id: None,
    };
    match kind {
        "runtime" => {
            if data.is_some() {
                return Err(selected_wrong_json_type());
            }
        }
        "agent" => {
            facts.family = EventRouteFamily::Agent;
            let data = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            facts.agent_id = Some(parse_id::<AgentId>(required(data, "agentId")?)?);
        }
        "session" => {
            facts.family = EventRouteFamily::Session;
            let data = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            facts.session_id = Some(parse_id(required(data, "sessionId")?)?);
        }
        "turn" | "item" | "interaction" => {
            facts.family = match kind {
                "turn" => EventRouteFamily::Turn,
                "item" => EventRouteFamily::Item,
                "interaction" => EventRouteFamily::Interaction,
                _ => unreachable!(),
            };
            let data = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            facts.session_id = Some(parse_id(required(data, "sessionId")?)?);
            facts.turn_id = Some(parse_id(required(data, "turnId")?)?);
            if matches!(
                facts.family,
                EventRouteFamily::Item | EventRouteFamily::Interaction
            ) {
                parse_id::<ItemId>(required(data, "itemId")?)?;
            }
            if facts.family == EventRouteFamily::Interaction {
                parse_id::<RequestId>(required(data, "requestId")?)?;
            }
        }
        _ => return Err(unknown_output_variant()),
    }
    Ok(facts)
}

fn validate_runtime_state_msg(
    node: &JsonNode,
    limits: ProtocolLimits,
    route: EventRouteFacts,
) -> Result<bool, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "kind")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let snapshot = required(object, "snapshot")?;
    match kind {
        "agent_created" | "agent_status_changed" => {
            if route.family != EventRouteFamily::Agent {
                return Err(invalid_scalar());
            }
            let summary = validate_runtime_agent_changed_detail(
                object.get("detail").ok_or_else(missing_required_field)?,
                limits,
            )?;
            if route.agent_id != Some(summary.agent_id)
                || (kind == "agent_created" && summary.status != AgentStatus::Enabled)
            {
                return Err(invalid_scalar());
            }
            match validate_runtime_snapshot_shape(snapshot, limits) {
                Ok(()) => Ok(false),
                Err(error) if error.is_pending_public_target() => Ok(true),
                Err(error) => Err(error),
            }
        }
        "session_loaded" | "session_unloaded" => {
            if route.family != EventRouteFamily::Session
                || object
                    .get("detail")
                    .is_some_and(|detail| !matches!(detail, JsonNode::Null))
            {
                return Err(invalid_scalar());
            }
            match validate_runtime_snapshot_shape(snapshot, limits) {
                Ok(()) => Ok(false),
                Err(error) if error.is_pending_public_target() => Ok(true),
                Err(error) => Err(error),
            }
        }
        "session_created" | "session_archived" | "session_unarchived" | "session_deleted"
        | "session_forked" => {
            if route.family != EventRouteFamily::Session {
                return Err(invalid_scalar());
            }
            let summary = validate_runtime_session_changed_detail(
                object.get("detail").ok_or_else(missing_required_field)?,
            )?;
            let expected_lifecycle = match kind {
                "session_archived" => SessionLifecycleView::Archived,
                "session_deleted" => SessionLifecycleView::Deleted,
                _ => SessionLifecycleView::Open,
            };
            let fork_contract_matches = match kind {
                "session_created" => !summary.forked,
                "session_forked" => summary.forked,
                _ => true,
            };
            if route.session_id != Some(summary.session_id)
                || summary.lifecycle != expected_lifecycle
                || !fork_contract_matches
            {
                return Err(invalid_scalar());
            }
            match validate_runtime_snapshot_shape(snapshot, limits) {
                Ok(()) => Ok(false),
                Err(error) if error.is_pending_public_target() => Ok(true),
                Err(error) => Err(error),
            }
        }
        "command_catalog_invalidated" => {
            if route.family != EventRouteFamily::Runtime
                || object
                    .get("detail")
                    .is_some_and(|detail| !matches!(detail, JsonNode::Null))
            {
                return Err(invalid_scalar());
            }
            let snapshot_pending = match validate_runtime_snapshot_shape(snapshot, limits) {
                Ok(()) => false,
                Err(error) if error.is_pending_public_target() => true,
                Err(error) => return Err(error),
            };
            Ok(snapshot_pending)
        }
        _ => {
            let expected_route =
                runtime_state_route_family(kind).ok_or_else(unknown_output_variant)?;
            if route.family != expected_route {
                return Err(invalid_scalar());
            }
            validate_runtime_snapshot_outer_shape(snapshot, limits)?;
            validate_future_state_detail(kind, object.get("detail"), true)?;
            Ok(true)
        }
    }
}

fn validate_runtime_agent_changed_detail(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<AgentSummaryFacts, TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "agent_changed" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_agent_summary(required(object, "agent")?, limits)
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_runtime_session_changed_detail(
    node: &JsonNode,
) -> Result<SessionSummaryFacts, TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "session_changed" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_session_summary(required(object, "session")?)
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_runtime_snapshot_outer_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let runtime = required(object, "runtime")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    match required(runtime, "status")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "running" | "closing" => {}
        _ => return Err(unknown_output_variant()),
    }
    let loaded = required(object, "loadedSessions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if loaded.len() > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX) {
        return Err(invalid_scalar());
    }
    required(object, "diagnostics")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    Ok(())
}

fn validate_session_state_msg(
    node: &JsonNode,
    limits: ProtocolLimits,
    route: EventRouteFacts,
) -> Result<bool, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "kind")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let snapshot = required(object, "snapshot")?;
    match kind {
        "turn_completed" | "turn_interrupted" | "turn_failed" => {
            let terminal = validate_turn_terminal_detail(
                object.get("detail").ok_or_else(missing_required_field)?,
            )?;
            let expected = match kind {
                "turn_completed" => TerminalKind::Completed,
                "turn_interrupted" => TerminalKind::Interrupted,
                "turn_failed" => TerminalKind::Failed,
                _ => return Err(TypedJsonError::EncodingInvariant),
            };
            let snapshot_session_id = validate_snapshot_session_id(snapshot)?;
            if terminal.terminal.kind != expected
                || route.family != EventRouteFamily::Turn
                || route.session_id != Some(snapshot_session_id)
                || route.turn_id != Some(terminal.turn_id)
            {
                return Err(invalid_scalar());
            }
            match validate_session_snapshot_shape(snapshot, limits) {
                Ok(()) => Ok(false),
                Err(error) if error.is_pending_public_target() => Ok(true),
                Err(error) => Err(error),
            }
        }
        "session_execution_changed" => {
            if route.family != EventRouteFamily::Session
                || object
                    .get("detail")
                    .is_some_and(|detail| !matches!(detail, JsonNode::Null))
            {
                return Err(invalid_scalar());
            }
            let snapshot_session_id = validate_snapshot_session_id(snapshot)?;
            if route.session_id != Some(snapshot_session_id) {
                return Err(invalid_scalar());
            }
            match validate_session_snapshot_shape(snapshot, limits) {
                Ok(()) => Ok(false),
                Err(error) if error.is_pending_public_target() => Ok(true),
                Err(error) => Err(error),
            }
        }
        _ => {
            let expected_route =
                session_state_route_family(kind).ok_or_else(unknown_output_variant)?;
            if route.family != expected_route {
                return Err(invalid_scalar());
            }
            validate_session_snapshot_outer_shape(snapshot)?;
            validate_future_state_detail(kind, object.get("detail"), false)?;
            Ok(true)
        }
    }
}

fn runtime_state_route_family(kind: &str) -> Option<EventRouteFamily> {
    Some(match kind {
        "agent_created"
        | "agent_definition_updated"
        | "agent_metadata_updated"
        | "agent_status_changed" => EventRouteFamily::Agent,
        "session_created"
        | "session_definition_updated"
        | "session_metadata_updated"
        | "session_archived"
        | "session_unarchived"
        | "session_deleted"
        | "session_forked"
        | "session_loaded"
        | "session_unloaded" => EventRouteFamily::Session,
        "diagnostics_updated" | "shared_resources_reloaded" => EventRouteFamily::Runtime,
        _ => return None,
    })
}

fn session_state_route_family(kind: &str) -> Option<EventRouteFamily> {
    Some(match kind {
        "turn_interrupted" | "turn_started" | "turn_phase_changed" => EventRouteFamily::Turn,
        "item_completed"
        | "item_tool_invocation_started"
        | "item_tool_invocation_completed"
        | "item_tool_invocation_abandoned" => EventRouteFamily::Item,
        "interaction_requested" | "interaction_resolved" => EventRouteFamily::Interaction,
        "session_definition_updated"
        | "session_metadata_updated"
        | "session_readiness_changed"
        | "session_execution_changed"
        | "session_settled"
        | "usage_updated"
        | "diagnostics_updated"
        | "session_workspace_reloaded"
        | "session_recording_changed"
        | "queue_updated" => EventRouteFamily::Session,
        _ => return None,
    })
}

fn validate_snapshot_session_id(node: &JsonNode) -> Result<SessionId, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    parse_id::<SessionId>(required(object, "sessionId")?)
}

fn validate_session_snapshot_outer_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    parse_id::<SessionId>(required(object, "sessionId")?)?;
    match required(object, "lifecycle")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "open" => {}
        "archived" | "deleted" => return Err(invalid_scalar()),
        _ => return Err(unknown_output_variant()),
    }
    required(object, "metadata")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    required(object, "definition")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    match required(object, "loadState")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "loaded" | "unloading" => {}
        _ => return Err(unknown_output_variant()),
    }
    validate_readiness(required(object, "readiness")?)?;
    validate_execution(required(object, "execution")?)?;
    if let Some(current_turn) = object.get("currentTurn") {
        if !matches!(current_turn, JsonNode::Null) {
            current_turn
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
        }
    }
    required(object, "activeItems")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    required(object, "pendingInteractions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    validate_session_queues_shape(required(object, "queues")?)?;
    validate_recording(required(object, "recording")?)?;
    if let Some(usage) = object.get("usage") {
        if !matches!(usage, JsonNode::Null) {
            usage.as_object().ok_or_else(selected_wrong_json_type)?;
        }
    }
    required(object, "diagnostics")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    Ok(())
}

fn validate_future_state_detail(
    kind: &str,
    detail: Option<&JsonNode>,
    runtime: bool,
) -> Result<(), TypedJsonError> {
    let expected_object_kind = if runtime {
        match kind {
            "agent_created"
            | "agent_definition_updated"
            | "agent_metadata_updated"
            | "agent_status_changed" => Some("agent_changed"),
            "session_created"
            | "session_definition_updated"
            | "session_metadata_updated"
            | "session_archived"
            | "session_unarchived"
            | "session_deleted"
            | "session_forked" => Some("session_changed"),
            "session_loaded"
            | "session_unloaded"
            | "diagnostics_updated"
            | "shared_resources_reloaded" => None,
            _ => return Err(TypedJsonError::EncodingInvariant),
        }
    } else {
        match kind {
            "turn_interrupted" => Some("turn_terminal"),
            "item_completed"
            | "item_tool_invocation_started"
            | "item_tool_invocation_completed"
            | "item_tool_invocation_abandoned" => Some("item_changed"),
            "interaction_resolved" => Some("interaction_resolved"),
            "queue_updated" => Some("queue_updated"),
            "session_definition_updated"
            | "session_metadata_updated"
            | "session_readiness_changed"
            | "session_execution_changed"
            | "session_settled"
            | "usage_updated"
            | "diagnostics_updated"
            | "session_workspace_reloaded"
            | "session_recording_changed"
            | "turn_started"
            | "turn_phase_changed"
            | "interaction_requested" => None,
            _ => return Err(TypedJsonError::EncodingInvariant),
        }
    };

    let Some(expected_object_kind) = expected_object_kind else {
        if detail.is_some_and(|value| !matches!(value, JsonNode::Null)) {
            return Err(selected_wrong_json_type());
        }
        return Ok(());
    };

    let detail = detail.ok_or_else(missing_required_field)?;
    let object = detail.as_object().ok_or_else(selected_wrong_json_type)?;
    let actual_kind = required(object, "type")?
        .as_str()
        .ok_or_else(selected_wrong_json_type)?;
    if actual_kind != expected_object_kind {
        return Err(unknown_output_variant());
    }
    required(object, "data")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TerminalKind {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Copy)]
struct TerminalCorrelationFacts {
    kind: TerminalKind,
}

struct TerminalFacts {
    turn_id: TurnId,
    terminal: TerminalCorrelationFacts,
}

fn validate_turn_terminal_detail(node: &JsonNode) -> Result<TerminalFacts, TypedJsonError> {
    let mut selected = None;
    let mut turn_id = None;
    validate_adjacent_output(node, |kind, data| match kind {
        "turn_terminal" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            turn_id = Some(parse_id(required(object, "turnId")?)?);
            validate_adjacent_output(required(object, "terminal")?, |terminal, data| {
                let data = data
                    .ok_or_else(missing_required_field)?
                    .as_object()
                    .ok_or_else(selected_wrong_json_type)?;
                required(data, "completedAt")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?
                    .parse::<Timestamp>()
                    .map_err(|_| invalid_scalar())?;
                let kind = match terminal {
                    "completed" => TerminalKind::Completed,
                    "interrupted" => {
                        validate_turn_interruption(required(data, "reason")?)?;
                        TerminalKind::Interrupted
                    }
                    "failed" => {
                        validate_turn_failure(required(data, "reason")?)?;
                        TerminalKind::Failed
                    }
                    _ => return Err(unknown_output_variant()),
                };
                selected = Some(TerminalCorrelationFacts { kind });
                Ok(())
            })
        }
        _ => Err(unknown_output_variant()),
    })?;
    Ok(TerminalFacts {
        turn_id: turn_id.ok_or(TypedJsonError::EncodingInvariant)?,
        terminal: selected.ok_or(TypedJsonError::EncodingInvariant)?,
    })
}

fn validate_turn_failure(node: &JsonNode) -> Result<(), TypedJsonError> {
    match node.as_str().ok_or_else(typed_wrong_json_type)? {
        "prompt"
        | "model"
        | "tool"
        | "context_overflow"
        | "dependency_unavailable"
        | "invariant_failure" => Ok(()),
        _ => Err(unknown_output_variant()),
    }
}

fn validate_turn_interruption(node: &JsonNode) -> Result<(), TypedJsonError> {
    match node.as_str().ok_or_else(typed_wrong_json_type)? {
        "user_cancelled" | "security_revoked" | "prepare_for_unload" | "runtime_shutdown"
        | "runtime_failure" => Ok(()),
        _ => Err(unknown_output_variant()),
    }
}

fn validate_progress_event_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    required(object, "timestamp")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    let route = validate_event_route(required(object, "route")?)?;
    match required(object, "kind")?.as_str() {
        Some("model") | Some("tool") | Some("compaction") | Some("retry") => {}
        Some(_) => return Err(unknown_output_variant()),
        None => return Err(typed_wrong_json_type()),
    }
    validate_adjacent_output(required(object, "update")?, |kind, data| {
        let object = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        match kind {
            "item_started" | "item_delta" => {
                if route.family != EventRouteFamily::Item {
                    return Err(invalid_scalar());
                }
                validate_id::<ItemId>(required(object, "itemId")?)?;
                validate_u32(required(object, "contentIndex")?)?;
                match required(object, "contentKind")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?
                {
                    "assistant_text" | "reasoning" => {}
                    _ => return Err(unknown_output_variant()),
                }
                if kind == "item_delta" {
                    required(object, "delta")?
                        .as_str()
                        .ok_or_else(typed_wrong_json_type)?;
                }
                Ok(())
            }
            "tool_output_delta" => {
                if route.family != EventRouteFamily::Item {
                    return Err(invalid_scalar());
                }
                validate_id::<ItemId>(required(object, "itemId")?)?;
                required(object, "toolCallId")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?
                    .parse::<ToolCallId>()
                    .map_err(|_| invalid_scalar())?;
                required(object, "delta")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?;
                Ok(())
            }
            "model_retry_scheduled" => {
                match required(object, "purpose")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?
                {
                    "agent_run" | "compaction_summary" => {}
                    _ => return Err(unknown_output_variant()),
                }
                validate_u8(required(object, "retryCount")?)?;
                required(object, "readyAt")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?
                    .parse::<Timestamp>()
                    .map_err(|_| invalid_scalar())?;
                Ok(())
            }
            "operation_status" => {
                required(object, "message")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?;
                Ok(())
            }
            _ => Err(unknown_output_variant()),
        }
    })
}

fn validate_u8(node: &JsonNode) -> Result<(), TypedJsonError> {
    let literal = node
        .as_number()
        .map(|number| number.raw())
        .ok_or_else(typed_wrong_json_type)?;
    if literal.bytes().all(|byte| byte.is_ascii_digit()) && literal.parse::<u8>().is_ok() {
        Ok(())
    } else {
        Err(invalid_scalar())
    }
}

fn validate_snapshot_semantic_limits(
    snapshot: &SnapshotResponse,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match snapshot {
        SnapshotResponse::Runtime(snapshot) => {
            validate_runtime_snapshot_semantic_limits(snapshot, limits)
        }
        SnapshotResponse::Session(snapshot) => {
            validate_session_snapshot_semantic_limits(snapshot, limits)
        }
    }
}

fn validate_runtime_snapshot_semantic_limits(
    snapshot: &RuntimeSnapshot,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    if snapshot.loaded_sessions().len()
        > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX)
        || snapshot.diagnostics().len() > usize::from(limits.observation.max_snapshot_diagnostics)
    {
        return Err(invalid_scalar());
    }
    for diagnostic in snapshot.diagnostics() {
        RuntimeDiagnosticView::new_with_limits(diagnostic.code(), diagnostic.message(), limits)
            .map_err(|_| invalid_scalar())?;
    }
    Ok(())
}

fn validate_session_snapshot_semantic_limits(
    snapshot: &SessionSnapshot,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    SessionMetadataView::new_with_limits(
        snapshot.metadata().revision(),
        snapshot.metadata().name(),
        snapshot.metadata().description(),
        snapshot.metadata().updated_at(),
        limits,
    )
    .map_err(|_| invalid_scalar())?;
    let workspace = snapshot.definition().workspace();
    if workspace.roots().len() > usize::from(limits.workspace.max_workspace_roots)
        || workspace.cwd().relative_path().as_str().len()
            > usize::try_from(limits.workspace.max_relative_path_bytes).unwrap_or(usize::MAX)
        || snapshot.definition().prompts().enabled().len()
            > usize::try_from(limits.transport.max_array_items).unwrap_or(usize::MAX)
        || snapshot.queues().submit_admissions().len()
            > usize::from(limits.queues.max_submit_admissions)
        || snapshot.queues().steers().len() > usize::from(limits.queues.max_steers)
        || snapshot.queues().follow_ups().len() > usize::from(limits.queues.max_follow_ups)
        || snapshot.diagnostics().len() > usize::from(limits.observation.max_snapshot_diagnostics)
    {
        return Err(invalid_scalar());
    }
    let relative = workspace.cwd().relative_path().as_str();
    if !relative.is_empty()
        && relative.split('/').count() > usize::from(limits.workspace.max_relative_path_segments)
    {
        return Err(invalid_scalar());
    }
    for diagnostic in snapshot.diagnostics() {
        SessionDiagnosticView::new_with_limits(diagnostic.code(), diagnostic.message(), limits)
            .map_err(|_| invalid_scalar())?;
    }
    Ok(())
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
        "agent" => validate_agent_command(data.ok_or_else(missing_required_field)?, limits),
        "interaction" => {
            validate_interaction_command(data.ok_or_else(missing_required_field)?, limits)
        }
        "command_surface" => {
            validate_pending_command_family(data, &["execute_text", "execute_catalog"])
        }
        _ => Err(unknown_input_variant()),
    })
}

fn validate_agent_command(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "create" => validate_create_agent_command(data.ok_or_else(missing_required_field)?, limits),
        "set_status" => validate_set_agent_status_command(data.ok_or_else(missing_required_field)?),
        "delete" => validate_delete_agent_command(data.ok_or_else(missing_required_field)?),
        "update_definition" | "update_metadata" => pending_object(data),
        _ => Err(unknown_input_variant()),
    })
}

fn validate_create_agent_command(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["definition", "metadata"],
    )?;
    let definition = input_object(required(object, "definition")?)?;
    reject_unknown_fields(definition.keys().map(AsRef::as_ref), &["prompts"])?;
    validate_session_prompt_selection(required(definition, "prompts")?, limits)?;

    let metadata = input_object(required(object, "metadata")?)?;
    reject_unknown_fields(metadata.keys().map(AsRef::as_ref), &["name", "description"])?;
    let name = required(metadata, "name")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let description = metadata
        .get("description")
        .map(nullable_string)
        .transpose()?
        .flatten();
    NewAgentMetadata::new_with_limits(name, description, limits).map_err(|_| invalid_scalar())?;
    Ok(())
}

fn validate_set_agent_status_command(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["agentId", "expectedStatus", "status"],
    )?;
    validate_id::<AgentId>(required(object, "agentId")?)?;
    validate_agent_status_input(required(object, "expectedStatus")?, true)?;
    validate_agent_status_input(required(object, "status")?, false)
}

fn validate_delete_agent_command(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["agentId", "expectedStatus"],
    )?;
    validate_id::<AgentId>(required(object, "agentId")?)?;
    validate_agent_status_input(required(object, "expectedStatus")?, true)
}

fn validate_agent_status_input(node: &JsonNode, allow_deleted: bool) -> Result<(), TypedJsonError> {
    match node.as_str().ok_or_else(typed_wrong_json_type)? {
        "enabled" | "disabled" => Ok(()),
        "deleted" if allow_deleted => Ok(()),
        _ => Err(unknown_input_variant()),
    }
}

fn validate_agent_status_output(node: &JsonNode) -> Result<(), TypedJsonError> {
    match node.as_str().ok_or_else(typed_wrong_json_type)? {
        "enabled" | "disabled" | "deleted" => Ok(()),
        _ => Err(unknown_output_variant()),
    }
}

fn validate_session_command(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "load" | "unload" | "archive" | "unarchive" | "delete" => {
            validate_session_id_object(data.ok_or_else(missing_required_field)?)
        }
        "create" => {
            validate_create_session_command(data.ok_or_else(missing_required_field)?, limits)
        }
        "fork" => validate_fork_session_command(data.ok_or_else(missing_required_field)?),
        "update_definition" | "upgrade_agent_revision" | "update_metadata" | "reload_workspace" => {
            pending_object(data)
        }
        _ => Err(unknown_input_variant()),
    })
}

fn validate_fork_session_command(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["sourceSessionId", "anchor"],
    )?;
    validate_id::<SessionId>(required(object, "sourceSessionId")?)?;
    validate_fork_anchor(required(object, "anchor")?)
}

fn validate_fork_anchor(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "genesis" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "before_user_message"
        | "after_user_message"
        | "before_final_agent_message"
        | "after_final_agent_message" => {
            let object = input_object(data.ok_or_else(missing_required_field)?)?;
            reject_unknown_fields(object.keys().map(AsRef::as_ref), &["itemId"])?;
            validate_id::<ItemId>(required(object, "itemId")?)
        }
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
        "steer" => validate_steer_command(data.ok_or_else(missing_required_field)?, limits),
        "follow_up" => validate_follow_up_command(data.ok_or_else(missing_required_field)?, limits),
        "cancel_queued_message" => {
            validate_cancel_queued_message_command(data.ok_or_else(missing_required_field)?)
        }
        "cancel" => validate_cancel_command(data.ok_or_else(missing_required_field)?),
        _ => Err(unknown_input_variant()),
    })
}

fn validate_submit_command(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["sessionId", "intent"])?;
    validate_id::<SessionId>(required(object, "sessionId")?)?;
    validate_prompt_intent(required(object, "intent")?, limits)
}

fn validate_steer_command(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["sessionId", "expectedTurnId", "intent"],
    )?;
    validate_id::<SessionId>(required(object, "sessionId")?)?;
    validate_id::<TurnId>(required(object, "expectedTurnId")?)?;
    validate_prompt_intent(required(object, "intent")?, limits)
}

fn validate_follow_up_command(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["sessionId", "intent"])?;
    validate_id::<SessionId>(required(object, "sessionId")?)?;
    validate_prompt_intent(required(object, "intent")?, limits)
}

fn validate_cancel_queued_message_command(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(
        object.keys().map(AsRef::as_ref),
        &["sessionId", "targetCommandId"],
    )?;
    validate_id::<SessionId>(required(object, "sessionId")?)?;
    validate_id::<CommandId>(required(object, "targetCommandId")?)
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

fn validate_interaction_command(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "resolve" => {
            let object = input_object(data.ok_or_else(missing_required_field)?)?;
            reject_unknown_fields(
                object.keys().map(AsRef::as_ref),
                &[
                    "sessionId",
                    "expectedTurnId",
                    "itemId",
                    "requestId",
                    "resolution",
                    "resolutionKey",
                ],
            )?;
            validate_id::<SessionId>(required(object, "sessionId")?)?;
            validate_id::<TurnId>(required(object, "expectedTurnId")?)?;
            validate_id::<ItemId>(required(object, "itemId")?)?;
            validate_id::<RequestId>(required(object, "requestId")?)?;
            validate_id::<InteractionResolutionKey>(required(object, "resolutionKey")?)?;
            validate_interaction_resolution(required(object, "resolution")?, limits)
        }
        _ => Err(unknown_input_variant()),
    })
}

fn validate_interaction_resolution(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    validate_adjacent_input(node, |kind, data| match kind {
        "cancelled" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "tool_approval" => validate_adjacent_input(
            data.ok_or_else(missing_required_field)?,
            |decision, data| match decision {
                "allow" => {
                    let object = input_object(data.ok_or_else(missing_required_field)?)?;
                    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["optionIndex"])?;
                    validate_u32(required(object, "optionIndex")?)
                }
                "deny" => {
                    if data.is_some() {
                        Err(selected_wrong_json_type())
                    } else {
                        Ok(())
                    }
                }
                _ => Err(unknown_input_variant()),
            },
        ),
        "user_answer" => {
            let object = input_object(data.ok_or_else(missing_required_field)?)?;
            reject_unknown_fields(object.keys().map(AsRef::as_ref), &["answers"])?;
            let answers = required(object, "answers")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?;
            if answers.len() > usize::from(limits.interaction.max_interaction_questions) {
                return Err(invalid_scalar());
            }
            for answer in answers {
                let answer = input_object(answer)?;
                reject_unknown_fields(
                    answer.keys().map(AsRef::as_ref),
                    &["questionIndex", "value"],
                )?;
                validate_u32(required(answer, "questionIndex")?)?;
                validate_adjacent_input(required(answer, "value")?, |kind, data| match kind {
                    "text" => {
                        let object = input_object(data.ok_or_else(missing_required_field)?)?;
                        reject_unknown_fields(object.keys().map(AsRef::as_ref), &["text"])?;
                        required(object, "text")?
                            .as_str()
                            .ok_or_else(typed_wrong_json_type)
                            .map(|_| ())
                    }
                    "choice" => {
                        let object = input_object(data.ok_or_else(missing_required_field)?)?;
                        reject_unknown_fields(object.keys().map(AsRef::as_ref), &["optionIndex"])?;
                        validate_u32(required(object, "optionIndex")?)
                    }
                    _ => Err(unknown_input_variant()),
                })?;
            }
            Ok(())
        }
        _ => Err(unknown_input_variant()),
    })
}

fn validate_u32(node: &JsonNode) -> Result<(), TypedJsonError> {
    let literal = node
        .as_number()
        .map(|number| number.raw())
        .ok_or_else(typed_wrong_json_type)?;
    if literal.bytes().all(|byte| byte.is_ascii_digit()) && literal.parse::<u32>().is_ok() {
        Ok(())
    } else {
        Err(invalid_scalar())
    }
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

fn validate_runtime_query_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
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
        "agent" => {
            validate_adjacent_input(data.ok_or_else(missing_required_field)?, |kind, data| {
                match kind {
                    "list_agents" => {
                        let object = input_object(data.ok_or_else(missing_required_field)?)?;
                        reject_unknown_fields(
                            object.keys().map(AsRef::as_ref),
                            &["page", "includeDeleted"],
                        )?;
                        validate_page_request(required(object, "page")?, limits)?;
                        if !matches!(required(object, "includeDeleted")?, JsonNode::Bool(_)) {
                            return Err(typed_wrong_json_type());
                        }
                        Ok(())
                    }
                    _ => Err(unknown_input_variant()),
                }
            })
        }
        "session" => {
            validate_adjacent_input(data.ok_or_else(missing_required_field)?, |kind, data| {
                match kind {
                    "list_sessions" => {
                        let object = input_object(data.ok_or_else(missing_required_field)?)?;
                        reject_unknown_fields(
                            object.keys().map(AsRef::as_ref),
                            &["page", "includeArchived"],
                        )?;
                        validate_page_request(required(object, "page")?, limits)?;
                        if !matches!(required(object, "includeArchived")?, JsonNode::Bool(_)) {
                            return Err(typed_wrong_json_type());
                        }
                        Ok(())
                    }
                    "get_session_fork_provenance" => {
                        validate_session_id_object(data.ok_or_else(missing_required_field)?)
                    }
                    _ => Err(unknown_input_variant()),
                }
            })
        }
        "command_surface" | "model" | "prompt" | "skill" | "tool" | "usage" | "diagnostics" => {
            Err(TypedJsonError::PendingPublicTarget)
        }
        _ => Err(unknown_input_variant()),
    })
}

fn validate_page_request(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = input_object(node)?;
    reject_unknown_fields(object.keys().map(AsRef::as_ref), &["cursor", "limit"])?;
    match required(object, "cursor")? {
        JsonNode::Null => {}
        cursor => {
            let cursor = cursor.as_str().ok_or_else(typed_wrong_json_type)?;
            if cursor.len() > usize::from(limits.paging.max_page_cursor_bytes) {
                return Err(invalid_scalar());
            }
            cursor.parse::<PageCursor>().map_err(|_| invalid_scalar())?;
        }
    }
    validate_nonzero_u32(required(object, "limit")?)
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

fn validate_query_response_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
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
        "agent" => {
            validate_adjacent_output(data.ok_or_else(missing_required_field)?, |kind, data| {
                match kind {
                    "agents" => {
                        validate_agent_page(data.ok_or_else(missing_required_field)?, limits)
                    }
                    _ => Err(unknown_output_variant()),
                }
            })
        }
        "session" => {
            validate_adjacent_output(data.ok_or_else(missing_required_field)?, |kind, data| {
                match kind {
                    "sessions" => {
                        validate_session_page(data.ok_or_else(missing_required_field)?, limits)
                    }
                    "fork_provenance" => match data.ok_or_else(missing_required_field)? {
                        JsonNode::Null => Ok(()),
                        provenance => validate_session_fork_provenance(provenance),
                    },
                    _ => Err(unknown_output_variant()),
                }
            })
        }
        "command_surface" | "model" | "prompt" | "skill" | "tool" | "usage" | "diagnostics" => {
            Err(TypedJsonError::PendingPublicTarget)
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_agent_page(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let items = required(object, "items")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if items.len() > usize::from(limits.paging.max_page_size) {
        return Err(invalid_scalar());
    }
    for item in items {
        validate_agent_summary(item, limits)?;
    }
    validate_page_next_cursor(object, limits)
}

#[derive(Clone, Copy)]
struct AgentSummaryFacts {
    agent_id: AgentId,
    status: AgentStatus,
}

fn validate_agent_summary(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<AgentSummaryFacts, TypedJsonError> {
    let item = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let agent_id = parse_id::<AgentId>(required(item, "agentId")?)?;
    required(item, "definitionRevision")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<AgentRevision>()
        .map_err(|_| invalid_scalar())?;
    let metadata = required(item, "metadata")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let revision = required(metadata, "revision")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<AgentMetadataRevision>()
        .map_err(|_| invalid_scalar())?;
    let name = required(metadata, "name")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let description = metadata
        .get("description")
        .map(nullable_string)
        .transpose()?
        .flatten();
    let updated_at = required(metadata, "updatedAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    AgentMetadataView::new_with_limits(revision, name, description, updated_at, limits)
        .map_err(|_| invalid_scalar())?;
    let status = match required(item, "status")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "enabled" => AgentStatus::Enabled,
        "disabled" => AgentStatus::Disabled,
        "deleted" => AgentStatus::Deleted,
        _ => return Err(unknown_output_variant()),
    };
    required(item, "createdAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    Ok(AgentSummaryFacts { agent_id, status })
}

fn validate_session_page(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let items = required(object, "items")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if items.len() > usize::from(limits.paging.max_page_size) {
        return Err(invalid_scalar());
    }
    for item in items {
        validate_session_summary(item)?;
    }
    validate_page_next_cursor(object, limits)
}

#[derive(Clone, Copy)]
struct SessionSummaryFacts {
    session_id: SessionId,
    lifecycle: SessionLifecycleView,
    forked: bool,
}

fn validate_session_summary(node: &JsonNode) -> Result<SessionSummaryFacts, TypedJsonError> {
    let item = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let session_id = parse_id::<SessionId>(required(item, "sessionId")?)?;
    required(item, "definitionRevision")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<SessionDefinitionRevision>()
        .map_err(|_| invalid_scalar())?;
    validate_session_metadata_summary(required(item, "metadata")?)?;
    let lifecycle = match required(item, "lifecycle")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "open" => SessionLifecycleView::Open,
        "archived" => SessionLifecycleView::Archived,
        "deleted" => SessionLifecycleView::Deleted,
        _ => return Err(unknown_output_variant()),
    };
    let forked = match required(item, "forked")? {
        JsonNode::Bool(forked) => *forked,
        _ => return Err(typed_wrong_json_type()),
    };
    required(item, "createdAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    Ok(SessionSummaryFacts {
        session_id,
        lifecycle,
        forked,
    })
}

fn validate_session_metadata_summary(node: &JsonNode) -> Result<(), TypedJsonError> {
    let metadata = node.as_object().ok_or_else(selected_wrong_json_type)?;
    required(metadata, "revision")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<SessionMetadataRevision>()
        .map_err(|_| invalid_scalar())?;
    for field in ["name", "description"] {
        match metadata.get(field) {
            None | Some(JsonNode::Null) => {}
            Some(value) if value.as_str().is_some() => {}
            Some(_) => return Err(typed_wrong_json_type()),
        }
    }
    required(metadata, "updatedAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map(|_| ())
        .map_err(|_| invalid_scalar())
}

fn validate_page_next_cursor(
    object: &std::collections::BTreeMap<Box<str>, JsonNode>,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match required(object, "nextCursor")? {
        JsonNode::Null => Ok(()),
        cursor => {
            let cursor = cursor.as_str().ok_or_else(typed_wrong_json_type)?;
            if cursor.len() > usize::from(limits.paging.max_page_cursor_bytes) {
                return Err(invalid_scalar());
            }
            cursor
                .parse::<PageCursor>()
                .map(|_| ())
                .map_err(|_| invalid_scalar())
        }
    }
}

fn validate_session_fork_provenance(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    validate_id::<SessionId>(required(object, "sourceSessionId")?)?;
    match required(object, "source")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "live_snapshot" | "recorded_history" => {}
        _ => return Err(unknown_output_variant()),
    }
    validate_fork_anchor_output(required(object, "anchor")?)
}

fn validate_fork_anchor_output(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "genesis" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "before_user_message"
        | "after_user_message"
        | "before_final_agent_message"
        | "after_final_agent_message" => {
            let object = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            validate_id::<ItemId>(required(object, "itemId")?)
        }
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

fn validate_adjacent_output<T>(
    node: &JsonNode,
    validate: impl FnOnce(&str, Option<&JsonNode>) -> Result<T, TypedJsonError>,
) -> Result<T, TypedJsonError> {
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

fn parse_id<T: FromStr>(node: &JsonNode) -> Result<T, TypedJsonError> {
    let value = node.as_str().ok_or_else(typed_wrong_json_type)?;
    T::from_str(value).map_err(|_| noncanonical_id())
}

fn validate_id<T: FromStr>(node: &JsonNode) -> Result<(), TypedJsonError> {
    parse_id::<T>(node).map(|_| ())
}

fn parse_revision<T: FromStr>(node: &JsonNode) -> Result<T, TypedJsonError> {
    let value = node.as_str().ok_or_else(typed_wrong_json_type)?;
    T::from_str(value).map_err(|_| invalid_scalar())
}

fn validate_revision<T: FromStr>(node: &JsonNode) -> Result<(), TypedJsonError> {
    parse_revision::<T>(node).map(|_| ())
}

fn noncanonical_id() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::TypedScalar,
        PublicDecodeCode::NoncanonicalId,
    )
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

fn duration_out_of_range() -> TypedJsonError {
    public_fault(
        PublicDecodeStage::TypedScalar,
        PublicDecodeCode::DurationOutOfRange,
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
