use super::{CanonicalJsonNumber, JsonNumberError};

#[test]
fn exact_decimal_numbers_have_one_canonical_spelling() {
    for (input, expected) in [
        ("1", "1"),
        ("1.0", "1"),
        ("1e0", "1"),
        ("-0", "0"),
        ("-0.000e999", "0"),
        ("1.2300", "1.23"),
        ("0.000001", "0.000001"),
        ("0.0000001", "1e-7"),
        ("1e20", "100000000000000000000"),
        ("1e21", "1e21"),
        ("-123.4500e+2", "-12345"),
        ("12.34e-7", "0.000001234"),
    ] {
        assert_eq!(
            CanonicalJsonNumber::parse(input).unwrap().as_str(),
            expected,
            "{input}"
        );
    }
}

#[test]
fn number_literal_and_exponent_boundaries_are_exact() {
    let coefficient = "1".repeat(60);
    let raw_64 = format!("{coefficient}e000");
    assert_eq!(raw_64.len(), 64);
    let number = CanonicalJsonNumber::parse(&raw_64).unwrap();
    assert_eq!(number.as_str().len(), 64);
    assert!(!number.as_str().contains("e+"));

    assert_eq!(
        CanonicalJsonNumber::parse(&format!("{coefficient}e0000")),
        Err(JsonNumberError::RawLiteralTooLong)
    );
    assert_eq!(
        CanonicalJsonNumber::parse("1e1000000").unwrap().as_str(),
        "1e1000000"
    );
    assert_eq!(
        CanonicalJsonNumber::parse("1e-1000000").unwrap().as_str(),
        "1e-1000000"
    );
    assert_eq!(
        CanonicalJsonNumber::parse("1e1000001"),
        Err(JsonNumberError::ExponentOutOfRange)
    );
    assert_eq!(
        CanonicalJsonNumber::parse("1e-1000001"),
        Err(JsonNumberError::ExponentOutOfRange)
    );
    assert_eq!(
        CanonicalJsonNumber::parse("0e1000001"),
        Err(JsonNumberError::ExponentOutOfRange)
    );
}

#[test]
fn invalid_json_number_grammar_is_rejected_without_float_lowering() {
    for invalid in [
        "", "-", "+1", "01", "-01", ".1", "1.", "1e", "1e+", "1_0", "NaN", "Infinity", "--1",
        "1 2", "é", "1é", "1.é", "1eé",
    ] {
        assert_eq!(
            CanonicalJsonNumber::parse(invalid),
            Err(JsonNumberError::InvalidSyntax),
            "accepted {invalid:?}",
        );
    }

    let canonical_too_long = "1".repeat(61);
    assert_eq!(canonical_too_long.len(), 61);
    assert_eq!(
        CanonicalJsonNumber::parse(&canonical_too_long),
        Err(JsonNumberError::CanonicalLiteralTooLong)
    );
}

#[test]
fn short_unicode_and_grammar_corpus_is_panic_free() {
    let alphabet = ['0', '1', '-', '+', '.', 'e', 'E', 'é'];
    for width in 0..=5_u32 {
        let cases = alphabet.len().pow(width);
        for mut encoded in 0..cases {
            let mut candidate = String::new();
            for _ in 0..width {
                candidate.push(alphabet[encoded % alphabet.len()]);
                encoded /= alphabet.len();
            }
            let _ = CanonicalJsonNumber::parse(&candidate);
        }
    }
}
