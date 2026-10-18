use std::str::FromStr;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModelRefError {
    #[error("model reference must be 1..=256 bytes")]
    InvalidLength,
    #[error("model reference violates its stable symbolic grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModelRef(Box<str>);

impl ModelRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ModelRef {
    type Err = ModelRefError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > 256 {
            return Err(ModelRefError::InvalidLength);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        }) {
            return Err(ModelRefError::InvalidGrammar);
        }
        Ok(Self(value.into()))
    }
}

crate::value::string_newtype_serde!(ModelRef);
