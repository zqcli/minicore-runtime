use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::value::BoundedText;

use super::{ConfigError, SemanticLimits, SessionSpec, Timestamp};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionManifest {
    pub format_version: u32,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub spec: SessionSpec,
}

impl SessionManifest {
    pub const FORMAT_VERSION: u32 = 3;

    pub fn new(session_id: SessionId, spec: SessionSpec) -> Result<Self, ConfigError> {
        spec.validate(&SemanticLimits::default())?;
        let created_at = Timestamp::now_utc().map_err(|_| ConfigError::Timestamp)?;
        Ok(Self {
            format_version: Self::FORMAT_VERSION,
            session_id,
            created_at,
            spec,
        })
    }

    pub fn validate(&self, limits: &SemanticLimits) -> Result<(), ConfigError> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(ConfigError::InvalidBounds);
        }
        self.spec.validate(limits)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionManifestWire {
    format_version: u32,
    session_id: SessionId,
    created_at: Timestamp,
    spec: SessionSpec,
}

impl<'de> Deserialize<'de> for SessionManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SessionManifestWire::deserialize(deserializer)?;
        let manifest = Self {
            format_version: wire.format_version,
            session_id: wire.session_id,
            created_at: wire.created_at,
            spec: wire.spec,
        };
        manifest
            .validate(&SemanticLimits::default())
            .map_err(serde::de::Error::custom)?;
        Ok(manifest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserInput {
    Text(BoundedText),
}

impl UserInput {
    pub fn text(value: impl AsRef<str>) -> Result<Self, ConfigError> {
        let text =
            BoundedText::new_with_max_bytes(value, SemanticLimits::default().max_user_input_bytes)
                .map_err(|_| ConfigError::InvalidText)?;
        if text.is_empty() {
            return Err(ConfigError::InvalidText);
        }
        Ok(Self::Text(text))
    }

    pub fn validate(&self, limits: &SemanticLimits) -> Result<(), ConfigError> {
        limits.validate()?;
        match self {
            Self::Text(text)
                if text.is_empty() || text.byte_len() > limits.max_user_input_bytes =>
            {
                Err(ConfigError::InvalidText)
            }
            Self::Text(_) => Ok(()),
        }
    }

    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text.as_str(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TurnOptions {
    pub deadline: Option<Instant>,
    pub max_tool_rounds: Option<u16>,
}

impl TurnOptions {
    pub fn validate(&self, limits: &SemanticLimits) -> Result<(), ConfigError> {
        limits.validate()?;
        if self
            .max_tool_rounds
            .is_some_and(|rounds| !(1..=limits.max_tool_rounds).contains(&rounds))
        {
            return Err(ConfigError::InvalidBounds);
        }
        Ok(())
    }
}
