use std::str::FromStr;

use minicore_runtime::{InteractionId, SessionId, SessionInstanceId, ToolCallId, TurnId};

fn assert_runtime_id<T>(id: T, prefix: &str)
where
    T: Clone + FromStr + std::fmt::Display + std::fmt::Debug + Eq,
    <T as FromStr>::Err: std::fmt::Debug,
{
    let encoded = id.to_string();
    assert_eq!(encoded.len(), prefix.len() + 32);
    assert!(encoded.starts_with(prefix));
    assert!(
        encoded[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    assert_eq!(encoded, encoded.to_ascii_lowercase());
    assert_eq!(encoded.parse::<T>().unwrap(), id);
}

#[test]
fn runtime_ids_are_canonical_random_prefixed_hex_values() {
    let session_id = SessionId::new().expect("the test entropy source is available");
    let instance_id = SessionInstanceId::new().expect("the test entropy source is available");
    let turn_id = TurnId::new().expect("the test entropy source is available");
    let interaction_id = InteractionId::new().expect("the test entropy source is available");

    assert_runtime_id(session_id, "ses_");
    assert_runtime_id(instance_id, "ins_");
    assert_runtime_id(turn_id, "trn_");
    assert_runtime_id(interaction_id, "int_");

    let first = SessionInstanceId::new().unwrap().to_string();
    let second = SessionInstanceId::new().unwrap().to_string();
    assert_ne!(first, second, "fresh runtime IDs must not be deterministic");
}

#[test]
fn runtime_id_errors_are_canonical_and_redacted() {
    let secret = "ses_00000000000000000000000000000000";
    let error = SessionId::from_str(secret).expect_err("zero payload must be rejected");
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));

    for invalid in [
        "trn_00000000000000000000000000000000",
        "trn_0000000000000000000000000000000",
        "trn_0000000000000000000000000000000g",
        "ses_11111111111111111111111111111111",
        "int_11111111111111111111111111111111 ",
    ] {
        assert!(TurnId::from_str(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn runtime_id_json_is_the_same_canonical_string() {
    let id = InteractionId::new().unwrap();
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, format!("\"{id}\""));
    assert_eq!(serde_json::from_str::<InteractionId>(&json).unwrap(), id);

    let instance_id = SessionInstanceId::new().unwrap();
    let json = serde_json::to_string(&instance_id).unwrap();
    assert_eq!(json, format!("\"{instance_id}\""));
    assert_eq!(
        serde_json::from_str::<SessionInstanceId>(&json).unwrap(),
        instance_id
    );
}

#[test]
fn tool_call_id_is_opaque_printable_provider_text_not_a_runtime_id() {
    let value = "provider/call:1!";
    let call_id = ToolCallId::from_str(value).unwrap();
    assert_eq!(call_id.as_str(), value);
    assert_eq!(call_id.to_string(), value);
    assert_eq!(
        serde_json::to_string(&call_id).unwrap(),
        format!("\"{value}\"")
    );

    for invalid in [
        "",
        "has space",
        "has\"quote",
        "has\\slash",
        "line\nbreak",
        "nul\0byte",
        "é",
    ] {
        assert!(
            ToolCallId::from_str(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(ToolCallId::from_str(&"!".repeat(256)).is_ok());
    assert!(ToolCallId::from_str(&"!".repeat(257)).is_err());
}
