use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use minicore_runtime::wire::{BoundedJsonError, BoundedJsonSchema, BoundedJsonSchemaError};
use serde::Deserialize;
use serde_json::Value;

const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

#[derive(Debug, Deserialize)]
struct BoundaryRecipes {
    cases: Vec<BoundaryCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoundaryCase {
    name: String,
    scope: String,
    generator: Option<BoundaryGenerator>,
    input_utf8: Option<String>,
    target_raw_input_bytes: Option<usize>,
    target_canonical_encoded_bytes: Option<usize>,
    max_raw_input_bytes: Option<usize>,
    target_depth: Option<usize>,
    target_properties_required_or_enum_items: Option<usize>,
    target_nodes: Option<usize>,
    target_regex_bytes: Option<usize>,
    expected: BoundaryExpectation,
}

#[derive(Debug, Deserialize)]
struct BoundaryGenerator {
    kind: String,
    collection: Option<String>,
    #[serde(rename = "canonicalOutputBelowBytes")]
    canonical_output_below_bytes: Option<usize>,
    #[serde(rename = "ref")]
    reference: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoundaryExpectation {
    schema_accepted: Option<bool>,
    failure: Option<String>,
    network_access: Option<bool>,
}

#[test]
fn authoritative_bounded_schema_recipes_are_all_executed() {
    let recipes: BoundaryRecipes = read_json(&fixture_root().join("recipes/boundary-cases.json"));
    let mut executed = BTreeSet::new();

    for case in recipes
        .cases
        .into_iter()
        .filter(|case| case.scope == "embedded_json_schema")
    {
        let input = generate_input(&case);
        validate_generated_metrics(&case, &input);
        let result = BoundedJsonSchema::from_slice(input.as_bytes());
        assert_eq!(
            result.is_ok(),
            case.expected.schema_accepted.unwrap(),
            "{} via {}: {result:?}",
            case.name,
            case.generator.as_ref().unwrap().kind
        );
        if let Some(target) = case.target_canonical_encoded_bytes {
            if let Ok(schema) = &result {
                assert_eq!(schema.canonical_bytes().len(), target, "{}", case.name);
            }
        }
        if let Err(error) = &result {
            validate_failure(&case, error);
        }
        assert!(executed.insert(case.name));
    }

    let expected = [
        "bounded_schema_input_bytes_boundary",
        "bounded_schema_input_bytes_oversized",
        "bounded_schema_output_bytes_boundary",
        "bounded_schema_output_bytes_oversized",
        "bounded_schema_depth_boundary",
        "bounded_schema_depth_oversized",
        "bounded_schema_properties_boundary",
        "bounded_schema_properties_oversized",
        "bounded_schema_required_boundary",
        "bounded_schema_required_oversized",
        "bounded_schema_enum_boundary",
        "bounded_schema_enum_oversized",
        "bounded_schema_nodes_boundary",
        "bounded_schema_nodes_oversized",
        "bounded_schema_regex_boundary",
        "bounded_schema_regex_oversized",
        "bounded_schema_remote_ref",
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
    let generator = case.generator.as_ref().unwrap();
    match generator.kind.as_str() {
        "schemaRawInputWithWhitespace" => {
            let base = format!(r#"{{"$schema":"{DRAFT}","type":"object"}}"#);
            format!(
                "{base}{}",
                " ".repeat(case.target_raw_input_bytes.unwrap() - base.len())
            )
        }
        "schemaCanonicalExpansion" => {
            canonical_expansion_schema(case.target_canonical_encoded_bytes.unwrap())
        }
        "nestedLocalDraft202012Schema" => nested_items_schema(case.target_depth.unwrap()),
        "localDraft202012Schema" => match generator.collection.as_deref() {
            Some("properties") => {
                properties_schema(case.target_properties_required_or_enum_items.unwrap())
            }
            Some("required") => array_keyword_schema(
                "required",
                case.target_properties_required_or_enum_items.unwrap(),
                true,
            ),
            Some("enum") => array_keyword_schema(
                "enum",
                case.target_properties_required_or_enum_items.unwrap(),
                false,
            ),
            None => node_schema(case.target_nodes.unwrap()),
            Some(other) => panic!("unimplemented schema collection {other}"),
        },
        "localPatternSchema" => format!(
            r#"{{"$schema":"{DRAFT}","pattern":"{}","type":"string"}}"#,
            "a".repeat(case.target_regex_bytes.unwrap())
        ),
        "schemaWithRef" => format!(
            r#"{{"$schema":"{DRAFT}","$ref":"{}"}}"#,
            generator.reference.as_deref().unwrap()
        ),
        other => panic!("unimplemented schema recipe generator {other}"),
    }
}

fn validate_generated_metrics(case: &BoundaryCase, input: &str) {
    let document: Value = serde_json::from_str(input).unwrap();
    assert_eq!(document.get("$schema").and_then(Value::as_str), Some(DRAFT));

    if let Some(target) = case.target_raw_input_bytes {
        assert_eq!(input.len(), target, "{} raw input bytes", case.name);
    }
    if let Some(maximum) = case.max_raw_input_bytes {
        assert!(input.len() <= maximum, "{} raw input bytes", case.name);
    }
    if let Some(maximum) = case
        .generator
        .as_ref()
        .unwrap()
        .canonical_output_below_bytes
    {
        assert!(
            serde_json::to_vec(&document).unwrap().len() < maximum,
            "{} canonical output bytes",
            case.name
        );
    }
    if let Some(target) = case.target_canonical_encoded_bytes {
        let expected = input.replace("1e-6", "0.000001");
        assert_eq!(
            expected.len(),
            target,
            "{} canonical output bytes",
            case.name
        );
    }

    let (depth, nodes) = json_metrics(&document);
    if let Some(target) = case.target_depth {
        assert_eq!(depth, target, "{} depth", case.name);
    }
    if let Some(target) = case.target_nodes {
        assert_eq!(nodes, target, "{} nodes", case.name);
    }
    if let Some(target) = case.target_properties_required_or_enum_items {
        let collection = case
            .generator
            .as_ref()
            .unwrap()
            .collection
            .as_deref()
            .unwrap();
        let actual = match collection {
            "properties" => document[collection].as_object().unwrap().len(),
            "required" | "enum" => document[collection].as_array().unwrap().len(),
            other => panic!("unimplemented schema collection {other}"),
        };
        assert_eq!(actual, target, "{} collection items", case.name);
    }
    if let Some(target) = case.target_regex_bytes {
        assert_eq!(
            document["pattern"].as_str().unwrap().len(),
            target,
            "{} regex bytes",
            case.name
        );
    }
}

fn validate_failure(case: &BoundaryCase, error: &BoundedJsonSchemaError) {
    let expected = match case.name.as_str() {
        "bounded_schema_input_bytes_oversized" => {
            assert_eq!(case.expected.failure.as_deref(), Some("raw_input_bytes"));
            BoundedJsonSchemaError::Json(BoundedJsonError::RawInputTooLarge)
        }
        "bounded_schema_output_bytes_oversized" => {
            assert_eq!(
                case.expected.failure.as_deref(),
                Some("canonical_output_bytes")
            );
            BoundedJsonSchemaError::Json(BoundedJsonError::CanonicalOutputTooLarge)
        }
        "bounded_schema_depth_oversized" => {
            BoundedJsonSchemaError::Json(BoundedJsonError::DepthLimit)
        }
        "bounded_schema_nodes_oversized" => {
            BoundedJsonSchemaError::Json(BoundedJsonError::NodeLimit)
        }
        "bounded_schema_properties_oversized"
        | "bounded_schema_required_oversized"
        | "bounded_schema_enum_oversized" => BoundedJsonSchemaError::KeywordCollectionLimit,
        "bounded_schema_regex_oversized" => BoundedJsonSchemaError::RegexBytesLimit,
        "bounded_schema_remote_ref" => {
            assert_eq!(case.expected.network_access, Some(false));
            BoundedJsonSchemaError::RemoteReference
        }
        other => panic!("unexpected schema failure {other}: {error:?}"),
    };
    assert_eq!(error, &expected, "{}", case.name);
}

fn json_metrics(value: &Value) -> (usize, usize) {
    match value {
        Value::Array(values) => {
            let metrics = values.iter().map(json_metrics).collect::<Vec<_>>();
            (
                1 + metrics.iter().map(|(depth, _)| *depth).max().unwrap_or(0),
                1 + metrics.iter().map(|(_, nodes)| *nodes).sum::<usize>(),
            )
        }
        Value::Object(values) => {
            let metrics = values.values().map(json_metrics).collect::<Vec<_>>();
            (
                1 + metrics.iter().map(|(depth, _)| *depth).max().unwrap_or(0),
                1 + metrics.iter().map(|(_, nodes)| *nodes).sum::<usize>(),
            )
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => (1, 1),
    }
}

fn canonical_expansion_schema(target: usize) -> String {
    let empty = format!(r#"{{"$schema":"{DRAFT}","description":"","minimum":1e-6}}"#);
    let expansion = "0.000001".len() - "1e-6".len();
    let target_input = target - expansion;
    format!(
        r#"{{"$schema":"{DRAFT}","description":"{}","minimum":1e-6}}"#,
        "x".repeat(target_input - empty.len())
    )
}

fn nested_items_schema(target_depth: usize) -> String {
    let mut schema = r#"{"type":"string"}"#.to_owned();
    for _ in 0..target_depth - 3 {
        schema = format!(r#"{{"items":{schema}}}"#);
    }
    format!(r#"{{"$schema":"{DRAFT}","items":{schema}}}"#)
}

fn properties_schema(count: usize) -> String {
    let properties = (0..count)
        .map(|index| format!(r#""p{index:03}":{{}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"$schema":"{DRAFT}","properties":{{{properties}}}}}"#)
}

fn array_keyword_schema(keyword: &str, count: usize, strings: bool) -> String {
    let values = (0..count)
        .map(|index| {
            if strings {
                format!("\"v{index:03}\"")
            } else {
                index.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"$schema":"{DRAFT}","{keyword}":[{values}]}}"#)
}

fn node_schema(target_nodes: usize) -> String {
    let schemas = std::iter::repeat_n("{}", target_nodes - 3)
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"$schema":"{DRAFT}","allOf":[{schemas}]}}"#)
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fixtures/wire-v1")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
