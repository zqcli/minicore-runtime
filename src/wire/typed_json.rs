use std::io::{self, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::bounded_json::{BoundedJsonError, JsonNode, JsonParseLimits, parse_node};
use super::limits::{
    CapabilityToken, ClientInfo, ProtocolBootstrapResponse, ProtocolHello, ProtocolLimits,
    ProtocolReject, ProtocolRejectReason, ProtocolVersion, ProtocolWelcome, RuntimeCapabilities,
    RuntimeInfo, is_v1_runtime_capability, protocol_hello_is_valid,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicJsonKind {
    Request,
    Response,
    RuntimeSnapshot,
    SessionSnapshot,
    StateEvent,
    ProgressEvent,
}

impl PublicJsonKind {
    fn maximum_bytes(self, limits: ProtocolLimits) -> usize {
        let transport = limits.transport;
        match self {
            Self::Request => transport.max_request_bytes as usize,
            Self::Response => transport.max_response_bytes as usize,
            Self::RuntimeSnapshot => transport.max_runtime_snapshot_bytes as usize,
            Self::SessionSnapshot => transport.max_session_snapshot_bytes as usize,
            Self::StateEvent => transport.max_state_event_bytes as usize,
            Self::ProgressEvent => transport.max_progress_event_bytes as usize,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TypedJsonError {
    #[error("selected protocol version is unsupported")]
    UnsupportedSelectedVersion,
    #[error("bounded JSON preflight failed")]
    Json(#[from] BoundedJsonError),
    #[error("public JSON decode failed")]
    PublicDecode(#[from] PublicDecodeError),
    #[error("typed JSON shape is invalid")]
    TypedShape,
    #[error("typed JSON output exceeds its frame limit")]
    FrameTooLarge,
    #[error("typed JSON output violated a codec invariant")]
    EncodingInvariant,
    #[error("bootstrap response does not match the selected protocol version")]
    SelectedVersionMismatch,
    #[error("effective protocol limits exceed the selected version hard maxima")]
    InvalidProtocolLimits,
}

impl TypedJsonError {
    pub const fn public_decode_error(self) -> Option<PublicDecodeError> {
        match self {
            Self::Json(BoundedJsonError::DuplicateKey) => Some(PublicDecodeError::new(
                PublicDecodeStage::JsonStructure,
                PublicDecodeCode::DuplicateKey,
            )),
            Self::PublicDecode(error) => Some(error),
            Self::UnsupportedSelectedVersion
            | Self::Json(_)
            | Self::TypedShape
            | Self::FrameTooLarge
            | Self::EncodingInvariant
            | Self::SelectedVersionMismatch
            | Self::InvalidProtocolLimits => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PublicDecodeStage {
    JsonStructure,
    SelectedSchema,
    TypedScalar,
}

impl PublicDecodeStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JsonStructure => "json_structure",
            Self::SelectedSchema => "selected_schema",
            Self::TypedScalar => "typed_scalar",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PublicDecodeCode {
    DuplicateKey,
    UnknownInputField,
    UnknownInputVariant,
    UnknownOutputVariant,
    WrongJsonType,
    NoncanonicalId,
    DurationOutOfRange,
}

impl PublicDecodeCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateKey => "duplicate_key",
            Self::UnknownInputField => "unknown_input_field",
            Self::UnknownInputVariant => "unknown_input_variant",
            Self::UnknownOutputVariant => "unknown_output_variant",
            Self::WrongJsonType => "wrong_json_type",
            Self::NoncanonicalId => "noncanonical_id",
            Self::DurationOutOfRange => "duration_out_of_range",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, Hash, PartialEq)]
#[error("public JSON value violates the selected schema")]
pub struct PublicDecodeError {
    stage: PublicDecodeStage,
    code: PublicDecodeCode,
}

impl PublicDecodeError {
    pub(crate) const fn new(stage: PublicDecodeStage, code: PublicDecodeCode) -> Self {
        Self { stage, code }
    }

    pub const fn stage(self) -> PublicDecodeStage {
        self.stage
    }

    pub const fn code(self) -> PublicDecodeCode {
        self.code
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireV1Codec {
    selected_version: ProtocolVersion,
    limits: ProtocolLimits,
}

impl WireV1Codec {
    pub fn new(
        selected_version: ProtocolVersion,
        limits: ProtocolLimits,
    ) -> Result<Self, TypedJsonError> {
        if selected_version != ProtocolVersion::V1_0 {
            return Err(TypedJsonError::UnsupportedSelectedVersion);
        }
        if !limits.is_within_v1_hard_maxima() {
            return Err(TypedJsonError::InvalidProtocolLimits);
        }
        Ok(Self {
            selected_version,
            limits,
        })
    }

    pub fn v1_0() -> Self {
        Self {
            selected_version: ProtocolVersion::V1_0,
            limits: ProtocolLimits::v1_0(),
        }
    }

    pub const fn selected_version(&self) -> ProtocolVersion {
        self.selected_version
    }

    pub const fn limits(&self) -> ProtocolLimits {
        self.limits
    }

    pub fn preflight(&self, kind: PublicJsonKind, input: &[u8]) -> Result<(), TypedJsonError> {
        self.parse(kind, input)?;
        Ok(())
    }

    fn parse(&self, kind: PublicJsonKind, input: &[u8]) -> Result<JsonNode, TypedJsonError> {
        let limits = JsonParseLimits::public(kind.maximum_bytes(self.limits), self.limits);
        parse_node(input, limits).map_err(Into::into)
    }

    fn decode_with_shape<T: DeserializeOwned>(
        &self,
        kind: PublicJsonKind,
        input: &[u8],
        validate_shape: impl FnOnce(&JsonNode) -> Result<(), TypedJsonError>,
    ) -> Result<T, TypedJsonError> {
        let node = self.parse(kind, input)?;
        validate_shape(&node)?;
        drop(node);
        serde_json::from_slice(input).map_err(|_| TypedJsonError::TypedShape)
    }

    fn encode<T: Serialize>(
        &self,
        kind: PublicJsonKind,
        value: &T,
    ) -> Result<Vec<u8>, TypedJsonError> {
        let mut writer = BoundedVecWriter::new(kind.maximum_bytes(self.limits));
        let encoded = serde_json::to_writer(&mut writer, value);
        if writer.limit_exceeded {
            return Err(TypedJsonError::FrameTooLarge);
        }
        encoded.map_err(|_| TypedJsonError::EncodingInvariant)?;
        self.preflight(kind, &writer.bytes)
            .map_err(|_| TypedJsonError::EncodingInvariant)?;
        Ok(writer.bytes)
    }
}

pub fn decode_protocol_hello_v1(input: &[u8]) -> Result<ProtocolHello, TypedJsonError> {
    let decoded = WireV1Codec::v1_0().decode_with_shape::<ProtocolHelloInput>(
        PublicJsonKind::Request,
        input,
        validate_protocol_hello_shape,
    )?;
    Ok(ProtocolHello::new(
        decoded
            .supported_versions
            .into_iter()
            .map(ProtocolVersionInput::into_semantic)
            .collect(),
        ClientInfo::new(decoded.client.name, decoded.client.version),
        decoded.capabilities.values,
    ))
}

pub fn encode_protocol_hello_v1(hello: &ProtocolHello) -> Result<Vec<u8>, TypedJsonError> {
    if !protocol_hello_is_valid(hello) {
        return Err(TypedJsonError::TypedShape);
    }
    let output = ProtocolHelloOutput {
        supported_versions: hello.supported_versions(),
        client: ClientInfoOutput {
            name: hello.client().name(),
            version: hello.client().version(),
        },
        capabilities: CapabilityValuesOutput {
            values: hello.capabilities(),
        },
    };
    WireV1Codec::v1_0().encode(PublicJsonKind::Request, &output)
}

pub fn decode_protocol_bootstrap_response_v1(
    input: &[u8],
) -> Result<ProtocolBootstrapResponse, TypedJsonError> {
    let codec = WireV1Codec::v1_0();
    let decoded: ProtocolBootstrapResponseInput =
        codec.decode_with_shape(PublicJsonKind::Response, input, |node| {
            validate_bootstrap_response_shape(node)
        })?;
    let response = decoded.into_semantic()?;
    validate_bootstrap_response(codec.selected_version, &response)?;
    Ok(response)
}

pub fn encode_protocol_bootstrap_response_v1(
    response: &ProtocolBootstrapResponse,
) -> Result<Vec<u8>, TypedJsonError> {
    let codec = WireV1Codec::v1_0();
    validate_bootstrap_response(codec.selected_version, response)?;
    codec.encode(PublicJsonKind::Response, response)
}

#[derive(Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code, reason = "consumed by mixed public enums"))]
enum UnknownFieldPolicy {
    Reject,
    Ignore,
}

#[derive(Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code, reason = "consumed by mixed public enums"))]
enum AdjacentPayload {
    Unit,
    Required,
}

#[derive(Clone, Copy)]
struct AdjacentVariant {
    name: &'static str,
    payload: AdjacentPayload,
}

impl AdjacentVariant {
    #[cfg_attr(not(test), allow(dead_code, reason = "consumed by mixed public enums"))]
    const fn unit(name: &'static str) -> Self {
        Self {
            name,
            payload: AdjacentPayload::Unit,
        }
    }

    const fn payload(name: &'static str) -> Self {
        Self {
            name,
            payload: AdjacentPayload::Required,
        }
    }
}

fn validate_adjacent_enum(
    node: &JsonNode,
    unknown_fields: UnknownFieldPolicy,
    variants: &[AdjacentVariant],
) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or(TypedJsonError::TypedShape)?;
    if matches!(unknown_fields, UnknownFieldPolicy::Reject)
        && object
            .keys()
            .any(|key| key.as_ref() != "type" && key.as_ref() != "data")
    {
        return Err(TypedJsonError::TypedShape);
    }
    let name = object
        .get("type")
        .and_then(JsonNode::as_str)
        .ok_or(TypedJsonError::TypedShape)?;
    let variant = variants
        .iter()
        .find(|variant| variant.name == name)
        .ok_or(TypedJsonError::TypedShape)?;
    match variant.payload {
        AdjacentPayload::Unit if object.contains_key("data") => Err(TypedJsonError::TypedShape),
        AdjacentPayload::Required if !object.contains_key("data") => {
            Err(TypedJsonError::TypedShape)
        }
        AdjacentPayload::Unit | AdjacentPayload::Required => Ok(()),
    }
}

fn validate_protocol_hello_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or(TypedJsonError::TypedShape)?;
    reject_unknown_input_fields(
        object.keys().map(AsRef::as_ref),
        &["supportedVersions", "client", "capabilities"],
    )?;
    let versions = object
        .get("supportedVersions")
        .and_then(JsonNode::as_array)
        .ok_or(TypedJsonError::TypedShape)?;
    for version in versions {
        let version = version.as_object().ok_or(TypedJsonError::TypedShape)?;
        reject_unknown_input_fields(version.keys().map(AsRef::as_ref), &["major", "minor"])?;
        validate_unsigned_integer(version.get("major").ok_or(TypedJsonError::TypedShape)?)?;
        validate_unsigned_integer(version.get("minor").ok_or(TypedJsonError::TypedShape)?)?;
    }
    let client = object
        .get("client")
        .and_then(JsonNode::as_object)
        .ok_or(TypedJsonError::TypedShape)?;
    reject_unknown_input_fields(client.keys().map(AsRef::as_ref), &["name", "version"])?;
    let capabilities = object
        .get("capabilities")
        .and_then(JsonNode::as_object)
        .ok_or(TypedJsonError::TypedShape)?;
    reject_unknown_input_fields(capabilities.keys().map(AsRef::as_ref), &["values"])?;
    Ok(())
}

fn reject_unknown_input_fields<'a>(
    fields: impl IntoIterator<Item = &'a str>,
    allowed: &[&str],
) -> Result<(), TypedJsonError> {
    if fields.into_iter().any(|field| !allowed.contains(&field)) {
        return Err(PublicDecodeError::new(
            PublicDecodeStage::SelectedSchema,
            PublicDecodeCode::UnknownInputField,
        )
        .into());
    }
    Ok(())
}

fn validate_bootstrap_response_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    validate_adjacent_enum(
        node,
        UnknownFieldPolicy::Ignore,
        &[
            AdjacentVariant::payload("welcome"),
            AdjacentVariant::payload("reject"),
        ],
    )?;
    let object = node.as_object().ok_or(TypedJsonError::TypedShape)?;
    let variant = object
        .get("type")
        .and_then(JsonNode::as_str)
        .ok_or(TypedJsonError::TypedShape)?;
    let data = object.get("data").ok_or(TypedJsonError::TypedShape)?;
    let data = data.as_object().ok_or(TypedJsonError::TypedShape)?;
    match variant {
        "welcome" => {
            validate_protocol_version_shape(
                data.get("selectedVersion")
                    .ok_or(TypedJsonError::TypedShape)?,
            )?;
            let runtime = data
                .get("runtime")
                .and_then(JsonNode::as_object)
                .ok_or(TypedJsonError::TypedShape)?;
            validate_protocol_version_shape(
                runtime
                    .get("protocolVersion")
                    .ok_or(TypedJsonError::TypedShape)?,
            )?;
            let limit_shape = serde_json::to_value(ProtocolLimits::v1_0())
                .map_err(|_| TypedJsonError::EncodingInvariant)?;
            validate_numbers_matching_shape(
                data.get("limits").ok_or(TypedJsonError::TypedShape)?,
                &limit_shape,
            )
        }
        "reject" => {
            let versions = data
                .get("supportedVersions")
                .and_then(JsonNode::as_array)
                .ok_or(TypedJsonError::TypedShape)?;
            for version in versions {
                validate_protocol_version_shape(version)?;
            }
            Ok(())
        }
        _ => Err(TypedJsonError::TypedShape),
    }
}

fn validate_protocol_version_shape(node: &JsonNode) -> Result<(), TypedJsonError> {
    let object = node.as_object().ok_or(TypedJsonError::TypedShape)?;
    validate_unsigned_integer(object.get("major").ok_or(TypedJsonError::TypedShape)?)?;
    validate_unsigned_integer(object.get("minor").ok_or(TypedJsonError::TypedShape)?)
}

fn validate_numbers_matching_shape(node: &JsonNode, shape: &Value) -> Result<(), TypedJsonError> {
    match shape {
        Value::Number(_) => validate_unsigned_integer(node),
        Value::Object(shape) => {
            let node = node.as_object().ok_or(TypedJsonError::TypedShape)?;
            for (key, child_shape) in shape {
                if let Some(child) = node.get(key.as_str()) {
                    validate_numbers_matching_shape(child, child_shape)?;
                }
            }
            Ok(())
        }
        Value::Array(shape) => {
            let node = node.as_array().ok_or(TypedJsonError::TypedShape)?;
            for (child, child_shape) in node.iter().zip(shape) {
                validate_numbers_matching_shape(child, child_shape)?;
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
    }
}

fn validate_unsigned_integer(node: &JsonNode) -> Result<(), TypedJsonError> {
    let literal = node
        .as_number()
        .map(|number| number.raw())
        .ok_or(TypedJsonError::TypedShape)?;
    if literal.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(TypedJsonError::TypedShape)
    }
}

fn validate_bootstrap_response(
    selected_version: ProtocolVersion,
    response: &ProtocolBootstrapResponse,
) -> Result<(), TypedJsonError> {
    if let ProtocolBootstrapResponse::Welcome(welcome) = response {
        if welcome.selected_version() != selected_version
            || welcome.runtime().protocol_version() != selected_version
        {
            return Err(TypedJsonError::SelectedVersionMismatch);
        }
        if !welcome.limits().is_within_v1_hard_maxima() {
            return Err(TypedJsonError::InvalidProtocolLimits);
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProtocolHelloInput {
    supported_versions: Vec<ProtocolVersionInput>,
    client: ClientInfoInput,
    capabilities: CapabilityValuesInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProtocolVersionInput {
    major: u16,
    minor: u16,
}

impl ProtocolVersionInput {
    fn into_semantic(self) -> ProtocolVersion {
        ProtocolVersion::new(self.major, self.minor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientInfoInput {
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityValuesInput {
    values: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ProtocolBootstrapResponseInput {
    Welcome(ProtocolWelcomeInput),
    Reject(ProtocolRejectInput),
}

impl ProtocolBootstrapResponseInput {
    fn into_semantic(self) -> Result<ProtocolBootstrapResponse, TypedJsonError> {
        match self {
            Self::Welcome(value) => Ok(ProtocolBootstrapResponse::Welcome(value.into_semantic()?)),
            Self::Reject(value) => Ok(ProtocolBootstrapResponse::Reject(value.into_semantic())),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolWelcomeInput {
    selected_version: ProtocolVersionOutputInput,
    runtime: RuntimeInfoInput,
    capabilities: RuntimeCapabilitiesInput,
    limits: Value,
}

impl ProtocolWelcomeInput {
    fn into_semantic(self) -> Result<ProtocolWelcome, TypedJsonError> {
        Ok(ProtocolWelcome::new(
            self.selected_version.into_semantic(),
            self.runtime.into_semantic(),
            self.capabilities.into_semantic()?,
            decode_tolerant_protocol_limits(self.limits)?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeInfoInput {
    protocol_version: ProtocolVersionOutputInput,
    implementation: String,
    implementation_version: String,
}

impl RuntimeInfoInput {
    fn into_semantic(self) -> RuntimeInfo {
        RuntimeInfo::new(
            self.protocol_version.into_semantic(),
            self.implementation,
            self.implementation_version,
        )
    }
}

#[derive(Deserialize)]
struct RuntimeCapabilitiesInput {
    values: Vec<String>,
}

impl RuntimeCapabilitiesInput {
    fn into_semantic(self) -> Result<RuntimeCapabilities, TypedJsonError> {
        let values = self
            .values
            .into_iter()
            .map(|value| value.parse::<CapabilityToken>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| TypedJsonError::TypedShape)?;
        RuntimeCapabilities::for_v1(
            values
                .into_iter()
                .filter(is_v1_runtime_capability)
                .collect(),
        )
        .map_err(|_| TypedJsonError::TypedShape)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolRejectInput {
    reason: ProtocolRejectReasonInput,
    supported_versions: Vec<ProtocolVersionOutputInput>,
}

impl ProtocolRejectInput {
    fn into_semantic(self) -> ProtocolReject {
        ProtocolReject::new(
            self.reason.into_semantic(),
            self.supported_versions
                .into_iter()
                .map(ProtocolVersionOutputInput::into_semantic)
                .collect(),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProtocolRejectReasonInput {
    UnsupportedProtocolVersion,
    InvalidHello,
}

impl ProtocolRejectReasonInput {
    const fn into_semantic(self) -> ProtocolRejectReason {
        match self {
            Self::UnsupportedProtocolVersion => ProtocolRejectReason::UnsupportedProtocolVersion,
            Self::InvalidHello => ProtocolRejectReason::InvalidHello,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolVersionOutputInput {
    major: u16,
    minor: u16,
}

impl ProtocolVersionOutputInput {
    const fn into_semantic(self) -> ProtocolVersion {
        ProtocolVersion::new(self.major, self.minor)
    }
}

fn decode_tolerant_protocol_limits(mut value: Value) -> Result<ProtocolLimits, TypedJsonError> {
    let shape = serde_json::to_value(ProtocolLimits::v1_0())
        .map_err(|_| TypedJsonError::EncodingInvariant)?;
    retain_known_shape(&mut value, &shape);
    serde_json::from_value(value).map_err(|_| TypedJsonError::TypedShape)
}

fn retain_known_shape(value: &mut Value, shape: &Value) {
    let (Some(value), Some(shape)) = (value.as_object_mut(), shape.as_object()) else {
        return;
    };
    value.retain(|key, _| shape.contains_key(key));
    for (key, child) in value {
        if let Some(child_shape) = shape.get(key) {
            retain_known_shape(child, child_shape);
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolHelloOutput<'a> {
    supported_versions: &'a [ProtocolVersion],
    client: ClientInfoOutput<'a>,
    capabilities: CapabilityValuesOutput<'a>,
}

#[derive(Serialize)]
struct ClientInfoOutput<'a> {
    name: &'a str,
    version: &'a str,
}

#[derive(Serialize)]
struct CapabilityValuesOutput<'a> {
    values: &'a [Box<str>],
}

struct BoundedVecWriter {
    bytes: Vec<u8>,
    maximum: usize,
    limit_exceeded: bool,
}

impl BoundedVecWriter {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
            limit_exceeded: false,
        }
    }
}

impl Write for BoundedVecWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.len().checked_add(bytes.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other("typed JSON frame limit exceeded"));
        };
        if next > self.maximum {
            self.limit_exceeded = true;
            return Err(io::Error::other("typed JSON frame limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::limits::{ProtocolNegotiation, negotiate_protocol, v1_runtime_capabilities};

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct RepresentativeDto {
        state: RepresentativeState,
        note: Option<String>,
    }

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(tag = "type", content = "data", rename_all = "snake_case")]
    enum RepresentativeState {
        Idle,
        Set(String),
        Detail(RepresentativeDetail),
    }

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    struct RepresentativeDetail {
        note: Option<String>,
    }

    #[test]
    fn mixed_enum_and_nullable_fields_have_one_canonical_shape() {
        let codec = WireV1Codec::v1_0();
        let value = RepresentativeDto {
            state: RepresentativeState::Detail(RepresentativeDetail { note: None }),
            note: None,
        };
        assert_eq!(
            codec.encode(PublicJsonKind::Request, &value).unwrap(),
            br#"{"state":{"type":"detail","data":{"note":null}},"note":null}"#
        );

        let decoded = decode_representative(&codec, br#"{"state":{"type":"idle"}}"#).unwrap();
        assert_eq!(
            decoded,
            RepresentativeDto {
                state: RepresentativeState::Idle,
                note: None,
            }
        );
        assert_eq!(
            codec.encode(PublicJsonKind::Request, &decoded).unwrap(),
            br#"{"state":{"type":"idle"},"note":null}"#
        );
    }

    #[test]
    fn semantic_invalid_hello_remains_raw_until_negotiation() {
        for input in [
            br#"{"supportedVersions":[{"major":1,"minor":0},{"major":1,"minor":0}],"client":{"name":"host","version":"1"},"capabilities":{"values":[]}}"#.as_slice(),
            br#"{"supportedVersions":[{"major":1,"minor":0}],"client":{"name":"host","version":"1"},"capabilities":{"values":["Future-Capability"]}}"#.as_slice(),
        ] {
            let hello = decode_protocol_hello_v1(input).unwrap();
            assert_eq!(
                negotiate_protocol(
                    &hello,
                    &RuntimeCapabilities::for_v1(v1_runtime_capabilities()).unwrap(),
                ),
                ProtocolNegotiation::Rejected {
                    reason: ProtocolRejectReason::InvalidHello,
                }
            );
        }
    }

    #[test]
    fn mixed_enum_rejects_ambiguous_or_unknown_shapes() {
        let codec = WireV1Codec::v1_0();
        for input in [
            br#"{"state":{"type":"idle","data":null},"note":null}"#.as_slice(),
            br#"{"state":{"type":"future"},"note":null}"#.as_slice(),
            br#"{"state":{"type":"set"},"note":null}"#.as_slice(),
            br#"{"state":{"type":"idle","future":1},"note":null}"#.as_slice(),
            br#"{"state":{"type":"detail","data":{"note":null,"future":1}},"note":null}"#
                .as_slice(),
            br#"{"state":{"type":"idle"},"note":null,"future":1}"#.as_slice(),
        ] {
            assert_eq!(
                decode_representative(&codec, input),
                Err(TypedJsonError::TypedShape)
            );
        }
    }

    #[test]
    fn typed_strings_and_present_options_encode_canonically() {
        let codec = WireV1Codec::v1_0();
        let value = RepresentativeDto {
            state: RepresentativeState::Set("é/\"\\\n\u{0001}".to_owned()),
            note: Some("present".to_owned()),
        };
        assert_eq!(
            String::from_utf8(codec.encode(PublicJsonKind::Request, &value).unwrap()).unwrap(),
            "{\"state\":{\"type\":\"set\",\"data\":\"é/\\\"\\\\\\n\\u0001\"},\"note\":\"present\"}"
        );
    }

    #[test]
    fn typed_encoder_stops_before_exceeding_the_frame_cap() {
        let mut limits = ProtocolLimits::v1_0();
        limits.transport.max_progress_event_bytes = 32;
        let codec = WireV1Codec::new(ProtocolVersion::V1_0, limits).unwrap();
        let value = RepresentativeDto {
            state: RepresentativeState::Set("x".repeat(64)),
            note: None,
        };
        assert_eq!(
            codec.encode(PublicJsonKind::ProgressEvent, &value),
            Err(TypedJsonError::FrameTooLarge)
        );

        let mut writer = BoundedVecWriter::new(4);
        writer.write_all(b"1234").unwrap();
        assert_eq!(writer.bytes, b"1234");
        assert!(writer.write_all(b"5").is_err());
        assert_eq!(writer.bytes, b"1234");
    }

    fn decode_representative(
        codec: &WireV1Codec,
        input: &[u8],
    ) -> Result<RepresentativeDto, TypedJsonError> {
        codec.decode_with_shape(PublicJsonKind::Request, input, |node| {
            let object = node.as_object().ok_or(TypedJsonError::TypedShape)?;
            if object
                .keys()
                .any(|key| key.as_ref() != "state" && key.as_ref() != "note")
            {
                return Err(TypedJsonError::TypedShape);
            }
            let state = object.get("state").ok_or(TypedJsonError::TypedShape)?;
            validate_adjacent_enum(
                state,
                UnknownFieldPolicy::Reject,
                &[
                    AdjacentVariant::unit("idle"),
                    AdjacentVariant::payload("set"),
                    AdjacentVariant::payload("detail"),
                ],
            )
        })
    }
}
