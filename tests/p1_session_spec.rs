use std::collections::BTreeSet;
use std::str::FromStr;

use minicore_runtime::config::ConfigError;
use minicore_runtime::model::{ModelRef, ModelRefError, ReasoningPreference};
use minicore_runtime::tools::ToolName;
use minicore_runtime::value::{BoundedText, MAX_TEXT_BYTES};
use minicore_runtime::{CompactionConfig, SemanticLimits, SessionSpec};
use serde_json::{Value, json};

fn model_ref() -> ModelRef {
    "model:v1".parse().unwrap()
}

fn tool_name(value: &str) -> ToolName {
    ToolName::from_str(value).unwrap()
}

fn tools(names: &[&str]) -> BTreeSet<ToolName> {
    names.iter().map(|name| tool_name(name)).collect()
}

fn spec_with(
    prompt: BoundedText,
    enabled_tools: BTreeSet<ToolName>,
    max_tool_rounds: u16,
    compaction: CompactionConfig,
) -> SessionSpec {
    SessionSpec::new(
        model_ref(),
        ReasoningPreference::Auto,
        prompt,
        enabled_tools,
        max_tool_rounds,
        compaction,
    )
    .unwrap()
}

#[test]
fn model_ref_is_checked_adapter_neutral_and_serializable() {
    let allowed = "A_1-model.v2:provider/model".parse::<ModelRef>().unwrap();
    assert_eq!(allowed.as_str(), "A_1-model.v2:provider/model");
    assert_eq!(allowed.to_string(), allowed.as_str());
    assert_eq!(format!("{allowed:?}"), allowed.as_str());

    let exact = "x".repeat(256).parse::<ModelRef>().unwrap();
    assert_eq!("x".parse::<ModelRef>().unwrap().as_str(), "x");
    assert_eq!(
        "".parse::<ModelRef>().unwrap_err(),
        ModelRefError::InvalidLength
    );
    assert_eq!(exact.as_str().len(), 256);
    assert_eq!(
        "x".repeat(257).parse::<ModelRef>().unwrap_err(),
        ModelRefError::InvalidLength
    );
    for invalid in ["has space", "has\nnewline", "é", "provider?model"] {
        assert_eq!(
            invalid.parse::<ModelRef>().unwrap_err(),
            ModelRefError::InvalidGrammar,
            "accepted {invalid:?}"
        );
    }

    let encoded = serde_json::to_string(&allowed).unwrap();
    assert_eq!(serde_json::from_str::<ModelRef>(&encoded).unwrap(), allowed);
    assert!(serde_json::from_value::<ModelRef>(json!(42)).is_err());
    assert!(serde_json::from_value::<ModelRef>(json!({"value": "model:v1"})).is_err());
}

#[test]
fn compaction_config_is_stable_and_checked() {
    let disabled = CompactionConfig::Disabled;
    assert!(disabled.validate().is_ok());
    assert_eq!(
        serde_json::to_value(&disabled).unwrap(),
        json!({"mode": "disabled"})
    );

    let enabled = CompactionConfig::Enabled {
        trigger_tokens: 1_000,
        target_tokens: 500,
    };
    assert!(enabled.validate().is_ok());
    assert_eq!(
        serde_json::to_value(&enabled).unwrap(),
        json!({"mode": "enabled", "trigger_tokens": 1_000, "target_tokens": 500})
    );
    assert_eq!(
        serde_json::from_value::<CompactionConfig>(serde_json::to_value(&enabled).unwrap())
            .unwrap(),
        enabled
    );

    for invalid in [
        CompactionConfig::Enabled {
            trigger_tokens: 0,
            target_tokens: 1,
        },
        CompactionConfig::Enabled {
            trigger_tokens: 1,
            target_tokens: 0,
        },
        CompactionConfig::Enabled {
            trigger_tokens: 1,
            target_tokens: 1,
        },
        CompactionConfig::Enabled {
            trigger_tokens: 1,
            target_tokens: 2,
        },
    ] {
        assert_eq!(invalid.validate(), Err(ConfigError::InvalidBounds));
    }

    assert!(
        serde_json::from_value::<CompactionConfig>(json!({
            "mode": "disabled",
            "extra": true
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<CompactionConfig>(json!({
            "mode": "enabled",
            "trigger_tokens": 10,
            "target_tokens": 5,
            "extra": true
        }))
        .is_err()
    );

    for invalid in [
        json!({"mode": "enabled", "trigger_tokens": 0, "target_tokens": 1}),
        json!({"mode": "enabled", "trigger_tokens": 1, "target_tokens": 0}),
        json!({"mode": "enabled", "trigger_tokens": 1, "target_tokens": 1}),
        json!({"mode": "enabled", "trigger_tokens": 1, "target_tokens": 2}),
        json!({"mode": "wrong"}),
        json!({"trigger_tokens": 10, "target_tokens": 5}),
    ] {
        assert!(
            serde_json::from_value::<CompactionConfig>(invalid).is_err(),
            "accepted invalid compaction JSON"
        );
    }
}

#[test]
fn session_spec_roundtrips_strictly_and_keeps_tool_order_deterministic() {
    let first = spec_with(
        BoundedText::new("").unwrap(),
        tools(&["zeta", "alpha"]),
        64,
        CompactionConfig::Disabled,
    );
    let second = spec_with(
        BoundedText::new("").unwrap(),
        tools(&["alpha", "zeta"]),
        64,
        CompactionConfig::Disabled,
    );
    let first_json = serde_json::to_string(&first).unwrap();
    assert_eq!(first_json, serde_json::to_string(&second).unwrap());
    assert_eq!(
        serde_json::from_str::<SessionSpec>(&first_json).unwrap(),
        first
    );

    let object = serde_json::to_value(&first).unwrap();
    let fields = object.as_object().unwrap();
    assert_eq!(fields.len(), 6);
    let forbidden_fields = [
        "workspace",
        "path",
        "provider",
        "endpoint",
        "credential",
        "runtime",
        "handle",
    ];
    for &forbidden in &forbidden_fields {
        assert!(!fields.contains_key(forbidden));
    }
    let debug = format!("{first:?}");
    for &forbidden in &forbidden_fields {
        assert!(!debug.contains(forbidden));
    }

    let mut unknown = object
        .as_object()
        .expect("session spec is an object")
        .clone();
    unknown.insert("workspace".to_owned(), Value::String("/tmp".to_owned()));
    assert!(serde_json::from_value::<SessionSpec>(Value::Object(unknown)).is_err());

    let mut invalid = object
        .as_object()
        .expect("session spec is an object")
        .clone();
    invalid.insert("max_tool_rounds".to_owned(), json!(0));
    assert!(serde_json::from_value::<SessionSpec>(Value::Object(invalid)).is_err());

    let mut invalid = object
        .as_object()
        .expect("session spec is an object")
        .clone();
    invalid.insert("max_tool_rounds".to_owned(), json!(1025));
    assert!(serde_json::from_value::<SessionSpec>(Value::Object(invalid)).is_err());

    let mut invalid = object
        .as_object()
        .expect("session spec is an object")
        .clone();
    invalid.insert(
        "compaction".to_owned(),
        json!({"mode": "enabled", "trigger_tokens": 1, "target_tokens": 1}),
    );
    assert!(serde_json::from_value::<SessionSpec>(Value::Object(invalid)).is_err());

    let tool_values: Vec<String> = (0..=4096).map(|index| format!("tool{index}")).collect();
    let mut invalid = object
        .as_object()
        .expect("session spec is an object")
        .clone();
    invalid.insert("enabled_tools".to_owned(), json!(tool_values));
    assert!(serde_json::from_value::<SessionSpec>(Value::Object(invalid)).is_err());
}

#[test]
fn session_spec_uses_custom_semantic_limits() {
    let prompt = BoundedText::new("abcd").unwrap();
    let base = spec_with(
        prompt.clone(),
        tools(&["one", "two"]),
        3,
        CompactionConfig::Disabled,
    );

    let limits = SemanticLimits {
        max_system_prompt_bytes: 3,
        ..SemanticLimits::default()
    };
    assert_eq!(base.validate(&limits), Err(ConfigError::InvalidBounds));

    let limits = SemanticLimits {
        max_tool_count: 1,
        ..SemanticLimits::default()
    };
    assert_eq!(base.validate(&limits), Err(ConfigError::InvalidBounds));

    let named = spec_with(
        prompt.clone(),
        tools(&["four"]),
        3,
        CompactionConfig::Disabled,
    );
    let limits = SemanticLimits {
        max_tool_name_bytes: 3,
        ..SemanticLimits::default()
    };
    assert_eq!(named.validate(&limits), Err(ConfigError::InvalidBounds));

    let rounds = spec_with(prompt, tools(&["one"]), 3, CompactionConfig::Disabled);
    let limits = SemanticLimits {
        max_tool_rounds: 2,
        ..SemanticLimits::default()
    };
    assert_eq!(rounds.validate(&limits), Err(ConfigError::InvalidBounds));

    let exact_prompt = BoundedText::new("x".repeat(MAX_TEXT_BYTES)).unwrap();
    let exact_name = tool_name(&"x".repeat(64));
    let exact = spec_with(
        exact_prompt,
        BTreeSet::from([exact_name]),
        64,
        CompactionConfig::Disabled,
    );
    assert!(exact.validate(&SemanticLimits::default()).is_ok());
}

#[test]
fn session_spec_new_rejects_round_and_compaction_invariants() {
    let prompt = BoundedText::new("").unwrap();
    assert_eq!(
        SessionSpec::new(
            model_ref(),
            ReasoningPreference::Auto,
            prompt.clone(),
            BTreeSet::new(),
            0,
            CompactionConfig::Disabled,
        ),
        Err(ConfigError::InvalidBounds)
    );
    assert_eq!(
        SessionSpec::new(
            model_ref(),
            ReasoningPreference::Auto,
            prompt,
            BTreeSet::new(),
            1,
            CompactionConfig::Enabled {
                trigger_tokens: 1,
                target_tokens: 1,
            },
        ),
        Err(ConfigError::InvalidBounds)
    );
}

#[test]
fn nested_checked_deserializers_reject_malformed_values() {
    assert!(serde_json::from_value::<ModelRef>(json!("bad value")).is_err());
    assert!(serde_json::from_value::<ToolName>(json!("bad/name")).is_err());
    assert!(serde_json::from_value::<BoundedText>(json!(123)).is_err());
    assert!(serde_json::from_value::<BoundedText>(json!({"text": "x"})).is_err());
}
