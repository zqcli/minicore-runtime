use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const MAX_DURATION_MILLISECONDS: u32 = 86_400_000;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WireValueError {
    #[error("invalid timestamp")]
    InvalidTimestamp,
    #[error("duration is outside the public wire limit")]
    DurationOutOfRange,
    #[error("invalid money amount")]
    InvalidMoneyAmount,
    #[error("invalid currency code")]
    InvalidCurrencyCode,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    pub fn from_utc(value: OffsetDateTime) -> Result<Self, WireValueError> {
        if value.offset() != time::UtcOffset::UTC
            || value.year() == 0
            || value.nanosecond() % 1_000_000 != 0
        {
            return Err(WireValueError::InvalidTimestamp);
        }
        Ok(Self(value))
    }

    pub const fn as_datetime(self) -> OffsetDateTime {
        self.0
    }
}

impl FromStr for Timestamp {
    type Err = WireValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_timestamp_shape(value)?;
        let parsed =
            OffsetDateTime::parse(value, &Rfc3339).map_err(|_| WireValueError::InvalidTimestamp)?;
        Self::from_utc(parsed)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let date = self.0.date();
        let time = self.0.time();
        write!(
            formatter,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            date.year(),
            u8::from(date.month()),
            date.day(),
            time.hour(),
            time.minute(),
            time.second(),
            time.millisecond(),
        )
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
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

fn validate_timestamp_shape(value: &str) -> Result<(), WireValueError> {
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
        return Err(WireValueError::InvalidTimestamp);
    }
    const DIGIT_RANGES: [std::ops::Range<usize>; 7] =
        [0..4, 5..7, 8..10, 11..13, 14..16, 17..19, 20..23];
    if DIGIT_RANGES
        .iter()
        .flat_map(|range| range.clone())
        .any(|index| !bytes[index].is_ascii_digit())
        || &bytes[0..4] == b"0000"
    {
        return Err(WireValueError::InvalidTimestamp);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Duration(u32);

impl Duration {
    pub fn new(milliseconds: u32) -> Result<Self, WireValueError> {
        if milliseconds > MAX_DURATION_MILLISECONDS {
            return Err(WireValueError::DurationOutOfRange);
        }
        Ok(Self(milliseconds))
    }

    pub const fn milliseconds(self) -> u32 {
        self.0
    }
}

impl Serialize for Duration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MoneyAmount {
    coefficient: u128,
    scale: u8,
}

impl MoneyAmount {
    pub const fn coefficient(self) -> u128 {
        self.coefficient
    }

    pub const fn scale(self) -> u8 {
        self.scale
    }
}

impl FromStr for MoneyAmount {
    type Err = WireValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (integer, fraction) = match value.split_once('.') {
            Some((integer, fraction)) if !fraction.contains('.') => (integer, Some(fraction)),
            Some(_) => return Err(WireValueError::InvalidMoneyAmount),
            None => (value, None),
        };

        if integer.is_empty()
            || integer.len() > 18
            || !integer.bytes().all(|byte| byte.is_ascii_digit())
            || (integer.len() > 1 && integer.starts_with('0'))
        {
            return Err(WireValueError::InvalidMoneyAmount);
        }

        let scale = if let Some(fraction) = fraction {
            if fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
                || fraction.ends_with('0')
            {
                return Err(WireValueError::InvalidMoneyAmount);
            }
            fraction.len() as u8
        } else {
            0
        };

        let coefficient_text = if let Some(fraction) = fraction {
            let mut text = String::with_capacity(integer.len() + fraction.len());
            text.push_str(integer);
            text.push_str(fraction);
            text
        } else {
            integer.to_owned()
        };
        let coefficient = coefficient_text
            .parse::<u128>()
            .map_err(|_| WireValueError::InvalidMoneyAmount)?;
        if coefficient == 0 && scale != 0 {
            return Err(WireValueError::InvalidMoneyAmount);
        }

        Ok(Self { coefficient, scale })
    }
}

impl fmt::Display for MoneyAmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return self.coefficient.fmt(formatter);
        }

        let digits = self.coefficient.to_string();
        let scale = usize::from(self.scale);
        if digits.len() <= scale {
            formatter.write_str("0.")?;
            for _ in 0..(scale - digits.len()) {
                formatter.write_str("0")?;
            }
            formatter.write_str(&digits)
        } else {
            let split = digits.len() - scale;
            write!(formatter, "{}.{}", &digits[..split], &digits[split..])
        }
    }
}

impl Serialize for MoneyAmount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for MoneyAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("currency code is validated ASCII")
    }
}

impl FromStr for CurrencyCode {
    type Err = WireValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 3] = value
            .as_bytes()
            .try_into()
            .map_err(|_| WireValueError::InvalidCurrencyCode)?;
        if !bytes.iter().all(|byte| byte.is_ascii_uppercase()) {
            return Err(WireValueError::InvalidCurrencyCode);
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for CurrencyCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for CurrencyCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Money {
    amount: MoneyAmount,
    currency: CurrencyCode,
}

impl Money {
    pub const fn new(amount: MoneyAmount, currency: CurrencyCode) -> Self {
        Self { amount, currency }
    }

    pub const fn amount(self) -> MoneyAmount {
        self.amount
    }

    pub const fn currency(self) -> CurrencyCode {
        self.currency
    }
}
