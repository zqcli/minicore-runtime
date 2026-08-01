use std::path::{Path, PathBuf};

use minicore_runtime::wire::{CapabilityToken, ProtocolLimits};
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProtocolLimitRecipe {
    limits: ProtocolLimits,
}

#[derive(Debug, Deserialize)]
struct BootstrapEnvelope {
    data: WelcomeData,
}

#[derive(Debug, Deserialize)]
struct WelcomeData {
    limits: Box<RawValue>,
}

#[test]
fn protocol_limits_match_authoritative_recipe_and_canonical_field_order() {
    let fixture_root = fixture_root();
    let recipe: ProtocolLimitRecipe =
        read_json(&fixture_root.join("recipes/protocol-limit-cases.json"));
    assert_eq!(ProtocolLimits::v1_0(), recipe.limits);

    let welcome: BootstrapEnvelope =
        read_json(&fixture_root.join("public/valid/protocol-welcome.json"));
    assert_eq!(
        serde_json::to_string(&ProtocolLimits::v1_0()).unwrap(),
        welcome.data.limits.get()
    );
}

#[test]
fn capability_tokens_enforce_exact_v1_grammar_boundaries() {
    let one: CapabilityToken = "a".parse().unwrap();
    assert_eq!(one.as_str(), "a");

    let max = format!("a{}", "0".repeat(63));
    assert_eq!(max.len(), 64);
    assert!(max.parse::<CapabilityToken>().is_ok());

    for invalid in [
        "",
        "A",
        "0starts_with_digit",
        "has-hyphen",
        "has space",
        "é",
        &format!("a{}", "0".repeat(64)),
    ] {
        assert!(
            invalid.parse::<CapabilityToken>().is_err(),
            "accepted {invalid:?}"
        );
    }
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fixtures/wire-v1")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
