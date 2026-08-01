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

pub(crate) fn canonical_json_string_len(value: &str) -> Option<usize> {
    let mut length = 2_usize;
    for character in value.chars() {
        let encoded = match character {
            '"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        length = length.checked_add(encoded)?;
    }
    Some(length)
}
