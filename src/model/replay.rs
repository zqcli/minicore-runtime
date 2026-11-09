use std::fmt;
use std::sync::Arc;

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
    payload: Arc<Value>,
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
        crate::value::validate_json_nesting(&payload)
            .map_err(|_| ModelValueError::InvalidProviderReplay)?;
        let mut replay = Self {
            format,
            payload: Arc::new(payload),
            bytes: 0,
        };
        replay.bytes = crate::value::json_bytes(&replay);
        Ok(replay)
    }

    pub fn format(&self) -> &str {
        &self.format
    }

    pub fn payload(&self) -> &Value {
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
