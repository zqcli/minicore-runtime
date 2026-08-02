use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use std::ops::Range;
use std::str::FromStr;

use thiserror::Error;

use crate::agent_session_lifecycle::AgentRevisionRef;
use crate::compaction::{StoredCompaction, StoredCompactionModelCall};
use crate::conversation_storage::{
    SessionHeader, StoredAssistantContent, StoredAssistantMessage, StoredEntryBody,
    StoredInteractionRequest, StoredInteractionRequestBody, StoredInteractionResolution,
    StoredInteractionResolutionBody, StoredSessionEntry, StoredToolMessage, StoredToolOutcome,
    StoredUserMessage,
};
use crate::model_gateway::{
    ModelFinishReason, ModelReasoningSummary, ModelResponseSummary, ModelServiceClass, ModelUsage,
    ProviderItemId, ProviderRequestId, ProviderResponseId, ProviderResponseMetadata,
    ReasoningContent, RedactedProviderCode,
};
use crate::prompt::{
    CanonicalUserMessage, MessageContent, MessageRecord, PromptContributionOrigin,
    PromptContributionStamp,
};
use crate::tools::{
    ToolAbandonReason, ToolApprovalOptionKindView, ToolApprovalOptionView, ToolApprovalRequestView,
    ToolApprovalResolution, ToolApprovalResolutionRef, ToolOutcomeSource,
    ToolRequirementSummaryView, ToolResultContent, ToolResultDisposition, UserQuestionAnswer,
    UserQuestionAnswerValue, UserQuestionChoice, UserQuestionField, UserQuestionFieldAnswer,
    UserQuestionInput, UserQuestionRequest,
};
use crate::turn_item_interaction::{
    AssistantDisposition, InteractionCancelReason, UserMessageSource,
};
use crate::wire::bounded_json::{
    BoundedJsonError, JsonNode, JsonParseLimits, decode_json_string_token, parse_node,
};
use crate::wire::json_number::validate_json_number_syntax;
use crate::wire::lexical::validate_safe_text;
use crate::wire::{BoundedJsonObject, InteractionResolutionKey, Money, SessionId};

/// The V1 header byte cap, excluding any physical line ending.
pub(crate) const MAX_CONVERSATION_HEADER_BYTES: usize = 65_536;
/// The V1 entry byte cap, excluding any physical line ending.
pub(crate) const MAX_CONVERSATION_ENTRY_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ConversationCodecError {
    #[error("conversation line exceeds the V1 limit")]
    LineTooLarge,
    #[error("conversation header exceeds the V1 limit")]
    HeaderTooLarge,
    #[error("conversation line fails bounded JSON preflight")]
    InvalidJson,
    #[error("conversation record has an unknown variant")]
    UnknownRecordVariant,
    #[error("conversation entry body has an unknown variant")]
    UnknownBodyVariant,
    #[error("conversation record does not have the required V1 shape")]
    InvalidShape,
    #[error("conversation record is missing a required field")]
    MissingRequiredField,
    #[error("conversation record has an invalid typed scalar")]
    InvalidScalar,
    #[error("conversation header has an unsupported format version")]
    UnsupportedFormatVersion,
    #[error("conversation record violates an owner semantic constraint")]
    InvalidSemantic,
    #[error("conversation header does not match the opened session")]
    SessionIdentityMismatch,
    #[error("conversation record kind is invalid at this position")]
    UnexpectedRecordKind,
}

/// A bounded, fully typed V1 JSONL record. It never exposes the parser's raw JSON tree.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ConversationRecord {
    Header(SessionHeader),
    Entry(Box<StoredSessionEntry>),
}

impl fmt::Debug for ConversationRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header(value) => formatter.debug_tuple("Header").field(value).finish(),
            Self::Entry(value) => formatter.debug_tuple("Entry").field(value).finish(),
        }
    }
}

/// Wire-owned bounded preflight plus exact V1 projection. Physical line framing belongs to M3.2.
pub(crate) struct ConversationLineCodec;

impl ConversationLineCodec {
    pub(crate) fn decode_record(
        input: &[u8],
    ) -> Result<ConversationRecord, ConversationCodecError> {
        if input.len() > MAX_CONVERSATION_ENTRY_BYTES {
            return Err(ConversationCodecError::LineTooLarge);
        }
        let preflight = preflight_record(input)?;
        let node = parse_node(input, JsonParseLimits::conversation(preflight.input_limit))
            .map_err(map_json_error)?;
        let mut tool_call_arguments = preflight.tool_call_arguments.into_iter();
        let record = decode_record_node(&node, input.len(), &mut tool_call_arguments)?;
        if tool_call_arguments.next().is_some() {
            return Err(ConversationCodecError::InvalidShape);
        }
        Ok(record)
    }

    pub(crate) fn decode_header_for_catalog(
        input: &[u8],
        opened_session_id: SessionId,
    ) -> Result<SessionHeader, ConversationCodecError> {
        if input.len() > MAX_CONVERSATION_HEADER_BYTES {
            return Err(ConversationCodecError::HeaderTooLarge);
        }
        let ConversationRecord::Header(header) = Self::decode_record(input)? else {
            return Err(ConversationCodecError::UnexpectedRecordKind);
        };
        if header.session_id() != opened_session_id {
            return Err(ConversationCodecError::SessionIdentityMismatch);
        }
        Ok(header)
    }

    pub(crate) fn decode_entry_for_session(
        input: &[u8],
        header_session_id: SessionId,
    ) -> Result<StoredSessionEntry, ConversationCodecError> {
        let ConversationRecord::Entry(entry) = Self::decode_record(input)? else {
            return Err(ConversationCodecError::UnexpectedRecordKind);
        };
        if entry.session_id() != header_session_id {
            return Err(ConversationCodecError::SessionIdentityMismatch);
        }
        Ok(*entry)
    }

    pub(crate) fn encode_header(header: &SessionHeader) -> Result<Vec<u8>, ConversationCodecError> {
        if header.format_version() != 1 {
            return Err(ConversationCodecError::InvalidSemantic);
        }
        let mut writer = JsonWriter::new(MAX_CONVERSATION_HEADER_BYTES);
        writer.object_start()?;
        writer.key("type")?;
        writer.string("session_header")?;
        writer.comma()?;
        writer.key("data")?;
        encode_header_data(&mut writer, header)?;
        writer.object_end()?;
        writer.finish()
    }

    pub(crate) fn encode_entry(
        entry: &StoredSessionEntry,
    ) -> Result<Vec<u8>, ConversationCodecError> {
        entry
            .body()
            .validate_for_wire()
            .map_err(|_| ConversationCodecError::InvalidSemantic)?;
        let mut writer = JsonWriter::new(MAX_CONVERSATION_ENTRY_BYTES);
        writer.object_start()?;
        writer.key("type")?;
        writer.string("entry")?;
        writer.comma()?;
        writer.key("data")?;
        encode_entry_data(&mut writer, entry)?;
        writer.object_end()?;
        writer.finish()
    }

    pub(crate) fn encode_record(
        record: &ConversationRecord,
    ) -> Result<Vec<u8>, ConversationCodecError> {
        match record {
            ConversationRecord::Header(value) => Self::encode_header(value),
            ConversationRecord::Entry(value) => Self::encode_entry(value),
        }
    }
}

struct RecordPreflight {
    input_limit: usize,
    tool_call_arguments: Vec<BoundedJsonObject>,
}

/// Locates only the raw values that have a stricter embedded-JSON contract than the enclosing
/// conversation record. This deliberately runs before the conversation AST is materialized, so
/// canonicalization cannot erase whitespace or escape bytes from a ToolCall's `arguments` span.
fn preflight_record(input: &[u8]) -> Result<RecordPreflight, ConversationCodecError> {
    let root = RawJsonScanner::complete_value(input)?;
    let root_fields = raw_object_fields(input, root, &["type", "data"])?;
    let is_header = root_fields
        .as_ref()
        .is_some_and(|fields| raw_strings_include(input, &fields[0], "session_header"));
    if is_header && input.len() > MAX_CONVERSATION_HEADER_BYTES {
        return Err(ConversationCodecError::HeaderTooLarge);
    }

    let mut tool_call_arguments = Vec::new();
    if root_fields
        .as_ref()
        .is_some_and(|fields| raw_strings_include(input, &fields[0], "entry"))
    {
        for data in &root_fields.expect("checked above")[1] {
            collect_assistant_tool_call_arguments(input, data.clone(), &mut tool_call_arguments)?;
        }
    }

    Ok(RecordPreflight {
        input_limit: if is_header {
            MAX_CONVERSATION_HEADER_BYTES
        } else {
            MAX_CONVERSATION_ENTRY_BYTES
        },
        tool_call_arguments,
    })
}

fn collect_assistant_tool_call_arguments(
    input: &[u8],
    entry_data: Range<usize>,
    tool_call_arguments: &mut Vec<BoundedJsonObject>,
) -> Result<(), ConversationCodecError> {
    let Some(entry_fields) = raw_object_fields(input, entry_data, &["body"])? else {
        return Ok(());
    };
    for body in &entry_fields[0] {
        let Some(body_fields) = raw_object_fields(input, body.clone(), &["type", "data"])? else {
            continue;
        };
        if !raw_strings_include(input, &body_fields[0], "assistant_message") {
            continue;
        }
        for assistant_data in &body_fields[1] {
            let Some(assistant_fields) =
                raw_object_fields(input, assistant_data.clone(), &["content"])?
            else {
                continue;
            };
            for content in &assistant_fields[0] {
                let Some(content_items) = raw_array_values(input, content.clone())? else {
                    continue;
                };
                for item in content_items {
                    collect_tool_call_arguments_from_content_item(
                        input,
                        item,
                        tool_call_arguments,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn collect_tool_call_arguments_from_content_item(
    input: &[u8],
    content_item: Range<usize>,
    tool_call_arguments: &mut Vec<BoundedJsonObject>,
) -> Result<(), ConversationCodecError> {
    let Some(content_fields) = raw_object_fields(input, content_item, &["type", "data"])? else {
        return Ok(());
    };
    if !raw_strings_include(input, &content_fields[0], "tool_call") {
        return Ok(());
    }
    for tool_call_data in &content_fields[1] {
        let Some(arguments_fields) =
            raw_object_fields(input, tool_call_data.clone(), &["arguments"])?
        else {
            continue;
        };
        for arguments in &arguments_fields[0] {
            if input.get(arguments.start) != Some(&b'{') {
                return Err(ConversationCodecError::InvalidShape);
            }
            let arguments =
                BoundedJsonObject::from_slice(&input[arguments.clone()]).map_err(map_json_error)?;
            tool_call_arguments.push(arguments);
        }
    }
    Ok(())
}

fn raw_strings_include(input: &[u8], spans: &[Range<usize>], expected: &str) -> bool {
    spans.iter().any(|span| {
        decode_json_string_token(&input[span.clone()], expected.len())
            .is_ok_and(|value| value.as_ref() == expected)
    })
}

fn raw_object_fields(
    input: &[u8],
    span: Range<usize>,
    names: &[&str],
) -> Result<Option<Vec<Vec<Range<usize>>>>, ConversationCodecError> {
    let mut scanner = RawJsonScanner::new(&input[span.clone()])?;
    scanner.skip_whitespace();
    if !scanner.consume_if(b'{') {
        return Ok(None);
    }

    let mut fields = vec![Vec::new(); names.len()];
    scanner.skip_whitespace();
    if scanner.consume_if(b'}') {
        return scanner
            .is_complete()
            .then_some(Some(fields))
            .ok_or(ConversationCodecError::InvalidJson);
    }

    let mut member_count = 0_usize;
    loop {
        member_count = member_count
            .checked_add(1)
            .ok_or(ConversationCodecError::InvalidJson)?;
        scanner.validate_object_members(member_count)?;
        scanner.skip_whitespace();
        let key = scanner.parse_string()?;
        scanner.skip_whitespace();
        scanner.expect_exact(b':')?;
        let value = scanner.parse_value(2)?;
        for (index, name) in names.iter().enumerate() {
            if decode_json_string_token(&scanner.input[key.clone()], name.len())
                .is_ok_and(|value| value.as_ref() == *name)
            {
                fields[index].push((span.start + value.start)..(span.start + value.end));
            }
        }
        scanner.skip_whitespace();
        if scanner.consume_if(b'}') {
            break;
        }
        scanner.expect_exact(b',')?;
    }
    scanner
        .is_complete()
        .then_some(Some(fields))
        .ok_or(ConversationCodecError::InvalidJson)
}

fn raw_array_values(
    input: &[u8],
    span: Range<usize>,
) -> Result<Option<Vec<Range<usize>>>, ConversationCodecError> {
    let mut scanner = RawJsonScanner::new(&input[span.clone()])?;
    scanner.skip_whitespace();
    if !scanner.consume_if(b'[') {
        return Ok(None);
    }

    let mut values = Vec::new();
    scanner.skip_whitespace();
    if scanner.consume_if(b']') {
        return scanner
            .is_complete()
            .then_some(Some(values))
            .ok_or(ConversationCodecError::InvalidJson);
    }

    loop {
        scanner.validate_array_items(values.len() + 1)?;
        let value = scanner.parse_value(2)?;
        values.push((span.start + value.start)..(span.start + value.end));
        scanner.skip_whitespace();
        if scanner.consume_if(b']') {
            break;
        }
        scanner.expect_exact(b',')?;
    }
    scanner
        .is_complete()
        .then_some(Some(values))
        .ok_or(ConversationCodecError::InvalidJson)
}

struct RawJsonScanner<'input> {
    input: &'input [u8],
    position: usize,
    node_count: usize,
}

impl<'input> RawJsonScanner<'input> {
    fn new(input: &'input [u8]) -> Result<Self, ConversationCodecError> {
        std::str::from_utf8(input).map_err(|_| ConversationCodecError::InvalidJson)?;
        Ok(Self {
            input,
            position: 0,
            node_count: 0,
        })
    }

    fn complete_value(input: &'input [u8]) -> Result<Range<usize>, ConversationCodecError> {
        let mut scanner = Self::new(input)?;
        let span = scanner.parse_value(1)?;
        if scanner.is_complete() {
            Ok(span)
        } else {
            Err(ConversationCodecError::InvalidJson)
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Range<usize>, ConversationCodecError> {
        if depth > 64 {
            return Err(ConversationCodecError::InvalidJson);
        }
        self.node_count = self
            .node_count
            .checked_add(1)
            .ok_or(ConversationCodecError::InvalidJson)?;
        if self.node_count > 16_384 {
            return Err(ConversationCodecError::InvalidJson);
        }
        self.skip_whitespace();
        let start = self.position;
        match self.peek().ok_or(ConversationCodecError::InvalidJson)? {
            b'n' => self.parse_literal(b"null")?,
            b't' => self.parse_literal(b"true")?,
            b'f' => self.parse_literal(b"false")?,
            b'"' => {
                self.parse_string()?;
            }
            b'[' => self.parse_array(depth)?,
            b'{' => self.parse_object(depth)?,
            b'-' | b'0'..=b'9' => self.parse_number()?,
            _ => return Err(ConversationCodecError::InvalidJson),
        }
        Ok(start..self.position)
    }

    fn parse_object(&mut self, depth: usize) -> Result<(), ConversationCodecError> {
        self.expect_exact(b'{')?;
        self.skip_whitespace();
        if self.consume_if(b'}') {
            return Ok(());
        }
        let mut member_count = 0_usize;
        loop {
            member_count = member_count
                .checked_add(1)
                .ok_or(ConversationCodecError::InvalidJson)?;
            self.validate_object_members(member_count)?;
            self.skip_whitespace();
            self.parse_string()?;
            self.skip_whitespace();
            self.expect_exact(b':')?;
            self.parse_value(depth + 1)?;
            self.skip_whitespace();
            if self.consume_if(b'}') {
                return Ok(());
            }
            self.expect_exact(b',')?;
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), ConversationCodecError> {
        self.expect_exact(b'[')?;
        self.skip_whitespace();
        if self.consume_if(b']') {
            return Ok(());
        }
        let mut item_count = 0_usize;
        loop {
            item_count = item_count
                .checked_add(1)
                .ok_or(ConversationCodecError::InvalidJson)?;
            self.validate_array_items(item_count)?;
            self.parse_value(depth + 1)?;
            self.skip_whitespace();
            if self.consume_if(b']') {
                return Ok(());
            }
            self.expect_exact(b',')?;
        }
    }

    fn parse_string(&mut self) -> Result<Range<usize>, ConversationCodecError> {
        let start = self.position;
        self.expect_exact(b'"')?;
        loop {
            let byte = self.next().ok_or(ConversationCodecError::InvalidJson)?;
            match byte {
                b'"' => return Ok(start..self.position),
                b'\\' => self.parse_escape()?,
                0x00..=0x1f => return Err(ConversationCodecError::InvalidJson),
                _ => {}
            }
        }
    }

    fn parse_escape(&mut self) -> Result<(), ConversationCodecError> {
        match self.next().ok_or(ConversationCodecError::InvalidJson)? {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Ok(()),
            b'u' => {
                let first = self.parse_hex_u16()?;
                if (0xd800..=0xdbff).contains(&first) {
                    self.expect_exact(b'\\')?;
                    self.expect_exact(b'u')?;
                    let second = self.parse_hex_u16()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(ConversationCodecError::InvalidJson);
                    }
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return Err(ConversationCodecError::InvalidJson);
                }
                Ok(())
            }
            _ => Err(ConversationCodecError::InvalidJson),
        }
    }

    fn parse_hex_u16(&mut self) -> Result<u16, ConversationCodecError> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let digit = match self.next().ok_or(ConversationCodecError::InvalidJson)? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(ConversationCodecError::InvalidJson),
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<(), ConversationCodecError> {
        let start = self.position;
        while self
            .peek()
            .is_some_and(|byte| !is_raw_value_delimiter(byte))
        {
            self.position += 1;
        }
        let literal = std::str::from_utf8(&self.input[start..self.position])
            .map_err(|_| ConversationCodecError::InvalidJson)?;
        validate_json_number_syntax(literal).map_err(|_| ConversationCodecError::InvalidJson)
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), ConversationCodecError> {
        if self.input.get(self.position..self.position + literal.len()) != Some(literal) {
            return Err(ConversationCodecError::InvalidJson);
        }
        self.position += literal.len();
        if self
            .peek()
            .is_some_and(|byte| !is_raw_value_delimiter(byte))
        {
            return Err(ConversationCodecError::InvalidJson);
        }
        Ok(())
    }

    fn validate_object_members(&self, count: usize) -> Result<(), ConversationCodecError> {
        (count <= 256)
            .then_some(())
            .ok_or(ConversationCodecError::InvalidJson)
    }

    fn validate_array_items(&self, count: usize) -> Result<(), ConversationCodecError> {
        (count <= 4_096)
            .then_some(())
            .ok_or(ConversationCodecError::InvalidJson)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn expect_exact(&mut self, expected: u8) -> Result<(), ConversationCodecError> {
        if self.consume_if(expected) {
            Ok(())
        } else {
            Err(ConversationCodecError::InvalidJson)
        }
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn is_complete(&mut self) -> bool {
        self.skip_whitespace();
        self.position == self.input.len()
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }
}

fn is_raw_value_delimiter(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}')
}

fn map_json_error(_: BoundedJsonError) -> ConversationCodecError {
    ConversationCodecError::InvalidJson
}

fn decode_record_node(
    node: &JsonNode,
    input_len: usize,
    tool_call_arguments: &mut std::vec::IntoIter<BoundedJsonObject>,
) -> Result<ConversationRecord, ConversationCodecError> {
    let root = as_object(node)?;
    let record_type = string(required(root, "type")?)?;
    let data = required(root, "data")?;
    match record_type {
        "session_header" => {
            if input_len > MAX_CONVERSATION_HEADER_BYTES {
                return Err(ConversationCodecError::HeaderTooLarge);
            }
            strict_fields(root, &["type", "data"])?;
            Ok(ConversationRecord::Header(decode_header(data)?))
        }
        "entry" => Ok(ConversationRecord::Entry(Box::new(decode_entry(
            data,
            tool_call_arguments,
        )?))),
        _ => Err(ConversationCodecError::UnknownRecordVariant),
    }
}

fn decode_header(node: &JsonNode) -> Result<SessionHeader, ConversationCodecError> {
    let data = as_object(node)?;
    strict_fields(
        data,
        &[
            "formatVersion",
            "sessionId",
            "createdAt",
            "initialAgent",
            "initialDefinitionRevision",
        ],
    )?;
    let format_version = u32_value(required(data, "formatVersion")?)?;
    if format_version != 1 {
        return Err(ConversationCodecError::UnsupportedFormatVersion);
    }
    let session_id = scalar(required(data, "sessionId")?)?;
    let created_at = scalar(required(data, "createdAt")?)?;
    let initial_agent = decode_initial_agent(required(data, "initialAgent")?)?;
    let initial_definition_revision = scalar(required(data, "initialDefinitionRevision")?)?;
    Ok(SessionHeader::reconstruct(
        format_version,
        session_id,
        created_at,
        initial_agent,
        initial_definition_revision,
    ))
}

fn decode_initial_agent(node: &JsonNode) -> Result<AgentRevisionRef, ConversationCodecError> {
    let object = as_object(node)?;
    strict_fields(object, &["agentId", "revision"])?;
    Ok(AgentRevisionRef::new(
        scalar(required(object, "agentId")?)?,
        scalar(required(object, "revision")?)?,
    ))
}

fn decode_entry(
    node: &JsonNode,
    tool_call_arguments: &mut std::vec::IntoIter<BoundedJsonObject>,
) -> Result<StoredSessionEntry, ConversationCodecError> {
    let data = as_object(node)?;
    // Entry objects are additive-compatible after a valid V1 header. Structural limits have
    // already bounded every ignored value before this projection.
    let entry_id = scalar(required(data, "entryId")?)?;
    let parent_id = optional_scalar(Some(required(data, "parentId")?))?;
    let session_id = scalar(required(data, "sessionId")?)?;
    let turn_id = scalar(required(data, "turnId")?)?;
    let timestamp = scalar(required(data, "timestamp")?)?;
    let body = decode_body(required(data, "body")?, tool_call_arguments)?;
    Ok(StoredSessionEntry::reconstruct(
        entry_id, parent_id, session_id, turn_id, timestamp, body,
    ))
}

fn decode_body(
    node: &JsonNode,
    tool_call_arguments: &mut std::vec::IntoIter<BoundedJsonObject>,
) -> Result<StoredEntryBody, ConversationCodecError> {
    let object = as_object(node)?;
    let body_type = string(required(object, "type")?)?;
    let data = required(object, "data")?;
    match body_type {
        "user_message" => decode_user_message(data).map(StoredEntryBody::UserMessage),
        "assistant_message" => decode_assistant_message(data, tool_call_arguments)
            .map(StoredEntryBody::AssistantMessage),
        "tool_message" => decode_tool_message(data).map(StoredEntryBody::ToolMessage),
        "interaction_requested" => {
            decode_interaction_request(data).map(StoredEntryBody::InteractionRequested)
        }
        "interaction_resolved" => {
            decode_interaction_resolution(data).map(StoredEntryBody::InteractionResolved)
        }
        "compaction" => decode_compaction(data).map(StoredEntryBody::Compaction),
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_user_message(node: &JsonNode) -> Result<StoredUserMessage, ConversationCodecError> {
    let object = as_object(node)?;
    let item_id = scalar(required(object, "itemId")?)?;
    let source = match string(required(object, "source")?)? {
        "input" => UserMessageSource::Input,
        "steer" => UserMessageSource::Steer,
        _ => return Err(ConversationCodecError::InvalidScalar),
    };
    let content = decode_user_content(required(object, "content")?)?;
    Ok(StoredUserMessage::reconstruct(item_id, source, content))
}

fn decode_user_content(node: &JsonNode) -> Result<CanonicalUserMessage, ConversationCodecError> {
    let object = as_object(node)?;
    let parts = array(required(object, "parts")?)?
        .iter()
        .map(decode_user_part)
        .collect::<Result<Vec<_>, _>>()?;
    let message =
        MessageRecord::reconstruct(parts).map_err(|_| ConversationCodecError::InvalidSemantic)?;

    // Stamps are intentionally the one independently degradable nested element. M3 only returns
    // the surviving value; M5 will own any diagnostics for dropped facts.
    let mut stamps = Vec::new();
    let mut seen_indices = std::collections::BTreeSet::new();
    let mut seen_origins = std::collections::BTreeSet::new();
    let mut previous_index = None;
    for stamp in array(required(object, "contributionStamps")?)? {
        let Ok(stamp) = decode_stamp(stamp) else {
            continue;
        };
        let index = stamp.content_part_index() as usize;
        if index >= message.content().len()
            || previous_index.is_some_and(|previous| index <= previous)
            || seen_indices.contains(&index)
            || seen_origins.contains(stamp.origin())
        {
            continue;
        }
        seen_indices.insert(index);
        seen_origins.insert(stamp.origin().clone());
        previous_index = Some(index);
        stamps.push(stamp);
    }
    CanonicalUserMessage::reconstruct(message, stamps)
        .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_user_part(node: &JsonNode) -> Result<MessageContent, ConversationCodecError> {
    let object = as_object(node)?;
    if string(required(object, "type")?)? != "text" {
        return Err(ConversationCodecError::UnknownBodyVariant);
    }
    let data = as_object(required(object, "data")?)?;
    MessageContent::reconstruct_text(string(required(data, "text")?)?)
        .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_stamp(node: &JsonNode) -> Result<PromptContributionStamp, ConversationCodecError> {
    let object = as_object(node)?;
    let content_part_index = u32_value(required(object, "contentPartIndex")?)?;
    let origin = as_object(required(object, "origin")?)?;
    let origin_type = string(required(origin, "type")?)?;
    let data = as_object(required(origin, "data")?)?;
    let origin = match origin_type {
        "skill" => PromptContributionOrigin::Skill {
            skill_id: scalar(required(data, "skillId")?)?,
        },
        "workspace" => PromptContributionOrigin::Workspace {
            root_key: scalar(required(data, "rootKey")?)?,
            relative_location: scalar(required(data, "relativeLocation")?)?,
        },
        _ => return Err(ConversationCodecError::UnknownBodyVariant),
    };
    PromptContributionStamp::reconstruct(content_part_index, origin)
        .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_assistant_message(
    node: &JsonNode,
    tool_call_arguments: &mut std::vec::IntoIter<BoundedJsonObject>,
) -> Result<StoredAssistantMessage, ConversationCodecError> {
    let object = as_object(node)?;
    let disposition = match string(required(object, "disposition")?)? {
        "intermediate" => AssistantDisposition::Intermediate,
        "final" => AssistantDisposition::Final,
        _ => return Err(ConversationCodecError::InvalidScalar),
    };
    let content = array(required(object, "content")?)?
        .iter()
        .map(|value| decode_assistant_content(value, tool_call_arguments))
        .collect::<Result<Vec<_>, _>>()?;
    let model = decode_model_summary(required(object, "model")?)?;
    let response_id = optional_scalar(object.get("responseId"))?;
    let finish_reason = decode_finish_reason(required(object, "finishReason")?)?;
    let effective_max_output_tokens = nonzero_u32(required(object, "effectiveMaxOutputTokens")?)?;
    let usage = optional_model_usage(object.get("usage"))?;
    let logical_retry_count = u8_value(required(object, "logicalRetryCount")?)?;
    let metadata = decode_metadata(required(object, "metadata")?)?;
    StoredAssistantMessage::reconstruct(
        disposition,
        content,
        model,
        response_id,
        finish_reason,
        effective_max_output_tokens,
        usage,
        logical_retry_count,
        metadata,
    )
    .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_assistant_content(
    node: &JsonNode,
    tool_call_arguments: &mut std::vec::IntoIter<BoundedJsonObject>,
) -> Result<StoredAssistantContent, ConversationCodecError> {
    let object = as_object(node)?;
    let data = as_object(required(object, "data")?)?;
    match string(required(object, "type")?)? {
        "reasoning" => Ok(StoredAssistantContent::Reasoning {
            item_id: scalar(required(data, "itemId")?)?,
            content: decode_reasoning(required(data, "content")?)?,
        }),
        "text" => {
            let text = string(required(data, "text")?)?;
            validate_safe_text(text, 65_536, false)
                .map_err(|_| ConversationCodecError::InvalidSemantic)?;
            Ok(StoredAssistantContent::Text {
                item_id: scalar(required(data, "itemId")?)?,
                text: text.into(),
            })
        }
        "tool_call" => Ok(StoredAssistantContent::ToolCall {
            item_id: scalar(required(data, "itemId")?)?,
            tool_call_id: scalar(required(data, "toolCallId")?)?,
            name: scalar(required(data, "name")?)?,
            arguments: tool_call_arguments
                .next()
                .ok_or(ConversationCodecError::InvalidShape)?,
        }),
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_reasoning(node: &JsonNode) -> Result<ReasoningContent, ConversationCodecError> {
    let object = as_object(node)?;
    ReasoningContent::reconstruct(
        optional_string(object.get("text"))?,
        optional_string(object.get("summary"))?,
        optional_string(object.get("encrypted"))?,
        optional_string(object.get("signature"))?,
        optional_scalar(object.get("providerItemId"))?,
    )
    .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_tool_message(node: &JsonNode) -> Result<StoredToolMessage, ConversationCodecError> {
    let object = as_object(node)?;
    let item_id = scalar(required(object, "itemId")?)?;
    let tool_call_id = scalar(required(object, "toolCallId")?)?;
    let outcome = decode_tool_outcome(required(object, "outcome")?)?;
    Ok(StoredToolMessage::reconstruct(
        item_id,
        tool_call_id,
        outcome,
    ))
}

fn decode_tool_outcome(node: &JsonNode) -> Result<StoredToolOutcome, ConversationCodecError> {
    let object = as_object(node)?;
    match string(required(object, "type")?)? {
        "completed" => {
            let data = as_object(required(object, "data")?)?;
            let source = match string(required(data, "source")?)? {
                "pre_execution" => ToolOutcomeSource::PreExecution,
                "executed" => ToolOutcomeSource::Executed,
                _ => return Err(ConversationCodecError::InvalidScalar),
            };
            let disposition = match string(required(data, "disposition")?)? {
                "succeeded" => ToolResultDisposition::Succeeded,
                "failed" => ToolResultDisposition::Failed,
                "denied" => ToolResultDisposition::Denied,
                "cancelled" => ToolResultDisposition::Cancelled,
                _ => return Err(ConversationCodecError::InvalidScalar),
            };
            let content = decode_tool_content(required(data, "content")?)?;
            StoredToolOutcome::completed(source, disposition, content)
                .map_err(|_| ConversationCodecError::InvalidSemantic)
        }
        "abandoned" => {
            let data = as_object(required(object, "data")?)?;
            let reason = match string(required(data, "reason")?)? {
                "outcome_unknown" => ToolAbandonReason::OutcomeUnknown,
                "runtime_failure" => ToolAbandonReason::RuntimeFailure,
                _ => return Err(ConversationCodecError::InvalidScalar),
            };
            Ok(StoredToolOutcome::Abandoned { reason })
        }
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_tool_content(node: &JsonNode) -> Result<ToolResultContent, ConversationCodecError> {
    let object = as_object(node)?;
    let mut parts = Vec::new();
    for part in array(required(object, "parts")?)? {
        let part = as_object(part)?;
        if string(required(part, "type")?)? != "text" {
            return Err(ConversationCodecError::UnknownBodyVariant);
        }
        let data = as_object(required(part, "data")?)?;
        parts.push(string(required(data, "text")?)?.to_owned());
    }
    ToolResultContent::from_text_parts(parts).map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_interaction_request(
    node: &JsonNode,
) -> Result<StoredInteractionRequest, ConversationCodecError> {
    let object = as_object(node)?;
    let request_id = scalar(required(object, "requestId")?)?;
    let item_id = scalar(required(object, "itemId")?)?;
    let request = decode_interaction_request_body(required(object, "request")?)?;
    Ok(StoredInteractionRequest::reconstruct(
        request_id, item_id, request,
    ))
}

fn decode_interaction_request_body(
    node: &JsonNode,
) -> Result<StoredInteractionRequestBody, ConversationCodecError> {
    let object = as_object(node)?;
    let data = required(object, "data")?;
    match string(required(object, "type")?)? {
        "tool_approval" => {
            decode_approval_request(data).map(StoredInteractionRequestBody::ToolApproval)
        }
        "user_question" => {
            decode_question_request(data).map(StoredInteractionRequestBody::UserQuestion)
        }
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_approval_request(
    node: &JsonNode,
) -> Result<ToolApprovalRequestView, ConversationCodecError> {
    let object = as_object(node)?;
    let requirements = decode_requirements(required(object, "requirements")?)?;
    let options = array(required(object, "options")?)?
        .iter()
        .map(decode_approval_option)
        .collect::<Result<Vec<_>, _>>()?;
    ToolApprovalRequestView::reconstruct(
        scalar(required(object, "toolName")?)?,
        string(required(object, "argumentsSummary")?)?,
        string(required(object, "reason")?)?,
        requirements,
        options,
    )
    .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_requirements(
    node: &JsonNode,
) -> Result<ToolRequirementSummaryView, ConversationCodecError> {
    let object = as_object(node)?;
    ToolRequirementSummaryView::reconstruct(
        optional_string(object.get("filesystem"))?,
        optional_string(object.get("network"))?,
        optional_string(object.get("process"))?,
    )
    .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_approval_option(
    node: &JsonNode,
) -> Result<ToolApprovalOptionView, ConversationCodecError> {
    let object = as_object(node)?;
    let kind = match string(required(object, "kind")?)? {
        "as_requested" => ToolApprovalOptionKindView::AsRequested,
        "restricted" => ToolApprovalOptionKindView::Restricted,
        _ => return Err(ConversationCodecError::InvalidScalar),
    };
    ToolApprovalOptionView::reconstruct(
        u32_value(required(object, "optionIndex")?)?,
        kind,
        string(required(object, "label")?)?,
        decode_requirements(required(object, "effectiveRequirements")?)?,
    )
    .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_question_request(node: &JsonNode) -> Result<UserQuestionRequest, ConversationCodecError> {
    let object = as_object(node)?;
    let questions = array(required(object, "questions")?)?
        .iter()
        .map(decode_question_field)
        .collect::<Result<Vec<_>, _>>()?;
    UserQuestionRequest::reconstruct(optional_string(object.get("title"))?, questions)
        .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_question_field(node: &JsonNode) -> Result<UserQuestionField, ConversationCodecError> {
    let object = as_object(node)?;
    UserQuestionField::reconstruct(
        u32_value(required(object, "questionIndex")?)?,
        string(required(object, "prompt")?)?,
        bool_value(required(object, "required")?)?,
        decode_question_input(required(object, "input")?)?,
    )
    .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_question_input(node: &JsonNode) -> Result<UserQuestionInput, ConversationCodecError> {
    let object = as_object(node)?;
    let data = as_object(required(object, "data")?)?;
    match string(required(object, "type")?)? {
        "text" => Ok(UserQuestionInput::Text {
            multiline: bool_value(required(data, "multiline")?)?,
        }),
        "single_choice" => Ok(UserQuestionInput::SingleChoice {
            options: array(required(data, "options")?)?
                .iter()
                .map(|option| {
                    let option = as_object(option)?;
                    UserQuestionChoice::reconstruct(
                        u32_value(required(option, "optionIndex")?)?,
                        string(required(option, "label")?)?,
                    )
                    .map_err(|_| ConversationCodecError::InvalidSemantic)
                })
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        }),
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_interaction_resolution(
    node: &JsonNode,
) -> Result<StoredInteractionResolution, ConversationCodecError> {
    let object = as_object(node)?;
    let request_id = scalar(required(object, "requestId")?)?;
    let item_id = scalar(required(object, "itemId")?)?;
    let resolution = decode_interaction_resolution_body(required(object, "resolution")?)?;
    let resolution_key = optional_scalar(object.get("resolutionKey"))?;
    validate_resolution_key(&resolution, resolution_key.as_ref())?;
    StoredInteractionResolution::reconstruct(request_id, item_id, resolution, resolution_key)
        .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_interaction_resolution_body(
    node: &JsonNode,
) -> Result<StoredInteractionResolutionBody, ConversationCodecError> {
    let object = as_object(node)?;
    match string(required(object, "type")?)? {
        "tool_approval" => {
            let resolution = decode_approval_resolution(required(object, "data")?)?;
            Ok(StoredInteractionResolutionBody::ToolApproval(resolution))
        }
        "user_answer" => {
            let answer = decode_user_answer(required(object, "data")?)?;
            Ok(StoredInteractionResolutionBody::UserAnswer(answer))
        }
        "cancelled" => Ok(StoredInteractionResolutionBody::Cancelled(
            decode_cancel_reason(required(object, "data")?)?,
        )),
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_approval_resolution(
    node: &JsonNode,
) -> Result<ToolApprovalResolution, ConversationCodecError> {
    let object = as_object(node)?;
    match string(required(object, "type")?)? {
        "allowed" => {
            let data = as_object(required(object, "data")?)?;
            let kind = match string(required(data, "kind")?)? {
                "as_requested" => ToolApprovalOptionKindView::AsRequested,
                "restricted" => ToolApprovalOptionKindView::Restricted,
                _ => return Err(ConversationCodecError::InvalidScalar),
            };
            Ok(ToolApprovalResolution::reconstruct_allowed(
                u32_value(required(data, "optionIndex")?)?,
                kind,
            ))
        }
        "denied" => {
            if object.contains_key("data") {
                return Err(ConversationCodecError::InvalidShape);
            }
            Ok(ToolApprovalResolution::reconstruct_denied())
        }
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_user_answer(node: &JsonNode) -> Result<UserQuestionAnswer, ConversationCodecError> {
    let object = as_object(node)?;
    let answers = array(required(object, "answers")?)?
        .iter()
        .map(|answer| {
            let answer = as_object(answer)?;
            let index = u32_value(required(answer, "questionIndex")?)?;
            let value = decode_answer_value(required(answer, "value")?)?;
            Ok(match value {
                UserQuestionAnswerValue::Text(value) => UserQuestionFieldAnswer::text(index, value)
                    .map_err(|_| ConversationCodecError::InvalidSemantic)?,
                UserQuestionAnswerValue::Choice { option_index } => {
                    UserQuestionFieldAnswer::choice(index, option_index)
                }
            })
        })
        .collect::<Result<Vec<_>, ConversationCodecError>>()?;
    UserQuestionAnswer::new(answers).map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn decode_answer_value(node: &JsonNode) -> Result<UserQuestionAnswerValue, ConversationCodecError> {
    let object = as_object(node)?;
    match string(required(object, "type")?)? {
        "text" => Ok(UserQuestionAnswerValue::Text(
            string(required(object, "data")?)?.into(),
        )),
        "choice" => {
            let data = as_object(required(object, "data")?)?;
            Ok(UserQuestionAnswerValue::Choice {
                option_index: u32_value(required(data, "optionIndex")?)?,
            })
        }
        _ => Err(ConversationCodecError::UnknownBodyVariant),
    }
}

fn decode_cancel_reason(
    node: &JsonNode,
) -> Result<InteractionCancelReason, ConversationCodecError> {
    match string(node)? {
        "host_cancelled" => Ok(InteractionCancelReason::HostCancelled),
        "turn_cancelled" => Ok(InteractionCancelReason::TurnCancelled),
        "security_revoked" => Ok(InteractionCancelReason::SecurityRevoked),
        "session_unloaded" => Ok(InteractionCancelReason::SessionUnloaded),
        "runtime_closing" => Ok(InteractionCancelReason::RuntimeClosing),
        "turn_terminal" => Ok(InteractionCancelReason::TurnTerminal),
        _ => Err(ConversationCodecError::InvalidScalar),
    }
}

fn validate_resolution_key(
    resolution: &StoredInteractionResolutionBody,
    resolution_key: Option<&InteractionResolutionKey>,
) -> Result<(), ConversationCodecError> {
    let expected_key = match resolution {
        StoredInteractionResolutionBody::ToolApproval(_)
        | StoredInteractionResolutionBody::UserAnswer(_) => true,
        StoredInteractionResolutionBody::Cancelled(InteractionCancelReason::HostCancelled) => true,
        StoredInteractionResolutionBody::Cancelled(_) => false,
    };
    if expected_key != resolution_key.is_some() {
        return Err(ConversationCodecError::InvalidSemantic);
    }
    Ok(())
}

fn decode_compaction(node: &JsonNode) -> Result<StoredCompaction, ConversationCodecError> {
    let object = as_object(node)?;
    let summary = string(required(object, "summary")?)?;
    let first_kept_entry_id = optional_scalar(object.get("firstKeptEntryId"))?;
    let model_call = optional_compaction_model_call(object.get("modelCall"))?;
    StoredCompaction::reconstruct(summary, first_kept_entry_id, model_call)
        .map_err(|_| ConversationCodecError::InvalidSemantic)
}

fn optional_compaction_model_call(
    node: Option<&JsonNode>,
) -> Result<Option<StoredCompactionModelCall>, ConversationCodecError> {
    let Some(node) = node else {
        return Ok(None);
    };
    if matches!(node, JsonNode::Null) {
        return Ok(None);
    }
    let object = as_object(node)?;
    Ok(Some(
        StoredCompactionModelCall::reconstruct(
            decode_model_summary(required(object, "model")?)?,
            optional_scalar(object.get("responseId"))?,
            optional_model_usage(object.get("usage"))?,
            decode_finish_reason(required(object, "finishReason")?)?,
            nonzero_u32(required(object, "requestedMaxOutputTokens")?)?,
            u8_value(required(object, "logicalRetryCount")?)?,
            decode_metadata(required(object, "metadata")?)?,
        )
        .map_err(|_| ConversationCodecError::InvalidSemantic)?,
    ))
}

fn decode_model_summary(node: &JsonNode) -> Result<ModelResponseSummary, ConversationCodecError> {
    let object = as_object(node)?;
    let reasoning = match string(required(object, "reasoning")?)? {
        "provider_default" => ModelReasoningSummary::ProviderDefault,
        "disabled" => ModelReasoningSummary::Disabled,
        "low" => ModelReasoningSummary::Low,
        "medium" => ModelReasoningSummary::Medium,
        "high" => ModelReasoningSummary::High,
        _ => return Err(ConversationCodecError::InvalidScalar),
    };
    let service_class = match string(required(object, "serviceClass")?)? {
        "standard" => ModelServiceClass::Standard,
        "priority" => ModelServiceClass::Priority,
        _ => return Err(ConversationCodecError::InvalidScalar),
    };
    Ok(ModelResponseSummary::reconstruct(
        scalar(required(object, "providerId")?)?,
        scalar(required(object, "modelId")?)?,
        reasoning,
        service_class,
    ))
}

fn decode_finish_reason(node: &JsonNode) -> Result<ModelFinishReason, ConversationCodecError> {
    match string(node)? {
        "stop" => Ok(ModelFinishReason::Stop),
        "tool_calls" => Ok(ModelFinishReason::ToolCalls),
        "length" => Ok(ModelFinishReason::Length),
        "content_filtered" => Ok(ModelFinishReason::ContentFiltered),
        "refused" => Ok(ModelFinishReason::Refused),
        "unknown" => Ok(ModelFinishReason::Unknown),
        _ => Err(ConversationCodecError::InvalidScalar),
    }
}

fn decode_metadata(node: &JsonNode) -> Result<ProviderResponseMetadata, ConversationCodecError> {
    let object = as_object(node)?;
    Ok(ProviderResponseMetadata::reconstruct(
        optional_scalar(object.get("providerRequestId"))?,
        optional_scalar(object.get("rawFinishCode"))?,
        optional_scalar(object.get("serviceTier"))?,
    ))
}

fn optional_model_usage(
    node: Option<&JsonNode>,
) -> Result<Option<ModelUsage>, ConversationCodecError> {
    let Some(node) = node else {
        return Ok(None);
    };
    if matches!(node, JsonNode::Null) {
        return Ok(None);
    }
    let object = as_object(node)?;
    Ok(Some(ModelUsage::reconstruct(
        optional_u64_string(object.get("inputTokens"))?,
        optional_u64_string(object.get("outputTokens"))?,
        optional_u64_string(object.get("reasoningTokens"))?,
        optional_u64_string(object.get("cacheReadTokens"))?,
        optional_u64_string(object.get("cacheWriteTokens"))?,
        optional_u64_string(object.get("providerTotalTokens"))?,
        optional_money(object.get("reportedCost"))?,
    )))
}

fn optional_money(node: Option<&JsonNode>) -> Result<Option<Money>, ConversationCodecError> {
    let Some(node) = node else {
        return Ok(None);
    };
    if matches!(node, JsonNode::Null) {
        return Ok(None);
    }
    let object = as_object(node)?;
    Ok(Some(Money::new(
        scalar(required(object, "amount")?)?,
        scalar(required(object, "currency")?)?,
    )))
}

fn as_object(node: &JsonNode) -> Result<&BTreeMap<Box<str>, JsonNode>, ConversationCodecError> {
    node.as_object().ok_or(ConversationCodecError::InvalidShape)
}

fn array(node: &JsonNode) -> Result<&[JsonNode], ConversationCodecError> {
    node.as_array().ok_or(ConversationCodecError::InvalidShape)
}

fn string(node: &JsonNode) -> Result<&str, ConversationCodecError> {
    node.as_str().ok_or(ConversationCodecError::InvalidScalar)
}

fn required<'a>(
    object: &'a BTreeMap<Box<str>, JsonNode>,
    field: &str,
) -> Result<&'a JsonNode, ConversationCodecError> {
    object
        .get(field)
        .ok_or(ConversationCodecError::MissingRequiredField)
}

fn strict_fields(
    object: &BTreeMap<Box<str>, JsonNode>,
    fields: &[&str],
) -> Result<(), ConversationCodecError> {
    if object.len() != fields.len() || object.keys().any(|key| !fields.contains(&key.as_ref())) {
        return Err(ConversationCodecError::InvalidShape);
    }
    Ok(())
}

fn scalar<T: FromStr>(node: &JsonNode) -> Result<T, ConversationCodecError> {
    string(node)?
        .parse()
        .map_err(|_| ConversationCodecError::InvalidScalar)
}

fn optional_scalar<T: FromStr>(
    node: Option<&JsonNode>,
) -> Result<Option<T>, ConversationCodecError> {
    match node {
        None | Some(JsonNode::Null) => Ok(None),
        Some(value) => scalar(value).map(Some),
    }
}

fn optional_string(node: Option<&JsonNode>) -> Result<Option<String>, ConversationCodecError> {
    match node {
        None | Some(JsonNode::Null) => Ok(None),
        Some(value) => Ok(Some(string(value)?.to_owned())),
    }
}

fn optional_u64_string(node: Option<&JsonNode>) -> Result<Option<u64>, ConversationCodecError> {
    match node {
        None | Some(JsonNode::Null) => Ok(None),
        Some(value) => string(value)?
            .parse::<crate::wire::CanonicalU64>()
            .map(|value| Some(value.get()))
            .map_err(|_| ConversationCodecError::InvalidScalar),
    }
}

fn bool_value(node: &JsonNode) -> Result<bool, ConversationCodecError> {
    match node {
        JsonNode::Bool(value) => Ok(*value),
        _ => Err(ConversationCodecError::InvalidScalar),
    }
}

fn u32_value(node: &JsonNode) -> Result<u32, ConversationCodecError> {
    let raw = node
        .as_number()
        .map(|value| value.raw())
        .ok_or(ConversationCodecError::InvalidScalar)?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ConversationCodecError::InvalidScalar);
    }
    raw.parse()
        .map_err(|_| ConversationCodecError::InvalidScalar)
}

fn u8_value(node: &JsonNode) -> Result<u8, ConversationCodecError> {
    u32_value(node)?
        .try_into()
        .map_err(|_| ConversationCodecError::InvalidScalar)
}

fn nonzero_u32(node: &JsonNode) -> Result<NonZeroU32, ConversationCodecError> {
    NonZeroU32::new(u32_value(node)?).ok_or(ConversationCodecError::InvalidScalar)
}

fn encode_header_data(
    writer: &mut JsonWriter,
    header: &SessionHeader,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("formatVersion")?;
    writer.u32(header.format_version())?;
    writer.comma()?;
    writer.key("sessionId")?;
    writer.string(&header.session_id().to_string())?;
    writer.comma()?;
    writer.key("createdAt")?;
    writer.string(&header.created_at().to_string())?;
    writer.comma()?;
    writer.key("initialAgent")?;
    writer.object_start()?;
    writer.key("agentId")?;
    writer.string(&header.initial_agent().agent_id().to_string())?;
    writer.comma()?;
    writer.key("revision")?;
    writer.string(&header.initial_agent().revision().to_string())?;
    writer.object_end()?;
    writer.comma()?;
    writer.key("initialDefinitionRevision")?;
    writer.string(&header.initial_definition_revision().to_string())?;
    writer.object_end()
}

fn encode_entry_data(
    writer: &mut JsonWriter,
    entry: &StoredSessionEntry,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("entryId")?;
    writer.string(&entry.entry_id().to_string())?;
    writer.comma()?;
    writer.key("parentId")?;
    optional_display(writer, entry.parent_id())?;
    writer.comma()?;
    writer.key("sessionId")?;
    writer.string(&entry.session_id().to_string())?;
    writer.comma()?;
    writer.key("turnId")?;
    writer.string(&entry.turn_id().to_string())?;
    writer.comma()?;
    writer.key("timestamp")?;
    writer.string(&entry.timestamp().to_string())?;
    writer.comma()?;
    writer.key("body")?;
    encode_body(writer, entry.body())?;
    writer.object_end()
}

fn adjacent_start(writer: &mut JsonWriter, tag: &str) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("type")?;
    writer.string(tag)?;
    writer.comma()?;
    writer.key("data")?;
    Ok(())
}

fn encode_body(
    writer: &mut JsonWriter,
    body: &StoredEntryBody,
) -> Result<(), ConversationCodecError> {
    match body {
        StoredEntryBody::UserMessage(value) => {
            adjacent_start(writer, "user_message")?;
            encode_user_message(writer, value)?;
            writer.object_end()
        }
        StoredEntryBody::AssistantMessage(value) => {
            adjacent_start(writer, "assistant_message")?;
            encode_assistant_message(writer, value)?;
            writer.object_end()
        }
        StoredEntryBody::ToolMessage(value) => {
            adjacent_start(writer, "tool_message")?;
            encode_tool_message(writer, value)?;
            writer.object_end()
        }
        StoredEntryBody::InteractionRequested(value) => {
            adjacent_start(writer, "interaction_requested")?;
            encode_interaction_request(writer, value)?;
            writer.object_end()
        }
        StoredEntryBody::InteractionResolved(value) => {
            adjacent_start(writer, "interaction_resolved")?;
            encode_interaction_resolution(writer, value)?;
            writer.object_end()
        }
        StoredEntryBody::Compaction(value) => {
            adjacent_start(writer, "compaction")?;
            encode_compaction(writer, value)?;
            writer.object_end()
        }
    }
}

fn encode_user_message(
    writer: &mut JsonWriter,
    message: &StoredUserMessage,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("itemId")?;
    writer.string(&message.item_id().to_string())?;
    writer.comma()?;
    writer.key("source")?;
    writer.string(match message.source() {
        UserMessageSource::Input => "input",
        UserMessageSource::Steer => "steer",
    })?;
    writer.comma()?;
    writer.key("content")?;
    writer.object_start()?;
    writer.key("parts")?;
    writer.array_start()?;
    for (index, part) in message.content().message().content().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        adjacent_start(writer, "text")?;
        writer.object_start()?;
        writer.key("text")?;
        writer.string(part.as_text())?;
        writer.object_end()?;
        writer.object_end()?;
    }
    writer.array_end()?;
    writer.comma()?;
    writer.key("contributionStamps")?;
    writer.array_start()?;
    for (index, stamp) in message.content().contribution_stamps().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        writer.object_start()?;
        writer.key("contentPartIndex")?;
        writer.u32(stamp.content_part_index())?;
        writer.comma()?;
        writer.key("origin")?;
        match stamp.origin() {
            PromptContributionOrigin::Skill { skill_id } => {
                adjacent_start(writer, "skill")?;
                writer.object_start()?;
                writer.key("skillId")?;
                writer.string(skill_id.as_str())?;
                writer.object_end()?;
                writer.object_end()?;
            }
            PromptContributionOrigin::Workspace {
                root_key,
                relative_location,
            } => {
                adjacent_start(writer, "workspace")?;
                writer.object_start()?;
                writer.key("rootKey")?;
                writer.string(root_key.as_str())?;
                writer.comma()?;
                writer.key("relativeLocation")?;
                writer.string(relative_location.as_str())?;
                writer.object_end()?;
                writer.object_end()?;
            }
        }
        writer.object_end()?;
    }
    writer.array_end()?;
    writer.object_end()?;
    writer.object_end()
}

fn encode_assistant_message(
    writer: &mut JsonWriter,
    message: &StoredAssistantMessage,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("disposition")?;
    writer.string(match message.disposition() {
        AssistantDisposition::Intermediate => "intermediate",
        AssistantDisposition::Final => "final",
    })?;
    writer.comma()?;
    writer.key("content")?;
    writer.array_start()?;
    for (index, content) in message.content().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        encode_assistant_content(writer, content)?;
    }
    writer.array_end()?;
    writer.comma()?;
    writer.key("model")?;
    encode_model_summary(writer, message.model())?;
    writer.comma()?;
    writer.key("responseId")?;
    optional_opaque(writer, message.response_id())?;
    writer.comma()?;
    writer.key("finishReason")?;
    writer.string(finish_reason_name(message.finish_reason()))?;
    writer.comma()?;
    writer.key("effectiveMaxOutputTokens")?;
    writer.u32(message.effective_max_output_tokens().get())?;
    writer.comma()?;
    writer.key("usage")?;
    optional_usage(writer, message.usage())?;
    writer.comma()?;
    writer.key("logicalRetryCount")?;
    writer.u32(u32::from(message.logical_retry_count()))?;
    writer.comma()?;
    writer.key("metadata")?;
    encode_metadata(writer, message.metadata())?;
    writer.object_end()
}

fn encode_assistant_content(
    writer: &mut JsonWriter,
    content: &StoredAssistantContent,
) -> Result<(), ConversationCodecError> {
    match content {
        StoredAssistantContent::Reasoning { item_id, content } => {
            adjacent_start(writer, "reasoning")?;
            writer.object_start()?;
            writer.key("itemId")?;
            writer.string(&item_id.to_string())?;
            writer.comma()?;
            writer.key("content")?;
            encode_reasoning(writer, content)?;
            writer.object_end()?;
            writer.object_end()
        }
        StoredAssistantContent::Text { item_id, text } => {
            adjacent_start(writer, "text")?;
            writer.object_start()?;
            writer.key("itemId")?;
            writer.string(&item_id.to_string())?;
            writer.comma()?;
            writer.key("text")?;
            writer.string(text)?;
            writer.object_end()?;
            writer.object_end()
        }
        StoredAssistantContent::ToolCall {
            item_id,
            tool_call_id,
            name,
            arguments,
        } => {
            adjacent_start(writer, "tool_call")?;
            writer.object_start()?;
            writer.key("itemId")?;
            writer.string(&item_id.to_string())?;
            writer.comma()?;
            writer.key("toolCallId")?;
            writer.string(tool_call_id.as_str())?;
            writer.comma()?;
            writer.key("name")?;
            writer.string(name.as_str())?;
            writer.comma()?;
            writer.key("arguments")?;
            writer.bytes(arguments.canonical_bytes())?;
            writer.object_end()?;
            writer.object_end()
        }
    }
}

fn encode_reasoning(
    writer: &mut JsonWriter,
    content: &ReasoningContent,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("text")?;
    optional_string_ref(writer, content.text())?;
    writer.comma()?;
    writer.key("summary")?;
    optional_string_ref(writer, content.summary())?;
    writer.comma()?;
    writer.key("encrypted")?;
    optional_string_ref(writer, content.encrypted())?;
    writer.comma()?;
    writer.key("signature")?;
    optional_string_ref(writer, content.signature())?;
    writer.comma()?;
    writer.key("providerItemId")?;
    optional_opaque(writer, content.provider_item_id())?;
    writer.object_end()
}

fn encode_tool_message(
    writer: &mut JsonWriter,
    message: &StoredToolMessage,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("itemId")?;
    writer.string(&message.item_id().to_string())?;
    writer.comma()?;
    writer.key("toolCallId")?;
    writer.string(message.tool_call_id().as_str())?;
    writer.comma()?;
    writer.key("outcome")?;
    match message.outcome() {
        StoredToolOutcome::Completed {
            source,
            disposition,
            content,
        } => {
            adjacent_start(writer, "completed")?;
            writer.object_start()?;
            writer.key("source")?;
            writer.string(match source {
                ToolOutcomeSource::PreExecution => "pre_execution",
                ToolOutcomeSource::Executed => "executed",
            })?;
            writer.comma()?;
            writer.key("disposition")?;
            writer.string(tool_disposition_name(*disposition))?;
            writer.comma()?;
            writer.key("content")?;
            encode_tool_content(writer, content)?;
            writer.object_end()?;
            writer.object_end()?;
        }
        StoredToolOutcome::Abandoned { reason } => {
            adjacent_start(writer, "abandoned")?;
            writer.object_start()?;
            writer.key("reason")?;
            writer.string(match reason {
                ToolAbandonReason::OutcomeUnknown => "outcome_unknown",
                ToolAbandonReason::RuntimeFailure => "runtime_failure",
            })?;
            writer.object_end()?;
            writer.object_end()?;
        }
    }
    writer.object_end()
}

fn encode_tool_content(
    writer: &mut JsonWriter,
    content: &ToolResultContent,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("parts")?;
    writer.array_start()?;
    for (index, part) in content.parts().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        adjacent_start(writer, "text")?;
        writer.object_start()?;
        writer.key("text")?;
        writer.string(part.as_text())?;
        writer.object_end()?;
        writer.object_end()?;
    }
    writer.array_end()?;
    writer.object_end()
}

fn encode_interaction_request(
    writer: &mut JsonWriter,
    request: &StoredInteractionRequest,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("requestId")?;
    writer.string(&request.request_id().to_string())?;
    writer.comma()?;
    writer.key("itemId")?;
    writer.string(&request.item_id().to_string())?;
    writer.comma()?;
    writer.key("request")?;
    match request.request() {
        StoredInteractionRequestBody::ToolApproval(value) => {
            adjacent_start(writer, "tool_approval")?;
            encode_approval_request(writer, value)?;
            writer.object_end()?;
        }
        StoredInteractionRequestBody::UserQuestion(value) => {
            adjacent_start(writer, "user_question")?;
            encode_question_request(writer, value)?;
            writer.object_end()?;
        }
    }
    writer.object_end()
}

fn encode_approval_request(
    writer: &mut JsonWriter,
    request: &ToolApprovalRequestView,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("toolName")?;
    writer.string(request.tool_name().as_str())?;
    writer.comma()?;
    writer.key("argumentsSummary")?;
    writer.string(request.arguments_summary())?;
    writer.comma()?;
    writer.key("reason")?;
    writer.string(request.reason())?;
    writer.comma()?;
    writer.key("requirements")?;
    encode_requirements(writer, request.requirements())?;
    writer.comma()?;
    writer.key("options")?;
    writer.array_start()?;
    for (index, option) in request.options().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        writer.object_start()?;
        writer.key("optionIndex")?;
        writer.u32(option.option_index())?;
        writer.comma()?;
        writer.key("kind")?;
        writer.string(approval_kind_name(option.kind()))?;
        writer.comma()?;
        writer.key("label")?;
        writer.string(option.label())?;
        writer.comma()?;
        writer.key("effectiveRequirements")?;
        encode_requirements(writer, option.effective_requirements())?;
        writer.object_end()?;
    }
    writer.array_end()?;
    writer.object_end()
}

fn encode_requirements(
    writer: &mut JsonWriter,
    value: &ToolRequirementSummaryView,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("filesystem")?;
    optional_string_ref(writer, value.filesystem())?;
    writer.comma()?;
    writer.key("network")?;
    optional_string_ref(writer, value.network())?;
    writer.comma()?;
    writer.key("process")?;
    optional_string_ref(writer, value.process())?;
    writer.object_end()
}

fn encode_question_request(
    writer: &mut JsonWriter,
    request: &UserQuestionRequest,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("title")?;
    optional_string_ref(writer, request.title())?;
    writer.comma()?;
    writer.key("questions")?;
    writer.array_start()?;
    for (index, question) in request.questions().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        writer.object_start()?;
        writer.key("questionIndex")?;
        writer.u32(question.question_index())?;
        writer.comma()?;
        writer.key("prompt")?;
        writer.string(question.prompt())?;
        writer.comma()?;
        writer.key("required")?;
        writer.raw(if question.required() { "true" } else { "false" })?;
        writer.comma()?;
        writer.key("input")?;
        match question.input() {
            UserQuestionInput::Text { multiline } => {
                adjacent_start(writer, "text")?;
                writer.object_start()?;
                writer.key("multiline")?;
                writer.raw(if *multiline { "true" } else { "false" })?;
                writer.object_end()?;
                writer.object_end()?;
            }
            UserQuestionInput::SingleChoice { options } => {
                adjacent_start(writer, "single_choice")?;
                writer.object_start()?;
                writer.key("options")?;
                writer.array_start()?;
                for (option_index, option) in options.iter().enumerate() {
                    if option_index != 0 {
                        writer.comma()?;
                    }
                    writer.object_start()?;
                    writer.key("optionIndex")?;
                    writer.u32(option.option_index())?;
                    writer.comma()?;
                    writer.key("label")?;
                    writer.string(option.label())?;
                    writer.object_end()?;
                }
                writer.array_end()?;
                writer.object_end()?;
                writer.object_end()?;
            }
        }
        writer.object_end()?;
    }
    writer.array_end()?;
    writer.object_end()
}

fn encode_interaction_resolution(
    writer: &mut JsonWriter,
    resolution: &StoredInteractionResolution,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("requestId")?;
    writer.string(&resolution.request_id().to_string())?;
    writer.comma()?;
    writer.key("itemId")?;
    writer.string(&resolution.item_id().to_string())?;
    writer.comma()?;
    writer.key("resolution")?;
    match resolution.resolution() {
        StoredInteractionResolutionBody::ToolApproval(value) => {
            adjacent_start(writer, "tool_approval")?;
            encode_approval_resolution(writer, value)?;
            writer.object_end()?;
        }
        StoredInteractionResolutionBody::UserAnswer(value) => {
            adjacent_start(writer, "user_answer")?;
            encode_user_answer(writer, value)?;
            writer.object_end()?;
        }
        StoredInteractionResolutionBody::Cancelled(reason) => {
            adjacent_start(writer, "cancelled")?;
            writer.string(cancel_reason_name(*reason))?;
            writer.object_end()?;
        }
    }
    writer.comma()?;
    writer.key("resolutionKey")?;
    optional_resolution_key(writer, resolution.resolution_key())?;
    writer.object_end()
}

fn encode_approval_resolution(
    writer: &mut JsonWriter,
    resolution: &ToolApprovalResolution,
) -> Result<(), ConversationCodecError> {
    match resolution.as_ref() {
        ToolApprovalResolutionRef::Allowed { option_index, kind } => {
            adjacent_start(writer, "allowed")?;
            writer.object_start()?;
            writer.key("optionIndex")?;
            writer.u32(option_index)?;
            writer.comma()?;
            writer.key("kind")?;
            writer.string(approval_kind_name(kind))?;
            writer.object_end()?;
            writer.object_end()
        }
        ToolApprovalResolutionRef::Denied => {
            writer.object_start()?;
            writer.key("type")?;
            writer.string("denied")?;
            writer.object_end()
        }
    }
}

fn encode_user_answer(
    writer: &mut JsonWriter,
    answer: &UserQuestionAnswer,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("answers")?;
    writer.array_start()?;
    for (index, answer) in answer.answers().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        writer.object_start()?;
        writer.key("questionIndex")?;
        writer.u32(answer.question_index())?;
        writer.comma()?;
        writer.key("value")?;
        match answer.value() {
            UserQuestionAnswerValue::Text(value) => {
                adjacent_start(writer, "text")?;
                writer.string(value)?;
                writer.object_end()?;
            }
            UserQuestionAnswerValue::Choice { option_index } => {
                adjacent_start(writer, "choice")?;
                writer.object_start()?;
                writer.key("optionIndex")?;
                writer.u32(*option_index)?;
                writer.object_end()?;
                writer.object_end()?;
            }
        }
        writer.object_end()?;
    }
    writer.array_end()?;
    writer.object_end()
}

fn encode_compaction(
    writer: &mut JsonWriter,
    value: &StoredCompaction,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("summary")?;
    writer.string(value.summary())?;
    writer.comma()?;
    writer.key("firstKeptEntryId")?;
    optional_display(writer, value.first_kept_entry_id())?;
    writer.comma()?;
    writer.key("modelCall")?;
    match value.model_call() {
        None => writer.raw("null")?,
        Some(value) => {
            writer.object_start()?;
            writer.key("model")?;
            encode_model_summary(writer, value.model())?;
            writer.comma()?;
            writer.key("responseId")?;
            optional_opaque(writer, value.response_id())?;
            writer.comma()?;
            writer.key("usage")?;
            optional_usage(writer, value.usage())?;
            writer.comma()?;
            writer.key("finishReason")?;
            writer.string(finish_reason_name(value.finish_reason()))?;
            writer.comma()?;
            writer.key("requestedMaxOutputTokens")?;
            writer.u32(value.requested_max_output_tokens().get())?;
            writer.comma()?;
            writer.key("logicalRetryCount")?;
            writer.u32(u32::from(value.logical_retry_count()))?;
            writer.comma()?;
            writer.key("metadata")?;
            encode_metadata(writer, value.metadata())?;
            writer.object_end()?;
        }
    }
    writer.object_end()
}

fn encode_model_summary(
    writer: &mut JsonWriter,
    model: &ModelResponseSummary,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("providerId")?;
    writer.string(model.provider_id().as_str())?;
    writer.comma()?;
    writer.key("modelId")?;
    writer.string(model.model_id().as_str())?;
    writer.comma()?;
    writer.key("reasoning")?;
    writer.string(match model.reasoning() {
        ModelReasoningSummary::ProviderDefault => "provider_default",
        ModelReasoningSummary::Disabled => "disabled",
        ModelReasoningSummary::Low => "low",
        ModelReasoningSummary::Medium => "medium",
        ModelReasoningSummary::High => "high",
    })?;
    writer.comma()?;
    writer.key("serviceClass")?;
    writer.string(match model.service_class() {
        ModelServiceClass::Standard => "standard",
        ModelServiceClass::Priority => "priority",
    })?;
    writer.object_end()
}

fn optional_usage(
    writer: &mut JsonWriter,
    usage: Option<&ModelUsage>,
) -> Result<(), ConversationCodecError> {
    let Some(usage) = usage else {
        return writer.raw("null");
    };
    writer.object_start()?;
    writer.key("inputTokens")?;
    optional_u64(writer, usage.input_tokens())?;
    writer.comma()?;
    writer.key("outputTokens")?;
    optional_u64(writer, usage.output_tokens())?;
    writer.comma()?;
    writer.key("reasoningTokens")?;
    optional_u64(writer, usage.reasoning_tokens())?;
    writer.comma()?;
    writer.key("cacheReadTokens")?;
    optional_u64(writer, usage.cache_read_tokens())?;
    writer.comma()?;
    writer.key("cacheWriteTokens")?;
    optional_u64(writer, usage.cache_write_tokens())?;
    writer.comma()?;
    writer.key("providerTotalTokens")?;
    optional_u64(writer, usage.provider_total_tokens())?;
    writer.comma()?;
    writer.key("reportedCost")?;
    match usage.reported_cost() {
        None => writer.raw("null")?,
        Some(value) => {
            writer.object_start()?;
            writer.key("amount")?;
            writer.string(&value.amount().to_string())?;
            writer.comma()?;
            writer.key("currency")?;
            writer.string(value.currency().as_str())?;
            writer.object_end()?;
        }
    }
    writer.object_end()
}

fn encode_metadata(
    writer: &mut JsonWriter,
    metadata: &ProviderResponseMetadata,
) -> Result<(), ConversationCodecError> {
    writer.object_start()?;
    writer.key("providerRequestId")?;
    optional_opaque(writer, metadata.provider_request_id())?;
    writer.comma()?;
    writer.key("rawFinishCode")?;
    optional_opaque(writer, metadata.raw_finish_code())?;
    writer.comma()?;
    writer.key("serviceTier")?;
    optional_opaque(writer, metadata.service_tier())?;
    writer.object_end()
}

fn optional_display<T: fmt::Display>(
    writer: &mut JsonWriter,
    value: Option<T>,
) -> Result<(), ConversationCodecError> {
    match value {
        Some(value) => writer.string(&value.to_string()),
        None => writer.raw("null"),
    }
}

fn optional_opaque<T: OpaqueString>(
    writer: &mut JsonWriter,
    value: Option<&T>,
) -> Result<(), ConversationCodecError> {
    match value {
        Some(value) => writer.string(value.opaque_str()),
        None => writer.raw("null"),
    }
}

trait OpaqueString {
    fn opaque_str(&self) -> &str;
}

impl OpaqueString for ProviderResponseId {
    fn opaque_str(&self) -> &str {
        self.as_str()
    }
}
impl OpaqueString for ProviderRequestId {
    fn opaque_str(&self) -> &str {
        self.as_str()
    }
}
impl OpaqueString for ProviderItemId {
    fn opaque_str(&self) -> &str {
        self.as_str()
    }
}
impl OpaqueString for RedactedProviderCode {
    fn opaque_str(&self) -> &str {
        self.as_str()
    }
}
fn optional_resolution_key(
    writer: &mut JsonWriter,
    value: Option<&InteractionResolutionKey>,
) -> Result<(), ConversationCodecError> {
    match value {
        Some(value) => writer.string(&value.encoded()),
        None => writer.raw("null"),
    }
}

fn optional_string_ref(
    writer: &mut JsonWriter,
    value: Option<&str>,
) -> Result<(), ConversationCodecError> {
    match value {
        Some(value) => writer.string(value),
        None => writer.raw("null"),
    }
}

fn optional_u64(writer: &mut JsonWriter, value: Option<u64>) -> Result<(), ConversationCodecError> {
    match value {
        Some(value) => writer.string(&value.to_string()),
        None => writer.raw("null"),
    }
}

fn finish_reason_name(value: ModelFinishReason) -> &'static str {
    match value {
        ModelFinishReason::Stop => "stop",
        ModelFinishReason::ToolCalls => "tool_calls",
        ModelFinishReason::Length => "length",
        ModelFinishReason::ContentFiltered => "content_filtered",
        ModelFinishReason::Refused => "refused",
        ModelFinishReason::Unknown => "unknown",
    }
}

fn tool_disposition_name(value: ToolResultDisposition) -> &'static str {
    match value {
        ToolResultDisposition::Succeeded => "succeeded",
        ToolResultDisposition::Failed => "failed",
        ToolResultDisposition::Denied => "denied",
        ToolResultDisposition::Cancelled => "cancelled",
    }
}

fn approval_kind_name(value: ToolApprovalOptionKindView) -> &'static str {
    match value {
        ToolApprovalOptionKindView::AsRequested => "as_requested",
        ToolApprovalOptionKindView::Restricted => "restricted",
    }
}

fn cancel_reason_name(value: InteractionCancelReason) -> &'static str {
    match value {
        InteractionCancelReason::HostCancelled => "host_cancelled",
        InteractionCancelReason::TurnCancelled => "turn_cancelled",
        InteractionCancelReason::SecurityRevoked => "security_revoked",
        InteractionCancelReason::SessionUnloaded => "session_unloaded",
        InteractionCancelReason::RuntimeClosing => "runtime_closing",
        InteractionCancelReason::TurnTerminal => "turn_terminal",
    }
}

struct JsonWriter {
    bytes: Vec<u8>,
    maximum: usize,
}

impl JsonWriter {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }

    fn finish(self) -> Result<Vec<u8>, ConversationCodecError> {
        Ok(self.bytes)
    }

    fn raw(&mut self, value: &str) -> Result<(), ConversationCodecError> {
        self.bytes(value.as_bytes())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), ConversationCodecError> {
        let next = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(ConversationCodecError::LineTooLarge)?;
        if next > self.maximum {
            return Err(ConversationCodecError::LineTooLarge);
        }
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn object_start(&mut self) -> Result<(), ConversationCodecError> {
        self.raw("{")
    }
    fn object_end(&mut self) -> Result<(), ConversationCodecError> {
        self.raw("}")
    }
    fn array_start(&mut self) -> Result<(), ConversationCodecError> {
        self.raw("[")
    }
    fn array_end(&mut self) -> Result<(), ConversationCodecError> {
        self.raw("]")
    }
    fn comma(&mut self) -> Result<(), ConversationCodecError> {
        self.raw(",")
    }

    fn key(&mut self, value: &str) -> Result<(), ConversationCodecError> {
        self.string(value)?;
        self.raw(":")
    }

    fn u32(&mut self, value: u32) -> Result<(), ConversationCodecError> {
        self.raw(&value.to_string())
    }

    fn string(&mut self, value: &str) -> Result<(), ConversationCodecError> {
        self.raw("\"")?;
        for character in value.chars() {
            match character {
                '"' => self.raw("\\\"")?,
                '\\' => self.raw("\\\\")?,
                '\u{0008}' => self.raw("\\b")?,
                '\t' => self.raw("\\t")?,
                '\n' => self.raw("\\n")?,
                '\u{000c}' => self.raw("\\f")?,
                '\r' => self.raw("\\r")?,
                '\u{0000}'..='\u{001f}' => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    let byte = character as u8;
                    self.bytes(&[
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[usize::from(byte >> 4)],
                        HEX[usize::from(byte & 15)],
                    ])?;
                }
                _ => {
                    let mut buffer = [0; 4];
                    self.raw(character.encode_utf8(&mut buffer))?;
                }
            }
        }
        self.raw("\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call_assistant_line(arguments: &str) -> Vec<u8> {
        let assistant =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/tool-exchange.jsonl")
                .split(|byte| *byte == b'\n')
                .nth(2)
                .unwrap();
        String::from_utf8(assistant.to_vec())
            .unwrap()
            .replacen(
                r#""arguments":{"city":"Paris","days":1,"units":"metric"}"#,
                &format!(r#""arguments":{arguments}"#),
                1,
            )
            .into_bytes()
    }

    #[test]
    fn every_complete_golden_line_round_trips_byte_exactly() {
        const EXPECTED_CATALOG_SESSIONS: &[(&str, &str)] = &[
            (
                "crlf-canonical.jsonl",
                "ses_15151515151515151515151515151515",
            ),
            ("header-only.jsonl", "ses_11111111111111111111111111111111"),
            (
                "interaction-compaction.jsonl",
                "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            (
                "interaction-variants.jsonl",
                "ses_14141414141414141414141414141414",
            ),
            (
                "tool-exchange.jsonl",
                "ses_11111111111111111111111111111111",
            ),
            (
                "tool-outcome-variants.jsonl",
                "ses_13131313131313131313131313131313",
            ),
            (
                "user-sources-and-stamps.jsonl",
                "ses_12121212121212121212121212121212",
            ),
        ];
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/fixtures/wire-v1/conversation/golden");
        let mut fixture_names = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".jsonl"))
            .collect::<Vec<_>>();
        fixture_names.sort_unstable();
        let mut expected_names = EXPECTED_CATALOG_SESSIONS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect::<Vec<_>>();
        expected_names.sort_unstable();
        assert_eq!(fixture_names, expected_names);

        for (fixture, expected_session_id) in EXPECTED_CATALOG_SESSIONS {
            let path = directory.join(fixture);
            let bytes = std::fs::read(&path).unwrap();
            let mut lines = bytes
                .strip_suffix(b"\n")
                .unwrap()
                .split(|byte| *byte == b'\n');
            let expected_session_id = expected_session_id.parse().unwrap();
            let header = ConversationLineCodec::decode_header_for_catalog(
                lines.next().expect("golden file has a header"),
                expected_session_id,
            )
            .unwrap_or_else(|error| panic!("{} header failed to decode: {error}", path.display()));
            assert_eq!(
                ConversationLineCodec::encode_header(&header).unwrap(),
                bytes.split(|byte| *byte == b'\n').next().unwrap(),
                "{} header did not canonicalize byte-exactly",
                path.display()
            );
            for line in lines {
                let entry =
                    ConversationLineCodec::decode_entry_for_session(line, header.session_id())
                        .unwrap_or_else(|error| {
                            panic!("{} entry failed to decode: {error}", path.display())
                        });
                assert_eq!(
                    ConversationLineCodec::encode_entry(&entry).unwrap(),
                    line,
                    "{} did not canonicalize byte-exactly",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn header_format_version_distinguishes_unsupported_canonical_integers() {
        let header =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl")
                .strip_suffix(b"\n")
                .unwrap();
        let header_with_version = |version: &str| {
            String::from_utf8(header.to_vec())
                .unwrap()
                .replacen(
                    "\"formatVersion\":1",
                    &format!("\"formatVersion\":{version}"),
                    1,
                )
                .into_bytes()
        };

        let unsupported = header_with_version("2");
        assert_eq!(
            ConversationLineCodec::decode_record(&unsupported),
            Err(ConversationCodecError::UnsupportedFormatVersion)
        );
        let debug = format!("{:?}", ConversationCodecError::UnsupportedFormatVersion);
        let display = ConversationCodecError::UnsupportedFormatVersion.to_string();
        assert!(!debug.contains('2'));
        assert!(!display.contains('2'));

        for (description, version, expected) in [
            ("wrong type", "\"2\"", ConversationCodecError::InvalidScalar),
            ("null", "null", ConversationCodecError::InvalidScalar),
            ("fraction", "1.0", ConversationCodecError::InvalidScalar),
            ("exponent", "1e0", ConversationCodecError::InvalidScalar),
            ("negative zero", "-0", ConversationCodecError::InvalidScalar),
            ("noncanonical", "01", ConversationCodecError::InvalidJson),
            (
                "overflow",
                "4294967296",
                ConversationCodecError::InvalidScalar,
            ),
        ] {
            let error =
                ConversationLineCodec::decode_record(&header_with_version(version)).unwrap_err();
            assert_eq!(error, expected, "{description} formatVersion");
            assert_ne!(
                error,
                ConversationCodecError::UnsupportedFormatVersion,
                "{description} formatVersion must not be classified as unsupported"
            );
        }

        let expected_session_id: SessionId =
            "ses_11111111111111111111111111111111".parse().unwrap();
        let decoded =
            ConversationLineCodec::decode_header_for_catalog(header, expected_session_id).unwrap();
        assert_eq!(
            ConversationLineCodec::encode_header(&decoded).unwrap(),
            header
        );
    }

    #[test]
    fn rejects_duplicate_required_unknown_and_identity_failures_without_echoing_input() {
        let header =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl")
                .strip_suffix(b"\n")
                .unwrap();
        let duplicated = br#"{"type":"session_header","data":{"formatVersion":1,"formatVersion":1,"sessionId":"ses_11111111111111111111111111111111","createdAt":"2026-07-31T12:00:00.000Z","initialAgent":{"agentId":"agt_22222222222222222222222222222222","revision":"ar_1"},"initialDefinitionRevision":"sdr_1"}}"#;
        assert_eq!(
            ConversationLineCodec::decode_record(duplicated),
            Err(ConversationCodecError::InvalidJson)
        );
        let duplicate_entry = br#"{"type":"entry","data":{"entryId":"ent_11111111111111111111111111111111","entryId":"ent_22222222222222222222222222222222","parentId":null,"sessionId":"ses_11111111111111111111111111111111","turnId":"trn_11111111111111111111111111111111","timestamp":"2026-07-31T12:00:00.000Z","body":{"type":"compaction","data":{"summary":"safe","firstKeptEntryId":null,"modelCall":null}}}}"#;
        assert_eq!(
            ConversationLineCodec::decode_record(duplicate_entry),
            Err(ConversationCodecError::InvalidJson)
        );
        assert_eq!(
            ConversationLineCodec::decode_record(br#"{"type":"future_record","data":{}}"#),
            Err(ConversationCodecError::UnknownRecordVariant)
        );
        assert_eq!(
            ConversationLineCodec::decode_record(
                br#"{"type":"entry","data":{"entryId":"ent_11111111111111111111111111111111","parentId":null,"sessionId":"ses_11111111111111111111111111111111","turnId":"trn_11111111111111111111111111111111","timestamp":"2026-07-31T12:00:00.000Z","body":{"type":"future_body","data":{}}}}"#
            ),
            Err(ConversationCodecError::UnknownBodyVariant)
        );
        assert_eq!(
            ConversationLineCodec::decode_record(
                br#"{"type":"entry","data":{"parentId":null,"sessionId":"ses_11111111111111111111111111111111","turnId":"trn_11111111111111111111111111111111","timestamp":"2026-07-31T12:00:00.000Z","body":{"type":"compaction","data":{"summary":"safe","firstKeptEntryId":null,"modelCall":null}}}}"#
            ),
            Err(ConversationCodecError::MissingRequiredField)
        );
        let opened: SessionId = "ses_99999999999999999999999999999999".parse().unwrap();
        assert_eq!(
            ConversationLineCodec::decode_header_for_catalog(header, opened),
            Err(ConversationCodecError::SessionIdentityMismatch)
        );
        let invalid_header_id = String::from_utf8(header.to_vec())
            .unwrap()
            .replace("ses_11111111111111111111111111111111", "ses_not-an-id");
        assert_eq!(
            ConversationLineCodec::decode_record(invalid_header_id.as_bytes()),
            Err(ConversationCodecError::InvalidScalar)
        );
        assert!(!format!("{:?}", ConversationCodecError::InvalidJson).contains("future_record"));
    }

    #[test]
    fn options_are_nullable_but_required_values_are_not() {
        let header =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl")
                .strip_suffix(b"\n")
                .unwrap();
        let mut optional = String::from_utf8(header.to_vec()).unwrap();
        optional = optional.replace(
            "\"initialDefinitionRevision\":\"sdr_1\"",
            "\"initialDefinitionRevision\":null",
        );
        assert_eq!(
            ConversationLineCodec::decode_record(optional.as_bytes()),
            Err(ConversationCodecError::InvalidScalar)
        );

        let entry = br#"{"type":"entry","data":{"entryId":"ent_11111111111111111111111111111111","parentId":null,"sessionId":"ses_11111111111111111111111111111111","turnId":"trn_11111111111111111111111111111111","timestamp":"2026-07-31T12:00:00.000Z","body":{"type":"compaction","data":{"summary":"safe","firstKeptEntryId":null,"modelCall":null}}}}"#;
        let record = ConversationLineCodec::decode_record(entry).unwrap();
        let encoded = ConversationLineCodec::encode_record(&record).unwrap();
        assert!(
            encoded
                .windows(b"\"parentId\":null".len())
                .any(|value| value == b"\"parentId\":null")
        );
        assert!(
            encoded
                .windows(b"\"modelCall\":null".len())
                .any(|value| value == b"\"modelCall\":null")
        );

        let assistant =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/tool-exchange.jsonl")
                .split(|byte| *byte == b'\n')
                .nth(2)
                .unwrap();
        let absent_option = String::from_utf8(assistant.to_vec())
            .unwrap()
            .replace(",\"responseId\":\"resp_tool_1\"", "")
            .into_bytes();
        let record = ConversationLineCodec::decode_record(&absent_option).unwrap();
        assert!(
            ConversationLineCodec::encode_record(&record)
                .unwrap()
                .windows(b"\"responseId\":null".len())
                .any(|value| value == b"\"responseId\":null")
        );

        let additive = String::from_utf8(entry.to_vec())
            .unwrap()
            .replace("}}}", "},\"futureField\":true}}")
            .into_bytes();
        let record = ConversationLineCodec::decode_record(&additive).unwrap();
        assert_eq!(
            ConversationLineCodec::encode_record(&record).unwrap(),
            entry
        );

        let mut header_unknown = header.to_vec();
        assert_eq!(header_unknown.pop(), Some(b'}'));
        header_unknown.extend_from_slice(b",\"futureField\":true}");
        assert_eq!(
            ConversationLineCodec::decode_record(&header_unknown),
            Err(ConversationCodecError::InvalidShape)
        );

        let other_session: SessionId = "ses_99999999999999999999999999999999".parse().unwrap();
        assert_eq!(
            ConversationLineCodec::decode_entry_for_session(entry, other_session),
            Err(ConversationCodecError::SessionIdentityMismatch)
        );
    }

    #[test]
    fn enforces_tool_truth_and_compaction_model_call_facts() {
        let invalid_tool = br#"{"type":"entry","data":{"entryId":"ent_11111111111111111111111111111111","parentId":null,"sessionId":"ses_11111111111111111111111111111111","turnId":"trn_11111111111111111111111111111111","timestamp":"2026-07-31T12:00:00.000Z","body":{"type":"tool_message","data":{"itemId":"itm_11111111111111111111111111111111","toolCallId":"call_1","outcome":{"type":"completed","data":{"source":"executed","disposition":"denied","content":{"parts":[{"type":"text","data":{"text":"redacted"}}]}}}}}}}"#;
        assert_eq!(
            ConversationLineCodec::decode_record(invalid_tool),
            Err(ConversationCodecError::InvalidSemantic)
        );

        let invalid_compaction = br#"{"type":"entry","data":{"entryId":"ent_11111111111111111111111111111111","parentId":null,"sessionId":"ses_11111111111111111111111111111111","turnId":"trn_11111111111111111111111111111111","timestamp":"2026-07-31T12:00:00.000Z","body":{"type":"compaction","data":{"summary":"safe","firstKeptEntryId":null,"modelCall":{"model":{"providerId":"fixture","modelId":"scripted","reasoning":"disabled","serviceClass":"standard"},"responseId":null,"usage":null,"finishReason":"tool_calls","requestedMaxOutputTokens":512,"logicalRetryCount":0,"metadata":{"providerRequestId":null,"rawFinishCode":null,"serviceTier":null}}}}}}"#;
        assert_eq!(
            ConversationLineCodec::decode_record(invalid_compaction),
            Err(ConversationCodecError::InvalidSemantic)
        );
    }

    #[test]
    fn keeps_required_identity_and_line_preflight_separate_from_physical_scanning() {
        let missing_parent = br#"{"type":"entry","data":{"entryId":"ent_11111111111111111111111111111111","sessionId":"ses_11111111111111111111111111111111","turnId":"trn_11111111111111111111111111111111","timestamp":"2026-07-31T12:00:00.000Z","body":{"type":"compaction","data":{"summary":"safe","firstKeptEntryId":null,"modelCall":null}}}}"#;
        assert_eq!(
            ConversationLineCodec::decode_record(missing_parent),
            Err(ConversationCodecError::MissingRequiredField)
        );

        let header =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl")
                .strip_suffix(b"\n")
                .unwrap();
        let mut too_large_header = header.to_vec();
        too_large_header.extend(std::iter::repeat_n(
            b' ',
            MAX_CONVERSATION_HEADER_BYTES + 1 - header.len(),
        ));
        assert_eq!(
            ConversationLineCodec::decode_record(&too_large_header),
            Err(ConversationCodecError::HeaderTooLarge)
        );
        let opened: SessionId = "ses_11111111111111111111111111111111".parse().unwrap();
        assert_eq!(
            ConversationLineCodec::decode_header_for_catalog(&too_large_header, opened),
            Err(ConversationCodecError::HeaderTooLarge)
        );
        assert_eq!(
            ConversationLineCodec::decode_record(&vec![b' '; MAX_CONVERSATION_ENTRY_BYTES + 1]),
            Err(ConversationCodecError::LineTooLarge)
        );
    }

    #[test]
    fn contribution_stamp_collection_is_required_and_stamp_salvage_preserves_the_message() {
        let line = include_bytes!(
            "../../docs/fixtures/wire-v1/conversation/corruption/contribution-stamp-salvage.jsonl"
        )
        .split(|byte| *byte == b'\n')
        .nth(1)
        .unwrap();
        let ConversationRecord::Entry(entry) = ConversationLineCodec::decode_record(line).unwrap()
        else {
            panic!("fixture line was not an entry");
        };
        let StoredEntryBody::UserMessage(message) = entry.body() else {
            panic!("fixture entry was not a user message");
        };
        assert_eq!(message.content().message().content().len(), 3);
        assert_eq!(message.content().contribution_stamps().len(), 1);
        assert_eq!(
            message.content().contribution_stamps()[0].content_part_index(),
            1
        );
        assert_eq!(
            ConversationLineCodec::encode_entry(&entry),
            Err(ConversationCodecError::InvalidSemantic)
        );

        let valid =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/tool-exchange.jsonl")
                .split(|byte| *byte == b'\n')
                .nth(1)
                .unwrap();
        let missing = String::from_utf8(valid.to_vec())
            .unwrap()
            .replace(",\"contributionStamps\":[]", "");
        assert_eq!(
            ConversationLineCodec::decode_record(missing.as_bytes()),
            Err(ConversationCodecError::MissingRequiredField)
        );
        let null = String::from_utf8(valid.to_vec())
            .unwrap()
            .replace("\"contributionStamps\":[]", "\"contributionStamps\":null");
        assert_eq!(
            ConversationLineCodec::decode_record(null.as_bytes()),
            Err(ConversationCodecError::InvalidShape)
        );
    }

    #[test]
    fn tool_call_arguments_use_the_raw_subtree_before_conversation_ast_materialization() {
        let whitespace_arguments = format!("{{{}}}", " ".repeat(65_535));
        assert!(whitespace_arguments.len() > 65_536);
        assert_eq!(
            ConversationLineCodec::decode_record(&tool_call_assistant_line(&whitespace_arguments)),
            Err(ConversationCodecError::InvalidJson)
        );

        let escaped_arguments = format!(r#"{{"value":"{}"}}"#, r#"\u0061"#.repeat(11_000));
        assert!(escaped_arguments.len() > 65_536);
        assert!(r#"{"value":"{}"}"#.len() + 11_000 < 65_536);
        assert_eq!(
            ConversationLineCodec::decode_record(&tool_call_assistant_line(&escaped_arguments)),
            Err(ConversationCodecError::InvalidJson)
        );

        for arguments in [
            r#"{"duplicate":1,"duplicate":2}"#.to_owned(),
            r#"{"number":1e1000001}"#.to_owned(),
            format!(r#"{{"number":{}}}"#, "1".repeat(65)),
            format!(r#"{{"value":"{}"}}"#, "x".repeat(16_385)),
            format!(r#"{{"value":{}{{}}{}}}"#, "[".repeat(32), "]".repeat(32)),
        ] {
            assert_eq!(
                ConversationLineCodec::decode_record(&tool_call_assistant_line(&arguments)),
                Err(ConversationCodecError::InvalidJson)
            );
        }

        let ConversationRecord::Entry(entry) =
            ConversationLineCodec::decode_record(&tool_call_assistant_line(r#"{"z":1.0,"a":-0}"#))
                .unwrap()
        else {
            panic!("fixture line was not an entry");
        };
        let StoredEntryBody::AssistantMessage(message) = entry.body() else {
            panic!("fixture entry was not an assistant message");
        };
        let [StoredAssistantContent::ToolCall { arguments, .. }] = message.content() else {
            panic!("fixture assistant content was not one tool call");
        };
        assert_eq!(arguments.canonical_json(), r#"{"a":0,"z":1}"#);

        let unrelated_arguments = format!("{{{}}}", " ".repeat(65_535));
        let assistant = tool_call_assistant_line("{}");
        let additive = String::from_utf8(assistant).unwrap().replacen(
            "\"content\":[",
            &format!("\"arguments\":{unrelated_arguments},\"content\":["),
            1,
        );
        assert!(ConversationLineCodec::decode_record(additive.as_bytes()).is_ok());
    }

    #[test]
    fn tool_call_content_allows_unknown_finish_reason_but_not_final_disposition() {
        let assistant = tool_call_assistant_line("{}");
        let unknown = String::from_utf8(assistant).unwrap().replace(
            "\"finishReason\":\"tool_calls\"",
            "\"finishReason\":\"unknown\"",
        );
        let ConversationRecord::Entry(entry) =
            ConversationLineCodec::decode_record(unknown.as_bytes()).unwrap()
        else {
            panic!("fixture line was not an entry");
        };
        let StoredEntryBody::AssistantMessage(message) = entry.body() else {
            panic!("fixture entry was not an assistant message");
        };
        assert_eq!(message.disposition(), AssistantDisposition::Intermediate);
        assert_eq!(message.finish_reason(), ModelFinishReason::Unknown);

        let final_unknown = unknown.replace(
            "\"disposition\":\"intermediate\"",
            "\"disposition\":\"final\"",
        );
        assert_eq!(
            ConversationLineCodec::decode_record(final_unknown.as_bytes()),
            Err(ConversationCodecError::InvalidSemantic)
        );
    }

    #[test]
    fn decoded_header_session_identity_drives_the_entry_production_seam() {
        let header =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl")
                .strip_suffix(b"\n")
                .unwrap();
        let expected: SessionId = "ses_11111111111111111111111111111111".parse().unwrap();
        let header = ConversationLineCodec::decode_header_for_catalog(header, expected).unwrap();
        let entry =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/tool-exchange.jsonl")
                .split(|byte| *byte == b'\n')
                .nth(1)
                .unwrap();
        let other_session = String::from_utf8(entry.to_vec()).unwrap().replace(
            "ses_11111111111111111111111111111111",
            "ses_99999999999999999999999999999999",
        );
        assert_eq!(
            ConversationLineCodec::decode_entry_for_session(
                other_session.as_bytes(),
                header.session_id()
            ),
            Err(ConversationCodecError::SessionIdentityMismatch)
        );
    }

    #[test]
    fn decoded_records_and_errors_do_not_debug_echo_conversation_text() {
        let line =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/tool-exchange.jsonl")
                .split(|byte| *byte == b'\n')
                .nth(3)
                .unwrap();
        let record = ConversationLineCodec::decode_record(line).unwrap();
        let debug = format!("{record:?} {:?}", ConversationCodecError::InvalidSemantic);
        for secret_or_text in [
            "It is 18 C",
            "Checked weather",
            "provider_req_1",
            "resp_final_1",
            "call_weather_1",
        ] {
            assert!(
                !debug.contains(secret_or_text),
                "debug leaked {secret_or_text:?}"
            );
        }
    }
}
