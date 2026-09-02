use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::value::BoundedText;

use super::{ConfigError, SemanticLimits, SessionSpec, Timestamp};

/// Checked v3 manifest for a Session.
///
/// Constructors and deserialization enforce structural bounds on the manifest and
/// its inner `SessionSpec`. Runtime instance limits are enforced when opening
/// a session via `SessionRuntime::create` or `SessionRuntime::load`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionManifest {
    pub format_version: u32,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub spec: SessionSpec,
}

impl SessionManifest {
    pub const FORMAT_VERSION: u32 = 3;

    /// Creates a new `SessionManifest` after validating structural bounds.
    pub fn new(session_id: SessionId, spec: SessionSpec) -> Result<Self, ConfigError> {
        spec.validate_structural()?;
        let created_at = Timestamp::now_utc().map_err(|_| ConfigError::Timestamp)?;
        Ok(Self {
            format_version: Self::FORMAT_VERSION,
            session_id,
            created_at,
            spec,
        })
    }

    /// Validates manifest format version and inner specification structural bounds.
    pub fn validate_structural(&self) -> Result<(), ConfigError> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(ConfigError::InvalidBounds);
        }
        self.spec.validate_structural()
    }

    /// Validates this manifest against the provided runtime instance limits.
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
            .validate_structural()
            .map_err(serde::de::Error::custom)?;
        Ok(manifest)
    }
}

/// User input for a Turn.
///
/// Constructors enforce absolute structural bounds (`BoundedText::MAX_BYTES` and non-empty text).
/// Runtime instance limits are enforced when submitting a turn via `SessionHandle::submit`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserInput {
    Text(BoundedText),
}

impl UserInput {
    /// Creates text user input within absolute structural bounds.
    pub fn text(value: impl AsRef<str>) -> Result<Self, ConfigError> {
        let text = BoundedText::new(value).map_err(|_| ConfigError::InvalidText)?;
        if text.is_empty() {
            return Err(ConfigError::InvalidText);
        }
        Ok(Self::Text(text))
    }

    /// Validates user input against the provided runtime instance limits.
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

    /// Returns the input text as a string slice.
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
