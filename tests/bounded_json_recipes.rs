use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use minicore_runtime::wire::BoundedJsonObject;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BoundaryRecipes {
    cases: Vec<BoundaryCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoundaryCase {
    name: String,
    scope: String,
    input_utf8: Option<String>,
    expected_canonical_utf8: Option<String>,
    target_raw_input_bytes: Option<usize>,
    target_canonical_encoded_bytes: Option<usize>,
    target_depth: Option<usize>,
    target_object_members: Option<usize>,
    target_array_items: Option<usize>,
    target_decoded_string_bytes: Option<usize>,
    target_number_literal_bytes: Option<usize>,
    expected: BoundaryExpectation,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoundaryExpectation {
    value_accepted: Option<bool>,
}

#[test]
fn authoritative_bounded_json_recipes_are_all_executed() {
    let recipes: BoundaryRecipes = read_json(&fixture_root().join("recipes/boundary-cases.json"));
    let mut executed = BTreeSet::new();

    for case in recipes
        .cases
        .into_iter()
        .filter(|case| case.scope == "embedded_json_value")
    {
        let input = generate_input(&case);
        let result = BoundedJsonObject::from_slice(input.as_bytes());
        assert_eq!(
            result.is_ok(),
            case.expected.value_accepted.unwrap(),
            "{}: {result:?}",
            case.name
        );
        if let Some(expected) = &case.expected_canonical_utf8 {
            assert_eq!(result.unwrap().canonical_json(), expected, "{}", case.name);
        }
        assert!(executed.insert(case.name));
    }

    let expected = [
        "bounded_json_canonicalization",
        "bounded_json_exponent_oversized",
        "bounded_json_input_bytes_boundary",
        "bounded_json_input_bytes_oversized",
        "bounded_json_output_bytes_boundary",
        "bounded_json_output_bytes_oversized",
        "bounded_json_depth_boundary",
        "bounded_json_depth_oversized",
        "bounded_json_members_boundary",
        "bounded_json_members_oversized",
        "bounded_json_array_boundary",
        "bounded_json_array_oversized",
        "bounded_json_string_boundary",
        "bounded_json_string_oversized",
        "bounded_json_number_boundary",
        "bounded_json_number_oversized",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    assert_eq!(executed, expected);
}

fn generate_input(case: &BoundaryCase) -> String {
    if let Some(input) = &case.input_utf8 {
        return input.clone();
    }
    match case.name.as_str() {
        "bounded_json_input_bytes_boundary" | "bounded_json_input_bytes_oversized" => {
            let target = case.target_raw_input_bytes.unwrap();
            format!("{{}}{}", " ".repeat(target - 2))
        }
        "bounded_json_output_bytes_boundary" | "bounded_json_output_bytes_oversized" => {
            canonical_expansion_input(case.target_canonical_encoded_bytes.unwrap())
        }
        "bounded_json_depth_boundary" | "bounded_json_depth_oversized" => {
            let arrays = case.target_depth.unwrap() - 2;
            format!(
                r#"{{"value":{}0{}}}"#,
                "[".repeat(arrays),
                "]".repeat(arrays)
            )
        }
        "bounded_json_members_boundary" | "bounded_json_members_oversized" => {
            let members = (0..case.target_object_members.unwrap())
                .map(|index| format!(r#""k{index:03}":0"#))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{members}}}")
        }
        "bounded_json_array_boundary" | "bounded_json_array_oversized" => {
            let items = std::iter::repeat_n("0", case.target_array_items.unwrap())
                .collect::<Vec<_>>()
                .join(",");
            format!(r#"{{"value":[{items}]}}"#)
        }
        "bounded_json_string_boundary" | "bounded_json_string_oversized" => format!(
            r#"{{"value":"{}"}}"#,
            "x".repeat(case.target_decoded_string_bytes.unwrap())
        ),
        "bounded_json_number_boundary" | "bounded_json_number_oversized" => {
            let target = case.target_number_literal_bytes.unwrap();
            let coefficient = "1".repeat(60);
            let exponent = "0".repeat(target - coefficient.len() - 1);
            format!(r#"{{"value":{coefficient}e{exponent}}}"#)
        }
        other => panic!("unimplemented embedded JSON recipe {other}"),
    }
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
    format!(
        r#"{{"a":"{}","b":"{}","c":"{}","d":"{}","n":1e-6}}"#,
        values[0], values[1], values[2], values[3]
    )
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fixtures/wire-v1")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
