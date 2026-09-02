use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use minicore_runtime::tools::{
    Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolFuture, ToolInputAnswer,
    ToolInputAnswerKind, ToolInputRequest, ToolInvocation, ToolOutput, ToolProgress,
    ToolProgressSink, ToolSet, ToolSetError, ToolSpec,
};
use minicore_runtime::value::{BoundedText, MAX_JSON_BYTES, MAX_JSON_DEPTH, MAX_JSON_NODES};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn invocation(name: &str, arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation::new(
        "call_00000000000000000000000000000001".parse().unwrap(),
        name.parse().unwrap(),
        arguments,
    )
    .unwrap()
}

fn spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        name.parse().unwrap(),
        description,
        json!({"type": "object"}),
    )
    .unwrap()
}

struct FakeTool {
    spec: ToolSpec,
    calls: Arc<AtomicUsize>,
    request_input: bool,
}

struct MutableSpecTool {
    specs: [ToolSpec; 2],
    calls: AtomicUsize,
}

impl MutableSpecTool {
    fn new() -> Self {
        Self {
            specs: [
                ToolSpec::new(
                    "mutable".parse().unwrap(),
                    "spec A",
                    json!({"version": "A"}),
                )
                .unwrap(),
                ToolSpec::new(
                    "mutable".parse().unwrap(),
                    "spec B",
                    json!({"version": "B"}),
                )
                .unwrap(),
            ],
            calls: AtomicUsize::new(0),
        }
    }
}

impl Tool for MutableSpecTool {
    fn spec(&self) -> &ToolSpec {
        let index = self.calls.fetch_add(1, Ordering::SeqCst).min(1);
        &self.specs[index]
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async {
            Ok(ToolExecutionOutcome::Completed(
                ToolOutput::new("ok").unwrap(),
            ))
        })
    }
}

impl FakeTool {
    fn new(name: &str, request_input: bool) -> Self {
        Self {
            spec: spec(name, name),
            calls: Arc::new(AtomicUsize::new(0)),
            request_input,
        }
    }
}

impl Tool for FakeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let request_input = self.request_input;
        Box::pin(async move {
            assert!(context.deadline > Instant::now() - Duration::from_secs(1));
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if request_input {
                Ok(ToolExecutionOutcome::RequestInput(
                    ToolInputRequest::new(
                        "choose",
                        vec![BoundedText::new("yes").unwrap()],
                        ToolInputAnswerKind::SingleChoice,
                    )
                    .unwrap(),
                ))
            } else {
                Ok(ToolExecutionOutcome::Completed(
                    ToolOutput::new("done").unwrap(),
                ))
            }
        })
    }
}

struct PanicSpecTool;

struct InvalidSpecTool {
    spec: ToolSpec,
}

impl Tool for InvalidSpecTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async { Err(ToolError::InvalidInvocation) })
    }
}

impl Tool for PanicSpecTool {
    fn spec(&self) -> &ToolSpec {
        panic!("spec panic")
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async { Err(ToolError::Panicked) })
    }
}

struct OverlapTool {
    spec: ToolSpec,
    gate: Arc<tokio::sync::Barrier>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl Tool for OverlapTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        let gate = Arc::clone(&self.gate);
        let active = Arc::clone(&self.active);
        let max_active = Arc::clone(&self.max_active);
        Box::pin(async move {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            let mut observed = max_active.load(Ordering::SeqCst);
            while current > observed {
                match max_active.compare_exchange(
                    observed,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
            gate.wait().await;
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolExecutionOutcome::Completed(
                ToolOutput::new("done").unwrap(),
            ))
        })
    }
}

#[test]
fn tool_spec_and_output_use_checked_bounded_content() {
    let tool_spec = spec("read_file", "Read one file");
    assert_eq!(tool_spec.name.as_str(), "read_file");
    assert_eq!(tool_spec.description.as_str(), "Read one file");
    assert_eq!(tool_spec.input_schema, json!({"type": "object"}));
    assert_eq!(tool_spec.description().as_str(), "Read one file");
    assert!(serde_json::to_value(&tool_spec).unwrap()["description"] == "Read one file");
    assert!(ToolSpec::new("read_file".parse().unwrap(), "", json!({})).is_err());
    assert!(ToolSpec::new("read_file".parse().unwrap(), "x\u{0001}", json!({})).is_err());

    let output = ToolOutput::new("safe output").unwrap();
    assert_eq!(output.content().as_str(), "safe output");
    assert_eq!(
        serde_json::to_value(&output).unwrap(),
        json!({"content": "safe output"})
    );
    assert!(!format!("{output:?}").contains("safe output"));
    assert!(serde_json::from_value::<ToolOutput>(json!({"is_error": false})).is_err());
}

#[test]
fn tool_spec_description_boundary_is_checked() {
    assert!(ToolSpec::new("bounded".parse().unwrap(), "x".repeat(4_096), json!({}),).is_ok());
    assert!(ToolSpec::new("bounded".parse().unwrap(), "x".repeat(4_097), json!({}),).is_err());
}

#[test]
fn tool_spec_serde_rejects_invalid_shapes_and_round_trips_valid_fields() {
    let valid = json!({
        "name": "read_file",
        "description": "Read one file",
        "input_schema": {"type": "object"}
    });
    let decoded: ToolSpec = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), valid);

    let mut unknown = valid.clone();
    unknown["extra"] = json!(true);
    assert!(serde_json::from_value::<ToolSpec>(unknown).is_err());
    assert!(
        serde_json::from_value::<ToolSpec>(json!({
            "name": "read_file",
            "description": "x".repeat(4_097),
            "input_schema": {}
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolSpec>(json!({
            "name": "read_file",
            "description": "ok",
            "input_schema": []
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolSpec>(json!({
            "name": "read_file",
            "description": "ok",
            "input_schema": {"padding": "x".repeat(MAX_JSON_BYTES)}
        }))
        .is_err()
    );

    let mut deep = json!({});
    for _ in 0..=MAX_JSON_DEPTH {
        deep = json!({"nested": deep});
    }
    assert!(
        serde_json::from_value::<ToolSpec>(json!({
            "name": "read_file",
            "description": "ok",
            "input_schema": deep
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ToolSpec>(json!({
            "name": "read_file",
            "description": "ok",
            "input_schema": {
                "items": vec![serde_json::Value::Null; MAX_JSON_NODES - 1]
            }
        }))
        .is_err()
    );
}

#[test]
fn tool_trait_and_future_are_send_sync_typed_ports() {
    fn assert_tool<T: Tool + Send + Sync + 'static>() {}
    fn assert_future_send<'a>(future: ToolFuture<'a>) -> ToolFuture<'a> {
        future
    }

    assert_tool::<FakeTool>();
    let tool = FakeTool::new("typed", false);
    let future = tool.execute(
        invocation("typed", json!({})),
        ToolContext {
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(60),
            progress: ToolProgressSink::default(),
        },
    );
    drop(assert_future_send(future));
}

#[test]
fn invocation_serde_is_strict_bounded_and_redacted() {
    let value = invocation("read_file", json!({"path": "README.md"}));
    assert_eq!(value.tool_name().as_str(), "read_file");
    assert!(!format!("{value:?}").contains("README.md"));
    let wire = serde_json::to_value(&value).unwrap();
    assert_eq!(
        wire,
        json!({
            "tool_call_id": "call_00000000000000000000000000000001",
            "tool_name": "read_file",
            "arguments": {"path": "README.md"}
        })
    );
    assert_eq!(
        serde_json::from_value::<ToolInvocation>(wire.clone()).unwrap(),
        value
    );

    let mut unknown = wire.clone();
    unknown["extra"] = json!(true);
    let mut non_object = wire.clone();
    non_object["arguments"] = json!([]);
    for invalid in [unknown, non_object] {
        assert!(serde_json::from_value::<ToolInvocation>(invalid).is_err());
    }
    for (field, invalid) in [("tool_call_id", ""), ("tool_name", "bad name")] {
        let mut candidate = wire.clone();
        candidate[field] = json!(invalid);
        assert!(serde_json::from_value::<ToolInvocation>(candidate).is_err());
    }

    let mut oversized = wire.clone();
    oversized["arguments"] = json!({"padding": "x".repeat(MAX_JSON_BYTES)});
    assert!(serde_json::from_value::<ToolInvocation>(oversized).is_err());
    let mut deep_arguments = json!({});
    for _ in 0..=MAX_JSON_DEPTH {
        deep_arguments = json!({"nested": deep_arguments});
    }
    let mut deep = wire.clone();
    deep["arguments"] = deep_arguments;
    assert!(serde_json::from_value::<ToolInvocation>(deep).is_err());
    let mut node_heavy = wire;
    node_heavy["arguments"] = json!({
        "items": vec![serde_json::Value::Null; MAX_JSON_NODES - 1]
    });
    assert!(serde_json::from_value::<ToolInvocation>(node_heavy).is_err());

    assert!(!include_str!("../src/tools/tool.rs").contains("continuation"));
}

#[test]
fn input_answers_reject_extra_fields_bad_shapes_and_invalid_text() {
    let request = ToolInputRequest::new(
        "choose",
        vec![
            BoundedText::new("yes").unwrap(),
            BoundedText::new("no").unwrap(),
        ],
        ToolInputAnswerKind::SingleChoice,
    )
    .unwrap();
    assert!(
        ToolInputAnswer::Choice { index: 1 }
            .validate(&request)
            .is_ok()
    );
    assert!(
        ToolInputAnswer::Choice { index: 2 }
            .validate(&request)
            .is_err()
    );
    assert!(
        ToolInputAnswer::Text(BoundedText::new("yes").unwrap())
            .validate(&request)
            .is_err()
    );

    for value in [
        json!({"kind": "choice", "data": {"index": 0, "extra": true}}),
        json!({"kind": "choice", "data": "0"}),
        json!({"kind": "choice", "data": {"index": "0"}}),
        json!({"kind": "choice", "data": {"index": 0}, "extra": false}),
        json!({"kind": "text", "data": 7}),
        json!({"kind": "text", "data": ""}),
        json!({"kind": "text", "data": "bad\u{0001}"}),
        json!({"kind": "unknown", "data": "x"}),
    ] {
        assert!(serde_json::from_value::<ToolInputAnswer>(value).is_err());
    }
    assert!(
        serde_json::from_value::<ToolInputAnswer>(json!({
            "kind": "text",
            "data": "x".repeat(8_193)
        }))
        .is_err()
    );
    let text_request =
        ToolInputRequest::new("prompt", Vec::new(), ToolInputAnswerKind::Text).unwrap();
    let exact_text = ToolInputAnswer::Text(BoundedText::new("x".repeat(8_192)).unwrap());
    let oversized_text = ToolInputAnswer::Text(BoundedText::new("x".repeat(8_193)).unwrap());
    assert!(
        serde_json::from_value::<ToolInputAnswer>(serde_json::to_value(&exact_text).unwrap())
            .is_ok()
    );
    assert!(
        serde_json::from_value::<ToolInputAnswer>(serde_json::to_value(&oversized_text).unwrap())
            .is_err()
    );
    assert!(exact_text.validate(&text_request).is_ok());
    assert!(oversized_text.validate(&text_request).is_err());
}

#[test]
fn tool_set_is_immutable_sorted_and_rejects_duplicate_or_panicking_specs() {
    let alpha = FakeTool::new("alpha", false);
    let alpha_calls = Arc::clone(&alpha.calls);
    let mut builder = ToolSet::builder();
    builder.register(FakeTool::new("zeta", false));
    builder.register(alpha);
    let set = builder.build().unwrap();
    let cloned = set.clone();
    assert!(cloned.contains(&"alpha".parse().unwrap()));
    let from_set = set.get(&"alpha".parse().unwrap()).unwrap();
    let from_clone = cloned.get(&"alpha".parse().unwrap()).unwrap();
    assert!(Arc::ptr_eq(&from_set, &from_clone));
    let enabled = BTreeSet::from(["zeta".parse().unwrap(), "alpha".parse().unwrap()]);
    let specs = set.specs_for(&enabled);
    assert_eq!(
        specs
            .iter()
            .map(|value| value.name().as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "zeta"]
    );
    assert_eq!(alpha_calls.load(Ordering::SeqCst), 0);
    assert!(
        set.specs_for(&BTreeSet::from(["missing".parse().unwrap()]))
            .is_empty()
    );

    let mut duplicate = ToolSet::builder();
    duplicate.register(FakeTool::new("same", false));
    duplicate.register(FakeTool::new("same", false));
    duplicate.register(PanicSpecTool);
    assert!(matches!(
        duplicate.build(),
        Err(ToolSetError::DuplicateTool)
    ));
    let mut panicking = ToolSet::builder();
    panicking.register(PanicSpecTool);
    assert!(matches!(panicking.build(), Err(ToolSetError::Panicked)));
    let mut invalid = spec("invalid", "invalid");
    invalid.input_schema = json!("not an object");
    let mut invalid_builder = ToolSet::builder();
    invalid_builder.register(InvalidSpecTool { spec: invalid });
    assert!(matches!(
        invalid_builder.build(),
        Err(ToolSetError::InvalidSpec)
    ));
    assert!(ToolSet::default().specs_for(&BTreeSet::new()).is_empty());
}

#[test]
fn tool_set_freezes_registration_spec_while_tool_spec_can_change_later() {
    let mut builder = ToolSet::builder();
    builder.register(MutableSpecTool::new());
    let set = builder.build().unwrap();
    let enabled = BTreeSet::from(["mutable".parse().unwrap()]);

    let frozen = set.specs_for(&enabled);
    assert_eq!(frozen[0].description().as_str(), "spec A");
    assert_eq!(frozen[0].input_schema(), &json!({"version": "A"}));

    let live = set.get(&"mutable".parse().unwrap()).unwrap();
    assert_eq!(live.spec().description().as_str(), "spec B");
    assert_eq!(live.spec().input_schema(), &json!({"version": "B"}));
    assert_eq!(set.specs_for(&enabled)[0].description().as_str(), "spec A");
}

#[tokio::test]
async fn context_cancellation_deadline_progress_and_outcomes_are_typed() {
    let cancellation = CancellationToken::new();
    let context = ToolContext {
        cancellation: cancellation.clone(),
        deadline: Instant::now() + Duration::from_secs(60),
        progress: ToolProgressSink::default(),
    };
    let progress =
        ToolProgress::new(Some(BoundedText::new("working").unwrap()), Some(1), Some(2)).unwrap();
    assert!(!context.progress.emit(progress));
    assert!(ToolProgress::new(None, Some(3), Some(2)).is_err());

    let tool = FakeTool::new("run", false);
    let result = tool
        .execute(invocation("run", json!({})), context.clone())
        .await;
    assert!(matches!(result, Ok(ToolExecutionOutcome::Completed(_))));
    cancellation.cancel();
    let result = tool.execute(invocation("run", json!({})), context).await;
    assert_eq!(result, Err(ToolError::Cancelled));

    let request_tool = FakeTool::new("ask", true);
    let context = ToolContext {
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(60),
        progress: ToolProgressSink::default(),
    };
    assert!(matches!(
        request_tool
            .execute(invocation("ask", json!({})), context)
            .await,
        Ok(ToolExecutionOutcome::RequestInput(_))
    ));
}

#[tokio::test]
async fn one_shared_tool_can_execute_concurrently_without_global_state() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut builder = ToolSet::builder();
    builder.register(OverlapTool {
        spec: spec("shared", "shared"),
        gate: Arc::new(tokio::sync::Barrier::new(2)),
        active: Arc::clone(&active),
        max_active: Arc::clone(&max_active),
    });
    let set = builder.build().unwrap();
    let cloned = set.clone();
    let tool = set.get(&"shared".parse().unwrap()).unwrap();
    let cloned_tool = cloned.get(&"shared".parse().unwrap()).unwrap();
    let first = tool.execute(
        invocation("shared", json!({"n": 1})),
        ToolContext {
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(60),
            progress: ToolProgressSink::default(),
        },
    );
    let second = cloned_tool.execute(
        invocation("shared", json!({"n": 2})),
        ToolContext {
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(60),
            progress: ToolProgressSink::default(),
        },
    );
    let (first, second) = tokio::join!(first, second);
    assert!(matches!(first, Ok(ToolExecutionOutcome::Completed(_))));
    assert!(matches!(second, Ok(ToolExecutionOutcome::Completed(_))));
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
}

#[test]
fn final_tool_surface_has_no_legacy_context_or_owner_dependencies() {
    let tools = include_str!("../src/tools/mod.rs");
    let context = include_str!("../src/tools/context.rs");
    let tool = include_str!("../src/tools/tool.rs");
    assert!(!tools.contains("pub use registry"));
    assert!(!tools.contains("ToolRegistry"));
    for source in [context, tool] {
        for forbidden in [
            "pub use legacy_context",
            "Workspace",
            "InteractionClient",
            "SessionHandle",
            "Runtime",
            "Any",
            "continuation",
        ] {
            assert!(
                !source.contains(forbidden),
                "found legacy/public owner token {forbidden}"
            );
        }
    }
}
