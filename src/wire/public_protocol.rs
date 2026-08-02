use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::agent_session_lifecycle::{AgentRevisionRef, SessionModelConfig};
use crate::model_gateway::{ModelId, ModelSelection, ProviderId, ReasoningPreference};
use crate::prompt::{
    PromptBodyIntent, PromptId, PromptIntent, PromptValueError, SessionPromptSelection,
    SkillIntent, TextIntent, normalize_text_intent, validate_skill_intent_count,
};
use crate::runtime_interface::{
    CommandCompletion, CommandError, CommandErrorCode, CommandOutcome, CommandOutput,
    CommandRequest, CommandResponse, EventFrame, EventRoute, NewSessionDefinition,
    NewSessionMetadata, PublicCancelTarget, PublicIngressLane, PublicSubject, QueryResponse,
    QueryResult, RetryAdvice, RuntimeCapabilities, RuntimeCommand, RuntimeDiagnosticView,
    RuntimeDispatchError, RuntimeLifecycleCommand, RuntimeQuery, RuntimeQueryResult,
    RuntimeReadQuery, RuntimeRequest, RuntimeSnapshot, RuntimeStateEventKind, RuntimeStatusView,
    RuntimeView, SessionCommand, SessionDefinitionSummary, SessionDiagnosticView,
    SessionEventDetail, SessionExecutionView, SessionMetadataView, SessionReadinessView,
    SessionRecordingState, SessionRecordingView, SessionSnapshot, SessionStateEventKind,
    SessionUsageView, SnapshotRequest, SnapshotResponse, StateEvent, StateEventMsg,
    SubscriptionRequest, SubscriptionScope, TurnCommand, TurnFailureView, TurnTerminalView,
    command_error_code_allows_retry_with_backoff, validate_command_error_contract,
    validate_command_error_message, validate_command_output,
};
use crate::skills::SkillId;
use crate::tools::{ToolCallId, ToolName};
use crate::workspace::{
    RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput,
    WorkspaceDefinitionSummaryView, WorkspaceRootInput, WorkspaceRootKey, WorkspaceRootSummaryView,
    WorkspaceSourcePolicy,
};

use super::bounded_json::{JsonNode, public_node_encoded_len};
use super::lexical::validate_safe_text;
use super::limits::{CapabilityToken, ProtocolLimits, runtime_capability_from_token};
use super::scalar::{
    AgentId, AgentMetadataRevision, AgentRevision, CommandId, ItemId, RequestId,
    SessionDefinitionRevision, SessionId, SessionMetadataRevision, TurnId,
};
use super::typed_json::{
    PublicDecodeCode, PublicDecodeError, PublicDecodeStage, PublicJsonKind, TypedJsonError,
    WireV1Codec,
};
use super::{CanonicalFileUri, Money, Timestamp, WorkspaceRelativePath};

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
enum EventFrameInput {
    Snapshot(SnapshotResponseInput),
    State(StateEventInput),
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

struct EmptyObservationListInput;

impl EmptyObservationListInput {
    const fn confirm_empty(&self) {}
}

impl<'de> Deserialize<'de> for EmptyObservationListInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = EmptyObservationListInput;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an empty observation list")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom(
                        "observation list belongs to a pending protocol slice",
                    ));
                }
                Ok(EmptyObservationListInput)
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

struct UnsupportedObservationOutput;

impl Serialize for UnsupportedObservationOutput {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        unreachable!("unsupported observation outputs cannot be constructed")
    }
}

struct EmptyObservationListOutput;

impl Serialize for EmptyObservationListOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;
        serializer.serialize_seq(Some(0))?.end()
    }
}

impl EventFrameInput {
    fn into_semantic(self, limits: ProtocolLimits) -> Result<EventFrame, TypedJsonError> {
        Ok(match self {
            Self::Snapshot(snapshot) => EventFrame::Snapshot(snapshot.into_semantic(limits)?),
            Self::State(event) => EventFrame::State(event.into_semantic(limits)?),
        })
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
    current_turn: Option<UnsupportedObservationInput>,
    active_items: EmptyObservationListInput,
    pending_interactions: EmptyObservationListInput,
    queues: SessionQueueInput,
    recording: SessionRecordingInput,
    usage: Option<SessionUsageInput>,
    diagnostics: Vec<PublicDiagnosticInput>,
}

impl SessionSnapshot {
    fn from_input(
        value: SessionSnapshotInput,
        limits: ProtocolLimits,
    ) -> Result<Self, TypedJsonError> {
        value.active_items.confirm_empty();
        value.pending_interactions.confirm_empty();
        if !matches!(value.lifecycle, SessionLifecycleInput::Open)
            || !matches!(value.load_state, SessionLoadStateInput::Loaded)
            || !matches!(value.readiness, SessionReadinessInput::Ready)
            || !matches!(value.execution, SessionExecutionInput::Idle)
            || value.current_turn.is_some()
            || !value.queues.is_idle_accepting()
        {
            return Err(TypedJsonError::PendingPublicTarget);
        }
        let diagnostics = value
            .diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.into_session(limits))
            .collect::<Result<Vec<_>, _>>()?;
        SessionSnapshot::new_loaded_ready_idle_with_limits(
            value.session_id,
            value.metadata.into_semantic(limits)?,
            value.definition.into_semantic(limits)?,
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

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionLoadStateInput {
    Loaded,
    Unloading,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionQueueInput {
    submit_admissions: EmptyObservationListInput,
    steers: EmptyObservationListInput,
    follow_ups: EmptyObservationListInput,
    accepting_input: bool,
}

impl SessionQueueInput {
    fn is_idle_accepting(&self) -> bool {
        self.submit_admissions.confirm_empty();
        self.steers.confirm_empty();
        self.follow_ups.confirm_empty();
        self.accepting_input
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
            Self::Runtime(value) => {
                if value.detail.is_some() {
                    return Err(invalid_scalar());
                }
                StateEventMsg::Runtime {
                    kind: value.kind.into_semantic()?,
                    snapshot: value.snapshot.into_semantic(limits)?,
                }
            }
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
    detail: Option<UnsupportedObservationInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeStateEventKindInput {
    CommandCatalogInvalidated,
}

impl RuntimeStateEventKindInput {
    fn into_semantic(self) -> Result<RuntimeStateEventKind, TypedJsonError> {
        Ok(match self {
            Self::CommandCatalogInvalidated => RuntimeStateEventKind::CommandCatalogInvalidated,
        })
    }
}

#[derive(Deserialize)]
struct SessionStateEventInput {
    kind: SessionStateEventKindInput,
    snapshot: SessionSnapshotInput,
    detail: Option<SessionEventDetailInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionStateEventKindInput {
    TurnCompleted,
    TurnFailed,
}

impl SessionStateEventKindInput {
    fn into_semantic(self) -> Result<SessionStateEventKind, TypedJsonError> {
        Ok(match self {
            Self::TurnCompleted => SessionStateEventKind::TurnCompleted,
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
    Failed(FailedTerminalInput),
}

impl TurnTerminalInput {
    const fn into_semantic(self) -> TurnTerminalView {
        match self {
            Self::Completed(value) => TurnTerminalView::Completed {
                completed_at: value.completed_at,
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
}

impl<'a> EventFrameOutput<'a> {
    fn from_semantic(value: &'a EventFrame) -> Self {
        match value {
            EventFrame::Snapshot(snapshot) => {
                Self::Snapshot(SnapshotResponseOutput::from_semantic(snapshot))
            }
            EventFrame::State(event) => Self::State(StateEventOutput::from_semantic(event)),
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
    current_turn: Option<UnsupportedObservationOutput>,
    active_items: EmptyObservationListOutput,
    pending_interactions: EmptyObservationListOutput,
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
            current_turn: None,
            active_items: EmptyObservationListOutput,
            pending_interactions: EmptyObservationListOutput,
            queues: SessionQueueOutput {
                submit_admissions: EmptyObservationListOutput,
                steers: EmptyObservationListOutput,
                follow_ups: EmptyObservationListOutput,
                accepting_input: true,
            },
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
#[serde(rename_all = "snake_case")]
enum SessionLifecycleOutput {
    Open,
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
    submit_admissions: EmptyObservationListOutput,
    steers: EmptyObservationListOutput,
    follow_ups: EmptyObservationListOutput,
    accepting_input: bool,
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
            StateEventMsg::Runtime { kind, snapshot } => Self::Runtime(RuntimeStateEventOutput {
                kind: RuntimeStateEventKindOutput::from_semantic(*kind),
                snapshot: RuntimeSnapshotOutput::from_semantic(snapshot),
                detail: None,
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
    detail: Option<UnsupportedObservationOutput>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeStateEventKindOutput {
    CommandCatalogInvalidated,
}

impl RuntimeStateEventKindOutput {
    const fn from_semantic(value: RuntimeStateEventKind) -> Self {
        match value {
            RuntimeStateEventKind::CommandCatalogInvalidated => Self::CommandCatalogInvalidated,
        }
    }
}

#[derive(Serialize)]
struct SessionStateEventOutput<'a> {
    kind: SessionStateEventKindOutput,
    snapshot: SessionSnapshotOutput<'a>,
    detail: Option<SessionEventDetailOutput>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionStateEventKindOutput {
    TurnCompleted,
    TurnFailed,
}

impl SessionStateEventKindOutput {
    const fn from_semantic(value: SessionStateEventKind) -> Self {
        match value {
            SessionStateEventKind::TurnCompleted => Self::TurnCompleted,
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
    Failed(FailedTerminalOutput),
}

impl TurnTerminalOutput {
    const fn from_semantic(value: TurnTerminalView) -> Self {
        match value {
            TurnTerminalView::Completed { completed_at } => {
                Self::Completed(CompletedTerminalOutput { completed_at })
            }
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

fn validate_event_frame_semantic_limits(
    frame: &EventFrame,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    match frame {
        EventFrame::Snapshot(snapshot) => validate_snapshot_semantic_limits(snapshot, limits),
        EventFrame::State(event) => match event.msg() {
            StateEventMsg::Runtime { snapshot, .. } => {
                validate_runtime_snapshot_semantic_limits(snapshot, limits)
            }
            StateEventMsg::Session { snapshot, .. } => {
                validate_session_snapshot_semantic_limits(snapshot, limits)
            }
        },
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
            validate_progress_event_shape(data.ok_or_else(missing_required_field)?, limits)?;
            Err(TypedJsonError::PendingPublicTarget)
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
            Err(TypedJsonError::PendingPublicTarget)
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
    let diagnostics_empty = validate_diagnostics(
        required(object, "diagnostics")?,
        limits,
        DiagnosticScope::Runtime,
    )?;
    if diagnostics_empty {
        Ok(())
    } else {
        Err(TypedJsonError::PendingPublicTarget)
    }
}

fn validate_session_snapshot_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let facts = validate_session_snapshot_facts(node, limits)?;
    if facts.pending {
        Err(TypedJsonError::PendingPublicTarget)
    } else {
        Ok(())
    }
}

struct SessionSnapshotShapeFacts {
    session_id: SessionId,
    ready: bool,
    execution: Box<str>,
    current_turn: Option<CurrentTurnShapeFacts>,
    current_turn_id: Option<TurnId>,
    active_items: BTreeMap<ItemId, ActiveItemShapeFacts>,
    pending_interactions: BTreeMap<RequestId, PendingInteractionShapeFacts>,
    queue_command_ids: BTreeSet<CommandId>,
    recording_healthy: bool,
    pending: bool,
}

fn validate_session_snapshot_facts(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<SessionSnapshotShapeFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let session_id = parse_id(required(object, "sessionId")?)?;

    let mut pending = false;
    let lifecycle = required(object, "lifecycle")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    match lifecycle {
        "open" => {}
        "archived" | "deleted" => pending = true,
        _ => return Err(unknown_output_variant()),
    }
    validate_session_metadata(required(object, "metadata")?, limits)?;
    let definition_session_id =
        validate_session_definition_summary(required(object, "definition")?, limits)?;
    if definition_session_id != session_id {
        return Err(invalid_scalar());
    }
    let load_state = required(object, "loadState")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    match load_state {
        "loaded" => {}
        "unloading" => pending = true,
        _ => return Err(unknown_output_variant()),
    }
    if lifecycle != "open" {
        return Err(invalid_scalar());
    }
    let ready = validate_readiness(required(object, "readiness")?)?;
    if !ready {
        pending = true;
    }
    let execution = validate_execution(required(object, "execution")?)?;
    if execution != "idle" {
        pending = true;
    }
    let mut current_turn = None;
    if let Some(current_turn_node) = object.get("currentTurn") {
        if !matches!(current_turn_node, JsonNode::Null) {
            current_turn = Some(validate_current_turn(current_turn_node)?);
            pending = true;
        }
    }
    let current_turn_id = current_turn.as_ref().map(|facts| facts.turn_id);
    if matches!(execution, "idle" | "starting") != current_turn_id.is_none() {
        return Err(invalid_scalar());
    }
    if execution == "running"
        && current_turn
            .as_ref()
            .is_some_and(|facts| facts.terminal.is_some())
    {
        return Err(invalid_scalar());
    }
    let active_items = required(object, "activeItems")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if active_items.len() > usize::from(limits.observation.max_active_items) {
        return Err(invalid_scalar());
    }
    let mut active_items_by_id = BTreeMap::new();
    let mut input_messages = 0_usize;
    let mut first_item_is_input = false;
    let mut has_final_agent_message = false;
    for (index, item) in active_items.iter().enumerate() {
        let item = validate_pending_item_identity(item, limits)?;
        if current_turn_id != Some(item.turn_id) {
            return Err(invalid_scalar());
        }
        let is_input = item.user_source == Some(UserMessageSourceFacts::Input);
        let is_final = item.agent_disposition == Some(AgentDispositionFacts::Final);
        if active_items_by_id.insert(item.item_id, item).is_some() {
            return Err(duplicate_value());
        }
        if is_input {
            input_messages = input_messages.saturating_add(1);
            first_item_is_input |= index == 0;
        }
        has_final_agent_message |= is_final;
        pending = true;
    }
    if current_turn.is_some() && (input_messages != 1 || !first_item_is_input) {
        return Err(invalid_scalar());
    }
    let turn_completed = current_turn
        .as_ref()
        .and_then(|turn| turn.terminal.as_ref())
        .is_some_and(|terminal| terminal.kind == TerminalKind::Completed);
    if has_final_agent_message && !turn_completed {
        return Err(invalid_scalar());
    }
    let interactions = required(object, "pendingInteractions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if interactions.len() > usize::from(limits.observation.max_pending_interactions) {
        return Err(invalid_scalar());
    }
    let mut pending_interactions = BTreeMap::new();
    for interaction in interactions {
        let identity = validate_pending_interaction_identity(interaction, limits)?;
        let Some(item) = active_items_by_id.get(&identity.item_id) else {
            return Err(invalid_scalar());
        };
        let matching_tool = item.status == ItemStatusFacts::Started
            && item.family == ItemFamilyFacts::ToolInvocation
            && match identity.family {
                InteractionRequestFamilyFacts::ToolApproval => item.tool_name == identity.tool_name,
                InteractionRequestFamilyFacts::UserQuestion => item
                    .tool_name
                    .as_ref()
                    .is_some_and(|tool_name| tool_name.as_str() == "ask_user"),
            };
        let matching_phase = current_turn.as_ref().is_some_and(|turn| {
            turn.phase
                == Some(match identity.family {
                    InteractionRequestFamilyFacts::ToolApproval => TurnPhaseFacts::WaitingApproval,
                    InteractionRequestFamilyFacts::UserQuestion => {
                        TurnPhaseFacts::WaitingForUserInput
                    }
                })
        });
        if current_turn_id != Some(identity.turn_id) || !matching_tool || !matching_phase {
            return Err(invalid_scalar());
        }
        if pending_interactions
            .insert(identity.request_id, identity)
            .is_some()
        {
            return Err(duplicate_value());
        }
        pending = true;
    }
    let queues = validate_session_queues(
        required(object, "queues")?,
        limits,
        execution,
        current_turn_id,
    )?;
    if !queues.minimal {
        pending = true;
    }
    let cleaned_up = execution == "idle"
        && current_turn_id.is_none()
        && active_items_by_id.is_empty()
        && pending_interactions.is_empty()
        && queues.empty;
    if (!ready || load_state == "unloading") && (!cleaned_up || queues.accepting) {
        return Err(invalid_scalar());
    }
    let recording_healthy = validate_recording(required(object, "recording")?)?;
    if !recording_healthy {
        pending = true;
    }
    if let Some(usage) = object.get("usage") {
        if !matches!(usage, JsonNode::Null) && !validate_session_usage(usage)? {
            pending = true;
        }
    }
    let diagnostics = required(object, "diagnostics")?;
    if !validate_diagnostics(diagnostics, limits, DiagnosticScope::Session)? {
        pending = true;
    }
    if (!recording_healthy && !has_current_recording_diagnostic(diagnostics)?)
        || (recording_healthy && has_recording_diagnostic(diagnostics)?)
    {
        return Err(invalid_scalar());
    }
    Ok(SessionSnapshotShapeFacts {
        session_id,
        ready,
        execution: execution.into(),
        current_turn,
        current_turn_id,
        active_items: active_items_by_id,
        pending_interactions,
        queue_command_ids: queues.command_ids,
        recording_healthy,
        pending,
    })
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

#[derive(Clone)]
struct CurrentTurnShapeFacts {
    turn_id: TurnId,
    terminal: Option<TerminalCorrelationFacts>,
    phase: Option<TurnPhaseFacts>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TurnPhaseFacts {
    Sampling,
    RetryBackoff,
    Compacting,
    WaitingApproval,
    WaitingForUserInput,
    ExecutingTools,
}

fn validate_current_turn(node: &JsonNode) -> Result<CurrentTurnShapeFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let turn_id = parse_id(required(object, "turnId")?)?;
    let mut terminal = None;
    validate_adjacent_output(required(object, "status")?, |kind, data| match kind {
        "running" => {
            if data.is_some() {
                Err(selected_wrong_json_type())
            } else {
                Ok(())
            }
        }
        "completed" | "interrupted" | "failed" => {
            let data = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            let completed_at = required(data, "completedAt")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
                .parse::<Timestamp>()
                .map_err(|_| invalid_scalar())?;
            let (kind, reason) = match kind {
                "completed" => (TerminalKind::Completed, None),
                "failed" => {
                    let reason = required(data, "reason")?
                        .as_str()
                        .ok_or_else(typed_wrong_json_type)?;
                    validate_turn_failure(required(data, "reason")?)?;
                    (TerminalKind::Failed, Some(Box::<str>::from(reason)))
                }
                "interrupted" => {
                    let reason = required(data, "reason")?
                        .as_str()
                        .ok_or_else(typed_wrong_json_type)?;
                    validate_turn_interruption(required(data, "reason")?)?;
                    (TerminalKind::Interrupted, Some(Box::<str>::from(reason)))
                }
                _ => unreachable!(),
            };
            terminal = Some(TerminalCorrelationFacts {
                kind,
                completed_at,
                reason,
            });
            Ok(())
        }
        _ => Err(unknown_output_variant()),
    })?;
    let mut phase = None;
    if let Some(phase_node) = object.get("phase") {
        if !matches!(phase_node, JsonNode::Null) {
            phase = Some(
                match phase_node.as_str().ok_or_else(typed_wrong_json_type)? {
                    "sampling" => TurnPhaseFacts::Sampling,
                    "retry_backoff" => TurnPhaseFacts::RetryBackoff,
                    "compacting" => TurnPhaseFacts::Compacting,
                    "waiting_approval" => TurnPhaseFacts::WaitingApproval,
                    "waiting_for_user_input" => TurnPhaseFacts::WaitingForUserInput,
                    "executing_tools" => TurnPhaseFacts::ExecutingTools,
                    _ => return Err(unknown_output_variant()),
                },
            );
        }
    }
    if terminal.is_some() == phase.is_some() {
        return Err(invalid_scalar());
    }
    required(object, "startedAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    Ok(CurrentTurnShapeFacts {
        turn_id,
        terminal,
        phase,
    })
}

fn validate_pending_item_identity(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<ActiveItemShapeFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let item_id = parse_id(required(object, "itemId")?)?;
    let turn_id = parse_id(required(object, "turnId")?)?;
    let status = match required(object, "status")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "started" => ItemStatusFacts::Started,
        "completed" => ItemStatusFacts::Completed,
        "abandoned" => ItemStatusFacts::Abandoned,
        _ => return Err(unknown_output_variant()),
    };
    let content = validate_adjacent_output(required(object, "content")?, |kind, data| {
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        validate_pending_item_content(kind, data, limits)
    })?;
    let valid_status = match content.family {
        ItemFamilyFacts::ToolInvocation => match status {
            ItemStatusFacts::Started | ItemStatusFacts::Abandoned => !content.tool_result,
            ItemStatusFacts::Completed => content.tool_result,
        },
        ItemFamilyFacts::UserMessage
        | ItemFamilyFacts::AgentMessage
        | ItemFamilyFacts::Reasoning => status == ItemStatusFacts::Completed,
    };
    if !valid_status {
        return Err(invalid_scalar());
    }
    required(object, "createdAt")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    let completed = match object.get("completedAt") {
        Some(JsonNode::Null) | None => false,
        Some(completed_at) => {
            completed_at
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
                .parse::<Timestamp>()
                .map_err(|_| invalid_scalar())?;
            true
        }
    };
    if (status == ItemStatusFacts::Started) == completed {
        return Err(invalid_scalar());
    }
    let projection = project_pending_item(object)?;
    if public_node_encoded_len(&projection).ok_or_else(invalid_scalar)?
        > usize::try_from(limits.observation.max_item_view_bytes).unwrap_or(usize::MAX)
    {
        return Err(invalid_scalar());
    }
    Ok(ActiveItemShapeFacts {
        turn_id,
        item_id,
        projection,
        status,
        family: content.family,
        tool_name: content.tool_name,
        user_source: content.user_source,
        agent_disposition: content.agent_disposition,
    })
}

fn project_pending_item(object: &BTreeMap<Box<str>, JsonNode>) -> Result<JsonNode, TypedJsonError> {
    let content = required(object, "content")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let kind = required(content, "type")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let data = required(content, "data")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let projected_data = match kind {
        "user_message" => json_node_object(vec![
            ("source", required(data, "source")?.clone()),
            ("body", optional_node(data.get("body"))),
            (
                "contributions",
                JsonNode::Array(
                    required(data, "contributions")?
                        .as_array()
                        .ok_or_else(selected_wrong_json_type)?
                        .iter()
                        .map(project_prompt_contribution_origin)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ),
        ]),
        "agent_message" => json_node_object(vec![
            ("disposition", required(data, "disposition")?.clone()),
            ("text", required(data, "text")?.clone()),
        ]),
        "reasoning" => json_node_object(vec![("summaries", required(data, "summaries")?.clone())]),
        "tool_invocation" => {
            let result = match data.get("result") {
                Some(result) if !matches!(result, JsonNode::Null) => {
                    let result = result.as_object().ok_or_else(selected_wrong_json_type)?;
                    json_node_object(vec![
                        ("disposition", required(result, "disposition")?.clone()),
                        ("summary", required(result, "summary")?.clone()),
                    ])
                }
                _ => JsonNode::Null,
            };
            json_node_object(vec![
                ("toolCallId", required(data, "toolCallId")?.clone()),
                ("toolName", required(data, "toolName")?.clone()),
                (
                    "argumentsSummary",
                    required(data, "argumentsSummary")?.clone(),
                ),
                ("result", result),
            ])
        }
        _ => return Err(unknown_output_variant()),
    };
    Ok(json_node_object(vec![
        ("itemId", required(object, "itemId")?.clone()),
        ("turnId", required(object, "turnId")?.clone()),
        ("status", required(object, "status")?.clone()),
        (
            "content",
            json_node_object(vec![
                ("type", JsonNode::String(kind.into())),
                ("data", projected_data),
            ]),
        ),
        ("createdAt", required(object, "createdAt")?.clone()),
        ("completedAt", optional_node(object.get("completedAt"))),
    ]))
}

fn project_prompt_contribution_origin(node: &JsonNode) -> Result<JsonNode, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let data = required(object, "data")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let projected = match kind {
        "skill" => json_node_object(vec![("skillId", required(data, "skillId")?.clone())]),
        "workspace" => json_node_object(vec![
            ("rootKey", required(data, "rootKey")?.clone()),
            (
                "relativeLocation",
                required(data, "relativeLocation")?.clone(),
            ),
        ]),
        _ => return Err(unknown_output_variant()),
    };
    Ok(json_node_object(vec![
        ("type", JsonNode::String(kind.into())),
        ("data", projected),
    ]))
}

fn optional_node(node: Option<&JsonNode>) -> JsonNode {
    match node {
        Some(value) if !matches!(value, JsonNode::Null) => value.clone(),
        Some(_) | None => JsonNode::Null,
    }
}

fn json_node_object(fields: Vec<(&str, JsonNode)>) -> JsonNode {
    JsonNode::Object(
        fields
            .into_iter()
            .map(|(field, value)| (Box::<str>::from(field), value))
            .collect(),
    )
}

fn validate_pending_item_content(
    kind: &str,
    data: &std::collections::BTreeMap<Box<str>, JsonNode>,
    limits: ProtocolLimits,
) -> Result<PendingItemContentFacts, TypedJsonError> {
    let maximum = usize::try_from(limits.observation.max_item_view_bytes).unwrap_or(usize::MAX);
    match kind {
        "user_message" => {
            let source = match required(data, "source")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
            {
                "input" => UserMessageSourceFacts::Input,
                "steer" => UserMessageSourceFacts::Steer,
                _ => return Err(unknown_output_variant()),
            };
            let body_parts = match data.get("body") {
                Some(body) if !matches!(body, JsonNode::Null) => {
                    let body = body.as_str().ok_or_else(typed_wrong_json_type)?;
                    let part_maximum =
                        usize::try_from(limits.prompt.max_message_part_bytes).unwrap_or(usize::MAX);
                    validate_safe_text(body, part_maximum, false).map_err(|_| invalid_scalar())?;
                    if body.len()
                        > usize::try_from(limits.prompt.max_user_message_bytes)
                            .unwrap_or(usize::MAX)
                    {
                        return Err(invalid_scalar());
                    }
                    1_usize
                }
                Some(_) | None => 0_usize,
            };
            let contributions = required(data, "contributions")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?;
            let part_count = body_parts
                .checked_add(contributions.len())
                .ok_or_else(invalid_scalar)?;
            if part_count == 0 || part_count > usize::from(limits.prompt.max_user_message_parts) {
                return Err(invalid_scalar());
            }
            let mut origins = BTreeSet::new();
            for contribution in contributions {
                if !origins.insert(validate_prompt_contribution_origin(contribution, limits)?) {
                    return Err(duplicate_value());
                }
            }
            Ok(PendingItemContentFacts {
                family: ItemFamilyFacts::UserMessage,
                tool_result: false,
                tool_name: None,
                user_source: Some(source),
                agent_disposition: None,
            })
        }
        "agent_message" => {
            let disposition = match required(data, "disposition")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
            {
                "intermediate" => AgentDispositionFacts::Intermediate,
                "final" => AgentDispositionFacts::Final,
                _ => return Err(unknown_output_variant()),
            };
            validate_safe_string_array(required(data, "text")?, maximum)?;
            Ok(PendingItemContentFacts {
                family: ItemFamilyFacts::AgentMessage,
                tool_result: false,
                tool_name: None,
                user_source: None,
                agent_disposition: Some(disposition),
            })
        }
        "reasoning" => {
            validate_safe_string_array(required(data, "summaries")?, maximum)?;
            Ok(PendingItemContentFacts::non_tool(
                ItemFamilyFacts::Reasoning,
            ))
        }
        "tool_invocation" => {
            required(data, "toolCallId")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
                .parse::<ToolCallId>()
                .map_err(|_| invalid_scalar())?;
            let tool_name = required(data, "toolName")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
                .parse::<ToolName>()
                .map_err(|_| invalid_scalar())?;
            validate_safe_string(
                required(data, "argumentsSummary")?,
                usize::try_from(limits.text.max_public_summary_bytes).unwrap_or(usize::MAX),
                false,
            )?;
            let mut tool_result = false;
            if let Some(result) = data.get("result") {
                if !matches!(result, JsonNode::Null) {
                    tool_result = true;
                    let result = result.as_object().ok_or_else(selected_wrong_json_type)?;
                    match required(result, "disposition")?
                        .as_str()
                        .ok_or_else(typed_wrong_json_type)?
                    {
                        "succeeded" | "failed" | "denied" | "cancelled" => {}
                        _ => return Err(unknown_output_variant()),
                    }
                    validate_safe_string(
                        required(result, "summary")?,
                        usize::try_from(limits.text.max_public_summary_bytes).unwrap_or(usize::MAX),
                        true,
                    )?;
                }
            }
            Ok(PendingItemContentFacts {
                family: ItemFamilyFacts::ToolInvocation,
                tool_result,
                tool_name: Some(tool_name),
                user_source: None,
                agent_disposition: None,
            })
        }
        _ => Err(unknown_output_variant()),
    }
}

fn validate_prompt_contribution_origin(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<PromptContributionIdentityFacts, TypedJsonError> {
    validate_adjacent_output(node, |kind, data| {
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        match kind {
            "skill" => {
                let skill_id = required(data, "skillId")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?;
                skill_id.parse::<SkillId>().map_err(|_| invalid_scalar())?;
                Ok(PromptContributionIdentityFacts::Skill(skill_id.into()))
            }
            "workspace" => {
                let root_key = required(data, "rootKey")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?;
                root_key
                    .parse::<WorkspaceRootKey>()
                    .map_err(|_| invalid_scalar())?;
                let relative_value = required(data, "relativeLocation")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?;
                let relative = relative_value
                    .parse::<WorkspaceRelativePath>()
                    .map_err(|_| invalid_scalar())?;
                validate_workspace_relative_limits(&relative, limits)?;
                Ok(PromptContributionIdentityFacts::Workspace(
                    root_key.into(),
                    relative_value.into(),
                ))
            }
            _ => Err(unknown_output_variant()),
        }
    })
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum PromptContributionIdentityFacts {
    Skill(Box<str>),
    Workspace(Box<str>, Box<str>),
}

fn validate_workspace_relative_limits(
    relative: &WorkspaceRelativePath,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let value = relative.as_str();
    if value.len() > usize::try_from(limits.workspace.max_relative_path_bytes).unwrap_or(usize::MAX)
        || (!value.is_empty()
            && value.split('/').count() > usize::from(limits.workspace.max_relative_path_segments))
    {
        Err(invalid_scalar())
    } else {
        Ok(())
    }
}

fn validate_safe_string_array(node: &JsonNode, maximum: usize) -> Result<(), TypedJsonError> {
    let values = node.as_array().ok_or_else(selected_wrong_json_type)?;
    for value in values {
        validate_safe_string(value, maximum, true)?;
    }
    Ok(())
}

fn validate_pending_interaction_identity(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<PendingInteractionShapeFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let request_id = parse_id(required(object, "requestId")?)?;
    let turn_id = parse_id(required(object, "turnId")?)?;
    let item_id = parse_id(required(object, "itemId")?)?;
    let request = validate_adjacent_output(required(object, "request")?, |kind, data| {
        if !matches!(kind, "tool_approval" | "user_question") {
            return Err(unknown_output_variant());
        }
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        validate_pending_interaction_request(kind, data, limits)
    })?;
    let projection = project_pending_interaction(object)?;
    if public_node_encoded_len(&projection).ok_or_else(invalid_scalar)?
        > usize::try_from(limits.interaction.max_interaction_view_bytes).unwrap_or(usize::MAX)
    {
        return Err(invalid_scalar());
    }
    Ok(PendingInteractionShapeFacts {
        request_id,
        turn_id,
        item_id,
        family: request.family,
        tool_name: request.tool_name,
    })
}

#[derive(Clone, Copy)]
enum InteractionRequestFamilyFacts {
    ToolApproval,
    UserQuestion,
}

struct PendingInteractionRequestFacts {
    family: InteractionRequestFamilyFacts,
    tool_name: Option<ToolName>,
}

struct PendingInteractionShapeFacts {
    request_id: RequestId,
    turn_id: TurnId,
    item_id: ItemId,
    family: InteractionRequestFamilyFacts,
    tool_name: Option<ToolName>,
}

fn project_pending_interaction(
    object: &BTreeMap<Box<str>, JsonNode>,
) -> Result<JsonNode, TypedJsonError> {
    let request = required(object, "request")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let kind = required(request, "type")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let data = required(request, "data")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let request_data = match kind {
        "tool_approval" => {
            let options = required(data, "options")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?
                .iter()
                .map(|option| {
                    let option = option.as_object().ok_or_else(selected_wrong_json_type)?;
                    Ok(json_node_object(vec![
                        ("optionIndex", required(option, "optionIndex")?.clone()),
                        ("kind", required(option, "kind")?.clone()),
                        ("label", required(option, "label")?.clone()),
                        (
                            "effectiveRequirements",
                            project_requirement_summary(required(
                                option,
                                "effectiveRequirements",
                            )?)?,
                        ),
                    ]))
                })
                .collect::<Result<Vec<_>, TypedJsonError>>()?;
            json_node_object(vec![
                ("toolName", required(data, "toolName")?.clone()),
                (
                    "argumentsSummary",
                    required(data, "argumentsSummary")?.clone(),
                ),
                ("reason", required(data, "reason")?.clone()),
                (
                    "requirements",
                    project_requirement_summary(required(data, "requirements")?)?,
                ),
                ("options", JsonNode::Array(options)),
            ])
        }
        "user_question" => {
            let questions = required(data, "questions")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?
                .iter()
                .map(|question| {
                    let question = question.as_object().ok_or_else(selected_wrong_json_type)?;
                    Ok(json_node_object(vec![
                        (
                            "questionIndex",
                            required(question, "questionIndex")?.clone(),
                        ),
                        ("prompt", required(question, "prompt")?.clone()),
                        ("required", required(question, "required")?.clone()),
                        (
                            "input",
                            project_question_input(required(question, "input")?)?,
                        ),
                    ]))
                })
                .collect::<Result<Vec<_>, TypedJsonError>>()?;
            json_node_object(vec![
                ("title", optional_node(data.get("title"))),
                ("questions", JsonNode::Array(questions)),
            ])
        }
        _ => return Err(unknown_output_variant()),
    };
    Ok(json_node_object(vec![
        ("requestId", required(object, "requestId")?.clone()),
        ("turnId", required(object, "turnId")?.clone()),
        ("itemId", required(object, "itemId")?.clone()),
        (
            "request",
            json_node_object(vec![
                ("type", JsonNode::String(kind.into())),
                ("data", request_data),
            ]),
        ),
    ]))
}

fn project_requirement_summary(node: &JsonNode) -> Result<JsonNode, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    Ok(json_node_object(vec![
        ("filesystem", optional_node(object.get("filesystem"))),
        ("network", optional_node(object.get("network"))),
        ("process", optional_node(object.get("process"))),
    ]))
}

fn project_question_input(node: &JsonNode) -> Result<JsonNode, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let kind = required(object, "type")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    let data = required(object, "data")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let data = match kind {
        "text" => json_node_object(vec![("multiline", required(data, "multiline")?.clone())]),
        "single_choice" => {
            let options = required(data, "options")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?
                .iter()
                .map(|option| {
                    let option = option.as_object().ok_or_else(selected_wrong_json_type)?;
                    Ok(json_node_object(vec![
                        ("optionIndex", required(option, "optionIndex")?.clone()),
                        ("label", required(option, "label")?.clone()),
                    ]))
                })
                .collect::<Result<Vec<_>, TypedJsonError>>()?;
            json_node_object(vec![("options", JsonNode::Array(options))])
        }
        _ => return Err(unknown_output_variant()),
    };
    Ok(json_node_object(vec![
        ("type", JsonNode::String(kind.into())),
        ("data", data),
    ]))
}

fn validate_pending_interaction_request(
    kind: &str,
    data: &std::collections::BTreeMap<Box<str>, JsonNode>,
    limits: ProtocolLimits,
) -> Result<PendingInteractionRequestFacts, TypedJsonError> {
    match kind {
        "tool_approval" => {
            let tool_name = required(data, "toolName")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
                .parse::<ToolName>()
                .map_err(|_| invalid_scalar())?;
            validate_safe_string(
                required(data, "argumentsSummary")?,
                usize::try_from(limits.text.max_public_summary_bytes).unwrap_or(usize::MAX),
                false,
            )?;
            validate_safe_string(
                required(data, "reason")?,
                usize::try_from(limits.text.max_description_bytes).unwrap_or(usize::MAX),
                false,
            )?;
            validate_requirement_summary(required(data, "requirements")?, limits)?;
            let options = required(data, "options")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?;
            if options.is_empty()
                || options.len() > usize::from(limits.interaction.max_tool_approval_options)
            {
                return Err(invalid_scalar());
            }
            for (expected_index, option) in options.iter().enumerate() {
                let option = option.as_object().ok_or_else(selected_wrong_json_type)?;
                let index = validate_u32_value(required(option, "optionIndex")?)?;
                if usize::try_from(index).ok() != Some(expected_index) {
                    return Err(invalid_scalar());
                }
                match required(option, "kind")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?
                {
                    "as_requested" | "restricted" => {}
                    _ => return Err(unknown_output_variant()),
                }
                validate_safe_string(
                    required(option, "label")?,
                    usize::from(limits.text.max_display_name_bytes),
                    false,
                )?;
                validate_requirement_summary(required(option, "effectiveRequirements")?, limits)?;
            }
            Ok(PendingInteractionRequestFacts {
                family: InteractionRequestFamilyFacts::ToolApproval,
                tool_name: Some(tool_name),
            })
        }
        "user_question" => {
            validate_optional_safe_string(
                data.get("title"),
                usize::from(limits.text.max_display_name_bytes),
            )?;
            let questions = required(data, "questions")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?;
            if questions.is_empty()
                || questions.len() > usize::from(limits.interaction.max_interaction_questions)
            {
                return Err(invalid_scalar());
            }
            let mut previous_index = None;
            for question in questions {
                let question = question.as_object().ok_or_else(selected_wrong_json_type)?;
                let index = validate_u32_value(required(question, "questionIndex")?)?;
                if previous_index.is_some_and(|previous| index <= previous) {
                    return Err(invalid_scalar());
                }
                previous_index = Some(index);
                validate_safe_string(
                    required(question, "prompt")?,
                    usize::try_from(limits.text.max_description_bytes).unwrap_or(usize::MAX),
                    false,
                )?;
                if !matches!(required(question, "required")?, JsonNode::Bool(_)) {
                    return Err(typed_wrong_json_type());
                }
                validate_question_input(required(question, "input")?, limits)?;
            }
            Ok(PendingInteractionRequestFacts {
                family: InteractionRequestFamilyFacts::UserQuestion,
                tool_name: None,
            })
        }
        _ => Err(unknown_output_variant()),
    }
}

fn validate_requirement_summary(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    for field in ["filesystem", "network", "process"] {
        if let Some(value) = object.get(field) {
            if !matches!(value, JsonNode::Null) {
                validate_safe_string(
                    value,
                    usize::try_from(limits.text.max_public_summary_bytes).unwrap_or(usize::MAX),
                    false,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_question_input(node: &JsonNode, limits: ProtocolLimits) -> Result<(), TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "text" => {
            let data = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            if matches!(required(data, "multiline")?, JsonNode::Bool(_)) {
                Ok(())
            } else {
                Err(typed_wrong_json_type())
            }
        }
        "single_choice" => {
            let data = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            let options = required(data, "options")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?;
            if options.is_empty()
                || options.len() > usize::from(limits.interaction.max_choices_per_question)
            {
                return Err(invalid_scalar());
            }
            let mut previous_index = None;
            for option in options {
                let option = option.as_object().ok_or_else(selected_wrong_json_type)?;
                let index = validate_u32_value(required(option, "optionIndex")?)?;
                if previous_index.is_some_and(|previous| index <= previous) {
                    return Err(invalid_scalar());
                }
                previous_index = Some(index);
                validate_safe_string(
                    required(option, "label")?,
                    usize::from(limits.text.max_display_name_bytes),
                    false,
                )?;
            }
            Ok(())
        }
        _ => Err(unknown_output_variant()),
    })
}

fn validate_safe_string(
    node: &JsonNode,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), TypedJsonError> {
    let value = node.as_str().ok_or_else(typed_wrong_json_type)?;
    validate_safe_text(value, maximum, allow_empty).map_err(|_| invalid_scalar())
}

struct QueueShapeFacts {
    minimal: bool,
    empty: bool,
    accepting: bool,
    command_ids: BTreeSet<CommandId>,
}

fn validate_session_queues(
    node: &JsonNode,
    limits: ProtocolLimits,
    execution: &str,
    current_turn_id: Option<TurnId>,
) -> Result<QueueShapeFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let submit = required(object, "submitAdmissions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if submit.len() > usize::from(limits.queues.max_submit_admissions) {
        return Err(invalid_scalar());
    }
    let mut command_ids = BTreeSet::new();
    let mut starting = 0_usize;
    for (index, value) in submit.iter().enumerate() {
        let value = value.as_object().ok_or_else(selected_wrong_json_type)?;
        let command_id: CommandId = parse_id(required(value, "commandId")?)?;
        if !command_ids.insert(command_id) {
            return Err(duplicate_value());
        }
        match required(value, "state")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?
        {
            "queued" => {}
            "starting" if index == 0 => starting = starting.saturating_add(1),
            "starting" => return Err(invalid_scalar()),
            _ => return Err(unknown_output_variant()),
        }
    }
    if starting > 1 {
        return Err(invalid_scalar());
    }
    if (execution == "starting") != (starting == 1) {
        return Err(invalid_scalar());
    }
    let steers = required(object, "steers")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if steers.len() > usize::from(limits.queues.max_steers) {
        return Err(invalid_scalar());
    }
    for value in steers {
        let value = value.as_object().ok_or_else(selected_wrong_json_type)?;
        let command_id: CommandId = parse_id(required(value, "commandId")?)?;
        if !command_ids.insert(command_id) {
            return Err(duplicate_value());
        }
        let expected_turn_id = parse_id(required(value, "expectedTurnId")?)?;
        if current_turn_id != Some(expected_turn_id) {
            return Err(invalid_scalar());
        }
    }
    let follow_ups = required(object, "followUps")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if follow_ups.len() > usize::from(limits.queues.max_follow_ups) {
        return Err(invalid_scalar());
    }
    for value in follow_ups {
        let value = value.as_object().ok_or_else(selected_wrong_json_type)?;
        let command_id: CommandId = parse_id(required(value, "commandId")?)?;
        if !command_ids.insert(command_id) {
            return Err(duplicate_value());
        }
    }
    let accepting = match required(object, "acceptingInput")? {
        JsonNode::Bool(value) => *value,
        _ => return Err(typed_wrong_json_type()),
    };
    Ok(QueueShapeFacts {
        minimal: submit.is_empty() && steers.is_empty() && follow_ups.is_empty() && accepting,
        empty: submit.is_empty() && steers.is_empty() && follow_ups.is_empty(),
        accepting,
        command_ids,
    })
}

fn validate_session_usage(node: &JsonNode) -> Result<bool, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let model_calls = validate_canonical_u64(required(object, "modelCalls")?)?;
    let compaction_calls = validate_canonical_u64(required(object, "compactionCalls")?)?;
    if model_calls > 1_000_000 || compaction_calls > 1_000_000 {
        return Err(invalid_scalar());
    }
    let mut has_tokens = false;
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
                has_tokens = true;
            }
        }
    }
    let costs = required(object, "reportedCosts")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    if costs.len() > 8 {
        return Err(invalid_scalar());
    }
    let mut currencies = Vec::with_capacity(costs.len());
    for cost in costs {
        let cost = cost.as_object().ok_or_else(selected_wrong_json_type)?;
        required(cost, "amount")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?
            .parse::<super::MoneyAmount>()
            .map_err(|_| invalid_scalar())?;
        let currency = required(cost, "currency")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?
            .parse::<super::CurrencyCode>()
            .map_err(|_| invalid_scalar())?;
        currencies.push(currency);
    }
    if currencies.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid_scalar());
    }
    Ok(model_calls == 0 && compaction_calls == 0 && !has_tokens && costs.is_empty())
}

fn validate_canonical_u64(node: &JsonNode) -> Result<u64, TypedJsonError> {
    let value = node
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<super::CanonicalU64>()
        .map_err(|_| invalid_scalar())?;
    Ok(value.get())
}

#[derive(Clone, Copy)]
enum DiagnosticScope {
    Runtime,
    Session,
}

fn validate_diagnostics(
    node: &JsonNode,
    limits: ProtocolLimits,
    scope: DiagnosticScope,
) -> Result<bool, TypedJsonError> {
    let diagnostics = node.as_array().ok_or_else(selected_wrong_json_type)?;
    if diagnostics.len() > usize::from(limits.observation.max_snapshot_diagnostics) {
        return Err(invalid_scalar());
    }
    for diagnostic in diagnostics {
        let diagnostic = diagnostic
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        let code = required(diagnostic, "code")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?;
        let message = required(diagnostic, "message")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?;
        match scope {
            DiagnosticScope::Runtime => {
                RuntimeDiagnosticView::new_with_limits(code, message, limits)
                    .map_err(|_| invalid_scalar())?;
            }
            DiagnosticScope::Session => {
                SessionDiagnosticView::new_with_limits(code, message, limits)
                    .map_err(|_| invalid_scalar())?;
            }
        }
    }
    Ok(diagnostics.is_empty())
}

fn has_current_recording_diagnostic(node: &JsonNode) -> Result<bool, TypedJsonError> {
    let first = node
        .as_array()
        .ok_or_else(selected_wrong_json_type)?
        .first();
    let Some(first) = first else {
        return Ok(false);
    };
    let code = required(
        first.as_object().ok_or_else(selected_wrong_json_type)?,
        "code",
    )?
    .as_str()
    .ok_or_else(typed_wrong_json_type)?;
    Ok(is_recording_diagnostic_code(code))
}

fn has_recording_diagnostic(node: &JsonNode) -> Result<bool, TypedJsonError> {
    for diagnostic in node.as_array().ok_or_else(selected_wrong_json_type)? {
        let code = required(
            diagnostic
                .as_object()
                .ok_or_else(selected_wrong_json_type)?,
            "code",
        )?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
        if is_recording_diagnostic_code(code) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_recording_diagnostic_code(code: &str) -> bool {
    matches!(
        code,
        "session_recording_initialization_failed"
            | "session_recording_encode_failed"
            | "session_recording_append_failed"
            | "session_recording_outcome_unknown"
    )
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
    item_id: Option<ItemId>,
    request_id: Option<RequestId>,
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
        item_id: None,
        request_id: None,
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
            facts.agent_id = Some(parse_id(required(data, "agentId")?)?);
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
                facts.item_id = Some(parse_id(required(data, "itemId")?)?);
            }
            if facts.family == EventRouteFamily::Interaction {
                facts.request_id = Some(parse_id(required(data, "requestId")?)?);
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
    let snapshot_node = required(object, "snapshot")?;
    let snapshot_pending = match validate_runtime_snapshot_shape(snapshot_node, limits) {
        Ok(()) => false,
        Err(error) if error.is_pending_public_target() => true,
        Err(error) => return Err(error),
    };
    let loaded_session_ids = runtime_loaded_session_ids(snapshot_node)?;
    let detail = object.get("detail");
    let detail_is_null = detail.is_none_or(|detail| matches!(detail, JsonNode::Null));
    match kind {
        "command_catalog_invalidated" => {
            if route.family != EventRouteFamily::Runtime || !detail_is_null {
                return Err(invalid_scalar());
            }
            Ok(snapshot_pending)
        }
        "agent_created"
        | "agent_definition_updated"
        | "agent_metadata_updated"
        | "agent_status_changed" => {
            let detail_agent = validate_runtime_agent_changed_detail(
                detail.ok_or_else(missing_required_field)?,
                limits,
            )?;
            let detail_agent = detail_agent.ok_or(TypedJsonError::EncodingInvariant)?;
            let status_matches = match kind {
                "agent_created" => {
                    detail_agent.status == AgentStatusFacts::Enabled
                        && detail_agent.definition_revision.get() == 1
                        && detail_agent.metadata_revision.get() == 1
                }
                "agent_definition_updated" | "agent_metadata_updated" => {
                    detail_agent.status != AgentStatusFacts::Deleted
                }
                "agent_status_changed" => true,
                _ => unreachable!(),
            };
            if route.family != EventRouteFamily::Agent
                || route.agent_id != Some(detail_agent.agent_id)
                || !status_matches
            {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "session_created"
        | "session_definition_updated"
        | "session_metadata_updated"
        | "session_archived"
        | "session_unarchived"
        | "session_deleted"
        | "session_forked" => {
            let detail_session = validate_runtime_session_changed_detail(
                detail.ok_or_else(missing_required_field)?,
                limits,
            )?;
            let detail_session = detail_session.ok_or(TypedJsonError::EncodingInvariant)?;
            let lifecycle_matches = match kind {
                "session_created" => {
                    detail_session.lifecycle == SessionLifecycleFacts::Open
                        && !detail_session.forked
                        && detail_session.definition_revision.get() == 1
                        && detail_session.metadata_revision.get() == 1
                }
                "session_definition_updated" => {
                    detail_session.lifecycle == SessionLifecycleFacts::Open
                }
                "session_metadata_updated" => {
                    detail_session.lifecycle != SessionLifecycleFacts::Deleted
                }
                "session_archived" => detail_session.lifecycle == SessionLifecycleFacts::Archived,
                "session_unarchived" => detail_session.lifecycle == SessionLifecycleFacts::Open,
                "session_deleted" => detail_session.lifecycle == SessionLifecycleFacts::Deleted,
                "session_forked" => {
                    detail_session.lifecycle == SessionLifecycleFacts::Open
                        && detail_session.forked
                        && detail_session.definition_revision.get() == 1
                        && detail_session.metadata_revision.get() == 1
                }
                _ => unreachable!(),
            };
            if route.family != EventRouteFamily::Session
                || route.session_id != Some(detail_session.session_id)
                || !lifecycle_matches
                || (matches!(
                    kind,
                    "session_created"
                        | "session_archived"
                        | "session_unarchived"
                        | "session_deleted"
                        | "session_forked"
                ) && loaded_session_ids.contains(&detail_session.session_id))
            {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "session_loaded" | "session_unloaded" => {
            let session_id = route.session_id.ok_or_else(invalid_scalar)?;
            let membership_matches = if kind == "session_loaded" {
                loaded_session_ids.contains(&session_id)
            } else {
                !loaded_session_ids.contains(&session_id)
            };
            if route.family != EventRouteFamily::Session || !detail_is_null || !membership_matches {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "diagnostics_updated" | "shared_resources_reloaded" => {
            if route.family != EventRouteFamily::Runtime || !detail_is_null {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        _ => Err(unknown_output_variant()),
    }
}

fn runtime_loaded_session_ids(node: &JsonNode) -> Result<BTreeSet<SessionId>, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let loaded = required(object, "loadedSessions")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?;
    loaded
        .iter()
        .map(|session| {
            parse_id(required(
                session.as_object().ok_or_else(selected_wrong_json_type)?,
                "sessionId",
            )?)
        })
        .collect()
}

fn validate_runtime_agent_changed_detail(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<Option<AgentSummaryFacts>, TypedJsonError> {
    let mut identity = None;
    validate_adjacent_output(node, |kind, data| {
        if kind != "agent_changed" {
            return Err(unknown_output_variant());
        }
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        identity = Some(validate_agent_summary(required(data, "agent")?, limits)?);
        Ok(())
    })?;
    Ok(identity)
}

fn validate_runtime_session_changed_detail(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<Option<SessionSummaryFacts>, TypedJsonError> {
    let mut identity = None;
    validate_adjacent_output(node, |kind, data| {
        if kind != "session_changed" {
            return Err(unknown_output_variant());
        }
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        identity = Some(validate_session_summary(
            required(data, "session")?,
            limits,
        )?);
        Ok(())
    })?;
    Ok(identity)
}

fn validate_agent_summary(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<AgentSummaryFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let agent_id = parse_id(required(object, "agentId")?)?;
    let definition_revision = parse_revision(required(object, "definitionRevision")?)?;
    let metadata = required(object, "metadata")?
        .as_object()
        .ok_or_else(selected_wrong_json_type)?;
    let metadata_revision = parse_revision(required(metadata, "revision")?)?;
    validate_safe_string(
        required(metadata, "name")?,
        usize::from(limits.text.max_display_name_bytes),
        false,
    )?;
    validate_optional_safe_string(
        metadata.get("description"),
        usize::try_from(limits.text.max_description_bytes).unwrap_or(usize::MAX),
    )?;
    validate_timestamp(required(metadata, "updatedAt")?)?;
    let status = match required(object, "status")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "enabled" => AgentStatusFacts::Enabled,
        "disabled" => AgentStatusFacts::Disabled,
        "deleted" => AgentStatusFacts::Deleted,
        _ => return Err(unknown_output_variant()),
    };
    validate_timestamp(required(object, "createdAt")?)?;
    Ok(AgentSummaryFacts {
        agent_id,
        definition_revision,
        metadata_revision,
        status,
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AgentStatusFacts {
    Enabled,
    Disabled,
    Deleted,
}

struct AgentSummaryFacts {
    agent_id: AgentId,
    definition_revision: AgentRevision,
    metadata_revision: AgentMetadataRevision,
    status: AgentStatusFacts,
}

fn validate_session_summary(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<SessionSummaryFacts, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let session_id = parse_id(required(object, "sessionId")?)?;
    let definition_revision = parse_revision(required(object, "definitionRevision")?)?;
    let metadata = required(object, "metadata")?;
    validate_session_metadata(metadata, limits)?;
    let metadata_revision = parse_revision(required(
        metadata.as_object().ok_or_else(selected_wrong_json_type)?,
        "revision",
    )?)?;
    let lifecycle = match required(object, "lifecycle")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
    {
        "open" => SessionLifecycleFacts::Open,
        "archived" => SessionLifecycleFacts::Archived,
        "deleted" => SessionLifecycleFacts::Deleted,
        _ => return Err(unknown_output_variant()),
    };
    let forked = match required(object, "forked")? {
        JsonNode::Bool(value) => *value,
        _ => return Err(typed_wrong_json_type()),
    };
    validate_timestamp(required(object, "createdAt")?)?;
    Ok(SessionSummaryFacts {
        session_id,
        definition_revision,
        metadata_revision,
        lifecycle,
        forked,
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SessionLifecycleFacts {
    Open,
    Archived,
    Deleted,
}

struct SessionSummaryFacts {
    session_id: SessionId,
    definition_revision: SessionDefinitionRevision,
    metadata_revision: SessionMetadataRevision,
    lifecycle: SessionLifecycleFacts,
    forked: bool,
}

fn validate_optional_safe_string(
    node: Option<&JsonNode>,
    maximum: usize,
) -> Result<(), TypedJsonError> {
    if let Some(value) = node {
        if !matches!(value, JsonNode::Null) {
            validate_safe_string(value, maximum, false)?;
        }
    }
    Ok(())
}

fn validate_timestamp(node: &JsonNode) -> Result<(), TypedJsonError> {
    node.as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map(|_| ())
        .map_err(|_| invalid_scalar())
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
    let snapshot = validate_session_snapshot_facts(required(object, "snapshot")?, limits)?;
    let detail = object.get("detail");
    let detail_is_null = detail.is_none_or(|detail| matches!(detail, JsonNode::Null));
    match kind {
        "turn_completed" | "turn_failed" | "turn_interrupted" => {
            let terminal =
                validate_turn_terminal_detail(detail.ok_or_else(missing_required_field)?)?;
            let expected = match kind {
                "turn_completed" => TerminalKind::Completed,
                "turn_failed" => TerminalKind::Failed,
                "turn_interrupted" => TerminalKind::Interrupted,
                _ => unreachable!(),
            };
            if terminal.terminal.kind != expected
                || route.family != EventRouteFamily::Turn
                || route.session_id != Some(snapshot.session_id)
                || route.turn_id != Some(terminal.turn_id)
                || snapshot.current_turn.as_ref().is_some_and(|current| {
                    current.turn_id != terminal.turn_id
                        || current.terminal.as_ref() != Some(&terminal.terminal)
                })
            {
                return Err(invalid_scalar());
            }
            Ok(snapshot.pending || expected == TerminalKind::Interrupted)
        }
        "session_definition_updated"
        | "session_metadata_updated"
        | "session_readiness_changed"
        | "session_execution_changed"
        | "session_settled"
        | "usage_updated"
        | "diagnostics_updated" => {
            require_session_route(route, snapshot.session_id)?;
            if !detail_is_null {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "session_workspace_reloaded" => {
            require_session_route(route, snapshot.session_id)?;
            if !detail_is_null || !snapshot.ready {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "session_recording_changed" => {
            require_session_route(route, snapshot.session_id)?;
            if !detail_is_null || snapshot.recording_healthy {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "turn_started" | "turn_phase_changed" => {
            if route.family != EventRouteFamily::Turn
                || route.session_id != Some(snapshot.session_id)
                || route.turn_id != snapshot.current_turn_id
                || snapshot.execution.as_ref() != "running"
                || snapshot
                    .current_turn
                    .as_ref()
                    .is_none_or(|current| current.terminal.is_some())
                || !detail_is_null
            {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "item_completed"
        | "item_tool_invocation_started"
        | "item_tool_invocation_completed"
        | "item_tool_invocation_abandoned" => {
            let item =
                validate_item_changed_detail(detail.ok_or_else(missing_required_field)?, limits)?;
            let kind_matches = match kind {
                "item_completed" => {
                    item.status == ItemStatusFacts::Completed
                        && item.family != ItemFamilyFacts::ToolInvocation
                }
                "item_tool_invocation_started" => {
                    item.status == ItemStatusFacts::Started
                        && item.family == ItemFamilyFacts::ToolInvocation
                }
                "item_tool_invocation_completed" => {
                    item.status == ItemStatusFacts::Completed
                        && item.family == ItemFamilyFacts::ToolInvocation
                }
                "item_tool_invocation_abandoned" => {
                    item.status == ItemStatusFacts::Abandoned
                        && item.family == ItemFamilyFacts::ToolInvocation
                }
                _ => unreachable!(),
            };
            if route.family != EventRouteFamily::Item
                || route.session_id != Some(snapshot.session_id)
                || route.turn_id != Some(item.turn_id)
                || route.item_id != Some(item.item_id)
                || snapshot
                    .active_items
                    .get(&item.item_id)
                    .map(|snapshot_item| &snapshot_item.projection)
                    != Some(&item.projection)
                || !kind_matches
            {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "interaction_requested" => {
            if route.family != EventRouteFamily::Interaction
                || route.session_id != Some(snapshot.session_id)
                || snapshot
                    .pending_interactions
                    .get(&route.request_id.ok_or_else(invalid_scalar)?)
                    .is_none_or(|interaction| {
                        Some(interaction.turn_id) != route.turn_id
                            || Some(interaction.item_id) != route.item_id
                    })
                || !detail_is_null
            {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "interaction_resolved" => {
            let resolution = validate_interaction_resolved_detail(
                detail.ok_or_else(missing_required_field)?,
                limits,
            )?;
            let matching_item = route.item_id.is_some_and(|item_id| {
                snapshot.active_items.get(&item_id).is_some_and(|item| {
                    item.status == ItemStatusFacts::Started
                        && item.family == ItemFamilyFacts::ToolInvocation
                        && match resolution.family {
                            InteractionResolutionFamilyFacts::UserAnswer => item
                                .tool_name
                                .as_ref()
                                .is_some_and(|tool_name| tool_name.as_str() == "ask_user"),
                            InteractionResolutionFamilyFacts::ToolApproval => item
                                .tool_name
                                .as_ref()
                                .is_some_and(|tool_name| tool_name.as_str() != "ask_user"),
                            InteractionResolutionFamilyFacts::Cancelled => true,
                        }
                })
            });
            if route.family != EventRouteFamily::Interaction
                || route.session_id != Some(snapshot.session_id)
                || route.request_id != Some(resolution.request_id)
                || snapshot
                    .pending_interactions
                    .contains_key(&resolution.request_id)
                || route.turn_id != snapshot.current_turn_id
                || !matching_item
            {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        "queue_updated" => {
            require_session_route(route, snapshot.session_id)?;
            let removed =
                validate_queue_updated_detail(detail.ok_or_else(missing_required_field)?, limits)?;
            if removed
                .iter()
                .any(|command_id| snapshot.queue_command_ids.contains(command_id))
            {
                return Err(invalid_scalar());
            }
            Ok(true)
        }
        _ => Err(unknown_output_variant()),
    }
}

fn require_session_route(
    route: EventRouteFacts,
    session_id: SessionId,
) -> Result<(), TypedJsonError> {
    if route.family == EventRouteFamily::Session && route.session_id == Some(session_id) {
        Ok(())
    } else {
        Err(invalid_scalar())
    }
}

fn validate_item_changed_detail(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<ItemChangedFacts, TypedJsonError> {
    let mut identity = None;
    validate_adjacent_output(node, |kind, data| {
        if kind != "item_changed" {
            return Err(unknown_output_variant());
        }
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        let item = validate_pending_item_identity(required(data, "item")?, limits)?;
        identity = Some(ItemChangedFacts {
            turn_id: item.turn_id,
            item_id: item.item_id,
            projection: item.projection,
            status: item.status,
            family: item.family,
        });
        Ok(())
    })?;
    identity.ok_or(TypedJsonError::EncodingInvariant)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ItemStatusFacts {
    Started,
    Completed,
    Abandoned,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ItemFamilyFacts {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolInvocation,
}

struct PendingItemContentFacts {
    family: ItemFamilyFacts,
    tool_result: bool,
    tool_name: Option<ToolName>,
    user_source: Option<UserMessageSourceFacts>,
    agent_disposition: Option<AgentDispositionFacts>,
}

impl PendingItemContentFacts {
    const fn non_tool(family: ItemFamilyFacts) -> Self {
        Self {
            family,
            tool_result: false,
            tool_name: None,
            user_source: None,
            agent_disposition: None,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UserMessageSourceFacts {
    Input,
    Steer,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AgentDispositionFacts {
    Intermediate,
    Final,
}

#[derive(Clone)]
struct ActiveItemShapeFacts {
    turn_id: TurnId,
    item_id: ItemId,
    projection: JsonNode,
    status: ItemStatusFacts,
    family: ItemFamilyFacts,
    tool_name: Option<ToolName>,
    user_source: Option<UserMessageSourceFacts>,
    agent_disposition: Option<AgentDispositionFacts>,
}

struct ItemChangedFacts {
    turn_id: TurnId,
    item_id: ItemId,
    projection: JsonNode,
    status: ItemStatusFacts,
    family: ItemFamilyFacts,
}

fn validate_interaction_resolved_detail(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<InteractionResolvedDetailFacts, TypedJsonError> {
    let mut request_id = None;
    let mut family = None;
    validate_adjacent_output(node, |kind, data| {
        if kind != "interaction_resolved" {
            return Err(unknown_output_variant());
        }
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        request_id = Some(parse_id(required(data, "requestId")?)?);
        family = Some(validate_interaction_resolution(
            required(data, "resolution")?,
            limits,
        )?);
        Ok(())
    })?;
    Ok(InteractionResolvedDetailFacts {
        request_id: request_id.ok_or(TypedJsonError::EncodingInvariant)?,
        family: family.ok_or(TypedJsonError::EncodingInvariant)?,
    })
}

#[derive(Clone, Copy)]
enum InteractionResolutionFamilyFacts {
    ToolApproval,
    UserAnswer,
    Cancelled,
}

struct InteractionResolvedDetailFacts {
    request_id: RequestId,
    family: InteractionResolutionFamilyFacts,
}

fn validate_interaction_resolution(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<InteractionResolutionFamilyFacts, TypedJsonError> {
    validate_adjacent_output(node, |kind, data| match kind {
        "tool_approval" => {
            validate_adjacent_output(
                data.ok_or_else(missing_required_field)?,
                |decision, data| match decision {
                    "denied" => {
                        if data.is_some() {
                            Err(selected_wrong_json_type())
                        } else {
                            Ok(())
                        }
                    }
                    "allowed" => {
                        let data = data
                            .ok_or_else(missing_required_field)?
                            .as_object()
                            .ok_or_else(selected_wrong_json_type)?;
                        validate_u32(required(data, "optionIndex")?)?;
                        match required(data, "kind")?
                            .as_str()
                            .ok_or_else(typed_wrong_json_type)?
                        {
                            "as_requested" | "restricted" => Ok(()),
                            _ => Err(unknown_output_variant()),
                        }
                    }
                    _ => Err(unknown_output_variant()),
                },
            )?;
            Ok(InteractionResolutionFamilyFacts::ToolApproval)
        }
        "user_answer" => {
            let data_node = data.ok_or_else(missing_required_field)?;
            let data = data_node.as_object().ok_or_else(selected_wrong_json_type)?;
            let answers = required(data, "answers")?
                .as_array()
                .ok_or_else(selected_wrong_json_type)?;
            if answers.len() > usize::from(limits.interaction.max_interaction_questions) {
                return Err(invalid_scalar());
            }
            let mut previous_index = None;
            let mut text_bytes = 0_usize;
            for answer in answers {
                let answer = answer.as_object().ok_or_else(selected_wrong_json_type)?;
                let index = validate_u32_value(required(answer, "questionIndex")?)?;
                if previous_index.is_some_and(|previous| index <= previous) {
                    return Err(invalid_scalar());
                }
                previous_index = Some(index);
                validate_adjacent_output(required(answer, "value")?, |kind, data| match kind {
                    "text" => {
                        let text = data
                            .ok_or_else(missing_required_field)?
                            .as_str()
                            .ok_or_else(typed_wrong_json_type)?;
                        validate_safe_text(
                            text,
                            usize::try_from(limits.interaction.max_answer_text_bytes)
                                .unwrap_or(usize::MAX),
                            true,
                        )
                        .map_err(|_| invalid_scalar())?;
                        text_bytes = text_bytes
                            .checked_add(text.len())
                            .ok_or_else(invalid_scalar)?;
                        if text_bytes
                            > usize::try_from(limits.interaction.max_interaction_answer_bytes)
                                .unwrap_or(usize::MAX)
                        {
                            return Err(invalid_scalar());
                        }
                        Ok(())
                    }
                    "choice" => {
                        let data = data
                            .ok_or_else(missing_required_field)?
                            .as_object()
                            .ok_or_else(selected_wrong_json_type)?;
                        validate_u32(required(data, "optionIndex")?)
                    }
                    _ => Err(unknown_output_variant()),
                })?;
            }
            let projection = project_user_answer_data(data_node)?;
            if public_node_encoded_len(&projection).ok_or_else(invalid_scalar)?
                > usize::try_from(limits.interaction.max_interaction_answer_bytes)
                    .unwrap_or(usize::MAX)
            {
                return Err(invalid_scalar());
            }
            Ok(InteractionResolutionFamilyFacts::UserAnswer)
        }
        "cancelled" => {
            let data = data
                .ok_or_else(missing_required_field)?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            let reason = required(data, "reason")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?;
            if matches!(
                reason,
                "host_cancelled"
                    | "turn_cancelled"
                    | "security_revoked"
                    | "session_unloaded"
                    | "runtime_closing"
                    | "turn_terminal"
            ) {
                Ok(InteractionResolutionFamilyFacts::Cancelled)
            } else {
                Err(unknown_output_variant())
            }
        }
        _ => Err(unknown_output_variant()),
    })
}

fn project_user_answer_data(node: &JsonNode) -> Result<JsonNode, TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    let answers = required(object, "answers")?
        .as_array()
        .ok_or_else(selected_wrong_json_type)?
        .iter()
        .map(|answer| {
            let answer = answer.as_object().ok_or_else(selected_wrong_json_type)?;
            let value = required(answer, "value")?
                .as_object()
                .ok_or_else(selected_wrong_json_type)?;
            let kind = required(value, "type")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?;
            let data = required(value, "data")?;
            let data = match kind {
                "text" => data.clone(),
                "choice" => {
                    let data = data.as_object().ok_or_else(selected_wrong_json_type)?;
                    json_node_object(vec![(
                        "optionIndex",
                        required(data, "optionIndex")?.clone(),
                    )])
                }
                _ => return Err(unknown_output_variant()),
            };
            Ok(json_node_object(vec![
                ("questionIndex", required(answer, "questionIndex")?.clone()),
                (
                    "value",
                    json_node_object(vec![
                        ("type", JsonNode::String(kind.into())),
                        ("data", data),
                    ]),
                ),
            ]))
        })
        .collect::<Result<Vec<_>, TypedJsonError>>()?;
    Ok(json_node_object(vec![(
        "answers",
        JsonNode::Array(answers),
    )]))
}

fn validate_queue_updated_detail(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<BTreeSet<CommandId>, TypedJsonError> {
    let mut removed_ids = None;
    validate_adjacent_output(node, |kind, data| {
        if kind != "queue_updated" {
            return Err(unknown_output_variant());
        }
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        let removed = required(data, "removedCommandIds")?
            .as_array()
            .ok_or_else(selected_wrong_json_type)?;
        let maximum = usize::from(limits.queues.max_submit_admissions)
            + usize::from(limits.queues.max_steers)
            + usize::from(limits.queues.max_follow_ups);
        if removed.len() > maximum {
            return Err(invalid_scalar());
        }
        let mut command_ids = BTreeSet::new();
        for command_id in removed {
            let command_id: CommandId = parse_id(command_id)?;
            if !command_ids.insert(command_id) {
                return Err(duplicate_value());
            }
        }
        match required(data, "reason")?
            .as_str()
            .ok_or_else(typed_wrong_json_type)?
        {
            "cancel_queued_message" | "turn_cancelled" | "turn_terminal" | "prepare_for_unload" => {
            }
            _ => return Err(unknown_output_variant()),
        }
        removed_ids = Some(command_ids);
        Ok(())
    })?;
    removed_ids.ok_or(TypedJsonError::EncodingInvariant)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TerminalKind {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Eq, PartialEq)]
struct TerminalCorrelationFacts {
    kind: TerminalKind,
    completed_at: Timestamp,
    reason: Option<Box<str>>,
}

#[derive(Clone)]
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
                let completed_at = required(data, "completedAt")?
                    .as_str()
                    .ok_or_else(typed_wrong_json_type)?
                    .parse::<Timestamp>()
                    .map_err(|_| invalid_scalar())?;
                let reason = match terminal {
                    "completed" => None,
                    "failed" => {
                        validate_turn_failure(required(data, "reason")?)?;
                        Some(Box::<str>::from(
                            required(data, "reason")?
                                .as_str()
                                .ok_or_else(typed_wrong_json_type)?,
                        ))
                    }
                    "interrupted" => {
                        validate_turn_interruption(required(data, "reason")?)?;
                        Some(Box::<str>::from(
                            required(data, "reason")?
                                .as_str()
                                .ok_or_else(typed_wrong_json_type)?,
                        ))
                    }
                    _ => return Err(unknown_output_variant()),
                };
                let kind = match terminal {
                    "completed" => TerminalKind::Completed,
                    "failed" => TerminalKind::Failed,
                    "interrupted" => TerminalKind::Interrupted,
                    _ => return Err(unknown_output_variant()),
                };
                selected = Some(TerminalCorrelationFacts {
                    kind,
                    completed_at,
                    reason,
                });
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

fn validate_progress_event_shape(
    node: &JsonNode,
    limits: ProtocolLimits,
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or_else(selected_wrong_json_type)?;
    required(object, "timestamp")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?
        .parse::<Timestamp>()
        .map_err(|_| invalid_scalar())?;
    let route = validate_event_route(required(object, "route")?)?;
    let kind = required(object, "kind")?
        .as_str()
        .ok_or_else(typed_wrong_json_type)?;
    if !matches!(kind, "model" | "tool" | "compaction" | "retry") {
        return Err(unknown_output_variant());
    }
    let update = validate_adjacent_output(required(object, "update")?, |update, data| {
        let data = data
            .ok_or_else(missing_required_field)?
            .as_object()
            .ok_or_else(selected_wrong_json_type)?;
        validate_progress_update(update, data, limits)
    })?;
    let coherent = match update {
        ProgressUpdateFacts::Item { item_id } => {
            kind == "model"
                && route.family == EventRouteFamily::Item
                && route.item_id == Some(item_id)
        }
        ProgressUpdateFacts::ToolOutput { item_id } => {
            kind == "tool"
                && route.family == EventRouteFamily::Item
                && route.item_id == Some(item_id)
        }
        ProgressUpdateFacts::Retry => kind == "retry" && route.family == EventRouteFamily::Turn,
        ProgressUpdateFacts::Operation => {
            kind == "compaction"
                && matches!(
                    route.family,
                    EventRouteFamily::Session | EventRouteFamily::Turn
                )
        }
    };
    if coherent {
        Ok(())
    } else {
        Err(invalid_scalar())
    }
}

enum ProgressUpdateFacts {
    Item { item_id: ItemId },
    ToolOutput { item_id: ItemId },
    Retry,
    Operation,
}

fn validate_progress_update(
    kind: &str,
    data: &std::collections::BTreeMap<Box<str>, JsonNode>,
    limits: ProtocolLimits,
) -> Result<ProgressUpdateFacts, TypedJsonError> {
    let maximum = usize::try_from(limits.transport.max_progress_event_bytes).unwrap_or(usize::MAX);
    match kind {
        "item_started" | "item_delta" => {
            let item_id = parse_id(required(data, "itemId")?)?;
            validate_u32(required(data, "contentIndex")?)?;
            match required(data, "contentKind")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
            {
                "assistant_text" | "reasoning" => {}
                _ => return Err(unknown_output_variant()),
            }
            if kind == "item_delta" {
                validate_safe_string(required(data, "delta")?, maximum, true)?;
            }
            Ok(ProgressUpdateFacts::Item { item_id })
        }
        "tool_output_delta" => {
            let item_id = parse_id(required(data, "itemId")?)?;
            required(data, "toolCallId")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
                .parse::<ToolCallId>()
                .map_err(|_| invalid_scalar())?;
            validate_safe_string(required(data, "delta")?, maximum, true)?;
            Ok(ProgressUpdateFacts::ToolOutput { item_id })
        }
        "model_retry_scheduled" => {
            let maximum = match required(data, "purpose")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
            {
                "agent_run" => 3,
                "compaction_summary" => 1,
                _ => return Err(unknown_output_variant()),
            };
            let retry_count = validate_u32_value(required(data, "retryCount")?)?;
            if retry_count > maximum {
                return Err(invalid_scalar());
            }
            required(data, "readyAt")?
                .as_str()
                .ok_or_else(typed_wrong_json_type)?
                .parse::<Timestamp>()
                .map_err(|_| invalid_scalar())?;
            Ok(ProgressUpdateFacts::Retry)
        }
        "operation_status" => {
            validate_safe_string(required(data, "message")?, maximum, false)?;
            Ok(ProgressUpdateFacts::Operation)
        }
        _ => Err(unknown_output_variant()),
    }
}

fn validate_u32_value(node: &JsonNode) -> Result<u32, TypedJsonError> {
    let literal = node
        .as_number()
        .map(|number| number.raw())
        .ok_or_else(typed_wrong_json_type)?;
    if literal.bytes().all(|byte| byte.is_ascii_digit()) {
        literal.parse::<u32>().map_err(|_| invalid_scalar())
    } else {
        Err(invalid_scalar())
    }
}

fn validate_u32(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_u32_value(node).map(|_| ())
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
