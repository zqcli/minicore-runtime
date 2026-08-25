use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;
use std::time::{Duration, Instant};

use ::time::OffsetDateTime;
use ::time::format_description::well_known::Rfc3339;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use tokio::time::Instant as TokioInstant;

// Keep this foundation type-checked before the SessionActor slice consumes it.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeadlineSource {
    Turn,
    Port,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveDeadline {
    standard: Instant,
    tokio: TokioInstant,
    source: DeadlineSource,
}

impl EffectiveDeadline {
    pub(crate) const fn standard(self) -> Instant {
        self.standard
    }

    pub(crate) const fn tokio(self) -> TokioInstant {
        self.tokio
    }

    pub(crate) const fn source(self) -> DeadlineSource {
        self.source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeadlineOverflow;

pub(crate) fn effective_deadline(
    turn_deadline: Instant,
    port_timeout: Duration,
) -> Result<EffectiveDeadline, DeadlineOverflow> {
    effective_deadline_from(TokioInstant::now(), turn_deadline, port_timeout)
}

fn effective_deadline_from(
    now: TokioInstant,
    turn_deadline: Instant,
    port_timeout: Duration,
) -> Result<EffectiveDeadline, DeadlineOverflow> {
    let port_deadline = now.checked_add(port_timeout).ok_or(DeadlineOverflow)?;
    let turn_deadline = TokioInstant::from_std(turn_deadline);
    let (tokio, source) = if turn_deadline <= port_deadline {
        (turn_deadline, DeadlineSource::Turn)
    } else {
        (port_deadline, DeadlineSource::Port)
    };
    Ok(EffectiveDeadline {
        standard: tokio.into_std(),
        tokio,
        source,
    })
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TimestampError {
    #[error("timestamp must be canonical UTC RFC3339 milliseconds")]
    Invalid,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(Box<str>);

impl Timestamp {
    pub fn now_utc() -> Result<Self, TimestampError> {
        let now = OffsetDateTime::now_utc();
        let mut value = String::with_capacity(24);
        write!(
            &mut value,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
            now.nanosecond() / 1_000_000,
        )
        .map_err(|_| TimestampError::Invalid)?;
        Self::new(&value)
    }

    pub fn new(value: &str) -> Result<Self, TimestampError> {
        value.parse()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Timestamp {
    type Err = TimestampError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_shape(value)?;
        OffsetDateTime::parse(value, &Rfc3339).map_err(|_| TimestampError::Invalid)?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(&value).map_err(serde::de::Error::custom)
    }
}

fn validate_shape(value: &str) -> Result<(), TimestampError> {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return Err(TimestampError::Invalid);
    }
    for range in [0..4, 5..7, 8..10, 11..13, 14..16, 17..19, 20..23] {
        if bytes[range].iter().any(|byte| !byte.is_ascii_digit()) {
            return Err(TimestampError::Invalid);
        }
    }
    if &bytes[..4] == b"0000" {
        return Err(TimestampError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn effective_deadline_selects_an_earlier_turn() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        let turn = standard_now + Duration::from_secs(2);
        let selected = effective_deadline_from(now, turn, Duration::from_secs(5)).unwrap();
        assert_eq!(selected.standard(), turn);
        assert_eq!(selected.tokio(), TokioInstant::from_std(turn));
        assert_eq!(selected.source(), DeadlineSource::Turn);
    }

    #[test]
    fn effective_deadline_selects_an_earlier_port_timeout() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        let selected = effective_deadline_from(
            now,
            standard_now + Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(selected.standard(), standard_now + Duration::from_secs(2));
        assert_eq!(selected.source(), DeadlineSource::Port);
    }

    #[test]
    fn equal_deadlines_choose_turn_conservatively() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        let turn = standard_now + Duration::from_secs(3);
        let selected = effective_deadline_from(now, turn, Duration::from_secs(3)).unwrap();
        assert_eq!(selected.standard(), turn);
        assert_eq!(selected.source(), DeadlineSource::Turn);
    }

    #[test]
    fn deadline_overflow_is_checked() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        assert_eq!(
            effective_deadline_from(now, standard_now, Duration::MAX),
            Err(DeadlineOverflow)
        );
    }

    #[test]
    fn timestamp_is_canonical_utc_millisecond_rfc3339() {
        let timestamp: Timestamp = "2026-08-19T12:34:56.789Z".parse().unwrap();
        assert_eq!(timestamp.to_string(), "2026-08-19T12:34:56.789Z");
        assert_eq!(
            serde_json::to_value(&timestamp).unwrap(),
            json!("2026-08-19T12:34:56.789Z")
        );
        assert_eq!(
            serde_json::from_value::<Timestamp>(json!(timestamp.to_string())).unwrap(),
            timestamp
        );
        for invalid in [
            "2026-08-19T12:34:56Z",
            "2026-08-19T12:34:56.78Z",
            "2026-08-19T12:34:56.789+00:00",
            "2026-08-19T12:34:56.789z",
            "2026-02-30T12:34:56.789Z",
            "0000-01-01T00:00:00.000Z",
            "2026-08-19T12:34:56.1234Z",
        ] {
            assert!(invalid.parse::<Timestamp>().is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn now_utc_uses_the_canonical_millisecond_shape() {
        let timestamp = Timestamp::now_utc().unwrap();
        let value = timestamp.as_str();
        assert_eq!(value.len(), 24);
        assert_eq!(&value[4..5], "-");
        assert_eq!(&value[7..8], "-");
        assert_eq!(&value[10..11], "T");
        assert_eq!(&value[13..14], ":");
        assert_eq!(&value[16..17], ":");
        assert_eq!(&value[19..20], ".");
        assert_eq!(&value[23..24], "Z");
        assert!(
            value[..4]
                .chars()
                .all(|character| character.is_ascii_digit())
        );
        assert!(
            value[20..23]
                .chars()
                .all(|character| character.is_ascii_digit())
        );
        assert_eq!(value.parse::<Timestamp>().unwrap(), timestamp);
    }
}
