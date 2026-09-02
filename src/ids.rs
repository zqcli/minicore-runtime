use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IdError {
    #[error("runtime ID has an invalid prefix")]
    InvalidPrefix,
    #[error("runtime ID has an invalid length")]
    InvalidLength,
    #[error("runtime ID contains non-canonical hexadecimal")]
    InvalidHex,
    #[error("runtime ID payload must not be all zero")]
    ZeroPayload,
    #[error("cryptographic random source unavailable")]
    EntropyUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolCallIdError {
    #[error("tool call ID must be 1..=256 bytes")]
    InvalidLength,
    #[error("tool call ID must contain printable ASCII text")]
    NonPrintable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ContextSourceIdError {
    #[error("context source ID must be 1..=128 bytes")]
    InvalidLength,
    #[error("context source ID violates the stable symbolic grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContextSourceId(Box<str>);

impl ContextSourceId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ContextSourceIdError> {
        value.as_ref().parse()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ContextSourceId {
    type Err = ContextSourceIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > 128 {
            return Err(ContextSourceIdError::InvalidLength);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        }) {
            return Err(ContextSourceIdError::InvalidGrammar);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ContextSourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for ContextSourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for ContextSourceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ContextSourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

fn random_nonzero_bytes() -> Result<[u8; 16], IdError> {
    loop {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| IdError::EntropyUnavailable)?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Ok(bytes);
        }
    }
}

fn decode_runtime_id(value: &str, prefix: &str) -> Result<[u8; 16], IdError> {
    let payload = value.strip_prefix(prefix).ok_or(IdError::InvalidPrefix)?;
    if payload.len() != 32 {
        return Err(IdError::InvalidLength);
    }

    let mut bytes = [0; 16];
    for (index, pair) in payload.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_lower_hex(pair[0]).ok_or(IdError::InvalidHex)?;
        let low = decode_lower_hex(pair[1]).ok_or(IdError::InvalidHex)?;
        bytes[index] = (high << 4) | low;
    }
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(IdError::ZeroPayload);
    }
    Ok(bytes)
}

fn decode_lower_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn encode_runtime_id(prefix: &str, bytes: &[u8; 16]) -> String {
    let mut value = String::with_capacity(prefix.len() + 32);
    value.push_str(prefix);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

fn deserialize_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(D::Error::custom)
}

macro_rules! runtime_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 16]);

        impl $name {
            pub fn new() -> Result<Self, IdError> {
                Self::generate()
            }

            pub fn generate() -> Result<Self, IdError> {
                random_nonzero_bytes().map(Self)
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                decode_runtime_id(value, $prefix).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&encode_runtime_id($prefix, &self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&encode_runtime_id($prefix, &self.0))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserialize_from_str(deserializer)
            }
        }
    };
}

runtime_id!(SessionId, "ses_");
runtime_id!(SessionInstanceId, "ins_");
runtime_id!(TurnId, "trn_");
runtime_id!(InteractionId, "int_");
runtime_id!(LoopId, "lup_");

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolCallId(Box<str>);

impl ToolCallId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ToolCallIdError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > 256 {
            return Err(ToolCallIdError::InvalidLength);
        }
        if !value
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
        {
            return Err(ToolCallIdError::NonPrintable);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolCallId {
    type Err = ToolCallIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
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

impl Serialize for ToolCallId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolCallId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}
