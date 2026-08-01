#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LexicalError {
    Empty,
    TooLong,
    InvalidGrammar,
    UnsafeText,
}

pub(crate) fn validate_stable_symbolic_key(
    value: &str,
    maximum_bytes: usize,
    allow_slash: bool,
) -> Result<(), LexicalError> {
    if value.is_empty() {
        return Err(LexicalError::Empty);
    }
    if value.len() > maximum_bytes {
        return Err(LexicalError::TooLong);
    }
    if value.bytes().any(|byte| {
        !(0x21..=0x7e).contains(&byte)
            || matches!(byte, b'"' | b'\\')
            || (!allow_slash && byte == b'/')
    }) {
        return Err(LexicalError::InvalidGrammar);
    }
    Ok(())
}

#[allow(dead_code, reason = "consumed by Tools and ModelGateway semantic IDs")]
pub(crate) fn validate_opaque_ascii(value: &str, maximum_bytes: usize) -> Result<(), LexicalError> {
    if value.is_empty() {
        return Err(LexicalError::Empty);
    }
    if value.len() > maximum_bytes {
        return Err(LexicalError::TooLong);
    }
    if value
        .bytes()
        .any(|byte| !(0x21..=0x7e).contains(&byte) || matches!(byte, b'"' | b'\\'))
    {
        return Err(LexicalError::InvalidGrammar);
    }
    Ok(())
}

pub(crate) fn validate_safe_text(
    value: &str,
    maximum_bytes: usize,
    allow_empty: bool,
) -> Result<(), LexicalError> {
    if value.is_empty() && !allow_empty {
        return Err(LexicalError::Empty);
    }
    if value.len() > maximum_bytes {
        return Err(LexicalError::TooLong);
    }
    if value.chars().any(|character| {
        matches!(
            u32::from(character),
            0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f
        )
    }) {
        return Err(LexicalError::UnsafeText);
    }
    Ok(())
}

pub(crate) fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}
