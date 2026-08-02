use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::runtime_interface::{
    CommandRequest, QueryResponse, QueryResult, RuntimeCapabilities, RuntimeCommand,
    RuntimeDispatchError, RuntimeLifecycleCommand, RuntimeQuery, RuntimeQueryResult,
    RuntimeReadQuery, RuntimeRequest, SnapshotRequest, SubscriptionRequest, SubscriptionScope,
};

use super::bounded_json::JsonNode;
use super::limits::{CapabilityToken, runtime_capability_from_token};
use super::scalar::{CommandId, SessionId};
use super::typed_json::{
    PublicDecodeCode, PublicDecodeError, PublicDecodeStage, PublicJsonKind, TypedJsonError,
    WireV1Codec,
};

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
    let decoded: CommandRequestInput = codec.decode_with_shape(
        PublicJsonKind::Request,
        input,
        validate_command_request_shape,
    )?;
    Ok(CommandRequest::new(
        decoded.command_id,
        decoded.command.into_semantic(),
    ))
}

fn encode_command_request(
    codec: &WireV1Codec,
    request: &CommandRequest,
) -> Result<Vec<u8>, TypedJsonError> {
    codec.encode(
        PublicJsonKind::Request,
        &CommandRequestOutput {
            command_id: request.command_id(),
            command: RuntimeCommandOutput::from_semantic(request.command()),
        },
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
}

impl RuntimeCommandInput {
    fn into_semantic(self) -> RuntimeCommand {
        match self {
            Self::Runtime(RuntimeLifecycleCommandInput::ReloadSharedResources) => {
                RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeLifecycleCommandInput {
    ReloadSharedResources,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandRequestOutput {
    command_id: CommandId,
    command: RuntimeCommandOutput,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RuntimeCommandOutput {
    Runtime(RuntimeLifecycleCommandOutput),
}

impl RuntimeCommandOutput {
    fn from_semantic(value: &RuntimeCommand) -> Self {
        match value {
            RuntimeCommand::Runtime(RuntimeLifecycleCommand::ReloadSharedResources) => {
                Self::Runtime(RuntimeLifecycleCommandOutput::ReloadSharedResources)
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeLifecycleCommandOutput {
    ReloadSharedResources,
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

fn validate_command_request_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
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
        "agent" | "session" | "turn" | "interaction" | "command_surface" => {
            Err(TypedJsonError::PendingPublicTarget)
        }
        _ => Err(unknown_input_variant()),
    })
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
