use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WireScalarError {
    #[error("unexpected scalar prefix")]
    UnexpectedPrefix,
    #[error("invalid scalar length")]
    InvalidLength,
    #[error("invalid scalar alphabet")]
    InvalidAlphabet,
    #[error("all-zero scalar payload is not allowed")]
    ZeroPayload,
    #[error("invalid decimal scalar")]
    InvalidDecimal,
    #[error("scalar is not canonically encoded")]
    NonCanonical,
}

#[derive(Debug, Error)]
#[error("cryptographic random source unavailable")]
pub struct IdGenerationError(getrandom::Error);

fn decode_prefixed_hex<const N: usize>(
    value: &str,
    prefix: &str,
) -> Result<[u8; N], WireScalarError> {
    let payload = value
        .strip_prefix(prefix)
        .ok_or(WireScalarError::UnexpectedPrefix)?;
    if payload.len() != N * 2 {
        return Err(WireScalarError::InvalidLength);
    }

    let mut bytes = [0; N];
    for (index, pair) in payload.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_lower_hex(pair[0]).ok_or(WireScalarError::InvalidAlphabet)?;
        let low = decode_lower_hex(pair[1]).ok_or(WireScalarError::InvalidAlphabet)?;
        bytes[index] = (high << 4) | low;
    }
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(WireScalarError::ZeroPayload);
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

fn encode_prefixed_hex<const N: usize>(prefix: &str, bytes: &[u8; N]) -> String {
    let mut value = String::with_capacity(prefix.len() + N * 2);
    value.push_str(prefix);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

fn random_nonzero_bytes<const N: usize>() -> Result<[u8; N], IdGenerationError> {
    loop {
        let mut bytes = [0; N];
        getrandom::fill(&mut bytes).map_err(IdGenerationError)?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Ok(bytes);
        }
    }
}

fn deserialize_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(serde::de::Error::custom)
}

macro_rules! define_runtime_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 16]);

        impl $name {
            pub fn generate() -> Result<Self, IdGenerationError> {
                random_nonzero_bytes().map(Self)
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = WireScalarError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                decode_prefixed_hex(value, $prefix).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&encode_prefixed_hex($prefix, &self.0))
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
                serializer.serialize_str(&encode_prefixed_hex($prefix, &self.0))
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

define_runtime_id!(AgentId, "agt_");
define_runtime_id!(SessionId, "ses_");
define_runtime_id!(TurnId, "trn_");
define_runtime_id!(ItemId, "itm_");
define_runtime_id!(RequestId, "req_");
define_runtime_id!(EntryId, "ent_");
define_runtime_id!(CommandId, "cmd_");

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct InteractionResolutionKey([u8; 16]);

impl InteractionResolutionKey {
    pub fn generate() -> Result<Self, IdGenerationError> {
        random_nonzero_bytes().map(Self)
    }

    pub(crate) fn encoded(&self) -> String {
        encode_prefixed_hex("irk_", &self.0)
    }
}

impl FromStr for InteractionResolutionKey {
    type Err = WireScalarError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_prefixed_hex(value, "irk_").map(Self)
    }
}

impl Serialize for InteractionResolutionKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_prefixed_hex("irk_", &self.0))
    }
}

impl<'de> Deserialize<'de> for InteractionResolutionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageCursor([u8; 32]);

impl PageCursor {
    pub fn generate() -> Result<Self, IdGenerationError> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(IdGenerationError)?;
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl FromStr for PageCursor {
    type Err = WireScalarError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let payload = value
            .strip_prefix("pc1_")
            .ok_or(WireScalarError::UnexpectedPrefix)?;
        if payload.len() != 43 {
            return Err(WireScalarError::InvalidLength);
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| WireScalarError::InvalidAlphabet)?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| WireScalarError::InvalidLength)?;
        if URL_SAFE_NO_PAD.encode(bytes) != payload {
            return Err(WireScalarError::NonCanonical);
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for PageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "pc1_{}", URL_SAFE_NO_PAD.encode(self.0))
    }
}

impl fmt::Debug for PageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for PageCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PageCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

macro_rules! define_revision {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            pub const fn new(value: NonZeroU64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl FromStr for $name {
            type Err = WireScalarError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let payload = value
                    .strip_prefix($prefix)
                    .ok_or(WireScalarError::UnexpectedPrefix)?;
                parse_canonical_u64(payload, false).and_then(|value| {
                    NonZeroU64::new(value)
                        .map(Self)
                        .ok_or(WireScalarError::InvalidDecimal)
                })
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}{}", $prefix, self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
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

define_revision!(AgentRevision, "ar_");
define_revision!(SessionDefinitionRevision, "sdr_");
define_revision!(AgentMetadataRevision, "amr_");
define_revision!(SessionMetadataRevision, "smr_");
define_revision!(WorkspaceRevision, "wr_");

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalU64(u64);

impl CanonicalU64 {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl FromStr for CanonicalU64 {
    type Err = WireScalarError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_canonical_u64(value, true).map(Self)
    }
}

impl fmt::Display for CanonicalU64 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for CanonicalU64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CanonicalU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

fn parse_canonical_u64(value: &str, allow_zero: bool) -> Result<u64, WireScalarError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WireScalarError::InvalidDecimal);
    }
    if value.len() > 1 && value.starts_with('0') {
        return Err(WireScalarError::NonCanonical);
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| WireScalarError::InvalidDecimal)?;
    if !allow_zero && parsed == 0 {
        return Err(WireScalarError::InvalidDecimal);
    }
    Ok(parsed)
}
