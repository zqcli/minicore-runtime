use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;

use crate::wire::lexical::{
    LexicalError, canonical_json_string_len, normalize_newlines, validate_opaque_ascii,
    validate_safe_text, validate_stable_symbolic_key,
};
use crate::wire::{BoundedJsonObject, BoundedJsonSchema, BoundedJsonValue, ItemId, ProtocolLimits};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolNameError {
    #[error("tool name must be 1..=64 bytes")]
    InvalidLength,
    #[error("tool name violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolName(Box<str>);

impl ToolName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolName {
    type Err = ToolNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 64, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => ToolNameError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                ToolNameError::InvalidGrammar
            }
        })?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ToolNameError::InvalidGrammar);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolCallIdError {
    #[error("tool call ID must be 1..=256 bytes")]
    InvalidLength,
    #[error("tool call ID violates the opaque ASCII grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolCallId(Box<str>);

impl ToolCallId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolCallId {
    type Err = ToolCallIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_opaque_ascii(value, 256).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => ToolCallIdError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                ToolCallIdError::InvalidGrammar
            }
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolValueError {
    #[error("tool input schema uses an unsupported or malformed keyword")]
    InvalidSchema,
    #[error("tool text is empty, unsafe, or exceeds its limit")]
    InvalidText,
    #[error("tool result content part count is outside 1..=32")]
    InvalidResultPartCount,
    #[error("tool result content exceeds its aggregate byte limit")]
    ResultContentTooLarge,
    #[error("tool outcome source and disposition are incompatible")]
    InvalidOutcome,
    #[error("tool approval request is invalid")]
    InvalidApproval,
    #[error("user question request or answer is invalid")]
    InvalidQuestion,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolInputSchema {
    schema: BoundedJsonSchema,
}

impl ToolInputSchema {
    #[allow(
        dead_code,
        reason = "semantic subset validation is consumed by ToolService in M8"
    )]
    fn new(schema: BoundedJsonSchema) -> Result<Self, ToolValueError> {
        let value: Value = serde_json::from_slice(schema.canonical_bytes())
            .map_err(|_| ToolValueError::InvalidSchema)?;
        if !validate_tool_schema(&value) {
            return Err(ToolValueError::InvalidSchema);
        }
        Ok(Self { schema })
    }

    pub const fn schema(&self) -> &BoundedJsonSchema {
        &self.schema
    }
}

fn validate_tool_schema(value: &Value) -> bool {
    let Some((nodes, root)) = build_tool_schema_graph(value) else {
        return false;
    };
    let Some(order) = schema_topological_order(&nodes) else {
        return false;
    };
    schema_declares_object(&nodes, &order, root)
}

#[derive(Default)]
struct ToolSchemaNode {
    edges: Vec<usize>,
    reference: Option<Box<str>>,
    reference_target: Option<usize>,
    direct_object: bool,
    all_of: Vec<usize>,
    any_of: Vec<usize>,
    one_of: Vec<usize>,
}

fn build_tool_schema_graph(
    value: &Value,
) -> Option<(std::collections::BTreeMap<usize, ToolSchemaNode>, usize)> {
    let root = schema_node_key(value);
    let maximum = ProtocolLimits::v1_0().embedded_json.schema.max_nodes as usize;
    let mut nodes = std::collections::BTreeMap::new();
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        let key = schema_node_key(value);
        if nodes.contains_key(&key) {
            continue;
        }
        if nodes.len() >= maximum {
            return None;
        }
        let mut node = ToolSchemaNode::default();
        if value.is_boolean() {
            nodes.insert(key, node);
            continue;
        }
        let object = value.as_object()?;
        if !valid_integer_bound_pair(object, "minLength", "maxLength")
            || !valid_integer_bound_pair(object, "minItems", "maxItems")
        {
            return None;
        }
        for (keyword, value) in object {
            match keyword.as_str() {
                "$schema" => {
                    if value.as_str() != Some("https://json-schema.org/draft/2020-12/schema") {
                        return None;
                    }
                }
                "$ref" => node.reference = Some(value.as_str()?.into()),
                "$defs" | "properties" => {
                    for schema in value.as_object()?.values() {
                        add_schema_child(&mut node.edges, &mut pending, schema);
                    }
                }
                "required" => {
                    let values = value.as_array()?;
                    if !values.iter().all(Value::is_string)
                        || values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            != values.len()
                    {
                        return None;
                    }
                }
                "additionalProperties" | "items" => {
                    add_schema_child(&mut node.edges, &mut pending, value);
                }
                "enum" => {
                    let values = value.as_array()?;
                    if values.is_empty()
                        || !values.iter().enumerate().all(|(index, value)| {
                            !values[index + 1..].iter().any(|other| other == value)
                        })
                    {
                        return None;
                    }
                }
                "const" => {}
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                    if !value.is_number() {
                        return None;
                    }
                }
                "multipleOf" => {
                    if !value.as_number().is_some_and(positive_json_number) {
                        return None;
                    }
                }
                "minLength" | "maxLength" | "minItems" | "maxItems" => {
                    exact_nonnegative_integer(value.as_number()?)?;
                }
                "description" => {
                    if !value.as_str().is_some_and(|text| {
                        validate_safe_text(
                            text,
                            ProtocolLimits::v1_0().text.max_description_bytes as usize,
                            true,
                        )
                        .is_ok()
                    }) {
                        return None;
                    }
                }
                "title" => {
                    if !value.as_str().is_some_and(|text| {
                        validate_safe_text(
                            text,
                            ProtocolLimits::v1_0().text.max_display_name_bytes as usize,
                            true,
                        )
                        .is_ok()
                    }) {
                        return None;
                    }
                }
                "allOf" | "anyOf" | "oneOf" => {
                    let schemas = value.as_array()?;
                    if schemas.is_empty() {
                        return None;
                    }
                    let group = match keyword.as_str() {
                        "allOf" => &mut node.all_of,
                        "anyOf" => &mut node.any_of,
                        "oneOf" => &mut node.one_of,
                        _ => unreachable!(),
                    };
                    for schema in schemas {
                        let child = schema_node_key(schema);
                        group.push(child);
                        add_schema_child(&mut node.edges, &mut pending, schema);
                    }
                }
                "type" => {
                    let value = value.as_str()?;
                    if !matches!(
                        value,
                        "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
                    ) {
                        return None;
                    }
                    node.direct_object = value == "object";
                }
                _ => return None,
            }
        }
        nodes.insert(key, node);
    }

    let references = nodes
        .iter()
        .filter_map(|(key, node)| {
            node.reference
                .as_deref()
                .map(|reference| (*key, reference.to_owned()))
        })
        .collect::<Vec<_>>();
    for (key, reference) in references {
        let target = resolve_reference(value, &reference)?;
        let target = schema_node_key(target);
        if !nodes.contains_key(&target) {
            return None;
        }
        let node = nodes.get_mut(&key)?;
        node.reference_target = Some(target);
        node.edges.push(target);
    }
    Some((nodes, root))
}

fn add_schema_child<'a>(edges: &mut Vec<usize>, pending: &mut Vec<&'a Value>, value: &'a Value) {
    edges.push(schema_node_key(value));
    pending.push(value);
}

fn schema_node_key(value: &Value) -> usize {
    std::ptr::from_ref(value) as usize
}

fn schema_topological_order(
    nodes: &std::collections::BTreeMap<usize, ToolSchemaNode>,
) -> Option<Vec<usize>> {
    let mut colors = std::collections::BTreeMap::<usize, u8>::new();
    let mut order = Vec::with_capacity(nodes.len());
    for start in nodes.keys().copied() {
        if colors.get(&start) == Some(&2) {
            continue;
        }
        colors.insert(start, 1);
        let mut stack = vec![(start, 0_usize)];
        while let Some((node, edge_index)) = stack.last().copied() {
            let edges = &nodes.get(&node)?.edges;
            if edge_index < edges.len() {
                stack.last_mut()?.1 += 1;
                let target = edges[edge_index];
                match colors.get(&target).copied().unwrap_or(0) {
                    0 => {
                        colors.insert(target, 1);
                        stack.push((target, 0));
                    }
                    1 => return None,
                    2 => {}
                    _ => return None,
                }
            } else {
                stack.pop();
                colors.insert(node, 2);
                order.push(node);
            }
        }
    }
    Some(order)
}

fn schema_declares_object(
    nodes: &std::collections::BTreeMap<usize, ToolSchemaNode>,
    order: &[usize],
    root: usize,
) -> bool {
    let mut object_nodes = std::collections::BTreeSet::new();
    for key in order {
        let Some(node) = nodes.get(key) else {
            return false;
        };
        let declares = node.direct_object
            || node
                .reference_target
                .is_some_and(|target| object_nodes.contains(&target))
            || node.all_of.iter().any(|child| object_nodes.contains(child))
            || (!node.any_of.is_empty()
                && node.any_of.iter().all(|child| object_nodes.contains(child)))
            || (!node.one_of.is_empty()
                && node.one_of.iter().all(|child| object_nodes.contains(child)));
        if declares {
            object_nodes.insert(*key);
        }
    }
    object_nodes.contains(&root)
}

fn resolve_reference<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = decode_local_json_pointer(reference)?;
    if pointer.is_empty() {
        Some(root)
    } else {
        root.pointer(&pointer)
    }
}

fn decode_local_json_pointer(reference: &str) -> Option<String> {
    let fragment = reference.strip_prefix('#')?;
    let bytes = fragment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = decode_hex(*bytes.get(index + 1)?)?;
            let low = decode_hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            if !is_uri_fragment_byte(bytes[index]) {
                return None;
            }
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let pointer = String::from_utf8(decoded).ok()?;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return None;
    }
    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            if !matches!(bytes.get(index + 1), Some(b'0' | b'1')) {
                return None;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    Some(pointer)
}

fn decode_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn is_uri_fragment_byte(value: u8) -> bool {
    value.is_ascii_alphanumeric()
        || matches!(
            value,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'/'
                | b'?'
        )
}

fn valid_integer_bound_pair(
    object: &serde_json::Map<String, Value>,
    minimum: &str,
    maximum: &str,
) -> bool {
    match (object.get(minimum), object.get(maximum)) {
        (Some(minimum), Some(maximum)) => minimum
            .as_number()
            .and_then(exact_nonnegative_integer)
            .zip(maximum.as_number().and_then(exact_nonnegative_integer))
            .is_some_and(|(minimum, maximum)| compare_exact_integers(&minimum, &maximum).is_le()),
        _ => true,
    }
}

struct ExactNonnegativeInteger {
    digits: Box<str>,
    trailing_zeros: usize,
}

fn exact_nonnegative_integer(number: &serde_json::Number) -> Option<ExactNonnegativeInteger> {
    let value = number.to_string();
    if value.starts_with('-') {
        return None;
    }
    let (coefficient, exponent) = if let Some(index) = value.find(['e', 'E']) {
        (&value[..index], value[index + 1..].parse::<i64>().ok()?)
    } else {
        (value.as_str(), 0)
    };
    let mut digits = String::with_capacity(coefficient.len());
    let mut fractional_digits = 0_i64;
    let mut after_decimal = false;
    for byte in coefficient.bytes() {
        match byte {
            b'.' if !after_decimal => after_decimal = true,
            b'0'..=b'9' => {
                digits.push(char::from(byte));
                if after_decimal {
                    fractional_digits += 1;
                }
            }
            _ => return None,
        }
    }
    let shift = exponent.checked_sub(fractional_digits)?;
    let trailing_zeros = if shift >= 0 {
        usize::try_from(shift).ok()?
    } else {
        let remove = usize::try_from(shift.checked_neg()?).ok()?;
        if remove > digits.len() {
            if digits.bytes().any(|byte| byte != b'0') {
                return None;
            }
            digits.clear();
        } else {
            if digits.as_bytes()[digits.len() - remove..]
                .iter()
                .any(|byte| *byte != b'0')
            {
                return None;
            }
            digits.truncate(digits.len() - remove);
        }
        0
    };
    let first_nonzero = digits.bytes().position(|byte| byte != b'0');
    let Some(first_nonzero) = first_nonzero else {
        return Some(ExactNonnegativeInteger {
            digits: "0".into(),
            trailing_zeros: 0,
        });
    };
    Some(ExactNonnegativeInteger {
        digits: digits[first_nonzero..].into(),
        trailing_zeros,
    })
}

fn compare_exact_integers(
    left: &ExactNonnegativeInteger,
    right: &ExactNonnegativeInteger,
) -> std::cmp::Ordering {
    let left_len = left.digits.len() + left.trailing_zeros;
    let right_len = right.digits.len() + right.trailing_zeros;
    match left_len.cmp(&right_len) {
        std::cmp::Ordering::Equal => {
            let shared = left.digits.len().min(right.digits.len());
            match left.digits.as_bytes()[..shared].cmp(&right.digits.as_bytes()[..shared]) {
                std::cmp::Ordering::Equal => {}
                ordering => return ordering,
            }
            if left.digits.as_bytes()[shared..]
                .iter()
                .any(|byte| *byte != b'0')
            {
                return std::cmp::Ordering::Greater;
            }
            if right.digits.as_bytes()[shared..]
                .iter()
                .any(|byte| *byte != b'0')
            {
                return std::cmp::Ordering::Less;
            }
            std::cmp::Ordering::Equal
        }
        ordering => ordering,
    }
}

fn positive_json_number(number: &serde_json::Number) -> bool {
    let value = number.to_string();
    !value.starts_with('-') && value.bytes().any(|byte| matches!(byte, b'1'..=b'9'))
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolSpec {
    name: ToolName,
    description: Arc<str>,
    input_schema: ToolInputSchema,
}

impl ToolSpec {
    #[allow(
        dead_code,
        reason = "constructed by validated ToolService resources in M8"
    )]
    fn new(
        name: ToolName,
        description: impl AsRef<str>,
        input_schema: ToolInputSchema,
    ) -> Result<Self, ToolValueError> {
        let description = normalize_and_validate_text(
            description.as_ref(),
            ProtocolLimits::v1_0().text.max_description_bytes as usize,
            false,
        )?;
        Ok(Self {
            name,
            description: description.into(),
            input_schema,
        })
    }

    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub const fn input_schema(&self) -> &ToolInputSchema {
        &self.input_schema
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolCall {
    tool_call_id: ToolCallId,
    name: ToolName,
    arguments: BoundedJsonObject,
    call_index: u32,
}

impl ToolCall {
    #[allow(
        dead_code,
        reason = "constructed from validated ModelGateway responses in M7"
    )]
    fn new(
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
        call_index: u32,
    ) -> Self {
        Self {
            tool_call_id,
            name,
            arguments,
            call_index,
        }
    }

    pub const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    pub const fn arguments(&self) -> &BoundedJsonObject {
        &self.arguments
    }

    pub const fn call_index(&self) -> u32 {
        self.call_index
    }
}

impl fmt::Debug for ToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCall")
            .field("tool_call_id", &self.tool_call_id)
            .field("name", &self.name)
            .field("argument_bytes", &self.arguments.canonical_bytes().len())
            .field("call_index", &self.call_index)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolExecutionRequest {
    item_id: ItemId,
    call: Arc<ToolCall>,
}

impl ToolExecutionRequest {
    #[allow(dead_code, reason = "constructed by Session Execution in M8")]
    fn new(item_id: ItemId, call: Arc<ToolCall>) -> Self {
        Self { item_id, call }
    }

    pub const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub const fn call(&self) -> &Arc<ToolCall> {
        &self.call
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolResultText(Arc<str>);

impl ToolResultText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ToolResultContentPart {
    Text(ToolResultText),
}

impl ToolResultContentPart {
    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text.as_str(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolResultContent {
    parts: Arc<[ToolResultContentPart]>,
}

impl ToolResultContent {
    pub fn from_text_parts(parts: Vec<String>) -> Result<Self, ToolValueError> {
        if parts.is_empty() || parts.len() > 32 {
            return Err(ToolValueError::InvalidResultPartCount);
        }
        let mut aggregate = 0_usize;
        let mut validated = Vec::with_capacity(parts.len());
        for part in parts {
            let text = validate_external_text(&part, 65_536, true)?;
            aggregate = aggregate
                .checked_add(text.len())
                .ok_or(ToolValueError::ResultContentTooLarge)?;
            if aggregate > 262_144 {
                return Err(ToolValueError::ResultContentTooLarge);
            }
            validated.push(ToolResultContentPart::Text(ToolResultText(text.into())));
        }
        Ok(Self {
            parts: validated.into(),
        })
    }

    pub fn parts(&self) -> &[ToolResultContentPart] {
        &self.parts
    }
}

impl fmt::Debug for ToolResultContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolResultContent")
            .field("parts", &self.parts.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolResultDisposition {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolResult {
    tool_call_id: ToolCallId,
    disposition: ToolResultDisposition,
    content: ToolResultContent,
    details: Option<BoundedJsonValue>,
}

impl ToolResult {
    #[allow(dead_code, reason = "constructed by ToolSet terminal projection in M8")]
    fn new(
        tool_call_id: ToolCallId,
        disposition: ToolResultDisposition,
        content: ToolResultContent,
        details: Option<BoundedJsonValue>,
    ) -> Self {
        Self {
            tool_call_id,
            disposition,
            content,
            details,
        }
    }

    pub const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub const fn disposition(&self) -> ToolResultDisposition {
        self.disposition
    }

    pub const fn content(&self) -> &ToolResultContent {
        &self.content
    }

    pub const fn details(&self) -> Option<&BoundedJsonValue> {
        self.details.as_ref()
    }
}

impl fmt::Debug for ToolResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolResult")
            .field("tool_call_id", &self.tool_call_id)
            .field("disposition", &self.disposition)
            .field("content", &self.content)
            .field("has_details", &self.details.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolOutcomeSource {
    PreExecution,
    Executed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolAbandonReason {
    OutcomeUnknown,
    RuntimeFailure,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolExecutionOutcome {
    kind: ToolExecutionOutcomeKind,
}

#[derive(Clone, Eq, PartialEq)]
enum ToolExecutionOutcomeKind {
    Completed {
        item_id: ItemId,
        source: ToolOutcomeSource,
        result: ToolResult,
    },
    Abandoned {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        reason: ToolAbandonReason,
    },
}

pub enum ToolExecutionOutcomeRef<'a> {
    Completed {
        item_id: ItemId,
        source: ToolOutcomeSource,
        result: &'a ToolResult,
    },
    Abandoned {
        item_id: ItemId,
        tool_call_id: &'a ToolCallId,
        reason: ToolAbandonReason,
    },
}

impl ToolExecutionOutcome {
    #[allow(dead_code, reason = "constructed by ToolSet terminal projection in M8")]
    fn completed(
        request: &ToolExecutionRequest,
        source: ToolOutcomeSource,
        result: ToolResult,
    ) -> Result<Self, ToolValueError> {
        if result.tool_call_id() != request.call().tool_call_id()
            || (source == ToolOutcomeSource::Executed
                && result.disposition() == ToolResultDisposition::Denied)
        {
            return Err(ToolValueError::InvalidOutcome);
        }
        Ok(Self {
            kind: ToolExecutionOutcomeKind::Completed {
                item_id: request.item_id(),
                source,
                result,
            },
        })
    }

    #[allow(dead_code, reason = "constructed by ToolSet terminal projection in M8")]
    fn abandoned(request: &ToolExecutionRequest, reason: ToolAbandonReason) -> Self {
        Self {
            kind: ToolExecutionOutcomeKind::Abandoned {
                item_id: request.item_id(),
                tool_call_id: request.call().tool_call_id().clone(),
                reason,
            },
        }
    }

    pub fn as_ref(&self) -> ToolExecutionOutcomeRef<'_> {
        match &self.kind {
            ToolExecutionOutcomeKind::Completed {
                item_id,
                source,
                result,
            } => ToolExecutionOutcomeRef::Completed {
                item_id: *item_id,
                source: *source,
                result,
            },
            ToolExecutionOutcomeKind::Abandoned {
                item_id,
                tool_call_id,
                reason,
            } => ToolExecutionOutcomeRef::Abandoned {
                item_id: *item_id,
                tool_call_id,
                reason: *reason,
            },
        }
    }
}

impl fmt::Debug for ToolExecutionOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_ref() {
            ToolExecutionOutcomeRef::Completed {
                item_id,
                source,
                result,
            } => formatter
                .debug_struct("ToolExecutionOutcome::Completed")
                .field("item_id", &item_id)
                .field("source", &source)
                .field("result", result)
                .finish(),
            ToolExecutionOutcomeRef::Abandoned {
                item_id,
                tool_call_id,
                reason,
            } => formatter
                .debug_struct("ToolExecutionOutcome::Abandoned")
                .field("item_id", &item_id)
                .field("tool_call_id", tool_call_id)
                .field("reason", &reason)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolRequirementSummaryView {
    filesystem: Option<Arc<str>>,
    network: Option<Arc<str>>,
    process: Option<Arc<str>>,
}

impl ToolRequirementSummaryView {
    #[allow(dead_code, reason = "constructed by Tools approval projection in M8")]
    fn new(
        filesystem: Option<String>,
        network: Option<String>,
        process: Option<String>,
    ) -> Result<Self, ToolValueError> {
        let maximum = ProtocolLimits::v1_0().text.max_public_summary_bytes as usize;
        Ok(Self {
            filesystem: validate_optional_text(filesystem, maximum)?,
            network: validate_optional_text(network, maximum)?,
            process: validate_optional_text(process, maximum)?,
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        filesystem: Option<String>,
        network: Option<String>,
        process: Option<String>,
    ) -> Result<Self, ToolValueError> {
        Self::new(filesystem, network, process)
    }

    pub fn filesystem(&self) -> Option<&str> {
        self.filesystem.as_deref()
    }

    pub fn network(&self) -> Option<&str> {
        self.network.as_deref()
    }

    pub fn process(&self) -> Option<&str> {
        self.process.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalOptionKindView {
    AsRequested,
    Restricted,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolApprovalOptionView {
    option_index: u32,
    kind: ToolApprovalOptionKindView,
    label: Arc<str>,
    effective_requirements: ToolRequirementSummaryView,
}

impl ToolApprovalOptionView {
    #[allow(dead_code, reason = "constructed by Tools approval projection in M8")]
    fn new(
        option_index: u32,
        kind: ToolApprovalOptionKindView,
        label: impl AsRef<str>,
        effective_requirements: ToolRequirementSummaryView,
    ) -> Result<Self, ToolValueError> {
        let label = normalize_and_validate_text(
            label.as_ref(),
            ProtocolLimits::v1_0().text.max_display_name_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidApproval)?;
        Ok(Self {
            option_index,
            kind,
            label: label.into(),
            effective_requirements,
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        option_index: u32,
        kind: ToolApprovalOptionKindView,
        label: impl AsRef<str>,
        effective_requirements: ToolRequirementSummaryView,
    ) -> Result<Self, ToolValueError> {
        Self::new(option_index, kind, label, effective_requirements)
    }

    pub const fn option_index(&self) -> u32 {
        self.option_index
    }

    pub const fn kind(&self) -> ToolApprovalOptionKindView {
        self.kind
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub const fn effective_requirements(&self) -> &ToolRequirementSummaryView {
        &self.effective_requirements
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolApprovalRequestView {
    tool_name: ToolName,
    arguments_summary: Arc<str>,
    reason: Arc<str>,
    requirements: ToolRequirementSummaryView,
    options: Arc<[ToolApprovalOptionView]>,
}

impl ToolApprovalRequestView {
    #[allow(dead_code, reason = "constructed by Tools approval projection in M8")]
    fn new(
        tool_name: ToolName,
        arguments_summary: impl AsRef<str>,
        reason: impl AsRef<str>,
        requirements: ToolRequirementSummaryView,
        options: Vec<ToolApprovalOptionView>,
    ) -> Result<Self, ToolValueError> {
        let maximum = ProtocolLimits::v1_0().interaction.max_tool_approval_options as usize;
        if options.is_empty()
            || options.len() > maximum
            || options
                .iter()
                .enumerate()
                .any(|(index, option)| option.option_index() as usize != index)
        {
            return Err(ToolValueError::InvalidApproval);
        }
        let arguments_summary = normalize_and_validate_text(
            arguments_summary.as_ref(),
            ProtocolLimits::v1_0().text.max_public_summary_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidApproval)?;
        let reason = normalize_and_validate_text(
            reason.as_ref(),
            ProtocolLimits::v1_0().text.max_description_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidApproval)?;
        let request = Self {
            tool_name,
            arguments_summary: arguments_summary.into(),
            reason: reason.into(),
            requirements,
            options: options.into(),
        };
        if tool_approval_encoded_len(&request).ok_or(ToolValueError::InvalidApproval)?
            > ProtocolLimits::v1_0()
                .interaction
                .max_interaction_view_bytes as usize
        {
            return Err(ToolValueError::InvalidApproval);
        }
        Ok(request)
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        tool_name: ToolName,
        arguments_summary: impl AsRef<str>,
        reason: impl AsRef<str>,
        requirements: ToolRequirementSummaryView,
        options: Vec<ToolApprovalOptionView>,
    ) -> Result<Self, ToolValueError> {
        Self::new(tool_name, arguments_summary, reason, requirements, options)
    }

    pub const fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }

    pub fn arguments_summary(&self) -> &str {
        &self.arguments_summary
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub const fn requirements(&self) -> &ToolRequirementSummaryView {
        &self.requirements
    }

    pub fn options(&self) -> &[ToolApprovalOptionView] {
        &self.options
    }

    #[allow(dead_code, reason = "consumed by Conversation replay in M3")]
    pub(crate) fn validate_recorded_resolution(
        &self,
        resolution: ToolApprovalResolution,
    ) -> Result<ToolApprovalResolution, ToolValueError> {
        match resolution.as_ref() {
            ToolApprovalResolutionRef::Denied => Ok(resolution),
            ToolApprovalResolutionRef::Allowed { option_index, kind } => {
                let option = self
                    .options()
                    .iter()
                    .find(|option| option.option_index() == option_index)
                    .ok_or(ToolValueError::InvalidApproval)?;
                if option.kind() != kind {
                    return Err(ToolValueError::InvalidApproval);
                }
                Ok(resolution)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalDecisionInput {
    Allow { option_index: u32 },
    Deny,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ToolApprovalResolution {
    kind: ToolApprovalResolutionKind,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
#[allow(
    dead_code,
    reason = "constructed by live approval and Conversation replay"
)]
enum ToolApprovalResolutionKind {
    Allowed {
        option_index: u32,
        kind: ToolApprovalOptionKindView,
    },
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalResolutionRef {
    Allowed {
        option_index: u32,
        kind: ToolApprovalOptionKindView,
    },
    Denied,
}

impl ToolApprovalResolution {
    pub const fn as_ref(&self) -> ToolApprovalResolutionRef {
        match self.kind {
            ToolApprovalResolutionKind::Allowed { option_index, kind } => {
                ToolApprovalResolutionRef::Allowed { option_index, kind }
            }
            ToolApprovalResolutionKind::Denied => ToolApprovalResolutionRef::Denied,
        }
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) const fn reconstruct_allowed(
        option_index: u32,
        kind: ToolApprovalOptionKindView,
    ) -> Self {
        Self {
            kind: ToolApprovalResolutionKind::Allowed { option_index, kind },
        }
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) const fn reconstruct_denied() -> Self {
        Self {
            kind: ToolApprovalResolutionKind::Denied,
        }
    }
}

impl fmt::Debug for ToolApprovalResolution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(formatter)
    }
}

#[derive(Clone, Eq, PartialEq)]
#[allow(dead_code, reason = "consumed by Tool execution control in M8")]
pub(crate) enum ToolApprovalDecision {
    AllowOnce,
    Deny,
}

#[derive(Clone, Eq, PartialEq)]
struct ToolApprovalOption {
    view: ToolApprovalOptionView,
    decision: ToolApprovalDecision,
}

impl ToolApprovalOption {
    #[allow(
        dead_code,
        reason = "constructed by ToolSet approval preparation in M8"
    )]
    fn new(
        view: ToolApprovalOptionView,
        decision: ToolApprovalDecision,
    ) -> Result<Self, ToolValueError> {
        let compatible = matches!(
            (view.kind(), &decision),
            (
                ToolApprovalOptionKindView::AsRequested,
                ToolApprovalDecision::AllowOnce
            )
        );
        if !compatible {
            return Err(ToolValueError::InvalidApproval);
        }
        Ok(Self { view, decision })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ToolApprovalRequest {
    view: ToolApprovalRequestView,
    options: Arc<[ToolApprovalOption]>,
}

impl ToolApprovalRequest {
    #[allow(
        dead_code,
        reason = "constructed by ToolSet approval preparation in M8"
    )]
    fn new(
        view: ToolApprovalRequestView,
        options: Vec<ToolApprovalOption>,
    ) -> Result<Self, ToolValueError> {
        if options.len() != view.options().len()
            || options
                .iter()
                .zip(view.options())
                .any(|(option, view)| &option.view != view)
        {
            return Err(ToolValueError::InvalidApproval);
        }
        Ok(Self {
            view,
            options: options.into(),
        })
    }

    #[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
    pub(crate) const fn view(&self) -> &ToolApprovalRequestView {
        &self.view
    }

    #[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
    pub(crate) fn resolve(
        &self,
        input: ToolApprovalDecisionInput,
    ) -> Result<ResolvedToolApproval, ToolValueError> {
        match input {
            ToolApprovalDecisionInput::Deny => Ok(ResolvedToolApproval {
                decision: ToolApprovalDecision::Deny,
                resolution: ToolApprovalResolution::reconstruct_denied(),
            }),
            ToolApprovalDecisionInput::Allow { option_index } => {
                let option = self
                    .options
                    .iter()
                    .find(|option| option.view.option_index() == option_index)
                    .ok_or(ToolValueError::InvalidApproval)?;
                Ok(ResolvedToolApproval {
                    decision: option.decision.clone(),
                    resolution: ToolApprovalResolution::reconstruct_allowed(
                        option_index,
                        option.view.kind(),
                    ),
                })
            }
        }
    }
}

#[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
pub(crate) struct ResolvedToolApproval {
    decision: ToolApprovalDecision,
    resolution: ToolApprovalResolution,
}

impl ResolvedToolApproval {
    #[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
    pub(crate) const fn decision(&self) -> &ToolApprovalDecision {
        &self.decision
    }

    #[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
    pub(crate) const fn resolution(&self) -> &ToolApprovalResolution {
        &self.resolution
    }

    #[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
    pub(crate) fn into_parts(self) -> (ToolApprovalDecision, ToolApprovalResolution) {
        (self.decision, self.resolution)
    }
}

#[cfg(test)]
pub(crate) fn live_approval_request_fixture() -> ToolApprovalRequest {
    let requirements = ToolRequirementSummaryView::new(None, None, None).unwrap();
    let option = ToolApprovalOptionView::new(
        0,
        ToolApprovalOptionKindView::AsRequested,
        "Allow once",
        requirements.clone(),
    )
    .unwrap();
    let view = ToolApprovalRequestView::new(
        "write_file".parse().unwrap(),
        "path: src/lib.rs",
        "write requested",
        requirements,
        vec![option.clone()],
    )
    .unwrap();
    ToolApprovalRequest::new(
        view,
        vec![ToolApprovalOption::new(option, ToolApprovalDecision::AllowOnce).unwrap()],
    )
    .unwrap()
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionChoice {
    option_index: u32,
    label: Arc<str>,
}

impl UserQuestionChoice {
    #[allow(dead_code, reason = "constructed by the ask-user Tool in M8")]
    fn new(option_index: u32, label: impl AsRef<str>) -> Result<Self, ToolValueError> {
        let label = normalize_and_validate_text(
            label.as_ref(),
            ProtocolLimits::v1_0().text.max_display_name_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidQuestion)?;
        Ok(Self {
            option_index,
            label: label.into(),
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        option_index: u32,
        label: impl AsRef<str>,
    ) -> Result<Self, ToolValueError> {
        Self::new(option_index, label)
    }

    pub const fn option_index(&self) -> u32 {
        self.option_index
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum UserQuestionInput {
    Text { multiline: bool },
    SingleChoice { options: Arc<[UserQuestionChoice]> },
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionField {
    question_index: u32,
    prompt: Arc<str>,
    required: bool,
    input: UserQuestionInput,
}

impl UserQuestionField {
    #[allow(dead_code, reason = "constructed by the ask-user Tool in M8")]
    fn new(
        question_index: u32,
        prompt: impl AsRef<str>,
        required: bool,
        input: UserQuestionInput,
    ) -> Result<Self, ToolValueError> {
        if let UserQuestionInput::SingleChoice { options } = &input {
            let maximum = ProtocolLimits::v1_0().interaction.max_choices_per_question as usize;
            if options.is_empty()
                || options.len() > maximum
                || !strictly_increasing(options.iter().map(UserQuestionChoice::option_index))
            {
                return Err(ToolValueError::InvalidQuestion);
            }
        }
        let prompt = normalize_and_validate_text(
            prompt.as_ref(),
            ProtocolLimits::v1_0().text.max_description_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidQuestion)?;
        Ok(Self {
            question_index,
            prompt: prompt.into(),
            required,
            input,
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        question_index: u32,
        prompt: impl AsRef<str>,
        required: bool,
        input: UserQuestionInput,
    ) -> Result<Self, ToolValueError> {
        Self::new(question_index, prompt, required, input)
    }

    pub const fn question_index(&self) -> u32 {
        self.question_index
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub const fn required(&self) -> bool {
        self.required
    }

    pub const fn input(&self) -> &UserQuestionInput {
        &self.input
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionRequest {
    title: Option<Arc<str>>,
    questions: Arc<[UserQuestionField]>,
}

impl UserQuestionRequest {
    #[allow(dead_code, reason = "constructed by the ask-user Tool in M8")]
    fn new(
        title: Option<String>,
        questions: Vec<UserQuestionField>,
    ) -> Result<Self, ToolValueError> {
        let maximum = ProtocolLimits::v1_0().interaction.max_interaction_questions as usize;
        if questions.is_empty()
            || questions.len() > maximum
            || !strictly_increasing(questions.iter().map(UserQuestionField::question_index))
        {
            return Err(ToolValueError::InvalidQuestion);
        }
        let request = Self {
            title: validate_optional_text(
                title,
                ProtocolLimits::v1_0().text.max_display_name_bytes as usize,
            )
            .map_err(|_| ToolValueError::InvalidQuestion)?,
            questions: questions.into(),
        };
        if user_question_encoded_len(&request).ok_or(ToolValueError::InvalidQuestion)?
            > ProtocolLimits::v1_0()
                .interaction
                .max_interaction_view_bytes as usize
        {
            return Err(ToolValueError::InvalidQuestion);
        }
        Ok(request)
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        title: Option<String>,
        questions: Vec<UserQuestionField>,
    ) -> Result<Self, ToolValueError> {
        Self::new(title, questions)
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn questions(&self) -> &[UserQuestionField] {
        &self.questions
    }

    pub fn validate_answer(
        &self,
        answer: UserQuestionAnswer,
    ) -> Result<UserQuestionAnswer, ToolValueError> {
        let mut answers = answer.answers().iter().peekable();
        for question in self.questions() {
            if answers
                .peek()
                .is_some_and(|answer| answer.question_index() < question.question_index())
            {
                return Err(ToolValueError::InvalidQuestion);
            }
            let matching = answers
                .peek()
                .filter(|answer| answer.question_index() == question.question_index())
                .copied();
            let Some(matching) = matching else {
                if question.required() {
                    return Err(ToolValueError::InvalidQuestion);
                }
                continue;
            };
            answers.next();
            match (question.input(), matching.value()) {
                (UserQuestionInput::Text { .. }, UserQuestionAnswerValue::Text(text)) => {
                    if question.required() && text.is_empty() {
                        return Err(ToolValueError::InvalidQuestion);
                    }
                }
                (
                    UserQuestionInput::SingleChoice { options },
                    UserQuestionAnswerValue::Choice { option_index },
                ) if options
                    .iter()
                    .any(|option| option.option_index() == *option_index) => {}
                _ => return Err(ToolValueError::InvalidQuestion),
            }
        }
        if answers.next().is_some() {
            return Err(ToolValueError::InvalidQuestion);
        }
        Ok(answer)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum UserQuestionAnswerValue {
    Text(Arc<str>),
    Choice { option_index: u32 },
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionFieldAnswer {
    question_index: u32,
    value: UserQuestionAnswerValue,
}

impl UserQuestionFieldAnswer {
    fn new(question_index: u32, value: UserQuestionAnswerValue) -> Self {
        Self {
            question_index,
            value,
        }
    }

    pub fn text(question_index: u32, text: impl AsRef<str>) -> Result<Self, ToolValueError> {
        let text = normalize_and_validate_text(
            text.as_ref(),
            ProtocolLimits::v1_0().interaction.max_answer_text_bytes as usize,
            true,
        )
        .map_err(|_| ToolValueError::InvalidQuestion)?;
        Ok(Self::new(
            question_index,
            UserQuestionAnswerValue::Text(text.into()),
        ))
    }

    pub const fn choice(question_index: u32, option_index: u32) -> Self {
        Self {
            question_index,
            value: UserQuestionAnswerValue::Choice { option_index },
        }
    }

    pub const fn question_index(&self) -> u32 {
        self.question_index
    }

    pub const fn value(&self) -> &UserQuestionAnswerValue {
        &self.value
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionAnswer {
    answers: Arc<[UserQuestionFieldAnswer]>,
}

impl UserQuestionAnswer {
    pub fn new(mut answers: Vec<UserQuestionFieldAnswer>) -> Result<Self, ToolValueError> {
        if answers.len() > ProtocolLimits::v1_0().interaction.max_interaction_questions as usize {
            return Err(ToolValueError::InvalidQuestion);
        }
        let mut aggregate = 0_usize;
        let mut previous = None;
        for answer in &mut answers {
            let index = answer.question_index();
            if previous.is_some_and(|previous| index <= previous) {
                return Err(ToolValueError::InvalidQuestion);
            }
            previous = Some(index);
            if let UserQuestionAnswerValue::Text(text) = &mut answer.value {
                let normalized = normalize_and_validate_text(
                    text,
                    ProtocolLimits::v1_0().interaction.max_answer_text_bytes as usize,
                    true,
                )
                .map_err(|_| ToolValueError::InvalidQuestion)?;
                aggregate = aggregate
                    .checked_add(normalized.len())
                    .ok_or(ToolValueError::InvalidQuestion)?;
                if aggregate
                    > ProtocolLimits::v1_0()
                        .interaction
                        .max_interaction_answer_bytes as usize
                {
                    return Err(ToolValueError::InvalidQuestion);
                }
                *text = normalized.into();
            }
        }
        let answer = Self {
            answers: answers.into(),
        };
        if user_answer_encoded_len(&answer).ok_or(ToolValueError::InvalidQuestion)?
            > ProtocolLimits::v1_0()
                .interaction
                .max_interaction_answer_bytes as usize
        {
            return Err(ToolValueError::InvalidQuestion);
        }
        Ok(answer)
    }

    pub fn answers(&self) -> &[UserQuestionFieldAnswer] {
        &self.answers
    }
}

const INTERACTION_VIEW_FIXED_BYTES: usize = 159;

fn tool_approval_encoded_len(request: &ToolApprovalRequestView) -> Option<usize> {
    let mut length = INTERACTION_VIEW_FIXED_BYTES;
    add_len(
        &mut length,
        "{\"type\":\"tool_approval\",\"data\":{\"toolName\":".len(),
    )?;
    add_len(
        &mut length,
        canonical_json_string_len(request.tool_name().as_str())?,
    )?;
    add_len(&mut length, ",\"argumentsSummary\":".len())?;
    add_len(
        &mut length,
        canonical_json_string_len(request.arguments_summary())?,
    )?;
    add_len(&mut length, ",\"reason\":".len())?;
    add_len(&mut length, canonical_json_string_len(request.reason())?)?;
    add_len(&mut length, ",\"requirements\":".len())?;
    add_len(
        &mut length,
        requirement_summary_encoded_len(request.requirements())?,
    )?;
    add_len(&mut length, ",\"options\":[".len())?;
    for (index, option) in request.options().iter().enumerate() {
        if index != 0 {
            add_len(&mut length, 1)?;
        }
        add_len(&mut length, tool_approval_option_encoded_len(option)?)?;
    }
    add_len(&mut length, "]}}".len())?;
    Some(length)
}

fn tool_approval_option_encoded_len(option: &ToolApprovalOptionView) -> Option<usize> {
    let mut length = "{\"optionIndex\":".len();
    add_len(&mut length, decimal_u32_len(option.option_index()))?;
    add_len(&mut length, ",\"kind\":".len())?;
    let kind = match option.kind() {
        ToolApprovalOptionKindView::AsRequested => "as_requested",
        ToolApprovalOptionKindView::Restricted => "restricted",
    };
    add_len(&mut length, canonical_json_string_len(kind)?)?;
    add_len(&mut length, ",\"label\":".len())?;
    add_len(&mut length, canonical_json_string_len(option.label())?)?;
    add_len(&mut length, ",\"effectiveRequirements\":".len())?;
    add_len(
        &mut length,
        requirement_summary_encoded_len(option.effective_requirements())?,
    )?;
    add_len(&mut length, 1)?;
    Some(length)
}

fn requirement_summary_encoded_len(summary: &ToolRequirementSummaryView) -> Option<usize> {
    let mut length = "{\"filesystem\":".len();
    add_len(
        &mut length,
        optional_string_encoded_len(summary.filesystem())?,
    )?;
    add_len(&mut length, ",\"network\":".len())?;
    add_len(&mut length, optional_string_encoded_len(summary.network())?)?;
    add_len(&mut length, ",\"process\":".len())?;
    add_len(&mut length, optional_string_encoded_len(summary.process())?)?;
    add_len(&mut length, 1)?;
    Some(length)
}

fn user_question_encoded_len(request: &UserQuestionRequest) -> Option<usize> {
    let mut length = INTERACTION_VIEW_FIXED_BYTES;
    add_len(
        &mut length,
        "{\"type\":\"user_question\",\"data\":{\"title\":".len(),
    )?;
    add_len(&mut length, optional_string_encoded_len(request.title())?)?;
    add_len(&mut length, ",\"questions\":[".len())?;
    for (index, question) in request.questions().iter().enumerate() {
        if index != 0 {
            add_len(&mut length, 1)?;
        }
        add_len(&mut length, user_question_field_encoded_len(question)?)?;
    }
    add_len(&mut length, "]}}".len())?;
    Some(length)
}

fn user_question_field_encoded_len(question: &UserQuestionField) -> Option<usize> {
    let mut length = "{\"questionIndex\":".len();
    add_len(&mut length, decimal_u32_len(question.question_index()))?;
    add_len(&mut length, ",\"prompt\":".len())?;
    add_len(&mut length, canonical_json_string_len(question.prompt())?)?;
    add_len(&mut length, ",\"required\":".len())?;
    add_len(&mut length, if question.required() { 4 } else { 5 })?;
    add_len(&mut length, ",\"input\":".len())?;
    match question.input() {
        UserQuestionInput::Text { multiline } => {
            add_len(
                &mut length,
                if *multiline {
                    "{\"type\":\"text\",\"data\":{\"multiline\":true}}".len()
                } else {
                    "{\"type\":\"text\",\"data\":{\"multiline\":false}}".len()
                },
            )?;
        }
        UserQuestionInput::SingleChoice { options } => {
            add_len(
                &mut length,
                "{\"type\":\"single_choice\",\"data\":{\"options\":[".len(),
            )?;
            for (index, option) in options.iter().enumerate() {
                if index != 0 {
                    add_len(&mut length, 1)?;
                }
                add_len(&mut length, "{\"optionIndex\":".len())?;
                add_len(&mut length, decimal_u32_len(option.option_index()))?;
                add_len(&mut length, ",\"label\":".len())?;
                add_len(&mut length, canonical_json_string_len(option.label())?)?;
                add_len(&mut length, 1)?;
            }
            add_len(&mut length, "]}}".len())?;
        }
    }
    add_len(&mut length, 1)?;
    Some(length)
}

fn user_answer_encoded_len(answer: &UserQuestionAnswer) -> Option<usize> {
    let mut length = "{\"answers\":[".len();
    for (index, answer) in answer.answers().iter().enumerate() {
        if index != 0 {
            add_len(&mut length, 1)?;
        }
        add_len(&mut length, "{\"questionIndex\":".len())?;
        add_len(&mut length, decimal_u32_len(answer.question_index()))?;
        add_len(&mut length, ",\"value\":".len())?;
        match answer.value() {
            UserQuestionAnswerValue::Text(text) => {
                add_len(&mut length, "{\"type\":\"text\",\"data\":".len())?;
                add_len(&mut length, canonical_json_string_len(text)?)?;
                add_len(&mut length, 1)?;
            }
            UserQuestionAnswerValue::Choice { option_index } => {
                add_len(
                    &mut length,
                    "{\"type\":\"choice\",\"data\":{\"optionIndex\":".len(),
                )?;
                add_len(&mut length, decimal_u32_len(*option_index))?;
                add_len(&mut length, 2)?;
            }
        }
        add_len(&mut length, 1)?;
    }
    add_len(&mut length, 2)?;
    Some(length)
}

fn optional_string_encoded_len(value: Option<&str>) -> Option<usize> {
    value.map_or(Some(4), canonical_json_string_len)
}

fn decimal_u32_len(value: u32) -> usize {
    if value == 0 {
        1
    } else {
        value.ilog10() as usize + 1
    }
}

fn add_len(total: &mut usize, value: usize) -> Option<()> {
    *total = total.checked_add(value)?;
    Some(())
}

fn normalize_and_validate_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<String, ToolValueError> {
    let value = normalize_newlines(value);
    validate_safe_text(&value, maximum, allow_empty).map_err(|_| ToolValueError::InvalidText)?;
    Ok(value)
}

fn validate_external_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<String, ToolValueError> {
    validate_safe_text(value, maximum, allow_empty).map_err(|_| ToolValueError::InvalidText)?;
    Ok(value.to_owned())
}

#[allow(
    dead_code,
    reason = "consumed by approval and question constructors in M8"
)]
fn validate_optional_text(
    value: Option<String>,
    maximum: usize,
) -> Result<Option<Arc<str>>, ToolValueError> {
    value
        .map(|value| normalize_and_validate_text(&value, maximum, false).map(Into::into))
        .transpose()
}

fn strictly_increasing(values: impl IntoIterator<Item = u32>) -> bool {
    let mut previous = None;
    for value in values {
        if previous.is_some_and(|previous| value <= previous) {
            return false;
        }
        previous = Some(value);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::ItemId;

    #[test]
    fn tool_input_schema_rejects_unsupported_keywords_without_scanning_const_data() {
        let supported = BoundedJsonSchema::from_slice(
            br##"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"##,
        )
        .unwrap();
        assert!(ToolInputSchema::new(supported).is_ok());

        for input in [
            br#"{"type":"string"}"#.as_slice(),
            br#"{"type":"object","multipleOf":0}"#.as_slice(),
            br#"{"type":"object","minLength":2,"maxLength":1}"#.as_slice(),
            br#"{"patternProperties":{".*":{}}}"#.as_slice(),
            br#"{"not":{"type":"string"}}"#.as_slice(),
            br##"{"$dynamicRef":"#x"}"##.as_slice(),
            br##"{"$recursiveRef":"#x"}"##.as_slice(),
            br##"{"$ref":"#/$defs/missing"}"##.as_slice(),
            br##"{"$defs":{"a":{"$ref":"#/$defs/b"},"b":{"$ref":"#/$defs/a"}},"$ref":"#/$defs/a"}"##.as_slice(),
            br##"{"type":"object","$defs":{"a~2b":{"type":"object"}},"$ref":"#/$defs/a~2b"}"##.as_slice(),
            br##"{"type":"object","$defs":{"a":{"type":"object"}},"$ref":"#/$defs/%ZZ"}"##.as_slice(),
            br#"{"type":"object","maxItems":1e-1}"#.as_slice(),
            br#"{"type":"object","minItems":18446744073709551617,"maxItems":18446744073709551616}"#.as_slice(),
        ] {
            let schema = BoundedJsonSchema::from_slice(input).unwrap();
            assert!(matches!(
                ToolInputSchema::new(schema),
                Err(ToolValueError::InvalidSchema)
            ));
        }

        let data = BoundedJsonSchema::from_slice(
            br#"{"const":{"patternProperties":{".*":{}},"not":{"type":"string"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            ToolInputSchema::new(data),
            Err(ToolValueError::InvalidSchema)
        ));

        let referenced = BoundedJsonSchema::from_slice(
            br##"{"$defs":{"arguments":{"type":"object","properties":{"path":{"type":"string"}}}},"$ref":"#/$defs/arguments"}"##,
        )
        .unwrap();
        assert!(ToolInputSchema::new(referenced).is_ok());

        for input in [
            br#"{"type":"object","multipleOf":1e-999999}"#.as_slice(),
            br#"{"type":"object","maximum":1e999999}"#.as_slice(),
            br#"{"type":"object","enum":[0,1e-999999]}"#.as_slice(),
            br#"{"type":"object","maxItems":18446744073709551616}"#.as_slice(),
            br#"{"type":"object","minItems":18446744073709551616,"maxItems":18446744073709551617}"#
                .as_slice(),
            br#"{"type":"object","maxItems":1e21}"#.as_slice(),
            br#"{"allOf":[{"type":"object"},{"properties":{"x":{"type":"string"}}}]}"#.as_slice(),
            br#"{"anyOf":[{"type":"object"},{"type":"object","additionalProperties":false}]}"#
                .as_slice(),
            br##"{"$defs":{"a/b":{"type":"object"}},"$ref":"#/$defs/a~1b"}"##.as_slice(),
            br##"{"$defs":{"a b":{"type":"object"}},"$ref":"#/$defs/a%20b"}"##.as_slice(),
        ] {
            let schema = BoundedJsonSchema::from_slice(input).unwrap();
            assert!(ToolInputSchema::new(schema).is_ok());
        }
        let mixed_root =
            BoundedJsonSchema::from_slice(br#"{"anyOf":[{"type":"object"},{"type":"string"}]}"#)
                .unwrap();
        assert!(matches!(
            ToolInputSchema::new(mixed_root),
            Err(ToolValueError::InvalidSchema)
        ));

        let mut definitions = Vec::new();
        for index in 0..300 {
            let schema = if index == 299 {
                r#"{"type":"object"}"#.to_owned()
            } else {
                format!(
                    r##"{{"anyOf":[{{"$ref":"#/$defs/n{}"}},{{"$ref":"#/$defs/n{}"}}]}}"##,
                    index + 1,
                    index + 1
                )
            };
            definitions.push(format!(r#""n{index}":{schema}"#));
        }
        let graph = format!(
            r##"{{"$defs":{{{}}},"$ref":"#/$defs/n0"}}"##,
            definitions.join(",")
        );
        let graph = BoundedJsonSchema::from_slice(graph.as_bytes()).unwrap();
        assert!(ToolInputSchema::new(graph).is_ok());

        let properties = (0..256)
            .map(|index| {
                format!(
                    r#""p{index}":{{"type":"integer","minItems":1e999999,"maxItems":1e999999}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let large_exponents = format!(r#"{{"type":"object","properties":{{{properties}}}}}"#);
        let large_exponents = BoundedJsonSchema::from_slice(large_exponents.as_bytes()).unwrap();
        assert!(ToolInputSchema::new(large_exponents).is_ok());

        let const_data = BoundedJsonSchema::from_slice(
            br#"{"type":"object","const":{"patternProperties":{".*":{}},"not":{"type":"string"}}}"#,
        )
        .unwrap();
        assert!(ToolInputSchema::new(const_data).is_ok());
    }

    #[test]
    fn result_content_enforces_part_and_aggregate_boundaries() {
        assert!(ToolResultContent::from_text_parts(vec!["x".repeat(65_536)]).is_ok());
        assert!(ToolResultContent::from_text_parts(vec!["x".repeat(65_537)]).is_err());
        assert!(
            ToolResultContent::from_text_parts((0..32).map(|_| "x".to_owned()).collect()).is_ok()
        );
        assert!(
            ToolResultContent::from_text_parts((0..33).map(|_| "x".to_owned()).collect()).is_err()
        );
        assert!(
            ToolResultContent::from_text_parts((0..4).map(|_| "x".repeat(65_536)).collect())
                .is_ok()
        );
        let mut oversized = (0..4).map(|_| "x".repeat(65_536)).collect::<Vec<_>>();
        oversized.push("x".to_owned());
        assert!(ToolResultContent::from_text_parts(oversized).is_err());
    }

    #[test]
    fn execution_outcome_enforces_source_disposition_matrix() {
        let item_id: ItemId = "itm_11111111111111111111111111111111".parse().unwrap();
        let call_id: ToolCallId = "call_1".parse().unwrap();
        let call = Arc::new(ToolCall::new(
            call_id.clone(),
            "read_file".parse().unwrap(),
            BoundedJsonObject::from_slice(b"{}").unwrap(),
            0,
        ));
        let request = ToolExecutionRequest::new(item_id, call);
        for source in [ToolOutcomeSource::PreExecution, ToolOutcomeSource::Executed] {
            for disposition in [
                ToolResultDisposition::Succeeded,
                ToolResultDisposition::Failed,
                ToolResultDisposition::Denied,
                ToolResultDisposition::Cancelled,
            ] {
                let result = ToolResult::new(
                    call_id.clone(),
                    disposition,
                    ToolResultContent::from_text_parts(vec!["result".to_owned()]).unwrap(),
                    None,
                );
                assert_eq!(
                    ToolExecutionOutcome::completed(&request, source, result).is_ok(),
                    !(source == ToolOutcomeSource::Executed
                        && disposition == ToolResultDisposition::Denied),
                    "{source:?} + {disposition:?}"
                );
            }
        }

        let completed = ToolExecutionOutcome::completed(
            &request,
            ToolOutcomeSource::Executed,
            ToolResult::new(
                call_id.clone(),
                ToolResultDisposition::Succeeded,
                ToolResultContent::from_text_parts(vec!["done".to_owned()]).unwrap(),
                None,
            ),
        )
        .unwrap();
        assert!(matches!(
            completed.as_ref(),
            ToolExecutionOutcomeRef::Completed {
                item_id: actual_item_id,
                result,
                ..
            } if actual_item_id == item_id && result.tool_call_id() == &call_id
        ));

        let mismatched = ToolResult::new(
            "call_2".parse().unwrap(),
            ToolResultDisposition::Succeeded,
            ToolResultContent::from_text_parts(vec!["done".to_owned()]).unwrap(),
            None,
        );
        assert_eq!(
            ToolExecutionOutcome::completed(&request, ToolOutcomeSource::PreExecution, mismatched),
            Err(ToolValueError::InvalidOutcome)
        );
        assert!(matches!(
            ToolExecutionOutcome::abandoned(&request, ToolAbandonReason::OutcomeUnknown).as_ref(),
            ToolExecutionOutcomeRef::Abandoned {
                item_id: actual_item_id,
                tool_call_id: actual_call_id,
                ..
            } if actual_item_id == item_id && actual_call_id == &call_id
        ));
    }

    #[test]
    fn approval_and_question_indices_and_encoded_sizes_are_bounded() {
        let requirements = ToolRequirementSummaryView::new(None, None, None).unwrap();
        let option = ToolApprovalOptionView::new(
            0,
            ToolApprovalOptionKindView::AsRequested,
            "Allow once",
            requirements.clone(),
        )
        .unwrap();
        let approval_view = ToolApprovalRequestView::new(
            "write_file".parse().unwrap(),
            "path: src/lib.rs",
            "write requested",
            requirements,
            vec![option.clone()],
        )
        .unwrap();
        let approval = ToolApprovalRequest::new(
            approval_view.clone(),
            vec![ToolApprovalOption::new(option, ToolApprovalDecision::AllowOnce).unwrap()],
        )
        .unwrap();
        let allowed = approval
            .resolve(ToolApprovalDecisionInput::Allow { option_index: 0 })
            .unwrap();
        assert!(matches!(
            allowed.decision(),
            ToolApprovalDecision::AllowOnce
        ));
        assert!(matches!(
            allowed.resolution().as_ref(),
            ToolApprovalResolutionRef::Allowed {
                option_index: 0,
                kind: ToolApprovalOptionKindView::AsRequested,
            }
        ));
        assert!(matches!(
            approval.resolve(ToolApprovalDecisionInput::Allow { option_index: 1 }),
            Err(ToolValueError::InvalidApproval)
        ));
        assert!(matches!(
            approval
                .resolve(ToolApprovalDecisionInput::Deny)
                .unwrap()
                .resolution()
                .as_ref(),
            ToolApprovalResolutionRef::Denied
        ));
        assert!(
            approval_view
                .validate_recorded_resolution(ToolApprovalResolution::reconstruct_allowed(
                    0,
                    ToolApprovalOptionKindView::AsRequested,
                ))
                .is_ok()
        );
        assert!(matches!(
            approval_view.validate_recorded_resolution(
                ToolApprovalResolution::reconstruct_allowed(
                    0,
                    ToolApprovalOptionKindView::Restricted,
                )
            ),
            Err(ToolValueError::InvalidApproval)
        ));
        assert!(matches!(
            approval_view.validate_recorded_resolution(
                ToolApprovalResolution::reconstruct_allowed(
                    1,
                    ToolApprovalOptionKindView::AsRequested,
                )
            ),
            Err(ToolValueError::InvalidApproval)
        ));
        assert!(
            approval_view
                .validate_recorded_resolution(ToolApprovalResolution::reconstruct_denied())
                .is_ok()
        );

        let restricted_view = ToolApprovalOptionView::new(
            0,
            ToolApprovalOptionKindView::Restricted,
            "Restricted",
            ToolRequirementSummaryView::new(None, None, None).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ToolApprovalOption::new(restricted_view, ToolApprovalDecision::AllowOnce),
            Err(ToolValueError::InvalidApproval)
        ));

        let large_requirements = ToolRequirementSummaryView::new(
            Some("x".repeat(8_192)),
            Some("x".repeat(8_192)),
            Some("x".repeat(8_192)),
        )
        .unwrap();
        let large_options = (0..16)
            .map(|index| {
                ToolApprovalOptionView::new(
                    index,
                    ToolApprovalOptionKindView::Restricted,
                    "x".repeat(256),
                    large_requirements.clone(),
                )
                .unwrap()
            })
            .collect();
        assert!(
            ToolApprovalRequestView::new(
                "write_file".parse().unwrap(),
                "x".repeat(8_192),
                "x".repeat(8_192),
                large_requirements,
                large_options,
            )
            .is_err()
        );

        let choices = vec![
            UserQuestionChoice::new(2, "A").unwrap(),
            UserQuestionChoice::new(4, "B").unwrap(),
        ];
        let first = UserQuestionField::new(
            1,
            "Where?",
            true,
            UserQuestionInput::SingleChoice {
                options: choices.into(),
            },
        )
        .unwrap();
        let second = UserQuestionField::new(
            3,
            "Why?",
            false,
            UserQuestionInput::Text { multiline: true },
        )
        .unwrap();
        let question = UserQuestionRequest::new(None, vec![first, second]).unwrap();

        let valid = UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::choice(1, 4)]).unwrap();
        assert!(question.validate_answer(valid).is_ok());
        let unknown_question =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::choice(5, 4)]).unwrap();
        assert!(question.validate_answer(unknown_question).is_err());
        let missing_required =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(3, "optional").unwrap()])
                .unwrap();
        assert!(question.validate_answer(missing_required).is_err());
        let wrong_family =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(1, "wrong").unwrap()])
                .unwrap();
        assert!(question.validate_answer(wrong_family).is_err());
        let unknown_choice =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::choice(1, 3)]).unwrap();
        assert!(question.validate_answer(unknown_choice).is_err());

        let required_text = UserQuestionRequest::new(
            None,
            vec![
                UserQuestionField::new(
                    0,
                    "Explain",
                    true,
                    UserQuestionInput::Text { multiline: false },
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let empty_text =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "").unwrap()]).unwrap();
        assert!(required_text.validate_answer(empty_text).is_err());

        let answer = UserQuestionFieldAnswer::text(1, "a\r\nb").unwrap();
        let answer = UserQuestionAnswer::new(vec![answer]).unwrap();
        match answer.answers()[0].value() {
            UserQuestionAnswerValue::Text(text) => assert_eq!(text.as_ref(), "a\nb"),
            UserQuestionAnswerValue::Choice { .. } => panic!("wrong answer family"),
        }
        let expected = serde_json::json!({
            "answers": [{
                "questionIndex": 1,
                "value": {"type": "text", "data": "a\nb"}
            }]
        });
        assert_eq!(
            user_answer_encoded_len(&answer).unwrap(),
            serde_json::to_vec(&expected).unwrap().len()
        );

        let empty = UserQuestionAnswer::new(
            (0..4)
                .map(|index| UserQuestionFieldAnswer::text(index, "").unwrap())
                .collect(),
        )
        .unwrap();
        let maximum = ProtocolLimits::v1_0()
            .interaction
            .max_interaction_answer_bytes as usize;
        let text_budget = maximum - user_answer_encoded_len(&empty).unwrap();
        let make_answer = |total_text: usize| {
            let mut remaining = total_text;
            let answers = (0..4)
                .map(|index| {
                    let size = remaining.min(16_384);
                    remaining -= size;
                    UserQuestionFieldAnswer::text(index, "x".repeat(size)).unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(remaining, 0);
            UserQuestionAnswer::new(answers)
        };
        let boundary = make_answer(text_budget).unwrap();
        assert_eq!(user_answer_encoded_len(&boundary), Some(maximum));
        assert!(make_answer(text_budget + 1).is_err());

        let large_choices = (0..64)
            .map(|index| UserQuestionChoice::new(index, "x".repeat(256)).unwrap())
            .collect::<Vec<_>>();
        let large_questions = (0..32)
            .map(|index| {
                UserQuestionField::new(
                    index,
                    "x".repeat(8_192),
                    true,
                    UserQuestionInput::SingleChoice {
                        options: large_choices.clone().into(),
                    },
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(UserQuestionRequest::new(None, large_questions).is_err());
    }

    #[test]
    fn interaction_request_size_gates_match_complete_canonical_views() {
        let maximum = ProtocolLimits::v1_0()
            .interaction
            .max_interaction_view_bytes as usize;

        let approval_base = approval_with_extra_text(0).unwrap();
        let approval_extra = maximum - tool_approval_encoded_len(&approval_base).unwrap();
        let approval = approval_with_extra_text(approval_extra).unwrap();
        assert_eq!(tool_approval_encoded_len(&approval), Some(maximum));
        assert_eq!(approval_view_json_len(&approval), maximum);
        assert!(approval_with_extra_text(approval_extra + 1).is_err());

        let question_base = question_with_extra_text(0).unwrap();
        let question_extra = maximum - user_question_encoded_len(&question_base).unwrap();
        let question = question_with_extra_text(question_extra).unwrap();
        assert_eq!(user_question_encoded_len(&question), Some(maximum));
        assert_eq!(question_view_json_len(&question), maximum);
        assert!(question_with_extra_text(question_extra + 1).is_err());
    }

    fn approval_with_extra_text(
        mut extra: usize,
    ) -> Result<ToolApprovalRequestView, ToolValueError> {
        fn requirements(extra: &mut usize) -> ToolRequirementSummaryView {
            let mut text = || {
                let additional = (*extra).min(8_191);
                *extra -= additional;
                Some("x".repeat(1 + additional))
            };
            ToolRequirementSummaryView::new(text(), text(), text()).unwrap()
        }

        let top_requirements = requirements(&mut extra);
        let options = (0..16)
            .map(|index| {
                ToolApprovalOptionView::new(
                    index,
                    ToolApprovalOptionKindView::Restricted,
                    "x",
                    requirements(&mut extra),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(extra, 0);
        ToolApprovalRequestView::new(
            "write_file".parse().unwrap(),
            "x",
            "x",
            top_requirements,
            options,
        )
    }

    fn question_with_extra_text(mut extra: usize) -> Result<UserQuestionRequest, ToolValueError> {
        let questions = (0..32)
            .map(|question_index| {
                let prompt_extra = extra.min(8_191);
                extra -= prompt_extra;
                let options = (0..64)
                    .map(|option_index| {
                        let label_extra = extra.min(255);
                        extra -= label_extra;
                        UserQuestionChoice::new(option_index, "x".repeat(1 + label_extra)).unwrap()
                    })
                    .collect::<Vec<_>>();
                UserQuestionField::new(
                    question_index,
                    "x".repeat(1 + prompt_extra),
                    true,
                    UserQuestionInput::SingleChoice {
                        options: options.into(),
                    },
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(extra, 0);
        UserQuestionRequest::new(Some("x".to_owned()), questions)
    }

    fn approval_view_json_len(request: &ToolApprovalRequestView) -> usize {
        let requirements = |value: &ToolRequirementSummaryView| {
            serde_json::json!({
                "filesystem": value.filesystem(),
                "network": value.network(),
                "process": value.process(),
            })
        };
        let options = request
            .options()
            .iter()
            .map(|option| {
                serde_json::json!({
                    "optionIndex": option.option_index(),
                    "kind": match option.kind() {
                        ToolApprovalOptionKindView::AsRequested => "as_requested",
                        ToolApprovalOptionKindView::Restricted => "restricted",
                    },
                    "label": option.label(),
                    "effectiveRequirements": requirements(option.effective_requirements()),
                })
            })
            .collect::<Vec<_>>();
        interaction_view_json_len(serde_json::json!({
            "type": "tool_approval",
            "data": {
                "toolName": request.tool_name().as_str(),
                "argumentsSummary": request.arguments_summary(),
                "reason": request.reason(),
                "requirements": requirements(request.requirements()),
                "options": options,
            }
        }))
    }

    fn question_view_json_len(request: &UserQuestionRequest) -> usize {
        let questions = request
            .questions()
            .iter()
            .map(|question| {
                let input = match question.input() {
                    UserQuestionInput::Text { multiline } => {
                        serde_json::json!({"type": "text", "data": {"multiline": multiline}})
                    }
                    UserQuestionInput::SingleChoice { options } => serde_json::json!({
                        "type": "single_choice",
                        "data": {"options": options.iter().map(|option| serde_json::json!({
                            "optionIndex": option.option_index(),
                            "label": option.label(),
                        })).collect::<Vec<_>>()}
                    }),
                };
                serde_json::json!({
                    "questionIndex": question.question_index(),
                    "prompt": question.prompt(),
                    "required": question.required(),
                    "input": input,
                })
            })
            .collect::<Vec<_>>();
        interaction_view_json_len(serde_json::json!({
            "type": "user_question",
            "data": {"title": request.title(), "questions": questions}
        }))
    }

    fn interaction_view_json_len(request: Value) -> usize {
        serde_json::to_vec(&serde_json::json!({
            "requestId": "req_00000000000000000000000000000000",
            "turnId": "trn_00000000000000000000000000000000",
            "itemId": "itm_00000000000000000000000000000000",
            "request": request,
        }))
        .unwrap()
        .len()
    }
}
