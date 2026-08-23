use std::collections::BTreeSet;
use std::time::Instant;

use minicore_runtime::config::ConfigError;
use minicore_runtime::config::{
    SessionManifest, Timestamp, TimestampError, TurnOptions, UserInput,
};
use minicore_runtime::ids::{SessionId, SessionInstanceId};
use minicore_runtime::model::ReasoningPreference;
use minicore_runtime::tools::ToolName;
use minicore_runtime::value::{BoundedText, MAX_TEXT_BYTES};
use minicore_runtime::{CompactionConfig, SemanticLimits, SessionSpec};
use serde_json::{Value, json};

fn spec() -> SessionSpec {
    SessionSpec::new(
        "model:v1".parse().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        64,
        CompactionConfig::Disabled,
    )
    .unwrap()
}

fn manifest() -> SessionManifest {
    SessionManifest::new(SessionId::new().unwrap(), spec()).unwrap()
}

#[test]
fn timestamp_is_canonical_serializable_and_public_only_through_config() {
    let timestamp: Timestamp = "2026-08-19T12:34:56.789Z".parse().unwrap();
    let _: Result<Timestamp, TimestampError> = Ok(timestamp.clone());
    assert_eq!(timestamp.as_str(), "2026-08-19T12:34:56.789Z");
    assert_eq!(
        serde_json::from_str::<Timestamp>(&serde_json::to_string(&timestamp).unwrap()).unwrap(),
        timestamp
    );
    assert_eq!(
        "2026-08-19T12:34:56Z".parse::<Timestamp>().unwrap_err(),
        TimestampError::Invalid
    );
    assert_eq!(Timestamp::now_utc().unwrap().as_str().len(), 24);

    let config = include_str!("../src/config.rs");
    assert!(config.contains("pub use crate::time::{Timestamp, TimestampError};"));
    let lib = include_str!("../src/lib.rs");
    assert!(!lib.contains("pub use time::"));
    let root_config_exports = lib
        .split_once("pub use config::{")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(exports, _)| exports)
        .unwrap_or("");
    assert!(!root_config_exports.contains("Timestamp"));
}

#[test]
fn session_manifest_is_versioned_strict_and_does_not_persist_instance_identity() {
    let session_manifest = manifest();
    assert_eq!(
        session_manifest.format_version,
        SessionManifest::FORMAT_VERSION
    );
    assert_eq!(SessionManifest::FORMAT_VERSION, 3);
    assert!(
        session_manifest
            .validate(&SemanticLimits::default())
            .is_ok()
    );

    let encoded = serde_json::to_value(&session_manifest).unwrap();
    let fields = encoded.as_object().unwrap();
    assert_eq!(fields.len(), 4);
    for field in ["format_version", "session_id", "created_at", "spec"] {
        assert!(fields.contains_key(field));
    }
    for forbidden in ["instance_id", "workspace", "provider", "credential"] {
        assert!(!fields.contains_key(forbidden));
    }
    let instance = SessionInstanceId::new().unwrap();
    assert!(!encoded.to_string().contains(&instance.to_string()));
    assert_eq!(
        serde_json::from_value::<SessionManifest>(encoded.clone()).unwrap(),
        session_manifest
    );

    let mut wrong_version = encoded.as_object().unwrap().clone();
    wrong_version.insert("format_version".to_owned(), json!(2));
    assert!(serde_json::from_value::<SessionManifest>(Value::Object(wrong_version)).is_err());

    let mut unknown = encoded.as_object().unwrap().clone();
    unknown.insert("instance_id".to_owned(), json!(instance.to_string()));
    assert!(serde_json::from_value::<SessionManifest>(Value::Object(unknown)).is_err());

    let mut invalid_spec = encoded.as_object().unwrap().clone();
    let mut spec_value = invalid_spec
        .get("spec")
        .and_then(Value::as_object)
        .expect("manifest spec is an object")
        .clone();
    spec_value.insert("max_tool_rounds".to_owned(), json!(0));
    invalid_spec.insert("spec".to_owned(), Value::Object(spec_value));
    assert!(serde_json::from_value::<SessionManifest>(Value::Object(invalid_spec)).is_err());

    let first = manifest();
    let second = manifest();
    assert!(first.created_at.as_str().parse::<Timestamp>().is_ok());
    assert!(second.created_at.as_str().parse::<Timestamp>().is_ok());
}

#[test]
fn user_input_is_nonempty_by_default_and_respects_custom_limits() {
    assert_eq!(UserInput::text(""), Err(ConfigError::InvalidText));
    let exact = UserInput::text("x".repeat(MAX_TEXT_BYTES)).unwrap();
    assert_eq!(exact.as_text().len(), MAX_TEXT_BYTES);
    assert_eq!(
        UserInput::text("x".repeat(MAX_TEXT_BYTES + 1)),
        Err(ConfigError::InvalidText)
    );

    let control = UserInput::text("nul\0tab\tnewline\n").unwrap();
    assert_eq!(control.as_text(), "nul\0tab\tnewline\n");

    let limits = SemanticLimits {
        max_user_input_bytes: 3,
        ..SemanticLimits::default()
    };
    assert_eq!(control.validate(&limits), Err(ConfigError::InvalidText));
    let empty = UserInput::Text(BoundedText::new("").unwrap());
    assert_eq!(
        empty.validate(&SemanticLimits::default()),
        Err(ConfigError::InvalidText)
    );
}

#[test]
fn turn_options_default_and_round_limits_are_checked() {
    let defaults = TurnOptions::default();
    assert!(defaults.deadline.is_none());
    assert!(defaults.max_tool_rounds.is_none());
    assert!(defaults.validate(&SemanticLimits::default()).is_ok());

    let limits = SemanticLimits {
        max_tool_rounds: 2,
        ..SemanticLimits::default()
    };
    assert!(
        TurnOptions {
            deadline: Some(Instant::now()),
            max_tool_rounds: Some(1),
        }
        .validate(&limits)
        .is_ok()
    );
    assert_eq!(
        TurnOptions {
            deadline: None,
            max_tool_rounds: Some(0),
        }
        .validate(&limits),
        Err(ConfigError::InvalidBounds)
    );
    assert_eq!(
        TurnOptions {
            deadline: None,
            max_tool_rounds: Some(3),
        }
        .validate(&limits),
        Err(ConfigError::InvalidBounds)
    );

    let source = include_str!("../src/config/session.rs");
    assert!(!source.contains("Serialize)]\npub struct TurnOptions"));
    assert!(!source.contains("Deserialize for TurnOptions"));
    assert!(!source.contains("SessionManifest {\n        pub deadline"));
}

#[test]
fn manifest_nested_checked_values_remain_checked() {
    assert!(
        serde_json::from_value::<SessionManifest>(json!({
            "format_version": 3,
            "session_id": "bad",
            "created_at": "2026-08-19T12:34:56.789Z",
            "spec": {}
        }))
        .is_err()
    );
    assert!(serde_json::from_value::<ToolName>(json!("bad/name")).is_err());
    assert!(serde_json::from_value::<BoundedText>(json!(42)).is_err());
}
