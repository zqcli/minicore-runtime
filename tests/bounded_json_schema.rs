use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use minicore_runtime::wire::{BoundedJsonSchema, BoundedJsonSchemaError, ProtocolLimits};

const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

#[test]
fn bounded_schema_is_a_canonical_draft_2020_12_object_carrier() {
    let input = format!(
        r##"{{"type":"string","$schema":"{DRAFT}","pattern":"^[a-z]+$","$ref":"#/$defs/name","$defs":{{"name":{{"type":"string"}}}}}}"##
    );
    let schema = BoundedJsonSchema::from_slice(input.as_bytes()).unwrap();
    assert_eq!(
        schema.canonical_json(),
        format!(
            r##"{{"$defs":{{"name":{{"type":"string"}}}},"$ref":"#/$defs/name","$schema":"{DRAFT}","pattern":"^[a-z]+$","type":"string"}}"##
        )
    );

    let equivalent = BoundedJsonSchema::from_slice(schema.canonical_bytes()).unwrap();
    assert_eq!(schema, equivalent);
    assert_eq!(hash(&schema), hash(&equivalent));
    assert!(!format!("{schema:?}").contains("^[a-z]+$"));
}

#[test]
fn bounded_schema_rejects_remote_refs_without_io() {
    for reference in [
        "https://example.invalid/schema.json",
        "file:///tmp/schema.json",
        "/absolute/schema.json",
        "relative/schema.json",
    ] {
        let input = format!(r#"{{"$schema":"{DRAFT}","$ref":"{reference}"}}"#);
        assert_eq!(
            BoundedJsonSchema::from_slice(input.as_bytes()),
            Err(BoundedJsonSchemaError::RemoteReference)
        );
    }
    let nested =
        format!(r#"{{"$schema":"{DRAFT}","allOf":[{{"$ref":"https://example.invalid/x"}}]}}"#);
    assert_eq!(
        BoundedJsonSchema::from_slice(nested.as_bytes()),
        Err(BoundedJsonSchemaError::RemoteReference)
    );
}

#[test]
fn bounded_schema_does_not_interpret_instance_data_as_nested_schemas() {
    let data_properties = (0..257)
        .map(|index| format!(r#""p{index:03}":0"#))
        .collect::<Vec<_>>()
        .join(",");
    let input = format!(
        r#"{{"const":{{"$ref":"https://example.invalid/data","pattern":"(?=a)","properties":{{{data_properties}}}}},"default":{{"$ref":"relative/data"}},"definitions":{{"x":{{"$ref":"file:///data"}}}},"enum":[{{"$ref":"file:///data"}}]}}"#
    );
    assert!(BoundedJsonSchema::from_slice(input.as_bytes()).is_ok());
}

#[test]
fn bounded_schema_enforces_regex_bytes_and_linear_engine_syntax() {
    let max = ProtocolLimits::v1_0().embedded_json.schema.max_regex_bytes as usize;
    let boundary = format!(r#"{{"$schema":"{DRAFT}","pattern":"{}"}}"#, "a".repeat(max));
    assert!(BoundedJsonSchema::from_slice(boundary.as_bytes()).is_ok());

    let oversized = format!(
        r#"{{"$schema":"{DRAFT}","pattern":"{}"}}"#,
        "a".repeat(max + 1)
    );
    assert_eq!(
        BoundedJsonSchema::from_slice(oversized.as_bytes()),
        Err(BoundedJsonSchemaError::RegexBytesLimit)
    );
    let unicode_boundary = format!(
        r#"{{"$schema":"{DRAFT}","pattern":"{}"}}"#,
        "é".repeat(max / 2)
    );
    assert!(BoundedJsonSchema::from_slice(unicode_boundary.as_bytes()).is_ok());
    let unicode_oversized = format!(
        r#"{{"$schema":"{DRAFT}","pattern":"{}a"}}"#,
        "é".repeat(max / 2)
    );
    assert_eq!(
        BoundedJsonSchema::from_slice(unicode_oversized.as_bytes()),
        Err(BoundedJsonSchemaError::RegexBytesLimit)
    );
    let oversized_property_pattern = format!(
        r#"{{"patternProperties":{{"{}":{{}}}}}}"#,
        "a".repeat(max + 1)
    );
    assert_eq!(
        BoundedJsonSchema::from_slice(oversized_property_pattern.as_bytes()),
        Err(BoundedJsonSchemaError::RegexBytesLimit)
    );
    let unsupported = format!(r#"{{"$schema":"{DRAFT}","pattern":"(?=a)"}}"#);
    assert_eq!(
        BoundedJsonSchema::from_slice(unsupported.as_bytes()),
        Err(BoundedJsonSchemaError::InvalidRegex)
    );
    let expansion = format!(r#"{{"$schema":"{DRAFT}","pattern":"(?:a{{1000}}){{1000}}"}}"#);
    assert!(BoundedJsonSchema::from_slice(expansion.as_bytes()).is_ok());
}

#[test]
fn bounded_schema_enforces_keyword_collection_limits() {
    let max = ProtocolLimits::v1_0()
        .embedded_json
        .schema
        .max_properties_required_or_enum_items as usize;

    assert!(BoundedJsonSchema::from_slice(properties_schema(max).as_bytes()).is_ok());
    assert_eq!(
        BoundedJsonSchema::from_slice(properties_schema(max + 1).as_bytes()),
        Err(BoundedJsonSchemaError::KeywordCollectionLimit)
    );
    assert!(
        BoundedJsonSchema::from_slice(array_keyword_schema("required", max).as_bytes()).is_ok()
    );
    assert_eq!(
        BoundedJsonSchema::from_slice(array_keyword_schema("required", max + 1).as_bytes()),
        Err(BoundedJsonSchemaError::KeywordCollectionLimit)
    );
    assert!(BoundedJsonSchema::from_slice(array_keyword_schema("enum", max).as_bytes()).is_ok());
    assert_eq!(
        BoundedJsonSchema::from_slice(array_keyword_schema("enum", max + 1).as_bytes()),
        Err(BoundedJsonSchemaError::KeywordCollectionLimit)
    );

    let combined = format!(
        r#"{{"enum":{},"properties":{},"required":{}}}"#,
        keyword_array(max, false),
        properties_object(max),
        keyword_array(max, true)
    );
    assert!(BoundedJsonSchema::from_slice(combined.as_bytes()).is_ok());
}

#[test]
fn bounded_schema_enforces_root_inclusive_depth_and_node_limits() {
    let limits = ProtocolLimits::v1_0().embedded_json.schema;
    assert!(
        BoundedJsonSchema::from_slice(nested_objects(limits.max_depth as usize - 1).as_bytes())
            .is_ok()
    );
    assert_eq!(
        BoundedJsonSchema::from_slice(nested_objects(limits.max_depth as usize).as_bytes()),
        Err(BoundedJsonSchemaError::Json(
            minicore_runtime::wire::BoundedJsonError::DepthLimit
        ))
    );

    let boundary_items = limits.max_nodes as usize - 2;
    assert!(BoundedJsonSchema::from_slice(node_schema(boundary_items).as_bytes()).is_ok());
    assert_eq!(
        BoundedJsonSchema::from_slice(node_schema(boundary_items + 1).as_bytes()),
        Err(BoundedJsonSchemaError::Json(
            minicore_runtime::wire::BoundedJsonError::NodeLimit
        ))
    );
    assert_eq!(
        BoundedJsonSchema::from_slice(b"[]"),
        Err(BoundedJsonSchemaError::RootObjectRequired)
    );
}

#[test]
fn bounded_schema_decode_is_panic_free_for_arbitrary_bytes() {
    let mut state = 0x7f4a_7c15_d3e2_91b9_u64;
    for len in 0..512 {
        let mut bytes = vec![0_u8; len];
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        assert!(
            std::panic::catch_unwind(|| BoundedJsonSchema::from_slice(&bytes)).is_ok(),
            "schema parser panicked for {len} bytes"
        );
    }
}

fn properties_schema(count: usize) -> String {
    format!(
        r#"{{"$schema":"{DRAFT}","properties":{}}}"#,
        properties_object(count)
    )
}

fn properties_object(count: usize) -> String {
    let properties = (0..count)
        .map(|index| format!(r#""p{index:03}":{{}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{properties}}}")
}

fn array_keyword_schema(keyword: &str, count: usize) -> String {
    format!(
        r#"{{"$schema":"{DRAFT}","{keyword}":{}}}"#,
        keyword_array(count, true)
    )
}

fn keyword_array(count: usize, strings: bool) -> String {
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
    format!("[{values}]")
}

fn nested_objects(wrappers: usize) -> String {
    let mut value = "0".to_owned();
    for _ in 0..wrappers {
        value = format!(r#"{{"x":{value}}}"#);
    }
    value
}

fn node_schema(items: usize) -> String {
    let values = std::iter::repeat_n("0", items)
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"nodes":[{values}]}}"#)
}

fn hash(value: &BoundedJsonSchema) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
