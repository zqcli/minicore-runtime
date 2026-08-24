pub mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use minicore_runtime::config::{
    ABSOLUTE_MAX_TOOL_COUNT, ABSOLUTE_MAX_TOOL_ROUNDS, ConfigError, SessionManifest,
};
use minicore_runtime::error::SessionOpenErrorKind;
use minicore_runtime::model::{
    Model, ModelCallContext, ModelDescriptor, ModelRef, ModelRequest, ModelStartFuture,
    ReasoningPreference,
};
use minicore_runtime::session::{SessionHealth, SessionStatus};
use minicore_runtime::tools::{
    Tool, ToolContext, ToolDecision, ToolFuture, ToolInvocation, ToolName, ToolPolicy,
    ToolPolicyFuture, ToolPolicyRequest, ToolSet, ToolSpec,
};
use minicore_runtime::{
    BoundedText, CompactionConfig, KernelConfig, SemanticLimits, SessionBindings, SessionId,
    SessionRuntime, SessionRuntimeOptions, SessionSpec,
};
use serde_json::json;

use support::fake_session_log::{FakeSessionLog, Operation};

struct PassthroughModel {
    descriptor: ModelDescriptor,
}

impl Model for PassthroughModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        panic!("idle runtime must not start model")
    }
}

struct DummyTool {
    spec: ToolSpec,
}

impl Tool for DummyTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture<'a> {
        panic!("tool execute must not be called during open")
    }
}

struct AllowAllPolicy;

impl ToolPolicy for AllowAllPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        Box::pin(async { Ok(ToolDecision::Allow) })
    }
}

fn session(value: u8) -> SessionId {
    format!("ses_{value:032}").parse().unwrap()
}

fn build_tools(count: usize) -> (BTreeSet<ToolName>, ToolSet) {
    let mut names = BTreeSet::new();
    let mut builder = ToolSet::builder();
    for i in 0..count {
        let name_str = format!("tool_{i:03}");
        let name: ToolName = name_str.parse().unwrap();
        names.insert(name.clone());
        let spec = ToolSpec::new(name, "description", json!({"type": "object"})).unwrap();
        builder.register(DummyTool { spec });
    }
    (names, builder.build().unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn create_with_custom_max_tool_count_allows_tools_exceeding_default_limit() {
    let model_ref: ModelRef = "host:limits-evidence".parse().unwrap();
    let (tool_names, tool_set) = build_tools(65);
    let spec = SessionSpec::new(
        model_ref.clone(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        tool_names,
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();

    let model: Arc<dyn Model> = Arc::new(PassthroughModel {
        descriptor: ModelDescriptor::new(
            model_ref,
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
    });
    let policy: Arc<dyn ToolPolicy> = Arc::new(AllowAllPolicy);
    let bindings = SessionBindings::new(model, tool_set, Some(policy), None, None);

    let kernel = KernelConfig {
        limits: SemanticLimits {
            max_tool_count: 128,
            ..SemanticLimits::default()
        },
        ..KernelConfig::default_checked().unwrap()
    };
    let options =
        SessionRuntimeOptions::new(kernel, bindings, tokio::runtime::Handle::current()).unwrap();

    let session_id = session(1);
    let log = FakeSessionLog::new();
    let runtime = SessionRuntime::create(session_id, spec, Box::new(log), options)
        .await
        .unwrap();

    assert_eq!(runtime.handle().state().status, SessionStatus::Idle);
    assert_eq!(runtime.handle().state().health, SessionHealth::Healthy);
    runtime.shutdown().await.unwrap();
}

#[test]
fn session_manifest_serde_roundtrip_preserves_custom_tool_count_for_instance_validation() {
    let tool_names: Vec<String> = (0..65).map(|i| format!("tool_{i:03}")).collect();
    let manifest_value = json!({
        "format_version": 3,
        "session_id": "ses_00000000000000000000000000000001",
        "created_at": "2026-08-19T12:34:56.789Z",
        "spec": {
            "model": "host:limits-evidence",
            "reasoning": "auto",
            "system_prompt": "system",
            "enabled_tools": tool_names,
            "max_tool_rounds": 4,
            "compaction": { "mode": "disabled" }
        }
    });

    let manifest: SessionManifest = serde_json::from_value(manifest_value.clone()).unwrap();
    let custom_limits = SemanticLimits {
        max_tool_count: 128,
        ..SemanticLimits::default()
    };
    assert!(manifest.validate(&custom_limits).is_ok());
    assert_eq!(
        manifest.validate(&SemanticLimits::default()),
        Err(ConfigError::InvalidBounds)
    );

    let serialized = serde_json::to_value(&manifest).unwrap();
    assert_eq!(serialized, manifest_value);
    let roundtripped: SessionManifest = serde_json::from_value(serialized).unwrap();
    assert_eq!(roundtripped, manifest);
    assert!(roundtripped.validate(&custom_limits).is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn create_shutdown_load_roundtrip_with_custom_max_tool_count() {
    let model_ref: ModelRef = "host:limits-evidence".parse().unwrap();
    let (tool_names, tool_set) = build_tools(65);
    let spec = SessionSpec::new(
        model_ref.clone(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        tool_names,
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();

    let model: Arc<dyn Model> = Arc::new(PassthroughModel {
        descriptor: ModelDescriptor::new(
            model_ref,
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
    });
    let policy: Arc<dyn ToolPolicy> = Arc::new(AllowAllPolicy);
    let bindings = SessionBindings::new(model, tool_set, Some(policy), None, None);

    let kernel = KernelConfig {
        limits: SemanticLimits {
            max_tool_count: 128,
            ..SemanticLimits::default()
        },
        ..KernelConfig::default_checked().unwrap()
    };
    let create_options = SessionRuntimeOptions::new(
        kernel.clone(),
        bindings.clone(),
        tokio::runtime::Handle::current(),
    )
    .unwrap();

    let session_id = session(3);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let runtime = SessionRuntime::create(session_id, spec, Box::new(log), create_options)
        .await
        .unwrap();

    assert_eq!(runtime.handle().state().status, SessionStatus::Idle);
    assert_eq!(runtime.handle().state().health, SessionHealth::Healthy);
    runtime.shutdown().await.unwrap();

    let durable_manifest = inspection.manifest().unwrap();
    let durable_entries = inspection.entries();
    let reload_log = FakeSessionLog::with_initial(durable_manifest, durable_entries).unwrap();
    let load_options =
        SessionRuntimeOptions::new(kernel, bindings, tokio::runtime::Handle::current()).unwrap();
    let loaded = SessionRuntime::load(session_id, Box::new(reload_log), load_options)
        .await
        .unwrap();

    assert_eq!(loaded.handle().state().status, SessionStatus::Idle);
    assert_eq!(loaded.handle().state().health, SessionHealth::Healthy);
    loaded.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn narrower_instance_limits_reject_create_before_log_initialize() {
    let model_ref: ModelRef = "host:limits-evidence".parse().unwrap();
    let (tool_names, tool_set) = build_tools(33);
    let spec = SessionSpec::new(
        model_ref.clone(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        tool_names,
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();

    let model: Arc<dyn Model> = Arc::new(PassthroughModel {
        descriptor: ModelDescriptor::new(
            model_ref,
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
    });
    let policy: Arc<dyn ToolPolicy> = Arc::new(AllowAllPolicy);
    let bindings = SessionBindings::new(model, tool_set, Some(policy), None, None);

    let kernel = KernelConfig {
        limits: SemanticLimits {
            max_tool_count: 32,
            ..SemanticLimits::default()
        },
        ..KernelConfig::default_checked().unwrap()
    };
    let options =
        SessionRuntimeOptions::new(kernel, bindings, tokio::runtime::Handle::current()).unwrap();

    let session_id = session(2);
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let error = match SessionRuntime::create(session_id, spec, Box::new(log), options).await {
        Ok(runtime) => {
            runtime.shutdown().await.unwrap();
            panic!("create unexpectedly succeeded with tool count exceeding instance limits");
        }
        Err(err) => err,
    };

    assert_eq!(error.kind(), SessionOpenErrorKind::InvalidManifest);
    assert!(!inspection.operations().contains(&Operation::Initialize));
    assert_eq!(inspection.operations(), vec![Operation::Close]);
}

#[test]
fn session_spec_and_manifest_enforce_absolute_structural_bounds() {
    let model_ref: ModelRef = "host:limits-evidence".parse().unwrap();
    let prompt = BoundedText::new("system").unwrap();

    let excessive_tools: BTreeSet<ToolName> = (0..=ABSOLUTE_MAX_TOOL_COUNT)
        .map(|index| format!("tool_{index}").parse().unwrap())
        .collect();
    assert_eq!(
        SessionSpec::new(
            model_ref.clone(),
            ReasoningPreference::Auto,
            prompt.clone(),
            excessive_tools,
            4,
            CompactionConfig::Disabled,
        ),
        Err(ConfigError::InvalidBounds)
    );

    assert_eq!(
        SessionSpec::new(
            model_ref.clone(),
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
            model_ref,
            ReasoningPreference::Auto,
            prompt,
            BTreeSet::new(),
            ABSOLUTE_MAX_TOOL_ROUNDS + 1,
            CompactionConfig::Disabled,
        ),
        Err(ConfigError::InvalidBounds)
    );

    let excessive_wire_tools: Vec<String> = (0..=ABSOLUTE_MAX_TOOL_COUNT)
        .map(|index| format!("tool_{index}"))
        .collect();
    let invalid_spec_json = json!({
        "model": "host:limits-evidence",
        "reasoning": "auto",
        "system_prompt": "system",
        "enabled_tools": excessive_wire_tools,
        "max_tool_rounds": 4,
        "compaction": { "mode": "disabled" }
    });
    assert!(serde_json::from_value::<SessionSpec>(invalid_spec_json.clone()).is_err());

    let invalid_manifest_json = json!({
        "format_version": 3,
        "session_id": "ses_00000000000000000000000000000001",
        "created_at": "2026-08-19T12:34:56.789Z",
        "spec": invalid_spec_json
    });
    assert!(serde_json::from_value::<SessionManifest>(invalid_manifest_json).is_err());
}
