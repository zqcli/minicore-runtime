use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use minicore_runtime::wire::{
    BoundedJsonError, BoundedJsonObject, BoundedJsonValue, ProtocolLimits,
};

#[test]
fn bounded_json_has_one_canonical_semantic_encoding() {
    let value = BoundedJsonObject::from_slice(br#"{"z":1.0,"a":1e0,"m":-0}"#).unwrap();
    assert_eq!(value.canonical_json(), r#"{"a":1,"m":0,"z":1}"#);

    let equivalent = BoundedJsonObject::from_slice(br#"{"m":0,"z":1,"a":1}"#).unwrap();
    assert_eq!(value, equivalent);
    assert_eq!(hash(&value), hash(&equivalent));

    let unicode = BoundedJsonObject::from_slice("{\"é\":1,\"z\":2,\"a\":3}".as_bytes()).unwrap();
    assert_eq!(unicode.canonical_json(), "{\"a\":3,\"z\":2,\"é\":1}");
}

#[test]
fn bounded_json_decodes_strings_once_and_rejects_duplicate_keys() {
    let value = BoundedJsonValue::from_slice(r#""\"\\\/\b\t\n\f\r\u0001é""#.as_bytes()).unwrap();
    assert_eq!(value.canonical_json(), r#""\"\\/\b\t\n\f\r\u0001é""#);

    for duplicate in [
        br#"{"a":1,"a":2}"#.as_slice(),
        br#"{"a":1,"\u0061":2}"#.as_slice(),
        br#"{"outer":{"a":1,"a":2}}"#.as_slice(),
    ] {
        assert_eq!(
            BoundedJsonObject::from_slice(duplicate),
            Err(BoundedJsonError::DuplicateKey)
        );
    }
    assert_eq!(
        BoundedJsonObject::from_slice(b"[]"),
        Err(BoundedJsonError::RootObjectRequired)
    );
}

#[test]
fn bounded_json_debug_does_not_expose_embedded_payload() {
    let value = BoundedJsonObject::from_slice(br#"{"token":"secret-value"}"#).unwrap();
    let debug = format!("{value:?}");
    assert!(debug.contains("BoundedJsonObject"));
    assert!(!debug.contains("secret-value"));
}

#[test]
fn bounded_json_enforces_raw_and_canonical_byte_limits_independently() {
    let max = ProtocolLimits::v1_0().embedded_json.value.max_encoded_bytes as usize;
    let raw_boundary = format!("{{}}{}", " ".repeat(max - 2));
    assert_eq!(raw_boundary.len(), max);
    assert!(BoundedJsonObject::from_slice(raw_boundary.as_bytes()).is_ok());
    assert_eq!(
        BoundedJsonObject::from_slice(format!("{raw_boundary} ").as_bytes()),
        Err(BoundedJsonError::RawInputTooLarge)
    );

    let canonical_boundary = canonical_expansion_input(max);
    let parsed = BoundedJsonObject::from_slice(canonical_boundary.as_bytes()).unwrap();
    assert_eq!(parsed.canonical_bytes().len(), max);

    let canonical_oversized = canonical_expansion_input(max + 1);
    assert_eq!(
        BoundedJsonObject::from_slice(canonical_oversized.as_bytes()),
        Err(BoundedJsonError::CanonicalOutputTooLarge)
    );
}

#[test]
fn bounded_json_enforces_structural_and_leaf_boundaries() {
    let limits = ProtocolLimits::v1_0().embedded_json.value;
    assert!(BoundedJsonObject::from_slice(nested_array_object(30).as_bytes()).is_ok());
    assert_eq!(
        BoundedJsonObject::from_slice(nested_array_object(31).as_bytes()),
        Err(BoundedJsonError::DepthLimit)
    );

    assert!(
        BoundedJsonObject::from_slice(
            object_with_members(limits.max_object_members as usize).as_bytes()
        )
        .is_ok()
    );
    assert_eq!(
        BoundedJsonObject::from_slice(
            object_with_members(limits.max_object_members as usize + 1).as_bytes()
        ),
        Err(BoundedJsonError::ObjectMembersLimit)
    );

    assert!(
        BoundedJsonObject::from_slice(
            object_with_array_items(limits.max_array_items as usize).as_bytes()
        )
        .is_ok()
    );
    assert_eq!(
        BoundedJsonObject::from_slice(
            object_with_array_items(limits.max_array_items as usize + 1).as_bytes()
        ),
        Err(BoundedJsonError::ArrayItemsLimit)
    );

    let string_boundary = format!(
        r#"{{"v":"{}"}}"#,
        "x".repeat(limits.max_string_bytes as usize)
    );
    assert!(BoundedJsonObject::from_slice(string_boundary.as_bytes()).is_ok());
    let string_oversized = format!(
        r#"{{"v":"{}"}}"#,
        "x".repeat(limits.max_string_bytes as usize + 1)
    );
    assert_eq!(
        BoundedJsonObject::from_slice(string_oversized.as_bytes()),
        Err(BoundedJsonError::StringBytesLimit)
    );

    let coefficient = "1".repeat(60);
    let number_boundary = format!(r#"{{"v":{coefficient}e000}}"#);
    assert!(BoundedJsonObject::from_slice(number_boundary.as_bytes()).is_ok());
    let number_oversized = format!(r#"{{"v":{coefficient}e0000}}"#);
    assert_eq!(
        BoundedJsonObject::from_slice(number_oversized.as_bytes()),
        Err(BoundedJsonError::NumberLiteralLimit)
    );
}

#[test]
fn bounded_json_rejects_invalid_utf8_and_surrogates() {
    assert_eq!(
        BoundedJsonValue::from_slice(b"{\"v\":\"\xff\"}"),
        Err(BoundedJsonError::InvalidUtf8)
    );
    assert_eq!(
        BoundedJsonValue::from_slice(br#""\uD83D\uDE00""#)
            .unwrap()
            .canonical_json(),
        "\"😀\""
    );
    for invalid in [
        br#""\uD800""#.as_slice(),
        br#""\uDC00""#.as_slice(),
        br#""\uD800\u0041""#.as_slice(),
    ] {
        assert_eq!(
            BoundedJsonValue::from_slice(invalid),
            Err(BoundedJsonError::InvalidSyntax)
        );
    }
}

#[test]
fn bounded_json_rejects_malformed_grammar_at_the_owning_value() {
    for invalid in [
        br#""\x""#.as_slice(),
        br#""\u0""#.as_slice(),
        b"\"raw\x01control\"".as_slice(),
        b"[1,]".as_slice(),
        b"{\"a\":1,}".as_slice(),
        b"[truex]".as_slice(),
        b"[1:2]".as_slice(),
        b"[1\x0b,2]".as_slice(),
        b"null false".as_slice(),
    ] {
        assert_eq!(
            BoundedJsonValue::from_slice(invalid),
            Err(BoundedJsonError::InvalidSyntax),
            "accepted {invalid:?}",
        );
    }
}

#[test]
fn bounded_json_handles_near_cap_unicode_linearly_and_is_panic_free_for_bytes() {
    let text = "é".repeat(8_000);
    let input = format!(r#"{{"a":"{text}","b":"{text}","c":"{text}"}}"#);
    assert!(input.len() < 65_536);
    assert!(BoundedJsonObject::from_slice(input.as_bytes()).is_ok());

    for first in 0_u8..=255 {
        let _ = BoundedJsonValue::from_slice(&[first]);
        for second in 0_u8..=255 {
            let _ = BoundedJsonValue::from_slice(&[first, second]);
        }
    }
}

fn hash(value: &BoundedJsonObject) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn nested_array_object(array_count: usize) -> String {
    format!(
        r#"{{"v":{}0{}}}"#,
        "[".repeat(array_count),
        "]".repeat(array_count)
    )
}

fn object_with_members(count: usize) -> String {
    let members = (0..count)
        .map(|index| format!(r#""k{index:03}":0"#))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{members}}}")
}

fn object_with_array_items(count: usize) -> String {
    let items = std::iter::repeat_n("0", count)
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"v":[{items}]}}"#)
}

fn canonical_expansion_input(target_canonical_bytes: usize) -> String {
    let empty = r#"{"a":"","b":"","c":"","d":"","n":1e-6}"#;
    let expansion = "0.000001".len() - "1e-6".len();
    let target_input_bytes = target_canonical_bytes - expansion;
    let total_text_bytes = target_input_bytes - empty.len();
    let mut remaining = total_text_bytes;
    let mut values = Vec::new();
    for _ in 0..4 {
        let size = remaining.min(16_384);
        values.push("x".repeat(size));
        remaining -= size;
    }
    assert_eq!(remaining, 0);
    let input = format!(
        r#"{{"a":"{}","b":"{}","c":"{}","d":"{}","n":1e-6}}"#,
        values[0], values[1], values[2], values[3]
    );
    assert_eq!(input.len(), target_input_bytes);
    input
}
