use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::wire::lexical::{LexicalError, validate_stable_symbolic_key};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SkillIdError {
    #[error("skill ID must be 1..=128 bytes")]
    InvalidLength,
    #[error("skill ID violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SkillId(Box<str>);

impl SkillId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SkillId {
    type Err = SkillIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 128, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => SkillIdError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => SkillIdError::InvalidGrammar,
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}
