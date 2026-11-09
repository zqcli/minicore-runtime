use std::fmt;
use std::io::{self, Write};
use std::str::FromStr;
use std::sync::Arc;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Matches serde_json's default recursion guard: 127 nested arrays or objects.
pub const MAX_JSON_NESTING: usize = 127;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ValueError {
    #[error("text contains disallowed control characters")]
    InvalidText,
    #[error("JSON nesting exceeds the parser's stack-safety guard")]
    JsonTooDeep,
}

/// Shared UTF-8 text without an application byte limit.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Text(Arc<str>);

impl Text {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(Arc::from(value.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0.to_string()
    }

    pub(crate) fn shared(&self) -> Arc<str> {
        Arc::clone(&self.0)
    }

    pub fn byte_len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) fn valid_text(value: &str, allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty())
        && value
            .chars()
            .all(|c| !c.is_control() || matches!(c, '\n' | '\t'))
}

/// Normalize model text without increasing its UTF-8 byte length.
pub(crate) fn normalize_model_text(value: &str) -> std::borrow::Cow<'_, str> {
    if valid_text(value, true) {
        return std::borrow::Cow::Borrowed(value);
    }
    let mut text = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        text.push(match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                '\n'
            }
            '\n' | '\t' => c,
            c if c.is_control() => ' ',
            c => c,
        });
    }
    std::borrow::Cow::Owned(text)
}

pub(crate) fn deserialize_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(de::Error::custom)
}

macro_rules! string_newtype_serde {
    ($name:ident) => {
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(self, f)
            }
        }
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                crate::value::deserialize_from_str(deserializer)
            }
        }
    };
}
pub(crate) use string_newtype_serde;

pub(crate) fn json_bytes(value: &impl Serialize) -> usize {
    let mut sink = JsonCountingWriter(0);
    serde_json::to_writer(&mut sink, value)
        .expect("validated JSON serialization to a counter cannot fail");
    sink.0
}

impl AsRef<str> for Text {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Text")
            .field("bytes", &self.byte_len())
            .finish()
    }
}

impl fmt::Display for Text {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Rejects nesting that would exceed serde_json's default stack-safety guard.
pub fn validate_json_nesting(value: &serde_json::Value) -> Result<(), ValueError> {
    fn visit(value: &serde_json::Value, containers: usize) -> Result<(), ValueError> {
        if value.is_array() || value.is_object() {
            if containers == MAX_JSON_NESTING {
                return Err(ValueError::JsonTooDeep);
            }
            match value {
                serde_json::Value::Array(values) => {
                    for value in values {
                        visit(value, containers + 1)?;
                    }
                }
                serde_json::Value::Object(values) => {
                    for value in values.values() {
                        visit(value, containers + 1)?;
                    }
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }
    visit(value, 0)
}

struct JsonCountingWriter(usize);

impl Write for JsonCountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
