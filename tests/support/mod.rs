// Shared fixtures are compiled separately into each integration-test binary.
#![allow(dead_code)]

pub mod loops;
pub mod tools;

use minicore_runtime::tools::{ToolInvocation, ToolSpec};
use serde_json::{Value, json};

pub fn invocation(name: &str, arguments: Value) -> ToolInvocation {
    ToolInvocation::new(
        "call_00000000000000000000000000000001".parse().unwrap(),
        name.parse().unwrap(),
        arguments,
    )
    .unwrap()
}

pub fn spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        name.parse().unwrap(),
        description,
        json!({"type": "object"}),
    )
    .unwrap()
}
