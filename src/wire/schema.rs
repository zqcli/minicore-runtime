use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use regex_syntax::Parser;
use thiserror::Error;

use super::bounded_json::{
    BoundedJsonError, BoundedJsonValue, JsonNode, JsonParseLimits, parse_node,
};
use super::limits::{ProtocolLimits, WireLimit};

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BoundedJsonSchemaError {
    #[error("bounded JSON failure")]
    Json(#[from] BoundedJsonError),
    #[error("JSON Schema root must be an object")]
    RootObjectRequired,
    #[error("JSON Schema keyword collection exceeds its limit")]
    KeywordCollectionLimit,
    #[error("JSON Schema regex text exceeds its byte limit")]
    RegexBytesLimit,
    #[error("JSON Schema regex is unsupported or invalid")]
    InvalidRegex,
    #[error("JSON Schema reference must be a local fragment")]
    RemoteReference,
    #[error("JSON Schema reference must be a string")]
    InvalidReference,
}

#[derive(Clone)]
pub struct BoundedJsonSchema(BoundedJsonValue);

impl BoundedJsonSchema {
    pub fn from_slice(input: &[u8]) -> Result<Self, BoundedJsonSchemaError> {
        let parse_limits = JsonParseLimits::schema();
        let node = parse_node(input, parse_limits)?;
        if node.as_object().is_none() {
            return Err(BoundedJsonSchemaError::RootObjectRequired);
        }
        validate_schema(&node)?;
        let value = BoundedJsonValue::from_node(node, parse_limits.max_encoded_bytes)?;
        Ok(Self(value))
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

impl FromStr for BoundedJsonSchema {
    type Err = BoundedJsonSchemaError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_slice(value.as_bytes())
    }
}

impl fmt::Debug for BoundedJsonSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedJsonSchema")
            .field("canonical_bytes", &self.0.canonical_bytes().len())
            .finish_non_exhaustive()
    }
}

impl PartialEq for BoundedJsonSchema {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for BoundedJsonSchema {}

impl Hash for BoundedJsonSchema {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

fn validate_schema(node: &JsonNode) -> Result<(), BoundedJsonSchemaError> {
    let schema_limits = ProtocolLimits::v1_0().embedded_json.schema;
    let collection_limit =
        WireLimit::new(schema_limits.max_properties_required_or_enum_items as usize);
    let regex_limit = WireLimit::new(schema_limits.max_regex_bytes as usize);
    validate_schema_node(node, collection_limit, regex_limit)
}

fn validate_schema_node(
    node: &JsonNode,
    collection_limit: WireLimit,
    regex_limit: WireLimit,
) -> Result<(), BoundedJsonSchemaError> {
    let Some(values) = node.as_object() else {
        return Ok(());
    };
    for (key, value) in values {
        match key.as_ref() {
            "$ref" => validate_reference(value)?,
            "pattern" => validate_pattern_node(value, regex_limit)?,
            "properties" => {
                validate_schema_map(value, true, false, collection_limit, regex_limit)?;
            }
            "patternProperties" => {
                validate_schema_map(value, false, true, collection_limit, regex_limit)?;
            }
            "$defs" | "dependentSchemas" => {
                validate_schema_map(value, false, false, collection_limit, regex_limit)?;
            }
            "required" | "enum" => {
                if let Some(items) = value.as_array() {
                    collection_limit
                        .validate(items.len())
                        .map_err(|_| BoundedJsonSchemaError::KeywordCollectionLimit)?;
                }
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                if let Some(schemas) = value.as_array() {
                    for schema in schemas {
                        validate_schema_node(schema, collection_limit, regex_limit)?;
                    }
                }
            }
            "additionalProperties"
            | "contains"
            | "contentSchema"
            | "else"
            | "if"
            | "items"
            | "not"
            | "propertyNames"
            | "then"
            | "unevaluatedItems"
            | "unevaluatedProperties" => {
                validate_schema_node(value, collection_limit, regex_limit)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_schema_map(
    node: &JsonNode,
    limit_members: bool,
    keys_are_patterns: bool,
    collection_limit: WireLimit,
    regex_limit: WireLimit,
) -> Result<(), BoundedJsonSchemaError> {
    let Some(schemas) = node.as_object() else {
        return Ok(());
    };
    if limit_members {
        collection_limit
            .validate(schemas.len())
            .map_err(|_| BoundedJsonSchemaError::KeywordCollectionLimit)?;
    }
    for (key, schema) in schemas {
        if keys_are_patterns {
            validate_pattern(key, regex_limit)?;
        }
        validate_schema_node(schema, collection_limit, regex_limit)?;
    }
    Ok(())
}

fn validate_reference(node: &JsonNode) -> Result<(), BoundedJsonSchemaError> {
    let reference = node
        .as_str()
        .ok_or(BoundedJsonSchemaError::InvalidReference)?;
    if !reference.starts_with('#') {
        return Err(BoundedJsonSchemaError::RemoteReference);
    }
    Ok(())
}

fn validate_pattern_node(node: &JsonNode, limit: WireLimit) -> Result<(), BoundedJsonSchemaError> {
    let pattern = node.as_str().ok_or(BoundedJsonSchemaError::InvalidRegex)?;
    validate_pattern(pattern, limit)
}

fn validate_pattern(pattern: &str, limit: WireLimit) -> Result<(), BoundedJsonSchemaError> {
    limit
        .validate_str(pattern)
        .map_err(|_| BoundedJsonSchemaError::RegexBytesLimit)?;
    Parser::new()
        .parse(pattern)
        .map(|_| ())
        .map_err(|_| BoundedJsonSchemaError::InvalidRegex)
}
