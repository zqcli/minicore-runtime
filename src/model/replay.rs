use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::ModelValueError;

/// Provider-owned replay metadata. Runtime transports it without interpreting it.
///
/// The host may inspect and serialize the payload. Debug output exposes only its
/// byte count. Canonical assistant parts remain the sole display/tool authority.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ProviderReplay {
    format: String,
    payload: Value,
    #[serde(skip)]
    bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    format: String,
    payload: Value,
}

impl ProviderReplay {
    /// Maximum compact JSON size of the entire replay envelope, including keys.
    pub const MAX_BYTES: usize = 256 * 1024;
    /// Maximum payload depth (the payload root has depth zero).
    pub const MAX_DEPTH: usize = 32;
    /// Maximum payload nodes, independent of the byte ceiling.
    pub const MAX_NODES: usize = 16_384;

    pub fn new(format: impl Into<String>, payload: Value) -> Result<Self, ModelValueError> {
        let format = format.into();
        if format.is_empty()
            || format.len() > 128
            || !format
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/'))
        {
            return Err(ModelValueError::InvalidProviderReplay);
        }
        validate_shape(&payload, 0, &mut 0)?;
        let mut replay = Self {
            format,
            payload,
            bytes: 0,
        };
        let mut sink = CountingWriter(0);
        serde_json::to_writer(&mut sink, &replay)
            .map_err(|_| ModelValueError::InvalidProviderReplay)?;
        replay.bytes = sink.0;
        Ok(replay)
    }

    pub fn format(&self) -> &str {
        &self.format
    }

    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    pub const fn byte_len(&self) -> usize {
        self.bytes
    }
}

impl fmt::Debug for ProviderReplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderReplay")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for ProviderReplay {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.format, wire.payload).map_err(serde::de::Error::custom)
    }
}

fn validate_shape(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), ModelValueError> {
    *nodes += 1;
    if depth > ProviderReplay::MAX_DEPTH || *nodes > ProviderReplay::MAX_NODES {
        return Err(ModelValueError::InvalidProviderReplay);
    }
    match value {
        Value::Array(items) => {
            for item in items {
                validate_shape(item, depth + 1, nodes)?;
            }
        }
        Value::Object(items) => {
            for item in items.values() {
                validate_shape(item, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

struct CountingWriter(usize);
impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > ProviderReplay::MAX_BYTES.saturating_sub(self.0) {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "replay byte limit exceeded",
            ));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
