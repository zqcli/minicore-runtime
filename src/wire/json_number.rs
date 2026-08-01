use std::fmt;

use thiserror::Error;

use super::limits::ProtocolLimits;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum JsonNumberError {
    #[error("invalid JSON number syntax")]
    InvalidSyntax,
    #[error("JSON number input literal exceeds the wire limit")]
    RawLiteralTooLong,
    #[error("JSON number exponent exceeds the wire limit")]
    ExponentOutOfRange,
    #[error("canonical JSON number literal exceeds the wire limit")]
    CanonicalLiteralTooLong,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct CanonicalJsonNumber(Box<str>);

impl CanonicalJsonNumber {
    pub fn parse(literal: &str) -> Result<Self, JsonNumberError> {
        let limit = ProtocolLimits::v1_0()
            .embedded_json
            .value
            .max_number_literal_bytes as usize;
        if literal.len() > limit {
            return Err(JsonNumberError::RawLiteralTooLong);
        }

        let parsed = ParsedNumber::parse(literal)?;
        let canonical = parsed.canonicalize()?;
        if canonical.len() > limit {
            return Err(JsonNumberError::CanonicalLiteralTooLong);
        }
        Ok(Self(canonical.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn validate_json_number_syntax(literal: &str) -> Result<(), JsonNumberError> {
    ParsedNumberSyntax::parse(literal).map(|_| ())
}

impl fmt::Debug for CanonicalJsonNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

struct ParsedNumber<'a> {
    negative: bool,
    integer: &'a str,
    fraction: &'a str,
    exponent: i32,
}

struct ParsedNumberSyntax<'a> {
    negative: bool,
    integer: &'a str,
    fraction: &'a str,
    exponent_negative: bool,
    exponent_digits: &'a str,
}

impl<'a> ParsedNumberSyntax<'a> {
    fn parse(literal: &'a str) -> Result<Self, JsonNumberError> {
        let bytes = literal.as_bytes();
        let mut position = 0;
        let negative = consume(bytes, &mut position, b'-');

        let integer_start = position;
        match bytes.get(position).copied() {
            Some(b'0') => position += 1,
            Some(b'1'..=b'9') => {
                position += 1;
                while matches!(bytes.get(position), Some(b'0'..=b'9')) {
                    position += 1;
                }
            }
            _ => return Err(JsonNumberError::InvalidSyntax),
        }
        let integer_end = position;

        let mut fraction = "";
        if consume(bytes, &mut position, b'.') {
            let fraction_start = position;
            while matches!(bytes.get(position), Some(b'0'..=b'9')) {
                position += 1;
            }
            if fraction_start == position {
                return Err(JsonNumberError::InvalidSyntax);
            }
            fraction = &literal[fraction_start..position];
        }

        let mut exponent_negative = false;
        let mut exponent_digits = "";
        if matches!(bytes.get(position), Some(b'e' | b'E')) {
            position += 1;
            exponent_negative = if consume(bytes, &mut position, b'+') {
                false
            } else {
                consume(bytes, &mut position, b'-')
            };
            let exponent_start = position;
            while matches!(bytes.get(position), Some(b'0'..=b'9')) {
                position += 1;
            }
            if exponent_start == position {
                return Err(JsonNumberError::InvalidSyntax);
            }
            exponent_digits = &literal[exponent_start..position];
        }

        if position != bytes.len() {
            return Err(JsonNumberError::InvalidSyntax);
        }
        Ok(Self {
            negative,
            integer: &literal[integer_start..integer_end],
            fraction,
            exponent_negative,
            exponent_digits,
        })
    }
}

impl<'a> ParsedNumber<'a> {
    fn parse(literal: &'a str) -> Result<Self, JsonNumberError> {
        let syntax = ParsedNumberSyntax::parse(literal)?;
        let mut exponent = 0_i32;
        if !syntax.exponent_digits.is_empty() {
            for byte in syntax.exponent_digits.bytes() {
                let digit = i32::from(byte - b'0');
                exponent = exponent
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(digit))
                    .ok_or(JsonNumberError::ExponentOutOfRange)?;
                if exponent > 1_000_000 {
                    return Err(JsonNumberError::ExponentOutOfRange);
                }
            }
            if syntax.exponent_negative {
                exponent = -exponent;
            }
        }
        Ok(Self {
            negative: syntax.negative,
            integer: syntax.integer,
            fraction: syntax.fraction,
            exponent,
        })
    }

    fn canonicalize(self) -> Result<String, JsonNumberError> {
        let mut coefficient = String::with_capacity(self.integer.len() + self.fraction.len());
        coefficient.push_str(self.integer);
        coefficient.push_str(self.fraction);

        let Some(first_nonzero) = coefficient.bytes().position(|byte| byte != b'0') else {
            return Ok("0".to_owned());
        };
        coefficient.drain(..first_nonzero);

        let fraction_len = i32::try_from(self.fraction.len())
            .map_err(|_| JsonNumberError::CanonicalLiteralTooLong)?;
        let mut decimal_exponent = self.exponent - fraction_len;
        while coefficient.ends_with('0') {
            coefficient.pop();
            decimal_exponent += 1;
        }

        let coefficient_len = i32::try_from(coefficient.len())
            .map_err(|_| JsonNumberError::CanonicalLiteralTooLong)?;
        let adjusted_exponent = decimal_exponent + coefficient_len - 1;
        let mut canonical = String::new();
        if self.negative {
            canonical.push('-');
        }

        if (-6..21).contains(&adjusted_exponent) {
            let point = coefficient_len + decimal_exponent;
            if point <= 0 {
                canonical.push_str("0.");
                for _ in 0..-point {
                    canonical.push('0');
                }
                canonical.push_str(&coefficient);
            } else if point >= coefficient_len {
                canonical.push_str(&coefficient);
                for _ in 0..(point - coefficient_len) {
                    canonical.push('0');
                }
            } else {
                let split =
                    usize::try_from(point).map_err(|_| JsonNumberError::CanonicalLiteralTooLong)?;
                canonical.push_str(&coefficient[..split]);
                canonical.push('.');
                canonical.push_str(&coefficient[split..]);
            }
        } else {
            canonical.push(char::from(coefficient.as_bytes()[0]));
            if coefficient.len() > 1 {
                canonical.push('.');
                canonical.push_str(&coefficient[1..]);
            }
            canonical.push('e');
            canonical.push_str(&adjusted_exponent.to_string());
        }
        Ok(canonical)
    }
}

fn consume(input: &[u8], position: &mut usize, expected: u8) -> bool {
    if input.get(*position) == Some(&expected) {
        *position += 1;
        true
    } else {
        false
    }
}

#[cfg(test)]
#[path = "json_number_tests.rs"]
mod tests;
