use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use thiserror::Error;

use super::json_number::{CanonicalJsonNumber, JsonNumberError};
use super::limits::{CheckedLimitCounter, ProtocolLimits, WireLimit};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BoundedJsonError {
    #[error("embedded JSON raw input exceeds its byte limit")]
    RawInputTooLarge,
    #[error("embedded JSON canonical output exceeds its byte limit")]
    CanonicalOutputTooLarge,
    #[error("embedded JSON is not valid UTF-8")]
    InvalidUtf8,
    #[error("embedded JSON syntax is invalid")]
    InvalidSyntax,
    #[error("embedded JSON exceeds its depth limit")]
    DepthLimit,
    #[error("embedded JSON array exceeds its item limit")]
    ArrayItemsLimit,
    #[error("embedded JSON object exceeds its member limit")]
    ObjectMembersLimit,
    #[error("embedded JSON string exceeds its decoded byte limit")]
    StringBytesLimit,
    #[error("embedded JSON number exceeds its literal limit")]
    NumberLiteralLimit,
    #[error("embedded JSON number exponent exceeds its limit")]
    NumberExponentLimit,
    #[error("embedded JSON contains a duplicate decoded object key")]
    DuplicateKey,
    #[error("embedded JSON exceeds its node limit")]
    NodeLimit,
    #[error("embedded JSON root must be an object")]
    RootObjectRequired,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) enum JsonNode {
    Null,
    Bool(bool),
    Number(ParsedJsonNumber),
    String(Box<str>),
    Array(Vec<JsonNode>),
    Object(BTreeMap<Box<str>, JsonNode>),
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct ParsedJsonNumber {
    raw: Box<str>,
    canonical: CanonicalJsonNumber,
}

impl ParsedJsonNumber {
    pub(super) fn raw(&self) -> &str {
        &self.raw
    }

    fn canonical(&self) -> &CanonicalJsonNumber {
        &self.canonical
    }
}

impl JsonNode {
    pub(super) fn as_object(&self) -> Option<&BTreeMap<Box<str>, JsonNode>> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn as_array(&self) -> Option<&[JsonNode]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn as_number(&self) -> Option<&ParsedJsonNumber> {
        match self {
            Self::Number(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct JsonParseLimits {
    pub max_encoded_bytes: WireLimit,
    pub max_depth: WireLimit,
    pub max_array_items: WireLimit,
    pub max_object_members: WireLimit,
    pub max_string_bytes: WireLimit,
    pub max_nodes: Option<WireLimit>,
}

impl JsonParseLimits {
    fn value() -> Self {
        let limits = ProtocolLimits::v1_0().embedded_json.value;
        Self {
            max_encoded_bytes: WireLimit::new(limits.max_encoded_bytes as usize),
            max_depth: WireLimit::new(limits.max_depth as usize),
            max_array_items: WireLimit::new(limits.max_array_items as usize),
            max_object_members: WireLimit::new(limits.max_object_members as usize),
            max_string_bytes: WireLimit::new(limits.max_string_bytes as usize),
            max_nodes: None,
        }
    }

    pub(super) fn public(max_encoded_bytes: usize, limits: ProtocolLimits) -> Self {
        Self {
            max_encoded_bytes: WireLimit::new(max_encoded_bytes),
            max_depth: WireLimit::new(limits.transport.max_json_depth as usize),
            max_array_items: WireLimit::new(limits.transport.max_array_items as usize),
            max_object_members: WireLimit::new(limits.transport.max_object_members as usize),
            max_string_bytes: WireLimit::new(limits.transport.max_string_bytes as usize),
            max_nodes: None,
        }
    }

    pub(super) fn schema() -> Self {
        let limits = ProtocolLimits::v1_0().embedded_json.schema;
        Self {
            max_encoded_bytes: WireLimit::new(limits.max_encoded_bytes as usize),
            max_depth: WireLimit::new(limits.max_depth as usize),
            max_array_items: WireLimit::new(limits.max_nodes as usize),
            max_object_members: WireLimit::new(limits.max_nodes as usize),
            max_string_bytes: WireLimit::new(limits.max_encoded_bytes as usize),
            max_nodes: Some(WireLimit::new(limits.max_nodes as usize)),
        }
    }
}

#[derive(Clone)]
pub struct BoundedJsonValue {
    canonical: Box<str>,
}

impl BoundedJsonValue {
    pub fn from_slice(input: &[u8]) -> Result<Self, BoundedJsonError> {
        Self::parse_with_limits(input, JsonParseLimits::value())
    }

    pub fn canonical_json(&self) -> &str {
        &self.canonical
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        self.canonical.as_bytes()
    }

    pub(super) fn parse_with_limits(
        input: &[u8],
        limits: JsonParseLimits,
    ) -> Result<Self, BoundedJsonError> {
        let node = parse_node(input, limits)?;
        Self::from_node(node, limits.max_encoded_bytes)
    }

    pub(super) fn from_node(
        node: JsonNode,
        output_limit: WireLimit,
    ) -> Result<Self, BoundedJsonError> {
        let mut encoder = CanonicalEncoder::new(output_limit);
        encoder.encode_node(&node)?;
        Ok(Self {
            canonical: encoder.finish().into(),
        })
    }
}

impl FromStr for BoundedJsonValue {
    type Err = BoundedJsonError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_slice(value.as_bytes())
    }
}

impl fmt::Debug for BoundedJsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedJsonValue")
            .field("canonical_bytes", &self.canonical.len())
            .finish_non_exhaustive()
    }
}

impl Hash for BoundedJsonValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.canonical.hash(state);
    }
}

impl PartialEq for BoundedJsonValue {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}

impl Eq for BoundedJsonValue {}

#[derive(Clone)]
pub struct BoundedJsonObject(BoundedJsonValue);

impl BoundedJsonObject {
    pub fn from_slice(input: &[u8]) -> Result<Self, BoundedJsonError> {
        let limits = JsonParseLimits::value();
        let node = parse_node(input, limits)?;
        if node.as_object().is_none() {
            return Err(BoundedJsonError::RootObjectRequired);
        }
        BoundedJsonValue::from_node(node, limits.max_encoded_bytes).map(Self)
    }

    pub fn canonical_json(&self) -> &str {
        self.0.canonical_json()
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        self.0.canonical_bytes()
    }

    pub fn as_value(&self) -> &BoundedJsonValue {
        &self.0
    }
}

impl FromStr for BoundedJsonObject {
    type Err = BoundedJsonError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_slice(value.as_bytes())
    }
}

impl fmt::Debug for BoundedJsonObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedJsonObject")
            .field("canonical_bytes", &self.0.canonical.len())
            .finish_non_exhaustive()
    }
}

impl Hash for BoundedJsonObject {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl PartialEq for BoundedJsonObject {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for BoundedJsonObject {}

pub(super) fn parse_node(
    input: &[u8],
    limits: JsonParseLimits,
) -> Result<JsonNode, BoundedJsonError> {
    limits
        .max_encoded_bytes
        .validate_bytes(input)
        .map_err(|_| BoundedJsonError::RawInputTooLarge)?;
    let input = std::str::from_utf8(input).map_err(|_| BoundedJsonError::InvalidUtf8)?;
    Parser::new(input, limits).parse()
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    position: usize,
    limits: JsonParseLimits,
    node_counter: Option<CheckedLimitCounter>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, limits: JsonParseLimits) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
            limits,
            node_counter: limits.max_nodes.map(CheckedLimitCounter::new),
        }
    }

    fn parse(mut self) -> Result<JsonNode, BoundedJsonError> {
        self.skip_whitespace();
        let value = self.parse_value(1)?;
        self.skip_whitespace();
        if self.position != self.bytes.len() {
            return Err(BoundedJsonError::InvalidSyntax);
        }
        Ok(value)
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonNode, BoundedJsonError> {
        self.limits
            .max_depth
            .validate(depth)
            .map_err(|_| BoundedJsonError::DepthLimit)?;
        if let Some(counter) = &mut self.node_counter {
            counter
                .try_add(1)
                .map_err(|_| BoundedJsonError::NodeLimit)?;
        }
        self.skip_whitespace();
        match self.peek().ok_or(BoundedJsonError::InvalidSyntax)? {
            b'n' => {
                self.consume_literal(b"null")?;
                Ok(JsonNode::Null)
            }
            b't' => {
                self.consume_literal(b"true")?;
                Ok(JsonNode::Bool(true))
            }
            b'f' => {
                self.consume_literal(b"false")?;
                Ok(JsonNode::Bool(false))
            }
            b'"' => self.parse_string().map(JsonNode::String),
            b'[' => self.parse_array(depth),
            b'{' => self.parse_object(depth),
            b'-' | b'0'..=b'9' => self.parse_number().map(JsonNode::Number),
            _ => Err(BoundedJsonError::InvalidSyntax),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonNode, BoundedJsonError> {
        self.expect(b'[')?;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.consume_if(b']') {
            return Ok(JsonNode::Array(values));
        }
        loop {
            self.limits
                .max_array_items
                .validate(values.len() + 1)
                .map_err(|_| BoundedJsonError::ArrayItemsLimit)?;
            values.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            if self.consume_if(b']') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonNode::Array(values))
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonNode, BoundedJsonError> {
        self.expect(b'{')?;
        self.skip_whitespace();
        let mut values = BTreeMap::new();
        if self.consume_if(b'}') {
            return Ok(JsonNode::Object(values));
        }
        loop {
            self.skip_whitespace();
            self.limits
                .max_object_members
                .validate(values.len() + 1)
                .map_err(|_| BoundedJsonError::ObjectMembersLimit)?;
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            if values.contains_key(&key) {
                return Err(BoundedJsonError::DuplicateKey);
            }
            let value = self.parse_value(depth + 1)?;
            values.insert(key, value);
            self.skip_whitespace();
            if self.consume_if(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonNode::Object(values))
    }

    fn parse_string(&mut self) -> Result<Box<str>, BoundedJsonError> {
        self.expect(b'"')?;
        let mut output = String::new();
        loop {
            let byte = self.next().ok_or(BoundedJsonError::InvalidSyntax)?;
            let character = match byte {
                b'"' => break,
                b'\\' => self.parse_escape()?,
                0x00..=0x1f => return Err(BoundedJsonError::InvalidSyntax),
                0x20..=0x7f => char::from(byte),
                _ => {
                    self.position -= 1;
                    let character = self.input[self.position..]
                        .chars()
                        .next()
                        .ok_or(BoundedJsonError::InvalidSyntax)?;
                    self.position += character.len_utf8();
                    character
                }
            };
            let next = output
                .len()
                .checked_add(character.len_utf8())
                .ok_or(BoundedJsonError::StringBytesLimit)?;
            self.limits
                .max_string_bytes
                .validate(next)
                .map_err(|_| BoundedJsonError::StringBytesLimit)?;
            output.push(character);
        }
        Ok(output.into())
    }

    fn parse_escape(&mut self) -> Result<char, BoundedJsonError> {
        let character = match self.next().ok_or(BoundedJsonError::InvalidSyntax)? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{0008}',
            b'f' => '\u{000c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let first = self.parse_hex_u16()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    self.expect_exact(b'\\')?;
                    self.expect_exact(b'u')?;
                    let second = self.parse_hex_u16()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(BoundedJsonError::InvalidSyntax);
                    }
                    0x1_0000 + (((u32::from(first) - 0xd800) << 10) | (u32::from(second) - 0xdc00))
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return Err(BoundedJsonError::InvalidSyntax);
                } else {
                    u32::from(first)
                };
                char::from_u32(scalar).ok_or(BoundedJsonError::InvalidSyntax)?
            }
            _ => return Err(BoundedJsonError::InvalidSyntax),
        };
        Ok(character)
    }

    fn parse_hex_u16(&mut self) -> Result<u16, BoundedJsonError> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let digit = match self.next().ok_or(BoundedJsonError::InvalidSyntax)? {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(BoundedJsonError::InvalidSyntax),
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<ParsedJsonNumber, BoundedJsonError> {
        let start = self.position;
        while matches!(self.peek(), Some(byte) if !is_value_delimiter(byte)) {
            self.position += 1;
        }
        let literal = &self.input[start..self.position];
        let canonical = CanonicalJsonNumber::parse(literal).map_err(|error| match error {
            JsonNumberError::InvalidSyntax => BoundedJsonError::InvalidSyntax,
            JsonNumberError::RawLiteralTooLong | JsonNumberError::CanonicalLiteralTooLong => {
                BoundedJsonError::NumberLiteralLimit
            }
            JsonNumberError::ExponentOutOfRange => BoundedJsonError::NumberExponentLimit,
        })?;
        Ok(ParsedJsonNumber {
            raw: literal.into(),
            canonical,
        })
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), BoundedJsonError> {
        if self.bytes.get(self.position..self.position + literal.len()) != Some(literal) {
            return Err(BoundedJsonError::InvalidSyntax);
        }
        self.position += literal.len();
        if matches!(self.peek(), Some(byte) if !is_value_delimiter(byte)) {
            return Err(BoundedJsonError::InvalidSyntax);
        }
        Ok(())
    }

    fn expect(&mut self, expected: u8) -> Result<(), BoundedJsonError> {
        self.skip_whitespace();
        self.expect_exact(expected)
    }

    fn expect_exact(&mut self, expected: u8) -> Result<(), BoundedJsonError> {
        if self.consume_if(expected) {
            Ok(())
        } else {
            Err(BoundedJsonError::InvalidSyntax)
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

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let value = self.peek()?;
        self.position += 1;
        Some(value)
    }
}

fn is_value_delimiter(value: u8) -> bool {
    matches!(value, b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}')
}

struct CanonicalEncoder {
    output: String,
    limit: WireLimit,
}

impl CanonicalEncoder {
    fn new(limit: WireLimit) -> Self {
        Self {
            output: String::new(),
            limit,
        }
    }

    fn finish(self) -> String {
        self.output
    }

    fn encode_node(&mut self, node: &JsonNode) -> Result<(), BoundedJsonError> {
        match node {
            JsonNode::Null => self.push_str("null"),
            JsonNode::Bool(value) => self.push_str(if *value { "true" } else { "false" }),
            JsonNode::Number(value) => self.push_str(value.canonical().as_str()),
            JsonNode::String(value) => self.encode_string(value),
            JsonNode::Array(values) => {
                self.push_char('[')?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        self.push_char(',')?;
                    }
                    self.encode_node(value)?;
                }
                self.push_char(']')
            }
            JsonNode::Object(values) => {
                self.push_char('{')?;
                for (index, (key, value)) in values.iter().enumerate() {
                    if index != 0 {
                        self.push_char(',')?;
                    }
                    self.encode_string(key)?;
                    self.push_char(':')?;
                    self.encode_node(value)?;
                }
                self.push_char('}')
            }
        }
    }

    fn encode_string(&mut self, value: &str) -> Result<(), BoundedJsonError> {
        self.push_char('"')?;
        for character in value.chars() {
            match character {
                '"' => self.push_str("\\\"")?,
                '\\' => self.push_str("\\\\")?,
                '\u{0008}' => self.push_str("\\b")?,
                '\t' => self.push_str("\\t")?,
                '\n' => self.push_str("\\n")?,
                '\u{000c}' => self.push_str("\\f")?,
                '\r' => self.push_str("\\r")?,
                '\u{0000}'..='\u{001f}' => {
                    let value = u32::from(character) as u8;
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    let escape = [
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[usize::from(value >> 4)],
                        HEX[usize::from(value & 0x0f)],
                    ];
                    self.push_str(
                        std::str::from_utf8(&escape).expect("control escape is validated ASCII"),
                    )?;
                }
                _ => self.push_char(character)?,
            }
        }
        self.push_char('"')
    }

    fn push_char(&mut self, value: char) -> Result<(), BoundedJsonError> {
        let mut buffer = [0; 4];
        self.push_str(value.encode_utf8(&mut buffer))
    }

    fn push_str(&mut self, value: &str) -> Result<(), BoundedJsonError> {
        let next = self
            .output
            .len()
            .checked_add(value.len())
            .ok_or(BoundedJsonError::CanonicalOutputTooLarge)?;
        self.limit
            .validate(next)
            .map_err(|_| BoundedJsonError::CanonicalOutputTooLarge)?;
        self.output.push_str(value);
        Ok(())
    }
}
