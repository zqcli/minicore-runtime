use minicore_runtime::Text as RootText;
use minicore_runtime::value::{MAX_JSON_NESTING, Text, ValueError, validate_json_nesting};
use serde_json::{Value, json};

#[test]
fn bounded_text_enforces_absolute_and_caller_byte_limits() {
    for value in ["".to_owned(), "ééab".to_owned(), "x".repeat(256 * 1024 + 1)] {
        let text = Text::new(&value);
        assert_eq!(text.byte_len(), value.len());
        assert_eq!(text.is_empty(), value.is_empty());
        assert_eq!(text.as_str(), value);
        assert_eq!(text.into_string(), value);
    }
}

#[test]
fn bounded_text_accepts_control_characters_without_extra_policy() {
    let control = "nul\0tab\tnewline\nunit\u{0001}";
    let value = Text::new(control);
    assert_eq!(value.as_str(), control);
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(serde_json::from_str::<Text>(&encoded).unwrap(), value);
}

#[test]
fn bounded_text_serde_roundtrip_and_debug_are_safe() {
    let secret = "secret text";
    let value = RootText::new(secret);
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(encoded, format!("\"{secret}\""));
    assert_eq!(serde_json::from_str::<RootText>(&encoded).unwrap(), value);
    let debug = format!("{value:?}");
    assert!(!debug.contains(secret));
    assert!(debug.contains(&value.byte_len().to_string()));
    assert_eq!(value.to_string(), secret);
    let beyond_former_limit = "x".repeat(256 * 1024 + 1);
    let encoded = serde_json::to_string(&beyond_former_limit).unwrap();
    assert_eq!(
        serde_json::from_str::<Text>(&encoded).unwrap().as_str(),
        beyond_former_limit
    );
    assert!(serde_json::from_str::<Text>("17").is_err());
}

#[test]
fn json_validation_enforces_compact_bytes_depth_and_nodes() {
    for value in [
        Value::String("x".repeat(64 * 1024 + 1)),
        Value::Array(vec![Value::Null; 16_385]),
        json!({"message": "hello"}),
    ] {
        assert!(validate_json_nesting(&value).is_ok());
        let wire = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&wire).unwrap(), value);
    }
    assert_eq!(MAX_JSON_NESTING, 127);
    for objects in [false, true] {
        let mut value = Value::Null;
        for nesting in 1..=MAX_JSON_NESTING + 1 {
            value = if objects {
                json!({"nested": value})
            } else {
                json!([value])
            };
            let wire = serde_json::to_string(&value).unwrap();
            let parsed = serde_json::from_str::<Value>(&wire);
            if nesting <= MAX_JSON_NESTING {
                assert!(validate_json_nesting(&value).is_ok());
                assert_eq!(parsed.unwrap(), value);
            } else {
                assert_eq!(validate_json_nesting(&value), Err(ValueError::JsonTooDeep));
                assert!(
                    parsed
                        .unwrap_err()
                        .to_string()
                        .contains("recursion limit exceeded")
                );
            }
        }
    }
}
