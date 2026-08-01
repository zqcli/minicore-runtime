mod bounded_json;
mod json_number;
pub(crate) mod lexical;
mod limits;
mod path;
mod scalar;
mod schema;
mod typed_json;
mod value;

pub use bounded_json::{BoundedJsonError, BoundedJsonObject, BoundedJsonValue};
pub use limits::{
    CapabilityToken, CapabilityTokenError, CatalogLimits, ClientInfo, EmbeddedJsonLimits,
    InteractionLimits, JsonSchemaLimits, JsonValueLimits, ObservationLimits, PagingLimits,
    PromptWireLimits, ProtocolBootstrapResponse, ProtocolHello, ProtocolLimits, ProtocolReject,
    ProtocolRejectReason, ProtocolVersion, ProtocolWelcome, QueueLimits, RuntimeCapabilities,
    RuntimeInfo, TextLimits, TransportLimits, WorkspaceWireLimits,
};
#[allow(unused_imports, reason = "crate-private M2 Runtime negotiation seam")]
pub(crate) use limits::{ProtocolNegotiation, negotiate_protocol, v1_runtime_capabilities};
pub use path::{CanonicalFileUri, FileUriFamily, PathWireError, WorkspaceRelativePath};
pub use scalar::{
    AgentId, AgentMetadataRevision, AgentRevision, CanonicalU64, CommandId, EntryId,
    IdGenerationError, InteractionResolutionKey, ItemId, PageCursor, RequestId,
    SessionDefinitionRevision, SessionId, SessionMetadataRevision, TurnId, WireScalarError,
    WorkspaceRevision,
};
pub use schema::{BoundedJsonSchema, BoundedJsonSchemaError};
pub use typed_json::{
    PublicJsonKind, TypedJsonError, WireV1Codec, decode_protocol_bootstrap_response_v1,
    decode_protocol_hello_v1, encode_protocol_bootstrap_response_v1, encode_protocol_hello_v1,
};
pub use value::{CurrencyCode, Duration, Money, MoneyAmount, Timestamp, WireValueError};
