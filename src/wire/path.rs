use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::limits::ProtocolLimits;

const MAX_FILE_URI_BYTES: usize =
    ProtocolLimits::v1_0().workspace.max_absolute_path_uri_bytes as usize;
const MAX_RELATIVE_PATH_BYTES: usize =
    ProtocolLimits::v1_0().workspace.max_relative_path_bytes as usize;
const MAX_RELATIVE_PATH_SEGMENTS: usize =
    ProtocolLimits::v1_0().workspace.max_relative_path_segments as usize;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FileUriFamily {
    Posix,
    Drive,
    Unc,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PathWireError {
    #[error("invalid file URI scheme or shape")]
    InvalidFileUri,
    #[error("file URI exceeds the wire limit")]
    FileUriTooLong,
    #[error("invalid file URI authority")]
    InvalidAuthority,
    #[error("invalid or noncanonical percent encoding")]
    InvalidPercentEncoding,
    #[error("file URI path is not canonical")]
    InvalidPath,
    #[error("workspace relative path is not canonical")]
    InvalidRelativePath,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct CanonicalFileUri {
    wire: Box<str>,
    family: FileUriFamily,
    authority: Option<Box<str>>,
    decoded_path: Box<str>,
}

impl CanonicalFileUri {
    pub const fn family(&self) -> FileUriFamily {
        self.family
    }

    pub fn authority(&self) -> Option<&str> {
        self.authority.as_deref()
    }

    pub fn decoded_path(&self) -> &str {
        &self.decoded_path
    }

    /// Returns the exact canonical wire value; unlike `Debug` and `Display`, this may expose an
    /// absolute path and should only be used at an explicit serialization or path-handling seam.
    pub fn as_str(&self) -> &str {
        &self.wire
    }

    /// Constructs the canonical RFC 8089 wire value from already decoded,
    /// family-tagged path parts. Hosts should use this typed constructor instead
    /// of interpolating native paths into URI strings: it validates the selected
    /// family and authority, applies canonical percent encoding, and then
    /// re-parses the exact wire form through the same public grammar.
    pub fn from_decoded_parts(
        family: FileUriFamily,
        authority: Option<&str>,
        decoded_path: &str,
    ) -> Result<Self, PathWireError> {
        if decoded_path.contains(['\\', '\0']) {
            return Err(PathWireError::InvalidPath);
        }

        match family {
            FileUriFamily::Posix => {
                if authority.is_some() {
                    return Err(PathWireError::InvalidFileUri);
                }
                validate_posix_path(decoded_path)?;
            }
            FileUriFamily::Drive => {
                if authority.is_some() {
                    return Err(PathWireError::InvalidFileUri);
                }
                validate_drive_path(decoded_path)?;
            }
            FileUriFamily::Unc => {
                let authority = authority.ok_or(PathWireError::InvalidFileUri)?;
                validate_unc_authority(authority)?;
                validate_unc_path(decoded_path)?;
            }
        }

        let raw_path = encode_decoded_path(family, decoded_path);
        let mut wire =
            String::with_capacity("file://".len() + authority.map_or(0, str::len) + raw_path.len());
        wire.push_str("file://");
        if let Some(authority) = authority {
            wire.push_str(authority);
        }
        if matches!(family, FileUriFamily::Drive | FileUriFamily::Unc) {
            wire.push('/');
        }
        wire.push_str(&raw_path);
        wire.parse()
    }
}

#[allow(
    dead_code,
    reason = "Workspace durable store encoding will use this crate-private wire seam"
)]
fn encode_decoded_path(family: FileUriFamily, path: &str) -> String {
    let bytes = path.as_bytes();
    let escape_posix_drive_colon = family == FileUriFamily::Posix
        && bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':';
    let mut encoded = String::with_capacity(path.len());
    for (index, byte) in path.bytes().enumerate() {
        if byte == b'/' || (is_pchar(byte) && !(escape_posix_drive_colon && index == 2)) {
            encoded.push(char::from(byte));
        } else {
            use fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

impl FromStr for CanonicalFileUri {
    type Err = PathWireError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > MAX_FILE_URI_BYTES {
            return Err(PathWireError::FileUriTooLong);
        }
        let rest = value
            .strip_prefix("file://")
            .ok_or(PathWireError::InvalidFileUri)?;
        if value.contains('?') || value.contains('#') {
            return Err(PathWireError::InvalidFileUri);
        }

        let (authority, raw_path) = if rest.starts_with('/') {
            (None, rest)
        } else {
            let slash = rest.find('/').ok_or(PathWireError::InvalidFileUri)?;
            let authority = &rest[..slash];
            validate_unc_authority(authority)?;
            (Some(authority), &rest[slash..])
        };
        if !raw_path.starts_with('/') || raw_path.starts_with("//") {
            return Err(PathWireError::InvalidPath);
        }

        let literal_drive_prefix = has_literal_drive_prefix(raw_path);
        let posix_drive_disambiguator = has_posix_drive_disambiguator(raw_path);
        let family = if authority.is_some() {
            FileUriFamily::Unc
        } else if literal_drive_prefix {
            if !raw_path.as_bytes()[1].is_ascii_uppercase() {
                return Err(PathWireError::InvalidPath);
            }
            FileUriFamily::Drive
        } else {
            if has_ambiguous_literal_colon_prefix(raw_path) {
                return Err(PathWireError::InvalidPath);
            }
            FileUriFamily::Posix
        };

        let allowed_colon_escape =
            (family == FileUriFamily::Posix && posix_drive_disambiguator).then_some(2);
        let decoded = decode_canonical_path(raw_path, allowed_colon_escape)?;
        let decoded_path = match family {
            FileUriFamily::Posix => {
                validate_posix_path(&decoded)?;
                decoded
            }
            FileUriFamily::Drive => {
                let drive_path = decoded
                    .strip_prefix('/')
                    .ok_or(PathWireError::InvalidPath)?;
                validate_drive_path(drive_path)?;
                drive_path.to_owned()
            }
            FileUriFamily::Unc => {
                let unc_path = decoded
                    .strip_prefix('/')
                    .ok_or(PathWireError::InvalidPath)?;
                validate_unc_path(unc_path)?;
                unc_path.to_owned()
            }
        };

        Ok(Self {
            wire: value.into(),
            family,
            authority: authority.map(Into::into),
            decoded_path: decoded_path.into(),
        })
    }
}

impl fmt::Display for CanonicalFileUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<canonical-file-uri:{:?}>", self.family)
    }
}

impl fmt::Debug for CanonicalFileUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for CanonicalFileUri {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.wire)
    }
}

impl<'de> Deserialize<'de> for CanonicalFileUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

fn validate_unc_authority(authority: &str) -> Result<(), PathWireError> {
    if authority.is_empty()
        || authority.len() > 253
        || authority == "localhost"
        || !authority.is_ascii()
        || authority.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        return Err(PathWireError::InvalidAuthority);
    }

    for label in authority.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(PathWireError::InvalidAuthority);
        }
        let bytes = label.as_bytes();
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
            || !bytes[bytes.len() - 1].is_ascii_lowercase()
                && !bytes[bytes.len() - 1].is_ascii_digit()
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return Err(PathWireError::InvalidAuthority);
        }
    }
    Ok(())
}

fn has_literal_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 4
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && bytes[3] == b'/'
}

fn has_ambiguous_literal_colon_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':'
}

fn has_posix_drive_disambiguator(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 5 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && &bytes[2..5] == b"%3A"
}

fn decode_canonical_path(
    raw_path: &str,
    allowed_colon_escape: Option<usize>,
) -> Result<String, PathWireError> {
    let raw = raw_path.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'%' => {
                if index + 2 >= raw.len() {
                    return Err(PathWireError::InvalidPercentEncoding);
                }
                let high = decode_upper_hex(raw[index + 1])
                    .ok_or(PathWireError::InvalidPercentEncoding)?;
                let low = decode_upper_hex(raw[index + 2])
                    .ok_or(PathWireError::InvalidPercentEncoding)?;
                let byte = (high << 4) | low;
                if byte == 0 || byte == b'/' || byte == b'\\' {
                    return Err(PathWireError::InvalidPercentEncoding);
                }
                if byte.is_ascii()
                    && is_pchar(byte)
                    && (byte != b':' || Some(index) != allowed_colon_escape)
                {
                    return Err(PathWireError::InvalidPercentEncoding);
                }
                decoded.push(byte);
                index += 3;
            }
            b'/' => {
                decoded.push(b'/');
                index += 1;
            }
            byte if byte.is_ascii() && is_pchar(byte) => {
                decoded.push(byte);
                index += 1;
            }
            _ => return Err(PathWireError::InvalidPath),
        }
    }

    String::from_utf8(decoded).map_err(|_| PathWireError::InvalidPercentEncoding)
}

fn decode_upper_hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn is_pchar(value: u8) -> bool {
    value.is_ascii_alphanumeric()
        || matches!(
            value,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
}

fn validate_posix_path(path: &str) -> Result<(), PathWireError> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(PathWireError::InvalidPath);
    }
    if path == "/" {
        return Ok(());
    }
    if path.ends_with('/') {
        return Err(PathWireError::InvalidPath);
    }
    validate_segments(&path[1..])
}

fn validate_drive_path(path: &str) -> Result<(), PathWireError> {
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_uppercase() || bytes[1] != b':' || bytes[2] != b'/' {
        return Err(PathWireError::InvalidPath);
    }
    if path.len() == 3 {
        return Ok(());
    }
    if path.ends_with('/') {
        return Err(PathWireError::InvalidPath);
    }
    validate_segments(&path[3..])
}

fn validate_unc_path(path: &str) -> Result<(), PathWireError> {
    if path.is_empty() || path.ends_with('/') {
        return Err(PathWireError::InvalidPath);
    }
    validate_segments(path)
}

fn validate_segments(path: &str) -> Result<(), PathWireError> {
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(PathWireError::InvalidPath);
    }
    Ok(())
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceRelativePath(Box<str>);

impl Default for WorkspaceRelativePath {
    fn default() -> Self {
        Self("".into())
    }
}

impl WorkspaceRelativePath {
    /// Returns the exact relative path; unlike `Debug` and `Display`, this may expose
    /// client-supplied path text and should only be used at an explicit path-handling seam.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromStr for WorkspaceRelativePath {
    type Err = PathWireError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > MAX_RELATIVE_PATH_BYTES
            || value.contains(['\\', '\0'])
            || value.chars().any(char::is_control)
            || value.starts_with('/')
            || value.ends_with('/')
        {
            return Err(PathWireError::InvalidRelativePath);
        }
        if value.is_empty() {
            return Ok(Self("".into()));
        }

        let mut segments = 0;
        for (index, segment) in value.split('/').enumerate() {
            segments += 1;
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(PathWireError::InvalidRelativePath);
            }
            if index == 0 {
                let bytes = segment.as_bytes();
                if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                    return Err(PathWireError::InvalidRelativePath);
                }
            }
        }
        if segments > MAX_RELATIVE_PATH_SEGMENTS {
            return Err(PathWireError::InvalidRelativePath);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for WorkspaceRelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<workspace-relative-path>")
    }
}

impl fmt::Debug for WorkspaceRelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for WorkspaceRelativePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WorkspaceRelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_from_str(deserializer)
    }
}

struct FromStrVisitor<T>(PhantomData<T>);

impl<'de, T> Visitor<'de> for FromStrVisitor<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical wire path string")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        value.parse().map_err(E::custom)
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(&value)
    }
}

fn deserialize_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    deserializer.deserialize_str(FromStrVisitor(PhantomData))
}

#[cfg(test)]
mod tests {
    use super::{CanonicalFileUri, FileUriFamily, PathWireError};

    #[test]
    fn decoded_parts_emit_a_canonical_posix_uri() {
        let uri = CanonicalFileUri::from_decoded_parts(FileUriFamily::Posix, None, "/work/project")
            .unwrap();

        assert_eq!(uri.as_str(), "file:///work/project");
    }

    #[test]
    fn decoded_parts_emit_exact_canonical_file_uri_literals() {
        for (family, authority, decoded_path, expected) in [
            (
                FileUriFamily::Posix,
                None,
                "/work/a b",
                "file:///work/a%20b",
            ),
            (
                FileUriFamily::Posix,
                None,
                "/项目/资料",
                "file:///%E9%A1%B9%E7%9B%AE/%E8%B5%84%E6%96%99",
            ),
            (
                FileUriFamily::Posix,
                None,
                "/work/100%",
                "file:///work/100%25",
            ),
            (FileUriFamily::Posix, None, "/C:/repo", "file:///C%3A/repo"),
            (
                FileUriFamily::Drive,
                None,
                "C:/work/project",
                "file:///C:/work/project",
            ),
            (
                FileUriFamily::Unc,
                Some("server"),
                "share/project",
                "file://server/share/project",
            ),
        ] {
            let uri = CanonicalFileUri::from_decoded_parts(family, authority, decoded_path)
                .unwrap_or_else(|error| panic!("rejected {decoded_path:?}: {error}"));
            assert_eq!(uri.as_str(), expected);
        }
    }

    #[test]
    fn decoded_parts_reject_invalid_family_shapes_and_paths() {
        for (family, authority, decoded_path, expected) in [
            (
                FileUriFamily::Posix,
                Some("server"),
                "/work",
                PathWireError::InvalidFileUri,
            ),
            (
                FileUriFamily::Drive,
                Some("server"),
                "C:/work",
                PathWireError::InvalidFileUri,
            ),
            (
                FileUriFamily::Unc,
                None,
                "share",
                PathWireError::InvalidFileUri,
            ),
            (
                FileUriFamily::Unc,
                Some("Server"),
                "share",
                PathWireError::InvalidAuthority,
            ),
            (
                FileUriFamily::Posix,
                None,
                "/work\\project",
                PathWireError::InvalidPath,
            ),
            (
                FileUriFamily::Posix,
                None,
                "/work\0project",
                PathWireError::InvalidPath,
            ),
            (
                FileUriFamily::Posix,
                None,
                "/work//project",
                PathWireError::InvalidPath,
            ),
            (
                FileUriFamily::Posix,
                None,
                "/work/.",
                PathWireError::InvalidPath,
            ),
            (
                FileUriFamily::Drive,
                None,
                "C:/work/",
                PathWireError::InvalidPath,
            ),
            (
                FileUriFamily::Unc,
                Some("server"),
                "share/",
                PathWireError::InvalidPath,
            ),
        ] {
            assert_eq!(
                CanonicalFileUri::from_decoded_parts(family, authority, decoded_path),
                Err(expected),
            );
        }
    }
}
