mod bootstrap;
mod bounded_json;
#[allow(
    dead_code,
    reason = "M3 line codec is consumed by future storage and recorder slices"
)]
pub(crate) mod conversation_jsonl;
mod json_number;
pub(crate) mod lexical;
mod limits;
mod path;
mod public_protocol;
mod scalar;
mod schema;
mod typed_json;
mod value;

pub use crate::runtime_interface::{
    RuntimeCapabilities, RuntimeCapabilitiesError, RuntimeCapability,
};
pub use bootstrap::{
    ProtocolBootstrapRoute, ProtocolBootstrapRouter, ProtocolBootstrapRouterError,
};
pub use bounded_json::{BoundedJsonError, BoundedJsonObject, BoundedJsonValue};
pub use limits::{
    CapabilityToken, CapabilityTokenError, CatalogLimits, ClientInfo, EmbeddedJsonLimits,
    InteractionLimits, JsonSchemaLimits, JsonValueLimits, ObservationLimits, PagingLimits,
    PromptWireLimits, ProtocolBootstrapResponse, ProtocolHello, ProtocolLimits, ProtocolReject,
    ProtocolRejectReason, ProtocolVersion, ProtocolWelcome, QueueLimits, RuntimeInfo, TextLimits,
    TransportLimits, WorkspaceWireLimits,
};
#[allow(unused_imports, reason = "crate-private M2 Runtime negotiation seam")]
pub(crate) use limits::{ProtocolNegotiation, negotiate_protocol};
pub use path::{CanonicalFileUri, FileUriFamily, PathWireError, WorkspaceRelativePath};
pub use public_protocol::{IncrementalRuntimeProtocolV1, RuntimeRequestKind};
pub use scalar::{
    AgentId, AgentMetadataRevision, AgentRevision, CanonicalU64, CommandId, EntryId,
    IdGenerationError, InteractionResolutionKey, ItemId, PageCursor, RequestId,
    SessionDefinitionRevision, SessionId, SessionMetadataRevision, TurnId, WireScalarError,
    WorkspaceRevision,
};
pub use schema::{BoundedJsonSchema, BoundedJsonSchemaError};
pub use typed_json::{
    PublicDecodeCode, PublicDecodeError, PublicDecodeStage, PublicJsonKind, TypedJsonError,
    WireV1Codec, decode_protocol_bootstrap_response_v1, decode_protocol_hello_v1,
    encode_protocol_bootstrap_response_v1, encode_protocol_hello_v1,
};
pub use value::{CurrencyCode, Duration, Money, MoneyAmount, Timestamp, WireValueError};
