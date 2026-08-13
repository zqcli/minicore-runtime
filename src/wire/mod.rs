mod bootstrap;
mod bounded_json;
pub(crate) mod conversation_jsonl;
pub(crate) mod conversation_jsonl_scanner;
pub(crate) mod durable_store;

#[cfg(feature = "heavy-tests")]
#[doc(hidden)]
pub mod heavy_test_support {
    use std::fs::File;

    use super::SessionId;
    use super::conversation_jsonl_scanner::{
        ConversationJsonlScanner, ConversationScanAccess, ConversationScanError,
        ConversationScanEvent,
    };

    /// Minimal feature-gated bridge used only by the generated heavy boundary target.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ConversationScanBoundaryError {
        HistoryTooLarge,
        HeaderRejected,
        InputChanged,
        InputUnavailable,
        InvariantViolation,
        CounterOverflow,
    }

    /// Safe, bounded aggregate scanner fault counters. No decoded record or raw line bytes cross
    /// this test-only public bridge.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct ConversationScanFaultCounters {
        pub oversized_line: u64,
        pub invalid_utf8: u64,
        pub malformed_json: u64,
        pub invalid_entry: u64,
        pub unknown_record_variant: u64,
        pub unknown_entry_variant: u64,
        pub session_mismatch: u64,
    }

    impl ConversationScanFaultCounters {
        fn record(
            &mut self,
            fault: super::conversation_jsonl_scanner::ConversationLineFault,
        ) -> Result<(), ConversationScanBoundaryError> {
            let counter = match fault {
                super::conversation_jsonl_scanner::ConversationLineFault::OversizedLine => {
                    &mut self.oversized_line
                }
                super::conversation_jsonl_scanner::ConversationLineFault::InvalidUtf8 => {
                    &mut self.invalid_utf8
                }
                super::conversation_jsonl_scanner::ConversationLineFault::MalformedJson => {
                    &mut self.malformed_json
                }
                super::conversation_jsonl_scanner::ConversationLineFault::InvalidEntry => {
                    &mut self.invalid_entry
                }
                super::conversation_jsonl_scanner::ConversationLineFault::UnknownRecordVariant => {
                    &mut self.unknown_record_variant
                }
                super::conversation_jsonl_scanner::ConversationLineFault::UnknownEntryVariant => {
                    &mut self.unknown_entry_variant
                }
                super::conversation_jsonl_scanner::ConversationLineFault::SessionMismatch => {
                    &mut self.session_mismatch
                }
            };
            *counter = counter
                .checked_add(1)
                .ok_or(ConversationScanBoundaryError::CounterOverflow)?;
            Ok(())
        }
    }

    /// Bounded aggregate facts from a physical scan. No decoded record or raw line bytes cross
    /// this test-only public bridge.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ConversationScanBoundarySummary {
        pub complete_entries: u64,
        pub faults: ConversationScanFaultCounters,
        pub saw_partial_tail: bool,
    }

    pub fn scan_conversation_file(
        file: File,
        opened_session_id: SessionId,
    ) -> Result<ConversationScanBoundarySummary, ConversationScanBoundaryError> {
        let mut scanner = ConversationJsonlScanner::open_file(
            file,
            opened_session_id,
            ConversationScanAccess::ReadOnly,
        )
        .map_err(map_error)?;
        let mut faults = ConversationScanFaultCounters::default();
        let mut saw_partial_tail = false;
        while let Some(event) = scanner.next_event().map_err(map_error)? {
            match event {
                ConversationScanEvent::Entry { .. } => {}
                ConversationScanEvent::Fault { fault, .. } => faults.record(fault)?,
                ConversationScanEvent::PartialTail { .. } => saw_partial_tail = true,
            }
        }
        Ok(ConversationScanBoundarySummary {
            complete_entries: scanner.complete_entry_records(),
            faults,
            saw_partial_tail,
        })
    }

    fn map_error(error: ConversationScanError) -> ConversationScanBoundaryError {
        match error {
            ConversationScanError::FileTooLarge | ConversationScanError::HistoryTooLarge => {
                ConversationScanBoundaryError::HistoryTooLarge
            }
            ConversationScanError::InputUnavailable => {
                ConversationScanBoundaryError::InputUnavailable
            }
            ConversationScanError::InputChanged => ConversationScanBoundaryError::InputChanged,
            ConversationScanError::LeaseMismatch | ConversationScanError::InvariantViolation => {
                ConversationScanBoundaryError::InvariantViolation
            }
            ConversationScanError::CounterOverflow => {
                ConversationScanBoundaryError::CounterOverflow
            }
            ConversationScanError::HeaderCorrupt { .. }
            | ConversationScanError::UnsupportedFormatVersion
            | ConversationScanError::MissingHeader => ConversationScanBoundaryError::HeaderRejected,
        }
    }
}
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
