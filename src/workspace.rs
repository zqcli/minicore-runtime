use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::wire::lexical::{LexicalError, validate_stable_symbolic_key};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkspaceRootKeyError {
    #[error("workspace root key must be 1..=64 bytes")]
    InvalidLength,
    #[error("workspace root key violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRootKey(Box<str>);

impl WorkspaceRootKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for WorkspaceRootKey {
    type Err = WorkspaceRootKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 64, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => WorkspaceRootKeyError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                WorkspaceRootKeyError::InvalidGrammar
            }
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for WorkspaceRootKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for WorkspaceRootKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}
