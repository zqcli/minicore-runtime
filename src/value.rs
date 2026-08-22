use std::fmt;
use std::io::{self, Write};

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub const MAX_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_JSON_BYTES: usize = 64 * 1024;
pub const MAX_JSON_DEPTH: usize = 32;
pub const MAX_JSON_NODES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ValueError {
    #[error("bounded text limit exceeds the absolute cap")]
    TextLimitTooLarge,
    #[error("bounded text exceeds its byte limit")]
    TextTooLarge,
    #[error("JSON limit exceeds the absolute cap")]
    JsonLimitTooLarge,
    #[error("JSON value exceeds the requested byte limit")]
    JsonTooLarge,
    #[error("JSON value exceeds the absolute nesting depth")]
    JsonTooDeep,
    #[error("JSON value exceeds the absolute node limit")]
    JsonTooManyNodes,
    #[error("JSON value could not be serialized")]
    JsonSerialization,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BoundedText(String);

impl BoundedText {
    pub const MAX_BYTES: usize = MAX_TEXT_BYTES;

    pub fn new(value: impl AsRef<str>) -> Result<Self, ValueError> {
        Self::new_with_max_bytes(value, Self::MAX_BYTES)
    }

    pub fn new_with_max_bytes(
        value: impl AsRef<str>,
        max_bytes: usize,
    ) -> Result<Self, ValueError> {
        let value = value.as_ref();
        if max_bytes > Self::MAX_BYTES {
            return Err(ValueError::TextLimitTooLarge);
        }
        if value.len() > max_bytes {
            return Err(ValueError::TextTooLarge);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn byte_len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AsRef<str> for BoundedText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for BoundedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedText")
            .field("bytes", &self.byte_len())
            .finish()
    }
}

impl fmt::Display for BoundedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for BoundedText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

struct BoundedTextVisitor;

impl<'de> Visitor<'de> for BoundedTextVisitor {
    type Value = BoundedText;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a UTF-8 string within the absolute byte cap")
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        BoundedText::new(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        BoundedText::new(value).map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        BoundedText::new(value).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for BoundedText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_string(BoundedTextVisitor)
    }
}

pub fn validate_json_size(value: &serde_json::Value, max_bytes: usize) -> Result<(), ValueError> {
    if max_bytes > MAX_JSON_BYTES {
        return Err(ValueError::JsonLimitTooLarge);
    }

    let mut nodes = 0;
    validate_json_shape(value, 0, &mut nodes)?;
    let mut sink = JsonCountingWriter::new(max_bytes);
    match serde_json::to_writer(&mut sink, value) {
        Ok(()) => Ok(()),
        Err(_) if sink.exceeded => Err(ValueError::JsonTooLarge),
        Err(_) => Err(ValueError::JsonSerialization),
    }
}

struct JsonCountingWriter {
    max_bytes: usize,
    written: usize,
    exceeded: bool,
}

impl JsonCountingWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            written: 0,
            exceeded: false,
        }
    }
}

impl Write for JsonCountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.max_bytes.saturating_sub(self.written);
        if bytes.len() > remaining {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "JSON byte limit exceeded",
            ));
        }
        self.written += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_json_shape(
    value: &serde_json::Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), ValueError> {
    if depth > MAX_JSON_DEPTH {
        return Err(ValueError::JsonTooDeep);
    }
    *nodes += 1;
    if *nodes > MAX_JSON_NODES {
        return Err(ValueError::JsonTooManyNodes);
    }

    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                validate_json_shape(value, depth + 1, nodes)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                validate_json_shape(value, depth + 1, nodes)?;
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
    Ok(())
}
