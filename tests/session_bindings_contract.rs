#[path = "support/fake_bindings.rs"]
mod fake_bindings;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use fake_bindings::{
    Calls, base_model, compact_schema, compaction, context, descriptor, model, policy, spec,
    tool_set, tool_spec, validate_tools,
};
use minicore_runtime::compaction::CompactionStrategy;
use minicore_runtime::context::ContextProvider;
use minicore_runtime::model::{Model, ReasoningPreference};
use minicore_runtime::session::{SessionBindingError, SessionBindings};
use minicore_runtime::tools::{ToolPolicy, ToolSet};
use minicore_runtime::{CompactionConfig, SemanticLimits, SessionSpec};
use serde_json::json;

fn empty_bindings(calls: &Arc<Calls>, supports_tools: bool) -> SessionBindings {
    SessionBindings::new(
        base_model(calls, supports_tools),
        ToolSet::default(),
        None,
        None,
        None,
    )
}

fn assert_error(
    bindings: &SessionBindings,
    spec: &SessionSpec,
    limits: &SemanticLimits,
    expected: SessionBindingError,
) {
    assert_eq!(bindings.validate(spec, limits), Err(expected));
}

fn error_name(error: SessionBindingError) -> &'static str {
    match error {
        SessionBindingError::InvalidLimits => "invalid_limits",
        SessionBindingError::InvalidSpec => "invalid_spec",
        SessionBindingError::ModelPanicked => "model_panicked",
        SessionBindingError::InvalidModelDescriptor => "invalid_model_descriptor",
        SessionBindingError::ModelMismatch => "model_mismatch",
        SessionBindingError::UnsupportedReasoning => "unsupported_reasoning",
        SessionBindingError::UnsupportedTools => "unsupported_tools",
        SessionBindingError::MissingTool => "missing_tool",
        SessionBindingError::MissingToolPolicy => "missing_tool_policy",
        SessionBindingError::MissingCompactionStrategy => "missing_compaction_strategy",
        SessionBindingError::TooManyTools => "too_many_tools",
        SessionBindingError::InvalidToolSpec => "invalid_tool_spec",
    }
}

#[test]
fn surface_is_exact_clone_send_sync_and_redacted() {
    type Constructor = fn(
        Arc<dyn Model>,
        ToolSet,
        Option<Arc<dyn ToolPolicy>>,
        Option<Arc<dyn ContextProvider>>,
        Option<Arc<dyn CompactionStrategy>>,
    ) -> SessionBindings;
    fn assert_send_sync<T: Send + Sync>() {}

    let _: Constructor = SessionBindings::new;
    assert_send_sync::<SessionBindings>();
    let calls = Arc::new(Calls::default());
    let bindings = empty_bindings(&calls, false);
    let cloned = bindings.clone();
    assert!(Arc::ptr_eq(&bindings.model, &cloned.model));
    let _ = (
        &bindings.tools,
        &bindings.tool_policy,
        &bindings.context,
        &bindings.compaction,
    );
    let debug = format!("{bindings:?}");
    assert!(debug.contains("tool_count: 0"));
    assert!(!debug.contains("host:model"));
    assert_eq!(calls.descriptor.load(Ordering::SeqCst), 0);

    let source = include_str!("../src/session/bindings.rs");
    let fields = source
        .split_once("pub struct SessionBindings")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    assert_eq!(
        fields
            .lines()
            .filter(|line| line.trim_start().starts_with("pub "))
            .count(),
        5
    );
    for required in [
        "pub model:",
        "pub tools:",
        "pub tool_policy:",
        "pub context:",
        "pub compaction:",
    ] {
        assert!(fields.contains(required));
    }
    for forbidden in [
        "Clock",
        "Runtime",
        "Handle",
        "SessionLog",
        "Store",
        "Workspace",
        "owner",
        "metadata",
        "serde",
        "Serialize",
        "Deserialize",
        "registry",
    ] {
        assert!(
            !source.contains(forbidden),
            "bindings source contains {forbidden}"
        );
    }
    for forbidden_call in [".start(", ".execute(", ".decide(", ".provide(", ".compact("] {
        assert!(!source.contains(forbidden_call));
    }
    assert!(
        include_str!("../src/session/mod.rs")
            .contains("pub use bindings::{SessionBindingError, SessionBindings};")
    );
    let root = include_str!("../src/lib.rs")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("");
    let session_exports = root
        .split_once("pubusesession::{")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(exports, _)| exports)
        .expect("root must contain one grouped public session export");
    assert!(
        session_exports
            .split(',')
            .any(|export| export == "SessionBindings")
    );
    let tool_source = include_str!("../src/tools/tool.rs");
    assert!(tool_source.contains("!self.input_schema.is_object()"));
    assert!(tool_source.contains("validate_json_size(&self.input_schema, max_schema_bytes)"));

    let errors = [
        SessionBindingError::InvalidLimits,
        SessionBindingError::InvalidSpec,
        SessionBindingError::ModelPanicked,
        SessionBindingError::InvalidModelDescriptor,
        SessionBindingError::ModelMismatch,
        SessionBindingError::UnsupportedReasoning,
        SessionBindingError::UnsupportedTools,
        SessionBindingError::MissingTool,
        SessionBindingError::MissingToolPolicy,
        SessionBindingError::MissingCompactionStrategy,
        SessionBindingError::TooManyTools,
        SessionBindingError::InvalidToolSpec,
    ];
    for error in errors {
        assert!(!error_name(error).is_empty());
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("host:model"));
        assert!(!diagnostic.contains("secret"));
    }
}

#[test]
fn succeeds_without_tools_or_compaction_and_has_no_context_window_rule() {
    let calls = Arc::new(Calls::default());
    let bindings = empty_bindings(&calls, false);
    assert_eq!(
        bindings.validate(
            &spec(&[], ReasoningPreference::Auto, CompactionConfig::Disabled),
            &SemanticLimits::default(),
        ),
        Ok(())
    );
    assert_eq!(calls.descriptor.load(Ordering::SeqCst), 1);
}

#[test]
fn reports_limit_spec_and_model_failures() {
    let calls = Arc::new(Calls::default());
    let valid_spec = spec(&[], ReasoningPreference::Auto, CompactionConfig::Disabled);
    let bindings = empty_bindings(&calls, false);

    let invalid_limits = SemanticLimits {
        max_tool_count: 0,
        ..SemanticLimits::default()
    };
    assert_error(
        &bindings,
        &valid_spec,
        &invalid_limits,
        SessionBindingError::InvalidLimits,
    );
    let mut invalid_spec = valid_spec.clone();
    invalid_spec.max_tool_rounds = 0;
    assert_error(
        &bindings,
        &invalid_spec,
        &SemanticLimits::default(),
        SessionBindingError::InvalidSpec,
    );

    let panic_model = model(
        &calls,
        descriptor(
            "host:model",
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
            1,
        ),
        true,
    );
    assert_error(
        &SessionBindings::new(panic_model, ToolSet::default(), None, None, None),
        &valid_spec,
        &SemanticLimits::default(),
        SessionBindingError::ModelPanicked,
    );
    let invalid = model(
        &calls,
        descriptor("host:model", BTreeSet::new(), false, 0),
        false,
    );
    assert_error(
        &SessionBindings::new(invalid, ToolSet::default(), None, None, None),
        &valid_spec,
        &SemanticLimits::default(),
        SessionBindingError::InvalidModelDescriptor,
    );
    let mismatch = model(
        &calls,
        descriptor(
            "host:other",
            BTreeSet::from([ReasoningPreference::Auto]),
            false,
            1,
        ),
        false,
    );
    assert_error(
        &SessionBindings::new(mismatch, ToolSet::default(), None, None, None),
        &valid_spec,
        &SemanticLimits::default(),
        SessionBindingError::ModelMismatch,
    );
    assert_error(
        &bindings,
        &spec(&[], ReasoningPreference::High, CompactionConfig::Disabled),
        &SemanticLimits::default(),
        SessionBindingError::UnsupportedReasoning,
    );
}

#[test]
fn enabled_tools_require_support_policy_and_registration() {
    let calls = Arc::new(Calls::default());
    let enabled = spec(
        &["run"],
        ReasoningPreference::Auto,
        CompactionConfig::Disabled,
    );
    let tools = tool_set(&calls, vec![tool_spec("run", "run", json!({}))]);
    let unsupported = SessionBindings::new(
        base_model(&calls, false),
        tools.clone(),
        Some(policy(&calls)),
        None,
        None,
    );
    assert_error(
        &unsupported,
        &enabled,
        &SemanticLimits::default(),
        SessionBindingError::UnsupportedTools,
    );
    let missing_policy = SessionBindings::new(base_model(&calls, true), tools, None, None, None);
    assert_error(
        &missing_policy,
        &enabled,
        &SemanticLimits::default(),
        SessionBindingError::MissingToolPolicy,
    );
    let missing_tool = SessionBindings::new(
        base_model(&calls, true),
        ToolSet::default(),
        Some(policy(&calls)),
        None,
        None,
    );
    assert_error(
        &missing_tool,
        &enabled,
        &SemanticLimits::default(),
        SessionBindingError::MissingTool,
    );

    let short_names = SemanticLimits {
        max_tool_name_bytes: 2,
        ..SemanticLimits::default()
    };
    assert_error(
        &missing_tool,
        &enabled,
        &short_names,
        SessionBindingError::InvalidSpec,
    );
}

#[test]
fn validates_all_frozen_tool_spec_semantic_boundaries() {
    let calls = Arc::new(Calls::default());
    let session_spec = spec(&[], ReasoningPreference::Auto, CompactionConfig::Disabled);
    let model = base_model(&calls, false);

    let count_limits = SemanticLimits {
        max_tool_count: 1,
        ..SemanticLimits::default()
    };
    let one = tool_set(&calls, vec![tool_spec("one", "ok", json!({}))]);
    assert!(validate_tools(&model, one, &session_spec, &count_limits).is_ok());
    let two = tool_set(
        &calls,
        vec![
            tool_spec("one", "ok", json!({})),
            tool_spec("two", "ok", json!({})),
        ],
    );
    assert_eq!(
        validate_tools(&model, two, &session_spec, &count_limits),
        Err(SessionBindingError::TooManyTools)
    );

    let name_limits = SemanticLimits {
        max_tool_name_bytes: 4,
        ..SemanticLimits::default()
    };
    let exact_name = tool_set(&calls, vec![tool_spec("four", "ok", json!({}))]);
    assert!(validate_tools(&model, exact_name, &session_spec, &name_limits).is_ok());
    let long_name = tool_set(&calls, vec![tool_spec("fives", "ok", json!({}))]);
    assert_eq!(
        validate_tools(&model, long_name, &session_spec, &name_limits),
        Err(SessionBindingError::InvalidToolSpec)
    );

    let description_limits = SemanticLimits {
        max_tool_schema_bytes: 16,
        ..SemanticLimits::default()
    };
    let exact_description = tool_set(
        &calls,
        vec![tool_spec("description", "x".repeat(16), json!({}))],
    );
    assert!(
        validate_tools(
            &model,
            exact_description,
            &session_spec,
            &description_limits
        )
        .is_ok()
    );
    let long_description = tool_set(
        &calls,
        vec![tool_spec("description", "x".repeat(17), json!({}))],
    );
    assert_eq!(
        validate_tools(&model, long_description, &session_spec, &description_limits),
        Err(SessionBindingError::InvalidToolSpec)
    );

    let schema_limits = SemanticLimits {
        max_tool_schema_bytes: 32,
        ..SemanticLimits::default()
    };
    let exact_schema = tool_set(&calls, vec![tool_spec("schema", "ok", compact_schema(32))]);
    assert!(validate_tools(&model, exact_schema, &session_spec, &schema_limits).is_ok());
    let large_schema = tool_set(&calls, vec![tool_spec("schema", "ok", compact_schema(33))]);
    assert_eq!(
        validate_tools(&model, large_schema, &session_spec, &schema_limits),
        Err(SessionBindingError::InvalidToolSpec)
    );
}

#[test]
fn compaction_requirements_and_validation_are_pure() {
    let calls = Arc::new(Calls::default());
    let enabled_compaction = CompactionConfig::Enabled {
        trigger_tokens: 20,
        target_tokens: 10,
    };
    let compacting_spec = spec(&[], ReasoningPreference::Auto, enabled_compaction.clone());
    assert_error(
        &empty_bindings(&calls, false),
        &compacting_spec,
        &SemanticLimits::default(),
        SessionBindingError::MissingCompactionStrategy,
    );

    let disabled = SessionBindings::new(
        base_model(&calls, false),
        ToolSet::default(),
        None,
        None,
        Some(compaction(&calls)),
    );
    assert!(
        disabled
            .validate(
                &spec(&[], ReasoningPreference::Auto, CompactionConfig::Disabled),
                &SemanticLimits::default(),
            )
            .is_ok()
    );

    let tools = tool_set(&calls, vec![tool_spec("run", "run", json!({}))]);
    let tool_spec_calls = calls.tool_spec.load(Ordering::SeqCst);
    let full = SessionBindings::new(
        base_model(&calls, true),
        tools,
        Some(policy(&calls)),
        Some(context(&calls)),
        Some(compaction(&calls)),
    );
    assert!(
        full.validate(
            &spec(&["run"], ReasoningPreference::Auto, enabled_compaction),
            &SemanticLimits::default(),
        )
        .is_ok()
    );
    assert_eq!(calls.model_start.load(Ordering::SeqCst), 0);
    assert_eq!(calls.tool_spec.load(Ordering::SeqCst), tool_spec_calls);
    assert_eq!(calls.tool_execute.load(Ordering::SeqCst), 0);
    assert_eq!(calls.policy.load(Ordering::SeqCst), 0);
    assert_eq!(calls.context.load(Ordering::SeqCst), 0);
    assert_eq!(calls.compaction.load(Ordering::SeqCst), 0);
}

#[test]
fn p4_load_contract_orders_binding_validation_before_proof_and_finish() {
    let source = include_str!("../src/conversation/load.rs");
    let validate = source
        .find("bindings.validate(&pending.manifest().spec, limits)")
        .unwrap();
    let proof = source
        .find("LoadCompatibilityValidated::after_session_bindings_validation(&pending)")
        .unwrap();
    let finish = source.find("pending.finish(proof)").unwrap();
    assert!(validate < proof && proof < finish);
    assert!(source.contains("pub(crate) struct LoadCompatibilityValidated"));
    assert!(!include_str!("../src/session/bindings.rs").contains("LoadCompatibilityValidated"));
}
