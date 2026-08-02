use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::*;

fn all_v1_capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities::for_v1(v1_runtime_capabilities()).unwrap()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LimitRecipe {
    validator_selectors: Vec<ValidatorSelector>,
    special_cases: BTreeMap<String, SpecialCase>,
    limits: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValidatorSelector {
    #[serde(default)]
    paths: Vec<String>,
    path_prefix: Option<String>,
    validator: String,
}

#[derive(Debug, Deserialize)]
struct SpecialCase {
    validator: Option<String>,
}

#[test]
fn every_advertised_limit_has_the_declared_owner_probe() {
    let recipe: LimitRecipe = read_json(&fixture_root().join("recipes/protocol-limit-cases.json"));
    let mut expected_limits = BTreeMap::new();
    flatten_limits("", &recipe.limits, &mut expected_limits);

    let probes = limit_probes(&ProtocolLimits::v1_0());
    assert_eq!(probes.len(), expected_limits.len());
    let mut observed_paths = BTreeSet::new();
    for probe in probes {
        assert!(
            observed_paths.insert(probe.path),
            "duplicate probe {}",
            probe.path
        );
        let expected_limit = expected_limits.get(probe.path).copied().unwrap();
        assert_eq!(probe.limit.maximum(), expected_limit, "{}", probe.path);
        assert_eq!(
            probe.validate_metric(expected_limit),
            Ok(()),
            "{}",
            probe.path
        );
        assert_eq!(
            probe.validate_metric(expected_limit + 1),
            Err(LimitError::Exceeded {
                maximum: expected_limit,
                actual: expected_limit + 1,
            }),
            "{}",
            probe.path,
        );
        assert_eq!(
            probe.validator.name(),
            expected_validator(&recipe, probe.path),
            "{}",
            probe.path,
        );
    }
    assert_eq!(
        observed_paths,
        expected_limits.keys().map(String::as_str).collect()
    );
}

#[test]
fn checked_counter_cannot_be_copied_and_keeps_state_after_errors() {
    let unit_limit = WireLimit::new(2);
    assert_eq!(unit_limit.validate_bytes(b"ab"), Ok(()));
    assert_eq!(unit_limit.validate_str("é"), Ok(()));
    assert_eq!(unit_limit.validate_items(&[1, 2]), Ok(()));

    let mut counter = CheckedLimitCounter::new(unit_limit);
    assert_eq!(counter.limit().maximum(), 2);
    assert_eq!(counter.try_add(2), Ok(2));
    assert_eq!(
        counter.try_add(1),
        Err(LimitError::Exceeded {
            maximum: 2,
            actual: 3,
        })
    );
    assert_eq!(counter.value(), 2);

    let mut overflow = CheckedLimitCounter::new(WireLimit::new(usize::MAX));
    overflow.try_add(usize::MAX).unwrap();
    assert_eq!(overflow.try_add(1), Err(LimitError::CounterOverflow));
    assert_eq!(overflow.value(), usize::MAX);
}

#[test]
fn hello_boundaries_and_safe_visible_text_follow_wire_rules() {
    let versions = (0..16)
        .map(|minor| ProtocolVersion::new(1, minor))
        .collect::<Vec<_>>();
    let capabilities = (0..64).map(|index| format!("c{index}")).collect::<Vec<_>>();
    let safe = ProtocolHello::new(
        versions.clone(),
        ClientInfo::new("\tfixture\nhost ", "v\n1"),
        capabilities.clone(),
    );
    assert!(matches!(
        negotiate_protocol(&safe, &all_v1_capabilities()),
        ProtocolNegotiation::Selected { .. }
    ));

    let too_many_versions = ProtocolHello::new(
        (0..17)
            .map(|minor| ProtocolVersion::new(1, minor))
            .collect(),
        ClientInfo::new("host", "1"),
        vec![],
    );
    let too_many_capabilities = ProtocolHello::new(
        vec![ProtocolVersion::V1_0],
        ClientInfo::new("host", "1"),
        (0..65).map(|index| format!("c{index}")).collect(),
    );
    let duplicate_capability = ProtocolHello::new(
        vec![ProtocolVersion::V1_0],
        ClientInfo::new("host", "1"),
        vec!["state_events".to_owned(), "state_events".to_owned()],
    );
    let invalid_capability = ProtocolHello::new(
        vec![ProtocolVersion::V1_0],
        ClientInfo::new("host", "1"),
        vec!["Future_capability".to_owned()],
    );
    let oversized_version = ProtocolHello::new(
        vec![ProtocolVersion::V1_0],
        ClientInfo::new("host", "v".repeat(129)),
        vec![],
    );
    for invalid in [
        too_many_versions,
        too_many_capabilities,
        duplicate_capability,
        invalid_capability,
        oversized_version,
    ] {
        assert_eq!(
            negotiate_protocol(&invalid, &all_v1_capabilities()),
            ProtocolNegotiation::Rejected {
                reason: ProtocolRejectReason::InvalidHello,
            }
        );
    }

    for forbidden in [
        '\0', '\u{0001}', '\u{000b}', '\r', '\u{001b}', '\u{007f}', '\u{0080}',
    ] {
        for client in [
            ClientInfo::new(format!("bad{forbidden}"), "1"),
            ClientInfo::new("host", format!("bad{forbidden}")),
        ] {
            let hello = ProtocolHello::new(vec![ProtocolVersion::V1_0], client, vec![]);
            assert_eq!(
                negotiate_protocol(&hello, &all_v1_capabilities()),
                ProtocolNegotiation::Rejected {
                    reason: ProtocolRejectReason::InvalidHello,
                },
                "accepted U+{:04X}",
                u32::from(forbidden),
            );
        }
    }

    let max_client = "é".repeat(64);
    let oversized_client = format!("{max_client}x");
    assert!(matches!(
        negotiate_protocol(
            &ProtocolHello::new(
                vec![ProtocolVersion::V1_0],
                ClientInfo::new(max_client, ""),
                vec![],
            ),
            &all_v1_capabilities(),
        ),
        ProtocolNegotiation::Selected { .. }
    ));
    assert_eq!(
        negotiate_protocol(
            &ProtocolHello::new(
                vec![ProtocolVersion::V1_0],
                ClientInfo::new(oversized_client, "1"),
                vec![],
            ),
            &all_v1_capabilities(),
        ),
        ProtocolNegotiation::Rejected {
            reason: ProtocolRejectReason::InvalidHello,
        }
    );

    assert_eq!(
        negotiate_protocol(
            &ProtocolHello::new(vec![], ClientInfo::new("", ""), vec![]),
            &all_v1_capabilities(),
        ),
        ProtocolNegotiation::Rejected {
            reason: ProtocolRejectReason::UnsupportedProtocolVersion,
        }
    );
}

#[test]
fn negotiation_selects_exact_v1_and_runtime_capability_order() {
    let hello = ProtocolHello::new(
        vec![
            ProtocolVersion::new(1, 0),
            ProtocolVersion::new(2, 0),
            ProtocolVersion::new(1, 1),
        ],
        ClientInfo::new("host", "1"),
        vec!["session_snapshot".to_owned(), "state_events".to_owned()],
    );
    assert_eq!(
        negotiate_protocol(&hello, &all_v1_capabilities()),
        ProtocolNegotiation::Selected {
            version: ProtocolVersion::V1_0,
            capabilities: RuntimeCapabilities::for_v1(vec![
                "state_events".parse().unwrap(),
                "session_snapshot".parse().unwrap(),
            ])
            .unwrap(),
        }
    );
}

#[test]
fn runtime_capability_outputs_are_known_unique_and_canonically_ordered() {
    let capabilities = RuntimeCapabilities::for_v1(vec![
        "session_snapshot".parse().unwrap(),
        "state_events".parse().unwrap(),
    ])
    .unwrap();
    assert_eq!(
        capabilities
            .values()
            .iter()
            .map(CapabilityToken::as_str)
            .collect::<Vec<_>>(),
        ["state_events", "session_snapshot"]
    );
    assert_eq!(
        RuntimeCapabilities::for_v1(vec![
            "state_events".parse().unwrap(),
            "state_events".parse().unwrap(),
        ]),
        Err(RuntimeCapabilitiesError::DuplicateCapability)
    );
    assert_eq!(
        RuntimeCapabilities::for_v1(vec!["future_capability".parse().unwrap()]),
        Err(RuntimeCapabilitiesError::UnknownCapability)
    );
}

fn expected_validator<'a>(recipe: &'a LimitRecipe, path: &str) -> &'a str {
    if let Some(validator) = recipe
        .special_cases
        .get(path)
        .and_then(|special| special.validator.as_deref())
    {
        return validator;
    }
    recipe
        .validator_selectors
        .iter()
        .find(|selector| {
            selector.paths.iter().any(|candidate| candidate == path)
                || selector
                    .path_prefix
                    .as_deref()
                    .is_some_and(|prefix| path.starts_with(prefix))
        })
        .map(|selector| selector.validator.as_str())
        .unwrap_or_else(|| panic!("missing validator selector for {path}"))
}

fn flatten_limits(prefix: &str, value: &Value, output: &mut BTreeMap<String, usize>) {
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_limits(&path, value, output);
            }
        }
        Value::Number(value) => {
            output.insert(prefix.to_owned(), value.as_u64().unwrap() as usize);
        }
        _ => panic!("limit recipe leaf must be a number"),
    }
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fixtures/wire-v1")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
