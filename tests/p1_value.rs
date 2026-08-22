use minicore_runtime::BoundedText as RootBoundedText;
use minicore_runtime::value::{
    BoundedText, MAX_JSON_BYTES, MAX_JSON_DEPTH, MAX_JSON_NODES, MAX_TEXT_BYTES, ValueError,
    validate_json_size,
};
use serde_json::{Value, json};

#[test]
fn bounded_text_enforces_absolute_and_caller_byte_limits() {
    let exact_absolute = BoundedText::new("x".repeat(MAX_TEXT_BYTES)).unwrap();
    assert_eq!(exact_absolute.byte_len(), MAX_TEXT_BYTES);
    assert_eq!(
        BoundedText::new("x".repeat(MAX_TEXT_BYTES + 1)).unwrap_err(),
        ValueError::TextTooLarge
    );

    let multibyte = "ééa";
    assert_eq!(multibyte.len(), 5);
    assert_eq!(
        BoundedText::new_with_max_bytes(multibyte, 5)
            .unwrap()
            .as_str(),
        multibyte
    );
    assert_eq!(
        BoundedText::new_with_max_bytes("ééab", 5).unwrap_err(),
        ValueError::TextTooLarge
    );
    assert_eq!(
        BoundedText::new_with_max_bytes("", MAX_TEXT_BYTES + 1).unwrap_err(),
        ValueError::TextLimitTooLarge
    );

    let empty = BoundedText::new("").unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.into_string(), "");
}

#[test]
fn bounded_text_accepts_control_characters_without_extra_policy() {
    let control = "nul\0tab\tnewline\nunit\u{0001}";
    let value = BoundedText::new(control).unwrap();
    assert_eq!(value.as_str(), control);
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(
        serde_json::from_str::<BoundedText>(&encoded).unwrap(),
        value
    );
}

#[test]
fn bounded_text_serde_roundtrip_and_debug_are_safe() {
    let secret = "secret text";
    let value = RootBoundedText::new(secret).unwrap();
    let encoded = serde_json::to_string(&value).unwrap();
    assert_eq!(encoded, format!("\"{secret}\""));
    assert_eq!(
        serde_json::from_str::<RootBoundedText>(&encoded).unwrap(),
        value
    );

    let debug = format!("{value:?}");
    assert!(!debug.contains(secret));
    assert!(debug.contains(&value.byte_len().to_string()));
    assert_eq!(value.to_string(), secret);

    let exact = serde_json::to_string(&"x".repeat(MAX_TEXT_BYTES)).unwrap();
    assert!(serde_json::from_str::<BoundedText>(&exact).is_ok());
    let oversized = serde_json::to_string(&"x".repeat(MAX_TEXT_BYTES + 1)).unwrap();
    let error = serde_json::from_str::<BoundedText>(&oversized).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("bounded text exceeds its byte limit")
    );
}

#[test]
fn json_validation_enforces_compact_bytes_depth_and_nodes() {
    let exact_text = Value::String("x".repeat(MAX_JSON_BYTES - 2));
    assert_eq!(
        serde_json::to_vec(&exact_text).unwrap().len(),
        MAX_JSON_BYTES
    );
    assert!(validate_json_size(&exact_text, MAX_JSON_BYTES).is_ok());
    assert_eq!(
        validate_json_size(&exact_text, MAX_JSON_BYTES - 1).unwrap_err(),
        ValueError::JsonTooLarge
    );

    let over_text = Value::String("x".repeat(MAX_JSON_BYTES - 1));
    assert_eq!(
        validate_json_size(&over_text, MAX_JSON_BYTES).unwrap_err(),
        ValueError::JsonTooLarge
    );
    assert_eq!(
        validate_json_size(&Value::Null, MAX_JSON_BYTES + 1).unwrap_err(),
        ValueError::JsonLimitTooLarge
    );
    assert_eq!(
        validate_json_size(&Value::Null, 0).unwrap_err(),
        ValueError::JsonTooLarge
    );

    let large_text = Value::String("x".repeat(MAX_JSON_BYTES + 1_024));
    assert_eq!(
        validate_json_size(&large_text, MAX_JSON_BYTES).unwrap_err(),
        ValueError::JsonTooLarge
    );

    let mut within_depth = Value::Null;
    for _ in 0..MAX_JSON_DEPTH {
        within_depth = Value::Array(vec![within_depth]);
    }
    assert!(validate_json_size(&within_depth, MAX_JSON_BYTES).is_ok());
    let too_deep = Value::Array(vec![within_depth]);
    assert_eq!(
        validate_json_size(&too_deep, MAX_JSON_BYTES).unwrap_err(),
        ValueError::JsonTooDeep
    );

    let within_nodes = Value::Array(vec![Value::Null; MAX_JSON_NODES - 1]);
    assert!(validate_json_size(&within_nodes, MAX_JSON_BYTES).is_ok());
    let too_many_nodes = Value::Array(vec![Value::Null; MAX_JSON_NODES]);
    assert_eq!(
        validate_json_size(&too_many_nodes, MAX_JSON_BYTES).unwrap_err(),
        ValueError::JsonTooManyNodes
    );

    let compact_object = json!({"message": "hello"});
    let compact_size = serde_json::to_vec(&compact_object).unwrap().len();
    assert!(validate_json_size(&compact_object, compact_size).is_ok());
    assert_eq!(
        validate_json_size(&compact_object, compact_size - 1).unwrap_err(),
        ValueError::JsonTooLarge
    );
}
