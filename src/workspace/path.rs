use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum RelativePathError {
    #[error("relative path is too long")]
    TooLong,
    #[error("relative path has too many segments")]
    TooManySegments,
    #[error("relative path contains a control character")]
    ControlCharacter,
    #[error("relative path contains a backslash")]
    Backslash,
    #[error("relative path has a leading slash")]
    LeadingSlash,
    #[error("relative path has a trailing slash")]
    TrailingSlash,
    #[error("relative path contains an empty segment")]
    EmptySegment,
    #[error("relative path contains a dot segment")]
    DotSegment,
    #[error("relative path contains a platform prefix")]
    PlatformPrefix,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RelativePath(Box<str>);

impl RelativePath {
    pub const MAX_BYTES: usize = 4_096;
    pub const MAX_SEGMENTS: usize = 256;

    pub fn new(value: impl AsRef<str>) -> Result<Self, RelativePathError> {
        let value = value.as_ref();
        validate(value)?;
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn as_path(&self) -> &Path {
        Path::new(self.as_str())
    }
}

impl Default for RelativePath {
    fn default() -> Self {
        Self(String::new().into_boxed_str())
    }
}

impl FromStr for RelativePath {
    type Err = RelativePathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<&str> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl AsRef<str> for RelativePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<Path> for RelativePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RelativePath")
            .field(&self.as_str())
            .finish()
    }
}

impl Serialize for RelativePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

fn validate(value: &str) -> Result<(), RelativePathError> {
    if value.len() > RelativePath::MAX_BYTES {
        return Err(RelativePathError::TooLong);
    }
    if value.is_empty() {
        return Ok(());
    }
    if value.starts_with('/') {
        return Err(RelativePathError::LeadingSlash);
    }
    if value.ends_with('/') {
        return Err(RelativePathError::TrailingSlash);
    }
    if value.contains('\\') {
        return Err(RelativePathError::Backslash);
    }
    if value.chars().any(char::is_control) {
        return Err(RelativePathError::ControlCharacter);
    }
    if has_platform_prefix(value) {
        return Err(RelativePathError::PlatformPrefix);
    }

    let mut segment_count = 0;
    for segment in value.split('/') {
        if segment.is_empty() {
            return Err(RelativePathError::EmptySegment);
        }
        if matches!(segment, "." | "..") {
            return Err(RelativePathError::DotSegment);
        }
        segment_count += 1;
        if segment_count > RelativePath::MAX_SEGMENTS {
            return Err(RelativePathError::TooManySegments);
        }
    }
    Ok(())
}

fn has_platform_prefix(value: &str) -> bool {
    let first_segment = value.split('/').next().unwrap_or_default();
    let bytes = first_segment.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

// P7 deletion target: remove with the private legacy Workspace implementation.
const _: fn(String) -> Result<RelativePath, RelativePathError> = RelativePath::new;
